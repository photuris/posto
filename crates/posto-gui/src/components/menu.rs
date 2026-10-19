//! Right-click menu driven by [`MENU`]: one item, "Empty <folder>…".

use dioxus::prelude::*;

use crate::app::{MENU, confirm_empty, focus_root};

/// A transparent overlay that closes the menu on any click or right-click,
/// with the menu itself at the click position.
#[component]
pub fn ContextMenu() -> Element {
    let Some(menu) = MENU() else {
        return rsx! {};
    };

    rsx! {
        div {
            class: "menu-backdrop",
            onclick: |_| *MENU.write() = None,
            oncontextmenu: |evt| {
                evt.prevent_default();
                *MENU.write() = None;
            },
        }
        div {
            class: "menu",
            tabindex: "0",
            style: "left: {menu.x}px; top: {menu.y}px;",
            onmounted: |evt| async move {
                let _ = evt.set_focus(true).await;
            },
            onkeydown: |evt| {
                evt.stop_propagation();

                if evt.key() == Key::Escape {
                    evt.prevent_default();
                    *MENU.write() = None;
                    focus_root();
                }
            },
            button { onclick: |_| confirm_empty(), "Empty {menu.name}…" }
        }
    }
}
