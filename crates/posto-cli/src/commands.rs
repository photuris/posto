//! One async function per command. Each returns the JSON value to print.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::CommandFactory;
use clap::error::ErrorKind;
use posto_core::actions::{self, Action};
use posto_core::body::{fetch_body, save_attachment};
use posto_core::classify::{self, JEV_BASE_URL, JevClient, Tray};
use posto_core::compose::{self, Draft};
use posto_core::config::{Account, Config, ProtonSecret, Provider, Secrets};
use posto_core::provider::{self, Connection};
use posto_core::shortcuts::{self, Keymap};
use posto_core::store::{
    CLASSIFY_MAX_ATTEMPTS, EmailBody, EmailSummary, ListQuery, Mailbox, Store,
};
use posto_core::{Error, Result, fixtures, html, proton, sync, theme};
use serde::Serialize;
use serde_json::{Value, json};

fn to_json<T: Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| Error::Other(e.to_string()))
}

fn open_store() -> Result<Store> {
    Store::open(&Config::db_path())
}

/// The account a command acts on: `--account`, else the first enabled one.
fn pick_account<'a>(cfg: &'a Config, flag: Option<&str>) -> Result<&'a Account> {
    match flag {
        Some(id) => cfg
            .account(id)
            .ok_or_else(|| Error::NotFound(format!("account {id}"))),
        None => cfg
            .default_account()
            .ok_or_else(|| Error::NotConfigured("no enabled account".into())),
    }
}

/// The ids of the accounts a read covers: `--account`, else every account.
fn scope(cfg: &Config, flag: Option<&str>) -> Result<Vec<String>> {
    match flag {
        Some(_) => Ok(vec![pick_account(cfg, flag)?.id.clone()]),
        None => Ok(cfg.accounts.iter().map(|a| a.id.clone()).collect()),
    }
}

/// Connects with the account's stored credentials; none is `NotConfigured`.
async fn client_for(cfg: &Config, account: &Account) -> Result<Connection> {
    provider::connect(cfg, account)
        .await?
        .ok_or_else(|| provider::credential_error(account))
}

/// `None` when the account has no credentials, so local-only commands keep working offline.
async fn optional_client(cfg: &Config, account: &Account) -> Result<Option<Connection>> {
    provider::connect(cfg, account).await
}

/// The configured account with this id.
fn configured<'a>(cfg: &'a Config, id: &str) -> Result<&'a Account> {
    pick_account(cfg, Some(id))
}

/// Finds a mailbox of `account` by id, then by case-insensitive name.
fn find_mailbox(store: &Store, account: &str, s: &str) -> Result<Option<Mailbox>> {
    if let Some(m) = store.mailboxes(account)?.into_iter().find(|m| m.id == s) {
        return Ok(Some(m));
    }

    store.mailbox_by_name(account, s)
}

fn resolve_mailbox(store: &Store, account: &str, s: &str) -> Result<Mailbox> {
    find_mailbox(store, account, s)?.ok_or_else(|| Error::NotFound(format!("mailbox {s}")))
}

/// Finds an email by bare id: in `--account`, else across all accounts.
///
/// # Errors
///
/// `NotFound` when no account has it; `Other` when several do.
fn find_email(store: &Store, cfg: &Config, flag: Option<&str>, id: &str) -> Result<EmailSummary> {
    let mut found = Vec::new();

    for account in scope(cfg, flag)? {
        found.extend(store.email(&account, id)?);
    }

    match found.len() {
        0 => Err(Error::NotFound(format!("email {id}"))),
        1 => Ok(found.remove(0)),
        _ => Err(Error::Other("ambiguous id; pass --account".into())),
    }
}

/// Resolves every id and groups them by account, in first-seen order.
fn group_by_account(
    store: &Store,
    cfg: &Config,
    flag: Option<&str>,
    ids: &[String],
) -> Result<Vec<(String, Vec<String>)>> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();

    for id in ids {
        let account = find_email(store, cfg, flag, id)?.account_id;

        match groups.iter_mut().find(|(a, _)| *a == account) {
            Some((_, list)) => list.push(id.clone()),
            None => groups.push((account, vec![id.clone()])),
        }
    }

    Ok(groups)
}

/// Cached body, else fetched from the account's server (which needs a token).
async fn body_for(store: &Store, cfg: &Config, email: &EmailSummary) -> Result<EmailBody> {
    match store.body(&email.account_id, &email.id)? {
        Some(body) => Ok(body),
        None => {
            let client = client_for(cfg, configured(cfg, &email.account_id)?).await?;

            fetch_body(store, Some(&client), &email.account_id, &email.id).await
        }
    }
}

/// An account as the CLI prints it.
fn account_json(cfg: &Config, a: &Account) -> Result<Value> {
    Ok(json!({
        "id": a.id,
        "name": a.name,
        "email": a.email,
        "provider": a.provider.as_str(),
        "username": a.username,
        "session_url": a.session_url,
        "enabled": a.enabled,
        "has_token": Secrets::has_credentials(cfg, a)?,
    }))
}

pub fn account_list() -> Result<Value> {
    let cfg = Config::load()?;

    Ok(Value::Array(
        cfg.accounts
            .iter()
            .map(|a| account_json(&cfg, a))
            .collect::<Result<_>>()?,
    ))
}

pub fn account_show(account: Option<&str>) -> Result<Value> {
    let cfg = Config::load()?;
    let mut shown = account_json(&cfg, pick_account(&cfg, account)?)?;

    shown["refresh_minutes"] = json!(cfg.sync.refresh_minutes);

    Ok(shown)
}

pub fn account_add(
    id: String,
    email: String,
    name: Option<String>,
    session_url: Option<String>,
    provider: Provider,
    username: Option<String>,
) -> Result<Value> {
    let mut cfg = Config::load()?;
    let mut account = match provider {
        Provider::Jmap => Account::new(&id, &email),
        Provider::Proton => Account::proton(&id, &email),
    };

    account.name = name.unwrap_or_default();
    account.username = username.unwrap_or_default();

    if let Some(url) = session_url {
        account.session_url = url;
    }

    cfg.add_account(account)?;
    cfg.save()?;
    account_show(Some(&id))
}

pub fn account_remove(id: &str) -> Result<Value> {
    let mut cfg = Config::load()?;

    cfg.remove_account(id)?;
    cfg.save()?;
    open_store()?.delete_account(id)?;
    Secrets::clear_account(id)?;

    Ok(json!({ "removed": id }))
}

pub fn account_identities(account: Option<&str>) -> Result<Value> {
    let cfg = Config::load()?;

    to_json(&open_store()?.identities(&pick_account(&cfg, account)?.id)?)
}

pub fn account_set_token(account: Option<&str>) -> Result<Value> {
    let cfg = Config::load()?;
    let picked = pick_account(&cfg, account)?;

    if picked.provider == Provider::Proton {
        return Err(Error::Config(format!(
            "account {} is a Proton account: use `account login`",
            picked.id
        )));
    }

    let id = picked.id.clone();
    let mut token = String::new();

    std::io::Read::read_to_string(&mut std::io::stdin(), &mut token)?;

    let token = token.trim();

    if token.is_empty() {
        return Err(Error::Config("empty token on stdin".into()));
    }

    Secrets::set_token(&id, token)?;
    account_show(Some(&id))
}

pub fn account_set(
    account: Option<&str>,
    email: Option<String>,
    session_url: Option<String>,
    name: Option<String>,
    username: Option<String>,
    enabled: Option<bool>,
) -> Result<Value> {
    let mut cfg = Config::load()?;
    let id = pick_account(&cfg, account)?.id.clone();

    if let Some(a) = cfg.accounts.iter_mut().find(|a| a.id == id) {
        if let Some(email) = email {
            a.email = email;
        }

        if let Some(url) = session_url {
            a.session_url = url;
        }

        if let Some(name) = name {
            a.name = name;
        }

        if let Some(username) = username {
            a.username = username;
        }

        if let Some(enabled) = enabled {
            a.enabled = enabled;
        }
    }

    cfg.save()?;
    account_show(Some(&id))
}

/// Signs in to a Proton account with the password read from stdin, and
/// stores the session.
pub async fn account_login(
    account: Option<&str>,
    totp: Option<String>,
    two_password: bool,
) -> Result<Value> {
    let mut cfg = Config::load()?;
    let picked = pick_account(&cfg, account)?.clone();

    if picked.provider != Provider::Proton {
        return Err(Error::Config(format!(
            "account {} is not a Proton account",
            picked.id
        )));
    }

    let mut input = String::new();

    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;

    let mut lines = input.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let password = lines.next().unwrap_or_default().to_owned();

    if password.is_empty() {
        return Err(Error::Config("empty password on stdin".into()));
    }

    let mailbox_password = if two_password {
        match lines.next().filter(|l| !l.is_empty()) {
            Some(line) => Some(line.to_owned()),
            None => {
                return Err(Error::Config(
                    "mailbox password missing: give it on the second stdin line".into(),
                ));
            }
        }
    } else {
        None
    };

    drop(input);

    let app_version = cfg
        .proton_app_version()
        .ok_or_else(|| Error::NotConfigured("proton.app_version".into()))?;
    let login = proton::login(
        &app_version,
        picked.login_name(),
        &password,
        totp.as_deref(),
        mailbox_password.as_deref(),
    )
    .await;

    drop(password);
    drop(mailbox_password);

    let login = login?;

    Secrets::set_proton(
        &picked.id,
        &ProtonSecret {
            session: login.session,
            mailbox_password: login.mailbox_password,
            generation: 0,
        },
    )?;

    if picked.email.is_empty()
        && !login.email.is_empty()
        && let Some(a) = cfg.accounts.iter_mut().find(|a| a.id == picked.id)
    {
        a.email = login.email;
        cfg.save()?;
    }

    account_show(Some(&picked.id))
}

/// The accounts `--account` selects (default: every enabled one); none is an error.
fn targets(cfg: &Config, account: Option<&str>) -> Result<Vec<Account>> {
    let targets: Vec<Account> = match account {
        Some(_) => vec![pick_account(cfg, account)?.clone()],
        None => cfg.accounts_enabled().cloned().collect(),
    };

    if targets.is_empty() {
        return Err(Error::NotConfigured("no enabled account".into()));
    }

    Ok(targets)
}

/// Connects every target that has credentials. An account that cannot connect is
/// returned with the reason instead of failing the others.
async fn connect_clients(
    cfg: &Config,
    targets: &[Account],
) -> Result<(HashMap<String, Arc<Connection>>, HashMap<String, String>)> {
    let mut clients: HashMap<String, Arc<Connection>> = HashMap::new();
    let mut unreachable: HashMap<String, String> = HashMap::new();

    for a in targets {
        if !Secrets::has_credentials(cfg, a)? {
            continue;
        }

        match client_for(cfg, a).await {
            Ok(client) => {
                clients.insert(a.id.clone(), Arc::new(client));
            }
            Err(e) => {
                unreachable.insert(a.id.clone(), e.to_string());
            }
        }
    }

    Ok((clients, unreachable))
}

/// Syncs `targets` and returns the reports with the connections used.
///
/// One target keeps typed errors (and exit codes); several report each
/// failure in its own entry.
async fn sync_targets(
    cfg: &Config,
    store: &Store,
    targets: &[Account],
) -> Result<(Vec<sync::SyncReport>, HashMap<String, Arc<Connection>>)> {
    if let [only] = targets {
        let client = Arc::new(client_for(cfg, only).await?);
        let report = sync::sync_account(&sync::lock_path(), cfg, &only.id, store, &client).await?;

        return Ok((vec![report], HashMap::from([(only.id.clone(), client)])));
    }

    let (clients, mut unreachable) = connect_clients(cfg, targets).await?;
    let scoped = Config {
        accounts: targets
            .iter()
            .map(|a| Account {
                enabled: true,
                ..a.clone()
            })
            .collect(),
        ..cfg.clone()
    };
    let mut reports = sync::sync_all(&scoped, store, &clients).await?;

    for r in &mut reports {
        if let Some(e) = unreachable.remove(&r.account_id) {
            r.error = Some(e);
        }
    }

    Ok((reports, clients))
}

/// Syncs the accounts `--account` selects (default: every enabled one).
pub async fn sync_run(account: Option<&str>) -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let targets = targets(&cfg, account)?;

    to_json(&sync_targets(&cfg, &store, &targets).await?.0)
}

pub fn mailbox_list(account: Option<&str>) -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let mut all = Vec::new();

    for id in scope(&cfg, account)? {
        all.extend(store.mailboxes(&id)?);
    }

    to_json(&all)
}

pub fn email_list(
    account: Option<&str>,
    mailbox: Option<String>,
    limit: u32,
    offset: u32,
    unread: bool,
) -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let accounts = scope(&cfg, account)?;
    let query = |mailbox_id: Option<String>, limit: u32, offset: u32| ListQuery {
        mailbox_id,
        limit,
        offset,
        unread_only: unread,
        ..ListQuery::default()
    };

    let Some(name) = mailbox else {
        return to_json(&store.list_emails(
            account.map(|_| accounts[0].as_str()),
            &query(None, limit, offset),
        )?);
    };

    // A name such as "Inbox" matches in several accounts: merge their rows.
    let mut members = Vec::new();

    for id in &accounts {
        if let Some(m) = find_mailbox(&store, id, &name)? {
            members.push((id.clone(), m.id));
        }
    }

    if members.is_empty() {
        return Err(Error::NotFound(format!("mailbox {name}")));
    }

    to_json(&store.list_emails_in(&members, &query(None, limit, offset))?)
}

pub async fn email_show(account: Option<&str>, id: &str, text: bool) -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let summary = find_email(&store, &cfg, account, id)?;
    let mut body = body_for(&store, &cfg, &summary).await?;

    if text {
        if body.text.as_deref().is_none_or(str::is_empty) && body.html.is_some() {
            body.text = body.html.as_deref().map(html::to_text);
        }

        body.html = None;
    }

    Ok(json!({
        "summary": summary,
        "body": { "text": body.text, "html": body.html, "attachments": body.attachments },
    }))
}

pub fn email_search(account: Option<&str>, query: &str, limit: u32) -> Result<Value> {
    let cfg = Config::load()?;
    let accounts = scope(&cfg, account)?;

    to_json(&open_store()?.search(account.map(|_| accounts[0].as_str()), query, limit)?)
}

/// Applies the action `make` builds for each account's ids: online when that
/// account has a token, else to the local store only.
pub async fn email_act(
    account: Option<&str>,
    ids: Vec<String>,
    make: impl Fn(&Store, &str) -> Result<Action>,
    name: &str,
) -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let (mut applied, mut offline) = (0, false);

    for (id, ids) in group_by_account(&store, &cfg, account, &ids)? {
        let client = optional_client(&cfg, configured(&cfg, &id)?).await?;

        offline |= client.is_none();
        applied += actions::apply(&store, client.as_ref(), &id, &ids, make(&store, &id)?).await?;
    }

    Ok(json!({ "applied": applied, "action": name, "offline": offline }))
}

/// Permanently deletes `ids`; without `yes` it exits 2 as a usage error.
pub async fn email_destroy(account: Option<&str>, ids: Vec<String>, yes: bool) -> Result<Value> {
    if !yes {
        crate::cli::Cli::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                "refusing to destroy without --yes",
            )
            .exit();
    }

    email_act(account, ids, |_, _| Ok(Action::Destroy), "destroy").await
}

pub async fn email_move(account: Option<&str>, mailbox: &str, ids: Vec<String>) -> Result<Value> {
    email_act(
        account,
        ids,
        |store, id| {
            Ok(Action::Move {
                mailbox_id: resolve_mailbox(store, id, mailbox)?.id,
            })
        },
        "move",
    )
    .await
}

/// Adds (`add`) or removes the mailbox `mailbox` on `ids`; the core refuses to
/// empty an email's mailbox set.
pub async fn email_label(
    account: Option<&str>,
    mailbox: &str,
    ids: Vec<String>,
    add: bool,
) -> Result<Value> {
    email_act(
        account,
        ids,
        |store, id| {
            let target = vec![resolve_mailbox(store, id, mailbox)?.id];

            Ok(if add {
                Action::PatchMailboxes {
                    add: target,
                    remove: vec![],
                }
            } else {
                Action::PatchMailboxes {
                    add: vec![],
                    remove: target,
                }
            })
        },
        if add { "label" } else { "unlabel" },
    )
    .await
}

/// Parses a `--from` value; an invalid address is an error, never ignored.
fn parse_from(from: Option<String>) -> Result<Option<posto_core::store::Address>> {
    let Some(from) = from else {
        return Ok(None);
    };

    compose::parse_addresses(&from)
        .into_iter()
        .next()
        .map(Some)
        .ok_or_else(|| Error::Other(format!("invalid --from address {from}")))
}

pub async fn email_reply(
    account: Option<&str>,
    id: &str,
    body_file: &Path,
    markdown: bool,
    all: bool,
    from: Option<String>,
) -> Result<Value> {
    let from = parse_from(from)?;
    let cfg = Config::load()?;
    let store = open_store()?;
    let summary = find_email(&store, &cfg, account, id)?;
    let text = std::fs::read_to_string(body_file)?;
    let owner = configured(&cfg, &summary.account_id)?;
    let client = client_for(&cfg, owner).await?;
    let body = fetch_body(&store, Some(&client), &owner.id, id).await?;
    let identities = store.identities(&owner.id)?;
    let mut draft = compose::reply(&summary, &body, &owner.email, all, &identities);

    draft.body = text + &draft.body;
    draft.markdown = markdown;

    if from.is_some() {
        draft.from = from;
    }

    Ok(json!({ "id": compose::send(&cfg, &store, &client, &owner.id, &draft).await? }))
}

#[expect(clippy::too_many_arguments, reason = "one parameter per CLI flag")]
pub async fn email_send(
    account: Option<&str>,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body_file: &Path,
    markdown: bool,
    from: Option<String>,
) -> Result<Value> {
    let cfg = Config::load()?;
    let owner = pick_account(&cfg, account)?;
    let draft = Draft {
        account_id: owner.id.clone(),
        from: parse_from(from)?,
        to: compose::parse_addresses(&to.join(",")),
        cc: compose::parse_addresses(&cc.join(",")),
        bcc: compose::parse_addresses(&bcc.join(",")),
        subject,
        body: std::fs::read_to_string(body_file)?,
        markdown,
        ..Draft::default()
    };

    if draft.to.is_empty() {
        return Err(Error::Other("no valid --to address".into()));
    }

    let store = open_store()?;
    let client = client_for(&cfg, owner).await?;

    Ok(json!({ "id": compose::send(&cfg, &store, &client, &owner.id, &draft).await? }))
}

pub async fn attachment_save(
    account: Option<&str>,
    email_id: &str,
    blob_id: &str,
    dir: PathBuf,
) -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let email = find_email(&store, &cfg, account, email_id)?;
    let client = client_for(&cfg, configured(&cfg, &email.account_id)?).await?;
    let body = fetch_body(&store, Some(&client), &email.account_id, email_id).await?;
    let att = body
        .attachments
        .iter()
        .find(|a| a.blob_id == blob_id)
        .ok_or_else(|| Error::NotFound(format!("attachment {blob_id}")))?;

    std::fs::create_dir_all(&dir)?;

    Ok(json!({ "path": save_attachment(&client, &email.id, att, &dir).await? }))
}

pub fn theme_show() -> Result<Value> {
    to_json(&theme::detect())
}

pub fn shortcuts_list() -> Result<Value> {
    use shortcuts::Action::*;

    let keymap = Keymap::from_config(&Config::load()?.shortcuts)?;
    let mut rows = Vec::new();

    for action in [
        Next, Prev, Open, Back, Archive, Delete, Reply, ReplyAll, Compose, ToggleRead, ToggleFlag,
        Move, Search, Sync, Junk, Send, Escape, Classify, Label,
    ] {
        let chords = keymap.chords_for(action);

        if !chords.is_empty() {
            let text: Vec<_> = chords.iter().map(|c| c.to_string()).collect();

            rows.push((to_json(&action)?, text.join(", ")));
        }
    }

    rows.sort_by_key(|(action, _)| action.as_str().unwrap_or_default().to_string());

    Ok(Value::Array(
        rows.into_iter()
            .map(|(action, chord)| json!({ "action": action, "chord": chord }))
            .collect(),
    ))
}

/// State block of a queued or backfill email, built from what is cached (no
/// network: the body falls back to the preview and the List-Unsubscribe
/// header is assumed absent).
fn dry_state(cfg: &Config, store: &Store, account: &str, id: &str) -> Result<String> {
    let email = store
        .email(account, id)?
        .ok_or_else(|| Error::NotFound(format!("email {account}/{id}")))?;
    let body = store.body(account, id)?.unwrap_or(EmailBody {
        email_id: id.to_string(),
        html: None,
        text: None,
        attachments: Vec::new(),
        in_reply_to: Vec::new(),
        references: Vec::new(),
        message_id: Vec::new(),
    });
    let mut own: Vec<String> = store
        .identities(account)?
        .into_iter()
        .map(|i| i.email)
        .collect();

    own.extend(cfg.account(account).map(|a| a.email.clone()));

    let replied = store.thread_has_reply_from(account, &email.thread_id, id, &own)?;

    Ok(classify::state::state_for(
        &email,
        &body,
        false,
        replied,
        cfg.classify.body_chars as usize,
    ))
}

/// What `classify run` would do, from the store alone.
fn classify_dry_run(
    cfg: &Config,
    store: &Store,
    targets: &[Account],
    account: Option<&str>,
    limit: Option<usize>,
    backfill: Option<usize>,
) -> Result<Value> {
    let wanted = |id: &str| targets.iter().any(|a| a.id == id);
    let eligible: Vec<String> = targets
        .iter()
        .filter(|a| cfg.classify_enabled_for(a))
        .map(|a| a.id.clone())
        .collect();
    let cap = limit.unwrap_or(cfg.classify.max_per_run as usize);
    let mut pending = Vec::new();

    for c in store
        .pending_classifications()?
        .iter()
        .filter(|c| wanted(&c.account_id))
    {
        pending.push(json!({
            "account_id": c.account_id,
            "email_id": c.email_id,
            "tray": c.effective_tray(),
            "state": dry_state(cfg, store, &c.account_id, &c.email_id)?,
        }));
    }

    let mut queued = Vec::new();

    for row in store.next_classify_batch(&eligible, cap)? {
        queued.push(json!({
            "account_id": row.account_id,
            "email_id": row.email_id,
            "attempts": row.attempts,
            "state": dry_state(cfg, store, &row.account_id, &row.email_id)?,
        }));
    }

    let mut found = Vec::new();

    if let Some(n) = backfill {
        for c in classify::backfill_ids(store, cfg, account, n)? {
            found.push(json!({
                "account_id": c.account_id,
                "email_id": c.email_id,
                "state": dry_state(cfg, store, &c.account_id, &c.email_id)?,
            }));
        }
    }

    Ok(json!({ "dry_run": true, "pending_apply": pending, "queued": queued, "backfill": found }))
}

pub async fn classify_run(
    account: Option<&str>,
    limit: Option<usize>,
    backfill: Option<usize>,
    retry_failed: bool,
    dry_run: bool,
) -> Result<Value> {
    let cfg = Config::load()?;
    let targets = targets(&cfg, account)?;

    if dry_run {
        // Read-only: no cache is created, rebuilt or written.
        let store = Store::open_read_only(&Config::db_path())?;

        return classify_dry_run(&cfg, &store, &targets, account, limit, backfill);
    }

    let store = open_store()?;

    // Before any network traffic, including the sync.
    let key =
        Secrets::typesafe_key()?.ok_or_else(|| Error::NotConfigured("typesafe key".into()))?;

    if !targets.iter().any(|a| cfg.classify_enabled_for(a)) {
        return Err(Error::NotConfigured(
            "classification is not enabled for the selected accounts".into(),
        ));
    }

    let reset = if retry_failed {
        store.reset_failed_classifications(account)?
    } else {
        0
    };
    let (sync_reports, clients, enqueued, unreachable) = match backfill {
        Some(n) => {
            let (clients, unreachable) = connect_clients(&cfg, &targets).await?;
            let enqueued = classify::backfill_candidates(&store, &cfg, account, n)?;

            (None, clients, enqueued, unreachable)
        }
        None => {
            let (reports, clients) = sync_targets(&cfg, &store, &targets).await?;

            (Some(reports), clients, 0, HashMap::new())
        }
    };
    let jev = JevClient::new(
        JEV_BASE_URL,
        &key,
        &cfg.classify.model,
        cfg.classify.unsure_margin,
    );
    let mut report = classify::run(&cfg, &store, &clients, &jev, limit).await;

    report
        .errors
        .extend(unreachable.into_iter().map(|(id, e)| format!("{id}: {e}")));

    Ok(json!({
        "sync": sync_reports.map(|r| to_json(&r)).transpose()?,
        "reset": reset,
        "enqueued": enqueued,
        "classify": report,
    }))
}

pub fn classify_status(account: Option<&str>) -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let mut accounts = Vec::new();

    for id in scope(&cfg, account)? {
        let rows = store.classifications(Some(&id), false, 0)?;
        let queue = store.classify_queue(Some(&id))?;
        let mut by_tray: BTreeMap<&str, usize> = BTreeMap::new();

        for c in &rows {
            *by_tray.entry(c.effective_tray()).or_default() += 1;
        }

        accounts.push(json!({
            "id": id,
            "classified": rows.len(),
            "unsure": rows.iter().filter(|c| c.unsure).count(),
            "queued": queue.iter().filter(|r| r.attempts < CLASSIFY_MAX_ATTEMPTS).count(),
            "failed": queue.iter().filter(|r| r.attempts >= CLASSIFY_MAX_ATTEMPTS).count(),
            "pending_apply": rows.iter().filter(|c| !c.applied).count(),
            "by_tray": by_tray,
            "last_classified_at": rows.iter().map(|c| c.classified_at.as_str()).max(),
        }));
    }

    Ok(json!({ "accounts": accounts }))
}

pub fn classify_show(account: Option<&str>, id: &str) -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let email = find_email(&store, &cfg, account, id)?;

    store
        .classification(&email.account_id, &email.id)?
        .ok_or_else(|| Error::NotFound(format!("classification of {id}")))
        .and_then(|c| to_json(&c))
}

pub async fn classify_correct(account: Option<&str>, id: &str, tray: &str) -> Result<Value> {
    let tray: Tray = tray.parse()?;
    let cfg = Config::load()?;
    let store = open_store()?;
    let email = find_email(&store, &cfg, account, id)?;

    // Intent first: whatever happens to the connection, the choice is kept.
    store.set_correction(&email.account_id, &email.id, tray.as_str())?;

    let (client, unreachable) =
        match optional_client(&cfg, configured(&cfg, &email.account_id)?).await {
            Ok(client) => (client, None),
            Err(e @ (Error::Http(_) | Error::Auth | Error::Jmap(_))) => (None, Some(e.to_string())),
            Err(e) => return Err(e),
        };
    let offline = client.is_none();
    let clients: HashMap<String, Arc<Connection>> = client
        .map(|c| (email.account_id.clone(), Arc::new(c)))
        .into_iter()
        .collect();

    classify::correct(&cfg, &store, &clients, &email.msg_ref(), tray).await?;

    Ok(json!({
        "account_id": email.account_id,
        "id": email.id,
        "tray": tray,
        "offline": offline,
        "applied": !offline,
        "pending": offline,
        "error": unreachable,
    }))
}

pub fn classify_key() -> Result<Value> {
    let mut key = String::new();

    std::io::Read::read_to_string(&mut std::io::stdin(), &mut key)?;

    let key = key.trim();

    if key.is_empty() {
        return Err(Error::Config("empty key on stdin".into()));
    }

    Secrets::set_typesafe_key(key)?;

    Ok(json!({ "has_key": true }))
}

pub fn dev_seed() -> Result<Value> {
    fixtures::seed(&open_store()?)?;

    Ok(json!({ "seeded": true }))
}
