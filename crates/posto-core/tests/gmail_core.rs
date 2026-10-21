//! A Gmail `Connection` driven through the provider-neutral core (`sync`,
//! `actions`) against a wiremock fake of Google's OAuth and Gmail API
//! endpoints.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use posto_core::actions::{self, Action};
use posto_core::compose::Draft;
use posto_core::config::{Account, Config};
use posto_core::gmail::{Endpoints, GoogleClient, connect_with};
use posto_core::provider::{Connection, Identity};
use posto_core::store::{EmailSummary, Mailbox, Store};
use posto_core::sync::{SyncReport, sync_account};
use posto_core::{Error, Result};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The API root path on the fake server.
const API: &str = "/gmail/v1/users/me";

/// A JSON response with `status`.
fn reply(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

/// Mounts `method p` below the API root, answering `status` with `body`.
async fn mount(server: &MockServer, verb: &str, p: &str, status: u16, body: Value) {
    Mock::given(method(verb))
        .and(path(format!("{API}{p}")))
        .respond_with(reply(status, body))
        .mount(server)
        .await;
}

/// Mounts the token endpoint.
async fn mount_token(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(reply(
            200,
            json!({"access_token": "at-1", "expires_in": 3599}),
        ))
        .mount(server)
        .await;
}

/// A Gmail connection to `server` for the Google account `email`, with a 1 ms
/// back-off.
async fn connection(server: &MockServer, email: &str) -> Connection {
    let uri = server.uri();
    let endpoints = Endpoints {
        auth: format!("{uri}/auth"),
        token: format!("{uri}/token"),
        api: format!("{uri}{API}"),
        retry_base: Duration::from_millis(1),
    };
    let client = GoogleClient {
        client_id: "cid".into(),
        client_secret: "csecret".into(),
    };

    mount_token(server).await;
    mount(
        server,
        "GET",
        "/profile",
        200,
        json!({"emailAddress": email, "historyId": "1"}),
    )
    .await;

    Connection::Gmail(
        connect_with(endpoints, client, "rt-1")
            .await
            .expect("connects"),
    )
}

/// A `format=metadata` reply.
fn metadata(id: &str, labels: &Value) -> Value {
    json!({
        "id": id,
        "threadId": format!("t-{id}"),
        "labelIds": labels,
        "snippet": "hi",
        "internalDate": "1700000000000",
        "sizeEstimate": 10,
        "payload": {"headers": [
            {"name": "From", "value": "Ann <ann@example.org>"},
            {"name": "Subject", "value": "S"},
        ]},
    })
}

/// Replaces everything the fake serves with one account state: the labels,
/// the profile (`email`, `history_id`), the send-as list, the listing of
/// `messages` and the metadata of each.
async fn serve(server: &MockServer, email: &str, history_id: &str, messages: &[(&str, Value)]) {
    let system = |id: &str| json!({"id": id, "name": id, "type": "system"});

    server.reset().await;
    mount_token(server).await;
    mount(
        server,
        "GET",
        "/labels",
        200,
        json!({"labels": [
            system("INBOX"), system("SENT"), system("DRAFT"), system("SPAM"),
            system("TRASH"), system("UNREAD"),
            {"id": "Label_1", "name": "Receipts", "type": "user"},
        ]}),
    )
    .await;
    mount(
        server,
        "GET",
        "/profile",
        200,
        json!({"emailAddress": email, "historyId": history_id}),
    )
    .await;
    mount(
        server,
        "GET",
        "/settings/sendAs",
        200,
        json!({"sendAs": [{
            "sendAsEmail": email, "displayName": "Me", "isPrimary": true,
        }]}),
    )
    .await;

    let listed: Vec<Value> = messages.iter().map(|(id, _)| json!({"id": id})).collect();

    mount(server, "GET", "/messages", 200, json!({"messages": listed})).await;

    for (id, labels) in messages {
        mount(
            server,
            "GET",
            &format!("/messages/{id}"),
            200,
            metadata(id, labels),
        )
        .await;
    }
}

/// The four messages of the main scenario.
fn four() -> Vec<(&'static str, Value)> {
    vec![
        ("m1", json!(["INBOX", "UNREAD"])),
        ("m2", json!(["INBOX", "Label_1"])),
        ("m3", json!(["Label_1", "CATEGORY_UPDATES"])),
        ("m4", json!(["TRASH", "SENT"])),
    ]
}

/// A config whose accounts are the Gmail accounts `ids`.
fn config(ids: &[&str]) -> Config {
    Config {
        accounts: ids
            .iter()
            .map(|id| Account::gmail(id, &format!("{id}@gmail.com")))
            .collect(),
        ..Config::default()
    }
}

/// Runs one sync of `account` under a lock file in `dir`.
async fn sync(
    dir: &Path,
    ids: &[&str],
    account: &str,
    store: &Store,
    conn: &Connection,
) -> Result<SyncReport> {
    sync_account(&dir.join("sync.lock"), &config(ids), account, store, conn).await
}

/// The requests the server saw for `p` below the API root, in order.
async fn requests(server: &MockServer, p: &str) -> Vec<Request> {
    let full = format!("{API}{p}");

    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == full)
        .collect()
}

/// A request's JSON body.
fn json_body(r: &Request) -> Value {
    serde_json::from_slice(&r.body).unwrap()
}

/// The mailbox `id` of `account`.
fn mailbox(store: &Store, account: &str, id: &str) -> Mailbox {
    store
        .mailboxes(account)
        .unwrap()
        .into_iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("mailbox {id}"))
}

/// The mailbox ids of email `id` of `account`.
fn mailbox_ids(store: &Store, account: &str, id: &str) -> Vec<String> {
    store
        .email(account, id)
        .unwrap()
        .unwrap_or_else(|| panic!("email {id}"))
        .mailbox_ids
}

/// A cached email row for the seeds.
fn row(account: &str, id: &str) -> EmailSummary {
    EmailSummary {
        account_id: account.into(),
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
        mailbox_ids: vec!["INBOX".into()],
        keywords: Vec::new(),
    }
}

fn state(store: &Store, account: &str, key: &str) -> Option<String> {
    store.account_state(account, key).unwrap()
}

#[tokio::test]
async fn gmail_syncs_through_the_core() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let store = Store::open_memory().unwrap();
    let conn = connection(&server, "me@gmail.com").await;

    serve(&server, "me@gmail.com", "500", &four()).await;

    let report = sync(dir.path(), &["g"], "g", &store, &conn).await.unwrap();

    assert!(report.initial);
    assert_eq!(state(&store, "g", "provider").as_deref(), Some("gmail"));
    assert_eq!(
        state(&store, "g", "account_id").as_deref(),
        Some("me@gmail.com")
    );
    assert_eq!(state(&store, "g", "email_state").as_deref(), Some("500"));
    assert_eq!(requests(&server, "/messages").await.len(), 1);

    let mut ids: Vec<String> = store
        .mailboxes("g")
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();

    ids.sort();

    assert_eq!(
        ids,
        [
            "ARCHIVE", "DRAFT", "INBOX", "Label_1", "SENT", "SPAM", "TRASH"
        ]
    );
    assert_eq!(mailbox_ids(&store, "g", "m1"), ["INBOX"]);
    assert_eq!(mailbox_ids(&store, "g", "m2"), ["INBOX", "Label_1"]);
    assert_eq!(mailbox_ids(&store, "g", "m3"), ["Label_1", "ARCHIVE"]);
    assert_eq!(mailbox_ids(&store, "g", "m4"), ["TRASH"]);

    let counts = |id: &str| {
        let m = mailbox(&store, "g", id);

        (m.total, m.unread)
    };

    assert_eq!(counts("INBOX"), (2, 1));
    assert_eq!(counts("ARCHIVE"), (1, 0));
    assert_eq!(counts("TRASH"), (1, 0));
    assert_eq!(counts("Label_1"), (2, 0));

    // Second sync: m1 leaves the Inbox, by history.
    let mut after = four();

    after[0].1 = json!(["UNREAD"]);

    serve(&server, "me@gmail.com", "501", &after).await;
    mount(
        &server,
        "GET",
        "/history",
        200,
        json!({
            "history": [{
                "id": "501",
                "labelsRemoved": [{"message": {"id": "m1"}, "labelIds": ["INBOX"]}],
            }],
            "historyId": "501",
        }),
    )
    .await;

    let report = sync(dir.path(), &["g"], "g", &store, &conn).await.unwrap();
    let history = requests(&server, "/history").await;

    assert!(!report.initial);
    assert_eq!(history.len(), 1);
    assert!(
        history[0]
            .url
            .query()
            .unwrap()
            .contains("startHistoryId=500")
    );
    assert!(requests(&server, "/messages").await.is_empty());
    assert_eq!(mailbox_ids(&store, "g", "m1"), ["ARCHIVE"]);
    assert_eq!(counts("INBOX").0, 1);
    assert_eq!(state(&store, "g", "email_state").as_deref(), Some("501"));

    // Third sync: Gmail forgot the cursor, so the cache is rebuilt.
    serve(&server, "me@gmail.com", "700", &after).await;
    mount(&server, "GET", "/history", 404, json!({})).await;

    let report = sync(dir.path(), &["g"], "g", &store, &conn).await.unwrap();

    assert!(report.initial);
    assert_eq!(requests(&server, "/history").await.len(), 1);
    assert_eq!(requests(&server, "/messages").await.len(), 1);
    assert_eq!(state(&store, "g", "email_state").as_deref(), Some("700"));
}

#[tokio::test]
async fn gmail_resync_clears_the_old_cache() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let store = Store::open_memory().unwrap();
    let conn = connection(&server, "me@gmail.com").await;

    serve(&server, "me@gmail.com", "500", &four()).await;
    sync(dir.path(), &["g", "h"], "g", &store, &conn)
        .await
        .unwrap();
    store
        .upsert_emails(&[row("g", "old"), row("h", "keep")])
        .unwrap();

    serve(&server, "me@gmail.com", "700", &four()[..1]).await;
    mount(&server, "GET", "/history", 404, json!({})).await;
    sync(dir.path(), &["g", "h"], "g", &store, &conn)
        .await
        .unwrap();

    let has = |account: &str, id: &str| store.email(account, id).unwrap().is_some();

    assert!(has("g", "m1"));

    for gone in ["m2", "m3", "m4", "old"] {
        assert!(!has("g", gone), "{gone} survived the resync");
    }

    assert!(has("h", "keep"), "another account's row was cleared");
    assert_eq!(state(&store, "g", "email_state").as_deref(), Some("700"));
}

#[tokio::test]
async fn two_gmail_accounts_stay_apart() {
    let dir = tempfile::tempdir().unwrap();
    let (sg, sh) = (MockServer::start().await, MockServer::start().await);
    let store = Store::open_memory().unwrap();
    let (cg, ch) = (
        connection(&sg, "g@gmail.com").await,
        connection(&sh, "h@gmail.com").await,
    );
    let two = || {
        vec![
            ("m1", json!(["INBOX", "UNREAD"])),
            ("m2", json!(["INBOX", "Label_1"])),
        ]
    };

    serve(&sg, "g@gmail.com", "500", &two()).await;
    serve(&sh, "h@gmail.com", "900", &two()).await;
    sync(dir.path(), &["g", "h"], "g", &store, &cg)
        .await
        .unwrap();
    sync(dir.path(), &["g", "h"], "h", &store, &ch)
        .await
        .unwrap();

    for (account, cursor, server_id) in [("g", "500", "g@gmail.com"), ("h", "900", "h@gmail.com")] {
        assert_eq!(
            state(&store, account, "email_state").as_deref(),
            Some(cursor)
        );
        assert_eq!(
            state(&store, account, "account_id").as_deref(),
            Some(server_id)
        );
        assert_eq!(mailbox(&store, account, "INBOX").total, 2);
        assert_eq!(mailbox(&store, account, "INBOX").unread, 1);
        assert_eq!(mailbox(&store, account, "Label_1").total, 1);
        assert_eq!(mailbox_ids(&store, account, "m1"), ["INBOX"]);
    }

    for server in [&sg, &sh] {
        mount(server, "POST", "/messages/batchModify", 200, json!({})).await;
    }

    let applied = actions::apply(&store, Some(&ch), "h", &["m1".into()], Action::Archive)
        .await
        .unwrap();

    assert_eq!(applied, 1);
    assert_eq!(requests(&sh, "/messages/batchModify").await.len(), 1);
    assert!(requests(&sg, "/messages/batchModify").await.is_empty());
    assert_eq!(mailbox_ids(&store, "h", "m1"), ["ARCHIVE"]);
    assert_eq!(mailbox_ids(&store, "g", "m1"), ["INBOX"]);
}

#[tokio::test]
async fn gmail_actions_follow_the_folder_rules() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let store = Store::open_memory().unwrap();
    let conn = connection(&server, "me@gmail.com").await;

    serve(&server, "me@gmail.com", "500", &four()).await;
    sync(dir.path(), &["g"], "g", &store, &conn).await.unwrap();
    mount(&server, "POST", "/messages/batchModify", 200, json!({})).await;
    mount(&server, "POST", "/messages/batchDelete", 200, json!({})).await;

    let modify = || requests(&server, "/messages/batchModify");
    let delete = || requests(&server, "/messages/batchDelete");
    let one = |id: &str| vec![id.to_string()];

    // Archive keeps the label and removes the folders.
    let applied = actions::apply(&store, Some(&conn), "g", &one("m2"), Action::Archive)
        .await
        .unwrap();

    assert_eq!(applied, 1);
    assert_eq!(
        json_body(&modify().await[0]),
        json!({
            "ids": ["m2"],
            "addLabelIds": [],
            "removeLabelIds": ["INBOX", "SPAM", "TRASH"],
        })
    );
    assert_eq!(mailbox_ids(&store, "g", "m2"), ["ARCHIVE", "Label_1"]);

    // A move to a label is refused before any request.
    let moved = actions::apply(
        &store,
        Some(&conn),
        "g",
        &one("m1"),
        Action::Move {
            mailbox_id: "Label_1".into(),
        },
    )
    .await;

    assert!(matches!(moved, Err(Error::Unsupported(_))), "{moved:?}");
    assert_eq!(modify().await.len(), 1);
    assert_eq!(mailbox_ids(&store, "g", "m1"), ["INBOX"]);

    // Emptying Trash destroys what the store lists there.
    let gone = actions::empty_mailbox(&store, Some(&conn), "g", "TRASH")
        .await
        .unwrap();

    assert_eq!(gone, 1);
    assert_eq!(json_body(&delete().await[0]), json!({"ids": ["m4"]}));
    assert!(store.email("g", "m4").unwrap().is_none());

    let applied = actions::apply(&store, Some(&conn), "g", &one("m3"), Action::Destroy)
        .await
        .unwrap();

    assert_eq!(applied, 1);
    assert_eq!(delete().await.len(), 2);
    assert_eq!(json_body(&delete().await[1]), json!({"ids": ["m3"]}));
    assert!(store.email("g", "m3").unwrap().is_none());

    // Marking read removes UNREAD and sets $seen locally.
    let applied = actions::apply(&store, Some(&conn), "g", &one("m1"), Action::MarkRead)
        .await
        .unwrap();

    assert_eq!(applied, 1);
    assert_eq!(modify().await.len(), 2);
    assert_eq!(
        json_body(&modify().await[1])["removeLabelIds"],
        json!(["UNREAD"])
    );
    assert!(
        store
            .email("g", "m1")
            .unwrap()
            .unwrap()
            .keywords
            .contains(&"$seen".to_string())
    );
}

/// Fails to compile unless the future is `Send`.
fn assert_send<T: Send>(_: &T) {}

/// The sync futures must be `Send` for every lifetime, or a caller that
/// spawns them (the GUI's sync pass) does not compile. The evidence is the
/// compile error, so nothing here is awaited except the spawned `initial`.
#[tokio::test]
async fn connection_futures_are_send() {
    let server = MockServer::start().await;
    let store = Store::open_memory().unwrap();
    let conn = Arc::new(connection(&server, "me@gmail.com").await);
    let identity = Identity {
        id: "me@gmail.com".into(),
        name: "Me".into(),
        email: "me@gmail.com".into(),
    };
    let draft = Draft {
        account_id: "g".into(),
        ..Draft::default()
    };

    assert_send(&conn.initial("g", 1));
    assert_send(&conn.delta("g", "1"));
    assert_send(&conn.sync_mailboxes(&store, "g"));
    assert_send(&conn.body("m1"));
    assert_send(&conn.send(&store, "g", &identity, &draft, "text", "html"));

    // As the GUI spawns its sync pass: the task owns the connection.
    serve(&server, "me@gmail.com", "500", &four()).await;

    let owned = Arc::clone(&conn);
    let (emails, cursor) = tokio::spawn(async move { owned.initial("g", 1).await })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(emails.len(), 1);
    assert_eq!(cursor, "500");
}
