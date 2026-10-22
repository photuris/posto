//! `posto-gui`: Dioxus desktop client over `posto_core`.

mod app;
mod components;
mod format;
mod keys;
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
mod menu;
mod selection;
mod state;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};

/// Installs the log subscriber before Dioxus does. `RUST_LOG` overrides the default, which is
/// `info` with the Dioxus edit-socket target silenced: WebKitGTK probes that socket with a plain
/// HTTP request before upgrading, and Dioxus logs the harmless rejection as an error on every start.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,dioxus_desktop::edits=off,html5ever=error"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() {
    init_logging();
    let backend = match state::Backend::from_env() {
        Ok(backend) => backend,
        Err(e) => {
            eprintln!("posto-gui: {e}");
            std::process::exit(1);
        }
    };
    let window = WindowBuilder::new()
        .with_title("Posto")
        .with_inner_size(LogicalSize::new(1400.0, 900.0))
        .with_min_inner_size(LogicalSize::new(360.0, 320.0));
    let window = window.with_decorations(want_decorations(&backend.cfg.ui.decorations));

    // `with_window` replaces the whole window builder, so the icon goes on after it.
    let mut cfg = Config::new()
        .with_window(window)
        .with_menu(app_menu(&backend.cfg.shortcuts))
        // Mail links target the top frame. Dioxus hands http(s)/mailto to the
        // browser and blocks them here; anything else a message could point
        // the window at (`data:`, `cid:`) is refused too. `about:` stays so
        // the srcdoc iframes load.
        .with_navigation_handler(|url| url.starts_with("about:"));

    if let Some(icon) = window_icon() {
        cfg = cfg.with_icon(icon);
    }

    dioxus::LaunchBuilder::desktop()
        .with_cfg(cfg)
        .with_context(backend)
        .launch(app::App);
}

/// The window icon: the 256 px Posto icon, decoded at start.
fn window_icon() -> Option<dioxus::desktop::tao::window::Icon> {
    dioxus::desktop::icon_from_memory(include_bytes!("../assets/icon-256.png"))
        .inspect_err(|e| tracing::warn!("window icon: {e}"))
        .ok()
}

/// The native menu bar on macOS and Windows; Linux (tiling sessions) gets none.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn app_menu(
    shortcuts: &std::collections::BTreeMap<String, String>,
) -> Option<dioxus::desktop::muda::Menu> {
    let keymap = posto_core::shortcuts::Keymap::from_config(shortcuts)
        .unwrap_or_else(|_| posto_core::shortcuts::Keymap::defaults());
    Some(menu::install(&keymap))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn app_menu(_: &std::collections::BTreeMap<String, String>) -> Option<dioxus::desktop::muda::Menu> {
    None
}

/// Whether the window should carry a title bar. Config "on"/"off" wins; "auto" keeps native
/// decorations on macOS and Windows, and on Linux turns them off only under a tiling
/// compositor (tao's GTK backend otherwise paints its own header bar there).
fn want_decorations(setting: &str) -> bool {
    match setting {
        "on" => true,
        "off" => false,
        _ => {
            if !cfg!(target_os = "linux") {
                return true;
            }
            let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
            let socket = |k: &str| std::env::var_os(k).is_some();
            !is_tiling_session(
                &desktop,
                socket("HYPRLAND_INSTANCE_SIGNATURE") || socket("SWAYSOCK") || socket("I3SOCK"),
            )
        }
    }
}

/// Heuristic: the session is a tiling window manager when a known tiler socket is set or
/// `XDG_CURRENT_DESKTOP` names one. Unknown desktops count as a DE (decorations stay on).
fn is_tiling_session(xdg_current_desktop: &str, tiler_socket: bool) -> bool {
    const TILERS: &[&str] = &[
        "hyprland",
        "sway",
        "i3",
        "river",
        "niri",
        "bspwm",
        "dwm",
        "qtile",
        "xmonad",
        "awesome",
        "herbstluftwm",
        "leftwm",
        "mango",
        "wayfire",
    ];
    tiler_socket
        || xdg_current_desktop
            .split(':')
            .map(|s| s.trim().to_ascii_lowercase())
            .any(|s| TILERS.contains(&s.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_icon_decodes() {
        assert!(window_icon().is_some());
    }

    #[test]
    fn tiling_detection() {
        assert!(is_tiling_session("Hyprland", false));
        assert!(is_tiling_session("sway", false));
        assert!(is_tiling_session("", true));
        assert!(!is_tiling_session("GNOME", false));
        assert!(!is_tiling_session("KDE", false));
        assert!(!is_tiling_session("COSMIC", false));
        assert!(!is_tiling_session("", false));
        assert!(is_tiling_session("ubuntu:GNOME:i3", false));
    }

    #[test]
    fn explicit_setting_wins() {
        assert!(want_decorations("on"));
        assert!(!want_decorations("off"));
    }
}
