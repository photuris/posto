//! `posto_core::proton` against a wiremock fake Proton API, with generated PGP
//! keys where decryption matters.

use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use pgp::composed::{
    ArmorOptions, EncryptionCaps, KeyType, MessageBuilder, SecretKeyParamsBuilder, SignedPublicKey,
    SignedSecretKey, SubkeyParamsBuilder,
};
use pgp::crypto::ecc_curve::ECCCurve;
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::types::{CompressionAlgorithm, Password};
use posto_core::compose::Draft;
use posto_core::proton::{Persist, Session, connect_with};
use posto_core::provider::{Change, Delta, Identity};
use posto_core::store::{Address, Attachment, Mailbox, Store};
use posto_core::{Error, Result};
use proton_mail_api::keys::UnlockedKeys;
use proton_mail_api::session::{Client, PersistedSession, Session as PmSession};
use rand::thread_rng;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MAILBOX: &str = "correct horse mailbox";
const LABELS_1: &str = include_str!("fixtures/proton/labels_1.json");
const LABELS_4: &str = include_str!("fixtures/proton/labels_4.json");
const LABELS_FOLDERS: &str = include_str!("fixtures/proton/labels_folders.json");
const LABELS_SYSTEM_FULL: &str = include_str!("fixtures/proton/labels_system_full.json");
const ADDRESSES: &str = include_str!("fixtures/proton/addresses.json");

// `TestKey` and `unlock` are adapted from proton-mail-api's tests/messages.rs (MIT).

/// A generated key, kept with the password that locks it.
struct TestKey {
    secret: SignedSecretKey,
    public: SignedPublicKey,
}

impl TestKey {
    /// Ed25519 primary plus a Curve25519 ECDH encryption subkey, both locked
    /// with `MAILBOX`.
    fn new() -> Self {
        let mut subkey = SubkeyParamsBuilder::default();

        subkey
            .key_type(KeyType::ECDH(ECCCurve::Curve25519Legacy))
            .can_sign(false)
            .can_encrypt(EncryptionCaps::All)
            .can_authenticate(false)
            .passphrase(Some(MAILBOX.to_owned()));

        let mut params = SecretKeyParamsBuilder::default();

        params
            .key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(true)
            .can_encrypt(EncryptionCaps::None)
            .primary_user_id("Test <test@example.com>".into())
            .passphrase(Some(MAILBOX.to_owned()))
            .subkeys(vec![subkey.build().expect("subkey params")]);

        let secret = params
            .build()
            .expect("key params")
            .generate(thread_rng())
            .expect("key generates");

        Self {
            public: secret.to_public_key(),
            secret,
        }
    }

    fn armored(&self) -> String {
        self.secret
            .to_armored_string(ArmorOptions::default())
            .expect("key armors")
    }

    /// Encrypts `plain` to the encryption subkey the way Proton stores a
    /// body: compressed, signed, armored.
    fn encrypt_body(&self, plain: &[u8]) -> String {
        let mut builder = MessageBuilder::from_bytes("", plain.to_vec())
            .seipd_v1(thread_rng(), SymmetricKeyAlgorithm::AES256);

        builder
            .compression(CompressionAlgorithm::ZLIB)
            .sign(
                &self.secret.primary_key,
                Password::from(MAILBOX),
                HashAlgorithm::Sha256,
            )
            .encrypt_to_key(thread_rng(), &self.public.public_subkeys[0])
            .expect("encrypts to subkey");
        builder
            .to_armored_string(thread_rng(), ArmorOptions::default())
            .expect("message armors")
    }

    /// Encrypts `plain` to the encryption subkey and splits the result the way
    /// Proton serves an attachment: the key packet, then the data packet.
    fn encrypt_attachment(&self, plain: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut builder = MessageBuilder::from_bytes("", plain.to_vec())
            .seipd_v1(thread_rng(), SymmetricKeyAlgorithm::AES256);

        builder
            .encrypt_to_key(thread_rng(), &self.public.public_subkeys[0])
            .expect("encrypts to subkey");

        let bytes = builder.to_vec(thread_rng()).expect("message serialises");

        // A new-format public-key encrypted session key packet (tag 1) with a
        // one-octet length comes first.
        assert!(bytes[0] == 0xC1 && bytes[1] < 192, "key packet header");

        let split = 2 + usize::from(bytes[1]);

        (bytes[..split].to_vec(), bytes[split..].to_vec())
    }
}

/// A fixed session blob; `access` is the access token.
fn blob_json(access: &str) -> Value {
    json!({
        "uid": "uid-1",
        "access_token": access,
        "refresh_token": "refresh-1",
        "password_mode": "Single",
        "scope": "full",
        "key_password": null,
    })
}

/// A crate client pointed at `server`, without retries.
fn client(server: &MockServer) -> Client {
    Client::builder()
        .app_version("posto-test@0")
        .base_url(server.uri())
        .max_retries(0)
        .build()
        .expect("valid config")
}

/// A resumed crate session pointed at `server`.
fn inner(server: &MockServer) -> PmSession {
    let blob: PersistedSession = serde_json::from_value(blob_json("access-1")).expect("blob");

    client(server).resume(blob).expect("resumes")
}

/// Keys that decrypt nothing, for operations that never decrypt.
fn empty_keys() -> UnlockedKeys {
    UnlockedKeys {
        user_keys: vec![],
        address_keys: vec![],
        key_password: String::new().into(),
    }
}

/// A provider session without keys.
fn plain(server: &MockServer, persist: Option<Persist>) -> Session {
    Session::for_tests(inner(server), empty_keys(), "user-1", persist)
}

/// The three key replies that let `unlock_keys` succeed for `address_key`.
fn key_replies(address_key: &TestKey) -> [(&'static str, Value); 3] {
    let user_key = TestKey::new();

    [
        (
            "/core/v4/users",
            json!({"Code": 1000, "User": {
                "ID": "user-1", "Name": "test", "Email": "test@example.com",
                "Keys": [{
                    "ID": "k1", "PrivateKey": user_key.armored(),
                    "Primary": 1, "Active": 1, "Flags": 3,
                }],
            }}),
        ),
        // No salt: the key password is the mailbox password itself.
        ("/core/v4/keys/salts", json!({"Code": 1000, "KeySalts": []})),
        (
            "/core/v4/addresses",
            json!({"Code": 1000, "Addresses": [{
                "ID": "a1", "Email": "a@example.com", "Send": 1, "Receive": 1,
                "Status": 1, "Type": 1, "Order": 1, "DisplayName": "Test",
                "Keys": [{
                    "ID": "ak1", "PrivateKey": address_key.armored(),
                    "Primary": 1, "Active": 1, "Flags": 3,
                }],
            }]}),
        ),
    ]
}

/// Mounts a 200 JSON reply for `GET p`.
async fn mount_get(server: &MockServer, p: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// Serves one user key and one address key for `address_key`, then unlocks
/// them through the session.
async fn unlock(server: &MockServer, session: &PmSession, address_key: &TestKey) -> UnlockedKeys {
    for (p, body) in key_replies(address_key) {
        mount_get(server, p, body).await;
    }

    let result = session.unlock_keys(MAILBOX).await;

    assert!(result.is_ok(), "unlock_keys must succeed");

    result.unwrap_or_else(|_| unreachable!())
}

/// Serves `GET /core/v4/labels` for each `Type` with its fixture.
async fn mount_labels(server: &MockServer, one: &str, folders: &str, system: &str) {
    for (kind, body) in [("1", one), ("3", folders), ("4", system)] {
        Mock::given(method("GET"))
            .and(path("/core/v4/labels"))
            .and(query_param("Type", kind))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.to_owned()))
            .mount(server)
            .await;
    }
}

/// A `{Code: 1000, Labels: []}` body.
fn no_labels() -> String {
    json!({"Code": 1000, "Labels": []}).to_string()
}

/// A minimal message as the API lists it.
fn message_json(id: &str) -> Value {
    json!({
        "ID": id, "AddressID": "a1", "ConversationID": "c1", "LabelIDs": ["0", "5"],
        "Subject": "Hello", "Sender": {"Name": "Ann", "Address": "ann@example.com"},
        "Flags": 1, "Time": 1_700_000_000, "Size": 10, "Unread": 1,
    })
}

/// Serves `GET /mail/v4/messages/<id>` with `fields` merged over a minimal
/// message.
async fn mount_message(server: &MockServer, id: &str, fields: Value) {
    let mut message = message_json(id);

    for (k, v) in fields.as_object().expect("fields are an object") {
        message[k] = v.clone();
    }

    mount_get(
        server,
        &format!("/mail/v4/messages/{id}"),
        json!({"Code": 1000, "Message": message}),
    )
    .await;
}

/// The `METHOD path` of every request the server saw, in order.
async fn lines(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("recording is on")
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

/// How many requests the server has seen.
async fn count(server: &MockServer) -> usize {
    lines(server).await.len()
}

/// A page of `n` listed messages.
fn page_body(n: usize, stale: bool) -> Value {
    let messages: Vec<Value> = (0..n).map(|i| message_json(&format!("m{i}"))).collect();

    json!({"Code": 1000, "Total": n, "Messages": messages, "Stale": u8::from(stale)})
}

/// Mounts the list endpoint for `page`, with the parameters the provider must send.
fn list_mock(page: u32, body: Value) -> Mock {
    Mock::given(method("GET"))
        .and(path("/mail/v4/messages"))
        .and(query_param("LabelID", "5"))
        .and(query_param("PageSize", "150"))
        .and(query_param("Desc", "1"))
        .and(query_param("Page", page.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
}

/// A mailbox row with the given counts.
fn mailbox(account: &str, id: &str, total: i64, unread: i64) -> Mailbox {
    Mailbox {
        account_id: account.into(),
        id: id.into(),
        name: format!("name-{id}"),
        role: None,
        parent_id: None,
        sort_order: 9,
        total,
        unread,
    }
}

#[tokio::test]
async fn labels_become_mailboxes() {
    let server = MockServer::start().await;

    mount_labels(&server, LABELS_1, LABELS_FOLDERS, LABELS_4).await;

    let store = Store::open_memory().unwrap();

    store
        .upsert_mailboxes(&[
            mailbox("a", "old", 1, 1),
            mailbox("b", "b1", 3, 3),
            mailbox("a", "0", 7, 2),
        ])
        .unwrap();

    let session = plain(&server, None);

    session.sync_mailboxes(&store, "a").await.unwrap();

    let rows = store.mailboxes("a").unwrap();
    let find = |id: &str| rows.iter().find(|m| m.id == id);

    for (i, role) in ["inbox", "drafts", "sent", "archive", "junk", "trash"]
        .into_iter()
        .enumerate()
    {
        let m = rows
            .iter()
            .find(|m| m.role.as_deref() == Some(role))
            .unwrap();

        assert_eq!(m.sort_order, i as i64, "sort order of {role}");
    }

    assert_eq!(find("3").unwrap().name, "Trash");
    assert_eq!(find("8").unwrap().name, "Drafts");

    for hidden in ["1", "2", "5", "10", "15"] {
        assert!(find(hidden).is_none(), "{hidden} must not be stored");
    }

    assert_eq!(find("f1").unwrap().role.as_deref(), Some("folder"));
    assert_eq!(find("f2").unwrap().parent_id.as_deref(), Some("f1"));

    for id in ["id-11", "id-12", "id-13"] {
        let label = find(id).unwrap();

        assert!(label.role.is_none() && label.parent_id.is_none());
    }

    assert!(find("old").is_none());
    assert_eq!(store.mailboxes("b").unwrap().len(), 1);

    let inbox = find("0").unwrap();

    assert_eq!((inbox.total, inbox.unread), (7, 2));

    let before = rows.len();

    session.sync_mailboxes(&store, "a").await.unwrap();

    assert_eq!(store.mailboxes("a").unwrap().len(), before);

    // Names the server supplies win over the defaults.
    let server = MockServer::start().await;
    let nothing = no_labels();

    mount_labels(&server, &nothing, &nothing, LABELS_SYSTEM_FULL).await;

    let store = Store::open_memory().unwrap();

    plain(&server, None)
        .sync_mailboxes(&store, "a")
        .await
        .unwrap();

    let rows = store.mailboxes("a").unwrap();
    let name = |role: &str| {
        rows.iter()
            .find(|m| m.role.as_deref() == Some(role))
            .unwrap()
            .name
            .clone()
    };

    assert_eq!(rows.len(), 6);
    assert_eq!(name("junk"), "Spam");
    assert_eq!(name("inbox"), "Inbox");
    assert!(
        rows.iter()
            .all(|m| !["1", "2", "5", "10"].contains(&m.id.as_str()))
    );
}

#[tokio::test]
async fn initial_captures_the_event_before_paging_and_retries_stale() {
    let server = MockServer::start().await;

    mount_get(
        &server,
        "/core/v4/events/latest",
        json!({"Code": 1000, "EventID": "ev0"}),
    )
    .await;
    list_mock(0, page_body(150, false)).mount(&server).await;
    list_mock(1, page_body(3, false)).mount(&server).await;

    let (emails, cursor) = plain(&server, None).initial("a", 1000).await.unwrap();

    assert_eq!((emails.len(), cursor.as_str()), (153, "ev0"));

    let seen = lines(&server).await;
    let latest = seen
        .iter()
        .position(|l| l.ends_with("/events/latest"))
        .unwrap();
    let first_page = seen
        .iter()
        .position(|l| l.ends_with("/mail/v4/messages"))
        .unwrap();

    assert!(latest < first_page, "the cursor is captured before paging");

    // A small limit stops after one page.
    let server = MockServer::start().await;

    mount_get(
        &server,
        "/core/v4/events/latest",
        json!({"Code": 1000, "EventID": "ev0"}),
    )
    .await;
    list_mock(0, page_body(150, false))
        .expect(1)
        .mount(&server)
        .await;

    let (emails, _) = plain(&server, None).initial("a", 100).await.unwrap();

    assert_eq!(emails.len(), 100);

    // Two stale answers, then a full page.
    let server = MockServer::start().await;

    mount_get(
        &server,
        "/core/v4/events/latest",
        json!({"Code": 1000, "EventID": "ev0"}),
    )
    .await;
    list_mock(0, page_body(0, true))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    list_mock(0, page_body(150, false)).mount(&server).await;

    let (emails, _) = plain(&server, None).initial("a", 150).await.unwrap();
    let page_zero = lines(&server)
        .await
        .iter()
        .filter(|l| l.ends_with("/mail/v4/messages"))
        .count();

    assert_eq!((emails.len(), page_zero), (150, 3));

    // Always stale: one request and three retries, then an error.
    let server = MockServer::start().await;

    mount_get(
        &server,
        "/core/v4/events/latest",
        json!({"Code": 1000, "EventID": "ev0"}),
    )
    .await;
    list_mock(0, page_body(0, true)).mount(&server).await;

    let result = plain(&server, None).initial("a", 150).await;
    let page_zero = lines(&server)
        .await
        .iter()
        .filter(|l| l.ends_with("/mail/v4/messages"))
        .count();

    assert!(matches!(result, Err(Error::Proton(_))));
    assert_eq!(page_zero, 4);
}

/// Serves the event `ev0` as `event`, and messages `m1` and `m3`; `m4` must
/// never be fetched.
async fn delta_server(event: Value) -> MockServer {
    let server = MockServer::start().await;

    mount_get(&server, "/core/v4/events/ev0", event).await;
    mount_message(&server, "m1", json!({})).await;
    mount_message(&server, "m3", json!({"Unread": 0, "Subject": "Full m3"})).await;

    Mock::given(method("GET"))
        .and(path("/mail/v4/messages/m4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000, "Message": message_json("m4"),
        })))
        .expect(0)
        .mount(&server)
        .await;

    server
}

/// The event of the first delta case, with `labels` and `refresh` filled in.
fn event_body(labels: Value, refresh: u8) -> Value {
    json!({
        "Code": 1000, "EventID": "ev1", "More": 0, "Refresh": refresh,
        "Messages": [
            {"ID": "m1", "Action": 1},
            {"ID": "m2", "Action": 0},
            {"ID": "m3", "Action": 3, "Message": {"ID": "m3", "Unread": 0}},
            {"ID": "m4", "Action": 1},
            {"ID": "m4", "Action": 0},
        ],
        "Labels": labels,
    })
}

#[tokio::test]
async fn delta_fetches_changed_and_drops_deleted() {
    let server = delta_server(event_body(json!([]), 0)).await;
    let Delta::Page(page) = plain(&server, None).delta("a", "ev0").await.unwrap() else {
        panic!("expected a page");
    };
    let ids: Vec<&str> = page.emails.iter().map(|e| e.id.as_str()).collect();

    assert_eq!(ids, ["m1", "m3"]);
    assert_eq!(page.destroyed, ["m2", "m4"]);
    assert_eq!(page.state, "ev1");
    assert!(!page.more && !page.mailboxes_changed);
    assert!(!page.emails[1].keywords.is_empty(), "m3 is read");
    // The inline payload of the event has no subject or conversation: these
    // come only from fetching the whole message.
    assert_eq!(page.emails[1].subject, "Full m3");
    assert_eq!(page.emails[1].thread_id, "c1");

    let labels = json!([{"ID": "l1", "Action": 2, "Label": {"ID": "l1", "Name": "x", "Type": 1}}]);
    let server = delta_server(event_body(labels, 0)).await;
    let Delta::Page(page) = plain(&server, None).delta("a", "ev0").await.unwrap() else {
        panic!("expected a page");
    };

    assert!(page.mailboxes_changed);

    let server = delta_server(event_body(json!([]), 1)).await;

    assert_eq!(
        plain(&server, None).delta("a", "ev0").await.unwrap(),
        Delta::Resync
    );

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/core/v4/events/gone"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({"Code": 2501, "Error": "x"})))
        .mount(&server)
        .await;

    assert_eq!(
        plain(&server, None).delta("a", "gone").await.unwrap(),
        Delta::Resync
    );

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/core/v4/events/ev0"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"Code": 2011, "Error": "x"})))
        .mount(&server)
        .await;

    let result = plain(&server, None).delta("a", "ev0").await;

    assert!(matches!(result, Err(Error::Proton(_))));

    let server = MockServer::start().await;

    mount_get(&server, "/core/v4/events/ev0", event_body(json!([]), 0)).await;
    mount_message(&server, "m3", json!({})).await;
    Mock::given(method("GET"))
        .and(path("/mail/v4/messages/m1"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({"Code": 2501, "Error": "x"})))
        .mount(&server)
        .await;

    let Delta::Page(page) = plain(&server, None).delta("a", "ev0").await.unwrap() else {
        panic!("expected a page");
    };

    assert!(page.destroyed.contains(&"m1".to_owned()));
    assert!(page.emails.iter().all(|e| e.id != "m1"));

    // A delete followed by a create or an update leaves the id changed only.
    let server = MockServer::start().await;

    mount_get(
        &server,
        "/core/v4/events/ev0",
        json!({
            "Code": 1000, "EventID": "ev1", "More": 0, "Refresh": 0, "Labels": [],
            "Messages": [
                {"ID": "m5", "Action": 0},
                {"ID": "m5", "Action": 1},
                {"ID": "m6", "Action": 0},
                {"ID": "m6", "Action": 2},
            ],
        }),
    )
    .await;
    mount_message(&server, "m5", json!({})).await;
    mount_message(&server, "m6", json!({})).await;

    let Delta::Page(page) = plain(&server, None).delta("a", "ev0").await.unwrap() else {
        panic!("expected a page");
    };
    let ids: Vec<&str> = page.emails.iter().map(|e| e.id.as_str()).collect();

    assert_eq!(ids, ["m5", "m6"]);
    assert!(page.destroyed.is_empty());
}

/// [`run_update_with`] where unlabelling succeeds.
async fn run_update(
    ids: &[&str],
    change: Change,
    label_reply: Value,
) -> (Result<Vec<String>>, Vec<(String, Value)>) {
    run_update_with(ids, change, label_reply, json!({"Code": 1000})).await
}

/// Runs `change` on messages `m1`, `m2` against a server that accepts every
/// triage call (`label_reply` and `unlabel_reply` answer those requests) and
/// returns the outcome and the `PUT`s it received as `(path, body)`.
async fn run_update_with(
    ids: &[&str],
    change: Change,
    label_reply: Value,
    unlabel_reply: Value,
) -> (Result<Vec<String>>, Vec<(String, Value)>) {
    let server = MockServer::start().await;

    for p in ["read", "unread"] {
        Mock::given(method("PUT"))
            .and(path(format!("/mail/v4/messages/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Code": 1000})))
            .mount(&server)
            .await;
    }

    Mock::given(method("PUT"))
        .and(path("/mail/v4/messages/unlabel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(unlabel_reply))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/mail/v4/messages/label"))
        .respond_with(ResponseTemplate::new(200).set_body_json(label_reply))
        .mount(&server)
        .await;

    let ids: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
    let result = plain(&server, None)
        .update(&ids, &change)
        .await
        .map(|u| u.refused);
    let puts = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .map(|r| {
            (
                r.url.path().to_owned(),
                serde_json::from_slice(&r.body).unwrap(),
            )
        })
        .collect();

    (result, puts)
}

#[tokio::test]
async fn update_maps_every_change() {
    let ok = json!({"Code": 1000});
    let ids = ["m1", "m2"];
    let want_ids = json!(["m1", "m2"]);

    let (result, puts) = run_update(&ids, Change::Keyword("$seen", true), ok.clone()).await;

    assert!(result.unwrap().is_empty());
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].0, "/mail/v4/messages/read");
    assert_eq!(puts[0].1["IDs"], want_ids);

    let (_, puts) = run_update(&ids, Change::Keyword("$seen", false), ok.clone()).await;

    assert_eq!(puts[0].0, "/mail/v4/messages/unread");

    let (_, puts) = run_update(&ids, Change::Keyword("$flagged", true), ok.clone()).await;

    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].0, "/mail/v4/messages/label");
    assert_eq!(puts[0].1["LabelID"], "10");

    let (_, puts) = run_update(&ids, Change::Keyword("$flagged", false), ok.clone()).await;

    assert_eq!(puts[0].0, "/mail/v4/messages/unlabel");
    assert_eq!(puts[0].1["LabelID"], "10");

    let (result, puts) = run_update(&ids, Change::Keyword("$answered", true), ok.clone()).await;

    assert!(result.unwrap().is_empty() && puts.is_empty());

    let (_, puts) = run_update(&ids, Change::Mailbox("6".into()), ok.clone()).await;

    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].0, "/mail/v4/messages/label");
    assert_eq!(puts[0].1["LabelID"], "6");

    let patch = Change::Patch {
        add: vec!["l1".into()],
        remove: vec!["0".into()],
    };
    let (_, puts) = run_update(&ids, patch, ok.clone()).await;

    assert_eq!(puts.len(), 2);
    assert_eq!(puts[0].0, "/mail/v4/messages/unlabel");
    assert_eq!(puts[0].1["LabelID"], "0");
    assert_eq!(puts[1].0, "/mail/v4/messages/label");
    assert_eq!(puts[1].1["LabelID"], "l1");

    let refusal = json!({"Code": 1001, "Responses": [{"ID": "m2", "Response": {"Code": 2000}}]});
    let (result, _) = run_update(&ids, Change::Mailbox("6".into()), refusal.clone()).await;

    assert_eq!(result.unwrap(), ["m2"]);

    // A refusal while unlabelling is kept and the additions still run.
    let patch = Change::Patch {
        add: vec!["l1".into()],
        remove: vec!["0".into()],
    };
    let (result, puts) = run_update_with(&ids, patch, ok.clone(), refusal).await;

    assert_eq!(result.unwrap(), ["m2"]);
    assert_eq!(puts.len(), 2);
    assert_eq!(puts[1].0, "/mail/v4/messages/label");
    assert_eq!(puts[1].1["LabelID"], "l1");

    let (result, puts) = run_update(&[], Change::Keyword("$seen", true), ok).await;

    assert!(result.unwrap().is_empty() && puts.is_empty());
}

#[tokio::test]
async fn unsupported_operations() {
    let server = MockServer::start().await;
    let session = plain(&server, None);

    assert!(matches!(
        session.create_mailbox("Tray").await,
        Err(Error::Unsupported(_))
    ));
    assert_eq!(count(&server).await, 0);
}

#[tokio::test]
async fn destroy_deletes_and_skips_refused_ids() {
    let server = MockServer::start().await;
    let refusal = json!({"Code": 1001, "Responses": [{"ID": "m2", "Response": {"Code": 2000}}]});

    Mock::given(method("PUT"))
        .and(path("/mail/v4/messages/delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(refusal))
        .mount(&server)
        .await;

    let gone = plain(&server, None)
        .destroy(&["m1".to_owned(), "m2".to_owned()])
        .await
        .unwrap();

    assert_eq!(gone, ["m1"]);

    let puts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "PUT")
        .collect();

    assert_eq!(puts.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&puts[0].body).unwrap()["IDs"],
        json!(["m1", "m2"])
    );
}

#[tokio::test]
async fn identities_are_active_sending_addresses() {
    let server = MockServer::start().await;
    let mut body: Value = serde_json::from_str(ADDRESSES).unwrap();
    let extra = |id: &str, send: u8, status: u8| {
        json!({
            "ID": id, "Email": format!("{id}@example.invalid"), "Send": send,
            "Receive": 1, "Status": status, "Order": 9, "DisplayName": "Extra", "Keys": [],
        })
    };

    body["Addresses"]
        .as_array_mut()
        .unwrap()
        .extend([extra("x-nosend", 0, 1), extra("x-disabled", 1, 2)]);
    mount_get(&server, "/core/v4/addresses", body).await;

    let identities = plain(&server, None).identities().await.unwrap();
    let ids: Vec<&str> = identities.iter().map(|i| i.id.as_str()).collect();

    assert_eq!(ids, ["id-4", "id-7"]);
    assert_eq!(identities[0].email, "user1@example.invalid");
    assert_eq!(identities[0].name, "<scrubbed>");
}

#[tokio::test]
async fn body_download_and_unsubscribe_with_real_keys() {
    let server = MockServer::start().await;
    let address_key = TestKey::new();
    let pm = inner(&server);
    let keys = unlock(&server, &pm, &address_key).await;
    let session = Session::for_tests(pm, keys, "user-1", None);
    let (key_packets, data_packet) = address_key.encrypt_attachment(b"hello");

    mount_message(
        &server,
        "m1",
        json!({
            "MIMEType": "text/html",
            "Body": address_key.encrypt_body(b"<p>Hi</p>"),
            "Header": "Message-ID: <m1@x>\r\nIn-Reply-To: <p@x>\r\nReferences: <r1@x>\r\n <p@x>\r\nList-Unsubscribe: <mailto:u@x>\r\n",
            "Attachments": [{
                "ID": "att1", "Name": "a.txt", "Size": 5, "MIMEType": "text/plain",
                "KeyPackets": B64.encode(&key_packets), "Disposition": "attachment",
            }],
        }),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/mail/v4/attachments/att1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data_packet))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/mail/v4/messages/m404"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({"Code": 2501, "Error": "x"})))
        .mount(&server)
        .await;

    let body = session.body("m1").await.unwrap().unwrap();

    assert_eq!(body.email_id, "m1");
    assert!(
        body.html.as_deref() == Some("<p>Hi</p>"),
        "html body matches"
    );
    assert!(body.text.is_none());
    assert_eq!(body.message_id, ["m1@x"]);
    assert_eq!(body.in_reply_to, ["p@x"]);
    assert_eq!(body.references, ["r1@x", "p@x"]);
    assert_eq!(body.attachments.len(), 1);
    assert_eq!(body.attachments[0].blob_id, "att1");
    assert_eq!(
        body.unsubscribe.and_then(|u| u.mailto).as_deref(),
        Some("mailto:u@x")
    );
    assert!(session.has_unsubscribe("m1").await);
    assert!(!session.has_unsubscribe("m404").await);

    let bytes = session.download("m1", &body.attachments[0]).await.unwrap();

    assert!(bytes == b"hello", "the attachment decrypts");

    let nope = Attachment {
        blob_id: "nope".into(),
        name: String::new(),
        mime: String::new(),
        size: 0,
    };

    assert!(matches!(
        session.download("m1", &nope).await,
        Err(Error::NotFound(_))
    ));
    assert!(session.body("m404").await.unwrap().is_none());
}

/// A persist callback that records every blob it is given.
fn collector() -> (Arc<Mutex<Vec<String>>>, Persist) {
    let blobs = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&blobs);
    let persist: Persist = Box::new(move |blob| {
        sink.lock().unwrap().push(blob.to_owned());

        Ok(true)
    });

    (blobs, persist)
}

/// Mounts a 401 for the first `GET p`; mount it before the real handler.
async fn mount_401_once(server: &MockServer, p: &str) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"Code": 401, "Error": "x"})))
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Mounts a 401 for the first `GET /core/v4/users` and the refresh that
/// follows it.
async fn mount_rotation(server: &MockServer) {
    mount_401_once(server, "/core/v4/users").await;
    mount_refresh(server).await;
}

/// Mounts the token refresh, answering with the rotated tokens.
async fn mount_refresh(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/auth/v4/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000, "UID": "uid-1",
            "AccessToken": "access-new", "RefreshToken": "refresh-new",
        })))
        .mount(server)
        .await;
}

/// True when `blob` holds the rotated access token.
fn is_rotated(blob: &str) -> bool {
    serde_json::from_str::<Value>(blob).is_ok_and(|v| v["access_token"] == "access-new")
}

#[tokio::test]
async fn connect_with_unlocks_and_persists_rotated_tokens() {
    let server = MockServer::start().await;
    let address_key = TestKey::new();

    mount_rotation(&server).await;

    for (p, body) in key_replies(&address_key) {
        mount_get(&server, p, body).await;
    }

    let (blobs, persist) = collector();
    let session = connect_with(
        client(&server),
        &blob_json("access-old").to_string(),
        MAILBOX,
        Some(persist),
    )
    .await
    .unwrap();

    assert_eq!(session.server_account_id(), "user-1");

    let saved = blobs.lock().unwrap().clone();

    assert_eq!(saved.len(), 1);
    assert!(is_rotated(&saved[0]));

    // A failure after the refresh still stores the rotated blob.
    let server = MockServer::start().await;

    mount_rotation(&server).await;

    for (p, body) in key_replies(&address_key).into_iter().take(2) {
        mount_get(&server, p, body).await;
    }

    Mock::given(method("GET"))
        .and(path("/core/v4/addresses"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"Code": 0, "Error": "x"})))
        .mount(&server)
        .await;

    let (blobs, persist) = collector();
    let result = connect_with(
        client(&server),
        &blob_json("access-old").to_string(),
        MAILBOX,
        Some(persist),
    )
    .await;

    assert!(result.is_err());

    let saved = blobs.lock().unwrap().clone();

    assert!(saved.len() == 1 && is_rotated(&saved[0]));

    // An unreadable blob fails before any request.
    let server = MockServer::start().await;
    let result = connect_with(client(&server), "not json", MAILBOX, None).await;

    assert!(matches!(result, Err(Error::NotConfigured(_))));
    assert_eq!(count(&server).await, 0);
}

#[tokio::test]
async fn send_creates_a_draft_and_returns_the_sent_id() {
    let server = MockServer::start().await;
    let address_key = TestKey::new();
    let pm = inner(&server);
    let keys = unlock(&server, &pm, &address_key).await;
    let session = Session::for_tests(pm, keys, "user-1", None);
    let store = Store::open_memory().unwrap();
    let identity = Identity {
        id: "a1".into(),
        name: "Test".into(),
        email: "a@example.com".into(),
    };

    mount_get(
        &server,
        "/core/v4/keys",
        json!({"Code": 1000, "Keys": [], "RecipientType": 2}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/mail/v4/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "Message": {"ID": "d1", "AddressID": "a1", "Flags": 0},
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mail/v4/messages/d1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "Sent": {"ID": "sent-1", "AddressID": "a1", "Flags": 2},
        })))
        .mount(&server)
        .await;

    let mut draft = Draft {
        to: vec![Address {
            name: Some("Bob".into()),
            email: "bob@example.net".into(),
        }],
        subject: "Hello Bob".into(),
        ..Draft::default()
    };
    let id = session
        .send(&store, "a", &identity, &draft, "text", "<p>x</p>")
        .await
        .unwrap();

    assert_eq!(id, "sent-1");

    let requests = server.received_requests().await.unwrap();
    let draft_request = requests
        .iter()
        .find(|r| r.method.as_str() == "POST" && r.url.path() == "/mail/v4/messages")
        .unwrap();
    let body: Value = serde_json::from_slice(&draft_request.body).unwrap();

    assert_eq!(body["Message"]["Subject"], "Hello Bob");
    assert_eq!(body["Message"]["MIMEType"], "text/html");

    draft.to.clear();

    let before = count(&server).await;
    let result = session
        .send(&store, "a", &identity, &draft, "text", "<p>x</p>")
        .await;

    assert!(matches!(result, Err(Error::Proton(_))));
    assert_eq!(count(&server).await, before);
}

#[tokio::test]
async fn persist_retries_after_a_failed_write() {
    let server = MockServer::start().await;
    let nothing = no_labels();

    Mock::given(method("GET"))
        .and(path("/core/v4/labels"))
        .and(query_param("Type", "1"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"Code": 401, "Error": "x"})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/v4/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000, "UID": "uid-1",
            "AccessToken": "access-new", "RefreshToken": "refresh-new",
        })))
        .mount(&server)
        .await;
    mount_labels(&server, &nothing, &nothing, &nothing).await;

    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&calls);
    let persist: Persist = Box::new(move |blob| {
        let mut calls = sink.lock().unwrap();

        calls.push(blob.to_owned());

        if calls.len() == 1 {
            return Err(Error::Other("disk full".into()));
        }

        Ok(true)
    });
    let session = plain(&server, Some(persist));
    let store = Store::open_memory().unwrap();
    let seen = || calls.lock().unwrap().clone();

    session.sync_mailboxes(&store, "a").await.unwrap();

    assert_eq!(seen().len(), 1);
    assert!(is_rotated(&seen()[0]));

    session.sync_mailboxes(&store, "a").await.unwrap();

    assert_eq!(seen().len(), 2);
    assert!(seen()[0] == seen()[1], "the retry carries the same blob");

    session.sync_mailboxes(&store, "a").await.unwrap();

    assert_eq!(seen().len(), 2);
}

/// A persist callback that records every blob and answers `outcome(n)` to its
/// `n`th call (counting from 1).
fn scripted_collector(outcome: fn(usize) -> Result<bool>) -> (Arc<Mutex<Vec<String>>>, Persist) {
    let blobs = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&blobs);
    let persist: Persist = Box::new(move |blob| {
        let mut blobs = sink.lock().unwrap();

        blobs.push(blob.to_owned());

        outcome(blobs.len())
    });

    (blobs, persist)
}

/// A server whose first labels request is rotated by a refresh, and a session
/// whose first persist write fails; returns them after the first
/// `sync_mailboxes` has left the rotated blob unsaved.
async fn rotated_with_unsaved_blob() -> (Session, Arc<Mutex<Vec<String>>>) {
    let server = MockServer::start().await;
    let nothing = no_labels();

    mount_401_once(&server, "/core/v4/labels").await;
    mount_refresh(&server).await;
    mount_labels(&server, &nothing, &nothing, &nothing).await;

    let (calls, persist) = scripted_collector(|n| match n {
        1 => Err(Error::Other("disk full".into())),
        _ => Ok(true),
    });
    let session = plain(&server, Some(persist));
    let store = Store::open_memory().unwrap();

    session.sync_mailboxes(&store, "a").await.unwrap();

    assert_eq!(calls.lock().unwrap().len(), 1);

    (session, calls)
}

#[tokio::test]
async fn operations_without_a_request_still_persist() {
    let (session, calls) = rotated_with_unsaved_blob().await;
    let result = session.create_mailbox("x").await;

    assert!(matches!(result, Err(Error::Unsupported(_))));
    assert_eq!(
        calls.lock().unwrap().len(),
        2,
        "create_mailbox retries the write"
    );

    // Destroying nothing sends nothing, so it too only retries the write.
    let (session, calls) = rotated_with_unsaved_blob().await;
    let result = session.destroy(&[]).await;

    assert!(result.unwrap().is_empty());
    assert_eq!(calls.lock().unwrap().len(), 2, "destroy retries the write");
}

#[tokio::test]
async fn persist_ok_false_is_not_retried() {
    let server = MockServer::start().await;
    let nothing = no_labels();

    mount_401_once(&server, "/core/v4/labels").await;
    mount_refresh(&server).await;
    mount_labels(&server, &nothing, &nothing, &nothing).await;

    let (calls, persist) = scripted_collector(|_| Ok(false));
    let session = plain(&server, Some(persist));
    let store = Store::open_memory().unwrap();

    session.sync_mailboxes(&store, "a").await.unwrap();
    session.sync_mailboxes(&store, "a").await.unwrap();

    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "a superseded write is final"
    );
}

#[tokio::test]
async fn persist_survives_operation_errors() {
    // A refresh followed by a failing call still stores the rotated blob.
    let server = MockServer::start().await;

    mount_401_once(&server, "/core/v4/addresses").await;
    mount_refresh(&server).await;
    Mock::given(method("GET"))
        .and(path("/core/v4/addresses"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"Code": 0, "Error": "x"})))
        .mount(&server)
        .await;

    let (calls, persist) = collector();
    let result = plain(&server, Some(persist)).identities().await;

    assert!(result.is_err());
    assert!(calls.lock().unwrap().len() == 1 && is_rotated(&calls.lock().unwrap()[0]));

    // `has_unsubscribe` swallows the failure but still stores the blob.
    let server = MockServer::start().await;

    mount_401_once(&server, "/mail/v4/messages/m1").await;
    mount_refresh(&server).await;
    Mock::given(method("GET"))
        .and(path("/mail/v4/messages/m1"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({"Code": 2501, "Error": "x"})))
        .mount(&server)
        .await;

    let (calls, persist) = collector();

    assert!(!plain(&server, Some(persist)).has_unsubscribe("m1").await);
    assert!(calls.lock().unwrap().len() == 1 && is_rotated(&calls.lock().unwrap()[0]));
}

/// An already unlocked session blob as the crate serialises it.
fn unlocked_blob() -> Value {
    let mut blob = blob_json("access-old");

    blob["key_password"] = json!(MAILBOX);

    blob
}

#[tokio::test]
async fn connect_baseline_is_the_supplied_blob_and_the_user_lookup_persists() {
    let address_key = TestKey::new();

    // The supplied text differs from the crate's serialisation and nothing
    // rotates: the first comparison writes the normalised blob once.
    let server = MockServer::start().await;

    for (p, body) in key_replies(&address_key) {
        mount_get(&server, p, body).await;
    }

    let (blobs, persist) = collector();
    let supplied = serde_json::to_string_pretty(&unlocked_blob()).unwrap();

    connect_with(client(&server), &supplied, MAILBOX, Some(persist))
        .await
        .unwrap();

    let saved = blobs.lock().unwrap().clone();

    assert!(saved.len() == 1 && !is_rotated(&saved[0]));

    // The supplied text equals the crate's serialisation: only a rotation
    // during the user lookup (the second `GET /core/v4/users`) is written,
    // on success and on failure.
    let supplied = serde_json::to_string(
        &serde_json::from_value::<PersistedSession>(unlocked_blob()).unwrap(),
    )
    .unwrap();

    for lookup_works in [true, false] {
        let server = MockServer::start().await;
        let replies = key_replies(&address_key);

        Mock::given(method("GET"))
            .and(path("/core/v4/users"))
            .respond_with(ResponseTemplate::new(200).set_body_json(replies[0].1.clone()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_rotation(&server).await;

        if lookup_works {
            mount_get(&server, replies[0].0, replies[0].1.clone()).await;
        } else {
            Mock::given(method("GET"))
                .and(path("/core/v4/users"))
                .respond_with(
                    ResponseTemplate::new(500).set_body_json(json!({"Code": 0, "Error": "x"})),
                )
                .mount(&server)
                .await;
        }

        for (p, body) in replies.into_iter().skip(1) {
            mount_get(&server, p, body).await;
        }

        let (blobs, persist) = collector();
        let result = connect_with(client(&server), &supplied, MAILBOX, Some(persist)).await;
        let saved = blobs.lock().unwrap().clone();

        assert_eq!(result.is_ok(), lookup_works);
        assert!(
            saved.len() == 1 && is_rotated(&saved[0]),
            "lookup_works={lookup_works}"
        );
    }
}
