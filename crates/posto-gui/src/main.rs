//! `posto-gui`: Dioxus desktop client.

use dioxus::prelude::*;

fn main() {
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            dioxus::desktop::Config::new()
                .with_window(dioxus::desktop::WindowBuilder::new().with_title("Posto")),
        )
        .launch(App);
}

/// Root component.
#[component]
fn App() -> Element {
    rsx! {
        document::Style { {include_str!("../assets/style.css")} }
        div { class: "app", "Posto" }
    }
}
