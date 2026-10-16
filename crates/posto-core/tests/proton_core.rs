//! A Proton `Connection` driven through the provider-neutral core (`sync`,
//! `actions`) against a wiremock fake Proton API. No crypto: the session has
//! empty keys and no operation here decrypts.

use posto_core::actions::{self, Action};
use posto_core::config::{Account, Config, ProtonSecret};
use posto_core::proton::Session;
use posto_core::provider::{Connection, Credentials, Identity, connect_with_credentials};
use posto_core::store::{ListQuery, Store};
use posto_core::sync::sync_account;
use posto_core::{Error, Result};
use proton_mail_api::keys::UnlockedKeys;
use proton_mail_api::session::{Client, PersistedSession, Session as PmSession};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LABELS_1: &str = include_str!("fixtures/proton/labels_1.json");
const LABELS_FOLDERS: &str = include_str!("fixtures/proton/labels_folders.json");
const LABELS_SYSTEM_FULL: &str = include_str!("fixtures/proton/labels_system_full.json");
const SESSION: &str = include_str!("fixtures/session.json");
const ADDRESSES: &str = include_str!("fixtures/proton/addresses.json");

/// The first user label in `labels_1.json`.
const LABEL: &str = "id-11";

/// A config whose only account is `a`.
fn cfg_a() -> Config {
    Config {
        accounts: vec![Account::proton("a", "me@proton.me")],
        ..Config::default()
    }
}

/// A fixed session blob.
fn blob_json() -> Value {
    json!({
        "uid": "uid-1",
        "access_token": "access-1",
        "refresh_token": "refresh-1",
        "password_mode": "Single",
        "scope": "full",
        "key_password": null,
    })
}

/// A crate session pointed at `server`, without retries.
fn inner(server: &MockServer) -> PmSession {
    let client = Client::builder()
        .app_version("posto-test@0")
        .base_url(server.uri())
        .max_retries(0)
        .build()
        .expect("valid config");
    let blob: PersistedSession = serde_json::from_value(blob_json()).expect("blob");

    client.resume(blob).expect("resumes")
}

/// A Proton connection without keys.
fn connection(server: &MockServer) -> Connection {
    let keys = UnlockedKeys {
        user_keys: vec![],
        address_keys: vec![],
        key_password: String::new().into(),
    };

    Connection::Proton(Session::for_tests(inner(server), keys, "user-1", None))
}

/// Mounts a 200 JSON reply for `GET p`.
async fn mount_get(server: &MockServer, p: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// A minimal message as the API lists it.
fn message(id: &str, labels: &[&str], unread: u8) -> Value {
    json!({
        "ID": id, "AddressID": "a1", "ConversationID": "c1", "LabelIDs": labels,
        "Subject": "Hello", "Sender": {"Name": "Ann", "Address": "ann@example.com"},
        "Flags": 1, "Time": 1_700_000_000, "Size": 10, "Unread": unread,
    })
}

/// The messages the initial listing returns.
fn listing() -> Vec<Value> {
    vec![
        message("m0", &["0", "5"], 1),
        message("m1", &["0", LABEL, "5"], 0),
        message("m2", &["6", "5"], 1),
        message("m3", &["f1", "5"], 0),
        message("m4", &["0", LABEL, "5"], 1),
    ]
}

/// Serves the labels and addresses every sync reads.
async fn mount_static(server: &MockServer) {
    for (kind, body) in [
        ("1", LABELS_1),
        ("3", LABELS_FOLDERS),
        ("4", LABELS_SYSTEM_FULL),
    ] {
        Mock::given(method("GET"))
            .and(path("/core/v4/labels"))
            .and(query_param("Type", kind))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    Mock::given(method("GET"))
        .and(path("/core/v4/addresses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ADDRESSES))
        .mount(server)
        .await;
}

/// Serves what an initial sync reads: the latest event id `ev0` and one page.
async fn mount_initial(server: &MockServer) {
    mount_get(
        server,
        "/core/v4/events/latest",
        json!({"Code": 1000, "EventID": "ev0"}),
    )
    .await;

    let messages = listing();

    mount_get(
        server,
        "/mail/v4/messages",
        json!({"Code": 1000, "Total": messages.len(), "Messages": messages, "Stale": 0}),
    )
    .await;
}

/// Serves the events batch after `cursor`.
async fn mount_events(server: &MockServer, cursor: &str, batch: Value) {
    mount_get(server, &format!("/core/v4/events/{cursor}"), batch).await;
}

/// Runs one sync of account `a`.
async fn sync(store: &Store, conn: &Connection) -> Result<posto_core::sync::SyncReport> {
    let dir = tempfile::tempdir()?;

    sync_account(&dir.path().join("sync.lock"), &cfg_a(), "a", store, conn).await
}

fn state(store: &Store, key: &str) -> Option<String> {
    store.account_state("a", key).unwrap()
}

#[tokio::test]
async fn proton_sync_through_the_core() {
    let server = MockServer::start().await;
    let store = Store::open_memory().unwrap();
    let conn = connection(&server);

    mount_static(&server).await;
    mount_initial(&server).await;

    let report = sync(&store, &conn).await.unwrap();

    assert!(report.initial);
    assert_eq!(state(&store, "provider").as_deref(), Some("proton"));
    assert_eq!(state(&store, "email_state").as_deref(), Some("ev0"));
    assert_eq!(state(&store, "account_id").as_deref(), Some("user-1"));

    // The sending addresses of the fixture, through this connection.
    let identity = |id: &str, email: &str| Identity {
        id: id.into(),
        name: "<scrubbed>".into(),
        email: email.into(),
    };

    assert_eq!(report.identities, 2);
    assert_eq!(
        store.identities("a").unwrap(),
        [
            identity("id-4", "user1@example.invalid"),
            identity("id-7", "user2@example.invalid"),
        ]
    );

    for role in ["inbox", "drafts", "sent", "archive", "junk", "trash"] {
        assert!(
            store.mailbox_by_role("a", role).unwrap().is_some(),
            "mailbox {role}"
        );
    }

    // Proton lists no counts: the totals are the cached rows.
    let mailboxes = store.mailboxes("a").unwrap();
    let rows = |id: &str, unread_only: bool| {
        store
            .list_emails(
                Some("a"),
                &ListQuery {
                    mailbox_id: Some(id.to_owned()),
                    limit: 1000,
                    unread_only,
                    ..ListQuery::default()
                },
            )
            .unwrap()
            .len() as i64
    };

    for m in &mailboxes {
        assert_eq!(m.total, rows(&m.id, false), "total of {}", m.id);
        assert_eq!(m.unread, rows(&m.id, true), "unread of {}", m.id);
    }

    let inbox = mailboxes
        .iter()
        .find(|m| m.role.as_deref() == Some("inbox"))
        .unwrap();

    assert_eq!((inbox.total, inbox.unread), (3, 2));

    // Second sync: one created message.
    server.reset().await;
    mount_static(&server).await;
    mount_events(
        &server,
        "ev0",
        json!({
            "Code": 1000, "EventID": "ev1", "More": 0, "Refresh": 0,
            "Messages": [{"ID": "m9", "Action": 1}],
        }),
    )
    .await;
    mount_get(
        &server,
        "/mail/v4/messages/m9",
        json!({"Code": 1000, "Message": message("m9", &["0", "5"], 1)}),
    )
    .await;

    let report = sync(&store, &conn).await.unwrap();

    assert!(!report.initial);
    assert_eq!(report.emails_added, 1);
    assert_eq!(state(&store, "email_state").as_deref(), Some("ev1"));
    assert!(store.email("a", "m9").unwrap().is_some());

    // Third sync: the fetch of m10 fails, so the batch is not applied and the
    // cursor stays where it was.
    server.reset().await;
    mount_static(&server).await;
    mount_events(
        &server,
        "ev1",
        json!({
            "Code": 1000, "EventID": "ev2", "More": 0, "Refresh": 0,
            "Messages": [{"ID": "m10", "Action": 1}],
        }),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/mail/v4/messages/m10"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    assert!(sync(&store, &conn).await.is_err());
    assert_eq!(state(&store, "email_state").as_deref(), Some("ev1"));
    assert!(store.email("a", "m9").unwrap().is_some());
    assert!(store.email("a", "m10").unwrap().is_none());

    // Fourth sync: a refresh clears the emails and fetches them again.
    server.reset().await;
    mount_static(&server).await;
    mount_initial(&server).await;
    mount_events(
        &server,
        "ev1",
        json!({"Code": 1000, "EventID": "ev3", "More": 0, "Refresh": 1}),
    )
    .await;

    let report = sync(&store, &conn).await.unwrap();

    assert!(report.initial);
    assert_eq!(state(&store, "email_state").as_deref(), Some("ev0"));
    assert!(store.email("a", "m9").unwrap().is_none());
    assert!(store.email("a", "m0").unwrap().is_some());
}

/// The `(path, body)` of every `PUT` the server received.
async fn puts(server: &MockServer) -> Vec<(String, Value)> {
    server
        .received_requests()
        .await
        .expect("recording is on")
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .map(|r| {
            (
                r.url.path().to_owned(),
                serde_json::from_slice(&r.body).unwrap(),
            )
        })
        .collect()
}

fn mailbox_ids(store: &Store, id: &str) -> Vec<String> {
    store.email("a", id).unwrap().unwrap().mailbox_ids
}

#[tokio::test]
async fn proton_actions_through_the_core() {
    let server = MockServer::start().await;
    let store = Store::open_memory().unwrap();
    let conn = connection(&server);

    mount_static(&server).await;
    mount_initial(&server).await;
    sync(&store, &conn).await.unwrap();

    for p in ["label", "unlabel", "delete"] {
        Mock::given(method("PUT"))
            .and(path(format!("/mail/v4/messages/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Code": 1000})))
            .mount(&server)
            .await;
    }

    let ids = |id: &str| vec![id.to_owned()];
    let apply = |id: &str, action: Action| {
        let ids = ids(id);
        let (store, conn) = (&store, &conn);

        async move { actions::apply(store, Some(conn), "a", &ids, action).await }
    };

    // Archive keeps the labels.
    assert_eq!(apply("m1", Action::Archive).await.unwrap(), 1);

    let seen = puts(&server).await;

    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "/mail/v4/messages/label");
    assert_eq!(seen[0].1["LabelID"], "6");
    assert_eq!(mailbox_ids(&store, "m1"), ["6", LABEL]);

    // A label is no move target; nothing is sent.
    let moved = apply(
        "m4",
        Action::Move {
            mailbox_id: LABEL.into(),
        },
    )
    .await;

    assert!(matches!(moved, Err(Error::Unsupported(_))), "{moved:?}");
    assert_eq!(puts(&server).await.len(), 1);

    // Adding a folder replaces the folder and keeps the labels.
    let patched = apply(
        "m4",
        Action::PatchMailboxes {
            add: vec!["f1".into()],
            remove: vec![],
        },
    )
    .await;

    assert_eq!(patched.unwrap(), 1);
    assert_eq!(mailbox_ids(&store, "m4"), [LABEL, "f1"]);

    // Permanent delete is one PUT and drops the local row.
    assert_eq!(apply("m4", Action::Destroy).await.unwrap(), 1);

    let seen = puts(&server).await;

    assert_eq!(seen.last().unwrap().0, "/mail/v4/messages/delete");
    assert_eq!(seen.last().unwrap().1["IDs"], json!(["m4"]));
    assert!(store.email("a", "m4").unwrap().is_none());

    // Flag is the starred label.
    assert_eq!(apply("m0", Action::Flag).await.unwrap(), 1);

    let seen = puts(&server).await;

    assert_eq!(seen.last().unwrap().0, "/mail/v4/messages/label");
    assert_eq!(seen.last().unwrap().1["LabelID"], "10");
    assert!(
        store
            .email("a", "m0")
            .unwrap()
            .unwrap()
            .keywords
            .contains(&"$flagged".to_owned())
    );
}

/// A Proton login whose session blob is not JSON, so connecting fails before
/// any request.
fn broken_login() -> ProtonSecret {
    ProtonSecret {
        session: "not json".into(),
        mailbox_password: "mp".into(),
        generation: 1,
    }
}

fn config_with_app_version(version: &str) -> Config {
    let mut cfg = cfg_a();

    cfg.proton.app_version = version.into();

    cfg
}

fn error_of(result: Result<Connection>) -> String {
    result.expect_err("connecting must fail").to_string()
}

#[tokio::test]
async fn connect_dispatches_on_the_account_provider() {
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

    let mut jmap = Account::new("j", "j@x.io");

    jmap.session_url = format!("{}/session", server.uri());

    let proton = Account::proton("p", "p@proton.me");
    let cfg = config_with_app_version("posto-test@0");

    // A JMAP account with a token is a JMAP connection.
    let conn = connect_with_credentials(&cfg, &jmap, Credentials::Token("tok".into()))
        .await
        .unwrap();

    assert!(matches!(conn, Connection::Jmap(_)));

    // Each account refuses the other provider's credentials, without asking
    // the server.
    let before = server.received_requests().await.unwrap().len();
    let wrong_kind =
        connect_with_credentials(&cfg, &jmap, Credentials::Proton(broken_login())).await;

    assert_eq!(error_of(wrong_kind), "not configured: token for account j");

    let wrong_kind =
        connect_with_credentials(&cfg, &proton, Credentials::Token("tok".into())).await;

    assert_eq!(
        error_of(wrong_kind),
        "not configured: proton login for account p"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), before);

    // A Proton account reads the session blob and reports an unreadable one.
    let broken = connect_with_credentials(&cfg, &proton, Credentials::Proton(broken_login())).await;

    assert_eq!(
        error_of(broken),
        "not configured: proton session for this account is unreadable; sign in again"
    );

    // Without an app version (config and environment) it cannot connect. The
    // test cannot clear the environment, so it skips this case if it is set.
    if std::env::var_os("POSTO_PROTON_APP_VERSION").is_none() {
        let unset = config_with_app_version("");
        let missing =
            connect_with_credentials(&unset, &proton, Credentials::Proton(broken_login())).await;

        assert_eq!(error_of(missing), "not configured: proton.app_version");
    }
}
