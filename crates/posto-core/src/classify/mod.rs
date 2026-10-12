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
//!
//! A user's correction always wins over an automatic answer: an answer never
//! replaces an existing row, is not applied once the row carries a correction,
//! and a pending correction is retried on every run, outside the queue's
//! attempt budget.
//!
//! Mailbox changes to one message are serialised per [`Store`] handle
//! ([`Store::lock_message`]): each application re-reads the row once it has its
//! turn, so it acts on the latest choice, and an automatic answer never flags
//! mail once a correction exists. The mailboxes the classifier may have added
//! are kept in the row (`owned_trays`); a confirmation merges into the row as
//! it is then (adds its tray, drops only the trays it removed), so successive
//! corrections remove every earlier one even when another handle on the same
//! database (the CLI beside the GUI) changed the message in between. Handles
//! are not ordered against each other, but none can discard another's
//! ownership.

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
            // The batch was read at the start: the email may have been
            // corrected, classified or removed from the queue since.
            if !store.awaits_classification(account, id)? {
                return Ok(());
            }

            store.begin_classify_attempt(account, id)?;

            let Some(answer) = ask(cfg, store, client, classifier, account, id).await? else {
                return Ok(());
            };

            report.classified += 1;
            report.input_tokens += answer.input_tokens;

            // A correction made while Jev was answering wins.
            if !store.insert_answer(&answer)? {
                return Ok(());
            }

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
/// Automatic answers of enabled accounts tied to a queue entry share its
/// attempt budget; corrections have none and are tried on every run.
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

        // A correction is the user's request: it needs only a connection.
        // An automatic answer also needs the classifier to be enabled still.
        if row.corrected_tray.is_none() {
            if !cfg
                .account(account)
                .is_some_and(|a| cfg.classify_enabled_for(a))
            {
                continue;
            }

            if let Some(queued) = queue.get(&(row.account_id.clone(), row.email_id.clone())) {
                if queued.attempts >= CLASSIFY_MAX_ATTEMPTS {
                    continue;
                }

                store.begin_classify_attempt(account, id)?;
            }
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
/// queue, or pending with the reason. Waits for its turn on the message, then
/// works from the row as it is by then, not from `c`.
async fn settle(
    cfg: &Config,
    store: &Store,
    client: Option<&Client>,
    c: &Classified,
    report: &mut ClassifyReport,
) -> Result<()> {
    let (account, id) = (c.account_id.as_str(), c.email_id.as_str());
    let _turn = store.lock_message(account, id).await;
    let Some((row, revision)) = store.classification_with_revision(account, id)? else {
        return Ok(());
    };

    // Someone else confirmed it meanwhile, or an automatic answer gives way
    // to a correction made since it was read.
    if row.applied || (c.corrected_tray.is_none() && row.corrected_tray.is_some()) {
        return Ok(());
    }

    match apply_tray(cfg, store, client, &row, revision).await {
        Ok(done) => {
            report.moved += 1;
            report.flagged += usize::from(done.flagged);

            if store.confirm_classification(
                account,
                id,
                row.effective_tray(),
                &done.removed,
                done.patched,
                revision,
            )? {
                store.remove_from_classify_queue(account, id)?;
            }

            Ok(())
        }
        Err(e) => {
            store.set_classification_applied(account, id, Some(&e.to_string()))?;

            Err(e)
        }
    }
}

/// Builds the state block of one email, asks `classifier` and returns the
/// unapplied answer; `None` when the email stopped awaiting classification
/// (corrected, classified or dequeued) before Jev was asked.
async fn ask<C: Classifier>(
    cfg: &Config,
    store: &Store,
    client: &Client,
    classifier: &C,
    account: &str,
    id: &str,
) -> Result<Option<Classified>> {
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

    // The fetches above took time; do not pay for an answer nobody wants.
    if !store.awaits_classification(account, id)? {
        return Ok(None);
    }

    let c = classifier.classify(&state).await?;

    Ok(Some(Classified {
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
    }))
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

/// What [`apply_tray`] did to the server's copy of one email.
pub struct Applied {
    /// Whether the email was flagged.
    pub flagged: bool,
    /// The trays whose mailboxes the change took the email out of.
    pub removed: Vec<String>,
    /// Whether a mailbox patch went to the server. False when none was
    /// needed, there was no connection, or the row's intent had moved on
    /// before the patch (nothing was sent then).
    pub patched: bool,
}

/// Files the email per its effective tray and flags it when it is an urgent
/// uncorrected needs-reply; returns whether it was flagged and which trays
/// it left (what the caller must drop from the row's ownership). Mailbox changes
/// are patches, so memberships the classifier does not own are untouched:
///
/// - add the tray's mailbox, and the unsure mailbox when unsure;
/// - leave the inbox only when sure and the tray is not in `keep_in_inbox`;
/// - spam: sure goes to the junk mailbox and leaves the inbox; unsure stays
///   in the inbox and goes to the unsure mailbox only.
///
/// A corrected row also leaves the mailbox of each tray it was in before (the
/// last confirmed one and Jev's own answer), the junk mailbox when that was
/// spam, and the unsure mailbox, and returns to the inbox when the new tray
/// keeps mail there. Without a connection only the local copy changes.
///
/// `revision` is the intent revision `c` was read at (see
/// [`Store::classification_with_revision`]). It is checked immediately before
/// the patch goes out: when the row has moved on, nothing is sent or owned
/// and the result has `patched` false.
///
/// # Errors
///
/// Returns an error when a needed mailbox is missing and cannot be created,
/// when the server refuses the change, and on database failure.
pub async fn apply_tray(
    cfg: &Config,
    store: &Store,
    client: Option<&Client>,
    c: &Classified,
    revision: i64,
) -> Result<Applied> {
    let tray: Tray = c.effective_tray().parse()?;
    let (account, id) = (c.account_id.as_str(), c.email_id.as_str());
    let settings = &cfg.classify;
    let corrected = c.corrected_tray.is_some();
    let keeps = settings.keep_in_inbox.iter().any(|k| k == tray.as_str());
    let inbox = || {
        store
            .mailbox_by_role(account, "inbox")?
            .map(|m| m.id)
            .ok_or_else(|| Error::NotFound("inbox mailbox".into()))
    };
    let (mut add, mut remove) = (Vec::new(), Vec::new());
    let mut removed_trays = Vec::new();

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

    if corrected {
        for (previous, mailbox) in previous_mailboxes(cfg, store, c, tray)? {
            removed_trays.push(previous.as_str().to_string());
            remove.push(mailbox);
        }

        if !c.unsure
            && let Some(m) = store.top_level_mailbox_by_name(account, &settings.unsure_mailbox)?
        {
            remove.push(m.id);
        }

        if tray != Tray::Spam && keeps {
            add.push(inbox()?);
        }
    }

    let add = unique(add);
    let remove: Vec<String> = unique(remove)
        .into_iter()
        .filter(|r| !add.contains(r))
        .collect();

    // An older application must not send once a newer intent exists. This
    // narrows the window but cannot close it across handles; a patch that
    // still lands late is repaired by the pending pass (see
    // `Store::confirm_classification`).
    if store
        .classification_with_revision(account, id)?
        .is_none_or(|(_, now)| now != revision)
    {
        return Ok(Applied {
            flagged: false,
            removed: Vec::new(),
            patched: false,
        });
    }

    // Recorded before the request: if it fails half way, a later correction
    // still knows to take this tray's mailbox away.
    store.own_tray(account, id, tray.as_str())?;

    let patched = !add.is_empty() || !remove.is_empty();

    if patched {
        accepted(
            store,
            client,
            account,
            id,
            Action::PatchMailboxes { add, remove },
        )
        .await?;

        // A newer application may have dropped this tray's ownership while
        // the patch was in flight; the server holds it again now, so the
        // newest intent's re-application must be able to take it away.
        store.own_tray(account, id, tray.as_str())?;
    }

    // A flag cannot be taken back, so look at the row again: a correction
    // made while the patch was in flight forbids it.
    let flag = !corrected
        && tray == Tray::NeedsReply
        && c.urgency >= settings.flag_urgency_at
        && !store
            .classification(account, id)?
            .is_some_and(|now| now.corrected_tray.is_some());

    if flag {
        accepted(store, client, account, id, Action::Flag).await?;
    }

    Ok(Applied {
        flagged: flag,
        removed: removed_trays,
        patched: patched && client.is_some(),
    })
}

/// The mailboxes a correction to `new` undoes: those of every tray the
/// classifier may have added (the row's `owned_trays`) and of Jev's own answer
/// (unless the row is a manual one), whichever differ from `new`, each with
/// the mailbox it undoes. Spam undoes the junk mailbox. Mailboxes that do not
/// exist locally are skipped.
fn previous_mailboxes(
    cfg: &Config,
    store: &Store,
    c: &Classified,
    new: Tray,
) -> Result<Vec<(Tray, String)>> {
    let (account, id) = (c.account_id.as_str(), c.email_id.as_str());
    let mut before: Vec<Tray> = store
        .owned_trays(account, id)?
        .iter()
        .filter_map(|t| t.parse().ok())
        .collect();

    if c.model != "manual" {
        before.extend(c.tray.parse::<Tray>());
    }

    let mut found = Vec::new();

    for tray in before.into_iter().filter(|t| *t != new) {
        let mailbox = if tray == Tray::Spam {
            store.mailbox_by_role(account, "junk")?
        } else {
            match cfg.classify.trays.get(tray.as_str()) {
                Some(name) => store.top_level_mailbox_by_name(account, name)?,
                None => None,
            }
        };

        found.extend(mailbox.map(|m| (tray, m.id)));
    }

    Ok(found)
}

/// Files an email under `tray` by the user's decision. The choice is
/// recorded first (inserting a manual row for an email Jev never saw) and
/// takes the email out of the classification queue, so when the server cannot
/// be reached the correction stays pending and every later [`run`] retries it
/// outside the queue's attempt budget; without a connection for the account
/// only the local copy changes. A correction never flags.
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
    store.remove_from_classify_queue(account, id)?;

    // Intent is stored; now wait for any change in flight, then apply the
    // latest choice, which may be newer than `tray`.
    let _turn = store.lock_message(account, id).await;
    let (row, revision) = store
        .classification_with_revision(account, id)?
        .ok_or_else(|| Error::NotFound(format!("classification {msg}")))?;
    let client = clients.get(account).map(Arc::as_ref);

    match apply_tray(cfg, store, client, &row, revision).await {
        Ok(_) if client.is_none() => Ok(()),
        Ok(done) => {
            store.confirm_classification(
                account,
                id,
                row.effective_tray(),
                &done.removed,
                done.patched,
                revision,
            )?;

            Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Account;
    use crate::store::{EmailSummary, Mailbox};

    fn mailbox(id: &str, name: &str, role: Option<&str>) -> Mailbox {
        Mailbox {
            account_id: "a".into(),
            id: id.into(),
            name: name.into(),
            role: role.map(String::from),
            parent_id: None,
            sort_order: 1,
            total: 0,
            unread: 0,
        }
    }

    #[tokio::test]
    async fn settle_gives_way_to_a_correction_made_since_the_answer_was_read() {
        let store = Store::open_memory().unwrap();
        let cfg = Config {
            accounts: vec![Account::new("a", "me@example.com")],
            ..Config::default()
        };

        store
            .upsert_mailboxes(&[
                mailbox("inbox", "Inbox", Some("inbox")),
                mailbox("up", "Updates", None),
                mailbox("soc", "Social", None),
            ])
            .unwrap();
        store
            .upsert_emails(&[EmailSummary {
                account_id: "a".into(),
                id: "e1".into(),
                thread_id: "t".into(),
                received_at: "2026-10-01T10:00:00Z".into(),
                from: vec![],
                to: vec![],
                cc: vec![],
                subject: "s".into(),
                preview: "p".into(),
                size: 1,
                has_attachment: false,
                mailbox_ids: vec!["inbox".into()],
                keywords: vec![],
            }])
            .unwrap();

        let answer = Classified {
            account_id: "a".into(),
            email_id: "e1".into(),
            tray: "updates".into(),
            probs: Default::default(),
            urgency: 1,
            personal: 0.0,
            unsure: false,
            model: "m".into(),
            input_tokens: 0,
            classified_at: "2026-10-01T10:00:00Z".into(),
            corrected_tray: None,
            applied: false,
            apply_error: None,
        };

        store.put_classification(&answer).unwrap();

        // The user corrects after `answer` was read but before it is applied.
        store.set_correction("a", "e1", "social").unwrap();

        let mut report = ClassifyReport::default();

        settle(&cfg, &store, None, &answer, &mut report)
            .await
            .unwrap();

        let mailboxes = store.email("a", "e1").unwrap().unwrap().mailbox_ids;

        assert_eq!(mailboxes, ["inbox"]);
        assert_eq!(report.moved, 0);

        let row = store.classification("a", "e1").unwrap().unwrap();

        assert!(!row.applied);
        assert_eq!(row.corrected_tray.as_deref(), Some("social"));
    }
}
