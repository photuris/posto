//! Email actions: applied on the server first, then mirrored in the store.

use crate::error::{Error, Result};
use crate::gmail;
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

/// True for the two memberships Gmail never lets a client change.
fn is_sent_or_draft(id: &str) -> bool {
    matches!(id, "SENT" | "DRAFT")
}

/// The Gmail mirror of `mirror`: the email's current `SENT` and `DRAFT`
/// memberships (`current`) are put back when the change lacks them, then
/// [`gmail::visible`] hides what Trash and Spam hide.
fn gmail_mirror(mut mirror: Vec<String>, current: &[String]) -> Vec<String> {
    for fixed in ["SENT", "DRAFT"] {
        if current.iter().any(|m| m == fixed) && !mirror.iter().any(|m| m == fixed) {
            mirror.push(fixed.to_string());
        }
    }

    gmail::visible(&mirror)
}

fn role_mailbox(store: &Store, account: &str, role: &str) -> Result<Change> {
    store
        .mailbox_by_role(account, role)?
        .map(|m| Change::Mailbox(m.id))
        .ok_or_else(|| Error::NotFound(format!("{role} mailbox")))
}

/// Permanently deletes every email the store lists in `mailbox_id` of
/// `account` (meant for Trash and Spam), as [`apply`] with
/// [`Action::Destroy`] would; returns how many went. Mail the store has not
/// synced yet stays until the next sync shows it.
///
/// # Errors
///
/// Any network, server or database error.
pub async fn empty_mailbox(
    store: &Store,
    client: Option<&Connection>,
    account: &str,
    mailbox_id: &str,
) -> Result<usize> {
    let ids = store.email_ids_in_mailbox(account, mailbox_id)?;

    destroy(store, client, account, &ids).await
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
/// An account whose sync state says `provider = proton` or `gmail` (connection
/// or not) follows the folder rules. A message sits in at most one folder (a
/// mailbox with a role) and any number of labels (no role); an id unknown to
/// the store counts as a folder. A move to a label is refused before anything
/// is sent. A move to a folder keeps the
/// message's labels. A patch that adds a folder drops every other folder from
/// the mirror (the last folder in `add` wins), as the server does.
///
/// Gmail never lets a client add or remove `SENT` or `DRAFT`: a move to either,
/// or a patch that adds or removes either, is refused before anything is sent.
/// The Gmail mirror of a move or patch keeps the email's `SENT` and `DRAFT`
/// memberships and passes the result through [`gmail::visible`] (Trash and
/// Spam hide Inbox, Drafts and Sent; `ARCHIVE` is present exactly when no
/// other built-in folder is).
///
/// # Errors
///
/// Returns [`Error::NotFound`] when a required role mailbox is missing,
/// [`Error::Unsupported`] for a move to a label on Proton or Gmail and for a
/// change to Sent or Drafts on Gmail, and any network, server or database
/// error.
pub async fn apply(
    store: &Store,
    client: Option<&Connection>,
    account: &str,
    ids: &[String],
    action: Action,
) -> Result<usize> {
    let provider = store.account_state(account, "provider")?;
    let gmail = provider.as_deref() == Some("gmail");
    let folder_rules = gmail || provider.as_deref() == Some("proton");
    let mailboxes = if folder_rules {
        store.mailboxes(account)?
    } else {
        Vec::new()
    };
    let is_label = |id: &str| mailboxes.iter().any(|m| m.id == id && m.role.is_none());

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
        && folder_rules
        && is_label(target)
    {
        return Err(Error::Unsupported(format!(
            "moving to a label on {} accounts",
            if gmail { "Gmail" } else { "Proton" }
        )));
    }

    if gmail {
        let touches_fixed = |ids: &[String]| ids.iter().any(|m| is_sent_or_draft(m));
        let refused = match &change {
            Change::Mailbox(target) => is_sent_or_draft(target),
            Change::Patch { add, remove } => touches_fixed(add) || touches_fixed(remove),
            Change::Keyword(..) => false,
        };

        if refused {
            return Err(Error::Unsupported(
                "changing Sent or Drafts on Gmail accounts".into(),
            ));
        }
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

                if folder_rules {
                    mirror.extend(email.mailbox_ids.iter().filter(|m| is_label(m)).cloned());
                }

                if gmail {
                    mirror = gmail_mirror(mirror, &email.mailbox_ids);
                }

                store.set_mailbox_ids(account, id, &mirror)?
            }
            Change::Patch { add, remove } => {
                let mut mirror = patched(&email.mailbox_ids, add, remove);

                // The server keeps one folder: the last folder added.
                if folder_rules && let Some(keep) = add.iter().rev().find(|m| !is_label(m)) {
                    mirror.retain(|m| is_label(m) || m == keep);
                }

                if mirror.is_empty() {
                    continue;
                }

                if gmail {
                    mirror = gmail_mirror(mirror, &email.mailbox_ids);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EmailSummary, Mailbox};

    fn mailbox(id: &str, name: &str, role: Option<&str>) -> Mailbox {
        Mailbox {
            account_id: "g".into(),
            id: id.into(),
            name: name.into(),
            role: role.map(String::from),
            parent_id: None,
            sort_order: 1,
            total: 0,
            unread: 0,
        }
    }

    fn email(id: &str, mailbox_ids: &[&str]) -> EmailSummary {
        EmailSummary {
            account_id: "g".into(),
            id: id.into(),
            thread_id: id.into(),
            received_at: "2026-10-20T10:00:00Z".into(),
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            subject: id.into(),
            preview: String::new(),
            size: 1,
            has_attachment: false,
            mailbox_ids: mailbox_ids.iter().map(|m| (*m).to_string()).collect(),
            keywords: Vec::new(),
        }
    }

    /// A Gmail account `g` with one email `m` in `mailbox_ids`.
    fn gmail_store(mailbox_ids: &[&str]) -> Store {
        let store = Store::open_memory().unwrap();

        store.set_account_state("g", "provider", "gmail").unwrap();
        store
            .upsert_mailboxes(&[
                mailbox("INBOX", "Inbox", Some("inbox")),
                mailbox("ARCHIVE", "Archive", Some("archive")),
                mailbox("TRASH", "Trash", Some("trash")),
                mailbox("Label_1", "Receipts", None),
            ])
            .unwrap();
        store.upsert_emails(&[email("m", mailbox_ids)]).unwrap();

        store
    }

    /// Applies `action` to `m` offline and returns the result with the row's
    /// mailboxes afterwards.
    async fn run(store: &Store, action: Action) -> (Result<usize>, Vec<String>) {
        let result = apply(store, None, "g", &["m".to_string()], action).await;
        let after = store
            .email("g", "m")
            .unwrap()
            .map(|e| e.mailbox_ids)
            .unwrap_or_default();

        (result, after)
    }

    #[tokio::test]
    async fn gmail_follows_the_folder_rules() {
        // A move to a label is refused, naming Gmail.
        let store = gmail_store(&["INBOX"]);
        let (result, after) = run(
            &store,
            Action::Move {
                mailbox_id: "Label_1".into(),
            },
        )
        .await;

        assert!(
            matches!(&result, Err(Error::Unsupported(s)) if s.contains("Gmail")),
            "{result:?}"
        );
        assert_eq!(after, ["INBOX"]);

        // Archive keeps labels.
        let store = gmail_store(&["INBOX", "Label_1"]);

        assert_eq!(run(&store, Action::Archive).await.1, ["ARCHIVE", "Label_1"]);

        // A patch adding a folder keeps the labels and that one folder.
        let store = gmail_store(&["ARCHIVE", "Label_1"]);
        let patch = Action::PatchMailboxes {
            add: vec!["TRASH".into()],
            remove: Vec::new(),
        };

        assert_eq!(run(&store, patch).await.1, ["Label_1", "TRASH"]);

        // Archiving a Sent-only message changes nothing: the mirror keeps SENT.
        let store = gmail_store(&["SENT"]);

        assert_eq!(run(&store, Action::Archive).await.1, ["SENT"]);

        // Deleting a Sent message with a label: Trash hides Sent.
        let store = gmail_store(&["SENT", "Label_1"]);

        assert_eq!(run(&store, Action::Delete).await.1, ["TRASH", "Label_1"]);

        // Sent and Drafts cannot be changed, and the row stays as it was.
        let store = gmail_store(&["INBOX"]);
        let (result, after) = run(
            &store,
            Action::Move {
                mailbox_id: "SENT".into(),
            },
        )
        .await;

        assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
        assert_eq!(after, ["INBOX"]);

        let store = gmail_store(&["INBOX", "DRAFT"]);
        let patch = Action::PatchMailboxes {
            add: Vec::new(),
            remove: vec!["DRAFT".into()],
        };
        let (result, after) = run(&store, patch).await;

        assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
        assert_eq!(after, ["INBOX", "DRAFT"]);

        // Destroy is not a mailbox change.
        let store = gmail_store(&["SENT"]);
        let (result, after) = run(&store, Action::Destroy).await;

        assert_eq!(result.unwrap(), 1);
        assert!(after.is_empty());
        assert!(store.email("g", "m").unwrap().is_none());
    }
}
