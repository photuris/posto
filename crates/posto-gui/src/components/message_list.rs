//! Middle pane: one row per email.

use dioxus::prelude::*;

use crate::app::{
    EMAILS, HAS_MORE, MAILBOXES, SEARCH_QUERY, SELECTED_MAILBOX, SELECTION, SIDEBAR_OPEN,
    click_email, load_more,
};
use crate::components::Icon;
use crate::format;
use crate::selection::Click;

/// Avatar, sender, date, subject, preview and mailbox chips for every email in view.
#[component]
pub fn MessageList() -> Element {
    let emails = EMAILS();
    let mailboxes = MAILBOXES();
    let current = SELECTED_MAILBOX();
    let selection = SELECTION();
    let picked = selection.len();
    let searching = !SEARCH_QUERY.read().trim().is_empty();
    let now = chrono::Utc::now();
    let mailbox = mailboxes
        .iter()
        .find(|m| Some(&m.id) == current.as_ref())
        .cloned();
    // Sent and drafts are about who the mail went to.
    let show_recipient = mailbox
        .as_ref()
        .is_some_and(|m| matches!(m.role.as_deref(), Some("sent" | "drafts")));
    let (title, counts) = match (&mailbox, searching) {
        (_, true) => ("Search".to_string(), format!("{} results", emails.len())),
        (Some(m), _) => (
            m.name.clone(),
            format!("{} messages, {} unread", m.total, m.unread),
        ),
        (None, _) => ("Posto".to_string(), String::new()),
    };
    let more = HAS_MORE();

    rsx! {
        section { class: "list",
            header { class: "list-head",
                button {
                    class: "tool",
                    title: "Toggle sidebar",
                    onmousedown: |evt| evt.prevent_default(),
                    onclick: |_| {
                        let open = *SIDEBAR_OPEN.peek();

                        *SIDEBAR_OPEN.write() = !open;
                    },
                    Icon { name: "menu" }
                }
                div {
                    div { class: "list-title", "{title}" }
                    div { class: "list-counts",
                        "{counts}"
                        if picked > 1 {
                            span { class: "sel-count", " · {picked} selected" }
                        }
                    }
                }
            }
            div { class: "rows",
                if emails.is_empty() {
                    div { class: "empty", "No messages" }
                }
                for e in emails {
                    {
                        let people = if show_recipient { &e.to } else { &e.from };
                        let person = people.first();
                        let name = person.map(format::who).unwrap_or("(unknown)").to_string();
                        let seed = person.map_or(name.clone(), |a| a.email.clone());
                        let hue = format::avatar_hue(&seed);
                        let chips: Vec<(String, u16)> = e
                            .mailbox_ids
                            .iter()
                            .filter(|id| Some(*id) != current.as_ref())
                            .filter_map(|id| mailboxes.iter().find(|m| &m.id == id))
                            .map(|m| (m.name.clone(), format::avatar_hue(&m.name)))
                            .collect();
                        let class = match (
                            selection.is_selected(&e.id),
                            selection.focus.as_ref() == Some(&e.id),
                            e.is_unread(),
                        ) {
                            (true, true, _) => "row selected focused",
                            (true, false, _) => "row selected",
                            (false, _, true) => "row unread",
                            (false, _, false) => "row",
                        };
                        let id = e.id.clone();

                        rsx! {
                            div {
                                key: "{e.id}",
                                id: "row-{e.id}",
                                class,
                                onmousedown: |evt| evt.prevent_default(),
                                onclick: move |evt| {
                                    let m = evt.modifiers();
                                    let kind = if m.intersects(Modifiers::META | Modifiers::CONTROL) {
                                        Click::Toggle
                                    } else if m.contains(Modifiers::SHIFT) {
                                        // Keeps the webview from starting a text selection.
                                        evt.prevent_default();
                                        Click::Range
                                    } else {
                                        Click::Plain
                                    };

                                    click_email(&id, kind);
                                },
                                div {
                                    class: "avatar",
                                    style: "background: hsl({hue} 55% 45%)",
                                    {format::initials(&name)}
                                }
                                div { class: "row-main",
                                    div { class: "row-top",
                                        span { class: "sender", "{name}" }
                                        if e.is_unread() {
                                            span { class: "dot", title: "Unread" }
                                        }
                                        if e.is_flagged() {
                                            span { class: "flag", title: "Flagged",
                                                Icon { name: "flag", size: 12 }
                                            }
                                        }
                                        if e.has_attachment {
                                            span { class: "clip", title: "Has attachment",
                                                Icon { name: "paperclip", size: 12 }
                                            }
                                        }
                                        span { class: "date", {format::short_date(&e.received_at, now)} }
                                    }
                                    div { class: "subject", "{e.subject}" }
                                    div { class: "preview", "{e.preview}" }
                                    if !chips.is_empty() {
                                        div { class: "chips",
                                            for (chip, chip_hue) in chips {
                                                span {
                                                    key: "{chip}",
                                                    class: "chip",
                                                    style: "--hue: {chip_hue}",
                                                    "{chip}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if more {
                    button { class: "more", onclick: |_| load_more(), "Load more" }
                }
            }
        }
    }
}
