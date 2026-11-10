//! Email actions: applied on the server first (or locally first, see
//! [`apply_local`]), then mirrored in the store.

use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::gmail;
use crate::provider::{Change, Connection};
use crate::store::{EmailSummary, Mailbox, Store};

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

/// The provider rules of one account: whether it follows the Proton or Gmail
/// folder rules, and which of its mailboxes are labels. [`apply`] and
/// [`apply_local`] share them, so the two never disagree.
struct Rules {
    gmail: bool,
    folder_rules: bool,
    mailboxes: Vec<Mailbox>,
}

/// The new state of one row, as [`Rules::mirror`] computes it.
enum Mirror {
    Mailboxes(Vec<String>),
    Keywords(Vec<String>),
}

impl Rules {
    fn load(store: &Store, account: &str) -> Result<Self> {
        let provider = store.account_state(account, "provider")?;
        let gmail = provider.as_deref() == Some("gmail");
        let folder_rules = gmail || provider.as_deref() == Some("proton");
        let mailboxes = if folder_rules {
            store.mailboxes(account)?
        } else {
            Vec::new()
        };

        Ok(Self {
            gmail,
            folder_rules,
            mailboxes,
        })
    }

    fn is_label(&self, id: &str) -> bool {
        self.mailboxes
            .iter()
            .any(|m| m.id == id && m.role.is_none())
    }

    /// The change `action` stands for, after the checks that precede any
    /// write. [`Action::Destroy`] is not a change.
    fn change(&self, store: &Store, account: &str, action: Action) -> Result<Change> {
        let change = match action {
            Action::Destroy => {
                return Err(Error::Unsupported(
                    "permanent delete is sent to the server first".into(),
                ));
            }
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
            && self.folder_rules
            && self.is_label(target)
        {
            return Err(Error::Unsupported(format!(
                "moving to a label on {} accounts",
                if self.gmail { "Gmail" } else { "Proton" }
            )));
        }

        if self.gmail {
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

        Ok(change)
    }

    /// What a patch leaves the email in: the folder rule, then Gmail's mirror
    /// rule. Both empty-result checks look at this, so Gmail (which files a
    /// message with no folder under Archive) is never skipped as empty.
    fn patch_mirror(&self, current: &[String], add: &[String], remove: &[String]) -> Vec<String> {
        let mut mirror = patched(current, add, remove);

        // The server keeps one folder: the last folder added.
        if self.folder_rules
            && let Some(keep) = add.iter().rev().find(|m| !self.is_label(m))
        {
            mirror.retain(|m| self.is_label(m) || m == keep);
        }

        if self.gmail {
            mirror = gmail_mirror(mirror, current);
        }

        mirror
    }

    /// `ids` without those a patch would leave without a mailbox (every other
    /// change keeps all of them). Ids unknown to the store stay in.
    fn sendable(
        &self,
        store: &Store,
        account: &str,
        ids: &[String],
        change: &Change,
    ) -> Result<Vec<String>> {
        let Change::Patch { add, remove } = change else {
            return Ok(ids.to_vec());
        };
        let mut keep = Vec::with_capacity(ids.len());

        for id in ids {
            let empties = store
                .email(account, id)?
                .is_some_and(|e| self.patch_mirror(&e.mailbox_ids, add, remove).is_empty());

            if !empties {
                keep.push(id.clone());
            }
        }

        Ok(keep)
    }

    /// The row `email` becomes under `change`; `None` for a patch that would
    /// leave it without a mailbox.
    fn mirror(&self, email: &EmailSummary, change: &Change) -> Option<Mirror> {
        match change {
            Change::Mailbox(target) => {
                let mut mirror = vec![target.clone()];

                if self.folder_rules {
                    mirror.extend(
                        email
                            .mailbox_ids
                            .iter()
                            .filter(|m| self.is_label(m))
                            .cloned(),
                    );
                }

                if self.gmail {
                    mirror = gmail_mirror(mirror, &email.mailbox_ids);
                }

                Some(Mirror::Mailboxes(mirror))
            }
            Change::Patch { add, remove } => {
                let mirror = self.patch_mirror(&email.mailbox_ids, add, remove);

                (!mirror.is_empty()).then_some(Mirror::Mailboxes(mirror))
            }
            Change::Keyword(k, on) => {
                let mut keywords = email.keywords.clone();

                keywords.retain(|x| x != k);

                if *on {
                    keywords.push((*k).to_string());
                }

                Some(Mirror::Keywords(keywords))
            }
        }
    }
}

/// Writes `mirror` to the row `id`; the store moves the badge counts.
fn write_mirror(store: &Store, account: &str, id: &str, mirror: &Mirror) -> Result<()> {
    match mirror {
        Mirror::Mailboxes(m) => store.set_mailbox_ids(account, id, m),
        Mirror::Keywords(k) => store.set_keywords(account, id, k),
    }
}

/// Applies `action` to `ids` of `account` on the server (when `client` is given)
/// and in the store. `client` must be the connection of `account`.
///
/// Ids the server refuses are neither mirrored nor counted.
/// Without a client, only emails the store knows are counted.
///
/// A [`Action::PatchMailboxes`] never leaves an email without a mailbox: ids
/// whose local copy would end up empty (after the folder rules below) are dropped before anything is sent and
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
/// other built-in folder is). So a Gmail patch that removes the last folder
/// is not empty: it leaves the email in `ARCHIVE`.
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
    if action == Action::Destroy {
        return destroy(store, client, account, ids).await;
    }

    let rules = Rules::load(store, account)?;
    let change = rules.change(store, account, action)?;
    let ids = rules.sendable(store, account, ids, &change)?;
    let mut refused: Vec<String> = Vec::new();

    if let Some(client) = client.filter(|_| !ids.is_empty()) {
        refused = client.update(&ids, &change).await?.refused;
    }

    let mut applied = 0;

    for id in ids.iter().filter(|id| !refused.contains(id)) {
        let Some(email) = store.email(account, id)? else {
            // Unknown locally: only a server that accepted it makes it applied.
            applied += usize::from(client.is_some());

            continue;
        };

        if let Some(mirror) = rules.mirror(&email, &change) {
            write_mirror(store, account, id, &mirror)?;
            applied += 1;
        }
    }

    Ok(applied)
}

/// An action already mirrored in the store and still owed to the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// Local account slug.
    pub account: String,
    /// The account's server id when the action was made
    /// (`account_state(account, "account_id")`, `None` when unset or empty).
    pub server_account: Option<String>,
    /// Ids to send: the requested ids de-duplicated (first occurrence wins,
    /// order kept), minus those a patch would leave without a mailbox.
    /// Ids unknown to the store stay in.
    pub ids: Vec<String>,
    /// What to send.
    pub change: Change,
    /// How many store rows the mirror wrote.
    pub mirrored: usize,
}

/// Applies `action` to `ids` of `account` in the store only, as [`apply`]
/// would after a server that accepted every id, and returns what is still
/// owed to the server. The ids are marked stale ([`Store::mark_stale`])
/// before any row changes, so a crash never leaves a mirrored row that
/// nothing will check again; [`reconcile`] clears the marks.
///
/// The checks of [`apply`] run first, even for an empty `ids`, and a failure
/// writes nothing. Ids are de-duplicated; ids the store does not know are
/// owed and marked, not mirrored. [`Action::Destroy`] is refused: it goes to
/// the server first (see [`apply`]).
///
/// # Errors
///
/// Returns [`Error::NotFound`] when a required role mailbox is missing,
/// [`Error::Unsupported`] for [`Action::Destroy`], for a move to a label on
/// Proton or Gmail and for a change to Sent or Drafts on Gmail, and any
/// database error.
pub fn apply_local(
    store: &Store,
    account: &str,
    ids: &[String],
    action: Action,
) -> Result<Pending> {
    let rules = Rules::load(store, account)?;
    let change = rules.change(store, account, action)?;
    let mut seen = HashSet::new();
    let unique: Vec<String> = ids
        .iter()
        .filter(|id| seen.insert(id.as_str()))
        .cloned()
        .collect();
    let ids = rules.sendable(store, account, &unique, &change)?;

    store.mark_stale(account, &ids)?;

    let mut mirrored = 0;

    for id in &ids {
        let Some(email) = store.email(account, id)? else {
            continue;
        };

        if let Some(mirror) = rules.mirror(&email, &change) {
            write_mirror(store, account, id, &mirror)?;
            mirrored += 1;
        }
    }

    let server_account = store
        .account_state(account, "account_id")?
        .filter(|s| !s.is_empty());

    Ok(Pending {
        account: account.to_string(),
        server_account,
        ids,
        change,
        mirrored,
    })
}

/// Sends a [`Pending`] to the server and returns the ids it refused. Never
/// writes to the store: [`reconcile`] settles what the server answered.
///
/// # Errors
///
/// Returns [`Error::Other`] when `client` is a different server account than
/// the one the action was made on (the account was replaced under the same
/// slug), and any network or server error.
pub async fn push(client: &Connection, pending: &Pending) -> Result<Vec<String>> {
    if pending.ids.is_empty() {
        return Ok(Vec::new());
    }

    if pending
        .server_account
        .as_deref()
        .is_some_and(|s| s != client.server_account_id())
    {
        return Err(Error::Other(
            "the account changed since this action was made".into(),
        ));
    }

    client
        .update(&pending.ids, &pending.change)
        .await
        .map(|u| u.refused)
}

/// Replaces the stale rows of `account` with what the server holds now,
/// refreshes the badge counts from the server and clears the marks; returns
/// how many marks it cleared.
///
/// `pending(id)` is true while the caller still owes a push for `id` (or
/// wants it left alone): such an id is neither fetched nor written nor
/// cleared. The core holds no queue, so `pending` is asked again after every
/// `.await`, and an action made while a request was in flight wins.
///
/// Providers that ignore `$answered` (Proton, Gmail) keep a local one until a
/// later sync. Without local counts (JMAP) the mailbox cursor is emptied
/// first, so a full `Mailbox/get` replaces the predicted counts. A failure
/// leaves every uncleared mark in place; every step is idempotent, so the
/// next call settles them.
///
/// # Errors
///
/// Any network, server or database error.
pub async fn reconcile(
    store: &Store,
    client: &Connection,
    account: &str,
    pending: impl Fn(&str) -> bool,
) -> Result<usize> {
    let mut ids = store.stale_ids(account)?;

    ids.retain(|id| !pending(id));

    if ids.is_empty() {
        return Ok(0);
    }

    let fetched = client.fetch_emails(account, &ids).await?;
    let mut keep = ids;

    keep.retain(|id| !pending(id));

    if keep.is_empty() {
        return Ok(0);
    }

    // No `.await` from here to the counts.
    let keeps_answered = matches!(
        store.account_state(account, "provider")?.as_deref(),
        Some("proton" | "gmail")
    );
    let mut rows: Vec<EmailSummary> = Vec::with_capacity(fetched.emails.len());

    for mut row in fetched.emails {
        if !keep.contains(&row.id) {
            continue;
        }

        if keeps_answered
            && !row.keywords.iter().any(|k| k == "$answered")
            && store
                .email(account, &row.id)?
                .is_some_and(|e| e.keywords.iter().any(|k| k == "$answered"))
        {
            row.keywords.push("$answered".to_string());
        }

        rows.push(row);
    }

    let gone: Vec<String> = fetched
        .gone
        .into_iter()
        .filter(|id| keep.contains(id))
        .collect();

    store.upsert_emails(&rows)?;
    store.delete_emails(account, &gone)?;

    if client.local_counts() {
        store.recount_mailboxes(account)?;
    } else {
        store.set_account_state(account, "mailbox_state", "")?;
        client.sync_mailboxes(store, account).await?;
    }

    let mut cleared = keep;

    cleared.retain(|id| !pending(id));
    store.clear_stale(account, &cleared)?;

    Ok(cleared.len())
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

        // Removing the only folder is a valid patch: the email lands in Archive.
        let store = gmail_store(&["INBOX"]);
        let patch = Action::PatchMailboxes {
            add: Vec::new(),
            remove: vec!["INBOX".into()],
        };
        let (result, after) = run(&store, patch).await;

        assert_eq!(result.unwrap(), 1);
        assert_eq!(after, ["ARCHIVE"]);

        // Destroy is not a mailbox change.
        let store = gmail_store(&["SENT"]);
        let (result, after) = run(&store, Action::Destroy).await;

        assert_eq!(result.unwrap(), 1);
        assert!(after.is_empty());
        assert!(store.email("g", "m").unwrap().is_none());
    }
}
