//! Email actions: applied on the server first, then mirrored in the store.

use crate::error::{Error, Result};
use crate::provider::{Change, Connection};
use crate::store::Store;

/// Something to do to one or more emails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Move to the archive mailbox.
    Archive,
    /// Move to the trash mailbox.
    Delete,
    /// Move to the given mailbox.
    Move {
        /// Target mailbox id.
        mailbox_id: String,
    },
    /// Add the `add` mailboxes and drop the `remove` ones, leaving every other
    /// membership alone.
    PatchMailboxes {
        /// Mailbox ids to join.
        add: Vec<String>,
        /// Mailbox ids to leave.
        remove: Vec<String>,
    },
    /// Add `$seen`.
    MarkRead,
    /// Remove `$seen`.
    MarkUnread,
    /// Add `$flagged`.
    Flag,
    /// Remove `$flagged`.
    Unflag,
    /// Move to the junk mailbox.
    Junk,
    /// Add `$answered`.
    MarkAnswered,
    /// Delete permanently (`Email/set destroy`).
    Destroy,
}

/// True when `id` is in the trash mailbox and nowhere else.
///
/// # Errors
///
/// Returns any database error.
pub fn in_trash(store: &Store, account: &str, id: &str) -> Result<bool> {
    let (Some(trash), Some(email)) = (
        store.mailbox_by_role(account, "trash")?,
        store.email(account, id)?,
    ) else {
        return Ok(false);
    };

    Ok(email.mailbox_ids == [trash.id])
}

/// Destroys `ids` on the server (when `client` is given) and in the store.
async fn destroy(
    store: &Store,
    client: Option<&Connection>,
    account: &str,
    ids: &[String],
) -> Result<usize> {
    let mut gone: Vec<String> = ids.to_vec();

    if let Some(client) = client.filter(|_| !ids.is_empty()) {
        gone = client.destroy(ids).await?;
    }

    // Without a client, only emails the store knows are counted.
    if client.is_none() {
        gone.retain(|id| store.email(account, id).ok().flatten().is_some());
    }

    store.delete_emails(account, &gone)?;

    Ok(gone.len())
}

/// `current` after leaving `remove` and joining `add`.
fn patched(current: &[String], add: &[String], remove: &[String]) -> Vec<String> {
    let mut mailboxes = current.to_vec();

    mailboxes.retain(|m| !remove.contains(m));

    for m in add {
        if !mailboxes.contains(m) {
            mailboxes.push(m.clone());
        }
    }

    mailboxes
}

fn role_mailbox(store: &Store, account: &str, role: &str) -> Result<Change> {
    store
        .mailbox_by_role(account, role)?
        .map(|m| Change::Mailbox(m.id))
        .ok_or_else(|| Error::NotFound(format!("{role} mailbox")))
}

/// Applies `action` to `ids` of `account` on the server (when `client` is given)
/// and in the store. `client` must be the connection of `account`.
///
/// Ids the server refuses are neither mirrored nor counted.
/// Without a client, only emails the store knows are counted.
///
/// A [`Action::PatchMailboxes`] never leaves an email without a mailbox: ids
/// whose local copy would end up empty are dropped before anything is sent and
/// not counted (ids unknown locally are sent; the server enforces its own
/// rule), and the mirror recomputes from the local copy as it is after the
/// server answered, skipping and not counting an id whose result would be empty
/// (another writer changed the row meanwhile; the next sync reconciles).
///
/// An account whose sync state says `provider = proton` (connection or not)
/// follows Proton's folder rules. A message sits in at most one folder (a
/// mailbox with a role) and any number of labels (no role); an id unknown to
/// the store counts as a folder. [`Action::Destroy`] is refused, and so is a
/// move to a label, both before anything is sent. A move to a folder keeps the
/// message's labels. A patch that adds a folder drops every other folder from
/// the mirror (the last folder in `add` wins), as the server does.
///
/// # Errors
///
/// Returns [`Error::NotFound`] when a required role mailbox is missing,
/// [`Error::Unsupported`] for the refused Proton actions, and any network,
/// server or database error.
pub async fn apply(
    store: &Store,
    client: Option<&Connection>,
    account: &str,
    ids: &[String],
    action: Action,
) -> Result<usize> {
    let proton = store.account_state(account, "provider")?.as_deref() == Some("proton");
    let mailboxes = if proton {
        store.mailboxes(account)?
    } else {
        Vec::new()
    };
    let is_label = |id: &str| mailboxes.iter().any(|m| m.id == id && m.role.is_none());

    if proton && action == Action::Destroy {
        return Err(Error::Unsupported(
            "permanent delete on Proton accounts".into(),
        ));
    }

    let change = match action {
        Action::Destroy => return destroy(store, client, account, ids).await,
        Action::Archive => role_mailbox(store, account, "archive")?,
        Action::Delete => role_mailbox(store, account, "trash")?,
        Action::Junk => role_mailbox(store, account, "junk")?,
        Action::Move { mailbox_id } => Change::Mailbox(mailbox_id),
        Action::PatchMailboxes { add, remove } => Change::Patch { add, remove },
        Action::MarkRead => Change::Keyword("$seen", true),
        Action::MarkUnread => Change::Keyword("$seen", false),
        Action::Flag => Change::Keyword("$flagged", true),
        Action::Unflag => Change::Keyword("$flagged", false),
        Action::MarkAnswered => Change::Keyword("$answered", true),
    };

    if let Change::Mailbox(target) = &change
        && proton
        && is_label(target)
    {
        return Err(Error::Unsupported(
            "moving to a label on Proton accounts".into(),
        ));
    }

    let mut refused: Vec<String> = Vec::new();
    let ids: Vec<String> = match &change {
        Change::Patch { add, remove } => {
            let mut keep = Vec::with_capacity(ids.len());

            for id in ids {
                let empties = store
                    .email(account, id)?
                    .is_some_and(|e| patched(&e.mailbox_ids, add, remove).is_empty());

                if !empties {
                    keep.push(id.clone());
                }
            }

            keep
        }
        _ => ids.to_vec(),
    };

    if let Some(client) = client.filter(|_| !ids.is_empty()) {
        refused = client.update(&ids, &change).await?.refused;
    }

    let mut applied = 0;

    for id in ids.iter().filter(|id| !refused.contains(id)) {
        let Some(mut email) = store.email(account, id)? else {
            // Unknown locally: only a server that accepted it makes it applied.
            applied += usize::from(client.is_some());

            continue;
        };

        match &change {
            Change::Mailbox(target) => {
                let mut mirror = vec![target.clone()];

                if proton {
                    mirror.extend(email.mailbox_ids.iter().filter(|m| is_label(m)).cloned());
                }

                store.set_mailbox_ids(account, id, &mirror)?
            }
            Change::Patch { add, remove } => {
                let mut mirror = patched(&email.mailbox_ids, add, remove);

                // The server keeps one folder: the last folder added.
                if proton && let Some(keep) = add.iter().rev().find(|m| !is_label(m)) {
                    mirror.retain(|m| is_label(m) || m == keep);
                }

                if mirror.is_empty() {
                    continue;
                }

                store.set_mailbox_ids(account, id, &mirror)?
            }
            Change::Keyword(k, on) => {
                email.keywords.retain(|x| x != k);

                if *on {
                    email.keywords.push((*k).to_string());
                }

                store.set_keywords(account, id, &email.keywords)?;
            }
        }

        applied += 1;
    }

    Ok(applied)
}
