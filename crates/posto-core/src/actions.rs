//! Email actions: applied on the server first, then mirrored in the store.

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};
use crate::jmap::Client;
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
}

/// What an action changes.
enum Change {
    /// Replace the mailbox set.
    Mailbox(String),
    /// Join and leave individual mailboxes.
    Patch {
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Add (`true`) or remove (`false`) a keyword.
    Keyword(&'static str, bool),
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
/// Ids the server refuses (`notUpdated`) are neither mirrored nor counted.
/// Without a client, only emails the store knows are counted.
///
/// # Errors
///
/// Returns [`Error::NotFound`] when a required role mailbox is missing, and
/// any network, server or database error.
pub async fn apply(
    store: &Store,
    client: Option<&Client>,
    account: &str,
    ids: &[String],
    action: Action,
) -> Result<usize> {
    let change = match action {
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
    let mut refused: Vec<String> = Vec::new();

    if let Some(client) = client.filter(|_| !ids.is_empty()) {
        let patch = match &change {
            Change::Mailbox(id) => json!({ "mailboxIds": { id: true } }),
            Change::Patch { add, remove } => {
                // Removals first so an id in both lists ends up added.
                let mut patch = Map::new();

                for id in remove {
                    patch.insert(format!("mailboxIds/{id}"), Value::Null);
                }

                for id in add {
                    patch.insert(format!("mailboxIds/{id}"), Value::Bool(true));
                }

                Value::Object(patch)
            }
            Change::Keyword(k, on) => json!({ format!("keywords/{k}"): on.then_some(true) }),
        };
        let update: Map<String, Value> = ids.iter().map(|id| (id.clone(), patch.clone())).collect();
        let r = client
            .call_one(
                "Email/set",
                json!({ "accountId": client.session().account_id, "update": update }),
            )
            .await?;

        if let Some(not_updated) = r["notUpdated"].as_object() {
            refused = not_updated.keys().cloned().collect();
        }
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
                store.set_mailbox_ids(account, id, std::slice::from_ref(target))?
            }
            Change::Patch { add, remove } => {
                let mut mailboxes = email.mailbox_ids.clone();

                mailboxes.retain(|m| !remove.contains(m));

                for m in add {
                    if !mailboxes.contains(m) {
                        mailboxes.push(m.clone());
                    }
                }

                store.set_mailbox_ids(account, id, &mailboxes)?
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
