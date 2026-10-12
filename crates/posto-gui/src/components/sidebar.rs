//! Mailbox list.

use dioxus::prelude::*;
use posto_core::store::Mailbox;

use crate::app::{MAILBOXES, SELECTED_MAILBOX, backend, select_mailbox};
use crate::components::Icon;
use crate::state::Folder;

fn icon(m: &Mailbox) -> &'static str {
    match m.role.as_deref() {
        Some("inbox") => "inbox",
        Some("drafts") => "drafts",
        Some("sent") => "sent",
        Some("archive") => "archive",
        Some("junk") => "junk",
        Some("trash") => "trash",
        _ => "folder",
    }
}

/// One clickable sidebar row.
#[component]
fn Entry(icon: &'static str, name: String, unread: i64, selected: bool, folder: Folder) -> Element {
    rsx! {
        div {
            class: if selected { "mailbox selected" } else { "mailbox" },
            onmousedown: |evt| evt.prevent_default(),
            onclick: move |_| select_mailbox(folder.clone()),
            Icon { name: icon }
            span { class: "name", "{name}" }
            if unread > 0 {
                span { class: "badge", "{unread}" }
            }
        }
    }
}

/// Left pane: mailboxes with unread badges. With one account the list is
/// flat; with several, each account gets a header and a merged-inbox entry
/// leads the list.
#[component]
pub fn Sidebar() -> Element {
    let selected = SELECTED_MAILBOX();
    let mailboxes = MAILBOXES();
    let accounts: Vec<(String, String)> = backend()
        .read()
        .cfg
        .accounts_enabled()
        .map(|a| (a.id.clone(), a.display_name().to_string()))
        .collect();
    let multi = accounts.len() > 1;
    let inbox_unread: i64 = mailboxes
        .iter()
        .filter(|m| m.role.as_deref() == Some("inbox"))
        .map(|m| m.unread)
        .sum();

    rsx! {
        nav { class: "sidebar",
            if multi {
                Entry {
                    icon: "inbox",
                    name: "All inboxes",
                    unread: inbox_unread,
                    selected: selected == Some(Folder::AllInboxes),
                    folder: Folder::AllInboxes,
                }
            } else {
                div { class: "sidebar-title", "Mailboxes" }
            }
            for (account_id , account_name) in accounts {
                if multi {
                    div { key: "head-{account_id}", class: "sidebar-group", "{account_name}" }
                }
                for m in mailboxes.iter().filter(|m| m.account_id == account_id) {
                    {
                        let folder = Folder::Mailbox {
                            account_id: m.account_id.clone(),
                            id: m.id.clone(),
                        };

                        rsx! {
                            Entry {
                                key: "{account_id}/{m.id}",
                                icon: icon(m),
                                name: m.name.clone(),
                                unread: m.unread,
                                selected: selected.as_ref() == Some(&folder),
                                folder,
                            }
                        }
                    }
                }
            }
        }
    }
}
