//! Mirrors the server's mailboxes and email summaries into the local store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{Config, data_dir};
use crate::error::{Error, Result};
use crate::jmap::Client;
use crate::store::{Address, EmailSummary, Mailbox, Store};

/// Email properties kept in the store.
const EMAIL_PROPS: [&str; 12] = [
    "id",
    "threadId",
    "mailboxIds",
    "keywords",
    "receivedAt",
    "from",
    "to",
    "cc",
    "subject",
    "preview",
    "size",
    "hasAttachment",
];

/// Largest page for `Email/query` and chunk for `Email/get`.
const PAGE: usize = 100;

/// Times a paged query restarts after the server's query state moved.
const MAX_RESTARTS: usize = 3;

/// What one sync run of one account changed.
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    /// Local slug of the account this report is about.
    pub account_id: String,
    /// Mailboxes in the store afterwards.
    pub mailboxes: usize,
    /// Emails that were new to the store.
    pub emails_added: usize,
    /// Emails that were already in the store and were refreshed.
    pub emails_updated: usize,
    /// Emails the server destroyed.
    pub emails_removed: usize,
    /// True when this was a full (first or recovery) sync.
    pub initial: bool,
    /// Sending identities cached afterwards.
    pub identities: usize,
    /// Ids newly added to the inbox-role mailbox by this run's delta; empty
    /// on an initial sync. They are also queued for classification.
    pub inbox_added: Vec<String>,
    /// Why this account failed; everything else in the report is then partial.
    pub error: Option<String>,
}

/// Path of the lock shared by every sync in the data directory.
pub fn lock_path() -> PathBuf {
    data_dir().join("sync.lock")
}

/// Takes an exclusive, non-blocking lock on `lock_path`; see [`try_lock`].
fn open_lock(lock_path: &Path) -> Result<fd_lock::RwLock<std::fs::File>> {
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    Ok(fd_lock::RwLock::new(std::fs::File::create(lock_path)?))
}

/// Locks `lock`; held elsewhere is [`Error::Locked`].
fn try_lock(
    lock: &mut fd_lock::RwLock<std::fs::File>,
) -> Result<fd_lock::RwLockWriteGuard<'_, std::fs::File>> {
    lock.try_write().map_err(|e| match e.kind() {
        std::io::ErrorKind::WouldBlock => Error::Locked("sync already running".into()),
        _ => Error::Io(e),
    })
}

/// Syncs one account while holding the lock on `lock_path`.
///
/// `client` must be the connection of `account`.
///
/// # Errors
///
/// Returns [`Error::Locked`] when another process holds the lock,
/// [`Error::NotFound`] for an account that is not configured, and any
/// network, server or database error otherwise.
pub async fn sync_account(
    lock_path: &Path,
    cfg: &Config,
    account: &str,
    store: &Store,
    client: &Client,
) -> Result<SyncReport> {
    if cfg.account(account).is_none() {
        return Err(Error::NotFound(format!("account {account}")));
    }

    let mut lock = open_lock(lock_path)?;
    let _guard = try_lock(&mut lock)?;

    sync_one(cfg, account, store, client).await
}

/// Syncs every enabled account, under `data_dir()/sync.lock`.
///
/// # Errors
///
/// See [`sync_all_with_lock`].
pub async fn sync_all(
    cfg: &Config,
    store: &Store,
    clients: &HashMap<String, Arc<Client>>,
) -> Result<Vec<SyncReport>> {
    sync_all_with_lock(&lock_path(), cfg, store, clients).await
}

/// Syncs every enabled account in config order under one lock.
///
/// An account without a client is reported as `error = "no token"` and
/// nothing else is done for it. One account failing never fails the pass:
/// its error goes into its report and the next account runs.
///
/// # Errors
///
/// Returns [`Error::Locked`] when another process holds the lock, and
/// [`Error::Io`] when the lock file cannot be opened.
pub async fn sync_all_with_lock(
    lock_path: &Path,
    cfg: &Config,
    store: &Store,
    clients: &HashMap<String, Arc<Client>>,
) -> Result<Vec<SyncReport>> {
    let mut lock = open_lock(lock_path)?;
    let _guard = try_lock(&mut lock)?;
    let mut reports = Vec::new();

    for account in cfg.accounts_enabled() {
        let failed = |error: String| SyncReport {
            account_id: account.id.clone(),
            error: Some(error),
            ..SyncReport::default()
        };
        let Some(client) = clients.get(&account.id) else {
            reports.push(failed("no token".into()));

            continue;
        };

        reports.push(match sync_one(cfg, &account.id, store, client).await {
            Ok(report) => report,
            Err(e) => failed(e.to_string()),
        });
    }

    Ok(reports)
}

/// One account's sync, under a lock the caller holds.
async fn sync_one(
    cfg: &Config,
    account: &str,
    store: &Store,
    client: &Client,
) -> Result<SyncReport> {
    let server_id = &client.session().account_id;

    // A different server account must not inherit the previous one's cache.
    if get_state(store, account, "account_id")?.is_some_and(|a| a != *server_id) {
        store.delete_account(account)?;
    }

    store.set_account_state(account, "account_id", server_id)?;
    sync_mailboxes(store, account, client).await?;

    let mut report = SyncReport::default();

    if get_state(store, account, "email_state")?.is_some() {
        match sync_emails_delta(store, account, client, &mut report).await {
            Err(Error::Jmap(m)) if m.starts_with("cannotCalculateChanges") => {
                tracing::debug!("email state expired; full resync");
                store.clear_emails(account)?;
                store.set_account_state(account, "email_state", "")?;
                report = SyncReport::default();
                sync_emails_initial(cfg, store, account, client, &mut report).await?;
            }
            other => other?,
        }
    } else {
        sync_emails_initial(cfg, store, account, client, &mut report).await?;
    }

    report.account_id = account.to_string();
    report.mailboxes = store.mailboxes(account)?.len();

    match client.identities().await {
        Ok(identities) => {
            store.set_identities(account, &identities)?;
            report.identities = identities.len();
        }
        Err(e) => tracing::warn!("identity sync failed: {e}"),
    }

    Ok(report)
}

/// Reads a state value, treating the empty string as unset.
fn get_state(store: &Store, account: &str, key: &str) -> Result<Option<String>> {
    Ok(store.account_state(account, key)?.filter(|s| !s.is_empty()))
}

/// Ids from a JSON string array; anything else yields an empty list.
fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Keys of a JSON object whose value is `true`.
fn true_keys(v: &Value) -> Vec<String> {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, v)| v.as_bool() == Some(true))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Reads a required string property.
fn str_of(v: &Value, key: &str) -> Result<String> {
    v[key]
        .as_str()
        .map(String::from)
        .ok_or_else(|| Error::Jmap(format!("response missing {key}")))
}

fn addresses(v: &Value) -> Vec<Address> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(Address {
                        name: x["name"].as_str().map(String::from),
                        email: x["email"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn mailbox_from(account: &str, v: &Value) -> Result<Mailbox> {
    Ok(Mailbox {
        account_id: account.to_string(),
        id: str_of(v, "id")?,
        name: v["name"].as_str().unwrap_or_default().to_string(),
        role: v["role"].as_str().map(String::from),
        parent_id: v["parentId"].as_str().map(String::from),
        sort_order: v["sortOrder"].as_i64().unwrap_or(0),
        total: v["totalEmails"].as_i64().unwrap_or(0),
        unread: v["unreadEmails"].as_i64().unwrap_or(0),
    })
}

/// Collapses runs of whitespace (including newlines) into single spaces, trims the ends,
/// and drops invisible format characters (zero-width spaces/joiners, soft hyphen, BOM,
/// combining grapheme joiner) that newsletters use to pad preheader text.
fn squash_ws(s: &str) -> String {
    let invisible = |c: char| {
        matches!(
            c,
            '\u{00AD}' | '\u{034F}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}'
        )
    };
    s.replace(invisible, "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn email_from(account: &str, v: &Value) -> Result<EmailSummary> {
    Ok(EmailSummary {
        account_id: account.to_string(),
        id: str_of(v, "id")?,
        thread_id: v["threadId"].as_str().unwrap_or_default().to_string(),
        received_at: v["receivedAt"].as_str().unwrap_or_default().to_string(),
        from: addresses(&v["from"]),
        to: addresses(&v["to"]),
        cc: addresses(&v["cc"]),
        subject: v["subject"].as_str().unwrap_or_default().to_string(),
        preview: squash_ws(v["preview"].as_str().unwrap_or_default()),
        size: v["size"].as_i64().unwrap_or(0),
        has_attachment: v["hasAttachment"].as_bool().unwrap_or(false),
        mailbox_ids: true_keys(&v["mailboxIds"]),
        keywords: true_keys(&v["keywords"]),
    })
}

/// Fetches mailboxes by id; `None` fetches all of them.
async fn get_mailboxes(
    client: &Client,
    account: &str,
    ids: Option<&[String]>,
) -> Result<(Vec<Mailbox>, String)> {
    let r = client
        .call_one(
            "Mailbox/get",
            json!({ "accountId": client.session().account_id, "ids": ids }),
        )
        .await?;
    let list = r["list"].as_array().map(Vec::as_slice).unwrap_or_default();

    Ok((
        list.iter()
            .map(|v| mailbox_from(account, v))
            .collect::<Result<_>>()?,
        str_of(&r, "state")?,
    ))
}

/// Replaces the mailbox table of `account` with the server's full list.
async fn replace_mailboxes(store: &Store, account: &str, client: &Client) -> Result<()> {
    let (boxes, state) = get_mailboxes(client, account, None).await?;
    let stale: Vec<String> = store
        .mailboxes(account)?
        .into_iter()
        .map(|m| m.id)
        .filter(|id| !boxes.iter().any(|m| m.id == *id))
        .collect();

    store.delete_mailboxes(account, &stale)?;
    store.upsert_mailboxes(&boxes)?;
    store.set_account_state(account, "mailbox_state", &state)
}

async fn sync_mailboxes(store: &Store, account: &str, client: &Client) -> Result<()> {
    let Some(mut since) = get_state(store, account, "mailbox_state")? else {
        return replace_mailboxes(store, account, client).await;
    };

    loop {
        let r = match client
            .call_one(
                "Mailbox/changes",
                json!({ "accountId": client.session().account_id, "sinceState": since }),
            )
            .await
        {
            Err(Error::Jmap(m)) if m.starts_with("cannotCalculateChanges") => {
                return replace_mailboxes(store, account, client).await;
            }
            other => other?,
        };
        let changed: Vec<String> = [&r["created"], &r["updated"]]
            .into_iter()
            .flat_map(strings)
            .collect();

        if !changed.is_empty() {
            store.upsert_mailboxes(&get_mailboxes(client, account, Some(&changed)).await?.0)?;
        }

        store.delete_mailboxes(account, &strings(&r["destroyed"]))?;

        since = str_of(&r, "newState")?;
        store.set_account_state(account, "mailbox_state", &since)?;

        if r["hasMoreChanges"].as_bool() != Some(true) {
            return Ok(());
        }
    }
}

/// Fetches summaries for `ids` in chunks, in request order.
async fn fetch_summaries(
    client: &Client,
    account: &str,
    ids: &[String],
) -> Result<Vec<EmailSummary>> {
    let mut emails = Vec::with_capacity(ids.len());

    for chunk in ids.chunks(PAGE) {
        let r = client
            .call_one(
                "Email/get",
                json!({
                    "accountId": client.session().account_id,
                    "ids": chunk,
                    "properties": EMAIL_PROPS,
                }),
            )
            .await?;
        let list = r["list"].as_array().map(Vec::as_slice).unwrap_or_default();

        for v in list {
            emails.push(email_from(account, v)?);
        }
    }

    Ok(emails)
}

/// Counts how many of `emails` the store already knows: `(added, updated)`.
fn count_known(store: &Store, emails: &[EmailSummary]) -> Result<(usize, usize)> {
    let (mut added, mut updated) = (0, 0);

    for e in emails {
        if store.email(&e.account_id, &e.id)?.is_some() {
            updated += 1;
        } else {
            added += 1;
        }
    }

    Ok((added, updated))
}

/// Ids among `emails` that are in the inbox now and were not before.
fn newly_in_inbox(store: &Store, account: &str, emails: &[EmailSummary]) -> Result<Vec<String>> {
    let Some(inbox) = store.mailbox_by_role(account, "inbox")? else {
        return Ok(Vec::new());
    };
    let mut added = Vec::new();

    for e in emails.iter().filter(|e| e.mailbox_ids.contains(&inbox.id)) {
        let was = store
            .email(account, &e.id)?
            .is_some_and(|old| old.mailbox_ids.contains(&inbox.id));

        if !was {
            added.push(e.id.clone());
        }
    }

    Ok(added)
}

async fn sync_emails_delta(
    store: &Store,
    account: &str,
    client: &Client,
    report: &mut SyncReport,
) -> Result<()> {
    let Some(mut since) = get_state(store, account, "email_state")? else {
        return Ok(());
    };

    loop {
        let r = client
            .call_one(
                "Email/changes",
                json!({
                    "accountId": client.session().account_id,
                    "sinceState": since,
                    "maxChanges": 500,
                }),
            )
            .await?;
        let changed: Vec<String> = [&r["created"], &r["updated"]]
            .into_iter()
            .flat_map(strings)
            .collect();
        let destroyed = strings(&r["destroyed"]);
        let emails = fetch_summaries(client, account, &changed).await?;
        let (added, updated) = count_known(store, &emails)?;
        let inbox_added = newly_in_inbox(store, account, &emails)?;

        since = str_of(&r, "newState")?;

        // One transaction per page: a crash never leaves a cursor ahead of its rows.
        store.apply_delta_page(account, &emails, &destroyed, &inbox_added, &since)?;

        report.emails_added += added;
        report.emails_updated += updated;
        report.emails_removed += destroyed.len();
        report.inbox_added.extend(inbox_added);

        if r["hasMoreChanges"].as_bool() != Some(true) {
            return Ok(());
        }
    }
}

/// Collects up to `limit` newest email ids, restarting if the query changes.
async fn query_ids(client: &Client, limit: usize) -> Result<Vec<String>> {
    let mut restarts = 0;

    'restart: loop {
        let mut ids: Vec<String> = Vec::new();
        let mut first_state: Option<String> = None;

        while ids.len() < limit {
            let r = client
                .call_one(
                    "Email/query",
                    json!({
                        "accountId": client.session().account_id,
                        "sort": [{ "property": "receivedAt", "isAscending": false }],
                        "calculateTotal": false,
                        "position": ids.len(),
                        "limit": PAGE.min(limit - ids.len()),
                    }),
                )
                .await?;
            let query_state = str_of(&r, "queryState")?;

            match &first_state {
                None => first_state = Some(query_state),
                Some(s) if *s != query_state && restarts < MAX_RESTARTS => {
                    restarts += 1;
                    continue 'restart;
                }
                // ponytail: after the restart budget, keep paging this pass even
                // though the snapshot moved; a later delta repairs any drift
                Some(_) => {}
            }

            let page = strings(&r["ids"]);

            // A server may clamp the page size, so only an empty page ends paging.
            if page.is_empty() {
                break;
            }

            ids.extend(page);
        }

        ids.truncate(limit);

        return Ok(ids);
    }
}

async fn sync_emails_initial(
    cfg: &Config,
    store: &Store,
    account: &str,
    client: &Client,
    report: &mut SyncReport,
) -> Result<()> {
    report.initial = true;

    // Capture the state before paging so changes made during paging show up in the next delta.
    let s0 = client
        .call_one(
            "Email/get",
            json!({ "accountId": client.session().account_id, "ids": [] }),
        )
        .await?;
    let s0 = str_of(&s0, "state")?;
    let ids = query_ids(client, cfg.sync.initial_limit as usize).await?;
    let emails = fetch_summaries(client, account, &ids).await?;
    let (added, updated) = count_known(store, &emails)?;

    store.upsert_emails(&emails)?;

    report.emails_added += added;
    report.emails_updated += updated;

    store.set_account_state(account, "email_state", &s0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_whitespace_is_squashed() {
        let v = json!({"id": "e1", "preview": "COMING UP      \n\t soon   "});
        assert_eq!(email_from("a", &v).unwrap().preview, "COMING UP soon");
        let v = json!({"id": "e2", "preview": "\u{034f}   \u{200b}\u{feff}🚨 Joshua, polls\u{00ad} close"});
        assert_eq!(
            email_from("a", &v).unwrap().preview,
            "🚨 Joshua, polls close"
        );
    }
}
