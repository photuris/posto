//! `posto_core::gmail` against a wiremock fake of Google's OAuth and Gmail API
//! endpoints, plus the loopback sign-in with a scripted "browser".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use posto_core::Error;
use posto_core::gmail::{Endpoints, GoogleClient, Login, Session, connect_with, login_with};
use posto_core::provider::{Delta, Identity};
use posto_core::store::{Mailbox, Store};
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The API root path on the fake server.
const API: &str = "/gmail/v1/users/me";

/// Endpoints pointing at `server`, with a 1 ms back-off.
fn endpoints(server: &MockServer) -> Endpoints {
    let uri = server.uri();

    Endpoints {
        auth: format!("{uri}/auth"),
        token: format!("{uri}/token"),
        api: format!("{uri}{API}"),
        retry_base: Duration::from_millis(1),
    }
}

/// The made-up OAuth client every test uses.
fn client() -> GoogleClient {
    GoogleClient {
        client_id: "cid".into(),
        client_secret: "csecret".into(),
    }
}

/// A JSON response with `status`.
fn reply(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

/// Mounts `GET <API><p>` answering `status` with `body`, at default priority.
async fn mount_get(server: &MockServer, p: &str, status: u16, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("{API}{p}")))
        .respond_with(reply(status, body))
        .mount(server)
        .await;
}

/// Mounts the token endpoint answering `access` once.
async fn mount_token_once(server: &MockServer, access: &str) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(reply(
            200,
            json!({"access_token": access, "expires_in": 3599}),
        ))
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Mounts the token endpoint answering `at-1` once and a profile with
/// `history_id`, and connects.
async fn connected(server: &MockServer, history_id: &str) -> Session {
    mount_token_once(server, "at-1").await;
    mount_get(
        server,
        "/profile",
        200,
        json!({"emailAddress": "me@gmail.com", "historyId": history_id}),
    )
    .await;

    connect_with(endpoints(server), client(), "rt-1")
        .await
        .unwrap()
}

/// The requests the server saw for `p` (a path below the API root, or a
/// plain path such as `/token`), in order.
async fn requests(server: &MockServer, p: &str) -> Vec<Request> {
    let full = if p == "/token" {
        p.to_owned()
    } else {
        format!("{API}{p}")
    };

    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == full)
        .collect()
}

/// The query pairs of a request.
fn query(r: &Request) -> Vec<(String, String)> {
    r.url.query_pairs().into_owned().collect()
}

/// The values of query parameter `name` of a request.
fn query_all(r: &Request, name: &str) -> Vec<String> {
    query(r)
        .into_iter()
        .filter(|(k, _)| k == name)
        .map(|(_, v)| v)
        .collect()
}

/// A request's form body as a map.
fn form(r: &Request) -> HashMap<String, String> {
    Url::parse(&format!("http://x/?{}", String::from_utf8_lossy(&r.body)))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// The ids of a message list, in order.
fn email_ids(delta: Delta) -> (Vec<String>, Vec<String>, String) {
    let Delta::Page(page) = delta else {
        panic!("expected a page");
    };

    assert!(!page.more);
    assert!(!page.mailboxes_changed);

    (
        page.emails.into_iter().map(|e| e.id).collect(),
        page.destroyed,
        page.state,
    )
}

/// A `format=metadata` reply.
fn metadata(id: &str, labels: Value) -> Value {
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

/// Mounts the metadata of message `id`.
async fn mount_message(server: &MockServer, id: &str, labels: Value) {
    mount_get(
        server,
        &format!("/messages/{id}"),
        200,
        metadata(id, labels),
    )
    .await;
}

// ── connect, tokens and requests ────────────────────────────────────────────

#[tokio::test]
async fn connect_refreshes_then_reads_the_profile() {
    let server = MockServer::start().await;
    let session = connected(&server, "100").await;
    let token = requests(&server, "/token").await;
    let profile = requests(&server, "/profile").await;

    assert_eq!(token.len(), 1);
    assert_eq!(
        form(&token[0]),
        HashMap::from([
            ("grant_type".to_owned(), "refresh_token".to_owned()),
            ("refresh_token".to_owned(), "rt-1".to_owned()),
            ("client_id".to_owned(), "cid".to_owned()),
            ("client_secret".to_owned(), "csecret".to_owned()),
        ])
    );
    assert_eq!(profile.len(), 1);
    assert_eq!(
        profile[0].headers.get("authorization").unwrap(),
        "Bearer at-1"
    );
    assert_eq!(session.server_account_id(), "me@gmail.com");
    assert_eq!(session.provider_name(), "gmail");
    assert!(session.local_counts());
    assert_eq!(format!("{session:?}"), "gmail::Session");

    let rejected = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(reply(400, json!({"error": "invalid_grant"})))
        .mount(&rejected)
        .await;

    let result = connect_with(endpoints(&rejected), client(), "rt-1").await;

    assert!(matches!(result, Err(Error::Auth)));

    let broken = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(reply(
            500,
            json!({"error": "server_error", "error_description": "secret-text"}),
        ))
        .mount(&broken)
        .await;

    let Err(Error::Http(text)) = connect_with(endpoints(&broken), client(), "rt-1").await else {
        panic!("expected an http error");
    };

    assert!(text.contains("500"));
    assert!(!text.contains("secret-text"));
}

#[tokio::test]
async fn expired_access_token_is_refreshed_once() {
    let server = MockServer::start().await;
    let session = connected(&server, "100").await;

    mount_token_once(&server, "at-2").await;
    Mock::given(method("GET"))
        .and(path(format!("{API}/labels")))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    mount_get(&server, "/labels", 200, json!({"labels": []})).await;

    let store = Store::open_memory().unwrap();

    session.sync_mailboxes(&store, "a").await.unwrap();

    let labels = requests(&server, "/labels").await;

    assert_eq!(requests(&server, "/token").await.len(), 2);
    assert_eq!(labels.len(), 2);
    assert_eq!(
        labels[0].headers.get("authorization").unwrap(),
        "Bearer at-1"
    );
    assert_eq!(
        labels[1].headers.get("authorization").unwrap(),
        "Bearer at-2"
    );

    let rejected = MockServer::start().await;
    let session = connected(&rejected, "100").await;

    mount_token_once(&rejected, "at-2").await;
    mount_get(&rejected, "/labels", 401, json!({})).await;

    let result = session.sync_mailboxes(&store, "a").await;

    assert!(matches!(result, Err(Error::Auth)));
    assert_eq!(requests(&rejected, "/labels").await.len(), 2);
}

#[tokio::test]
async fn rate_limits_and_server_errors_are_retried() {
    const SEND_AS: &str = "/settings/sendAs";

    /// Mounts `status` with `body` for the first `times` requests of sendAs.
    async fn first(server: &MockServer, times: u64, status: u16, body: Value) {
        Mock::given(method("GET"))
            .and(path(format!("{API}{SEND_AS}")))
            .respond_with(reply(status, body))
            .up_to_n_times(times)
            .with_priority(1)
            .mount(server)
            .await;
    }

    let server = MockServer::start().await;
    let session = connected(&server, "100").await;

    first(&server, 2, 429, json!({})).await;
    mount_get(&server, SEND_AS, 200, json!({"sendAs": []})).await;

    assert_eq!(session.identities().await.unwrap(), vec![]);
    assert_eq!(requests(&server, SEND_AS).await.len(), 3);

    let down = MockServer::start().await;
    let session = connected(&down, "100").await;

    mount_get(&down, SEND_AS, 503, json!({})).await;

    assert!(matches!(session.identities().await, Err(Error::Http(_))));
    assert_eq!(requests(&down, SEND_AS).await.len(), 4);

    let limited = MockServer::start().await;
    let session = connected(&limited, "100").await;

    first(
        &limited,
        1,
        403,
        json!({"error": {"errors": [{"reason": "rateLimitExceeded"}]}}),
    )
    .await;
    mount_get(&limited, SEND_AS, 200, json!({"sendAs": []})).await;

    assert!(session.identities().await.is_ok());
    assert_eq!(requests(&limited, SEND_AS).await.len(), 2);

    let denied = MockServer::start().await;
    let session = connected(&denied, "100").await;

    mount_get(
        &denied,
        SEND_AS,
        403,
        json!({"error": {"errors": [{"reason": "insufficientPermissions"}], "message": "SECRETTEXT"}}),
    )
    .await;

    assert!(matches!(session.identities().await, Err(Error::Auth)));
    assert_eq!(requests(&denied, SEND_AS).await.len(), 1);

    let bad = MockServer::start().await;
    let session = connected(&bad, "100").await;

    mount_get(
        &bad,
        SEND_AS,
        400,
        json!({"error": {"status": "SECRETTEXT", "message": "SECRETTEXT"}}),
    )
    .await;

    let err = session.identities().await.unwrap_err();

    assert!(matches!(err, Error::Http(_)));
    assert!(err.to_string().contains("400"));
    assert!(!err.to_string().contains("SECRETTEXT"));
    assert!(!format!("{err:?}").contains("SECRETTEXT"));

    // A refresh in the middle of a call that fails at the token endpoint.
    let stale = MockServer::start().await;
    let session = connected(&stale, "100").await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(reply(400, json!({"error": "csecret"})))
        .mount(&stale)
        .await;
    mount_get(&stale, SEND_AS, 401, json!({})).await;

    let err = session.identities().await.unwrap_err();

    assert!(matches!(err, Error::Http(_)));
    assert!(!err.to_string().contains("csecret"));
    assert!(!format!("{err:?}").contains("csecret"));
}

// ── identities and labels ───────────────────────────────────────────────────

#[tokio::test]
async fn identities_are_primary_or_accepted_send_as() {
    let identity = |email: &str, name: &str| Identity {
        id: email.into(),
        name: name.into(),
        email: email.into(),
    };

    for (body, want) in [
        (json!({"sendAs": []}), vec![]),
        (json!({}), vec![]),
        (
            json!({"sendAs": [{
                "sendAsEmail": "me@gmail.com",
                "displayName": "Me",
                "isPrimary": true,
                "verificationStatus": "accepted",
            }]}),
            vec![identity("me@gmail.com", "Me")],
        ),
        (
            json!({"sendAs": [
                {"sendAsEmail": "me@gmail.com", "displayName": "Me", "isPrimary": true},
                {"sendAsEmail": "alias@example.org", "verificationStatus": "accepted"},
                {"sendAsEmail": "pending@example.org", "displayName": "P", "verificationStatus": "pending"},
            ]}),
            vec![
                identity("me@gmail.com", "Me"),
                identity("alias@example.org", ""),
            ],
        ),
    ] {
        let server = MockServer::start().await;
        let session = connected(&server, "100").await;

        mount_get(&server, "/settings/sendAs", 200, body).await;

        assert_eq!(session.identities().await.unwrap(), want);
    }
}

/// A mailbox row of `account`.
fn mailbox(account: &str, id: &str, total: i64, unread: i64) -> Mailbox {
    Mailbox {
        account_id: account.into(),
        id: id.into(),
        name: id.into(),
        role: None,
        parent_id: None,
        sort_order: 50,
        total,
        unread,
    }
}

#[tokio::test]
async fn labels_become_mailboxes() {
    let system = |id: &str| json!({"id": id, "name": id, "type": "system"});
    let server = MockServer::start().await;
    let session = connected(&server, "100").await;

    mount_get(
        &server,
        "/labels",
        200,
        json!({"labels": [
            system("INBOX"), system("SENT"), system("DRAFT"), system("SPAM"), system("TRASH"),
            system("UNREAD"), system("STARRED"), system("IMPORTANT"), system("CHAT"),
            system("CATEGORY_SOCIAL"),
            {"id": "Label_1", "name": "Receipts", "type": "user"},
            {"id": "Label_2", "name": "Jev/Alerts", "type": "user"},
        ]}),
    )
    .await;

    let store = Store::open_memory().unwrap();
    let other = mailbox("b", "keep", 3, 1);

    store
        .upsert_mailboxes(&[
            mailbox("a", "old", 1, 1),
            other.clone(),
            mailbox("a", "INBOX", 7, 2),
        ])
        .unwrap();
    session.sync_mailboxes(&store, "a").await.unwrap();

    let rows = store.mailboxes("a").unwrap();
    let got: Vec<(&str, Option<&str>, &str, i64)> = rows
        .iter()
        .map(|m| {
            (
                m.id.as_str(),
                m.role.as_deref(),
                m.name.as_str(),
                m.sort_order,
            )
        })
        .collect();

    assert_eq!(
        got,
        vec![
            ("INBOX", Some("inbox"), "Inbox", 0),
            ("DRAFT", Some("drafts"), "Drafts", 1),
            ("SENT", Some("sent"), "Sent", 2),
            ("ARCHIVE", Some("archive"), "Archive", 3),
            ("SPAM", Some("junk"), "Spam", 4),
            ("TRASH", Some("trash"), "Trash", 5),
            ("Label_1", None, "Receipts", 100),
            ("Label_2", None, "Jev/Alerts", 101),
        ]
    );
    assert!(
        rows.iter()
            .all(|m| m.parent_id.is_none() && m.account_id == "a")
    );

    let inbox = &rows[0];

    assert_eq!((inbox.total, inbox.unread), (7, 2));
    assert_eq!(store.mailboxes("b").unwrap(), vec![other]);

    session.sync_mailboxes(&store, "a").await.unwrap();

    assert_eq!(store.mailboxes("a").unwrap().len(), 8);

    let empty = MockServer::start().await;
    let session = connected(&empty, "100").await;

    mount_get(&empty, "/labels", 200, json!({"labels": []})).await;

    let store = Store::open_memory().unwrap();

    session.sync_mailboxes(&store, "a").await.unwrap();

    assert_eq!(store.mailboxes("a").unwrap().len(), 6);

    let odd = MockServer::start().await;
    let session = connected(&odd, "100").await;
    let user = |id: &str, name: &str| json!({"id": id, "name": name, "type": "user"});

    mount_get(
        &odd,
        "/labels",
        200,
        json!({"labels": [
            user("INBOX", "Mine"), user("ARCHIVE", "Mine"), user("", "Mine"),
            user("Label_1", "One"), user("Label_1", "Again"), user("Lbl-opaque", "Other"),
        ]}),
    )
    .await;

    let store = Store::open_memory().unwrap();

    session.sync_mailboxes(&store, "a").await.unwrap();

    let rows = store.mailboxes("a").unwrap();
    let built_in: Vec<(&str, Option<&str>, &str)> = rows
        .iter()
        .take(6)
        .map(|m| (m.id.as_str(), m.role.as_deref(), m.name.as_str()))
        .collect();
    let users: Vec<(&str, &str, i64)> = rows
        .iter()
        .skip(6)
        .map(|m| (m.id.as_str(), m.name.as_str(), m.sort_order))
        .collect();

    assert_eq!(
        built_in,
        vec![
            ("INBOX", Some("inbox"), "Inbox"),
            ("DRAFT", Some("drafts"), "Drafts"),
            ("SENT", Some("sent"), "Sent"),
            ("ARCHIVE", Some("archive"), "Archive"),
            ("SPAM", Some("junk"), "Spam"),
            ("TRASH", Some("trash"), "Trash"),
        ]
    );
    assert_eq!(
        users,
        vec![("Label_1", "One", 100), ("Lbl-opaque", "Other", 101)]
    );
}

#[tokio::test]
async fn create_mailbox_makes_a_visible_label() {
    let server = MockServer::start().await;
    let session = connected(&server, "100").await;

    Mock::given(method("POST"))
        .and(path(format!("{API}/labels")))
        .respond_with(reply(
            200,
            json!({"id": "Label_9", "name": "Alerts", "type": "user"}),
        ))
        .mount(&server)
        .await;

    let created = session.create_mailbox("Alerts").await.unwrap();
    let posted = requests(&server, "/labels").await;
    let body: Value = serde_json::from_slice(&posted[0].body).unwrap();

    assert_eq!(
        body,
        json!({"name": "Alerts", "labelListVisibility": "labelShow", "messageListVisibility": "show"})
    );
    assert_eq!(created.id, "Label_9");
    assert_eq!(created.name, "Alerts");
    assert_eq!(created.role, None);
    assert_eq!(created.account_id, "");
}

// ── sync ────────────────────────────────────────────────────────────────────

/// Mounts the message list: `first` for a request without `pageToken`.
async fn mount_list(server: &MockServer, first: Value, second: Option<Value>) {
    mount_get(server, "/messages", 200, first).await;

    if let Some(second) = second {
        Mock::given(method("GET"))
            .and(path(format!("{API}/messages")))
            .and(query_param("pageToken", "p2"))
            .respond_with(reply(200, second))
            .with_priority(1)
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn initial_captures_history_before_listing() {
    let server = MockServer::start().await;
    let session = connected(&server, "500").await;

    mount_list(
        &server,
        json!({"messages": [{"id": "m1", "threadId": "t1"}, {"id": "m2"}, {"id": "m3"}], "nextPageToken": "p2"}),
        Some(json!({"messages": [{"id": "m4"}]})),
    )
    .await;

    for id in ["m1", "m2", "m4"] {
        mount_message(&server, id, json!(["INBOX", "UNREAD"])).await;
    }

    mount_get(&server, "/messages/m3", 404, json!({})).await;

    let (emails, cursor) = session.initial("a", 1000).await.unwrap();
    let got: Vec<&str> = emails.iter().map(|e| e.id.as_str()).collect();

    assert_eq!(got, ["m1", "m2", "m4"]);
    assert_eq!(cursor, "500");
    assert_eq!(emails[0].account_id, "a");
    assert_eq!(emails[0].mailbox_ids, ["INBOX"]);

    let all = server.received_requests().await.unwrap();
    let last_profile = all
        .iter()
        .rposition(|r| r.url.path() == format!("{API}/profile"))
        .unwrap();
    let first_list = all
        .iter()
        .position(|r| r.url.path() == format!("{API}/messages"))
        .unwrap();

    assert!(last_profile < first_list);

    let lists = requests(&server, "/messages").await;

    assert_eq!(lists.len(), 2);

    for list in &lists {
        let q = query(list);

        for pair in [
            ("includeSpamTrash", "true"),
            ("q", "-in:chats"),
            ("maxResults", "500"),
        ] {
            assert!(
                q.contains(&(pair.0.to_owned(), pair.1.to_owned())),
                "{pair:?}"
            );
        }
    }

    assert_eq!(query_all(&lists[1], "pageToken"), ["p2"]);

    for id in ["m1", "m2", "m3", "m4"] {
        let fetches = requests(&server, &format!("/messages/{id}")).await;

        assert_eq!(fetches.len(), 1, "{id}");
        assert_eq!(query_all(&fetches[0], "format"), ["metadata"]);
        assert_eq!(
            query_all(&fetches[0], "metadataHeaders"),
            ["From", "To", "Cc", "Subject", "Content-Type"]
        );
    }

    let capped = MockServer::start().await;
    let session = connected(&capped, "500").await;

    mount_list(
        &capped,
        json!({"messages": [{"id": "m1"}, {"id": "m2"}, {"id": "m3"}]}),
        None,
    )
    .await;

    for id in ["m1", "m2", "m3"] {
        mount_message(&capped, id, json!(["INBOX"])).await;
    }

    let (emails, _) = session.initial("a", 2).await.unwrap();

    assert_eq!(emails.len(), 2);
    assert_eq!(
        query_all(&requests(&capped, "/messages").await[0], "maxResults"),
        ["2"]
    );
    assert_eq!(
        requests(&capped, "/messages/m1").await.len()
            + requests(&capped, "/messages/m2").await.len()
            + requests(&capped, "/messages/m3").await.len(),
        2
    );

    let none = MockServer::start().await;
    let session = connected(&none, "500").await;
    let (emails, cursor) = session.initial("a", 0).await.unwrap();

    assert!(emails.is_empty());
    assert_eq!(cursor, "500");
    assert!(requests(&none, "/messages").await.is_empty());

    let chats = MockServer::start().await;
    let session = connected(&chats, "500").await;

    mount_list(
        &chats,
        json!({"messages": [{"id": "m1"}, {"id": "m2"}]}),
        None,
    )
    .await;
    mount_message(&chats, "m1", json!(["INBOX"])).await;
    mount_message(&chats, "m2", json!(["CHAT"])).await;

    let (emails, _) = session.initial("a", 10).await.unwrap();
    let got: Vec<&str> = emails.iter().map(|e| e.id.as_str()).collect();

    assert_eq!(got, ["m1"]);
}

/// Mounts the two history pages of the main delta scenario.
async fn mount_history(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("{API}/history")))
        .and(query_param("pageToken", "h2"))
        .respond_with(reply(
            200,
            json!({"history": [
                {"id": "103", "messagesDeleted": [{"message": {"id": "m3"}}]},
                {"id": "104", "messagesAdded": [{"message": {"id": "m4"}}]},
                {"id": "105", "messagesDeleted": [{"message": {"id": "m4"}}]},
            ], "historyId": "106"}),
        ))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API}/history")))
        .and(query_param("startHistoryId", "100"))
        .respond_with(reply(
            200,
            json!({"history": [
                {"id": "101", "messagesAdded": [{"message": {"id": "m1"}}]},
                {"id": "102", "labelsAdded": [{"message": {"id": "m2"}, "labelIds": ["STARRED"]}]},
            ], "nextPageToken": "h2", "historyId": "105"}),
        ))
        .mount(server)
        .await;
}

#[tokio::test]
async fn delta_collects_changes_and_deletions() {
    let server = MockServer::start().await;
    let session = connected(&server, "100").await;

    mount_history(&server).await;
    mount_message(&server, "m1", json!(["INBOX"])).await;
    mount_get(&server, "/messages/m2", 404, json!({})).await;
    Mock::given(method("GET"))
        .and(path(format!("{API}/messages/m4")))
        .respond_with(reply(200, metadata("m4", json!(["INBOX"]))))
        .expect(0)
        .mount(&server)
        .await;

    let (emails, destroyed, state) = email_ids(session.delta("a", "100").await.unwrap());

    assert_eq!(emails, ["m1"]);
    assert_eq!(destroyed, ["m3", "m4", "m2"]);
    assert_eq!(state, "106");

    let pages = requests(&server, "/history").await;

    assert_eq!(pages.len(), 2);
    assert_eq!(query_all(&pages[0], "maxResults"), ["500"]);
    assert_eq!(query_all(&pages[1], "pageToken"), ["h2"]);

    let gone = MockServer::start().await;
    let session = connected(&gone, "100").await;

    mount_get(&gone, "/history", 404, json!({})).await;

    assert_eq!(session.delta("a", "100").await.unwrap(), Delta::Resync);

    let broken = MockServer::start().await;
    let session = connected(&broken, "100").await;

    mount_get(&broken, "/history", 500, json!({})).await;

    assert!(matches!(
        session.delta("a", "100").await,
        Err(Error::Http(_))
    ));

    let quiet = MockServer::start().await;
    let session = connected(&quiet, "100").await;

    mount_get(&quiet, "/history", 200, json!({"historyId": "100"})).await;

    let (emails, destroyed, state) = email_ids(session.delta("a", "100").await.unwrap());

    assert!(emails.is_empty() && destroyed.is_empty());
    assert_eq!(state, "100");

    let late = MockServer::start().await;
    let session = connected(&late, "100").await;

    Mock::given(method("GET"))
        .and(path(format!("{API}/history")))
        .and(query_param("pageToken", "h2"))
        .respond_with(ResponseTemplate::new(404))
        .with_priority(1)
        .mount(&late)
        .await;
    mount_get(
        &late,
        "/history",
        200,
        json!({"history": [{"id": "101", "messagesAdded": [{"message": {"id": "m1"}}]}], "nextPageToken": "h2", "historyId": "105"}),
    )
    .await;
    mount_message(&late, "m1", json!(["INBOX"])).await;

    assert_eq!(session.delta("a", "100").await.unwrap(), Delta::Resync);

    let churn = MockServer::start().await;
    let session = connected(&churn, "100").await;

    mount_get(
        &churn,
        "/history",
        200,
        json!({"history": [
            {"id": "1", "messagesAdded": [{"message": {"id": "A"}}]},
            {"id": "2", "messagesDeleted": [{"message": {"id": "B"}}]},
            {"id": "3", "messagesDeleted": [{"message": {"id": "A"}}]},
        ], "historyId": "3"}),
    )
    .await;
    Mock::given(method("GET"))
        .respond_with(reply(200, metadata("A", json!(["INBOX"]))))
        .expect(0)
        .with_priority(9)
        .mount(&churn)
        .await;

    let (emails, destroyed, _) = email_ids(session.delta("a", "100").await.unwrap());

    assert!(emails.is_empty());
    assert_eq!(destroyed, ["A", "B"]);

    let chat = MockServer::start().await;
    let session = connected(&chat, "100").await;

    mount_get(
        &chat,
        "/history",
        200,
        json!({"history": [
            {"id": "1", "messagesAdded": [{"message": {"id": "c1"}}]},
            {"id": "2", "messagesAdded": [{"message": {"id": "z1"}}]},
        ], "historyId": "2"}),
    )
    .await;
    mount_message(&chat, "c1", json!(["CHAT"])).await;
    mount_message(&chat, "z1", json!([])).await;

    let Delta::Page(page) = session.delta("a", "100").await.unwrap() else {
        panic!("expected a page");
    };

    assert_eq!(page.emails.len(), 1);
    assert_eq!(page.emails[0].id, "z1");
    assert_eq!(page.emails[0].mailbox_ids, ["ARCHIVE"]);
    assert_eq!(page.destroyed, ["c1"]);
}

// ── sign-in ─────────────────────────────────────────────────────────────────

/// Mounts the token endpoint of a sign-in answering `body`.
async fn mount_exchange(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(reply(200, body))
        .mount(server)
        .await;
}

/// The reply of a successful code exchange.
fn exchange_ok() -> Value {
    json!({"access_token": "at-1", "refresh_token": "rt-new", "expires_in": 3599})
}

/// What a scripted browser did.
struct Browser {
    /// The sign-in address `open` received.
    url: Arc<Mutex<Option<Url>>>,
    /// The task that sent the callbacks.
    task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

/// Runs `login_with` against `server` with a browser that, once opened, sends
/// each `(target, expected status)` of `steps` to the loopback address in
/// order; `{state}` in a target is replaced with the real state.
async fn sign_in(
    server: &MockServer,
    email: &str,
    timeout: Duration,
    steps: &[(&str, u16)],
) -> (Result<Login, Error>, Browser) {
    let url = Arc::new(Mutex::new(None));
    let task = Arc::new(Mutex::new(None));
    let steps: Vec<(String, u16)> = steps.iter().map(|(t, s)| ((*t).to_owned(), *s)).collect();
    let (url_slot, task_slot) = (url.clone(), task.clone());
    let want_hint = !email.is_empty();
    let email_owned = email.to_owned();
    let result = login_with(
        &endpoints(server),
        &client(),
        email,
        timeout,
        move |address| {
            let parsed = Url::parse(address).unwrap();
            let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();

            assert_eq!(params["client_id"], "cid");
            assert_eq!(params["response_type"], "code");
            assert_eq!(params["scope"], "https://mail.google.com/");
            assert_eq!(params["access_type"], "offline");
            assert_eq!(params["prompt"], "consent");
            assert_eq!(params["code_challenge_method"], "S256");
            assert!(!params["code_challenge"].is_empty());
            assert!(!params["state"].is_empty());
            assert_eq!(params.get("login_hint"), want_hint.then_some(&email_owned));

            let redirect = params["redirect_uri"].clone();
            let state = params["state"].clone();

            assert!(redirect.starts_with("http://127.0.0.1:"));
            assert!(!redirect.ends_with('/'));

            *url_slot.lock().unwrap() = Some(parsed);
            *task_slot.lock().unwrap() = Some(tokio::spawn(async move {
                let http = reqwest::Client::builder().no_proxy().build().unwrap();

                for (target, expected) in steps {
                    let target = target.replace("{state}", &state);
                    let status = http
                        .get(format!("{redirect}{target}"))
                        .send()
                        .await
                        .unwrap()
                        .status();

                    assert_eq!(status.as_u16(), expected, "{target}");
                }
            }));
        },
    )
    .await;

    (result, Browser { url, task })
}

/// Waits for the scripted browser so a failed assertion in it fails the test.
async fn finish(browser: &Browser) {
    let task = browser.task.lock().unwrap().take();

    if let Some(task) = task {
        task.await.unwrap();
    }
}

const SHORT: Duration = Duration::from_millis(300);
const LONG: Duration = Duration::from_secs(10);
const GOOD: &[(&str, u16)] = &[
    ("/favicon.ico", 404),
    ("/?state=wrong&code=x", 404),
    ("/?state={state}&code=c-1", 200),
];

#[tokio::test]
async fn login_with_completes_the_loopback_flow() {
    let server = MockServer::start().await;

    mount_exchange(&server, exchange_ok()).await;
    mount_get(
        &server,
        "/profile",
        200,
        json!({"emailAddress": "Me@Gmail.com", "historyId": "1"}),
    )
    .await;

    let (result, browser) = sign_in(&server, "me@gmail.com", LONG, GOOD).await;

    finish(&browser).await;

    let login = result.unwrap();

    assert_eq!(login.refresh_token, "rt-new");
    assert_eq!(login.email, "Me@Gmail.com");
    assert!(!format!("{login:?}").contains("rt-new"));

    let exchange = form(&requests(&server, "/token").await[0]);
    let url = browser.url.lock().unwrap().clone().unwrap();
    let challenge = url
        .query_pairs()
        .find(|(k, _)| k == "code_challenge")
        .unwrap()
        .1
        .into_owned();

    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(exchange["code"], "c-1");
    assert_eq!(exchange["client_id"], "cid");
    assert_eq!(exchange["client_secret"], "csecret");
    assert!(exchange["redirect_uri"].starts_with("http://127.0.0.1:"));
    assert!(!exchange["code_verifier"].is_empty());
    assert_eq!(
        B64.encode(Sha256::digest(exchange["code_verifier"].as_bytes())),
        challenge
    );

    let profile = requests(&server, "/profile").await;

    assert_eq!(
        profile[0].headers.get("authorization").unwrap(),
        "Bearer at-1"
    );
    assert!(requests(&server, "/settings/sendAs").await.is_empty());

    // Another Google account that cannot send as the configured address.
    let stranger = MockServer::start().await;

    mount_exchange(&stranger, exchange_ok()).await;
    mount_get(
        &stranger,
        "/profile",
        200,
        json!({"emailAddress": "other@gmail.com", "historyId": "1"}),
    )
    .await;
    mount_get(
        &stranger,
        "/settings/sendAs",
        200,
        json!({"sendAs": [{"sendAsEmail": "other@gmail.com", "isPrimary": true}]}),
    )
    .await;

    let (result, browser) = sign_in(&stranger, "me@gmail.com", LONG, GOOD).await;

    finish(&browser).await;

    assert!(matches!(result, Err(Error::Config(_))));

    // Another Google account that has the configured address as an alias.
    let owner = MockServer::start().await;

    mount_exchange(&owner, exchange_ok()).await;
    mount_get(
        &owner,
        "/profile",
        200,
        json!({"emailAddress": "other@gmail.com", "historyId": "1"}),
    )
    .await;
    mount_get(
        &owner,
        "/settings/sendAs",
        200,
        json!({"sendAs": [
            {"sendAsEmail": "other@gmail.com", "isPrimary": true},
            {"sendAsEmail": "me@gmail.com", "verificationStatus": "accepted"},
        ]}),
    )
    .await;

    let (result, browser) = sign_in(&owner, "me@gmail.com", LONG, GOOD).await;

    finish(&browser).await;

    assert_eq!(result.unwrap().email, "other@gmail.com");

    // An empty configured address: no hint, any account.
    let anyone = MockServer::start().await;

    mount_exchange(&anyone, exchange_ok()).await;
    mount_get(
        &anyone,
        "/profile",
        200,
        json!({"emailAddress": "other@gmail.com", "historyId": "1"}),
    )
    .await;

    let (result, browser) = sign_in(&anyone, "", LONG, GOOD).await;

    finish(&browser).await;

    assert_eq!(result.unwrap().email, "other@gmail.com");
    assert!(
        !browser
            .url
            .lock()
            .unwrap()
            .clone()
            .unwrap()
            .query_pairs()
            .any(|(k, _)| k == "login_hint")
    );

    login_failures().await;
}

/// The sign-in failures: wrong path, timeouts, refusal and a reply without a
/// refresh token.
async fn login_failures() {
    // A valid state on another path is not the redirect.
    let server = MockServer::start().await;
    let (result, browser) =
        sign_in(&server, "", SHORT, &[("/cb?state={state}&code=c-1", 404)]).await;

    finish(&browser).await;

    assert!(matches!(&result, Err(Error::Other(s)) if s.contains("timed out")));
    assert!(requests(&server, "/token").await.is_empty());

    // A token endpoint slower than the whole sign-in.
    let slow = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(reply(200, exchange_ok()).set_delay(Duration::from_secs(2)))
        .mount(&slow)
        .await;

    let (result, browser) = sign_in(&slow, "", SHORT, &[("/?state={state}&code=c-1", 200)]).await;

    finish(&browser).await;

    assert!(matches!(&result, Err(Error::Other(s)) if s.contains("timed out")));

    // The user said no: no exchange.
    let refused = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(reply(200, exchange_ok()))
        .expect(0)
        .mount(&refused)
        .await;

    let (result, browser) = sign_in(
        &refused,
        "",
        LONG,
        &[("/?state={state}&error=access_denied", 200)],
    )
    .await;

    finish(&browser).await;

    assert!(matches!(result, Err(Error::Other(_))));

    // Nobody opens the address.
    let idle = MockServer::start().await;
    let result = login_with(
        &endpoints(&idle),
        &client(),
        "",
        Duration::from_millis(200),
        |_| {},
    )
    .await;

    assert!(matches!(&result, Err(Error::Other(s)) if s.contains("timed out")));

    // Google issued no refresh token.
    let bare = MockServer::start().await;

    mount_exchange(&bare, json!({"access_token": "at-1", "expires_in": 3599})).await;

    let (result, browser) = sign_in(&bare, "", LONG, &[("/?state={state}&code=c-1", 200)]).await;

    finish(&browser).await;

    assert!(matches!(result, Err(Error::Other(_))));
}
