//! Integration tests of the classifier against wiremock fakes of Jev and JMAP.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use posto_core::classify::{
    self, Candidate, ClassifyReport, JevClient, Tray, backfill_candidates, backfill_ids, correct,
    run_with_lock,
};
use posto_core::config::{Account, ClassifyConfig, Config};
use posto_core::jmap::Client;
use posto_core::provider::Connection;
use posto_core::store::{Address, Classified, EmailSummary, Mailbox, MsgRef, Store};
use posto_core::{Error, Result};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SESSION: &str = include_str!("fixtures/session.json");
const KEY: &str = "sk-test-key";

// ── Fake Jev ────────────────────────────────────────────────────────────────

/// Answers each request with the next queued reply, then with the fallback.
struct Script {
    queue: Arc<Mutex<VecDeque<ResponseTemplate>>>,
    fallback: ResponseTemplate,
}

impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone())
    }
}

/// A 200 System One response choosing `tray`: sure (0.9 vs 0.05) or unsure
/// (0.5 vs 0.4); `score` is the raw urgency score; no action required.
fn answer(tray: &str, unsure: bool, score: f64) -> ResponseTemplate {
    answer_acting(tray, unsure, score, 0.1)
}

/// [`answer`] with `action` as the `action_required` probability.
fn answer_acting(tray: &str, unsure: bool, score: f64, action: f64) -> ResponseTemplate {
    let (top, second) = if unsure { (0.5, 0.4) } else { (0.9, 0.05) };
    let other = if tray == "updates" {
        "social"
    } else {
        "updates"
    };

    ResponseTemplate::new(200).set_body_json(json!({
        "model": "jev-test-1",
        "answers": {
            "category": {
                "choice": tray,
                "probabilities": { tray: top, other: second },
            },
            "urgency": { "score": score },
            "is_personal": { "noul": 0.9 },
            "action_required": { "noul": action },
        },
        "usage": { "input_tokens": 123 },
    }))
}

/// A fake Jev server with a scripted queue of replies.
struct FakeJev {
    server: MockServer,
    queue: Arc<Mutex<VecDeque<ResponseTemplate>>>,
}

impl FakeJev {
    async fn start(fallback: ResponseTemplate) -> FakeJev {
        let server = MockServer::start().await;
        let queue = Arc::new(Mutex::new(VecDeque::new()));

        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(Script {
                queue: queue.clone(),
                fallback,
            })
            .mount(&server)
            .await;

        FakeJev { server, queue }
    }

    fn push(&self, replies: impl IntoIterator<Item = ResponseTemplate>) {
        self.queue.lock().unwrap().extend(replies);
    }

    fn client(&self) -> JevClient {
        JevClient::new(&self.server.uri(), KEY, &legacy_trays()).unwrap()
    }

    /// The JSON bodies of the requests received so far.
    async fn bodies(&self) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    /// The `state` strings sent so far.
    async fn states(&self) -> Vec<String> {
        self.bodies()
            .await
            .iter()
            .map(|b| b["state"].as_str().unwrap().to_string())
            .collect()
    }
}

// ── Fake JMAP ───────────────────────────────────────────────────────────────

/// Behaviour switches of the fake JMAP server.
#[derive(Default)]
struct JmapState {
    /// Answer every `Email/set` with `notUpdated`.
    refuse_moves: AtomicBool,
    /// Ids whose `List-Unsubscribe` header is set.
    unsubscribe: Mutex<HashSet<String>>,
}

struct FakeJmap(Arc<JmapState>);

/// The mailbox id the fake gives a created mailbox.
fn mb_id(name: &str) -> String {
    format!("mb-{}", name.to_lowercase().replace(' ', "-"))
}

impl Respond for FakeJmap {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let call = &body["methodCalls"][0];
        let (name, args) = (call[0].as_str().unwrap(), &call[1]);
        let response = match name {
            "Mailbox/set" => {
                let created = args["create"]["c"]["name"]
                    .as_str()
                    .map(|n| json!({ "c": { "id": mb_id(n) } }))
                    .unwrap_or_default();

                json!({ "created": created })
            }
            "Email/set" => {
                let ids: Vec<&String> = args["update"].as_object().unwrap().keys().collect();

                if self.0.refuse_moves.load(Ordering::SeqCst) {
                    let refused: serde_json::Map<String, Value> = ids
                        .into_iter()
                        .map(|id| (id.clone(), json!({ "type": "forbidden" })))
                        .collect();

                    json!({ "notUpdated": refused })
                } else {
                    let done: serde_json::Map<String, Value> = ids
                        .into_iter()
                        .map(|id| (id.clone(), Value::Null))
                        .collect();

                    json!({ "updated": done })
                }
            }
            "Email/get" => {
                let id = args["ids"][0].as_str().unwrap();
                let wants_header = args["properties"][0] == "header:List-Unsubscribe:asText";

                if wants_header {
                    let header = self
                        .0
                        .unsubscribe
                        .lock()
                        .unwrap()
                        .contains(id)
                        .then_some("<mailto:unsub@x.example>");

                    json!({ "list": [{ "id": id, "header:List-Unsubscribe:asText": header }] })
                } else {
                    json!({ "list": [{
                        "id": id,
                        "textBody": [{ "partId": "1", "type": "text/plain" }],
                        "htmlBody": [],
                        "bodyValues": { "1": { "value": format!("Body of {id}") } },
                        "attachments": [],
                    }] })
                }
            }
            other => panic!("unexpected JMAP call {other}"),
        };

        ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [[name, response, "0"]],
        }))
    }
}

async fn jmap_server(state: Arc<JmapState>) -> MockServer {
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
    Mock::given(method("POST"))
        .and(path("/api"))
        .respond_with(FakeJmap(state))
        .mount(&server)
        .await;

    server
}

/// Every method call the JMAP server received, as `(name, arguments)`.
async fn calls(server: &MockServer) -> Vec<(String, Value)> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api")
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();

            (
                body["methodCalls"][0][0].as_str().unwrap().to_string(),
                body["methodCalls"][0][1].clone(),
            )
        })
        .collect()
}

/// The `Email/set` patches sent for `id`, in order.
async fn patches(server: &MockServer, id: &str) -> Vec<Value> {
    calls(server)
        .await
        .into_iter()
        .filter(|(name, _)| name == "Email/set")
        .filter_map(|(_, args)| args["update"].get(id).cloned())
        .collect()
}

/// Names passed to `Mailbox/set` creates, in order.
async fn created(server: &MockServer) -> Vec<String> {
    calls(server)
        .await
        .into_iter()
        .filter(|(name, _)| name == "Mailbox/set")
        .map(|(_, args)| args["create"]["c"]["name"].as_str().unwrap().to_string())
        .collect()
}

// ── Environment ─────────────────────────────────────────────────────────────

struct Env {
    jmap: MockServer,
    jmap_state: Arc<JmapState>,
    jev: FakeJev,
    store: Store,
    cfg: Config,
    clients: HashMap<String, Arc<Connection>>,
    lock: PathBuf,
    _dir: tempfile::TempDir,
}

fn mailbox(account: &str, id: &str, name: &str, role: Option<&str>) -> Mailbox {
    Mailbox {
        account_id: account.into(),
        id: id.into(),
        name: name.into(),
        role: role.map(String::from),
        parent_id: None,
        sort_order: 1,
        total: 0,
        unread: 0,
    }
}

fn email(account: &str, id: &str, mailboxes: &[&str]) -> EmailSummary {
    EmailSummary {
        account_id: account.into(),
        id: id.into(),
        thread_id: format!("t-{id}"),
        received_at: "2026-10-01T10:00:00Z".into(),
        from: vec![Address {
            name: Some("Ann".into()),
            email: "Ann@Example.com".into(),
        }],
        to: vec![],
        cc: vec![],
        subject: format!("S{id}"),
        preview: "hello".into(),
        size: 10,
        has_attachment: false,
        mailbox_ids: mailboxes.iter().map(|s| s.to_string()).collect(),
        keywords: vec![],
    }
}

/// Classifier settings with the four trays configs written before the
/// current defaults carry: these tests double as a check that such a config
/// keeps working.
fn legacy_trays() -> ClassifyConfig {
    ClassifyConfig {
        keep_in_inbox: vec!["needs_reply".into(), "updates".into()],
        trays: [
            ("needs_reply", "Needs reply"),
            ("updates", "Updates"),
            ("promotional", "Promos"),
            ("social", "Social"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        ..ClassifyConfig::default()
    }
}

/// An environment with one JMAP and one Jev fake; every `(slug, classify)`
/// becomes an account connected to the JMAP fake, with an inbox, a junk
/// mailbox and a custom "Projects" mailbox.
async fn env_with(accounts: &[(&str, Option<bool>)], fallback: ResponseTemplate) -> Env {
    let jmap_state = Arc::new(JmapState::default());
    let jmap = jmap_server(jmap_state.clone()).await;
    let jev = FakeJev::start(fallback).await;
    let store = Store::open_memory().unwrap();
    let mut cfg = Config {
        accounts: accounts
            .iter()
            .map(|(id, classify)| Account {
                classify: *classify,
                ..Account::new(id, "me@example.com")
            })
            .collect(),
        classify: legacy_trays(),
        ..Config::default()
    };
    let mut clients = HashMap::new();

    cfg.classify.max_per_run = 50;

    for (id, _) in accounts {
        store
            .upsert_mailboxes(&[
                mailbox(id, "mb-inbox", "Inbox", Some("inbox")),
                mailbox(id, "mb-junk", "Junk", Some("junk")),
                mailbox(id, "mb-projects", "Projects", None),
            ])
            .unwrap();
        clients.insert(
            id.to_string(),
            Arc::new(Connection::Jmap(
                Client::connect(&format!("{}/session", jmap.uri()), "tok")
                    .await
                    .unwrap(),
            )),
        );
    }

    let dir = tempfile::tempdir().unwrap();

    Env {
        jmap,
        jmap_state,
        jev,
        store,
        cfg,
        clients,
        lock: dir.path().join("classify.lock"),
        _dir: dir,
    }
}

/// One classifying account `a` and a fallback of sure `updates`.
async fn env() -> Env {
    env_with(&[("a", Some(true))], answer("updates", false, 0.0)).await
}

impl Env {
    /// Stores `id` in `mailboxes` of `account` and queues it.
    fn queue(&self, account: &str, id: &str, mailboxes: &[&str]) {
        self.store
            .upsert_emails(&[email(account, id, mailboxes)])
            .unwrap();
        self.store
            .enqueue_for_classification(account, &[id.to_string()])
            .unwrap();
    }

    async fn run(&self, limit: Option<usize>) -> ClassifyReport {
        run_with_lock(
            &self.lock,
            &self.cfg,
            &self.store,
            &self.clients,
            &self.jev.client(),
            limit,
        )
        .await
    }

    fn mailboxes_of(&self, account: &str, id: &str) -> Vec<String> {
        let mut ids = self.store.email(account, id).unwrap().unwrap().mailbox_ids;

        ids.sort();

        ids
    }
}

fn sorted(ids: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = ids.iter().map(|s| s.to_string()).collect();

    v.sort();

    v
}

// ── Jev client ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn request_matches_jevmail_shape() -> Result<()> {
    let jev = FakeJev::start(answer("social", false, 3.4)).await;
    let c = classify::Classifier::classify(&jev.client(), "State text").await?;
    let requests = jev.server.received_requests().await.unwrap();

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/v1/systemone");
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        format!("Bearer {KEY}")
    );

    let body = &jev.bodies().await[0];
    let q = &body["questions"];

    assert_eq!(body["state"], "State text");
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(q["category"]["type"], "choice");
    assert_eq!(q["urgency"]["type"], "score");
    assert_eq!(q["is_personal"]["type"], "noul");

    let mut keys: Vec<&String> = q["category"]["criteria"]
        .as_object()
        .unwrap()
        .keys()
        .collect();

    keys.sort();
    assert_eq!(
        keys,
        ["needs_reply", "promotional", "social", "spam", "updates"]
    );

    assert_eq!(c.tray, Tray::new("social"));
    assert_eq!(c.urgency, 4);
    assert_eq!((c.model.as_str(), c.input_tokens), ("jev-test-1", 123));
    assert!(!c.unsure);

    Ok(())
}

#[tokio::test]
async fn retries_on_429_then_succeeds() -> Result<()> {
    let jev = FakeJev::start(answer("updates", false, 0.0)).await;

    jev.push([
        ResponseTemplate::new(429).insert_header("Retry-After", "0"),
        answer("promotional", false, 0.0),
    ]);

    let c = classify::Classifier::classify(&jev.client(), "s").await;

    assert_eq!(c.map(|c| c.tray).ok(), Some(Tray::new("promotional")));
    assert_eq!(jev.bodies().await.len(), 2);

    Ok(())
}

#[tokio::test]
async fn gives_up_after_five_requests() {
    let jev = FakeJev::start(ResponseTemplate::new(529).insert_header("Retry-After", "0")).await;
    let err = classify::Classifier::classify(&jev.client(), "s")
        .await
        .unwrap_err();

    assert!(
        matches!(&err, Error::Http(m) if m.contains("status 529")),
        "{err:?}"
    );
    assert_eq!(jev.bodies().await.len(), 5);
}

#[tokio::test]
async fn http_errors_are_cut_and_key_free() {
    let body = format!("{KEY}{}", "x".repeat(500));
    let jev = FakeJev::start(ResponseTemplate::new(500).set_body_string(body)).await;
    let err = classify::Classifier::classify(&jev.client(), "s")
        .await
        .unwrap_err()
        .to_string();

    // A server error is not retried.
    assert_eq!(jev.bodies().await.len(), 1);
    assert!(err.contains("status 500"), "{err}");
    assert!(!err.contains(KEY), "{err}");
    assert!(err.len() < 300, "{} chars", err.len());
}

#[tokio::test]
async fn undecodable_and_incomplete_answers_are_errors() {
    for (reply, wanted) in [
        (
            ResponseTemplate::new(200).set_body_string("not json"),
            "decode",
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({ "answers": {} })),
            "missing answer",
        ),
    ] {
        let jev = FakeJev::start(reply).await;
        let err = classify::Classifier::classify(&jev.client(), "s")
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains(wanted), "{err}");
    }
}

#[tokio::test]
async fn a_configured_tray_is_offered_and_accepted() -> Result<()> {
    let jev = FakeJev::start(answer("bills", false, 0.0)).await;
    let mut settings = ClassifyConfig::default();

    settings.trays.insert("bills".into(), "Bills".into());
    settings
        .criteria
        .insert("bills".into(), "Bills the recipient has to pay.".into());

    let client = JevClient::new(&jev.server.uri(), KEY, &settings)?;
    let c = classify::Classifier::classify(&client, "s").await?;

    assert_eq!(c.tray, Tray::new("bills"));
    assert_eq!(
        jev.bodies().await[0]["questions"]["category"]["criteria"]["bills"],
        "Bills the recipient has to pay."
    );

    // Without a description the client refuses to start.
    settings.criteria.clear();

    assert!(matches!(
        JevClient::new(&jev.server.uri(), KEY, &settings),
        Err(Error::Config(_))
    ));

    Ok(())
}

#[tokio::test]
async fn unknown_choice_error_does_not_echo_the_server() {
    let jev = FakeJev::start(ResponseTemplate::new(200).set_body_json(json!({
        "model": "m",
        "answers": {
            "category": { "choice": KEY },
            "urgency": { "score": 1.0 },
            "is_personal": { "noul": 0.1 },
            "action_required": { "noul": 0.1 },
        },
    })))
    .await;
    let err = classify::Classifier::classify(&jev.client(), "s")
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("unknown choice"), "{err}");
    assert!(!err.contains(KEY), "{err}");
}

// ── Mailbox creation ────────────────────────────────────────────────────────

#[tokio::test]
async fn create_mailbox_reconstructs_minimal_response() -> Result<()> {
    let jmap = jmap_server(Arc::new(JmapState::default())).await;
    let client = Client::connect(&format!("{}/session", jmap.uri()), "tok").await?;
    let mailbox = client.create_mailbox("Needs reply").await?;

    // The fake returns only `created: {c: {id}}`.
    assert_eq!(mailbox.id, "mb-needs-reply");
    assert_eq!(mailbox.name, "Needs reply");
    assert_eq!(mailbox.role, None);
    assert_eq!(mailbox.parent_id, None);
    assert_eq!(mailbox.account_id, "");
    assert_eq!(
        (mailbox.total, mailbox.unread, mailbox.sort_order),
        (0, 0, 0)
    );

    let (name, args) = calls(&jmap).await.remove(0);

    assert_eq!(name, "Mailbox/set");
    assert_eq!(args["accountId"], "acc1");
    assert_eq!(
        args["create"],
        json!({ "c": { "name": "Needs reply", "parentId": null } })
    );

    Ok(())
}

#[tokio::test]
async fn create_mailbox_refusal_is_an_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/session"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(SESSION.replace("{{base}}", &server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [["Mailbox/set", {
                "notCreated": { "c": { "type": "invalidProperties", "description": "name taken" } },
            }, "0"]],
        })))
        .mount(&server)
        .await;

    let client = Client::connect(&format!("{}/session", server.uri()), "tok")
        .await
        .unwrap();
    let err = client.create_mailbox("Updates").await.unwrap_err();

    assert!(
        matches!(&err, Error::Jmap(m) if m.contains("invalidProperties") && m.contains("name taken")),
        "{err:?}"
    );
}

// ── run ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_moves_flags_and_records() -> Result<()> {
    let env = env().await;

    // e1 sits in the inbox and a custom mailbox; e2 is unsure but urgent.
    env.queue("a", "e1", &["mb-inbox", "mb-projects"]);
    env.queue("a", "e2", &["mb-inbox"]);
    env.jev.push([
        answer("needs_reply", false, 3.6),
        answer("needs_reply", true, 3.0),
    ]);

    let report = env.run(None).await;

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(
        (
            report.classified,
            report.moved,
            report.flagged,
            report.unsure
        ),
        (2, 2, 2, 1)
    );
    assert_eq!(report.input_tokens, 246);

    // The missing tray and unsure mailboxes were created once each.
    assert_eq!(created(&env.jmap).await, ["Needs reply", "Unsure"]);

    let tray = env
        .store
        .top_level_mailbox_by_name("a", "Needs reply")?
        .unwrap();

    assert_eq!(
        (tray.id.as_str(), tray.account_id.as_str()),
        ("mb-needs-reply", "a")
    );

    // e1: one patch adds the tray mailbox and touches nothing else.
    let p = patches(&env.jmap, "e1").await;

    assert_eq!(p.len(), 2, "tray patch and flag patch: {p:?}");
    assert_eq!(p[0], json!({ "mailboxIds/mb-needs-reply": true }));
    assert_eq!(p[1], json!({ "keywords/$flagged": true }));
    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&["mb-inbox", "mb-needs-reply", "mb-projects"])
    );
    assert!(env.store.email("a", "e1")?.unwrap().is_flagged());

    let row = env.store.classification("a", "e1")?.unwrap();

    assert_eq!(
        (row.tray.as_str(), row.unsure, row.applied),
        ("needs_reply", false, true)
    );
    assert_eq!((row.urgency, row.model.as_str()), (5, "jev-test-1"));
    assert_eq!(row.apply_error, None);
    assert_eq!(env.store.classify_queue_len("a")?, 0);

    // e2: unsure, so it also joins Unsure, keeps the inbox, and is still flagged.
    let p = patches(&env.jmap, "e2").await;

    assert_eq!(
        p[0],
        json!({ "mailboxIds/mb-needs-reply": true, "mailboxIds/mb-unsure": true })
    );
    assert_eq!(p[1], json!({ "keywords/$flagged": true }));
    assert_eq!(
        env.mailboxes_of("a", "e2"),
        sorted(&["mb-inbox", "mb-needs-reply", "mb-unsure"])
    );
    assert!(env.store.email("a", "e2")?.unwrap().is_flagged());
    assert!(env.store.classification("a", "e2")?.unwrap().unsure);

    Ok(())
}

#[tokio::test]
async fn run_sends_thread_and_header_facts_to_jev() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.queue("a", "e2", &["mb-inbox"]);

    // e1's thread holds an earlier message from the account's own address.
    let mut mine = email("a", "mine", &["mb-inbox"]);

    mine.thread_id = "t-e1".into();
    mine.from = vec![Address {
        name: None,
        email: "ME@example.com".into(),
    }];
    env.store.upsert_emails(&[mine])?;
    env.jmap_state
        .unsubscribe
        .lock()
        .unwrap()
        .insert("e2".into());

    env.run(None).await;

    let states = env.jev.states().await;

    assert_eq!(
        states[0],
        "From: Ann <ann@example.com>\n\
         Subject: Se1\n\
         Received: 2026-10-01T10:00:00Z\n\
         Has unsubscribe header: no\n\
         User has previously replied in this thread: yes\n\
         \n\
         Body:\n\
         Body of e1"
    );
    assert!(
        states[1].contains("Has unsubscribe header: yes"),
        "{}",
        states[1]
    );
    assert!(
        states[1].contains("replied in this thread: no"),
        "{}",
        states[1]
    );

    Ok(())
}

#[tokio::test]
async fn run_unsure_keeps_inbox() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.jev.push([answer("promotional", true, 3.5)]);

    let report = env.run(None).await;

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!((report.unsure, report.moved, report.flagged), (1, 1, 0));

    let p = patches(&env.jmap, "e1").await;

    assert_eq!(p.len(), 1, "no flag patch: {p:?}");
    assert_eq!(
        p[0],
        json!({ "mailboxIds/mb-promos": true, "mailboxIds/mb-unsure": true })
    );
    assert!(p[0].get("mailboxIds/mb-inbox").is_none());
    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&["mb-inbox", "mb-promos", "mb-unsure"])
    );
    assert!(env.store.classification("a", "e1")?.unwrap().unsure);

    Ok(())
}

#[tokio::test]
async fn run_spam_goes_to_junk() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.queue("a", "e2", &["mb-inbox"]);
    env.jev
        .push([answer("spam", false, 0.0), answer("spam", true, 0.0)]);

    let report = env.run(None).await;

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!((report.moved, report.unsure), (2, 1));

    // Sure spam: add junk, leave the inbox, no tray mailbox exists for spam.
    assert_eq!(
        patches(&env.jmap, "e1").await,
        [json!({ "mailboxIds/mb-junk": true, "mailboxIds/mb-inbox": null })]
    );
    assert_eq!(env.mailboxes_of("a", "e1"), ["mb-junk"]);

    // Unsure spam: no junk, no inbox removal, only the Unsure mailbox is added.
    assert_eq!(
        patches(&env.jmap, "e2").await,
        [json!({ "mailboxIds/mb-unsure": true })]
    );
    assert_eq!(
        env.mailboxes_of("a", "e2"),
        sorted(&["mb-inbox", "mb-unsure"])
    );
    assert_eq!(created(&env.jmap).await, ["Unsure"]);

    Ok(())
}

#[tokio::test]
async fn run_keep_in_inbox_rule() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.queue("a", "e2", &["mb-inbox"]);
    env.jev.push([
        answer("promotional", false, 0.0),
        answer("updates", false, 0.0),
    ]);

    env.run(None).await;

    // Promotional is not in keep_in_inbox: it leaves the inbox.
    assert_eq!(
        patches(&env.jmap, "e1").await,
        [json!({ "mailboxIds/mb-promos": true, "mailboxIds/mb-inbox": null })]
    );
    assert_eq!(env.mailboxes_of("a", "e1"), ["mb-promos"]);

    // Updates is: the inbox membership is left alone.
    assert_eq!(
        patches(&env.jmap, "e2").await,
        [json!({ "mailboxIds/mb-updates": true })]
    );
    assert_eq!(
        env.mailboxes_of("a", "e2"),
        sorted(&["mb-inbox", "mb-updates"])
    );

    Ok(())
}

#[tokio::test]
async fn run_files_a_remembered_sender_without_asking_jev() -> Result<()> {
    let env = env().await;

    // Every fixture email is from Ann. Correcting one email twice counts once.
    env.queue("a", "e1", &["mb-inbox"]);
    env.queue("a", "e2", &["mb-inbox"]);
    env.store.set_correction("a", "e1", "promotional")?;
    env.store.set_correction("a", "e1", "promotional")?;

    assert_eq!(env.store.remembered_tray("ann@example.com")?, None);

    env.store.set_correction("a", "e2", "promotional")?;

    assert_eq!(
        env.store.remembered_tray("ANN@example.com")?.as_deref(),
        Some("promotional")
    );

    env.queue("a", "e3", &["mb-inbox"]);

    let report = env.run(None).await;

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!((report.classified, report.remembered), (1, 1));
    assert_eq!(report.input_tokens, 0);
    assert!(env.jev.bodies().await.is_empty());
    assert_eq!(
        patches(&env.jmap, "e3").await,
        [json!({ "mailboxIds/mb-promos": true, "mailboxIds/mb-inbox": null })]
    );

    // A correction to another tray starts the streak again.
    env.store.set_correction("a", "e3", "social")?;

    assert_eq!(env.store.remembered_tray("ann@example.com")?, None);

    // Needs-reply streaks are kept but never answer for Jev.
    env.store.set_correction("a", "e1", "needs_reply")?;
    env.store.set_correction("a", "e2", "needs_reply")?;
    env.queue("a", "e4", &["mb-inbox"]);
    env.jev.push([answer("updates", false, 0.0)]);

    let report = env.run(None).await;

    assert_eq!((report.classified, report.remembered), (1, 0));
    assert_eq!(env.jev.bodies().await.len(), 1);

    Ok(())
}

#[tokio::test]
async fn run_action_required_stays_in_inbox_and_flags_when_urgent() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.queue("a", "e2", &["mb-inbox"]);
    env.queue("a", "e3", &["mb-inbox"]);
    env.jev.push([
        // A bill filed as promotional: kept in the inbox and flagged.
        answer_acting("promotional", false, 3.0, 0.9),
        // Action but not urgent: kept, not flagged.
        answer_acting("promotional", false, 0.0, 0.9),
        // Spam ignores the action answer.
        answer_acting("spam", false, 3.0, 0.9),
    ]);

    let report = env.run(None).await;

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.flagged, 1);
    assert_eq!(
        patches(&env.jmap, "e1").await,
        [
            json!({ "mailboxIds/mb-promos": true }),
            json!({ "keywords/$flagged": true })
        ]
    );
    assert_eq!(
        patches(&env.jmap, "e2").await,
        [json!({ "mailboxIds/mb-promos": true })]
    );
    assert_eq!(
        patches(&env.jmap, "e3").await,
        [json!({ "mailboxIds/mb-junk": true, "mailboxIds/mb-inbox": null })]
    );

    Ok(())
}

#[tokio::test]
async fn run_drains_queue_with_cap_and_retries() -> Result<()> {
    let mut env = env().await;

    env.cfg.classify.max_per_run = 1;

    for id in ["e1", "e2", "e3"] {
        env.queue("a", id, &["mb-inbox"]);
    }

    // Run 1: the cap lets one Jev request through.
    let report = env.run(None).await;

    assert_eq!((report.classified, report.moved), (1, 1));
    assert_eq!(env.jev.bodies().await.len(), 1);
    assert_eq!(env.store.classify_queue_len("a")?, 2);

    // Run 2: the server refuses the move for e2.
    env.jmap_state.refuse_moves.store(true, Ordering::SeqCst);

    let report = env.run(None).await;

    assert_eq!((report.classified, report.moved), (1, 0));
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(
        report.errors[0].starts_with("a/e2: "),
        "{:?}",
        report.errors
    );
    assert_eq!(env.jev.bodies().await.len(), 2);

    let row = env.store.classification("a", "e2")?.unwrap();

    assert!(!row.applied);
    assert!(row.apply_error.is_some());

    let queued = env.store.classify_queue(Some("a"))?;
    let e2 = queued.iter().find(|r| r.email_id == "e2").unwrap();

    assert_eq!(queued.len(), 2);
    assert_eq!(e2.attempts, 1);
    assert!(e2.last_error.is_some());

    // Run 3: the server accepts again. e2 is applied without a Jev request;
    // the single new request is for e3.
    env.jmap_state.refuse_moves.store(false, Ordering::SeqCst);

    let report = env.run(None).await;

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!((report.classified, report.moved), (1, 2));

    let row = env.store.classification("a", "e2")?.unwrap();

    assert!(row.applied);
    assert_eq!(row.apply_error, None);
    assert_eq!(env.store.classify_queue_len("a")?, 0);

    let states = env.jev.states().await;

    assert_eq!(states.len(), 3);
    assert_eq!(
        states.iter().filter(|s| s.contains("Subject: Se2")).count(),
        1
    );

    Ok(())
}

#[tokio::test]
async fn run_gives_up_after_three_attempts() -> Result<()> {
    let env = env_with(
        &[("a", Some(true))],
        ResponseTemplate::new(500).set_body_string("boom"),
    )
    .await;

    env.queue("a", "e1", &["mb-inbox"]);

    for attempt in 1..=3 {
        let report = env.run(None).await;

        assert_eq!(report.errors.len(), 1, "run {attempt}: {:?}", report.errors);
        assert_eq!(env.store.classify_queue(None)?[0].attempts, attempt);
    }

    let row = env.store.classify_queue(None)?.remove(0);

    assert!(
        row.last_error.as_deref().unwrap().contains("status 500"),
        "{row:?}"
    );
    assert_eq!(env.jev.bodies().await.len(), 3);

    // A fourth run leaves the failed row alone: no request, no error.
    let report = env.run(None).await;

    assert_eq!(report, ClassifyReport::default());
    assert_eq!(env.jev.bodies().await.len(), 3);
    assert_eq!(env.store.classify_queue_len("a")?, 1);

    // Resetting gives it another go.
    assert_eq!(env.store.reset_failed_classifications(None)?, 1);
    env.run(None).await;
    assert_eq!(env.jev.bodies().await.len(), 4);

    Ok(())
}

#[tokio::test]
async fn run_stops_retrying_refused_moves_after_three_attempts() -> Result<()> {
    let env = env().await;

    env.jmap_state.refuse_moves.store(true, Ordering::SeqCst);
    env.queue("a", "e1", &["mb-inbox"]);

    for _ in 0..4 {
        env.run(None).await;
    }

    // One Jev request in all; the answer was kept and retried twice more.
    assert_eq!(env.jev.bodies().await.len(), 1);

    let row = env.store.classification("a", "e1")?.unwrap();

    assert!(!row.applied);
    assert!(row.apply_error.is_some());
    assert_eq!(env.store.classify_queue(None)?[0].attempts, 3);

    let refused = patches(&env.jmap, "e1").await.len();

    env.run(None).await;
    assert_eq!(
        patches(&env.jmap, "e1").await.len(),
        refused,
        "a fourth retry was sent"
    );

    Ok(())
}

#[tokio::test]
async fn run_isolates_errors() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.queue("a", "e2", &["mb-inbox"]);
    env.jev.push([
        ResponseTemplate::new(500).set_body_string("down"),
        answer("social", false, 0.0),
    ]);

    let report = env.run(None).await;

    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(
        report.errors[0].starts_with("a/e1: "),
        "{:?}",
        report.errors
    );
    assert_eq!(report.classified, 1);
    assert!(env.store.classification("a", "e1")?.is_none());
    assert!(env.store.classification("a", "e2")?.is_some());
    assert!(env.store.classify_queue(None)?[0].last_error.is_some());

    Ok(())
}

#[tokio::test]
async fn run_refuses_when_locked() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);

    let mut held = fd_lock::RwLock::new(std::fs::File::create(&env.lock)?);
    let guard = held.try_write()?;
    let report = env.run(None).await;

    assert_eq!(report.errors, ["classifier already running"]);
    assert_eq!((report.classified, report.moved, report.skipped), (0, 0, 0));
    assert_eq!(env.jev.bodies().await.len(), 0);
    assert_eq!(env.store.classify_queue_len("a")?, 1);

    drop(guard);

    // Once released, the same run goes through.
    assert_eq!(env.run(None).await.classified, 1);

    Ok(())
}

#[tokio::test]
async fn run_limit_skips_ineligible_rows() -> Result<()> {
    let mut env = env_with(
        &[("a", Some(false)), ("b", Some(true)), ("c", None)],
        answer("updates", false, 0.0),
    )
    .await;

    // The old row belongs to a disabled account; "c" follows the (off) global switch.
    env.queue("a", "old", &["mb-inbox"]);
    env.queue("c", "also-off", &["mb-inbox"]);
    env.queue("b", "new", &["mb-inbox"]);
    env.cfg.classify.enabled = false;

    let report = env.run(Some(1)).await;

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!((report.classified, report.moved, report.skipped), (1, 1, 2));
    assert!(env.store.classification("b", "new")?.is_some());
    assert_eq!(env.jev.bodies().await.len(), 1);

    // The skipped rows are untouched.
    let left = env.store.classify_queue(None)?;

    assert_eq!(left.len(), 2);
    assert!(
        left.iter()
            .all(|r| r.attempts == 0 && r.last_error.is_none())
    );

    // Turning the global switch on makes "c" eligible too.
    env.cfg.classify.enabled = true;

    let report = env.run(None).await;

    assert_eq!((report.classified, report.skipped), (1, 1));
    assert!(env.store.classification("c", "also-off")?.is_some());

    Ok(())
}

#[tokio::test]
async fn run_skips_accounts_without_a_client() -> Result<()> {
    let mut env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.clients.clear();

    let report = env.run(None).await;

    assert_eq!((report.classified, report.skipped), (0, 1));
    assert_eq!(env.jev.bodies().await.len(), 0);
    assert_eq!(env.store.classify_queue_len("a")?, 1);

    Ok(())
}

#[tokio::test]
async fn run_does_not_classify_a_message_twice() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.run(None).await;

    // It comes back to the inbox and is queued again by the next sync.
    env.store
        .enqueue_for_classification("a", &["e1".to_string()])?;

    let report = env.run(None).await;

    assert_eq!((report.classified, report.skipped), (0, 1));
    assert_eq!(env.jev.bodies().await.len(), 1);
    assert_eq!(env.store.classify_queue_len("a")?, 0);

    Ok(())
}

// ── correct ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn correct_classified_and_unclassified() -> Result<()> {
    let env = env().await;

    env.store.upsert_mailboxes(&[
        mailbox("a", "mb-promos", "Promos", None),
        mailbox("a", "mb-unsure", "Unsure", None),
        mailbox("a", "mb-updates", "Updates", None),
    ])?;

    // e1 was filed as unsure promotional; the user says it is updates.
    env.store.upsert_emails(&[email(
        "a",
        "e1",
        &["mb-inbox", "mb-promos", "mb-unsure", "mb-projects"],
    )])?;
    env.store.put_classification(&Classified {
        account_id: "a".into(),
        email_id: "e1".into(),
        tray: "promotional".into(),
        probs: Default::default(),
        urgency: 4,
        personal: 0.1,
        action: 0.0,
        unsure: true,
        model: "jev-test-1".into(),
        input_tokens: 10,
        classified_at: "2026-10-01T10:00:00Z".into(),
        corrected_tray: None,
        applied: true,
        apply_error: None,
    })?;

    let msg = MsgRef::new("a", "e1");

    correct(
        &env.cfg,
        &env.store,
        &env.clients,
        &msg,
        Tray::new("updates"),
    )
    .await?;

    let row = env.store.classification("a", "e1")?.unwrap();

    assert_eq!(row.corrected_tray.as_deref(), Some("updates"));
    assert_eq!(row.tray, "promotional");
    assert!(!row.unsure && row.applied);
    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&["mb-inbox", "mb-projects", "mb-updates"])
    );

    let p = patches(&env.jmap, "e1").await;

    assert_eq!(
        p,
        [json!({
            "mailboxIds/mb-promos": null,
            "mailboxIds/mb-unsure": null,
            "mailboxIds/mb-updates": true,
            "mailboxIds/mb-inbox": true,
        })]
    );

    // An email Jev never saw gets a manual row, applied, and is not flagged
    // even though the tray is needs_reply.
    env.store
        .upsert_emails(&[email("a", "e2", &["mb-inbox"])])?;
    correct(
        &env.cfg,
        &env.store,
        &env.clients,
        &MsgRef::new("a", "e2"),
        Tray::NEEDS_REPLY,
    )
    .await?;

    let manual = env.store.classification("a", "e2")?.unwrap();

    assert_eq!(
        (
            manual.model.as_str(),
            manual.tray.as_str(),
            manual.corrected_tray.as_deref()
        ),
        ("manual", "needs_reply", Some("needs_reply"))
    );
    assert!(manual.applied && !manual.unsure);
    assert!(
        env.mailboxes_of("a", "e2")
            .contains(&"mb-needs-reply".to_string())
    );
    assert!(
        patches(&env.jmap, "e2")
            .await
            .iter()
            .all(|p| p.get("keywords/$flagged").is_none())
    );
    assert!(!env.store.email("a", "e2")?.unwrap().is_flagged());
    assert!(!env.store.email("a", "e1")?.unwrap().is_flagged());

    // A failed correction stays pending and the next run retries it.
    env.store
        .upsert_emails(&[email("a", "e3", &["mb-inbox"])])?;
    env.jmap_state.refuse_moves.store(true, Ordering::SeqCst);

    let m3 = MsgRef::new("a", "e3");

    assert!(
        correct(&env.cfg, &env.store, &env.clients, &m3, Tray::new("social"))
            .await
            .is_err()
    );

    let pending = env.store.classification("a", "e3")?.unwrap();

    assert!(!pending.applied);
    assert!(pending.apply_error.is_some());
    assert_eq!(pending.corrected_tray.as_deref(), Some("social"));
    assert_eq!(env.mailboxes_of("a", "e3"), ["mb-inbox"]);

    env.jmap_state.refuse_moves.store(false, Ordering::SeqCst);

    let jev_requests = env.jev.bodies().await.len();
    let report = env.run(None).await;

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.moved, 1);
    assert_eq!(
        env.jev.bodies().await.len(),
        jev_requests,
        "a retry must not ask Jev"
    );
    assert!(env.store.classification("a", "e3")?.unwrap().applied);
    assert!(
        env.mailboxes_of("a", "e3")
            .contains(&"mb-social".to_string())
    );
    assert!(
        !env.mailboxes_of("a", "e3")
            .contains(&"mb-inbox".to_string())
    );

    // Correcting twice to the same tray is idempotent.
    let before = env.mailboxes_of("a", "e3");

    correct(&env.cfg, &env.store, &env.clients, &m3, Tray::new("social")).await?;

    assert_eq!(env.mailboxes_of("a", "e3"), before);
    assert!(env.store.classification("a", "e3")?.unwrap().applied);

    let p3 = patches(&env.jmap, "e3").await;

    assert_eq!(
        p3.last(),
        p3.get(p3.len() - 2),
        "the repeat sends the same patch"
    );

    // An urgent paid row corrected into needs_reply is never flagged, neither
    // by the correction itself nor by its retry.
    for id in ["e4", "e5"] {
        env.store
            .upsert_emails(&[email("a", id, &["mb-inbox", "mb-promos"])])?;
        env.store.put_classification(&Classified {
            account_id: "a".into(),
            email_id: id.into(),
            tray: "promotional".into(),
            probs: Default::default(),
            urgency: 5,
            personal: 0.9,
            action: 0.0,
            unsure: false,
            model: "jev-test-1".into(),
            input_tokens: 10,
            classified_at: "2026-10-01T10:00:00Z".into(),
            corrected_tray: None,
            applied: true,
            apply_error: None,
        })?;
    }

    let m5 = MsgRef::new("a", "e5");

    correct(&env.cfg, &env.store, &env.clients, &m5, Tray::NEEDS_REPLY).await?;

    env.jmap_state.refuse_moves.store(true, Ordering::SeqCst);

    let m4 = MsgRef::new("a", "e4");

    assert!(
        correct(&env.cfg, &env.store, &env.clients, &m4, Tray::NEEDS_REPLY)
            .await
            .is_err()
    );

    env.jmap_state.refuse_moves.store(false, Ordering::SeqCst);

    let report = env.run(None).await;

    assert_eq!((report.moved, report.flagged), (1, 0));

    for id in ["e4", "e5"] {
        assert!(env.store.classification("a", id)?.unwrap().applied, "{id}");
        assert!(
            env.mailboxes_of("a", id)
                .contains(&"mb-needs-reply".to_string()),
            "{id}"
        );
        assert!(!env.store.email("a", id)?.unwrap().is_flagged(), "{id}");
        assert!(
            patches(&env.jmap, id)
                .await
                .iter()
                .all(|p| p.get("keywords/$flagged").is_none()),
            "{id}"
        );
    }

    Ok(())
}

/// Classifies as `answer`, but the user corrects the email to `tray` while
/// Jev is still thinking.
struct CorrectsMidFlight<'a> {
    env: &'a Env,
    msg: MsgRef,
    tray: Tray,
    answer: Tray,
}

impl classify::Classifier for CorrectsMidFlight<'_> {
    async fn classify(&self, _state: &str) -> Result<classify::Classification> {
        correct(
            &self.env.cfg,
            &self.env.store,
            &self.env.clients,
            &self.msg,
            self.tray.clone(),
        )
        .await?;

        Ok(classify::Classification {
            tray: self.answer.clone(),
            probs: Default::default(),
            urgency: 5,
            personal: 0.9,
            action: 0.0,
            unsure: false,
            model: "jev-test-1".into(),
            input_tokens: 7,
        })
    }
}

#[tokio::test]
async fn correction_wins_over_inflight_answer() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);

    let mid = CorrectsMidFlight {
        env: &env,
        msg: MsgRef::new("a", "e1"),
        tray: Tray::new("social"),
        answer: Tray::NEEDS_REPLY,
    };
    let report = run_with_lock(&env.lock, &env.cfg, &env.store, &env.clients, &mid, None).await;
    let row = env.store.classification("a", "e1")?.unwrap();

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(row.corrected_tray.as_deref(), Some("social"));
    assert!(row.applied);
    assert!(
        env.mailboxes_of("a", "e1")
            .contains(&"mb-social".to_string())
    );
    assert!(
        !env.mailboxes_of("a", "e1")
            .contains(&"mb-needs-reply".to_string())
    );
    assert!(!env.store.email("a", "e1")?.unwrap().is_flagged());
    assert_eq!(env.store.classify_queue_len("a")?, 0);

    Ok(())
}

#[tokio::test]
async fn failed_correction_retries_after_exhausted_queue() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);

    for _ in 0..3 {
        env.store.begin_classify_attempt("a", "e1")?;
    }

    env.jmap_state.refuse_moves.store(true, Ordering::SeqCst);

    assert!(
        correct(
            &env.cfg,
            &env.store,
            &env.clients,
            &MsgRef::new("a", "e1"),
            Tray::new("social"),
        )
        .await
        .is_err()
    );

    env.jmap_state.refuse_moves.store(false, Ordering::SeqCst);

    let report = env.run(None).await;

    assert_eq!((report.moved, report.errors.len()), (1, 0));
    assert!(env.store.classification("a", "e1")?.unwrap().applied);
    assert!(
        env.mailboxes_of("a", "e1")
            .contains(&"mb-social".to_string())
    );
    assert_eq!(env.jev.bodies().await.len(), 0);

    Ok(())
}

#[tokio::test]
async fn correction_preserves_unrelated_memberships() -> Result<()> {
    let env = env().await;

    env.store.upsert_mailboxes(&[
        mailbox("a", "mb-social", "Social", None),
        mailbox("a", "mb-updates", "Updates", None),
    ])?;
    env.queue(
        "a",
        "e1",
        &["mb-inbox", "mb-projects", "mb-social", "mb-junk"],
    );
    env.run(None).await;

    let m1 = MsgRef::new("a", "e1");

    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&[
            "mb-inbox",
            "mb-projects",
            "mb-social",
            "mb-junk",
            "mb-updates"
        ])
    );

    // Updates -> needs_reply drops only the updates folder.
    correct(&env.cfg, &env.store, &env.clients, &m1, Tray::NEEDS_REPLY).await?;

    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&[
            "mb-inbox",
            "mb-projects",
            "mb-social",
            "mb-junk",
            "mb-needs-reply"
        ])
    );

    // A failed second correction is retried; the retry still knows the folder
    // the server holds (needs_reply), not only Jev's original answer.
    env.jmap_state.refuse_moves.store(true, Ordering::SeqCst);

    assert!(
        correct(
            &env.cfg,
            &env.store,
            &env.clients,
            &m1,
            Tray::new("promotional")
        )
        .await
        .is_err()
    );

    env.jmap_state.refuse_moves.store(false, Ordering::SeqCst);
    env.run(None).await;

    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&["mb-projects", "mb-social", "mb-junk", "mb-promos"])
    );

    // Junk goes only when the previous tray was spam.
    env.queue("a", "e2", &["mb-inbox"]);
    env.run(None).await;

    let m2 = MsgRef::new("a", "e2");

    correct(&env.cfg, &env.store, &env.clients, &m2, Tray::SPAM).await?;

    assert_eq!(env.mailboxes_of("a", "e2"), ["mb-junk"]);

    correct(
        &env.cfg,
        &env.store,
        &env.clients,
        &m2,
        Tray::new("updates"),
    )
    .await?;

    assert_eq!(
        env.mailboxes_of("a", "e2"),
        sorted(&["mb-inbox", "mb-updates"])
    );

    Ok(())
}

#[tokio::test]
async fn pending_automatic_answer_respects_opt_out_but_corrections_do_not() -> Result<()> {
    let mut env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.queue("a", "e2", &["mb-inbox"]);
    env.jmap_state.refuse_moves.store(true, Ordering::SeqCst);
    env.run(None).await;

    // Both answers are saved, unapplied. The user corrects e2 (refused too).
    assert!(!env.store.classification("a", "e1")?.unwrap().applied);
    assert!(
        correct(
            &env.cfg,
            &env.store,
            &env.clients,
            &MsgRef::new("a", "e2"),
            Tray::new("social"),
        )
        .await
        .is_err()
    );

    env.cfg.accounts[0].classify = Some(false);
    env.jmap_state.refuse_moves.store(false, Ordering::SeqCst);
    env.run(None).await;

    assert!(
        !env.store.classification("a", "e1")?.unwrap().applied,
        "an opted-out account gets no automatic moves"
    );
    assert!(
        patches(&env.jmap, "e1").await.len() == 1,
        "only the refused try"
    );
    assert!(env.store.classification("a", "e2")?.unwrap().applied);
    assert!(
        env.mailboxes_of("a", "e2")
            .contains(&"mb-social".to_string())
    );

    Ok(())
}

#[tokio::test]
async fn correction_without_connection_mirrors_locally_and_stays_pending() -> Result<()> {
    let mut env = env().await;

    env.store
        .upsert_mailboxes(&[mailbox("a", "mb-updates", "Updates", None)])?;
    env.store
        .upsert_emails(&[email("a", "e1", &["mb-inbox"])])?;
    env.clients.clear();

    correct(
        &env.cfg,
        &env.store,
        &env.clients,
        &MsgRef::new("a", "e1"),
        Tray::new("updates"),
    )
    .await?;

    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&["mb-inbox", "mb-updates"])
    );
    assert!(!env.store.classification("a", "e1")?.unwrap().applied);
    assert!(calls(&env.jmap).await.is_empty());

    Ok(())
}

// ── backfill ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn backfill_queues_newest_unclassified_inbox_mail() -> Result<()> {
    let env = env_with(
        &[("a", Some(true)), ("b", Some(false))],
        answer("updates", false, 0.0),
    )
    .await;

    for (account, id, at) in [
        ("a", "e1", "01"),
        ("a", "e2", "02"),
        ("a", "e3", "03"),
        ("a", "e4", "04"),
        ("b", "x1", "05"),
    ] {
        let mut e = email(account, id, &["mb-inbox"]);

        e.received_at = format!("2026-10-01T10:00:{at}Z");
        env.store.upsert_emails(&[e])?;
    }

    // e4 is classified already; nothing for the disabled account b.
    env.queue("a", "e4", &["mb-inbox"]);
    env.run(None).await;

    let wanted = |ids: &[&str]| -> Vec<Candidate> {
        ids.iter()
            .map(|id| Candidate {
                account_id: "a".into(),
                email_id: id.to_string(),
            })
            .collect()
    };

    assert_eq!(
        backfill_ids(&env.store, &env.cfg, None, 2)?,
        wanted(&["e3", "e2"])
    );
    // Reading changed nothing.
    assert_eq!(env.store.classify_queue_len("a")?, 0);
    assert_eq!(backfill_candidates(&env.store, &env.cfg, None, 2)?, 2);
    assert_eq!(env.store.classify_queue_len("a")?, 2);
    // A second call finds only what is not queued yet.
    assert_eq!(backfill_candidates(&env.store, &env.cfg, Some("a"), 10)?, 1);
    assert_eq!(backfill_candidates(&env.store, &env.cfg, Some("b"), 10)?, 0);

    Ok(())
}
// Round 2 concurrency probes, appended only in the disposable worktree.
async fn wait_for_mailbox_patch(env: &Env, key: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if patches(&env.jmap, "e1")
                .await
                .iter()
                .any(|p| p[key] == true)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("request never reached fake JMAP");
}

async fn delay_mailbox_patch(env: &Env, key: &'static str, delay: u64) {
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(move |r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            b["methodCalls"][0][0] == "Email/set"
                && b["methodCalls"][0][1]["update"]["e1"][key] == true
        })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(
                    json!({"methodResponses": [["Email/set", {"updated": {"e1": null}}, "0"]]}),
                )
                .set_delay(std::time::Duration::from_millis(delay)),
        )
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&env.jmap)
        .await;
}

#[tokio::test]
async fn review_correction_during_move_does_not_flag() -> Result<()> {
    let env = env_with(&[("a", Some(true))], answer("needs_reply", false, 4.0)).await;
    env.queue("a", "e1", &["mb-inbox"]);
    delay_mailbox_patch(&env, "mailboxIds/mb-needs-reply", 300).await;
    let (report, correction) = tokio::join!(env.run(None), async {
        wait_for_mailbox_patch(&env, "mailboxIds/mb-needs-reply").await;
        correct(
            &env.cfg,
            &env.store,
            &env.clients,
            &MsgRef::new("a", "e1"),
            Tray::new("social"),
        )
        .await
    });
    correction?;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        env.store
            .classification("a", "e1")?
            .unwrap()
            .corrected_tray
            .as_deref(),
        Some("social")
    );
    env.run(None).await;
    assert!(
        !env.store.email("a", "e1")?.unwrap().is_flagged(),
        "stale automatic move flagged corrected Social mail, and retry never cleared it"
    );
    Ok(())
}

struct CorrectAnotherQueued<'a> {
    env: &'a Env,
    first: AtomicBool,
}
impl classify::Classifier for CorrectAnotherQueued<'_> {
    async fn classify(&self, state: &str) -> Result<classify::Classification> {
        if self.first.swap(false, Ordering::SeqCst) {
            correct(
                &self.env.cfg,
                &self.env.store,
                &self.env.clients,
                &MsgRef::new("a", "e2"),
                Tray::new("social"),
            )
            .await?;
        }
        classify::Classifier::classify(&self.env.jev.client(), state).await
    }
}

#[tokio::test]
async fn review_batch_rechecks_corrected_candidates() -> Result<()> {
    let env = env().await;
    env.queue("a", "e1", &["mb-inbox"]);
    env.queue("a", "e2", &["mb-inbox"]);
    let classifier = CorrectAnotherQueued {
        env: &env,
        first: AtomicBool::new(true),
    };
    let report = run_with_lock(
        &env.lock,
        &env.cfg,
        &env.store,
        &env.clients,
        &classifier,
        None,
    )
    .await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        env.jev.bodies().await.len(),
        1,
        "asked Jev about e2 after it was manually classified and removed from the queue"
    );
    Ok(())
}

#[tokio::test]
async fn review_concurrent_corrections_remove_all_owned_trays() -> Result<()> {
    let env = env().await;
    env.store
        .upsert_emails(&[email("a", "e1", &["mb-inbox"])])?;
    env.store.upsert_mailboxes(&[
        mailbox("a", "mb-social", "Social", None),
        mailbox("a", "mb-promos", "Promos", None),
        mailbox("a", "mb-updates", "Updates", None),
    ])?;
    delay_mailbox_patch(&env, "mailboxIds/mb-social", 300).await;
    delay_mailbox_patch(&env, "mailboxIds/mb-promos", 600).await;
    let msg = MsgRef::new("a", "e1");
    let (first, second, latest) = tokio::join!(
        correct(
            &env.cfg,
            &env.store,
            &env.clients,
            &msg,
            Tray::new("social")
        ),
        async {
            wait_for_mailbox_patch(&env, "mailboxIds/mb-social").await;
            correct(
                &env.cfg,
                &env.store,
                &env.clients,
                &msg,
                Tray::new("promotional"),
            )
            .await
        },
        async {
            wait_for_mailbox_patch(&env, "mailboxIds/mb-promos").await;
            correct(
                &env.cfg,
                &env.store,
                &env.clients,
                &msg,
                Tray::new("updates"),
            )
            .await
        },
    );
    first?;
    second?;
    latest?;
    assert_eq!(
        env.store
            .classification("a", "e1")?
            .unwrap()
            .corrected_tray
            .as_deref(),
        Some("updates")
    );
    env.run(None).await;
    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&["mb-inbox", "mb-updates"]),
        "a stale correction's mailbox survived even after retry"
    );
    Ok(())
}
#[tokio::test]
async fn review_html_heading_keeps_jevmail_dividers() -> Result<()> {
    let e = email("a", "e1", &["mb-inbox"]);
    let b = posto_core::store::EmailBody {
        email_id: "e1".into(),
        html: Some("<h1>Heading</h1>".into()),
        text: None,
        attachments: vec![],
        in_reply_to: vec![],
        references: vec![],
        message_id: vec![],
        reply_to: vec![],
    };
    let state = classify::state::state_for(&e, &b, false, false, 2000);
    let body = state.split_once("Body:\n").unwrap().1;
    // Pinned html2text 1a4bdc82ecec emits asterisks around an H1 heading.
    assert!(
        body.lines()
            .any(|line| line.len() > 1 && line.chars().all(|c| c == '*')),
        "Go's H1 divider is missing: {body:?}"
    );
    Ok(())
}

#[tokio::test]
async fn review_batch_rechecks_after_the_body_fetches() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();

            b["methodCalls"][0][1]["properties"][0] == "header:List-Unsubscribe:asText"
        })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"methodResponses": [[
                    "Email/get",
                    {"list": [{"id": "e1", "header:List-Unsubscribe:asText": null}]},
                    "0"
                ]]}))
                .set_delay(std::time::Duration::from_millis(300)),
        )
        .with_priority(1)
        .mount(&env.jmap)
        .await;

    let msg = MsgRef::new("a", "e1");
    let (report, correction) = tokio::join!(env.run(None), async {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !calls(&env.jmap)
                .await
                .iter()
                .any(|(_, a)| a["properties"][0] == "header:List-Unsubscribe:asText")
            {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("header fetch never started");
        correct(
            &env.cfg,
            &env.store,
            &env.clients,
            &msg,
            Tray::new("social"),
        )
        .await
    });

    correction?;

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.classified, 0);
    assert_eq!(
        env.jev.bodies().await.len(),
        0,
        "asked Jev after the email was corrected during the fetches"
    );
    assert_eq!(
        env.store
            .classification("a", "e1")?
            .unwrap()
            .corrected_tray
            .as_deref(),
        Some("social")
    );

    Ok(())
}

#[tokio::test]
async fn batch_skips_the_body_fetch_for_corrected_candidates() -> Result<()> {
    let env = env().await;

    env.queue("a", "e1", &["mb-inbox"]);
    env.queue("a", "e2", &["mb-inbox"]);

    let classifier = CorrectAnotherQueued {
        env: &env,
        first: AtomicBool::new(true),
    };
    let report = run_with_lock(
        &env.lock,
        &env.cfg,
        &env.store,
        &env.clients,
        &classifier,
        None,
    )
    .await;

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(
        calls(&env.jmap)
            .await
            .iter()
            .all(|(_, args)| args["ids"][0] != "e2"),
        "fetched the body of an email that was corrected and dequeued"
    );
    assert_eq!(
        env.store.classify_queue(Some("a"))?.len(),
        0,
        "the corrected email's queue row must stay gone"
    );

    Ok(())
}

#[tokio::test]
async fn correction_after_a_refused_one_removes_the_tray_it_may_have_added() -> Result<()> {
    let env = env().await;

    env.store
        .upsert_emails(&[email("a", "e1", &["mb-inbox"])])?;
    env.store.upsert_mailboxes(&[
        mailbox("a", "mb-social", "Social", None),
        mailbox("a", "mb-promos", "Promos", None),
    ])?;

    let msg = MsgRef::new("a", "e1");

    env.jmap_state.refuse_moves.store(true, Ordering::SeqCst);
    assert!(
        correct(
            &env.cfg,
            &env.store,
            &env.clients,
            &msg,
            Tray::new("social")
        )
        .await
        .is_err()
    );
    env.jmap_state.refuse_moves.store(false, Ordering::SeqCst);
    correct(
        &env.cfg,
        &env.store,
        &env.clients,
        &msg,
        Tray::new("promotional"),
    )
    .await?;

    let last = patches(&env.jmap, "e1").await.pop().unwrap();

    assert!(
        last.as_object().unwrap().get("mailboxIds/mb-social") == Some(&Value::Null),
        "the refused Social change may have reached the server: {last}"
    );
    assert_eq!(env.mailboxes_of("a", "e1"), sorted(&["mb-promos"]));

    Ok(())
}

#[tokio::test]
async fn correction_waits_for_the_move_in_flight_then_undoes_it() -> Result<()> {
    let env = env_with(&[("a", Some(true))], answer("needs_reply", false, 4.0)).await;

    env.queue("a", "e1", &["mb-inbox"]);
    delay_mailbox_patch(&env, "mailboxIds/mb-needs-reply", 300).await;

    let msg = MsgRef::new("a", "e1");
    let (_, waited) = tokio::join!(env.run(None), async {
        wait_for_mailbox_patch(&env, "mailboxIds/mb-needs-reply").await;

        let since = std::time::Instant::now();

        correct(
            &env.cfg,
            &env.store,
            &env.clients,
            &msg,
            Tray::new("social"),
        )
        .await
        .map(|()| since.elapsed())
    });

    assert!(
        waited? >= std::time::Duration::from_millis(200),
        "the correction was sent while the automatic move was still in flight"
    );

    let sent = patches(&env.jmap, "e1").await;

    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1]["mailboxIds/mb-needs-reply"], Value::Null);
    assert_eq!(sent[1]["mailboxIds/mb-social"], true);

    Ok(())
}
#[tokio::test]
async fn review_distinct_store_handles_keep_all_owned_trays() -> Result<()> {
    let mut env = env().await;
    let path = env._dir.path().join("shared.db");
    env.store = Store::open(&path)?;
    let second_store = Store::open(&path)?;
    env.store
        .upsert_emails(&[email("a", "e1", &["mb-inbox"])])?;
    env.store.upsert_mailboxes(&[
        mailbox("a", "mb-inbox", "Inbox", Some("inbox")),
        mailbox("a", "mb-junk", "Junk", Some("junk")),
        mailbox("a", "mb-social", "Social", None),
        mailbox("a", "mb-promos", "Promos", None),
        mailbox("a", "mb-updates", "Updates", None),
    ])?;
    delay_mailbox_patch(&env, "mailboxIds/mb-social", 600).await;
    delay_mailbox_patch(&env, "mailboxIds/mb-promos", 300).await;
    let msg = MsgRef::new("a", "e1");
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        let (first, second, latest) = tokio::join!(
            correct(
                &env.cfg,
                &env.store,
                &env.clients,
                &msg,
                Tray::new("social")
            ),
            async {
                wait_for_mailbox_patch(&env, "mailboxIds/mb-social").await;
                correct(
                    &env.cfg,
                    &second_store,
                    &env.clients,
                    &msg,
                    Tray::new("promotional"),
                )
                .await
            },
            async {
                wait_for_mailbox_patch(&env, "mailboxIds/mb-promos").await;
                correct(
                    &env.cfg,
                    &env.store,
                    &env.clients,
                    &msg,
                    Tray::new("updates"),
                )
                .await
            },
        );
        first?;
        second?;
        latest?;
        Ok::<(), Error>(())
    })
    .await
    .expect("corrections deadlocked")?;
    assert_eq!(
        env.store
            .classification("a", "e1")?
            .unwrap()
            .corrected_tray
            .as_deref(),
        Some("updates")
    );
    let report = env.run(None).await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(env.store.classification("a", "e1")?.unwrap().applied);
    assert_eq!(
        env.store.owned_trays("a", "e1")?,
        vec!["updates".to_string()]
    );
    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&["mb-inbox", "mb-updates"]),
        "cleanup was lost between independent Store handles to the shared cache"
    );
    assert!(env.store.classification("a", "e1")?.unwrap().applied);
    Ok(())
}

#[tokio::test]
async fn review_stale_confirmation_does_not_resurrect_removed_tray() -> Result<()> {
    let mut env = env().await;
    let path = env._dir.path().join("shared.db");
    env.store = Store::open(&path)?;
    let second_store = Store::open(&path)?;
    env.store
        .upsert_emails(&[email("a", "e1", &["mb-inbox"])])?;
    env.store.upsert_mailboxes(&[
        mailbox("a", "mb-inbox", "Inbox", Some("inbox")),
        mailbox("a", "mb-junk", "Junk", Some("junk")),
        mailbox("a", "mb-social", "Social", None),
        mailbox("a", "mb-updates", "Updates", None),
    ])?;
    let msg = MsgRef::new("a", "e1");
    env.store.set_correction("a", "e1", "social")?;
    let _turn = env.store.lock_message("a", "e1").await;
    let (old, revision) = env.store.classification_with_revision("a", "e1")?.unwrap();
    // Social's HTTP operation succeeds; its local confirmation is delayed.
    let done = classify::apply_tray(
        &env.cfg,
        &env.store,
        env.clients.get("a").map(Arc::as_ref),
        &old,
        revision,
    )
    .await?;
    assert!(
        env.mailboxes_of("a", "e1")
            .contains(&"mb-social".to_string())
    );
    // Another handle completes a newer correction, including Social's removal.
    correct(
        &env.cfg,
        &second_store,
        &env.clients,
        &msg,
        Tray::new("updates"),
    )
    .await?;
    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&["mb-inbox", "mb-updates"])
    );
    assert_eq!(
        env.store.owned_trays("a", "e1")?,
        vec!["updates".to_string()]
    );
    assert!(env.store.classification("a", "e1")?.unwrap().applied);
    // Resume confirmation of the already completed, older Social operation.
    assert!(!env.store.confirm_classification(
        "a",
        "e1",
        "social",
        &done.removed,
        done.patched,
        revision
    )?);
    drop(_turn);
    assert_eq!(
        env.store.owned_trays("a", "e1")?,
        vec!["updates".to_string()],
        "stale confirmation resurrected ownership of Social after a completed Updates correction removed it"
    );
    // Social's patch was sent, so the row is pending for a repair pass.
    assert!(!env.store.classification("a", "e1")?.unwrap().applied);
    Ok(())
}

#[tokio::test]
async fn superseded_confirmation_returns_false_and_changes_nothing() -> Result<()> {
    let env = env().await;
    env.store.put_classification(&Classified {
        account_id: "a".into(),
        email_id: "e1".into(),
        tray: "promotional".into(),
        probs: Default::default(),
        urgency: 0,
        personal: 0.0,
        action: 0.0,
        unsure: false,
        model: "jev-test-1".into(),
        input_tokens: 1,
        classified_at: "2026-10-01T10:00:00Z".into(),
        corrected_tray: None,
        applied: true,
        apply_error: None,
    })?;
    let (_, revision) = env.store.classification_with_revision("a", "e1")?.unwrap();

    env.store.set_correction("a", "e1", "updates")?;

    let before = env.store.classification("a", "e1")?.unwrap();
    let owned = env.store.owned_trays("a", "e1")?;

    assert!(!before.applied);
    assert_eq!(owned, vec!["promotional".to_string()]);
    assert!(!env.store.confirm_classification(
        "a",
        "e1",
        "social",
        &["promotional".to_string()],
        false,
        revision
    )?);

    let after = env.store.classification("a", "e1")?.unwrap();

    assert_eq!(after.applied, before.applied);
    assert_eq!(after.corrected_tray, before.corrected_tray);
    assert_eq!(after.corrected_tray.as_deref(), Some("updates"));
    assert_eq!(env.store.owned_trays("a", "e1")?, owned);

    let (_, current) = env.store.classification_with_revision("a", "e1")?.unwrap();

    assert!(
        env.store
            .confirm_classification("a", "e1", "updates", &[], false, current)?
    );
    assert!(env.store.classification("a", "e1")?.unwrap().applied);
    Ok(())
}

#[tokio::test]
async fn review_latest_intent_stays_applied_after_superseded_move_finishes_last() -> Result<()> {
    let mut env = env().await;
    let path = env._dir.path().join("shared.db");
    env.store = Store::open(&path)?;
    let second_store = Store::open(&path)?;
    env.store
        .upsert_emails(&[email("a", "e1", &["mb-inbox"])])?;
    env.store.upsert_mailboxes(&[
        mailbox("a", "mb-inbox", "Inbox", Some("inbox")),
        mailbox("a", "mb-junk", "Junk", Some("junk")),
        mailbox("a", "mb-social", "Social", None),
        mailbox("a", "mb-promos", "Promos", None),
        mailbox("a", "mb-updates", "Updates", None),
    ])?;
    delay_mailbox_patch(&env, "mailboxIds/mb-social", 600).await;
    delay_mailbox_patch(&env, "mailboxIds/mb-promos", 300).await;
    let msg = MsgRef::new("a", "e1");
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        let (first, second, latest) = tokio::join!(
            correct(
                &env.cfg,
                &env.store,
                &env.clients,
                &msg,
                Tray::new("social")
            ),
            async {
                wait_for_mailbox_patch(&env, "mailboxIds/mb-social").await;
                correct(
                    &env.cfg,
                    &second_store,
                    &env.clients,
                    &msg,
                    Tray::new("promotional"),
                )
                .await
            },
            async {
                wait_for_mailbox_patch(&env, "mailboxIds/mb-promos").await;
                correct(
                    &env.cfg,
                    &second_store,
                    &env.clients,
                    &msg,
                    Tray::new("updates"),
                )
                .await
            },
        );
        first?;
        second?;
        latest?;
        Ok::<(), Error>(())
    })
    .await
    .expect("corrections deadlocked")?;
    // Social finished last and undid the newest application: the row is
    // pending again, with the newest intent and its ownership intact.
    let row = env.store.classification("a", "e1")?.unwrap();

    assert_eq!(row.corrected_tray.as_deref(), Some("updates"));
    assert!(!row.applied, "a late superseded patch left the row settled");

    let report = env.run(None).await;

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(env.store.classification("a", "e1")?.unwrap().applied);
    assert_eq!(
        env.mailboxes_of("a", "e1"),
        sorted(&["mb-inbox", "mb-updates"]),
        "the pending pass did not restore the newest intent"
    );
    Ok(())
}

#[tokio::test]
async fn apply_tray_sends_nothing_once_the_intent_has_moved_on() -> Result<()> {
    let env = env().await;
    env.store
        .upsert_emails(&[email("a", "e1", &["mb-inbox"])])?;
    env.store.upsert_mailboxes(&[
        mailbox("a", "mb-inbox", "Inbox", Some("inbox")),
        mailbox("a", "mb-social", "Social", None),
    ])?;
    env.store.set_correction("a", "e1", "social")?;

    let (old, revision) = env.store.classification_with_revision("a", "e1")?.unwrap();

    env.store.set_correction("a", "e1", "updates")?;

    let done = classify::apply_tray(
        &env.cfg,
        &env.store,
        env.clients.get("a").map(Arc::as_ref),
        &old,
        revision,
    )
    .await?;

    assert!(!done.patched);
    assert!(patches(&env.jmap, "e1").await.is_empty());
    assert!(env.store.owned_trays("a", "e1")?.is_empty());
    Ok(())
}
