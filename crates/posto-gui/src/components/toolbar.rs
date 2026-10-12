//! Top-right toolbar: actions and search.

use dioxus::prelude::*;
use posto_core::shortcuts::Action;

use crate::app::{KEYMAP, SEARCH_QUERY, SELECTION, SHOW_SETTINGS, dispatch, search_input};
use crate::components::{Icon, focus_in, focus_out};
use crate::keys;

/// Tooltip such as `Archive (e)`.
fn title(label: &str, action: Action) -> String {
    let mut text = match KEYMAP.read().chord_for(action) {
        Some(chord) => format!("{label} ({})", keys::chord_text(&chord)),
        None => label.to_string(),
    };
    let picked = SELECTION.read().len();

    if picked > 1 && matches!(action, Action::Archive | Action::Junk | Action::Delete) {
        text.push_str(&format!(" ({picked} selected)"));
    }

    text
}

/// Toolbar above the reader.
#[component]
pub fn Toolbar() -> Element {
    let buttons = [
        ("compose", "Compose", Action::Compose),
        ("reply", "Reply", Action::Reply),
        ("reply-all", "Reply all", Action::ReplyAll),
        ("archive", "Archive", Action::Archive),
        ("junk", "Junk", Action::Junk),
        ("trash", "Delete", Action::Delete),
        ("refresh", "Sync", Action::Sync),
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
            for (icon, label, action) in buttons {
                button {
                    key: "{label}",
                    class: "tool",
                    title: title(label, action),
                    onmousedown: |evt| evt.prevent_default(),
                    onclick: move |_| dispatch(action),
                    Icon { name: icon }
                }
            }
            button {
                class: "tool",
                title: "Settings",
                onmousedown: |evt| evt.prevent_default(),
                onclick: |_| *SHOW_SETTINGS.write() = true,
                Icon { name: "settings" }
            }
        }
    }
}
