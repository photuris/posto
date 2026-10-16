//! Right-click menu driven by [`MENU`]: one item, "Empty <folder>…".

use dioxus::prelude::*;

use crate::app::{MENU, confirm_empty};

/// A transparent overlay that closes the menu on any click or right-click,
/// with the menu itself at the click position.
#[component]
pub fn ContextMenu() -> Element {
    let Some(menu) = MENU() else {
        return rsx! {};
    };

    rsx! {
        div {
            class: "overlay menu-overlay",
            onclick: |_| *MENU.write() = None,
            oncontextmenu: |evt| {
                evt.prevent_default();
                *MENU.write() = None;
            },
            div {
                class: "menu",
                style: "left: {menu.x}px; top: {menu.y}px;",
                onclick: |evt| evt.stop_propagation(),
                div {
                    class: "picker-row",
                    onclick: |_| confirm_empty(),
                    "Empty {menu.name}…"
                }
            }
        }
    }
}
