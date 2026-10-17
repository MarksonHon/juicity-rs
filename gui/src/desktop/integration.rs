//! Desktop integration: the application icon in the system's menus.
//!
//! Linux writes an XDG desktop entry and hicolor icons for the current user,
//! so Wayland compositors (which resolve the window icon from the app id
//! `io.juicity.gui`) find it; this is skipped when a system-wide desktop file
//! already exists. Windows writes a Start Menu shortcut to the executable.
//! Both are installed on demand from the Settings page.

#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::path::{Path, PathBuf};

#[cfg(any(target_os = "linux", target_os = "windows"))]
use anyhow::Context as _;

#[cfg(target_os = "linux")]
use super::autostart::exec_argument;

/// Application id; also the base name of the desktop file and the icons.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const APP_ID: &str = "io.juicity.gui";
/// Display name of the application and its shortcut.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const APP_NAME: &str = "Juicity GUI";

/// Whether the Settings button can install the app icon on this platform.
pub const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "windows"));

/// Install the desktop entry and icons (Linux) or the Start Menu shortcut
/// (Windows) for the current user.
pub fn install() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        install_linux()
    }
    #[cfg(target_os = "windows")]
    {
        install_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        anyhow::bail!("installing the app icon is not supported on this platform")
    }
}

/// Directories of system-wide installs; when one has our desktop file, the
/// package owns the integration.
#[cfg(target_os = "linux")]
const SYSTEM_DIRS: [&str; 2] = ["/usr/share/applications", "/usr/local/share/applications"];

#[cfg(target_os = "linux")]
const DESKTOP_TEMPLATE: &str = include_str!("../../linux/io.juicity.gui.desktop");

#[cfg(target_os = "linux")]
const SVG: &[u8] = include_bytes!("../../icon.svg");

/// Raster icons rendered by `build.rs`, keyed by edge length in pixels.
#[cfg(target_os = "linux")]
const PNGS: [(u32, &[u8]); 7] = [
    (16, include_bytes!(concat!(env!("OUT_DIR"), "/16.png"))),
    (32, include_bytes!(concat!(env!("OUT_DIR"), "/32.png"))),
    (48, include_bytes!(concat!(env!("OUT_DIR"), "/48.png"))),
    (64, include_bytes!(concat!(env!("OUT_DIR"), "/64.png"))),
    (128, include_bytes!(concat!(env!("OUT_DIR"), "/128.png"))),
    (256, include_bytes!(concat!(env!("OUT_DIR"), "/256.png"))),
    (512, include_bytes!(concat!(env!("OUT_DIR"), "/512.png"))),
];

/// Write the desktop entry and icons below `$XDG_DATA_HOME`.
#[cfg(target_os = "linux")]
fn install_linux() -> anyhow::Result<()> {
    let data_home = data_home(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))?;
    let exe = std::env::current_exe().context("cannot resolve the current executable")?;
    let system_dirs: Vec<&Path> = SYSTEM_DIRS.iter().map(Path::new).collect();
    if install_in(&data_home, &system_dirs, &exe)? {
        tracing::info!("desktop entry and icons installed");
    }
    Ok(())
}

/// `$XDG_DATA_HOME`, or `$HOME/.local/share` when it is unset, empty or
/// relative (XDG Base Directory rules).
#[cfg(target_os = "linux")]
fn data_home(xdg_data: Option<OsString>, home: Option<OsString>) -> anyhow::Result<PathBuf> {
    xdg_data
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
        .ok_or_else(|| anyhow::anyhow!("neither XDG_DATA_HOME nor HOME is set"))
}

/// Write the desktop file and icons below `data_home`. Returns whether
/// anything was written; skips everything when a `system_dirs` entry already
/// has the desktop file.
#[cfg(target_os = "linux")]
fn install_in(data_home: &Path, system_dirs: &[&Path], exe: &Path) -> anyhow::Result<bool> {
    let desktop_name = format!("{APP_ID}.desktop");
    if system_dirs
        .iter()
        .any(|dir| dir.join(&desktop_name).exists())
    {
        return Ok(false);
    }
    let icons = data_home.join("icons/hicolor");
    let mut wrote = false;
    for (size, png) in PNGS {
        let path = icons.join(format!("{size}x{size}/apps/{APP_ID}.png"));
        wrote |= write_if_changed(&path, png)?;
    }
    let svg = icons.join(format!("scalable/apps/{APP_ID}.svg"));
    wrote |= write_if_changed(&svg, SVG)?;
    let desktop = data_home.join("applications").join(&desktop_name);
    wrote |= write_if_changed(&desktop, desktop_entry(exe).as_bytes())?;
    Ok(wrote)
}

/// The packaged desktop file with `Exec=` pointing at `exe`.
#[cfg(target_os = "linux")]
fn desktop_entry(exe: &Path) -> String {
    let exec = format!("Exec={}", exec_argument(&exe.to_string_lossy()));
    let mut out = String::new();
    for line in DESKTOP_TEMPLATE.lines() {
        out.push_str(if line.starts_with("Exec=") {
            &exec
        } else {
            line
        });
        out.push('\n');
    }
    out
}

/// Write `content` to `path` through a temp file and a rename in the same
/// directory, unless the file already holds it.
#[cfg(target_os = "linux")]
fn write_if_changed(path: &Path, content: &[u8]) -> anyhow::Result<bool> {
    if std::fs::read(path).is_ok_and(|current| current == content) {
        return Ok(false);
    }
    let dir = path.parent().context("path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let name = path.file_name().context("path has no file name")?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp = dir.join(tmp_name);
    let result = std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(err) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("cannot write {}", path.display()));
    }
    Ok(true)
}

/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs`.
#[cfg(target_os = "windows")]
fn start_menu_dir() -> anyhow::Result<PathBuf> {
    let appdata =
        std::env::var_os("APPDATA").ok_or_else(|| anyhow::anyhow!("APPDATA is not set"))?;
    Ok(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs"))
}

/// Write a Start Menu shortcut to the current executable.
#[cfg(target_os = "windows")]
fn install_windows() -> anyhow::Result<()> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

    let programs = start_menu_dir()?;
    std::fs::create_dir_all(&programs)
        .with_context(|| format!("cannot create {}", programs.display()))?;
    let exe = std::env::current_exe().context("cannot resolve the current executable")?;
    let link = programs.join(format!("{APP_NAME}.lnk"));

    // COM must be initialized on this thread before IShellLink can be created;
    // S_FALSE means it already was, and then the apartment is not ours to close.
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result = write_shortcut(&exe, &link);
    if initialized {
        unsafe { CoUninitialize() };
    }
    result.with_context(|| format!("cannot write {}", link.display()))?;
    tracing::info!("Start Menu shortcut created at {}", link.display());
    Ok(())
}

/// Create the `.lnk` at `link` pointing at `exe`.
#[cfg(target_os = "windows")]
fn write_shortcut(exe: &Path, link: &Path) -> anyhow::Result<()> {
    use windows::core::{Interface as _, HSTRING};
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    let shortcut: IShellLinkW =
        unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
    unsafe {
        shortcut.SetPath(&HSTRING::from(exe))?;
        shortcut.SetDescription(&HSTRING::from(APP_NAME))?;
        if let Some(dir) = exe.parent() {
            shortcut.SetWorkingDirectory(&HSTRING::from(dir))?;
        }
    }
    let file: IPersistFile = shortcut.cast()?;
    unsafe { file.Save(&HSTRING::from(link), true)? };
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("juicity-integration-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn desktop_path(data: &Path) -> PathBuf {
        data.join("applications").join("io.juicity.gui.desktop")
    }

    #[test]
    fn data_home_follows_xdg_then_home() {
        let dir = |xdg: Option<&str>, home: Option<&str>| {
            data_home(xdg.map(Into::into), home.map(Into::into)).ok()
        };
        assert_eq!(dir(Some("/data"), Some("/h")), Some(PathBuf::from("/data")));
        assert_eq!(
            dir(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.local/share"))
        );
        assert_eq!(
            dir(None, Some("/h")),
            Some(PathBuf::from("/h/.local/share"))
        );
        assert_eq!(dir(None, None), None);
    }

    #[test]
    fn first_call_writes_everything_and_second_is_a_no_op() {
        let data = temp("first");
        let exe = Path::new("/usr/bin/juicity-gui");
        assert!(install_in(&data, &[], exe).unwrap());
        for size in [16, 32, 48, 64, 128, 256, 512] {
            let png = data.join(format!(
                "icons/hicolor/{size}x{size}/apps/io.juicity.gui.png"
            ));
            assert!(png.is_file(), "{}", png.display());
        }
        assert!(data
            .join("icons/hicolor/scalable/apps/io.juicity.gui.svg")
            .is_file());
        let entry = std::fs::read_to_string(desktop_path(&data)).unwrap();
        assert!(entry.contains("Exec=/usr/bin/juicity-gui\n"), "{entry}");
        assert!(entry.contains("Icon=io.juicity.gui\n"), "{entry}");
        assert!(!install_in(&data, &[], exe).unwrap());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn changed_exe_rewrites_only_the_desktop_file() {
        let data = temp("exe");
        assert!(install_in(&data, &[], Path::new("/usr/bin/juicity-gui")).unwrap());
        let png = data.join("icons/hicolor/16x16/apps/io.juicity.gui.png");
        // A rewrite would replace the old modification time.
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&png)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let new_exe = Path::new("/opt/juicity/juicity-gui");
        assert!(install_in(&data, &[], new_exe).unwrap());
        assert_eq!(std::fs::metadata(&png).unwrap().modified().unwrap(), old);
        let entry = std::fs::read_to_string(desktop_path(&data)).unwrap();
        assert!(entry.contains("Exec=/opt/juicity/juicity-gui\n"), "{entry}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn exec_is_escaped_for_paths_with_spaces() {
        let data = temp("spaces");
        let exe = Path::new("/opt/My Apps/100%/juicity-gui");
        assert!(install_in(&data, &[], exe).unwrap());
        let entry = std::fs::read_to_string(desktop_path(&data)).unwrap();
        assert!(
            entry.contains("Exec=\"/opt/My Apps/100%%/juicity-gui\"\n"),
            "{entry}"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn system_install_skips_everything() {
        let root = temp("system");
        let system = root.join("system");
        std::fs::create_dir_all(&system).unwrap();
        std::fs::write(system.join("io.juicity.gui.desktop"), "x").unwrap();
        let data = root.join("data");
        let wrote = install_in(&data, &[&system], Path::new("/usr/bin/juicity-gui")).unwrap();
        assert!(!wrote);
        assert!(!data.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
