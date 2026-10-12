//! Appearance page: window decorations, column widths and theme.

use dioxus::prelude::*;
use posto_core::config::{Config, UiConfig};
use posto_core::theme;

use crate::app::{LIST_WIDTH, SIDEBAR_WIDTH};

#[component]
pub fn AppearancePage(mut draft: Signal<Config>) -> Element {
    let themed = use_hook(|| theme::detect().is_some());
    let decorations = draft.read().ui.decorations.clone();

    rsx! {
        h3 { "Window decorations" }
        div { class: "setting-row",
            label { "Title bar" }
            select {
                value: "{decorations}",
                onchange: move |evt| draft.write().ui.decorations = evt.value(),
                option { value: "auto", "Auto" }
                option { value: "on", "On" }
                option { value: "off", "Off" }
            }
        }
        div { class: "hint",
            "Auto keeps the native title bar on macOS, Windows and desktop environments, and turns it off under tiling window managers. Takes effect after a restart."
        }

        h3 { "Columns" }
        div { class: "setting-row",
            span { "Mailbox and message-list widths" }
            span { class: "spacer" }
            button {
                onclick: move |_| {
                    let ui = UiConfig::default();

                    *SIDEBAR_WIDTH.write() = ui.sidebar_width;
                    *LIST_WIDTH.write() = ui.list_width;

                    let mut w = draft.write();

                    w.ui.sidebar_width = ui.sidebar_width;
                    w.ui.list_width = ui.list_width;
                },
                "Reset column widths"
            }
        }

        h3 { "Theme" }
        div { class: "hint",
            if themed { "Theme: Omarchy palette detected" } else { "Theme: system colours" }
        }
    }
}
