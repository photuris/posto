//! Mirrors the server's mailboxes and email summaries into the local store.

use std::path::Path;

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

/// What one sync run changed.
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
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
}

/// Syncs under `data_dir()/sync.lock`.
///
/// # Errors
///
/// See [`sync_with_lock`].
pub async fn sync(cfg: &Config, store: &Store, client: &Client) -> Result<SyncReport> {
    sync_with_lock(&data_dir().join("sync.lock"), cfg, store, client).await
}

/// Syncs while holding an exclusive, non-blocking lock on `lock_path`.
///
/// # Errors
///
/// Returns [`Error::Locked`] when another process holds the lock, and any
/// network, server or database error otherwise.
pub async fn sync_with_lock(
    lock_path: &Path,
    cfg: &Config,
    store: &Store,
    client: &Client,
) -> Result<SyncReport> {
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let mut lock = fd_lock::RwLock::new(std::fs::File::create(lock_path)?);
    let _guard = lock.try_write().map_err(|e| match e.kind() {
        std::io::ErrorKind::WouldBlock => Error::Locked("sync already running".into()),
        _ => Error::Io(e),
    })?;
    let account = &client.session().account_id;

    // A different account must not inherit the previous one's cursors.
    if get_state(store, "account_id")?.is_some_and(|a| a != *account) {
        store.clear_emails()?;
        store.set_state("mailbox_state", "")?;
        store.set_state("email_state", "")?;
    }

    store.set_state("account_id", account)?;
    sync_mailboxes(store, client).await?;

    let mut report = SyncReport::default();

    if get_state(store, "email_state")?.is_some() {
        match sync_emails_delta(store, client, &mut report).await {
            Err(Error::Jmap(m)) if m.starts_with("cannotCalculateChanges") => {
                tracing::debug!("email state expired; full resync");
                store.clear_emails()?;
                store.set_state("email_state", "")?;
                report = SyncReport::default();
                sync_emails_initial(cfg, store, client, &mut report).await?;
            }
            other => other?,
        }
    } else {
        sync_emails_initial(cfg, store, client, &mut report).await?;
    }

    report.mailboxes = store.mailboxes()?.len();

    Ok(report)
}

/// Reads a state value, treating the empty string as unset.
fn get_state(store: &Store, key: &str) -> Result<Option<String>> {
    Ok(store.state(key)?.filter(|s| !s.is_empty()))
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

fn mailbox_from(v: &Value) -> Result<Mailbox> {
    Ok(Mailbox {
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

fn email_from(v: &Value) -> Result<EmailSummary> {
    Ok(EmailSummary {
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
async fn get_mailboxes(client: &Client, ids: Option<&[String]>) -> Result<(Vec<Mailbox>, String)> {
    let r = client
        .call_one(
            "Mailbox/get",
            json!({ "accountId": client.session().account_id, "ids": ids }),
        )
        .await?;
    let list = r["list"].as_array().map(Vec::as_slice).unwrap_or_default();

    Ok((
        list.iter().map(mailbox_from).collect::<Result<_>>()?,
        str_of(&r, "state")?,
    ))
}

/// Replaces the mailbox table with the server's full list.
async fn replace_mailboxes(store: &Store, client: &Client) -> Result<()> {
    let (boxes, state) = get_mailboxes(client, None).await?;
    let stale: Vec<String> = store
        .mailboxes()?
        .into_iter()
        .map(|m| m.id)
        .filter(|id| !boxes.iter().any(|m| m.id == *id))
        .collect();

    store.delete_mailboxes(&stale)?;
    store.upsert_mailboxes(&boxes)?;
    store.set_state("mailbox_state", &state)
}

async fn sync_mailboxes(store: &Store, client: &Client) -> Result<()> {
    let Some(mut since) = get_state(store, "mailbox_state")? else {
        return replace_mailboxes(store, client).await;
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
                return replace_mailboxes(store, client).await;
            }
            other => other?,
        };
        let changed: Vec<String> = [&r["created"], &r["updated"]]
            .into_iter()
            .flat_map(strings)
            .collect();

        if !changed.is_empty() {
            store.upsert_mailboxes(&get_mailboxes(client, Some(&changed)).await?.0)?;
        }

        store.delete_mailboxes(&strings(&r["destroyed"]))?;

        since = str_of(&r, "newState")?;
        store.set_state("mailbox_state", &since)?;

        if r["hasMoreChanges"].as_bool() != Some(true) {
            return Ok(());
        }
    }
}

/// Fetches summaries for `ids` in chunks and upserts them.
///
/// Returns `(added, updated)` counts, by whether the store knew the id.
async fn fetch_and_store(store: &Store, client: &Client, ids: &[String]) -> Result<(usize, usize)> {
    let (mut added, mut updated) = (0, 0);

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
        let emails: Vec<EmailSummary> = list.iter().map(email_from).collect::<Result<_>>()?;

        for e in &emails {
            if store.email(&e.id)?.is_some() {
                updated += 1;
            } else {
                added += 1;
            }
        }

        store.upsert_emails(&emails)?;
    }

    Ok((added, updated))
}

async fn sync_emails_delta(store: &Store, client: &Client, report: &mut SyncReport) -> Result<()> {
    let Some(mut since) = get_state(store, "email_state")? else {
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
        let (added, updated) = fetch_and_store(store, client, &changed).await?;

        store.delete_emails(&destroyed)?;

        report.emails_added += added;
        report.emails_updated += updated;
        report.emails_removed += destroyed.len();

        since = str_of(&r, "newState")?;
        store.set_state("email_state", &since)?;

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
    let (added, updated) = fetch_and_store(store, client, &ids).await?;

    report.emails_added += added;
    report.emails_updated += updated;

    store.set_state("email_state", &s0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_whitespace_is_squashed() {
        let v = json!({"id": "e1", "preview": "COMING UP      \n\t soon   "});
        assert_eq!(email_from(&v).unwrap().preview, "COMING UP soon");
        let v = json!({"id": "e2", "preview": "\u{034f}   \u{200b}\u{feff}🚨 Joshua, polls\u{00ad} close"});
        assert_eq!(email_from(&v).unwrap().preview, "🚨 Joshua, polls close");
    }
}
