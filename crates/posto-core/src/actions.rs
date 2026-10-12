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
    /// Add (`true`) or remove (`false`) a keyword.
    Keyword(&'static str, bool),
}

fn role_mailbox(store: &Store, role: &str) -> Result<Change> {
    store
        .mailbox_by_role(role)?
        .map(|m| Change::Mailbox(m.id))
        .ok_or_else(|| Error::NotFound(format!("{role} mailbox")))
}

/// Applies `action` to `ids` on the server (when `client` is given) and in the store.
///
/// Ids the server refuses (`notUpdated`) are neither mirrored nor counted.
///
/// # Errors
///
/// Returns [`Error::NotFound`] when a required role mailbox is missing, and
/// any network, server or database error.
pub async fn apply(
    store: &Store,
    client: Option<&Client>,
    ids: &[String],
    action: Action,
) -> Result<usize> {
    let change = match action {
        Action::Archive => role_mailbox(store, "archive")?,
        Action::Delete => role_mailbox(store, "trash")?,
        Action::Junk => role_mailbox(store, "junk")?,
        Action::Move { mailbox_id } => Change::Mailbox(mailbox_id),
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
        match &change {
            Change::Mailbox(target) => store.set_mailbox_ids(id, std::slice::from_ref(target))?,
            Change::Keyword(k, on) => {
                if let Some(mut email) = store.email(id)? {
                    email.keywords.retain(|x| x != k);

                    if *on {
                        email.keywords.push((*k).to_string());
                    }

                    store.set_keywords(id, &email.keywords)?;
                }
            }
        }

        applied += 1;
    }

    Ok(applied)
}
