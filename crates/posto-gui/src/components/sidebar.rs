//! Mailbox list.

use dioxus::prelude::*;
use posto_core::store::Mailbox;

use crate::app::{MAILBOXES, SELECTED_MAILBOX, select_mailbox};

fn icon(m: &Mailbox) -> &'static str {
    match m.role.as_deref() {
        Some("inbox") => "📥",
        Some("drafts") => "📝",
        Some("sent") => "📤",
        Some("archive") => "🗄",
        Some("junk") => "🚫",
        Some("trash") => "🗑",
        _ => "📁",
    }
}

/// Left pane: mailboxes with unread badges.
#[component]
pub fn Sidebar() -> Element {
    let selected = SELECTED_MAILBOX();

    rsx! {
        nav { class: "sidebar",
            div { class: "sidebar-title", "Mailboxes" }
            for m in MAILBOXES() {
                div {
                    key: "{m.id}",
                    class: if selected.as_deref() == Some(m.id.as_str()) { "mailbox selected" } else { "mailbox" },
                    onmousedown: |evt| evt.prevent_default(),
                    onclick: {
                        let id = m.id.clone();

                        move |_| select_mailbox(id.clone())
                    },
                    span { class: "icon", {icon(&m)} }
                    span { class: "name", "{m.name}" }
                    if m.unread > 0 {
                        span { class: "badge", "{m.unread}" }
                    }
                }
            }
        }
    }
}
