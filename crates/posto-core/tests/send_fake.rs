//! `compose::send` against a wiremock fake JMAP server.

use posto_core::Error;
use posto_core::compose::{Draft, send};
use posto_core::config::{Account, Config};
use posto_core::jmap::Client;
use posto_core::provider::Connection;
use posto_core::store::{Address, EmailSummary, Mailbox, Store};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SESSION: &str = include_str!("fixtures/session.json");

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

async fn client(server: &MockServer) -> Connection {
    Connection::Jmap(
        Client::connect(&format!("{}/session", server.uri()), "tok")
            .await
            .unwrap(),
    )
}

/// Mounts `responses` for POSTs whose body contains `needle`.
async fn mount(server: &MockServer, needle: &str, responses: Value, times: u64) {
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains(needle))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "methodResponses": responses })),
        )
        .expect(times)
        .mount(server)
        .await;
}

/// Mounts the Identity/get reply: a decoy first, then the configured address.
async fn identities(server: &MockServer, list: Value) {
    mount(
        server,
        "Identity/get",
        json!([["Identity/get", { "list": list }, "0"]]),
        1,
    )
    .await;
}

fn two_identities() -> Value {
    json!([
        { "id": "i1", "name": "Other", "email": "other@example.com" },
        { "id": "i2", "name": "Me", "email": "Me@Example.com" },
    ])
}

fn cfg() -> Config {
    Config {
        accounts: vec![Account::new("a", "me@example.com")],
        ..Config::default()
    }
}

fn store() -> Store {
    let store = Store::open_memory().unwrap();
    let mailbox = |id: &str, role: &str| Mailbox {
        account_id: "a".into(),
        id: id.into(),
        name: role.into(),
        role: Some(role.into()),
        parent_id: None,
        sort_order: 1,
        total: 0,
        unread: 0,
    };

    store
        .upsert_mailboxes(&[mailbox("mb-drafts", "drafts"), mailbox("mb-sent", "sent")])
        .unwrap();

    store
}

fn draft() -> Draft {
    Draft {
        account_id: "a".into(),
        to: vec![Address {
            name: None,
            email: "bob@example.com".into(),
        }],
        subject: "Hi".into(),
        body: "hello".into(),
        ..Draft::default()
    }
}

fn submit_responses(set_updated: Value) -> Value {
    json!([
        ["Email/set", { "created": { "d": { "id": "e-new" } } }, "0"],
        ["EmailSubmission/set", { "created": { "s": { "id": "sub1" } } }, "1"],
        ["Email/set", set_updated, "1"],
    ])
}

#[tokio::test]
async fn send_creates_and_submits() {
    let server = server().await;
    identities(&server, two_identities()).await;
    mount(
        &server,
        "EmailSubmission/set",
        submit_responses(json!({ "updated": { "e-new": null } })),
        1,
    )
    .await;

    let id = send(&cfg(), &store(), &client(&server).await, "a", &draft())
        .await
        .unwrap();

    assert_eq!(id, "e-new");

    // Assert on the recorded request, so a wrong field fails here by name.
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests
        .iter()
        .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
        .find(|b| b["methodCalls"][1][0] == "EmailSubmission/set")
        .expect("no send request recorded");
    let calls = &body["methodCalls"];
    let create = &calls[0][1]["create"]["d"];
    let submission = &calls[1][1];

    assert_eq!(calls[0][0], "Email/set");
    assert_eq!(calls[0][2], "0");
    assert_eq!(calls[1][2], "1");
    assert_eq!(create["mailboxIds"], json!({ "mb-drafts": true }));
    assert_eq!(create["from"][0]["email"], "Me@Example.com");
    assert_eq!(
        create["bodyStructure"]["subParts"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(submission["create"]["s"]["emailId"], "#d");
    assert_eq!(submission["create"]["s"]["identityId"], "i2");
    assert_eq!(
        submission["onSuccessUpdateEmail"]["#s"]["mailboxIds/mb-sent"],
        true
    );
}

#[tokio::test]
async fn send_uses_draft_from_identity() {
    let server = server().await;
    // The account address is the first identity, so only `draft.from` can
    // select the second.
    identities(
        &server,
        json!([
            { "id": "i1", "name": "Me", "email": "me@example.com" },
            { "id": "i2", "name": "Shop", "email": "Shop@Example.com" },
        ]),
    )
    .await;
    mount(
        &server,
        "EmailSubmission/set",
        submit_responses(json!({ "updated": { "e-new": null } })),
        1,
    )
    .await;

    let mut d = draft();
    d.from = Some(Address {
        name: None,
        email: "shop@example.com".into(),
    });

    send(&cfg(), &store(), &client(&server).await, "a", &d)
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let body: Value = requests
        .iter()
        .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
        .find(|b| b["methodCalls"][1][0] == "EmailSubmission/set")
        .expect("no send request recorded");
    let calls = &body["methodCalls"];

    assert_eq!(
        calls[0][1]["create"]["d"]["from"][0]["email"],
        "Shop@Example.com"
    );
    assert_eq!(calls[1][1]["create"]["s"]["identityId"], "i2");
}

#[tokio::test]
async fn send_rejects_unknown_from() {
    let server = server().await;
    identities(&server, two_identities()).await;
    mount(&server, "Email/set", json!([]), 0).await;

    let mut d = draft();
    d.from = Some(Address {
        name: None,
        email: "nobody@x.io".into(),
    });

    let err = send(&cfg(), &store(), &client(&server).await, "a", &d)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, Error::NotConfigured(m) if m.contains("nobody@x.io")),
        "{err:?}"
    );
}

#[tokio::test]
async fn send_rejects_responses_with_wrong_call_ids() {
    let created = json!({ "created": { "d": { "id": "e-new" } } });
    let submitted = json!({ "created": { "s": { "id": "sub1" } } });
    let moved = json!({ "updated": { "e-new": null } });
    let cases = [
        // (name, responses, expected Jmap message or None for Error::Other)
        (
            "create under the wrong id",
            json!([
                ["Email/set", created, "9"],
                ["EmailSubmission/set", submitted, "1"],
                ["Email/set", moved, "1"],
            ]),
            Some("no create response"),
        ),
        (
            "submission under the wrong id",
            json!([
                ["Email/set", created, "0"],
                ["EmailSubmission/set", submitted, "9"],
                ["Email/set", moved, "1"],
            ]),
            Some("no submission response"),
        ),
        (
            "move to Sent under the wrong id",
            json!([
                ["Email/set", created, "0"],
                ["EmailSubmission/set", submitted, "1"],
                ["Email/set", moved, "9"],
            ]),
            None,
        ),
    ];

    for (name, responses, jmap_message) in cases {
        let server = server().await;
        identities(&server, two_identities()).await;
        mount(&server, "EmailSubmission/set", responses, 1).await;

        let result = send(&cfg(), &store(), &client(&server).await, "a", &draft()).await;

        assert!(result.is_err(), "{name}: accepted as {result:?}");

        let err = result.unwrap_err();

        match jmap_message {
            Some(message) => assert!(
                matches!(&err, Error::Jmap(m) if m == message),
                "{name}: {err:?}"
            ),
            None => assert!(
                matches!(&err, Error::Other(m) if m.contains("e-new") && m.contains("Sent")),
                "{name}: {err:?}"
            ),
        }
    }
}

#[tokio::test]
async fn send_reports_not_created() {
    let server = server().await;
    identities(&server, two_identities()).await;
    mount(
        &server,
        "EmailSubmission/set",
        json!([[
            "Email/set",
            { "notCreated": { "d": { "type": "invalidProperties", "description": "bad to" } } },
            "0"
        ]]),
        1,
    )
    .await;

    let err = send(&cfg(), &store(), &client(&server).await, "a", &draft())
        .await
        .unwrap_err();

    assert!(
        matches!(&err, Error::Jmap(m) if m.contains("invalidProperties")),
        "{err:?}"
    );
}

#[tokio::test]
async fn send_marks_replied_answered() {
    let server = server().await;
    let store = store();
    store
        .upsert_emails(&[EmailSummary {
            account_id: "a".into(),
            id: "e1".into(),
            thread_id: "t1".into(),
            received_at: "2026-10-01T10:00:00Z".into(),
            from: vec![],
            to: vec![],
            cc: vec![],
            subject: "Orig".into(),
            preview: String::new(),
            size: 1,
            has_attachment: false,
            mailbox_ids: vec!["mb-inbox".into()],
            keywords: vec!["$seen".into()],
        }])
        .unwrap();
    identities(&server, two_identities()).await;
    mount(
        &server,
        "EmailSubmission/set",
        submit_responses(json!({ "updated": { "e-new": null } })),
        1,
    )
    .await;
    mount(
        &server,
        r#""keywords/$answered":true"#,
        json!([["Email/set", { "updated": { "e1": null } }, "0"]]),
        1,
    )
    .await;

    let draft = Draft {
        reply_to_email_id: Some("e1".into()),
        ..draft()
    };
    send(&cfg(), &store, &client(&server).await, "a", &draft)
        .await
        .unwrap();

    let keywords = store.email("a", "e1").unwrap().unwrap().keywords;
    assert!(keywords.contains(&"$answered".to_string()), "{keywords:?}");
}

#[tokio::test]
async fn send_without_identity_fails() {
    let server = server().await;
    identities(&server, json!([])).await;

    let err = send(&cfg(), &store(), &client(&server).await, "a", &draft())
        .await
        .unwrap_err();

    assert!(matches!(err, Error::NotConfigured(_)), "{err:?}");
}

#[tokio::test]
async fn send_reports_move_to_sent_failure() {
    let cases = [
        // (i) the implicit Email/set refuses the update
        submit_responses(json!({ "notUpdated": { "e-new": { "type": "notFound" } } })),
        // (ii) the implicit Email/set is missing
        json!([
            ["Email/set", { "created": { "d": { "id": "e-new" } } }, "0"],
            ["EmailSubmission/set", { "created": { "s": { "id": "sub1" } } }, "1"],
        ]),
    ];

    for responses in cases {
        let server = server().await;
        identities(&server, two_identities()).await;
        mount(&server, "EmailSubmission/set", responses, 1).await;

        let err = send(&cfg(), &store(), &client(&server).await, "a", &draft())
            .await
            .unwrap_err();

        assert!(
            matches!(&err, Error::Other(m) if m.contains("e-new") && m.contains("Sent")),
            "{err:?}"
        );
    }
}
