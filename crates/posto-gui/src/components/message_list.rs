//! Middle pane: one row per email.

use dioxus::prelude::*;
use posto_core::classify::Tray;
use posto_core::store::mailbox_groups;

use crate::app::{
    CLASSIFIED, EMAILS, HAS_MORE, MAILBOXES, SEARCH_QUERY, SELECTED_MAILBOX, SELECTION,
    SIDEBAR_OPEN, TRIAGE, backend, click_email, dispatch, enabled_ids, load_more,
    open_message_menu, tray_label,
};
use crate::components::{Icon, SidebarOpen};
use crate::format;
use crate::selection::Click;
use crate::state::{Folder, rows_for_display};
use posto_core::shortcuts::Action;

/// Avatar, sender, date, subject, preview and mailbox chips for every email in view.
#[component]
pub fn MessageList() -> Element {
    let emails = EMAILS();
    let mailboxes = MAILBOXES();
    let current = SELECTED_MAILBOX();
    let selection = SELECTION();
    let classified = CLASSIFIED();
    let cfg = backend().read().cfg.clone();
    let picked = selection.len();
    let searching = !SEARCH_QUERY.read().trim().is_empty();
    let now = chrono::Utc::now();
    let mailbox = match &current {
        Some(Folder::Mailbox { account_id, id }) => mailboxes
            .iter()
            .find(|m| m.account_id == *account_id && m.id == *id)
            .cloned(),
        _ => None,
    };
    let group = match &current {
        Some(Folder::Merged { key }) => mailbox_groups(&mailboxes, &enabled_ids(&cfg))
            .into_iter()
            .find(|g| g.key == *key),
        _ => None,
    };
    let accounts: Vec<(String, String)> = cfg
        .accounts_enabled()
        .map(|a| (a.id.clone(), a.display_name().to_string()))
        .collect();
    // Rows of several accounts say which account they belong to.
    let tag_accounts =
        accounts.len() > 1 && (group.is_some() || searching || current == Some(Folder::Triage));
    // Sent and drafts are about who the mail went to.
    let folder_role = mailbox
        .as_ref()
        .and_then(|m| m.role.as_deref())
        .or(group.as_ref().and_then(|g| g.role.as_deref()));
    let show_recipient = matches!(folder_role, Some("sent" | "drafts"));
    let (title, counts) = match (&mailbox, searching) {
        (_, true) => ("Search".to_string(), format!("{} results", emails.len())),
        (None, _) => match &current {
            Some(Folder::Triage) => ("Triage".to_string(), format!("{} to deal with", TRIAGE())),
            Some(Folder::Unsure) => (
                "Unsure".to_string(),
                format!("{} messages to review", emails.len()),
            ),
            _ => match &group {
                Some(g) => (
                    g.name.clone(),
                    format!("{} messages, {} unread", g.total, g.unread),
                ),
                None => ("Posto".to_string(), String::new()),
            },
        },
        (Some(m), _) => (
            m.name.clone(),
            format!("{} messages, {} unread", m.total, m.unread),
        ),
    };
    let more = HAS_MORE();

    rsx! {
        section { class: "list",
            header { class: "list-head",
                if !SIDEBAR_OPEN() {
                    SidebarOpen {}
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
                    div { class: "empty",
                        if current == Some(Folder::Triage) && !searching {
                            "Nothing owed"
                        } else {
                            "No messages"
                        }
                    }
                }
                for (key, e) in rows_for_display(&emails) {
                    {
                        let people = if show_recipient { &e.to } else { &e.from };
                        let person = people.first();
                        let name = person.map(format::who).unwrap_or("(unknown)").to_string();
                        let seed = person.map_or(name.clone(), |a| a.email.clone());
                        let hue = format::avatar_hue(&seed);
                        let account_chip = tag_accounts
                            .then(|| accounts.iter().find(|(id, _)| *id == e.account_id))
                            .flatten()
                            .map(|(_, name)| (name.clone(), format::avatar_hue(name)));
                        let chips: Vec<(String, u16)> = account_chip
                            .into_iter()
                            .chain(
                                e.mailbox_ids
                                    .iter()
                                    .filter_map(|id| {
                                        mailboxes
                                            .iter()
                                            .find(|m| m.account_id == e.account_id && &m.id == id)
                                    })
                                    .filter(|m| match &current {
                                        Some(Folder::Mailbox { account_id, id }) => {
                                            !(m.account_id == *account_id && m.id == *id)
                                        }
                                        Some(Folder::Merged { .. }) => !group.as_ref().is_some_and(|g| {
                                            g.members.iter().any(|(a, id)| *a == m.account_id && *id == m.id)
                                        }),
                                        Some(Folder::Unsure | Folder::Triage) | None => true,
                                    })
                                    .map(|m| (m.name.clone(), format::avatar_hue(&m.name))),
                            )
                            .collect();
                        // The tray's mailbox name, unless a mailbox chip or the open folder says it.
                        let (tray_chip, unsure_chip) = match classified.get(&key) {
                            Some((tray, unsure)) => {
                                let name = Tray::parse_in(&cfg.classify, tray).ok().map(|t| tray_label(&cfg, &t));
                                let shown = name.filter(|n| {
                                    !chips.iter().any(|(c, _)| c.eq_ignore_ascii_case(n))
                                        && mailbox.as_ref().is_none_or(|m| !m.name.eq_ignore_ascii_case(n))
                                        && group.as_ref().is_none_or(|g| {
                                            // Grouping trims and lower-cases names; so does this.
                                            g.key != format!("name:{}", n.trim().to_lowercase())
                                                && !g.name.eq_ignore_ascii_case(n)
                                        })
                                });

                                (shown, *unsure)
                            }
                            None => (None, false),
                        };
                        // Unread is the only thing that bolds a row; selection and focus
                        // are shown by background and outline alone.
                        let mut class = String::from("row");
                        if selection.is_selected(&key) {
                            class.push_str(" selected");
                        }
                        if selection.focus.as_deref() == Some(key.as_str()) {
                            class.push_str(" focused");
                        }
                        if e.is_unread() {
                            class.push_str(" unread");
                        }
                        let row_id = key.clone();
                        let menu_id = key.clone();
                        let archive_id = key.clone();
                        let delete_id = key.clone();

                        rsx! {
                            div {
                                key: "{key}",
                                id: "row-{key}",
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

                                    click_email(&row_id, kind);
                                },
                                oncontextmenu: move |evt| {
                                    let at = evt.client_coordinates();

                                    evt.prevent_default();
                                    evt.stop_propagation();
                                    open_message_menu(&menu_id, at.x, at.y);
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
                                        span { class: "row-actions",
                                            button {
                                                class: "row-action",
                                                title: "Archive",
                                                onmousedown: |evt| evt.prevent_default(),
                                                onclick: move |evt| {
                                                    evt.stop_propagation();
                                                    click_email(&archive_id, Click::Plain);
                                                    dispatch(Action::Archive);
                                                },
                                                Icon { name: "archive", size: 14 }
                                            }
                                            button {
                                                class: "row-action",
                                                title: "Delete",
                                                onmousedown: |evt| evt.prevent_default(),
                                                onclick: move |evt| {
                                                    evt.stop_propagation();
                                                    click_email(&delete_id, Click::Plain);
                                                    dispatch(Action::Delete);
                                                },
                                                Icon { name: "trash", size: 14 }
                                            }
                                        }
                                    }
                                    div { class: "subject", "{e.subject}" }
                                    div { class: "preview", "{e.preview}" }
                                    if !chips.is_empty() || tray_chip.is_some() || unsure_chip {
                                        div { class: "chips",
                                            for (i, (chip, chip_hue)) in chips.into_iter().enumerate() {
                                                span {
                                                    key: "{i}",
                                                    class: "chip",
                                                    style: "--hue: {chip_hue}",
                                                    "{chip}"
                                                }
                                            }
                                            if let Some(name) = tray_chip {
                                                span { class: "chip muted", title: "Classified as", "{name}" }
                                            }
                                            if unsure_chip {
                                                span { class: "chip unsure", title: "Jev was not sure; review it", "Unsure" }
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
