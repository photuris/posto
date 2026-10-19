//! Mailbox list.

use dioxus::prelude::*;
use posto_core::store::mailbox_groups;

use crate::app::{
    EXPANDED, FOCUS_SEARCH, MAILBOXES, SEARCH_QUERY, SELECTED_MAILBOX, SHOW_SETTINGS, SidebarRow,
    UNSURE, backend, enabled_ids, open_empty_menu, open_settings, search_input, select_mailbox,
    sidebar_rows, toggle_expanded, toggle_sidebar,
};
use crate::components::{Icon, focus_in, focus_out};
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

/// One clickable sidebar row; `member` rows sit, indented, under an expanded group.
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

/// A mailbox group row: a click selects the merged view, a double click shows
/// or hides the accounts' own mailboxes. Nothing marks a row as expandable:
/// the merged view is the default and the members are there for whoever wants them.
#[component]
fn GroupEntry(
    group_key: String,
    icon: &'static str,
    name: String,
    unread: i64,
    selected: bool,
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
            ondoubleclick: move |_| toggle_expanded(&group_key),
            oncontextmenu: move |evt| on_context_menu(evt, &target, &empty_name),
            Icon { name: icon }
            span { class: "name", "{name}" }
            if unread > 0 {
                span { class: "badge", "{unread}" }
            }
        }
    }
}

/// The top row: the collapse button and the search box.
#[component]
fn SideHead() -> Element {
    rsx! {
        div { class: "side-head",
            button {
                class: "tool",
                title: "Hide sidebar (Ctrl+B)",
                onmousedown: |evt| evt.prevent_default(),
                onclick: |_| toggle_sidebar(),
                Icon { name: "panel-close" }
            }
            input {
                id: "search",
                class: "search",
                r#type: "text",
                placeholder: "Search",
                value: "{SEARCH_QUERY}",
                onmounted: |evt| async move {
                    if *FOCUS_SEARCH.peek() {
                        *FOCUS_SEARCH.write() = false;
                        let _ = evt.set_focus(true).await;
                    }
                },
                onfocusin: focus_in,
                onfocusout: focus_out,
                oninput: |evt| search_input(evt.value()),
            }
        }
    }
}

/// The eyebrow before row `at` of a list whose rows have `roles`: "Mailboxes"
/// above the first row, "Folders" above the first row without a role.
fn eyebrow(roles: &[bool], at: usize) -> Option<&'static str> {
    match (at, roles.get(at)) {
        (0, _) if roles.first() == Some(&true) => Some("Mailboxes"),
        (_, Some(false)) if !roles[..at].contains(&false) => Some("Folders"),
        _ => None,
    }
}

/// Left pane: search, mailboxes with unread badges, and Settings at the foot.
/// With one account the list is flat; with several, one row per mailbox group
/// (a double click lists the accounts' own mailboxes under it). One Unsure
/// row covers every account.
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
    // Single account: the account's mailboxes, those with a role first.
    let mut own: Vec<_> = accounts
        .first()
        .filter(|_| !multi)
        .map(|(id, _, _)| mailboxes.iter().filter(|m| m.account_id == *id).collect())
        .unwrap_or_default();

    own.sort_by_key(|m| m.role.is_none());

    // A member row has its group's role.
    let mut role = false;
    let group_roles: Vec<bool> = rows
        .iter()
        .map(|row| {
            if let SidebarRow::Group { role: r, .. } = row {
                role = r.is_some();
            }
            role
        })
        .collect();
    let own_roles: Vec<bool> = own.iter().map(|m| m.role.is_some()).collect();
    let count: usize = unsure.values().sum();
    let classifying = accounts.iter().any(|(_, _, on)| *on);

    rsx! {
        nav { class: "sidebar",
            SideHead {}
            div { class: "mailboxes",
                for (at , row) in rows.into_iter().enumerate() {
                    if let Some(title) = eyebrow(&group_roles, at) {
                        div { key: "eyebrow-{title}", class: "group-head", "{title}" }
                    }
                    {
                        match row {
                            SidebarRow::Group { key, name, role, unread, .. } => rsx! {
                                GroupEntry {
                                    key: "{key}",
                                    selected: selected == Some(Folder::Merged { key: key.clone() }),
                                    group_key: key,
                                    icon: icon(role.as_deref()),
                                    empty_name: emptyable(role.as_deref(), &name),
                                    name,
                                    unread,
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
                for (at , m) in own.into_iter().enumerate() {
                    if let Some(title) = eyebrow(&own_roles, at) {
                        div { key: "eyebrow-{title}", class: "group-head", "{title}" }
                    }
                    {
                        let folder = Folder::Mailbox {
                            account_id: m.account_id.clone(),
                            id: m.id.clone(),
                        };

                        rsx! {
                            Entry {
                                key: "{m.account_id}/{m.id}",
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
            div { class: "side-foot",
                button {
                    class: if SHOW_SETTINGS() { "nav-item active" } else { "nav-item" },
                    onmousedown: |evt| evt.prevent_default(),
                    onclick: |_| open_settings(),
                    Icon { name: "settings" }
                    "Settings"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eyebrows_head_the_role_rows_and_the_first_folder() {
        let roles = [true, true, false, false];

        assert_eq!(eyebrow(&roles, 0), Some("Mailboxes"));
        assert_eq!(eyebrow(&roles, 1), None);
        assert_eq!(eyebrow(&roles, 2), Some("Folders"));
        assert_eq!(eyebrow(&roles, 3), None);
        assert_eq!(eyebrow(&[false], 0), Some("Folders"));
        assert_eq!(eyebrow(&[], 0), None);
    }
}
