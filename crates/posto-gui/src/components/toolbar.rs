//! Top-right toolbar: actions and search.

use dioxus::prelude::*;
use posto_core::shortcuts::Action;

use crate::app::{KEYMAP, SEARCH_QUERY, SHOW_SETTINGS, dispatch, search_input};
use crate::components::{focus_in, focus_out};
use crate::keys;

/// Tooltip such as `Archive (e)`.
fn title(label: &str, action: Action) -> String {
    match KEYMAP.read().chord_for(action) {
        Some(chord) => format!("{label} ({})", keys::chord_text(&chord)),
        None => label.to_string(),
    }
}

/// Toolbar above the reader.
#[component]
pub fn Toolbar() -> Element {
    let buttons = [
        ("✎", "Compose", Action::Compose),
        ("↩", "Reply", Action::Reply),
        ("↩↩", "Reply all", Action::ReplyAll),
        ("🗄", "Archive", Action::Archive),
        ("🚫", "Junk", Action::Junk),
        ("🗑", "Delete", Action::Delete),
        ("⟳", "Sync", Action::Sync),
    ];

    rsx! {
        div { class: "toolbar",
            input {
                id: "search",
                class: "search",
                r#type: "text",
                placeholder: "Search",
                title: title("Search", Action::Search),
                value: "{SEARCH_QUERY}",
                onfocusin: focus_in,
                onfocusout: focus_out,
                oninput: |evt| search_input(evt.value()),
            }
            for (glyph, label, action) in buttons {
                button {
                    key: "{label}",
                    class: "tool",
                    title: title(label, action),
                    onmousedown: |evt| evt.prevent_default(),
                    onclick: move |_| dispatch(action),
                    "{glyph}"
                }
            }
            button {
                class: "tool",
                title: "Settings",
                onmousedown: |evt| evt.prevent_default(),
                onclick: |_| *SHOW_SETTINGS.write() = true,
                "⚙"
            }
        }
    }
}
