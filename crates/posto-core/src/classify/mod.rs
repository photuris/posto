//! Classifies newly arrived inbox mail with Jev (TypeSafe System One), files
//! it into tray mailboxes and flags urgent replies. A port of jevmail's
//! questions, decision rule, state block and mailbox patch rules.
//!
//! New inbox arrivals are queued by the sync (`classify_queue`). [`run`]
//! drains the queue under `classify.lock`: it first applies any answer whose
//! moves are still unconfirmed (without asking Jev again), then asks Jev
//! about the oldest queued emails of accounts where [`Config::classify_enabled_for`]
//! holds, up to the run's limit.
//!
//! A paid answer is written to `classified` (`applied = 0`) before any
//! mailbox is touched, so a failed move is retried without a second request.
//! Billing is not exactly-once: if the process dies between Jev answering
//! and that row being written, the answer is lost and the next run asks
//! again.

pub mod jev;
pub mod state;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

pub use jev::{Classification, Classifier, JEV_BASE_URL, JevClient};

use crate::actions::{Action, apply};
use crate::body::fetch_body;
use crate::config::{Config, data_dir};
use crate::error::{Error, Result};
use crate::jmap::Client;
use crate::store::{CLASSIFY_MAX_ATTEMPTS, Classified, MsgRef, QueueRow, Store};

/// A mailbox category Jev sorts mail into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tray {
    /// A real person expects a reply.
    NeedsReply,
    /// Transactional and informational mail.
    Updates,
    /// Marketing.
    Promotional,
    /// Social network and community notifications.
    Social,
    /// Scams and junk; goes to the junk mailbox.
    Spam,
}

impl Tray {
    /// Every tray, in jevmail's order.
    pub const ALL: [Tray; 5] = [
        Tray::NeedsReply,
        Tray::Updates,
        Tray::Promotional,
        Tray::Social,
        Tray::Spam,
    ];

    /// The key jevmail and Jev use for this tray.
    pub fn as_str(self) -> &'static str {
        match self {
            Tray::NeedsReply => "needs_reply",
            Tray::Updates => "updates",
            Tray::Promotional => "promotional",
            Tray::Social => "social",
            Tray::Spam => "spam",
        }
    }
}

impl fmt::Display for Tray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Tray {
    type Err = Error;

    fn from_str(s: &str) -> Result<Tray> {
        Tray::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| {
                Error::Other(format!(
                    "unknown tray {s:?}; use needs_reply, updates, promotional, social or spam"
                ))
            })
    }
}

/// An email to classify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Local account slug.
    pub account_id: String,
    /// Server-assigned email id.
    pub email_id: String,
}

/// What one [`run`] did.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct ClassifyReport {
    /// Answers obtained from Jev.
    pub classified: usize,
    /// Emails whose mailbox changes the server accepted, including retried ones.
    pub moved: usize,
    /// Emails flagged.
    pub flagged: usize,
    /// New answers that were unsure.
    pub unsure: usize,
    /// Queue rows left alone: disabled accounts, no connection, or already classified.
    pub skipped: usize,
    /// One `<account>/<id>: <reason>` line per failure.
    pub errors: Vec<String>,
    /// Input tokens Jev billed.
    pub input_tokens: u64,
}

/// Path of the lock that keeps two classifier runs apart.
pub fn lock_path() -> PathBuf {
    data_dir().join("classify.lock")
}

/// Drains the classification queue under `data_dir()/classify.lock`; see
/// [`run_with_lock`].
pub async fn run<C: Classifier>(
    cfg: &Config,
    store: &Store,
    clients: &HashMap<String, Arc<Client>>,
    classifier: &C,
    limit: Option<usize>,
) -> ClassifyReport {
    run_with_lock(&lock_path(), cfg, store, clients, classifier, limit).await
}

/// Applies unconfirmed answers, then asks `classifier` about up to `limit`
/// (default `classify.max_per_run`) queued emails, holding the lock on
/// `lock_path` throughout.
///
/// Never fails: every problem is a line in [`ClassifyReport::errors`]. A held
/// lock yields the single line `classifier already running` and nothing else.
pub async fn run_with_lock<C: Classifier>(
    lock_path: &Path,
    cfg: &Config,
    store: &Store,
    clients: &HashMap<String, Arc<Client>>,
    classifier: &C,
    limit: Option<usize>,
) -> ClassifyReport {
    let mut report = ClassifyReport::default();
    let file = lock_path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::File::create(lock_path));
    let mut lock = match file {
        Ok(file) => fd_lock::RwLock::new(file),
        Err(e) => {
            report.errors.push(format!("classifier lock: {e}"));

            return report;
        }
    };
    let _guard = match lock.try_write() {
        Ok(guard) => guard,
        Err(e) => {
            report.errors.push(match e.kind() {
                std::io::ErrorKind::WouldBlock => "classifier already running".to_string(),
                _ => format!("classifier lock: {e}"),
            });

            return report;
        }
    };

    if let Err(e) = drain(cfg, store, clients, classifier, limit, &mut report).await {
        report.errors.push(format!("classifier: {e}"));
    }

    report
}

/// The body of a run, under the lock.
async fn drain<C: Classifier>(
    cfg: &Config,
    store: &Store,
    clients: &HashMap<String, Arc<Client>>,
    classifier: &C,
    limit: Option<usize>,
    report: &mut ClassifyReport,
) -> Result<()> {
    apply_pending(cfg, store, clients, report).await?;

    // An email that came back to the inbox keeps its first classification.
    report.skipped += store.prune_classified_queue()?;

    let eligible: Vec<String> = cfg
        .accounts_enabled()
        .filter(|a| cfg.classify_enabled_for(a) && clients.contains_key(&a.id))
        .map(|a| a.id.clone())
        .collect();

    report.skipped += store
        .classify_queue(None)?
        .iter()
        .filter(|r| r.attempts < CLASSIFY_MAX_ATTEMPTS && !eligible.contains(&r.account_id))
        .count();

    let cap = limit.unwrap_or(cfg.classify.max_per_run as usize);

    for row in store.next_classify_batch(&eligible, cap)? {
        let Some(client) = clients.get(&row.account_id) else {
            continue;
        };
        let (account, id) = (row.account_id.as_str(), row.email_id.as_str());
        let result = async {
            store.begin_classify_attempt(account, id)?;

            let answer = ask(cfg, store, client, classifier, account, id).await?;

            store.put_classification(&answer)?;
            report.classified += 1;
            report.input_tokens += answer.input_tokens;
            report.unsure += usize::from(answer.unsure);

            settle(cfg, store, Some(client), &answer, report).await
        }
        .await;

        if let Err(e) = result {
            fail(store, report, account, id, &e);
        }
    }

    Ok(())
}

/// Applies every answer whose moves are unconfirmed, without asking Jev.
/// Rows tied to a queue entry share its attempt budget; correction rows have
/// none and are tried on every run.
async fn apply_pending(
    cfg: &Config,
    store: &Store,
    clients: &HashMap<String, Arc<Client>>,
    report: &mut ClassifyReport,
) -> Result<()> {
    let queue: HashMap<(String, String), QueueRow> = store
        .classify_queue(None)?
        .into_iter()
        .map(|r| ((r.account_id.clone(), r.email_id.clone()), r))
        .collect();

    for row in store.pending_classifications()? {
        let (account, id) = (row.account_id.as_str(), row.email_id.as_str());
        let Some(client) = clients.get(account) else {
            continue;
        };

        if !cfg.account(account).is_some_and(|a| a.enabled) {
            continue;
        }

        if let Some(queued) = queue.get(&(row.account_id.clone(), row.email_id.clone())) {
            if queued.attempts >= CLASSIFY_MAX_ATTEMPTS {
                continue;
            }

            store.begin_classify_attempt(account, id)?;
        }

        if let Err(e) = settle(cfg, store, Some(client), &row, report).await {
            fail(store, report, account, id, &e);
        }
    }

    Ok(())
}

/// Records a failure on the report and on the email's queue row.
fn fail(store: &Store, report: &mut ClassifyReport, account: &str, id: &str, e: &Error) {
    report.errors.push(format!("{account}/{id}: {e}"));

    if let Err(db) = store.set_queue_error(account, id, &e.to_string()) {
        tracing::warn!("recording classifier error failed: {db}");
    }
}

/// Applies a stored answer and records the outcome: confirmed and out of the
/// queue, or pending with the reason.
async fn settle(
    cfg: &Config,
    store: &Store,
    client: Option<&Client>,
    c: &Classified,
    report: &mut ClassifyReport,
) -> Result<()> {
    let (account, id) = (c.account_id.as_str(), c.email_id.as_str());

    match apply_row(cfg, store, client, c).await {
        Ok(flagged) => {
            store.set_classification_applied(account, id, None)?;
            store.remove_from_classify_queue(account, id)?;
            report.moved += 1;
            report.flagged += usize::from(flagged);

            Ok(())
        }
        Err(e) => {
            store.set_classification_applied(account, id, Some(&e.to_string()))?;

            Err(e)
        }
    }
}

/// Builds the state block of one email, asks `classifier` and returns the
/// unapplied answer.
async fn ask<C: Classifier>(
    cfg: &Config,
    store: &Store,
    client: &Client,
    classifier: &C,
    account: &str,
    id: &str,
) -> Result<Classified> {
    let email = store
        .email(account, id)?
        .ok_or_else(|| Error::NotFound(format!("email {account}/{id}")))?;
    let body = fetch_body(store, Some(client), account, id).await?;
    let has_unsubscribe = has_unsubscribe(client, id).await;
    let replied = store.thread_has_reply_from(
        account,
        &email.thread_id,
        id,
        &own_addresses(cfg, store, account)?,
    )?;
    let state = state::state_for(
        &email,
        &body,
        has_unsubscribe,
        replied,
        cfg.classify.body_chars as usize,
    );
    let c = classifier.classify(&state).await?;

    Ok(Classified {
        account_id: account.to_string(),
        email_id: id.to_string(),
        tray: c.tray.as_str().to_string(),
        probs: c.probs,
        urgency: c.urgency,
        personal: c.personal,
        unsure: c.unsure,
        model: c.model,
        input_tokens: c.input_tokens,
        classified_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        corrected_tray: None,
        applied: false,
        apply_error: None,
    })
}

/// The addresses the user writes from in `account`: its identities and its
/// own address.
fn own_addresses(cfg: &Config, store: &Store, account: &str) -> Result<Vec<String>> {
    let mut addresses: Vec<String> = store
        .identities(account)?
        .into_iter()
        .map(|i| i.email)
        .collect();

    addresses.extend(
        cfg.account(account)
            .map(|a| a.email.clone())
            .filter(|e| !e.is_empty()),
    );

    Ok(addresses)
}

/// Whether the email has a non-empty `List-Unsubscribe` header; a failed
/// fetch counts as no.
async fn has_unsubscribe(client: &Client, id: &str) -> bool {
    const HEADER: &str = "header:List-Unsubscribe:asText";

    client
        .call_one(
            "Email/get",
            json!({
                "accountId": client.session().account_id,
                "ids": [id],
                "properties": [HEADER],
            }),
        )
        .await
        .ok()
        .and_then(|r| r["list"][0][HEADER].as_str().map(|h| !h.trim().is_empty()))
        .unwrap_or(false)
}

/// The id of the top-level mailbox `name` in `account`, created on the
/// server (and stored) when missing.
async fn ensure_mailbox(
    store: &Store,
    client: Option<&Client>,
    account: &str,
    name: &str,
) -> Result<String> {
    if name.trim().is_empty() {
        return Err(Error::Config("a classifier mailbox name is empty".into()));
    }

    if let Some(m) = store.top_level_mailbox_by_name(account, name)? {
        return Ok(m.id);
    }

    let client = client.ok_or_else(|| Error::NotFound(format!("mailbox {name}")))?;
    let mut mailbox = client.create_mailbox(name).await?;

    mailbox.account_id = account.to_string();
    store.upsert_mailboxes(std::slice::from_ref(&mailbox))?;

    Ok(mailbox.id)
}

/// Runs one action on one email; an email nobody accepted is an error.
async fn accepted(
    store: &Store,
    client: Option<&Client>,
    account: &str,
    id: &str,
    action: Action,
) -> Result<()> {
    match apply(store, client, account, &[id.to_string()], action).await? {
        0 => Err(Error::Other(
            "the change was refused or the email is gone".into(),
        )),
        _ => Ok(()),
    }
}

/// Removes repeats, keeping the first of each.
fn unique(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();

    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

/// Files the email per its effective tray and flags it when it is an urgent
/// uncorrected needs-reply; returns whether it was flagged. Mailbox changes
/// are patches, so memberships the classifier does not own are untouched:
///
/// - add the tray's mailbox, and the unsure mailbox when unsure;
/// - leave the inbox only when sure and the tray is not in `keep_in_inbox`;
/// - spam: sure goes to the junk mailbox and leaves the inbox; unsure stays
///   in the inbox and goes to the unsure mailbox only.
///
/// A corrected row also leaves every other tray mailbox and the unsure
/// mailbox, leaves junk unless it is spam, and returns to the inbox when the
/// tray keeps mail there.
async fn apply_row(
    cfg: &Config,
    store: &Store,
    client: Option<&Client>,
    c: &Classified,
) -> Result<bool> {
    let tray: Tray = c.effective_tray().parse()?;
    let (account, id) = (c.account_id.as_str(), c.email_id.as_str());
    let settings = &cfg.classify;
    let tidy = c.corrected_tray.is_some();
    let keeps = settings.keep_in_inbox.iter().any(|k| k == tray.as_str());
    let inbox = || {
        store
            .mailbox_by_role(account, "inbox")?
            .map(|m| m.id)
            .ok_or_else(|| Error::NotFound("inbox mailbox".into()))
    };
    let (mut add, mut remove) = (Vec::new(), Vec::new());

    if tray == Tray::Spam {
        if !c.unsure {
            add.push(
                store
                    .mailbox_by_role(account, "junk")?
                    .map(|m| m.id)
                    .ok_or_else(|| Error::NotFound("junk mailbox".into()))?,
            );
            remove.push(inbox()?);
        }
    } else {
        let name = settings
            .trays
            .get(tray.as_str())
            .ok_or_else(|| Error::Config(format!("no mailbox configured for tray {tray}")))?;

        add.push(ensure_mailbox(store, client, account, name).await?);

        if !c.unsure && !keeps {
            remove.push(inbox()?);
        }
    }

    if c.unsure {
        add.push(ensure_mailbox(store, client, account, &settings.unsure_mailbox).await?);
    }

    if tidy {
        for other in Tray::ALL.into_iter().filter(|t| *t != tray) {
            let name = settings.trays.get(other.as_str());

            if let Some(m) =
                name.and_then(|n| store.top_level_mailbox_by_name(account, n).transpose())
            {
                remove.push(m?.id);
            }
        }

        if !c.unsure
            && let Some(m) = store.top_level_mailbox_by_name(account, &settings.unsure_mailbox)?
        {
            remove.push(m.id);
        }

        if tray != Tray::Spam {
            if let Some(junk) = store.mailbox_by_role(account, "junk")? {
                remove.push(junk.id);
            }

            if keeps {
                add.push(inbox()?);
            }
        }
    }

    let add = unique(add);
    let remove: Vec<String> = unique(remove)
        .into_iter()
        .filter(|r| !add.contains(r))
        .collect();

    if !add.is_empty() || !remove.is_empty() {
        accepted(
            store,
            client,
            account,
            id,
            Action::PatchMailboxes { add, remove },
        )
        .await?;
    }

    let flag = !tidy && tray == Tray::NeedsReply && c.urgency >= settings.flag_urgency_at;

    if flag {
        accepted(store, client, account, id, Action::Flag).await?;
    }

    Ok(flag)
}

/// Files an email under `tray` by the user's decision. The choice is
/// recorded first (inserting a manual row for an email Jev never saw), so
/// when the server cannot be reached the correction stays pending and the
/// next [`run`] retries it; without a connection for the account only the
/// local copy changes. A correction never flags.
///
/// # Errors
///
/// Returns the error that kept the change from being applied, and database
/// errors.
pub async fn correct(
    cfg: &Config,
    store: &Store,
    clients: &HashMap<String, Arc<Client>>,
    msg: &MsgRef,
    tray: Tray,
) -> Result<()> {
    let (account, id) = (msg.account_id.as_str(), msg.id.as_str());

    store.set_correction(account, id, tray.as_str())?;

    let row = store
        .classification(account, id)?
        .ok_or_else(|| Error::NotFound(format!("classification {msg}")))?;
    let client = clients.get(account).map(Arc::as_ref);

    match apply_row(cfg, store, client, &row).await {
        Ok(_) if client.is_none() => Ok(()),
        Ok(_) => {
            store.set_classification_applied(account, id, None)?;
            store.remove_from_classify_queue(account, id)
        }
        Err(e) => {
            store.set_classification_applied(account, id, Some(&e.to_string()))?;

            Err(e)
        }
    }
}

/// The newest `n` inbox emails per account that were never classified or
/// queued, for accounts where classification is enabled (`account` narrows
/// it to one). Reads only.
///
/// # Errors
///
/// Returns an error on database failure.
pub fn backfill_ids(
    store: &Store,
    cfg: &Config,
    account: Option<&str>,
    n: usize,
) -> Result<Vec<Candidate>> {
    let mut found = Vec::new();

    for a in cfg
        .accounts_enabled()
        .filter(|a| cfg.classify_enabled_for(a) && account.is_none_or(|id| id == a.id))
    {
        for email_id in store.unclassified_inbox_ids(&a.id, n)? {
            found.push(Candidate {
                account_id: a.id.clone(),
                email_id,
            });
        }
    }

    Ok(found)
}

/// Queues what [`backfill_ids`] finds and returns how many emails it added.
///
/// # Errors
///
/// Returns an error on database failure.
pub fn backfill_candidates(
    store: &Store,
    cfg: &Config,
    account: Option<&str>,
    n: usize,
) -> Result<usize> {
    let found = backfill_ids(store, cfg, account, n)?;
    let mut by_account: HashMap<&str, Vec<String>> = HashMap::new();

    for c in &found {
        by_account
            .entry(c.account_id.as_str())
            .or_default()
            .push(c.email_id.clone());
    }

    for (account, ids) in by_account {
        store.enqueue_for_classification(account, &ids)?;
    }

    Ok(found.len())
}
