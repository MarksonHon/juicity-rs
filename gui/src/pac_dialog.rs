//! PAC Settings dialog — lets the user configure:
//!   - Rule list source URLs (direct-list / proxy-list)
//!   - Auto-update interval
//!   - PAC server listen address
//!   - Online PAC URL (bypasses local server)

use adw::prelude::*;
use gtk::prelude::*;
use gtk4 as gtk;
use rust_i18n::t;

use crate::config::AppConfig;
use crate::pac;

/// Open the PAC settings dialog as a modal window on top of `parent`.
///
/// * `cfg`           – current configuration snapshot for initial widget values.
/// * `on_save`       – called with the updated `AppConfig` when the user clicks Save.
/// * `on_update_now` – called (after saving) when the user requests an immediate
///                     rule download.  The caller is responsible for actually
///                     starting the download thread and updating the status label.
pub fn open(
    parent: &gtk::ApplicationWindow,
    cfg: AppConfig,
    on_save: impl Fn(AppConfig) + 'static,
    on_update_now: impl Fn() + 'static,
) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(&*t!("pac_dialog.title"))
        .default_width(520)
        .resizable(false)
        .build();

    // ── Root layout ───────────────────────────────────────────────────────
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();

    // ── Scrollable content ────────────────────────────────────────────────
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .max_content_height(520)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    // ── Rule Lists group ──────────────────────────────────────────────────
    let rules_group = adw::PreferencesGroup::builder()
        .title(&*t!("pac_dialog.group_rules"))
        .build();

    let direct_row = adw::EntryRow::builder()
        .title(&*t!("pac_dialog.direct_url"))
        .text(&cfg.pac_direct_url)
        .build();

    let proxy_row = adw::EntryRow::builder()
        .title(&*t!("pac_dialog.proxy_url"))
        .text(&cfg.pac_proxy_url)
        .build();

    rules_group.add(&direct_row);
    rules_group.add(&proxy_row);
    content.append(&rules_group);

    // ── Auto-update group ─────────────────────────────────────────────────
    let update_group = adw::PreferencesGroup::builder()
        .title(&*t!("pac_dialog.group_update"))
        .build();

    let interval_row = adw::SpinRow::with_range(0.0, 720.0, 1.0);
    interval_row.set_title(&*t!("pac_dialog.update_interval"));
    interval_row.set_value(cfg.pac_auto_update_hours as f64);

    update_group.add(&interval_row);
    content.append(&update_group);

    // ── PAC Server group ──────────────────────────────────────────────────
    let server_group = adw::PreferencesGroup::builder()
        .title(&*t!("pac_dialog.group_server"))
        .build();

    let listen_row = adw::EntryRow::builder()
        .title(&*t!("pac_dialog.listen_addr"))
        .text(&cfg.pac_listen)
        .build();

    let initial_pac_url = pac::pac_url(&cfg.pac_listen);
    let pac_url_row = adw::ActionRow::builder()
        .title(&*t!("pac_dialog.pac_url_label"))
        .subtitle(&initial_pac_url)
        .build();

    // Dynamically update the displayed PAC URL when listen address is edited.
    {
        let pac_url_row = pac_url_row.clone();
        listen_row.connect_changed(move |row| {
            let addr = row.text();
            let url = pac::pac_url(addr.trim());
            pac_url_row.set_subtitle(&url);
        });
    }

    server_group.add(&listen_row);
    server_group.add(&pac_url_row);
    content.append(&server_group);

    // ── Online PAC group ──────────────────────────────────────────────────
    let online_group = adw::PreferencesGroup::builder()
        .title(&*t!("pac_dialog.group_online"))
        .description(&*t!("pac_dialog.online_desc"))
        .build();

    let online_row = adw::EntryRow::builder()
        .title(&*t!("pac_dialog.online_pac_url"))
        .text(cfg.online_pac_url.as_deref().unwrap_or(""))
        .build();

    online_group.add(&online_row);
    content.append(&online_group);

    scroll.set_child(Some(&content));
    root.append(&scroll);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // ── Bottom action bar ─────────────────────────────────────────────────
    let btn_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let update_now_btn = gtk::Button::with_label(&*t!("pac_dialog.update_now"));
    let spacer = gtk::Box::builder().hexpand(true).build();
    let cancel_btn = gtk::Button::with_label(&*t!("btn.cancel"));
    let save_btn = gtk::Button::with_label(&*t!("pac_dialog.save"));
    save_btn.add_css_class("suggested-action");

    btn_bar.append(&update_now_btn);
    btn_bar.append(&spacer);
    btn_bar.append(&cancel_btn);
    btn_bar.append(&save_btn);
    root.append(&btn_bar);

    window.set_child(Some(&root));

    // ── Shared callbacks wrapped in Rc so they can be used in two closures ─
    let on_save = std::rc::Rc::new(on_save);
    let on_update_now = std::rc::Rc::new(on_update_now);

    // Helper: collect current widget values into an AppConfig.
    let collect = {
        let cfg = cfg.clone();
        let direct_row = direct_row.clone();
        let proxy_row = proxy_row.clone();
        let interval_row = interval_row.clone();
        let listen_row = listen_row.clone();
        let online_row = online_row.clone();
        std::rc::Rc::new(move || -> AppConfig {
            let mut c = cfg.clone();
            c.pac_direct_url = direct_row.text().trim().to_string();
            c.pac_proxy_url = proxy_row.text().trim().to_string();
            c.pac_auto_update_hours = interval_row.value().round() as u32;
            c.pac_listen = listen_row.text().trim().to_string();
            let url = online_row.text().trim().to_string();
            c.online_pac_url = if url.is_empty() { None } else { Some(url) };
            c
        })
    };

    // ── Cancel ────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        cancel_btn.connect_clicked(move |_| window.close());
    }

    // ── Save ──────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let collect = std::rc::Rc::clone(&collect);
        let on_save = std::rc::Rc::clone(&on_save);
        save_btn.connect_clicked(move |_| {
            on_save(collect());
            window.close();
        });
    }

    // ── Update Now ────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let collect = std::rc::Rc::clone(&collect);
        let on_save = std::rc::Rc::clone(&on_save);
        let on_update_now = std::rc::Rc::clone(&on_update_now);
        update_now_btn.connect_clicked(move |_| {
            on_save(collect());
            on_update_now();
            window.close();
        });
    }

    window.present();
}
