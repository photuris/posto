//! Mailbox list.

use dioxus::prelude::*;
use posto_core::store::mailbox_groups;

use crate::app::{
    EXPANDED, MAILBOXES, SELECTED_MAILBOX, SidebarRow, UNSURE, backend, enabled_ids,
    open_empty_menu, select_mailbox, sidebar_rows, toggle_expanded,
};
use crate::components::Icon;
use crate::state::Folder;

/// The icon of a mailbox with this role.
fn icon(role: Option<&str>) -> &'static str {
    match role {
        Some("inbox") => "inbox",
        Some("drafts") => "drafts",
        Some("sent") => "sent",
        Some("archive") => "archive",
        Some("junk") => "junk",
        Some("trash") => "trash",
        _ => "folder",
    }
}

/// The name a Trash or Spam row's "Empty …" menu shows; `None` for other roles.
fn emptyable(role: Option<&str>, name: &str) -> Option<String> {
    matches!(role, Some("trash" | "junk")).then(|| name.to_string())
}

/// Opens the "Empty …" menu for `folder` at the pointer.
fn on_context_menu(evt: MouseEvent, folder: &Folder, name: &Option<String>) {
    if let Some(name) = name {
        let at = evt.client_coordinates();

        evt.prevent_default();
        evt.stop_propagation();
        open_empty_menu(folder.clone(), name.clone(), at.x, at.y);
    }
}

/// One clickable sidebar row; `member` rows sit under an expanded group.
/// `empty_name` (Trash and Spam only) enables the right-click "Empty …" menu.
#[component]
fn Entry(
    icon: &'static str,
    name: String,
    unread: i64,
    selected: bool,
    folder: Folder,
    #[props(default)] member: bool,
    #[props(default)] empty_name: Option<String>,
) -> Element {
    let class = match (selected, member) {
        (true, true) => "mailbox member selected",
        (true, false) => "mailbox selected",
        (false, true) => "mailbox member",
        (false, false) => "mailbox",
    };
    let target = folder.clone();

    rsx! {
        div {
            class,
            onmousedown: |evt| evt.prevent_default(),
            onclick: move |_| select_mailbox(folder.clone()),
            oncontextmenu: move |evt| on_context_menu(evt, &target, &empty_name),
            Icon { name: icon }
            span { class: "name", "{name}" }
            if unread > 0 {
                span { class: "badge", "{unread}" }
            }
        }
    }
}

/// A mailbox group row: selects the merged view; its disclosure button shows
/// or hides the members without selecting the group.
#[component]
fn GroupEntry(
    group_key: String,
    icon: &'static str,
    name: String,
    unread: i64,
    selected: bool,
    expanded: bool,
    #[props(default)] empty_name: Option<String>,
) -> Element {
    let folder = Folder::Merged {
        key: group_key.clone(),
    };
    let target = folder.clone();

    rsx! {
        div {
            class: if selected { "mailbox selected" } else { "mailbox" },
            onmousedown: |evt| evt.prevent_default(),
            onclick: move |_| select_mailbox(folder.clone()),
            oncontextmenu: move |evt| on_context_menu(evt, &target, &empty_name),
            button {
                class: "disclosure",
                title: if expanded { "Hide accounts" } else { "Show accounts" },
                onmousedown: |evt| evt.prevent_default(),
                onclick: move |evt| {
                    evt.stop_propagation();
                    toggle_expanded(&group_key);
                },
                if expanded {
                    "▾"
                } else {
                    "▸"
                }
            }
            Icon { name: icon }
            span { class: "name", "{name}" }
            if unread > 0 {
                span { class: "badge", "{unread}" }
            }
        }
    }
}

/// Left pane: mailboxes with unread badges. With one account the list is
/// flat; with several, one row per mailbox group (expandable into the
/// accounts' own mailboxes). One Unsure row covers every account.
#[component]
pub fn Sidebar() -> Element {
    let selected = SELECTED_MAILBOX();
    let mailboxes = MAILBOXES();
    let unsure = UNSURE();
    let expanded = EXPANDED();
    // The third field: whether the classifier is on for the account.
    let accounts: Vec<(String, String, bool)> = {
        let b = backend();
        let b = b.read();

        b.cfg
            .accounts_enabled()
            .map(|a| {
                (
                    a.id.clone(),
                    a.display_name().to_string(),
                    b.cfg.classify_enabled_for(a),
                )
            })
            .collect()
    };
    let multi = accounts.len() > 1;
    let rows = if multi {
        let names: Vec<(String, String)> = accounts
            .iter()
            .map(|(id, name, _)| (id.clone(), name.clone()))
            .collect();
        let ids = enabled_ids(&backend().read().cfg);

        sidebar_rows(
            &mailbox_groups(&mailboxes, &ids),
            &expanded,
            &names,
            &mailboxes,
        )
    } else {
        Vec::new()
    };

    rsx! {
        nav { class: "sidebar",
            if !multi {
                div { class: "sidebar-title", "Mailboxes" }
            }
            for row in rows {
                {
                    match row {
                        SidebarRow::Group { key, name, role, unread, expanded } => rsx! {
                            GroupEntry {
                                key: "{key}",
                                selected: selected == Some(Folder::Merged { key: key.clone() }),
                                group_key: key,
                                icon: icon(role.as_deref()),
                                empty_name: emptyable(role.as_deref(), &name),
                                name,
                                unread,
                                expanded,
                            }
                        },
                        SidebarRow::Member { folder, label, unread } => {
                            let key = format!("{folder:?}");
                            let empty_name = match &folder {
                                Folder::Mailbox { account_id, id } => mailboxes
                                    .iter()
                                    .find(|m| m.account_id == *account_id && m.id == *id)
                                    .and_then(|m| emptyable(m.role.as_deref(), &label)),
                                _ => None,
                            };

                            rsx! {
                                Entry {
                                    key: "{key}",
                                    icon: "folder",
                                    name: label,
                                    unread,
                                    selected: selected.as_ref() == Some(&folder),
                                    folder,
                                    member: true,
                                    empty_name,
                                }
                            }
                        }
                    }
                }
            }
            for (account_id , _ , _) in accounts.iter() {
                if !multi {
                    for m in mailboxes.iter().filter(|m| m.account_id == *account_id) {
                        {
                            let folder = Folder::Mailbox {
                                account_id: m.account_id.clone(),
                                id: m.id.clone(),
                            };

                            rsx! {
                                Entry {
                                    key: "{account_id}/{m.id}",
                                    icon: icon(m.role.as_deref()),
                                    name: m.name.clone(),
                                    unread: m.unread,
                                    selected: selected.as_ref() == Some(&folder),
                                    folder,
                                    empty_name: emptyable(m.role.as_deref(), &m.name),
                                }
                            }
                        }
                    }
                }
            }
            {
                let count: usize = unsure.values().sum();
                let classifying = accounts.iter().any(|(_, _, on)| *on);

                rsx! {
                    if classifying || count > 0 {
                        Entry {
                            icon: "flag",
                            name: "Unsure",
                            unread: count as i64,
                            selected: selected == Some(Folder::Unsure),
                            folder: Folder::Unsure,
                        }
                    }
                }
            }
        }
    }
}
