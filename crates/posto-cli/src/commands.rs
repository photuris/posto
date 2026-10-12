//! One async function per command. Each returns the JSON value to print.

use std::path::{Path, PathBuf};

use posto_core::actions::{self, Action};
use posto_core::body::{fetch_body, save_attachment};
use posto_core::compose::{self, Draft};
use posto_core::config::{Config, Secrets};
use posto_core::jmap::Client;
use posto_core::shortcuts::{self, Keymap};
use posto_core::store::{ListQuery, Mailbox, Store};
use posto_core::{Error, Result, fixtures, html, sync, theme};
use serde::Serialize;
use serde_json::{Value, json};

fn to_json<T: Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| Error::Other(e.to_string()))
}

fn open_store() -> Result<Store> {
    Store::open(&Config::db_path())
}

/// Connects with the stored token; no token is `NotConfigured`.
async fn client() -> Result<Client> {
    let cfg = Config::load()?;
    let token = Secrets::token()?.ok_or_else(|| Error::NotConfigured("token".into()))?;

    Client::connect(&cfg.account.session_url, &token).await
}

/// `None` when no token exists, so local-only commands keep working offline.
async fn optional_client() -> Result<Option<Client>> {
    match Secrets::token()? {
        None => Ok(None),
        Some(_) => client().await.map(Some),
    }
}

/// Finds a mailbox by id, then by case-insensitive name.
fn resolve_mailbox(store: &Store, s: &str) -> Result<Mailbox> {
    if let Some(m) = store.mailboxes()?.into_iter().find(|m| m.id == s) {
        return Ok(m);
    }

    store
        .mailbox_by_name(s)?
        .ok_or_else(|| Error::NotFound(format!("mailbox {s}")))
}

fn find_email(store: &Store, id: &str) -> Result<posto_core::store::EmailSummary> {
    store
        .email(id)?
        .ok_or_else(|| Error::NotFound(format!("email {id}")))
}

/// Cached body, else fetched from the server (which needs a token).
async fn body_for(store: &Store, id: &str) -> Result<posto_core::store::EmailBody> {
    match store.body(id)? {
        Some(body) => Ok(body),
        None => fetch_body(store, Some(&client().await?), id).await,
    }
}

pub fn account_show() -> Result<Value> {
    let cfg = Config::load()?;

    Ok(json!({
        "email": cfg.account.email,
        "session_url": cfg.account.session_url,
        "has_token": Secrets::token()?.is_some(),
        "refresh_minutes": cfg.sync.refresh_minutes,
    }))
}

pub fn account_set_token() -> Result<Value> {
    let mut token = String::new();

    std::io::Read::read_to_string(&mut std::io::stdin(), &mut token)?;

    let token = token.trim();

    if token.is_empty() {
        return Err(Error::Config("empty token on stdin".into()));
    }

    Secrets::set_token(token)?;
    account_show()
}

pub fn account_set(email: Option<String>, session_url: Option<String>) -> Result<Value> {
    let mut cfg = Config::load()?;

    if let Some(email) = email {
        cfg.account.email = email;
    }

    if let Some(url) = session_url {
        cfg.account.session_url = url;
    }

    cfg.save()?;
    account_show()
}

pub async fn sync_run() -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;

    to_json(&sync::sync(&cfg, &store, &client().await?).await?)
}

pub fn mailbox_list() -> Result<Value> {
    to_json(&open_store()?.mailboxes()?)
}

pub fn email_list(mailbox: Option<String>, limit: u32, offset: u32, unread: bool) -> Result<Value> {
    let store = open_store()?;
    let mailbox_id = match mailbox {
        Some(m) => Some(resolve_mailbox(&store, &m)?.id),
        None => None,
    };

    to_json(&store.list_emails(&ListQuery {
        mailbox_id,
        limit,
        offset,
        unread_only: unread,
    })?)
}

pub async fn email_show(id: &str, text: bool) -> Result<Value> {
    let store = open_store()?;
    let summary = find_email(&store, id)?;
    let mut body = body_for(&store, id).await?;

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

pub fn email_search(query: &str, limit: u32) -> Result<Value> {
    to_json(&open_store()?.search(query, limit)?)
}

/// Applies `action` online when a token exists, else to the local store only.
pub async fn email_act(ids: Vec<String>, action: Action, name: &str) -> Result<Value> {
    let store = open_store()?;
    let client = optional_client().await?;
    let applied = actions::apply(&store, client.as_ref(), &ids, action).await?;

    Ok(json!({ "applied": applied, "action": name, "offline": client.is_none() }))
}

pub async fn email_move(mailbox: &str, ids: Vec<String>) -> Result<Value> {
    let mailbox_id = resolve_mailbox(&open_store()?, mailbox)?.id;

    email_act(ids, Action::Move { mailbox_id }, "move").await
}

pub async fn email_reply(id: &str, body_file: &Path, markdown: bool, all: bool) -> Result<Value> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let summary = find_email(&store, id)?;
    let text = std::fs::read_to_string(body_file)?;
    let client = client().await?;
    let body = fetch_body(&store, Some(&client), id).await?;
    let mut draft = compose::reply(&summary, &body, &cfg.account.email, all);

    draft.body = text + &draft.body;
    draft.markdown = markdown;

    Ok(json!({ "id": compose::send(&cfg, &store, &client, &draft).await? }))
}

pub async fn email_send(
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body_file: &Path,
    markdown: bool,
) -> Result<Value> {
    let draft = Draft {
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

    let cfg = Config::load()?;
    let store = open_store()?;
    let client = client().await?;

    Ok(json!({ "id": compose::send(&cfg, &store, &client, &draft).await? }))
}

pub async fn attachment_save(email_id: &str, blob_id: &str, dir: PathBuf) -> Result<Value> {
    let store = open_store()?;
    let client = client().await?;
    let body = fetch_body(&store, Some(&client), email_id).await?;
    let att = body
        .attachments
        .iter()
        .find(|a| a.blob_id == blob_id)
        .ok_or_else(|| Error::NotFound(format!("attachment {blob_id}")))?;

    std::fs::create_dir_all(&dir)?;

    Ok(json!({ "path": save_attachment(&client, att, &dir).await? }))
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
        Move, Search, Sync, Junk, Send, Escape,
    ] {
        if let Some(chord) = keymap.chord_for(action) {
            rows.push((to_json(&action)?, chord.to_string()));
        }
    }

    rows.sort_by_key(|(action, _)| action.as_str().unwrap_or_default().to_string());

    Ok(Value::Array(
        rows.into_iter()
            .map(|(action, chord)| json!({ "action": action, "chord": chord }))
            .collect(),
    ))
}

pub fn dev_seed() -> Result<Value> {
    fixtures::seed(&open_store()?)?;

    Ok(json!({ "seeded": true }))
}
