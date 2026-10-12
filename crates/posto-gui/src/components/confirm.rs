//! Small yes/no modal driven by [`CONFIRM`].

use dioxus::prelude::*;

use crate::app::{CONFIRM, dispatch, focus_root};

/// Closes the dialog; `accept` also runs its action.
fn finish(accept: bool) {
    let pending = CONFIRM.write().take();

    focus_root();

    if let (true, Some(confirm)) = (accept, pending) {
        dispatch(confirm.on_accept);
    }
}

/// Overlay with title, body, Cancel and a highlighted accept button.
/// Enter accepts, Escape or a click outside cancels.
#[component]
pub fn ConfirmDialog() -> Element {
    let Some(confirm) = CONFIRM() else {
        return rsx! {};
    };

    rsx! {
        div { class: "overlay", onclick: |_| finish(false),
            div {
                class: "picker confirm",
                tabindex: "0",
                onclick: |evt| evt.stop_propagation(),
                onmounted: |evt| async move {
                    let _ = evt.set_focus(true).await;
                },
                // Nothing else sees keys while the dialog is open.
                onkeydown: |evt| {
                    evt.stop_propagation();

                    match evt.key() {
                        Key::Enter => {
                            evt.prevent_default();
                            finish(true);
                        }
                        Key::Escape => {
                            evt.prevent_default();
                            finish(false);
                        }
                        _ => {}
                    }
                },
                div { class: "picker-title", "{confirm.title}" }
                div { class: "confirm-body", "{confirm.body}" }
                div { class: "confirm-buttons",
                    button { onclick: |_| finish(false), "Cancel" }
                    button { class: "primary", onclick: |_| finish(true), "{confirm.accept_label}" }
                }
            }
        }
    }
}
