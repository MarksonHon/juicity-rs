use crate::config::{
    method_to_index, ProfileStore, ProxyProfile, ProxyProtocol, RuntimeState,
    StartupConnectionState, Storage, SS_METHODS,
};
use crate::core::CoreManager;
use crate::link;
use crate::pac;
use crate::system_proxy;
use crate::tray::{self, TrayEvent, TraySharedState};
use crate::widgets::{self, TextField};
use gpui::prelude::*;
use gpui::{
    actions, div, px, rgb, size, App, Bounds, ClickEvent, Context, Entity, FontWeight, Global,
    KeyBinding, MouseButton, SharedString, Timer, Window, WindowBounds, WindowHandle,
    WindowOptions,
};
use rust_i18n::t;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

actions!(app, [Quit]);

/// App-wide registry that survives the main window being closed to the tray.
#[derive(Default)]
struct AppRoot {
    view: Option<Entity<AppView>>,
    main_window: Option<WindowHandle<AppView>>,
    /// Set right before the main window is closed via OK/Cancel so the
    /// `on_window_closed` handler keeps the app alive in the tray.
    suppress_quit: bool,
    /// Whether the main window has already been closed (to the tray).  Once
    /// set, closing *dialog* windows never quits the application; only the
    /// first close of the main window is able to trigger the quit path.
    main_window_closed: bool,
}

impl Global for AppRoot {}

/// Open (or focus) the main window bound to the persistent `AppView` entity.
fn open_main_window(cx: &mut App) {
    let Some(view) = cx.default_global::<AppRoot>().view.clone() else {
        return;
    };
    let existing = cx.default_global::<AppRoot>().main_window;
    let already_active = existing
        .as_ref()
        .and_then(|h| h.update(cx, |_, window, _| window.activate_window()).ok())
        .is_some();
    if already_active {
        return;
    }
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(760.), px(600.)),
                    cx,
                ))),
                app_id: Some("io.juicity.gui".to_string()),
                ..Default::default()
            },
            |window, _cx| {
                window.set_window_title(&t!("window.title"));
                window.set_app_id("io.juicity.gui");
                view
            },
        )
        .ok();
    if let Some(handle) = handle {
        let g = cx.default_global::<AppRoot>();
        g.main_window = Some(handle);
        g.main_window_closed = false;
    }
}

struct GuiState {
    storage: Storage,
    config: crate::config::AppConfig,
    profiles: ProfileStore,
    runtime: RuntimeState,
    core_manager: CoreManager,
    pac_server: Option<pac::PacServer>,
    pac_update_rx: Option<std::sync::mpsc::Receiver<anyhow::Result<()>>>,
    _tray_service: Option<tray::TrayService>,
}

impl GuiState {
    fn new() -> anyhow::Result<Self> {
        let storage = Storage::new()?;
        let config = storage.load_app_config()?;
        let mut profiles = storage.load_profiles()?;
        let mut runtime = storage.load_runtime_state()?;

        if profiles.profiles.is_empty() {
            profiles.profiles.push(ProxyProfile::default());
            runtime.selected_profile = 0;
        }

        Ok(Self {
            storage,
            config,
            profiles,
            runtime,
            core_manager: CoreManager::new(),
            pac_server: None,
            pac_update_rx: None,
            _tray_service: None,
        })
    }

    fn flush(&self) -> anyhow::Result<()> {
        self.storage.save_app_config(&self.config)?;
        self.storage.save_profiles(&self.profiles)?;
        self.storage.save_runtime_state(&self.runtime)?;
        Ok(())
    }

    fn selected_profile(&self) -> Option<&ProxyProfile> {
        self.profiles.profiles.get(self.runtime.selected_profile)
    }

    fn selected_profile_mut(&mut self) -> Option<&mut ProxyProfile> {
        self.profiles
            .profiles
            .get_mut(self.runtime.selected_profile)
    }

    fn normalize_selected_index(&mut self) {
        if self.profiles.profiles.is_empty() {
            self.profiles.profiles.push(ProxyProfile::default());
        }
        if self.runtime.selected_profile >= self.profiles.profiles.len() {
            self.runtime.selected_profile = self.profiles.profiles.len().saturating_sub(1);
        }
    }
}

/// Restart or update the PAC server with fresh rules from disk.
///
/// If `force_restart` is `true` (e.g. the listen address changed), a new
/// server is started even if one already exists.  Otherwise the existing
/// server is updated in-place, or a new one is started if none exists.
fn restart_pac_server(state: &mut GuiState, force_restart: bool) -> anyhow::Result<()> {
    let (direct, proxy) = pac::load_rules(&state.storage.paths().config_dir);
    let content = pac::generate_pac(
        state.config.pac_rule_mode,
        &state.config.socks_listen,
        &direct,
        &proxy,
    );
    if force_restart || state.pac_server.is_none() {
        state.pac_server = Some(pac::start(&state.config.pac_listen, content)?);
    } else if let Some(srv) = &state.pac_server {
        srv.update(content);
    }
    Ok(())
}

/// Which dropdown is currently open (used to render its popup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropdownId {
    Protocol,
    Method,
}

pub struct AppView {
    gui: GuiState,
    tray_tx: std::sync::mpsc::Sender<TrayEvent>,
    tray_rx: std::sync::mpsc::Receiver<TrayEvent>,
    tray_shared: Arc<Mutex<TraySharedState>>,

    // ── Editor text fields ────────────────────────────────────────────────
    server: Entity<TextField>,
    port: Entity<TextField>,
    password: Entity<TextField>,
    uuid: Entity<TextField>,
    sni: Entity<TextField>,
    plugin: Entity<TextField>,
    plugin_opts: Entity<TextField>,
    plugin_args: Entity<TextField>,
    remarks: Entity<TextField>,
    timeout: Entity<TextField>,
    group: Entity<TextField>,
    proxy_port: Entity<TextField>,

    // ── Editor widget state ───────────────────────────────────────────────
    protocol: usize,
    method: usize,
    show_password: bool,
    allow_insecure: bool,
    need_plugin_arg: bool,
    close_to_tray: bool,
    open_dropdown: Option<DropdownId>,

    status: String,
}

impl AppView {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut gui = GuiState::new().expect("failed to initialize app state");
        if let Err(err) = restart_pac_server(&mut gui, true) {
            tracing::warn!("PAC server failed to start: {err}");
        }

        // Auto-update PAC rules on startup if interval is set and overdue.
        if gui.config.pac_auto_update_hours > 0 {
            let age_h = pac::rules_age_hours(&gui.storage.paths().config_dir);
            let overdue = age_h.is_none_or(|h| h >= gui.config.pac_auto_update_hours as u64);
            if overdue {
                let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
                std::thread::spawn({
                    let data_dir = gui.storage.paths().config_dir.clone();
                    let direct_url = gui.config.pac_direct_url.clone();
                    let proxy_url = gui.config.pac_proxy_url.clone();
                    move || {
                        let _ = tx.send(
                            pac::download_rules(&data_dir, &direct_url, &proxy_url).map(|_| ()),
                        );
                    }
                });
                gui.pac_update_rx = Some(rx);
            }
        }

        let new_field = |cx: &mut Context<TextField>| TextField::new(cx);
        let server = cx.new(new_field);
        let port = cx.new(new_field);
        let password = cx.new(new_field);
        let uuid = cx.new(new_field);
        let sni = cx.new(new_field);
        let plugin = cx.new(new_field);
        let plugin_opts = cx.new(new_field);
        let plugin_args = cx.new(new_field);
        let remarks = cx.new(new_field);
        let timeout = cx.new(new_field);
        let group = cx.new(new_field);
        let proxy_port = cx.new(new_field);

        // ── Shared tray state + service ─────────────────────────────────────
        let (tray_tx, tray_rx) = std::sync::mpsc::channel::<TrayEvent>();
        let tray_shared = Arc::new(Mutex::new(TraySharedState::default()));
        {
            let mut ts = tray_shared.lock().unwrap_or_else(|e| e.into_inner());
            ts.system_proxy_mode = gui.config.system_proxy_mode;
            ts.pac_rule_mode = gui.config.pac_rule_mode;
            ts.server_names = gui
                .profiles
                .profiles
                .iter()
                .map(|p| p.display_name())
                .collect();
            ts.active_server_idx = gui.runtime.selected_profile;
        }
        gui._tray_service = Some(tray::start(tray_tx.clone(), Arc::clone(&tray_shared)));

        let mut view = Self {
            gui,
            tray_tx,
            tray_rx,
            tray_shared,
            server,
            port,
            password,
            uuid,
            sni,
            plugin,
            plugin_opts,
            plugin_args,
            remarks,
            timeout,
            group,
            proxy_port,
            protocol: 0,
            method: 0,
            show_password: false,
            allow_insecure: false,
            need_plugin_arg: false,
            close_to_tray: false,
            open_dropdown: None,
            status: t!("status.stopped").to_string(),
        };
        view.load_fields(cx);

        // ── Periodic poll loop: tray events + PAC + core status ────────────
        cx.spawn(async move |this, cx| {
            let mut timer = Timer::after(Duration::from_millis(300));
            loop {
                timer.await;
                if this.update(cx, |view, cx| view.poll(cx)).is_err() {
                    break;
                }
                timer = Timer::after(Duration::from_millis(300));
            }
        })
        .detach();

        view
    }

    // ── Status helper ─────────────────────────────────────────────────────

    fn set_status(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.status != text {
            self.status = text.to_string();
            cx.notify();
        }
    }

    // ── Field load / save ─────────────────────────────────────────────────

    fn load_fields(&mut self, cx: &mut Context<Self>) {
        let profile = self.gui.selected_profile().cloned();
        if let Some(p) = profile {
            self.protocol = p.protocol.index() as usize;
            self.server.update(cx, |f, cx| f.set_text(p.server, cx));
            self.port
                .update(cx, |f, cx| f.set_text(p.server_port.to_string(), cx));
            self.password.update(cx, |f, cx| f.set_text(p.password, cx));
            self.uuid.update(cx, |f, cx| f.set_text(p.uuid, cx));
            self.sni
                .update(cx, |f, cx| f.set_text(p.sni.unwrap_or_default(), cx));
            self.allow_insecure = p.allow_insecure;
            self.method = method_to_index(&p.method) as usize;
            self.plugin
                .update(cx, |f, cx| f.set_text(p.plugin.unwrap_or_default(), cx));
            self.plugin_opts.update(cx, |f, cx| {
                f.set_text(p.plugin_opts.unwrap_or_default(), cx)
            });
            self.need_plugin_arg = p.plugin_args.is_some();
            self.plugin_args.update(cx, |f, cx| {
                f.set_text(p.plugin_args.unwrap_or_default(), cx)
            });
            self.remarks.update(cx, |f, cx| f.set_text(p.name, cx));
            self.timeout
                .update(cx, |f, cx| f.set_text(p.timeout.to_string(), cx));
            self.group
                .update(cx, |f, cx| f.set_text(p.group.unwrap_or_default(), cx));
        }
        let port = extract_port(&self.gui.config.socks_listen);
        self.proxy_port
            .update(cx, |f, cx| f.set_text(port.to_string(), cx));
        self.close_to_tray = self.gui.runtime.close_to_tray;
        cx.notify();
    }

    fn save_fields(&mut self, cx: &mut Context<Self>) {
        let server = self.server.read(cx).text();
        let port = self.port.read(cx).text();
        let password = self.password.read(cx).text();
        let uuid = self.uuid.read(cx).text();
        let sni = self.sni.read(cx).text();
        let plugin = self.plugin.read(cx).text();
        let plugin_opts = self.plugin_opts.read(cx).text();
        let plugin_args = self.plugin_args.read(cx).text();
        let remarks = self.remarks.read(cx).text();
        let timeout = self.timeout.read(cx).text();
        let group = self.group.read(cx).text();
        let proxy_port = self.proxy_port.read(cx).text();

        let mut invalid: Vec<&str> = Vec::new();
        let server_port = match port.trim().parse::<u16>() {
            Ok(v) => v,
            Err(_) => {
                invalid.push("server port");
                443
            }
        };
        let timeout_v = match timeout.trim().parse::<u32>() {
            Ok(v) => v,
            Err(_) => {
                invalid.push("timeout");
                5
            }
        };
        let proxy_port_v = match proxy_port.trim().parse::<u16>() {
            Ok(v) => v,
            Err(_) => {
                invalid.push("proxy port");
                1080
            }
        };
        let plugin_args_v = if self.need_plugin_arg {
            non_empty_text(&plugin_args)
        } else {
            None
        };

        {
            let g = &mut self.gui;
            g.normalize_selected_index();
            if let Some(p) = g.selected_profile_mut() {
                p.protocol = ProxyProtocol::from_index(self.protocol as u32);
                p.server = server.trim().to_string();
                p.server_port = server_port;
                p.password = password;
                p.uuid = uuid.trim().to_string();
                p.sni = non_empty_text(&sni);
                p.allow_insecure = self.allow_insecure;
                p.method = SS_METHODS
                    .get(self.method)
                    .copied()
                    .unwrap_or("chacha20-ietf-poly1305")
                    .to_string();
                p.plugin = non_empty_text(&plugin);
                p.plugin_opts = non_empty_text(&plugin_opts);
                p.plugin_args = plugin_args_v;
                let remarks_trim = remarks.trim().to_string();
                p.name = if remarks_trim.is_empty() {
                    "New Server".to_string()
                } else {
                    remarks_trim
                };
                p.timeout = timeout_v;
                p.group = non_empty_text(&group);
            }
            let (addr, _) = crate::util::split_host_port(&g.config.socks_listen);
            g.config.socks_listen = crate::util::format_host_port(addr, proxy_port_v);
            g.runtime.close_to_tray = self.close_to_tray;
        }

        if !invalid.is_empty() {
            self.set_status(&format!("Status: invalid {}", invalid.join(", ")), cx);
        }
    }

    // ── Dropdown helpers ──────────────────────────────────────────────────

    fn select_dropdown(&mut self, dd: DropdownId, idx: usize) {
        match dd {
            DropdownId::Protocol => self.protocol = idx,
            DropdownId::Method => self.method = idx,
        }
        self.open_dropdown = None;
    }

    fn close_dropdown_if_open(&mut self, cx: &mut Context<Self>) {
        if self.open_dropdown.is_some() {
            self.open_dropdown = None;
            cx.notify();
        }
    }

    fn dropdown_element(
        &mut self,
        cx: &mut Context<Self>,
        dd: DropdownId,
        id: &'static str,
        selected: usize,
        options: Vec<SharedString>,
    ) -> impl IntoElement {
        let open = self.open_dropdown == Some(dd);
        let this = cx.weak_entity();
        let on_toggle: widgets::ClickHandler = Rc::new(move |_e, _w, cx| {
            this.update(cx, |view, cx| {
                view.open_dropdown = if view.open_dropdown == Some(dd) {
                    None
                } else {
                    Some(dd)
                };
                cx.notify();
            })
            .ok();
        });
        let this = cx.weak_entity();
        let on_select: widgets::IndexHandler = Rc::new(move |idx, _w, cx| {
            this.update(cx, |view, cx| {
                view.select_dropdown(dd, idx);
                cx.notify();
            })
            .ok();
        });
        widgets::dropdown(id, selected, options, open, on_toggle, on_select)
    }

    // ── Button handlers ───────────────────────────────────────────────────

    fn add_clicked(&mut self, cx: &mut Context<Self>) {
        let n = self.gui.profiles.profiles.len() + 1;
        let p = ProxyProfile {
            name: t!("misc.new_server", n = n).to_string(),
            ..Default::default()
        };
        self.gui.profiles.profiles.push(p);
        self.gui.runtime.selected_profile = self.gui.profiles.profiles.len() - 1;
        self.sync_tray_servers();
        self.load_fields(cx);
        cx.notify();
    }

    fn delete_clicked(&mut self, cx: &mut Context<Self>) {
        if self.gui.profiles.profiles.len() > 1 {
            let idx = self.gui.runtime.selected_profile;
            self.gui.profiles.profiles.remove(idx);
            self.gui.normalize_selected_index();
            self.sync_tray_servers();
            self.load_fields(cx);
            cx.notify();
        }
    }

    fn duplicate_clicked(&mut self, cx: &mut Context<Self>) {
        let idx = self.gui.runtime.selected_profile;
        if let Some(p) = self.gui.profiles.profiles.get(idx).cloned() {
            self.gui.profiles.profiles.insert(idx + 1, p);
            self.gui.runtime.selected_profile = idx + 1;
            self.sync_tray_servers();
            self.load_fields(cx);
            cx.notify();
        }
    }

    fn move_up_clicked(&mut self, cx: &mut Context<Self>) {
        let idx = self.gui.runtime.selected_profile;
        if idx > 0 {
            self.gui.profiles.profiles.swap(idx, idx - 1);
            self.gui.runtime.selected_profile = idx - 1;
            self.sync_tray_servers();
            self.load_fields(cx);
            cx.notify();
        }
    }

    fn move_down_clicked(&mut self, cx: &mut Context<Self>) {
        let idx = self.gui.runtime.selected_profile;
        if idx + 1 < self.gui.profiles.profiles.len() {
            self.gui.profiles.profiles.swap(idx, idx + 1);
            self.gui.runtime.selected_profile = idx + 1;
            self.sync_tray_servers();
            self.load_fields(cx);
            cx.notify();
        }
    }

    fn start_selected(&mut self, cx: &mut Context<Self>) {
        self.save_fields(cx);
        if let Err(err) = self.gui.flush() {
            self.set_status(&t!("status.save_failed", err = err.to_string()), cx);
            return;
        }
        let profile = match self.gui.selected_profile().cloned() {
            Some(p) => p,
            None => {
                self.set_status(&t!("status.no_server"), cx);
                return;
            }
        };
        let config_snap = self.gui.config.clone();
        match self.gui.core_manager.start_profile(&config_snap, &profile) {
            Ok(()) => {
                self.set_status(
                    &t!(
                        "status.running",
                        proto = profile.protocol.label(),
                        name = profile.display_name()
                    ),
                    cx,
                );
                self.gui.runtime.was_running = true;
                let _ = self.gui.flush();
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = true;
                    ts.active_server_name = profile.display_name();
                }
            }
            Err(err) => self.set_status(&t!("status.start_failed", err = err.to_string()), cx),
        }
    }

    fn stop_core(&mut self, cx: &mut Context<Self>) {
        match self.gui.core_manager.stop() {
            Ok(()) => {
                self.set_status(&t!("status.stopped"), cx);
                self.gui.runtime.was_running = false;
                let _ = self.gui.flush();
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = false;
                    ts.active_server_name = String::new();
                }
            }
            Err(err) => self.set_status(&t!("status.stop_failed", err = err.to_string()), cx),
        }
    }

    fn import_link(&mut self, input: &str, cx: &mut Context<Self>) {
        match link::import_share_link(input.trim()) {
            Ok(imported) => {
                let idx = self.gui.runtime.selected_profile;
                if let Some(p) = self.gui.profiles.profiles.get_mut(idx) {
                    imported.apply_to(p);
                }
                self.sync_tray_servers();
                self.load_fields(cx);
                self.set_status(&t!("status.imported"), cx);
            }
            Err(err) => self.set_status(&t!("status.import_failed", err = err.to_string()), cx),
        }
    }

    fn export_link(&mut self, cx: &mut Context<Self>) {
        let url = match self.gui.selected_profile() {
            Some(p) => link::export_share_link(p),
            None => {
                self.set_status(&t!("status.no_server_selected"), cx);
                return;
            }
        };
        match url {
            Ok(url) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(url));
                self.set_status(&t!("status.url_copied"), cx);
            }
            Err(err) => self.set_status(&t!("status.export_failed", err = err.to_string()), cx),
        }
    }

    fn ok_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_fields(cx);
        match self.gui.flush() {
            Ok(()) => {
                Self::suppress_quit(cx, true);
                window.remove_window();
            }
            Err(err) => self.set_status(&t!("status.save_failed", err = err.to_string()), cx),
        }
    }

    fn cancel_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_fields(cx);
        Self::suppress_quit(cx, true);
        window.remove_window();
    }

    /// Apply a new `AppConfig` snapshot from a dialog (PAC settings) and make
    /// it take effect (persist + restart/update the local PAC server).
    pub(crate) fn apply_pac_config(
        &mut self,
        cfg: crate::config::AppConfig,
        cx: &mut Context<Self>,
    ) {
        let force_restart = self.gui.config.pac_listen != cfg.pac_listen;
        self.gui.config = cfg;
        let _ = self.gui.flush();
        let _ = restart_pac_server(&mut self.gui, force_restart);
        if let Ok(mut ts) = self.tray_shared.lock() {
            ts.system_proxy_mode = self.gui.config.system_proxy_mode;
        }
        cx.notify();
    }

    /// Trigger an immediate PAC rule download (used by the PAC dialog's
    /// "Update Now" button).
    pub(crate) fn update_pac_rules_now(&mut self, cx: &mut Context<Self>) {
        self.start_pac_download(cx);
    }

    /// Apply a new `RuntimeState` snapshot from a dialog (startup settings).
    pub(crate) fn apply_runtime_state(
        &mut self,
        state: crate::config::RuntimeState,
        cx: &mut Context<Self>,
    ) {
        self.gui.runtime = state;
        let _ = apply_autostart(&self.gui.runtime);
        let _ = self.gui.flush();
        cx.notify();
    }

    /// Snapshot of the current app configuration (for dialogs).
    pub(crate) fn config_snapshot(&self) -> crate::config::AppConfig {
        self.gui.config.clone()
    }

    /// Snapshot of the current runtime state (for dialogs).
    pub(crate) fn runtime_snapshot(&self) -> crate::config::RuntimeState {
        self.gui.runtime.clone()
    }

    fn apply_clicked(&mut self, cx: &mut Context<Self>) {
        self.save_fields(cx);
        match self.gui.flush() {
            Ok(()) => {
                let _ = system_proxy::apply_system_proxy(&self.gui.config);
                self.set_status(&t!("status.saved"), cx);
            }
            Err(err) => self.set_status(&t!("status.save_failed", err = err.to_string()), cx),
        }
    }

    fn suppress_quit(cx: &mut Context<Self>, val: bool) {
        cx.spawn(async move |_this, cx| {
            let _ = cx.update(|app| {
                let g = app.default_global::<AppRoot>();
                g.suppress_quit = val;
            });
        })
        .detach();
    }

    fn toggle_show_password(&mut self, cx: &mut Context<Self>) {
        self.show_password = !self.show_password;
        self.password
            .update(cx, |f, _| f.set_masked(!self.show_password));
        cx.notify();
    }

    fn toggle_allow_insecure(&mut self, cx: &mut Context<Self>) {
        self.allow_insecure = !self.allow_insecure;
        cx.notify();
    }

    fn toggle_need_plugin_arg(&mut self, cx: &mut Context<Self>) {
        self.need_plugin_arg = !self.need_plugin_arg;
        cx.notify();
    }

    fn toggle_close_to_tray(&mut self, cx: &mut Context<Self>) {
        self.close_to_tray = !self.close_to_tray;
        cx.notify();
    }

    fn select_server(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.gui.profiles.profiles.len() {
            self.gui.runtime.selected_profile = idx;
        }
        self.sync_tray_servers();
        self.load_fields(cx);
        cx.notify();
    }

    fn sync_tray_servers(&mut self) {
        if let Ok(mut ts) = self.tray_shared.lock() {
            ts.server_names = self
                .gui
                .profiles
                .profiles
                .iter()
                .map(|p| p.display_name())
                .collect();
            ts.active_server_idx = self.gui.runtime.selected_profile;
        }
    }

    // ── PAC download (spawns a background thread) ─────────────────────────

    fn start_pac_download(&mut self, cx: &mut Context<Self>) {
        if self.gui.pac_update_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
        self.gui.pac_update_rx = Some(rx);
        let data_dir = self.gui.storage.paths().config_dir.clone();
        let direct_url = self.gui.config.pac_direct_url.clone();
        let proxy_url = self.gui.config.pac_proxy_url.clone();
        std::thread::spawn(move || {
            let _ = tx.send(pac::download_rules(&data_dir, &direct_url, &proxy_url).map(|_| ()));
        });
        self.set_status(&t!("status.pac_downloading"), cx);
    }

    // ── Startup connection ────────────────────────────────────────────────

    fn apply_startup_connection(&mut self, cx: &mut Context<Self>) {
        let should_start = match self.gui.runtime.startup_connection_state {
            StartupConnectionState::On => true,
            StartupConnectionState::LastState => self.gui.runtime.was_running,
            StartupConnectionState::Off => false,
        };
        if should_start {
            if let Some(profile) = self.gui.selected_profile().cloned() {
                let config = self.gui.config.clone();
                match self.gui.core_manager.start_profile(&config, &profile) {
                    Ok(()) => {
                        let name = profile.display_name();
                        self.set_status(
                            &t!(
                                "status.running",
                                proto = profile.protocol.label(),
                                name = name.clone()
                            ),
                            cx,
                        );
                        self.gui.runtime.was_running = true;
                        let _ = self.gui.flush();
                        if let Ok(mut ts) = self.tray_shared.lock() {
                            ts.is_running = true;
                            ts.active_server_name = name;
                        }
                    }
                    Err(err) => {
                        self.set_status(&t!("status.start_failed", err = err.to_string()), cx);
                    }
                }
            }
        }
        cx.notify();
    }

    // ── Tray events ───────────────────────────────────────────────────────

    fn handle_tray_event(&mut self, ev: TrayEvent, cx: &mut Context<Self>) {
        match ev {
            TrayEvent::ShowEditServers => {
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(open_main_window);
                })
                .detach();
            }
            TrayEvent::ShowPacSettings => {
                let this = cx.weak_entity();
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(|app| crate::pac_dialog::open(&this, app));
                })
                .detach();
            }
            TrayEvent::ShowStartupSettings => {
                let this = cx.weak_entity();
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(|app| crate::startup_dialog::open(&this, app));
                })
                .detach();
            }
            TrayEvent::SetSystemProxy(mode) => {
                self.gui.config.system_proxy_mode = mode;
                let _ = self.gui.flush();
                let snap = self.gui.config.clone();
                let _ = system_proxy::apply_system_proxy(&snap);
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.system_proxy_mode = mode;
                }
                self.set_status(&t!("status.system_proxy", mode = mode.label()), cx);
            }
            TrayEvent::SetPacRuleMode(mode) => {
                self.gui.config.pac_rule_mode = mode;
                let _ = self.gui.flush();
                let _ = restart_pac_server(&mut self.gui, false);
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.pac_rule_mode = mode;
                }
                self.set_status(&t!("status.pac_rule", mode = mode.label()), cx);
            }
            TrayEvent::UpdatePacRules => {
                self.start_pac_download(cx);
            }
            TrayEvent::SelectServer(idx) => {
                self.select_server(idx, cx);
            }
            TrayEvent::ImportFromClipboard => {
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(open_main_window);
                })
                .detach();
            }
            TrayEvent::ToggleProxy => {
                if self.gui.core_manager.is_running() {
                    self.stop_core(cx);
                } else {
                    self.start_selected(cx);
                }
            }
            TrayEvent::QuitApp => {
                let _ = self.gui.core_manager.stop();
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(|app| app.quit());
                })
                .detach();
            }
        }
    }

    // ── Periodic poll (runs every 300 ms from the spawned task) ───────────

    fn poll(&mut self, cx: &mut Context<Self>) {
        loop {
            match self.tray_rx.try_recv() {
                Ok(ev) => self.handle_tray_event(ev, cx),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }

        tray::poll(
            self.gui._tray_service.as_mut(),
            &self.tray_shared,
            &self.tray_tx,
        );

        // Poll PAC download completion.
        if let Some(rx) = &self.gui.pac_update_rx {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    self.gui.pac_update_rx = None;
                    let _ = restart_pac_server(&mut self.gui, false);
                    self.set_status(&t!("status.pac_updated"), cx);
                }
                Ok(Err(e)) => {
                    self.gui.pac_update_rx = None;
                    self.set_status(&t!("status.pac_download_failed", err = e.to_string()), cx);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.gui.pac_update_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        // Poll the core process status.
        match self.gui.core_manager.poll() {
            Ok(Some(exit)) => {
                self.set_status(&t!("status.core_exited", code = exit.to_string()), cx);
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = false;
                    ts.active_server_name = String::new();
                }
            }
            Ok(None) if self.gui.core_manager.is_running() => {
                let proto = self
                    .gui
                    .core_manager
                    .current_protocol()
                    .unwrap_or(ProxyProtocol::Juicity)
                    .label();
                let name = self
                    .gui
                    .selected_profile()
                    .map(|p| p.display_name())
                    .unwrap_or_default();
                self.set_status(&t!("status.running", proto = proto, name = name), cx);
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = true;
                    ts.active_server_name = name;
                }
            }
            Err(err) => {
                self.set_status(&t!("status.poll_error", err = err.to_string()), cx);
            }
            _ => {}
        }
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let is_juicity = self.protocol == 0;

        let protocols: Vec<SharedString> = vec![
            t!("protocol.juicity").to_string().into(),
            t!("protocol.shadowsocks").to_string().into(),
        ];
        let methods: Vec<SharedString> =
            SS_METHODS.iter().map(|s| SharedString::from(*s)).collect();

        let selected_profile = self.gui.runtime.selected_profile;
        let server_rows = self.gui.profiles.profiles.iter().enumerate().map({
            let this = this.clone();
            move |(i, p)| {
                let selected = i == selected_profile;
                let this = this.clone();
                let name = p.display_name();
                div()
                    .id(("server-row", i))
                    .px_2()
                    .py_1()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |s| s.bg(rgb(0xddf4ff)).text_color(rgb(0x0969da)))
                    .hover(|s| {
                        s.bg(if selected {
                            rgb(0xddf4ff)
                        } else {
                            rgb(0xf0f3f6)
                        })
                    })
                    .on_click(move |_e, _w, cx| {
                        this.update(cx, |view, cx| view.select_server(i, cx)).ok();
                    })
                    .child(name)
            }
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xf6f8fa))
            .on_mouse_down(MouseButton::Left, {
                let this = this.clone();
                move |_e, _w, cx| {
                    this.update(cx, |view, cx| view.close_dropdown_if_open(cx))
                        .ok();
                }
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .size_full()
                    // ── Left panel: server list ───────────────────────────
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w(px(210.))
                            .flex_none()
                            .h_full()
                            .bg(rgb(0xffffff))
                            .border_r_1()
                            .border_color(rgb(0xd0d7de))
                            .child(
                                div()
                                    .id("server-list")
                                    .flex_grow()
                                    .overflow_y_scroll()
                                    .children(server_rows),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_1()
                                    .p_1()
                                    .child(widgets::button(
                                        "add-btn",
                                        t!("btn.add").to_string(),
                                        false,
                                        with_view(&this, AppView::add_clicked),
                                    ))
                                    .child(widgets::button(
                                        "del-btn",
                                        t!("btn.delete").to_string(),
                                        false,
                                        with_view(&this, AppView::delete_clicked),
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_1()
                                    .px_1()
                                    .pb_1()
                                    .child(widgets::button(
                                        "dup-btn",
                                        t!("btn.duplicate").to_string(),
                                        false,
                                        with_view(&this, AppView::duplicate_clicked),
                                    ))
                                    .child(widgets::button(
                                        "up-btn",
                                        t!("btn.up").to_string(),
                                        false,
                                        with_view(&this, AppView::move_up_clicked),
                                    ))
                                    .child(widgets::button(
                                        "dn-btn",
                                        t!("btn.down").to_string(),
                                        false,
                                        with_view(&this, AppView::move_down_clicked),
                                    )),
                            ),
                    )
                    // ── Right panel: editor ──────────────────────────────
                    .child(
                        div()
                            .id("editor-scroll")
                            .flex_grow()
                            .h_full()
                            .overflow_y_scroll()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .mb_1()
                                    .child(t!("field.server_hdr").to_string()),
                            )
                            .child(widgets::field_row(
                                t!("field.protocol").to_string(),
                                self.dropdown_element(
                                    cx,
                                    DropdownId::Protocol,
                                    "protocol-dropdown",
                                    self.protocol,
                                    protocols,
                                ),
                            ))
                            .child(widgets::field_row(
                                t!("field.server_ip").to_string(),
                                self.server.clone(),
                            ))
                            .child(widgets::field_row(
                                t!("field.server_port").to_string(),
                                self.port.clone(),
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .py_0p5()
                                    .child(
                                        div()
                                            .w(px(130.))
                                            .flex_none()
                                            .text_right()
                                            .text_color(rgb(0x57606a))
                                            .child(t!("field.password").to_string()),
                                    )
                                    .child(self.password.clone())
                                    .child(widgets::checkbox(
                                        "show-pwd-check",
                                        t!("field.show_password").to_string(),
                                        self.show_password,
                                        with_view(&this, AppView::toggle_show_password),
                                    )),
                            )
                            .when(is_juicity, |el| {
                                el.child(separator())
                                    .child(widgets::field_row(
                                        t!("field.uuid").to_string(),
                                        self.uuid.clone(),
                                    ))
                                    .child(widgets::field_row(
                                        t!("field.sni").to_string(),
                                        self.sni.clone(),
                                    ))
                                    .child(div().pl(px(138.)).child(widgets::checkbox(
                                        "allow-insecure-check",
                                        t!("field.allow_insecure").to_string(),
                                        self.allow_insecure,
                                        with_view(&this, AppView::toggle_allow_insecure),
                                    )))
                            })
                            .when(!is_juicity, |el| {
                                el.child(separator())
                                    .child(widgets::field_row(
                                        t!("field.encryption").to_string(),
                                        self.dropdown_element(
                                            cx,
                                            DropdownId::Method,
                                            "method-dropdown",
                                            self.method,
                                            methods,
                                        ),
                                    ))
                                    .child(widgets::field_row(
                                        t!("field.plugin_program").to_string(),
                                        self.plugin.clone(),
                                    ))
                                    .child(widgets::field_row(
                                        t!("field.plugin_options").to_string(),
                                        self.plugin_opts.clone(),
                                    ))
                                    .child(div().pl(px(138.)).child(widgets::checkbox(
                                        "need-plugin-arg-check",
                                        t!("field.need_plugin_arg").to_string(),
                                        self.need_plugin_arg,
                                        with_view(&this, AppView::toggle_need_plugin_arg),
                                    )))
                                    .when(self.need_plugin_arg, |el| {
                                        el.child(widgets::field_row(
                                            t!("field.plugin_args").to_string(),
                                            self.plugin_args.clone(),
                                        ))
                                    })
                            })
                            .child(separator())
                            .child(widgets::field_row(
                                t!("field.remarks").to_string(),
                                self.remarks.clone(),
                            ))
                            .child(widgets::field_row(
                                t!("field.timeout").to_string(),
                                self.timeout.clone(),
                            ))
                            .child(widgets::field_row(
                                t!("field.group").to_string(),
                                self.group.clone(),
                            )),
                    ),
            )
            // ── Status bar ────────────────────────────────────────────────
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(0xd0d7de))
                    .bg(rgb(0xffffff))
                    .child(
                        div()
                            .flex_grow()
                            .text_sm()
                            .text_color(rgb(0x57606a))
                            .child(self.status.clone()),
                    )
                    .child(widgets::button(
                        "start-btn",
                        t!("btn.start").to_string(),
                        false,
                        with_view(&this, AppView::start_selected),
                    ))
                    .child(widgets::button(
                        "stop-btn",
                        t!("btn.stop").to_string(),
                        false,
                        with_view(&this, AppView::stop_core),
                    )),
            )
            // ── Bottom bar ────────────────────────────────────────────────
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(0xd0d7de))
                    .bg(rgb(0xffffff))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x57606a))
                            .child(t!("field.proxy_port").to_string()),
                    )
                    .child(div().w(px(90.)).child(self.proxy_port.clone()))
                    .child(widgets::checkbox(
                        "close-to-tray-check",
                        t!("field.close_to_tray").to_string(),
                        self.close_to_tray,
                        with_view(&this, AppView::toggle_close_to_tray),
                    ))
                    .child(widgets::button(
                        "pac-settings-btn",
                        t!("btn.pac_settings").to_string(),
                        false,
                        {
                            let this = this.clone();
                            move |_e, _w, cx| {
                                crate::pac_dialog::open(&this, cx);
                            }
                        },
                    ))
                    .child(div().flex_grow())
                    .child(widgets::button(
                        "import-btn",
                        t!("btn.import_url").to_string(),
                        false,
                        {
                            let this = this.clone();
                            move |_e, _w, cx| {
                                if let Some(text) =
                                    cx.read_from_clipboard().and_then(|item| item.text())
                                {
                                    this.update(cx, |view, cx| {
                                        view.import_link(&text, cx);
                                    })
                                    .ok();
                                }
                            }
                        },
                    ))
                    .child(widgets::button(
                        "export-btn",
                        t!("btn.export_url").to_string(),
                        false,
                        with_view(&this, AppView::export_link),
                    ))
                    .child(widgets::button("ok-btn", t!("btn.ok").to_string(), true, {
                        let this = this.clone();
                        move |_e, window, cx| {
                            this.update(cx, |view, cx| view.ok_clicked(window, cx)).ok();
                        }
                    }))
                    .child(widgets::button(
                        "cancel-btn",
                        t!("btn.cancel").to_string(),
                        false,
                        {
                            let this = this.clone();
                            move |_e, window, cx| {
                                this.update(cx, |view, cx| view.cancel_clicked(window, cx))
                                    .ok();
                            }
                        },
                    ))
                    .child(widgets::button(
                        "apply-btn",
                        t!("btn.apply").to_string(),
                        false,
                        with_view(&this, AppView::apply_clicked),
                    )),
            )
    }
}

/// Build a click handler that routes to a `&mut self` view method.
fn with_view<F>(
    this: &gpui::WeakEntity<AppView>,
    f: F,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static
where
    F: Fn(&mut AppView, &mut Context<AppView>) + 'static,
{
    let this = this.clone();
    move |_e, _w, cx| {
        this.update(cx, |view, cx| f(view, cx)).ok();
    }
}

/// Thin horizontal separator line.
fn separator() -> impl IntoElement {
    div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).my_1()
}

/// Apply or remove system auto-start for the application.
#[allow(unused_variables)]
fn apply_autostart(state: &RuntimeState) -> anyhow::Result<()> {
    fn autostart_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        std::path::PathBuf::from(&home).join(".config/autostart")
    }

    if !state.auto_start {
        #[cfg(target_os = "linux")]
        {
            let desktop_file = autostart_dir().join("io.juicity.gui.desktop");
            if desktop_file.exists() {
                let _ = std::fs::remove_file(&desktop_file);
            }
        }
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        let dir = autostart_dir();
        std::fs::create_dir_all(&dir)?;

        let exe =
            std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("juicity-gui"));

        let desktop_content = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Juicity GUI\n\
             Comment=Juicity GUI Client\n\
             Exec={}\n\
             Icon=io.juicity.gui\n\
             Terminal=false\n\
             Categories=Network;\n\
             X-GNOME-Autostart-enabled=true\n",
            exe.display()
        );

        let desktop_file = dir.join("io.juicity.gui.desktop");
        std::fs::write(&desktop_file, desktop_content.as_bytes())?;
        tracing::info!(
            "autostart desktop file created at {}",
            desktop_file.display()
        );
    }

    Ok(())
}

fn extract_port(addr: &str) -> u16 {
    addr.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1080)
}

fn non_empty_text(input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub fn run() -> anyhow::Result<()> {
    gpui::Application::new().run(|cx: &mut App| {
        crate::icon::install();
        widgets::bind_text_field_keys(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let view = cx.new(AppView::new);
        cx.default_global::<AppRoot>().view = Some(view.clone());

        cx.on_window_closed({
            let view = view.downgrade();
            move |cx| {
                // Only the first close of the *main* window may trigger the
                // quit path; closing dialog windows must never quit the app.
                if !cx.windows().is_empty() {
                    return;
                }
                let already_closed = cx.default_global::<AppRoot>().main_window_closed;
                if already_closed {
                    return;
                }
                let close_to_tray = view
                    .update(cx, |v, _| v.gui.runtime.close_to_tray)
                    .unwrap_or(false);
                let g = cx.default_global::<AppRoot>();
                g.main_window_closed = true;
                g.main_window = None;
                let suppress = g.suppress_quit;
                g.suppress_quit = false;
                if !suppress && !close_to_tray {
                    cx.quit();
                }
            }
        })
        .detach();

        let hide = view.read(cx).gui.runtime.hide_window_on_startup;
        if hide {
            // Treat the never-shown main window as already closed so that
            // dialog windows (opened from the tray) can be closed freely.
            cx.default_global::<AppRoot>().main_window_closed = true;
        } else {
            open_main_window(cx);
        }

        view.update(cx, |view, cx| view.apply_startup_connection(cx));
        cx.activate(true);
    });
    Ok(())
}
