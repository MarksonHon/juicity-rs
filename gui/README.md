# Juicity GUI

A [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) based desktop frontend for the Juicity proxy client.

## Implemented

- Main window with Shadowsocks-Windows-like split layout (Servers + Details)
- JSON config persistence in the platform standard config directory
- Protocol-driven core process manager (no manual core type switch):
  - Juicity profile -> `juicity-client run -c <config>`
  - Shadowsocks profile -> `sslocal -c <config>`
- Profile/protocol selectors and a server editor with per-field validation
- URL import/export entry for `juicity://` and `ss://` with parser validation
- System tray support:
  - Linux: StatusNotifierItem via `ksni` (background thread)
  - Windows/macOS: native tray via `tray-icon` (polled on the main loop)
- System proxy apply action (Linux GNOME/KDE implemented, macOS/Windows scaffolded)
- Start/stop and process status polling (300 ms)
- PAC settings dialog and Startup settings dialog

## Config directory

The app uses `directories::ProjectDirs` with:

- Qualifier: `io`
- Organization: `juicity`
- Application: `juicity-gui`

Typical resolved paths:

- Linux: `~/.config/juicity/juicity-gui/`
- macOS: `~/Library/Application Support/io.juicity.juicity-gui/`
- Windows: `%APPDATA%\\io\\juicity\\juicity-gui\\config\\`

JSON files currently used:

- `app.json`
- `profiles.json`
- `runtime.json`

## Build dependencies

GPUI requires a Vulkan-capable display server (Wayland or X11) at runtime and
the following native libraries at build time: X11, xcb, xkbcommon, wayland.

### Linux (Debian/Ubuntu)

```bash
sudo apt update
sudo apt install -y pkg-config libx11-dev libxcb1-dev libxkbcommon-dev \
  libwayland-dev libvulkan-dev mesa-vulkan-drivers
```

### Fedora

```bash
sudo dnf install -y pkgconf-pkg-config libX11-devel libxcb-devel \
  libxkbcommon-devel wayland-devel vulkan-loader-devel mesa-vulkan-drivers
```

### Arch

```bash
sudo pacman -S --needed pkgconf libx11 libxcb libxkbcommon wayland vulkan-icd-loader \
  vulkan-mesa-layer
```

### NixOS

System libraries live in the nix store, so point `LIBRARY_PATH` at them for
linking and `LD_LIBRARY_PATH` for running. Example with a Vulkan software
rasterizer (llvmpipe) on Wayland:

```bash
export LIBRARY_PATH=/nix/store/zyvz6mkqf6iihqr5yfvmfr2inafxdlq4-libxcb-1.17.0/lib:/nix/store/xg73b708qsrdvb82vdwvir097p9w7vr3-libxkbcommon-1.13.2/lib:/nix/store/b4r5xlxclsvy3z6fvvwf74vln5l1hw4y-wayland-1.25.0/lib

export LD_LIBRARY_PATH=$LIBRARY_PATH:/nix/store/xin0b9mlvl6w1qqhvr2nfdcv5qns1b13-vulkan-loader-1.4.350.0/lib:/nix/store/3967gykw3wcyq3svf238nk31jlhxnl7c-mesa-26.1.5/lib
export VK_ICD_FILENAMES=/nix/store/3967gykw3wcyq3svf238nk31jlhxnl7c-mesa-26.1.5/share/vulkan/icd.d/lvp_icd.x86_64.json

cargo build -p juicity-gui
```

(The store hashes depend on the installed nixpkgs revision; resolve them with
`nix-store -q` or use a `pkgs.symlinkJoin` dev shell.)

### macOS

Vulkan is provided via MoltenVK:

```bash
brew install molten-vk
```

### Windows

Ensure a Vulkan-capable driver and runtime (e.g. the Vulkan SDK from LunarG).

## Run

From workspace root:

```bash
cargo run -p juicity-gui
```

On Linux a running StatusNotifierHost (KDE/GNOME) is needed for the tray icon.
