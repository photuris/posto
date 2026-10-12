//! `posto-gui`: Dioxus desktop client over `posto_core`.

mod app;
mod components;
mod format;
mod keys;
mod state;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};

fn main() {
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
