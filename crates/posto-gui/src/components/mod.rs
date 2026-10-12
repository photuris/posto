//! UI components. State lives in the globals of [`crate::app`].

mod composer;
mod message_list;
mod reader;
mod settings;
mod sidebar;
mod toolbar;

use dioxus::prelude::*;

pub use composer::Composer;
pub use message_list::MessageList;
pub use reader::Reader;
pub use settings::Settings;
pub use sidebar::Sidebar;
pub use toolbar::Toolbar;

use crate::app::{MAILBOXES, SHOW_MOVE_PICKER, TYPING, focus_root, move_selected};

/// `onfocusin` for inputs: shortcuts are suspended while typing.
pub fn focus_in(_: FocusEvent) {
    *TYPING.write() = true;
}

/// `onfocusout` for inputs.
pub fn focus_out(_: FocusEvent) {
    *TYPING.write() = false;
}

/// Wraps untrusted HTML for a sandboxed `srcdoc` iframe: links target a new
/// window, colours follow the system scheme.
pub fn frame_doc(inner: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><base target=\"_blank\">\
         <style>:root{{color-scheme:light dark}}\
         body{{margin:16px;font-family:system-ui,sans-serif;overflow-wrap:anywhere}}\
         img{{max-width:100%;height:auto}}</style></head><body>{inner}</body></html>"
    )
}

/// Small filterable list of mailboxes to move the selected email to.
#[component]
pub fn MovePicker() -> Element {
    let mut query = use_signal(String::new);
    let mut index = use_signal(|| 0usize);
    let needle = query().to_lowercase();
    let shown: Vec<_> = MAILBOXES()
        .into_iter()
        .filter(|m| m.name.to_lowercase().contains(&needle))
        .collect();
    let last = shown.len().saturating_sub(1);
    let active = index().min(last);
    let enter_target = shown.get(active).map(|m| m.id.clone());

    rsx! {
        div {
            class: "overlay",
            onclick: |_| {
                *SHOW_MOVE_PICKER.write() = false;
                focus_root();
            },
            div { class: "picker", onclick: |evt| evt.stop_propagation(),
                div { class: "picker-title", "Move to…" }
                input {
                    r#type: "text",
                    placeholder: "Filter mailboxes",
                    value: "{query}",
                    onmounted: |evt| async move {
                        let _ = evt.set_focus(true).await;
                    },
                    onfocusin: focus_in,
                    onfocusout: focus_out,
                    oninput: move |evt| {
                        query.set(evt.value());
                        index.set(0);
                    },
                    onkeydown: move |evt| match evt.key() {
                        Key::ArrowDown => index.set((active + 1).min(last)),
                        Key::ArrowUp => index.set(active.saturating_sub(1)),
                        Key::Enter => {
                            if let Some(id) = enter_target.clone() {
                                move_selected(id);
                            }
                        }
                        _ => {}
                    },
                }
                for (i, m) in shown.into_iter().enumerate() {
                    div {
                        key: "{m.id}",
                        class: if i == active { "picker-row active" } else { "picker-row" },
                        onclick: move |_| move_selected(m.id.clone()),
                        "{m.name}"
                    }
                }
            }
        }
    }
}
