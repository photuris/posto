//! Integration tests against a wiremock fake JMAP server.

use posto_core::actions::{Action, apply};
use posto_core::body::{fetch_body, save_attachment};
use posto_core::compose::{Draft, send};
use posto_core::config::Config;
use posto_core::jmap::Client;
use posto_core::store::{EmailSummary, Mailbox, Store};
use posto_core::sync::sync_with_lock;
use posto_core::{Error, Result};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const SESSION: &str = include_str!("fixtures/session.json");

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
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/session"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(SESSION.replace("{{base}}", &server.uri()))
                .insert_header("content-type", "application/json"),
        )
        .mount(&server)
        .await;

    server
}

async fn client(server: &MockServer) -> Client {
    Client::connect(&format!("{}/session", server.uri()), "tok")
        .await
        .unwrap()
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

fn email(id: &str, mailbox: &str) -> EmailSummary {
    EmailSummary {
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
    let c = client(&server).await;

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
    let c = client(&server).await;

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

    let report = sync_with_lock(
        &dir.path().join("sync.lock"),
        &Config::default(),
        &store,
        &c,
    )
    .await?;

    assert!(report.initial);
    assert_eq!(report.emails_added, 3);
    assert_eq!(report.mailboxes, 3);
    assert_eq!(store.list_emails(&Default::default())?.len(), 3);
    assert_eq!(store.state("email_state")?.as_deref(), Some("s0"));
    assert_eq!(store.state("mailbox_state")?.as_deref(), Some("m1"));

    let e1 = store.email("e1")?.unwrap();

    assert_eq!(e1.mailbox_ids, ["mb-inbox"]);
    assert_eq!(e1.keywords, ["$seen"]);
    assert!(e1.cc.is_empty());
    assert_eq!(e1.from[0].email, "ann@example.com");

    Ok(())
}

#[tokio::test]
async fn delta_sync_applies_changes() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    store.set_state("account_id", "acc1")?;
    store.set_state("mailbox_state", "m1")?;
    store.set_state("email_state", "s0")?;
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

    let report = sync_with_lock(
        &dir.path().join("sync.lock"),
        &Config::default(),
        &store,
        &c,
    )
    .await?;

    assert!(!report.initial);
    assert_eq!(report.emails_added, 1);
    assert_eq!(report.emails_updated, 1);
    assert_eq!(report.emails_removed, 1);
    assert!(store.email("e3")?.is_none());
    assert!(store.email("e4")?.is_some());
    assert_eq!(store.state("email_state")?.as_deref(), Some("s2"));

    Ok(())
}

#[tokio::test]
async fn delta_sync_recovers_from_cannot_calculate() -> Result<()> {
    let server = server().await;
    let c = client(&server).await;
    let store = Store::open_memory()?;
    let dir = tempfile::tempdir()?;

    store.set_state("mailbox_state", "m1")?;
    store.set_state("email_state", "expired")?;
    store.upsert_emails(&[email("e-stale", "mb-inbox")])?;
    store.put_body(&posto_core::store::EmailBody {
        email_id: "e-stale".into(),
        html: None,
        text: Some("old".into()),
        attachments: vec![],
        in_reply_to: vec![],
        references: vec![],
        message_id: vec![],
    })?;

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

    let report = sync_with_lock(
        &dir.path().join("sync.lock"),
        &Config::default(),
        &store,
        &c,
    )
    .await?;

    assert!(report.initial);
    assert!(store.email("e-stale")?.is_none());
    assert!(store.body("e-stale")?.is_none());
    assert!(store.email("e1")?.is_some());
    assert_eq!(store.state("email_state")?.as_deref(), Some("s9"));

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

    let err = sync_with_lock(&lock_path, &Config::default(), &store, &c)
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

    let n = apply(&store, Some(&c), &ids(&["e1"]), Action::Archive).await?;

    assert_eq!(n, 1);
    assert_eq!(store.email("e1")?.unwrap().mailbox_ids, ["mb-archive"]);

    Ok(())
}

#[tokio::test]
async fn mark_read_offline_mirrors_only() -> Result<()> {
    let store = Store::open_memory()?;

    store.upsert_emails(&[email("e1", "mb-inbox")])?;

    let n = apply(&store, None, &ids(&["e1"]), Action::MarkRead).await?;

    assert_eq!(n, 1);
    assert!(
        store
            .email("e1")?
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

    let n = apply(&store, Some(&c), &ids(&["e1"]), Action::MarkRead).await?;

    assert_eq!(n, 0);
    assert!(store.email("e1")?.unwrap().keywords.is_empty());

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

    let first = fetch_body(&store, Some(&c), "e1").await?;
    let second = fetch_body(&store, Some(&c), "e1").await?;

    assert_eq!(first, second);
    assert!(first.html.unwrap().contains("<p>Hi</p>"));
    assert_eq!(first.text.as_deref(), Some("Hi"));
    assert_eq!(first.attachments.len(), 1);
    assert_eq!(first.attachments[0].name, "attachment");
    assert_eq!(first.references, ["r1"]);

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

    let one = save_attachment(&c, &att, dir.path()).await?;
    let two = save_attachment(&c, &att, dir.path()).await?;

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

    sync_with_lock(
        &dir.path().join("sync.lock"),
        &Config::default(),
        &store,
        &c,
    )
    .await?;

    assert!(
        store.email("e1")?.unwrap().has_attachment,
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

    let mut cfg = Config::default();
    cfg.sync.initial_limit = 2;

    let report = sync_with_lock(&dir.path().join("sync.lock"), &cfg, &store, &c).await?;

    assert_eq!(
        report.emails_added, 2,
        "restart budget stopped paging before the limit or an empty page"
    );
    assert!(store.email("e2")?.is_some());

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
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "apiUrl": [SECRET], "downloadUrl": "", "uploadUrl": "",
            "primaryAccounts": { "urn:ietf:params:jmap:mail": "acc1" },
        })))
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

    let err = send(&Config::default(), &store, &c, &Draft::default())
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

    let count = apply(&store, None, &ids(&["missing"]), Action::MarkRead).await?;
    assert_eq!(count, 0, "nothing was mirrored for a nonexistent email");

    let count = apply(&store, None, &ids(&["e1", "missing"]), Action::MarkRead).await?;
    assert_eq!(count, 1);

    store.upsert_mailboxes(&[mailbox("mb-archive", "archive")])?;

    let count = apply(&store, None, &ids(&["missing"]), Action::Archive).await?;
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

    let count = apply(&store, Some(&c), &ids(&["missing"]), Action::MarkRead).await?;

    assert_eq!(count, 1, "the server applied it, so it is counted");

    Ok(())
}
