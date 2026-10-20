//! Mirrors the server's mailboxes and email summaries into the local store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use crate::config::{Config, data_dir};
use crate::error::{Error, Result};
use crate::provider::{Connection, Delta};
use crate::store::{EmailSummary, Store};

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
    /// Masked email addresses cached afterwards.
    pub masked_emails: usize,
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
    client: &Connection,
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
    clients: &HashMap<String, Arc<Connection>>,
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
    clients: &HashMap<String, Arc<Connection>>,
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
    conn: &Connection,
) -> Result<SyncReport> {
    let server_id = conn.server_account_id();

    // A different server account must not inherit the previous one's cache.
    if get_state(store, account, "account_id")?.is_some_and(|a| a != server_id) {
        store.delete_account(account)?;
    }

    store.set_account_state(account, "account_id", server_id)?;
    store.set_account_state(account, "provider", conn.provider_name())?;
    conn.sync_mailboxes(store, account).await?;

    let mut report = SyncReport::default();

    if get_state(store, account, "email_state")?.is_some() {
        sync_emails_delta(cfg, store, account, conn, &mut report).await?;
    } else {
        sync_emails_initial(cfg, store, account, conn, &mut report).await?;
    }

    if conn.local_counts() {
        store.recount_mailboxes(account)?;
    }

    report.account_id = account.to_string();
    report.mailboxes = store.mailboxes(account)?.len();

    match conn.identities().await {
        Ok(identities) => {
            store.set_identities(account, &identities)?;
            report.identities = identities.len();
        }
        Err(e) => tracing::warn!("identity sync failed: {e}"),
    }

    match conn.masked_emails().await {
        Ok(masked) => {
            store.set_masked_emails(account, &masked)?;
            report.masked_emails = masked.len();
        }
        Err(e) => tracing::warn!("masked email sync failed: {e}"),
    }

    Ok(report)
}

/// Reads a state value, treating the empty string as unset.
pub(crate) fn get_state(store: &Store, account: &str, key: &str) -> Result<Option<String>> {
    Ok(store.account_state(account, key)?.filter(|s| !s.is_empty()))
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

/// Applies delta pages after the stored cursor until the server reports no
/// more; a cursor the server cannot continue from restarts as an initial sync.
async fn sync_emails_delta(
    cfg: &Config,
    store: &Store,
    account: &str,
    conn: &Connection,
    report: &mut SyncReport,
) -> Result<()> {
    let Some(mut since) = get_state(store, account, "email_state")? else {
        return Ok(());
    };

    loop {
        let page = match conn.delta(account, &since).await? {
            Delta::Page(page) => page,
            Delta::Resync => {
                tracing::debug!("email state expired; full resync");
                store.clear_emails(account)?;
                store.set_account_state(account, "email_state", "")?;
                *report = SyncReport::default();

                return sync_emails_initial(cfg, store, account, conn, report).await;
            }
        };
        let (added, updated) = count_known(store, &page.emails)?;
        let inbox_added = newly_in_inbox(store, account, &page.emails)?;

        // One transaction per page: a crash never leaves a cursor ahead of its rows.
        store.apply_delta_page(
            account,
            &page.emails,
            &page.destroyed,
            &inbox_added,
            &page.state,
        )?;

        report.emails_added += added;
        report.emails_updated += updated;
        report.emails_removed += page.destroyed.len();
        report.inbox_added.extend(inbox_added);

        if page.mailboxes_changed {
            conn.sync_mailboxes(store, account).await?;
        }

        if !page.more {
            return Ok(());
        }

        since = page.state;
    }
}

async fn sync_emails_initial(
    cfg: &Config,
    store: &Store,
    account: &str,
    conn: &Connection,
    report: &mut SyncReport,
) -> Result<()> {
    report.initial = true;

    let (emails, s0) = conn
        .initial(account, cfg.sync.initial_limit as usize)
        .await?;
    let (added, updated) = count_known(store, &emails)?;

    store.upsert_emails(&emails)?;

    report.emails_added += added;
    report.emails_updated += updated;

    store.set_account_state(account, "email_state", &s0)
}
