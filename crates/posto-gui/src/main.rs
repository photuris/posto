//! `posto-gui`: Dioxus desktop client over `posto_core`.

mod app;
mod components;
mod format;
mod keys;
mod state;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};

/// Installs the log subscriber before Dioxus does. `RUST_LOG` overrides the default, which is
/// `info` with the Dioxus edit-socket target silenced: WebKitGTK probes that socket with a plain
/// HTTP request before upgrading, and Dioxus logs the harmless rejection as an error on every start.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,dioxus_desktop::edits=off"));
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
        .with_decorations(false)
        .with_inner_size(LogicalSize::new(1400.0, 900.0))
        .with_min_inner_size(LogicalSize::new(900.0, 600.0));

    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(window).with_menu(None))
        .with_context(backend)
        .launch(app::App);
}
