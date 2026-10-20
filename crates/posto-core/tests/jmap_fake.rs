//! Integration tests against a wiremock fake JMAP server.

use posto_core::actions::{self, Action, apply, in_trash};
use posto_core::body::{fetch_body, save_attachment};
use posto_core::compose::{Draft, send};
use posto_core::config::{Account, Config};
use posto_core::jmap::Client;
use posto_core::provider::Connection;
use posto_core::store::{EmailBody, EmailSummary, Mailbox, Store};
use posto_core::sync::{sync_account, sync_all_with_lock};
use posto_core::{Error, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const SESSION: &str = include_str!("fixtures/session.json");

/// The masked email capability, as the session advertises it.
const MASKED: &str = "https://www.fastmail.com/dev/maskedemail";

/// The `using` of every request that does not need the masked capability.
const USING: [&str; 3] = [
    "urn:ietf:params:jmap:core",
    "urn:ietf:params:jmap:mail",
    "urn:ietf:params:jmap:submission",
];

/// Matches `POST /api` requests whose first call has this name and whose
/// arguments contain every key/value of `args`.
struct Call {
    name: &'static str,
    args: Value,
}

impl Match for Call {
    fn matches(&self, request: &Request) -> bool {
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            return false;
        };
        let call = &body["methodCalls"][0];

        call[0] == self.name
            && self
                .args
                .as_object()
                .unwrap()
                .iter()
                .all(|(k, v)| call[1].get(k) == Some(v))
    }
}

async fn server() -> MockServer {
    server_with(SESSION.to_string()).await
}

/// A server whose session also advertises the masked email capability.
async fn masked_server() -> MockServer {
    let session = SESSION.replace(
        r#""urn:ietf:params:jmap:submission": {}"#,
        &format!(r#""urn:ietf:params:jmap:submission": {{}}, "{MASKED}": {{}}"#),
    );

    assert_ne!(session, SESSION, "the fixture changed shape");

    server_with(session).await
}

async fn server_with(session: String) -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/session"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(session.replace("{{base}}", &server.uri()))
                .insert_header("content-type", "application/json"),
        )
        .mount(&server)
        .await;

    server
}

async fn raw_client(server: &MockServer) -> Client {
    Client::connect(&format!("{}/session", server.uri()), "tok")
        .await
        .unwrap()
}

async fn client(server: &MockServer) -> Connection {
    Connection::Jmap(raw_client(server).await)
}

/// Mounts a one-call reply for `name` whose args contain `args`.
async fn reply(server: &MockServer, name: &'static str, args: Value, response: Value, times: u64) {
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(Call { name, args })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [[name, response, "0"]],
        })))
        .expect(times)
        .mount(server)
        .await;
}

/// Mounts a method-level error reply.
async fn reply_error(server: &MockServer, name: &'static str, args: Value, kind: &str) {
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(Call { name, args })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [["error", { "type": kind }, "0"]],
        })))
        .expect(1)
        .mount(server)
        .await;
}

fn mailbox_json(id: &str, name: &str, role: Option<&str>) -> Value {
    json!({
        "id": id, "name": name, "role": role, "parentId": null, "sortOrder": 1,
        "totalEmails": 5, "unreadEmails": 2,
    })
}

fn email_json(id: &str) -> Value {
    json!({
        "id": id, "threadId": format!("t-{id}"), "receivedAt": "2026-10-01T10:00:00Z",
        "mailboxIds": { "mb-inbox": true, "mb-other": false },
        "keywords": { "$seen": true, "$flagged": false },
        "from": [{ "name": "Ann", "email": "ann@example.com" }],
        "to": [{ "name": null, "email": "me@example.com" }],
        "cc": null,
        "subject": format!("Subject {id}"), "preview": "hello", "size": 1234,
        "hasAttachment": false,
    })
}

/// A config whose only account is `a`.
fn cfg_a() -> Config {
    Config {
        accounts: vec![Account::new("a", "me@example.com")],
        ..Config::default()
    }
}

fn email(id: &str, mailbox: &str) -> EmailSummary {
    EmailSummary {
        account_id: "a".into(),
        id: id.into(),
        thread_id: format!("t-{id}"),
        received_at: "2026-10-01T10:00:00Z".into(),
        from: vec![],
        to: vec![],
        cc: vec![],
        subject: format!("Subject {id}"),
        preview: "hello".into(),
        size: 10,
        has_attachment: false,
        mailbox_ids: vec![mailbox.into()],
        keywords: vec![],
    }
}

fn mailbox(id: &str, role: &str) -> Mailbox {
    Mailbox {
        account_id: "a".into(),
        id: id.into(),
        name: role.into(),
        role: Some(role.into()),
        parent_id: None,
        sort_order: 1,
        total: 0,
        unread: 0,
    }
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn connect_parses_session() {
    let server = server().await;
    let c = raw_client(&server).await;

    assert_eq!(c.session().account_id, "acc1");
    assert!(c.session().api_url.ends_with("/api"));
}

#[tokio::test]
async fn connect_401_is_auth_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/session"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = Client::connect(&format!("{}/session", server.uri()), "bad")
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Auth), "{err:?}");
}

#[tokio::test]
async fn call_one_maps_error_response() {
    let server = server().await;
    let c = raw_client(&server).await;

    reply_error(&server, "Foo/bar", json!({}), "unknownMethod").await;

    let err = c.call_one("Foo/bar", json!({})).await.unwrap_err();

    assert!(
        matches!(&err, Error::Jmap(m) if m.contains("unknownMethod")),
        "{err:?}"
    );
}

#[tokio::test]
async fn initial_sync_fills_store() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    reply(
        &server,
        "Mailbox/get",
        json!({ "ids": null }),
        json!({ "state": "m1", "list": [
            mailbox_json("mb-inbox", "Inbox", Some("inbox")),
            mailbox_json("mb-archive", "Archive", Some("archive")),
            mailbox_json("mb-trash", "Trash", Some("trash")),
        ] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/get",
        json!({ "ids": [] }),
        json!({ "state": "s0", "list": [] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/query",
        json!({ "position": 0, "limit": 100 }),
        json!({ "queryState": "q1", "ids": ["e1", "e2", "e3"] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/query",
        json!({ "position": 3, "limit": 100 }),
        json!({ "queryState": "q1", "ids": [] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/get",
        json!({ "ids": ["e1", "e2", "e3"] }),
        json!({ "state": "s1", "list": [email_json("e1"), email_json("e2"), email_json("e3")] }),
        1,
    )
    .await;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await?;

    assert!(report.initial);
    assert_eq!(report.emails_added, 3);
    assert_eq!(report.mailboxes, 3);
    assert_eq!(store.list_emails(Some("a"), &Default::default())?.len(), 3);
    assert_eq!(
        store.account_state("a", "email_state")?.as_deref(),
        Some("s0")
    );
    assert_eq!(
        store.account_state("a", "mailbox_state")?.as_deref(),
        Some("m1")
    );
    assert_eq!(
        store.account_state("a", "provider")?.as_deref(),
        Some("jmap")
    );

    let e1 = store.email("a", "e1")?.unwrap();

    assert_eq!(e1.mailbox_ids, ["mb-inbox"]);
    assert_eq!(e1.keywords, ["$seen"]);
    assert!(e1.cc.is_empty());
    assert_eq!(e1.from[0].email, "ann@example.com");

    Ok(())
}

/// Mounts an account with one mailbox and no emails, for a full sync.
async fn mount_empty_account(server: &MockServer) {
    reply(
        server,
        "Mailbox/get",
        json!({ "ids": null }),
        json!({ "state": "m1", "list": [mailbox_json("mb-inbox", "Inbox", Some("inbox"))] }),
        1,
    )
    .await;
    reply(
        server,
        "Email/get",
        json!({ "ids": [] }),
        json!({ "state": "s0", "list": [] }),
        1,
    )
    .await;
    reply(
        server,
        "Email/query",
        json!({ "position": 0, "limit": 100 }),
        json!({ "queryState": "q1", "ids": [] }),
        1,
    )
    .await;
}

#[tokio::test]
async fn initial_sync_stores_identities() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    mount_empty_account(&server).await;
    reply(
        &server,
        "Identity/get",
        json!({ "ids": null }),
        json!({ "list": [
            { "id": "id-1", "name": "Me", "email": "me@example.com" },
            { "id": "id-2", "name": "Shop", "email": "shop@example.com" },
        ] }),
        1,
    )
    .await;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await?;
    let stored = store.identities("a")?;

    assert_eq!(report.identities, 2);
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].email, "shop@example.com");

    // A failing Identity/get must not fail the sync.
    let server = self::server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    mount_empty_account(&server).await;
    reply_error(&server, "Identity/get", json!({ "ids": null }), "forbidden").await;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await?;

    assert_eq!(report.identities, 0);
    assert!(store.identities("a")?.is_empty());

    Ok(())
}

#[tokio::test]
async fn masked_emails_need_the_capability() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;

    reply(
        &server,
        "MaskedEmail/get",
        json!({}),
        json!({ "list": [] }),
        0,
    )
    .await;

    assert!(c.masked_emails().await?.is_empty());

    Ok(())
}

#[tokio::test]
async fn sync_stores_masked_emails_with_the_capability() -> Result<()> {
    let server = masked_server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    mount_empty_account(&server).await;
    reply(
        &server,
        "Identity/get",
        json!({ "ids": null }),
        json!({ "list": [
            { "id": "id-1", "name": "Me", "email": "me@example.com" },
            { "id": "id-2", "name": "Shop", "email": "shop@example.com" },
        ] }),
        1,
    )
    .await;
    reply(
        &server,
        "MaskedEmail/get",
        json!({ "accountId": "acc1", "ids": null }),
        json!({ "list": [
            { "id": "m1", "email": "old.owl@example.com", "state": "enabled",
              "description": "Forum", "forDomain": "https://forum.example",
              "lastMessageAt": "2026-09-01T00:00:00Z" },
            { "id": "m2", "email": "gone@example.com", "state": "deleted" },
            { "id": "m3", "email": "new.fox@example.com", "state": "disabled",
              "lastMessageAt": "2026-10-01T00:00:00Z" },
        ] }),
        1,
    )
    .await;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await?;
    let masked = store.masked_emails("a")?;
    let emails: Vec<&str> = masked.iter().map(|m| m.email.as_str()).collect();

    assert_eq!(report.identities, 2);
    assert_eq!(store.identities("a")?.len(), 2);
    assert_eq!(emails, ["new.fox@example.com", "old.owl@example.com"]);
    assert_eq!(report.masked_emails, 2);
    assert_eq!(masked[1].description, "Forum");
    assert_eq!(masked[1].for_domain, "https://forum.example");

    let bodies: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter_map(|r| serde_json::from_slice(&r.body).ok())
        .collect();
    let with_call = |name: &str| {
        bodies
            .iter()
            .find(|b| b["methodCalls"][0][0] == name)
            .unwrap_or_else(|| panic!("no {name} request recorded"))
    };
    let mut masked_using = USING.to_vec();
    masked_using.push(MASKED);

    assert_eq!(with_call("Identity/get")["using"], json!(USING));
    assert_eq!(with_call("MaskedEmail/get")["using"], json!(masked_using));
    assert_eq!(
        with_call("MaskedEmail/get")["methodCalls"][0][1],
        json!({ "accountId": "acc1", "ids": null })
    );

    let carrying = bodies
        .iter()
        .filter(|b| {
            b["using"]
                .as_array()
                .is_some_and(|u| u.iter().any(|c| c == MASKED))
        })
        .count();

    assert_eq!(carrying, 1, "only MaskedEmail/get may name the capability");

    Ok(())
}

#[tokio::test]
async fn masked_email_method_errors_are_redacted() {
    let server = masked_server().await;
    let c = client(&server).await;

    reply_raw(
        &server,
        json!([["error", { "type": "forbidden", "description": "token tok leaked" }, "0"]]),
    )
    .await;

    let err = c.masked_emails().await.unwrap_err();

    assert!(
        matches!(&err, Error::Jmap(m) if m.contains("<redacted>") && !m.contains("tok")),
        "{err}"
    );
}

#[tokio::test]
async fn delta_sync_applies_changes() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    store.set_account_state("a", "account_id", "acc1")?;
    store.set_account_state("a", "mailbox_state", "m1")?;
    store.set_account_state("a", "email_state", "s0")?;
    store.upsert_emails(&[email("e1", "mb-inbox"), email("e3", "mb-inbox")])?;

    reply(
        &server,
        "Mailbox/changes",
        json!({ "sinceState": "m1" }),
        json!({ "oldState": "m1", "newState": "m1", "hasMoreChanges": false,
                "created": [], "updated": [], "destroyed": [] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/changes",
        json!({ "sinceState": "s0" }),
        json!({ "oldState": "s0", "newState": "s1b", "hasMoreChanges": true,
                "created": ["e4"], "updated": [], "destroyed": [] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/changes",
        json!({ "sinceState": "s1b" }),
        json!({ "oldState": "s1b", "newState": "s2", "hasMoreChanges": false,
                "created": [], "updated": ["e1"], "destroyed": ["e3"] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/get",
        json!({ "ids": ["e4"] }),
        json!({ "state": "s1b", "list": [email_json("e4")] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/get",
        json!({ "ids": ["e1"] }),
        json!({ "state": "s2", "list": [email_json("e1")] }),
        1,
    )
    .await;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await?;

    assert!(!report.initial);
    assert_eq!(report.emails_added, 1);
    assert_eq!(report.emails_updated, 1);
    assert_eq!(report.emails_removed, 1);
    assert!(store.email("a", "e3")?.is_none());
    assert!(store.email("a", "e4")?.is_some());
    assert_eq!(
        store.account_state("a", "email_state")?.as_deref(),
        Some("s2")
    );

    Ok(())
}

#[tokio::test]
async fn delta_sync_recovers_from_cannot_calculate() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    store.set_account_state("a", "mailbox_state", "m1")?;
    store.set_account_state("a", "email_state", "expired")?;
    store.upsert_emails(&[email("e-stale", "mb-inbox")])?;
    store.enqueue_for_classification("a", &["e-stale".into()])?;
    store.enqueue_for_classification("b", &["e-stale".into()])?;
    store.put_body(
        "a",
        &posto_core::store::EmailBody {
            email_id: "e-stale".into(),
            html: None,
            text: Some("old".into()),
            attachments: vec![],
            in_reply_to: vec![],
            references: vec![],
            message_id: vec![],
            reply_to: vec![],
        },
    )?;

    reply(
        &server,
        "Mailbox/changes",
        json!({ "sinceState": "m1" }),
        json!({ "oldState": "m1", "newState": "m1", "hasMoreChanges": false,
                "created": [], "updated": [], "destroyed": [] }),
        1,
    )
    .await;
    reply_error(
        &server,
        "Email/changes",
        json!({ "sinceState": "expired" }),
        "cannotCalculateChanges",
    )
    .await;
    reply(
        &server,
        "Email/get",
        json!({ "ids": [] }),
        json!({ "state": "s9", "list": [] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/query",
        json!({ "position": 0 }),
        json!({ "queryState": "q", "ids": ["e1"] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/query",
        json!({ "position": 1 }),
        json!({ "queryState": "q", "ids": [] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/get",
        json!({ "ids": ["e1"] }),
        json!({ "state": "s9", "list": [email_json("e1")] }),
        1,
    )
    .await;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await?;

    assert!(report.initial);
    assert!(store.email("a", "e-stale")?.is_none());
    assert!(store.body("a", "e-stale")?.is_none());
    assert_eq!(store.classify_queue_len("a")?, 0);
    assert_eq!(store.classify_queue_len("b")?, 1);
    assert!(store.email("a", "e1")?.is_some());
    assert_eq!(
        store.account_state("a", "email_state")?.as_deref(),
        Some("s9")
    );

    Ok(())
}

/// Mounts an account whose full sync finds `ids`, all in the inbox.
async fn mount_initial_sync(server: &MockServer, ids: &[&str]) {
    reply(
        server,
        "Mailbox/get",
        json!({ "ids": null }),
        json!({ "state": "m1", "list": [mailbox_json("mb-inbox", "Inbox", Some("inbox"))] }),
        1,
    )
    .await;
    reply(
        server,
        "Email/get",
        json!({ "ids": [] }),
        json!({ "state": "s0", "list": [] }),
        1,
    )
    .await;
    reply(
        server,
        "Email/query",
        json!({ "position": 0, "limit": 100 }),
        json!({ "queryState": "q1", "ids": ids }),
        1,
    )
    .await;
    reply(
        server,
        "Email/query",
        json!({ "position": ids.len(), "limit": 100 }),
        json!({ "queryState": "q1", "ids": [] }),
        1,
    )
    .await;
    reply(
        server,
        "Email/get",
        json!({ "ids": ids }),
        json!({ "state": "s1", "list": ids.iter().map(|id| email_json(id)).collect::<Vec<_>>() }),
        1,
    )
    .await;
}

#[tokio::test]
async fn sync_all_isolates_failures() -> Result<()> {
    let (server_a, server_b) = (server().await, server().await);
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;
    let mut cfg = cfg_a();

    cfg.accounts.push(Account::new("b", "b@example.com"));
    cfg.accounts.push(Account::new("c", "c@example.com"));

    // Account b connects fine, then its first API call is refused.
    Mock::given(method("POST"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server_b)
        .await;
    mount_initial_sync(&server_a, &["e1", "e2"]).await;

    let clients: HashMap<String, Arc<Connection>> = HashMap::from([
        ("a".to_string(), Arc::new(client(&server_a).await)),
        ("b".to_string(), Arc::new(client(&server_b).await)),
    ]);
    let reports = sync_all_with_lock(&dir.path().join("sync.lock"), &cfg, &store, &clients).await?;
    let ids: Vec<&str> = reports.iter().map(|r| r.account_id.as_str()).collect();

    assert_eq!(ids, ["a", "b", "c"], "config order");
    assert_eq!(reports[0].error, None);
    assert_eq!(reports[0].emails_added, 2);
    assert!(
        reports[1]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("auth")),
        "{:?}",
        reports[1].error
    );
    assert_eq!(reports[2].error.as_deref(), Some("no token"));
    assert_eq!(reports[2].emails_added, 0);
    assert_eq!(store.list_emails(Some("a"), &Default::default())?.len(), 2);
    assert!(
        store
            .list_emails(Some("b"), &Default::default())?
            .is_empty()
    );
    assert!(store.mailboxes("b")?.is_empty());
    assert_eq!(store.list_emails(None, &Default::default())?.len(), 2);

    Ok(())
}

#[tokio::test]
async fn sync_reports_inbox_added() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;
    let mut in_archive = email_json("e5");

    in_archive["mailboxIds"] = json!({ "mb-archive": true });

    store.set_account_state("a", "account_id", "acc1")?;
    store.set_account_state("a", "mailbox_state", "m1")?;
    store.set_account_state("a", "email_state", "s0")?;
    store.upsert_mailboxes(&[
        mailbox("mb-inbox", "inbox"),
        mailbox("mb-archive", "archive"),
    ])?;
    // e1 is already in the inbox: refreshing it is not an arrival.
    store.upsert_emails(&[email("e1", "mb-inbox")])?;

    reply(
        &server,
        "Mailbox/changes",
        json!({ "sinceState": "m1" }),
        json!({ "oldState": "m1", "newState": "m1", "hasMoreChanges": false,
                "created": [], "updated": [], "destroyed": [] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/changes",
        json!({ "sinceState": "s0" }),
        json!({ "oldState": "s0", "newState": "s1", "hasMoreChanges": false,
                "created": ["e4", "e5"], "updated": ["e1"], "destroyed": [] }),
        1,
    )
    .await;
    reply(
        &server,
        "Email/get",
        json!({ "ids": ["e4", "e5", "e1"] }),
        json!({ "state": "s1", "list": [email_json("e4"), in_archive, email_json("e1")] }),
        1,
    )
    .await;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await?;

    assert!(!report.initial);
    assert_eq!(report.inbox_added, ["e4"]);
    assert_eq!(store.classify_queue_len("a")?, 1);
    assert_eq!(store.classify_queue_len("b")?, 0);

    // An initial sync reports nothing and queues nothing.
    let server = self::server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    mount_initial_sync(&server, &["e1", "e2"]).await;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await?;

    assert!(report.initial);
    assert_eq!(report.emails_added, 2);
    assert!(report.inbox_added.is_empty());
    assert_eq!(store.classify_queue_len("a")?, 0);

    Ok(())
}

#[tokio::test]
async fn sync_clears_account_whose_server_id_changed() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    // The stored cursors belong to another server account (`other`, not acc1).
    store.set_account_state("a", "account_id", "other")?;
    store.set_account_state("a", "mailbox_state", "m-old")?;
    store.set_account_state("a", "email_state", "s-old")?;
    store.upsert_emails(&[email("e-foreign", "mb-inbox")])?;
    mount_initial_sync(&server, &["e1"]).await;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await;

    assert!(report.as_ref().is_ok_and(|r| r.initial), "{report:?}");
    assert!(store.email("a", "e-foreign")?.is_none());
    assert!(store.email("a", "e1")?.is_some());
    assert_eq!(
        store.account_state("a", "account_id")?.as_deref(),
        Some("acc1")
    );

    Ok(())
}

#[tokio::test]
async fn sync_lock_is_exclusive() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;
    let lock_path = dir.path().join("sync.lock");
    let mut held = fd_lock::RwLock::new(std::fs::File::create(&lock_path)?);
    let _guard = held.try_write()?;

    let err = sync_account(&lock_path, &cfg_a(), "a", &store, &c)
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Locked(_)), "{err:?}");

    Ok(())
}

#[tokio::test]
async fn archive_action_patches_and_mirrors() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    store.upsert_mailboxes(&[
        mailbox("mb-inbox", "inbox"),
        mailbox("mb-archive", "archive"),
    ])?;
    store.upsert_emails(&[email("e1", "mb-inbox")])?;

    Mock::given(method("POST"))
        .and(path("/api"))
        .and(Call {
            name: "Email/set",
            args: json!({}),
        })
        .and(body_string_contains(
            r#""e1":{"mailboxIds":{"mb-archive":true}}"#,
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [["Email/set", { "updated": { "e1": null } }, "0"]],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let n = apply(&store, Some(&c), "a", &ids(&["e1"]), Action::Archive).await?;

    assert_eq!(n, 1);
    assert_eq!(store.email("a", "e1")?.unwrap().mailbox_ids, ["mb-archive"]);

    Ok(())
}

#[tokio::test]
async fn move_mirror_drops_labels_on_jmap() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    store.upsert_mailboxes(&[
        mailbox("inbox", "inbox"),
        mailbox("archive", "archive"),
        Mailbox {
            role: None,
            ..mailbox("promos", "promos")
        },
    ])?;
    store.upsert_emails(&[email_in("e1", &["inbox", "promos"])])?;
    reply(
        &server,
        "Email/set",
        json!({}),
        json!({ "updated": { "e1": null } }),
        1,
    )
    .await;

    let n = apply(&store, Some(&c), "a", &ids(&["e1"]), Action::Archive).await?;

    assert_eq!(n, 1);
    assert_eq!(store.email("a", "e1")?.unwrap().mailbox_ids, ["archive"]);

    Ok(())
}

#[tokio::test]
async fn proton_rules_apply_offline() -> Result<()> {
    let store = Store::open_memory()?;
    let mailbox_ids = |id: &str| store.email("a", id).unwrap().unwrap().mailbox_ids;

    store.set_account_state("a", "provider", "proton")?;
    store.upsert_mailboxes(&[
        mailbox("inbox", "inbox"),
        mailbox("archive", "archive"),
        mailbox("work", "folder"),
        Mailbox {
            role: None,
            ..mailbox("promos", "promos")
        },
    ])?;
    store.upsert_emails(&[
        email_in("e1", &["inbox", "promos"]),
        email_in("e2", &["inbox"]),
        email_in("e3", &["inbox", "promos"]),
        email_in("e4", &["inbox"]),
    ])?;

    let n = apply(&store, None, "a", &ids(&["e1"]), Action::Archive).await?;

    assert_eq!(n, 1);
    assert_eq!(mailbox_ids("e1"), ["archive", "promos"]);

    let err = apply(
        &store,
        None,
        "a",
        &ids(&["e2"]),
        Action::Move {
            mailbox_id: "promos".into(),
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
    assert_eq!(mailbox_ids("e2"), ["inbox"]);

    apply(
        &store,
        None,
        "a",
        &ids(&["e3"]),
        Action::PatchMailboxes {
            add: ids(&["work"]),
            remove: vec![],
        },
    )
    .await?;

    assert_eq!(mailbox_ids("e3"), ["promos", "work"]);

    apply(
        &store,
        None,
        "a",
        &ids(&["e4"]),
        Action::PatchMailboxes {
            add: ids(&["promos"]),
            remove: ids(&["inbox"]),
        },
    )
    .await?;

    assert_eq!(mailbox_ids("e4"), ["promos"]);

    // Permanent delete is allowed on Proton too.
    assert_eq!(
        apply(&store, None, "a", &ids(&["e1"]), Action::Destroy).await?,
        1
    );
    assert!(store.email("a", "e1")?.is_none());

    store.set_account_state("a", "provider", "jmap")?;
    apply(&store, None, "a", &ids(&["e3"]), Action::Archive).await?;

    assert_eq!(mailbox_ids("e3"), ["archive"]);

    Ok(())
}

/// Matches an `Email/set` whose `update` has exactly the keys `keys`.
struct UpdatesExactly(&'static [&'static str]);

impl Match for UpdatesExactly {
    fn matches(&self, request: &Request) -> bool {
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            return false;
        };
        let call = &body["methodCalls"][0];
        let Some(update) = call[1]["update"].as_object() else {
            return false;
        };
        let mut keys: Vec<&str> = update.keys().map(String::as_str).collect();

        keys.sort_unstable();

        call[0] == "Email/set" && keys == self.0
    }
}

/// An email of account `a` in all of `mailboxes`.
fn email_in(id: &str, mailboxes: &[&str]) -> EmailSummary {
    EmailSummary {
        mailbox_ids: ids(mailboxes),
        ..email(id, "")
    }
}

#[tokio::test]
async fn patch_never_empties_mailboxes() -> Result<()> {
    let remove_inbox = || Action::PatchMailboxes {
        add: vec![],
        remove: ids(&["mb-inbox"]),
    };
    let seed = || -> Result<Store> {
        let store = Store::open_memory()?;

        store.upsert_emails(&[
            email_in("e1", &["mb-inbox"]),
            email_in("e2", &["mb-inbox", "mb-promos"]),
        ])?;

        Ok(store)
    };

    // Offline.
    let store = seed()?;
    let n = apply(&store, None, "a", &ids(&["e1", "e2"]), remove_inbox()).await?;

    assert_eq!(n, 1);
    assert_eq!(store.email("a", "e1")?.unwrap().mailbox_ids, ["mb-inbox"]);
    assert_eq!(store.email("a", "e2")?.unwrap().mailbox_ids, ["mb-promos"]);

    // Online: e1 never reaches the server.
    let server = server().await;
    let c = client(&server).await;
    let store = seed()?;

    Mock::given(method("POST"))
        .and(path("/api"))
        .and(UpdatesExactly(&["e2"]))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [["Email/set", { "updated": { "e2": null } }, "0"]],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let n = apply(&store, Some(&c), "a", &ids(&["e1", "e2"]), remove_inbox()).await?;

    assert_eq!(n, 1);
    assert_eq!(store.email("a", "e1")?.unwrap().mailbox_ids, ["mb-inbox"]);
    assert_eq!(store.email("a", "e2")?.unwrap().mailbox_ids, ["mb-promos"]);

    Ok(())
}

#[tokio::test]
async fn patch_rechecks_after_the_server_answers() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    store.upsert_emails(&[email_in("e1", &["mb-inbox", "mb-promos"])])?;

    Mock::given(method("POST"))
        .and(path("/api"))
        .and(Call {
            name: "Email/set",
            args: json!({}),
        })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "methodResponses": [["Email/set", { "updated": { "e1": null } }, "0"]],
                }))
                .set_delay(std::time::Duration::from_millis(300)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let remove_inbox = Action::PatchMailboxes {
        add: vec![],
        remove: ids(&["mb-inbox"]),
    };
    let (n, ()) = tokio::join!(
        async {
            apply(&store, Some(&c), "a", &ids(&["e1"]), remove_inbox)
                .await
                .unwrap()
        },
        async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            store
                .set_mailbox_ids("a", "e1", &ids(&["mb-inbox"]))
                .unwrap();
        },
    );

    assert_eq!(n, 0);
    assert_eq!(store.email("a", "e1")?.unwrap().mailbox_ids, ["mb-inbox"]);

    Ok(())
}

#[tokio::test]
async fn mark_read_offline_mirrors_only() -> Result<()> {
    let store = Store::open_memory()?;

    store.upsert_emails(&[email("e1", "mb-inbox")])?;

    let n = apply(&store, None, "a", &ids(&["e1"]), Action::MarkRead).await?;

    assert_eq!(n, 1);
    assert!(
        store
            .email("a", "e1")?
            .unwrap()
            .keywords
            .contains(&"$seen".to_string())
    );

    Ok(())
}

#[tokio::test]
async fn not_updated_ids_are_skipped() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    store.upsert_emails(&[email("e1", "mb-inbox")])?;

    reply(
        &server,
        "Email/set",
        json!({}),
        json!({ "notUpdated": { "e1": { "type": "notFound" } } }),
        1,
    )
    .await;

    let n = apply(&store, Some(&c), "a", &ids(&["e1"]), Action::MarkRead).await?;

    assert_eq!(n, 0);
    assert!(store.email("a", "e1")?.unwrap().keywords.is_empty());

    Ok(())
}

#[tokio::test]
async fn fetch_body_caches() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    reply(
        &server,
        "Email/get",
        json!({ "ids": ["e1"] }),
        json!({ "state": "s", "list": [{
            "id": "e1",
            "htmlBody": [{ "partId": "h", "type": "text/html" }],
            "textBody": [{ "partId": "t", "type": "text/plain" }],
            "bodyValues": { "h": { "value": "<p>Hi</p>" }, "t": { "value": "Hi" } },
            "attachments": [{ "blobId": "b1", "name": null, "type": "application/pdf", "size": 5 }],
            "inReplyTo": null, "references": ["r1"], "messageId": ["m@x"],
        }] }),
        1,
    )
    .await;

    let first = fetch_body(&store, Some(&c), "a", "e1").await?;
    let second = fetch_body(&store, Some(&c), "a", "e1").await?;

    assert_eq!(first, second);
    assert!(first.html.unwrap().contains("<p>Hi</p>"));
    assert_eq!(first.text.as_deref(), Some("Hi"));
    assert_eq!(first.attachments.len(), 1);
    assert_eq!(first.attachments[0].name, "attachment");
    assert_eq!(first.references, ["r1"]);

    Ok(())
}

#[tokio::test]
async fn body_reads_reply_to() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;

    for (id, extra) in [
        (
            "e1",
            json!({ "replyTo": [
                { "name": "R", "email": "ra@passmail.com" },
                { "name": null, "email": "r2@x.io" },
            ] }),
        ),
        ("e2", json!({ "replyTo": null })),
        ("e3", json!({})),
    ] {
        let mut email = json!({ "id": id, "textBody": [], "htmlBody": [], "bodyValues": {} });

        email
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        reply(
            &server,
            "Email/get",
            json!({ "ids": [id] }),
            json!({ "state": "s", "list": [email] }),
            1,
        )
        .await;
    }

    let reply_to = |id: &'static str| {
        let c = &c;

        async move { c.body(id).await.map(|b| b.unwrap().reply_to) }
    };
    let got = reply_to("e1").await?;

    assert_eq!(
        got.iter()
            .map(|a| (a.name.as_deref(), a.email.as_str()))
            .collect::<Vec<_>>(),
        [(Some("R"), "ra@passmail.com"), (None, "r2@x.io")]
    );
    assert!(reply_to("e2").await?.is_empty());
    assert!(reply_to("e3").await?.is_empty());

    let requests = server.received_requests().await.unwrap();
    let gets: Vec<Value> = requests
        .iter()
        .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
        .filter(|b| b["methodCalls"][0][0] == "Email/get")
        .collect();

    assert_eq!(gets.len(), 3);
    for g in gets {
        assert!(
            g["methodCalls"][0][1]["properties"]
                .as_array()
                .unwrap()
                .contains(&json!("replyTo"))
        );
    }

    Ok(())
}

#[tokio::test]
async fn save_attachment_downloads_and_dedupes() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let dir = tempfile::tempdir()?;
    let att = posto_core::store::Attachment {
        blob_id: "b1".into(),
        name: "report.pdf".into(),
        mime: "application/pdf".into(),
        size: 5,
    };

    Mock::given(method("GET"))
        .and(path("/dl/acc1/b1/report.pdf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF-".to_vec()))
        .mount(&server)
        .await;

    let one = save_attachment(&c, "e1", &att, dir.path()).await?;
    let two = save_attachment(&c, "e1", &att, dir.path()).await?;

    assert_eq!(one, dir.path().join("report.pdf"));
    assert_eq!(two, dir.path().join("report (2).pdf"));
    assert_eq!(std::fs::read(&one)?, b"%PDF-");
    assert_eq!(std::fs::read(&two)?, b"%PDF-");

    Ok(())
}

const SECRET: &str = "review-secret-bearer-123";

async fn secret_client(server: &MockServer) -> Client {
    Client::connect(&format!("{}/session", server.uri()), SECRET)
        .await
        .unwrap()
}

/// Mounts a catch-all `POST /api` reply with these `methodResponses`.
async fn reply_raw(server: &MockServer, method_responses: Value) {
    Mock::given(method("POST"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "methodResponses": method_responses })),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn sync_preserves_attachment_flag() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    reply(
        &server,
        "Mailbox/get",
        json!({"ids": null}),
        json!({"state": "m1", "list": []}),
        1,
    )
    .await;
    reply(
        &server,
        "Email/get",
        json!({"ids": []}),
        json!({"state": "s0", "list": []}),
        1,
    )
    .await;
    reply(
        &server,
        "Email/query",
        json!({"position": 0}),
        json!({"queryState": "q", "ids": ["e1"]}),
        1,
    )
    .await;
    reply(
        &server,
        "Email/query",
        json!({"position": 1}),
        json!({"queryState": "q", "ids": []}),
        1,
    )
    .await;

    // Like a real server, return only the requested properties (plus id).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(Call {
            name: "Email/get",
            args: json!({"ids": ["e1"]}),
        })
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let requested = body["methodCalls"][0][1]["properties"].as_array().unwrap();
            let mut e = email_json("e1");

            e["hasAttachment"] = json!(true);
            e.as_object_mut()
                .unwrap()
                .retain(|k, _| k == "id" || requested.iter().any(|p| p == k));

            ResponseTemplate::new(200).set_body_json(json!({
                "methodResponses": [["Email/get", {"state": "s1", "list": [e]}, "0"]]
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", &store, &c).await?;

    assert!(
        store.email("a", "e1")?.unwrap().has_attachment,
        "a server email with an attachment lost its attachment flag"
    );

    Ok(())
}

#[tokio::test]
async fn query_continues_after_restart_budget() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    reply(
        &server,
        "Mailbox/get",
        json!({"ids": null}),
        json!({"state": "m1", "list": []}),
        1,
    )
    .await;
    reply(
        &server,
        "Email/get",
        json!({"ids": []}),
        json!({"state": "s0", "list": []}),
        1,
    )
    .await;

    // Every response carries a new queryState, so no pass is ever consistent.
    let sequence = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/api"))
        .and(Call {
            name: "Email/query",
            args: json!({}),
        })
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let position = body["methodCalls"][0][1]["position"].as_u64().unwrap();
            let n = sequence.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let page = match position {
                0 => json!(["e1"]),
                1 => json!(["e2"]),
                _ => json!([]),
            };

            ResponseTemplate::new(200).set_body_json(json!({
                "methodResponses": [["Email/query", {
                    "queryState": format!("q{n}"), "ids": page
                }, "0"]]
            }))
        })
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api"))
        .and(Call {
            name: "Email/get",
            args: json!({}),
        })
        .and(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            !body["methodCalls"][0][1]["ids"]
                .as_array()
                .unwrap()
                .is_empty()
        })
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let wanted = body["methodCalls"][0][1]["ids"].as_array().unwrap();
            let emails: Vec<Value> = wanted
                .iter()
                .map(|id| email_json(id.as_str().unwrap()))
                .collect();

            ResponseTemplate::new(200).set_body_json(json!({
                "methodResponses": [["Email/get", {"state": "s0", "list": emails}, "0"]]
            }))
        })
        .mount(&server)
        .await;

    let mut cfg = cfg_a();
    cfg.sync.initial_limit = 2;

    let report = sync_account(&dir.path().join("sync.lock"), &cfg, "a", &store, &c).await?;

    assert_eq!(
        report.emails_added, 2,
        "restart budget stopped paging before the limit or an empty page"
    );
    assert!(store.email("a", "e2")?.is_some());

    Ok(())
}

#[tokio::test]
async fn error_description_redacts_token() {
    let server = server().await;
    let c = secret_client(&server).await;

    reply_raw(
        &server,
        json!([["error", {
            "type": format!("bad-{SECRET}"),
            "description": format!("Rejected Authorization: Bearer {SECRET}"),
        }, "0"]]),
    )
    .await;

    let err = c.call_one("Email/get", json!({})).await.unwrap_err();
    let text = format!("{err} {err:?}");

    assert!(!text.contains(SECRET), "error leaked token: {text}");
    assert!(text.contains("<redacted>"), "{text}");
}

#[tokio::test]
async fn malformed_response_redacts_token() {
    let server = server().await;
    let c = secret_client(&server).await;

    reply_raw(&server, json!(SECRET)).await;

    let err = c.call_one("Email/get", json!({})).await.unwrap_err();
    let text = format!("{err} {err:?}");

    assert!(!text.contains(SECRET), "serde error leaked token: {text}");
    assert!(matches!(err, Error::Jmap(_)), "{text}");
}

#[tokio::test]
async fn malformed_identities_redacts_token() {
    let server = server().await;
    let c = secret_client(&server).await;

    reply_raw(&server, json!([["Identity/get", { "list": SECRET }, "0"]])).await;

    let err = c.identities().await.unwrap_err();
    let text = format!("{err} {err:?}");

    assert!(!text.contains(SECRET), "serde error leaked token: {text}");
    assert!(matches!(err, Error::Jmap(_)), "{text}");
}

#[tokio::test]
async fn malformed_session_redacts_token() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(SECRET)))
        .mount(&server)
        .await;

    let err = Client::connect(&format!("{}/session", server.uri()), SECRET)
        .await
        .unwrap_err();
    let text = format!("{err} {err:?}");

    assert!(!text.contains(SECRET), "serde error leaked token: {text}");
    assert!(matches!(err, Error::Jmap(_)), "{text}");
}

#[tokio::test]
async fn send_error_description_redacts_token() {
    let server = server().await;
    let c = secret_client(&server).await;
    let store = Store::open_memory().unwrap();

    store
        .upsert_mailboxes(&[mailbox("mb-drafts", "drafts"), mailbox("mb-sent", "sent")])
        .unwrap();
    reply(
        &server,
        "Identity/get",
        json!({}),
        json!({"list": [{"id": "i1", "name": "Me", "email": "me@example.com"}]}),
        1,
    )
    .await;
    reply(
        &server,
        "Email/set",
        json!({}),
        json!({"notCreated": {"d": {
            "type": "invalidProperties",
            "description": format!("Authorization: Bearer {SECRET}"),
        }}}),
        1,
    )
    .await;

    let err = send(
        &cfg_a(),
        &store,
        &Connection::Jmap(c),
        "a",
        &Draft::default(),
    )
    .await
    .unwrap_err();
    let text = format!("{err} {err:?}");

    assert!(!text.contains(SECRET), "send error leaked token: {text}");
    assert!(matches!(err, Error::Jmap(_)), "{text}");
}

#[tokio::test]
async fn offline_action_counts_only_mirrored_emails() -> Result<()> {
    let store = Store::open_memory()?;

    store.upsert_emails(&[email("e1", "mb-inbox")])?;

    let count = apply(&store, None, "a", &ids(&["missing"]), Action::MarkRead).await?;
    assert_eq!(count, 0, "nothing was mirrored for a nonexistent email");

    let count = apply(
        &store,
        None,
        "a",
        &ids(&["e1", "missing"]),
        Action::MarkRead,
    )
    .await?;
    assert_eq!(count, 1);

    store.upsert_mailboxes(&[mailbox("mb-archive", "archive")])?;

    let count = apply(&store, None, "a", &ids(&["missing"]), Action::Archive).await?;
    assert_eq!(count, 0, "a mailbox move of an unknown email was counted");

    Ok(())
}

#[tokio::test]
async fn online_action_counts_server_accepted_unknown_email() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    reply(
        &server,
        "Email/set",
        json!({}),
        json!({"updated": {"missing": null}}),
        1,
    )
    .await;

    let count = apply(&store, Some(&c), "a", &ids(&["missing"]), Action::MarkRead).await?;

    assert_eq!(count, 1, "the server applied it, so it is counted");

    Ok(())
}

#[tokio::test]
async fn destroy_removes_locally_and_skips_not_destroyed() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    store.upsert_mailboxes(&[mailbox("mb-trash", "trash")])?;
    store.upsert_emails(&[email("e1", "mb-trash"), email("e2", "mb-trash")])?;
    store.put_body(
        "a",
        &EmailBody {
            email_id: "e1".into(),
            html: None,
            text: Some("hi".into()),
            attachments: vec![],
            in_reply_to: vec![],
            references: vec![],
            message_id: vec![],
            reply_to: vec![],
        },
    )?;

    reply(
        &server,
        "Email/set",
        json!({ "destroy": ["e1", "e2"] }),
        json!({ "destroyed": ["e1"], "notDestroyed": { "e2": { "type": "notFound" } } }),
        1,
    )
    .await;

    let n = apply(&store, Some(&c), "a", &ids(&["e1", "e2"]), Action::Destroy).await?;

    assert_eq!(n, 1);
    assert!(store.email("a", "e1")?.is_none());
    assert!(store.body("a", "e1")?.is_none());
    assert!(store.email("a", "e2")?.is_some());

    Ok(())
}

#[tokio::test]
async fn empty_mailbox_destroys_what_the_store_lists_there() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;

    store.upsert_mailboxes(&[mailbox("mb-trash", "trash"), mailbox("mb-inbox", "inbox")])?;
    store.upsert_emails(&[
        email("e1", "mb-trash"),
        email("e2", "mb-trash"),
        email("e3", "mb-inbox"),
    ])?;
    reply(
        &server,
        "Email/set",
        json!({ "destroy": ["e1", "e2"] }),
        json!({ "destroyed": ["e1", "e2"] }),
        1,
    )
    .await;

    let n = actions::empty_mailbox(&store, Some(&c), "a", "mb-trash").await?;

    assert_eq!(n, 2);
    assert!(store.email("a", "e1")?.is_none());
    assert!(store.email("a", "e2")?.is_none());
    assert!(store.email("a", "e3")?.is_some());

    // An empty mailbox sends nothing (the mock above would 500 on a second call).
    assert_eq!(
        actions::empty_mailbox(&store, Some(&c), "a", "mb-trash").await?,
        0
    );

    Ok(())
}

#[tokio::test]
async fn in_trash_requires_trash_only() -> Result<()> {
    let store = Store::open_memory()?;
    let mut both = email("e2", "mb-trash");

    both.mailbox_ids.push("mb-archive".into());
    store.upsert_emails(&[email("e1", "mb-trash"), both, email("e3", "mb-inbox")])?;

    // No trash-role mailbox yet.
    assert!(!in_trash(&store, "a", "e1")?);

    store.upsert_mailboxes(&[mailbox("mb-trash", "trash")])?;

    assert!(in_trash(&store, "a", "e1")?);
    assert!(!in_trash(&store, "a", "e2")?);
    assert!(!in_trash(&store, "a", "e3")?);
    assert!(!in_trash(&store, "a", "missing")?);

    Ok(())
}
