//! Everything the UI needs from `posto_core`, built once at startup.

use std::collections::HashMap;
use std::sync::Arc;

use dioxus::prelude::*;
use posto_core::actions::{self, Action};
use posto_core::classify::state::state_for;
use posto_core::classify::{Classification, Classifier, ClassifyReport, Tray};
use posto_core::config::{Config, Secrets};
use posto_core::provider::{self, Connection};
use posto_core::store::{Classified, EmailBody, EmailSummary, MsgRef, Store};
use posto_core::{Error, Result, fixtures};

/// What the message list shows when no search is active.
#[derive(Clone, Debug, PartialEq)]
pub enum Folder {
    /// A mailbox group (see `posto_core::store::mailbox_groups`) listed as one:
    /// the members are looked up from the current mailboxes by `key`.
    Merged {
        /// Group key, `role:<role>` or `name:<name>`.
        key: String,
    },
    /// One mailbox of one account.
    Mailbox {
        /// Local account slug.
        account_id: String,
        /// Server mailbox id.
        id: String,
    },
    /// The classifier's unsure, uncorrected messages of every account.
    Unsure,
    /// What the user still owes someone, across every enabled account
    /// (`Store::list_triage`), oldest first.
    Triage,
}

/// Stands in for Jev in UI test mode, so no key and no network are needed: a
/// subject with a question mark needs a reply, anything else is a receipt.
/// Never unsure.
pub struct FakeJev;

impl Classifier for FakeJev {
    async fn classify(&self, state: &str) -> Result<Classification> {
        let asks = state
            .lines()
            .find_map(|l| l.strip_prefix("Subject:"))
            .is_some_and(|subject| subject.contains('?'));
        let tray = if asks {
            Tray::NEEDS_REPLY
        } else {
            Tray::new("receipts")
        };

        Ok(Classification {
            probs: [(tray.as_str().to_string(), 0.9)].into(),
            tray,
            urgency: if asks { 3 } else { 1 },
            personal: if asks { 0.8 } else { 0.1 },
            action: if asks { 0.8 } else { 0.0 },
            unsure: false,
            model: "fake-jev".into(),
            input_tokens: 0,
        })
    }
}

/// UI test mode's classifier pass, with no client and no network: drains the
/// queue of every enabled account through [`FakeJev`], stores each answer as
/// applied, and mirrors the tray move on the local copy only.
pub async fn fake_classify_pass(cfg: &Config, store: &Store) -> Result<ClassifyReport> {
    let accounts: Vec<String> = cfg.accounts_enabled().map(|a| a.id.clone()).collect();
    let mut report = ClassifyReport::default();

    for row in store.next_classify_batch(&accounts, cfg.classify.max_per_run as usize)? {
        let (account, id) = (row.account_id.as_str(), row.email_id.as_str());
        let Some(email) = store.email(account, id)? else {
            store.remove_from_classify_queue(account, id)?;

            continue;
        };
        let body = store.body(account, id)?.unwrap_or(EmailBody {
            email_id: id.to_string(),
            html: None,
            text: None,
            attachments: Vec::new(),
            in_reply_to: Vec::new(),
            references: Vec::new(),
            message_id: Vec::new(),
            reply_to: Vec::new(),
        });
        let state = state_for(
            &email,
            &body,
            false,
            false,
            cfg.classify.body_chars as usize,
        );
        let c = FakeJev.classify(&state).await?;

        store.put_classification(&Classified {
            account_id: account.to_string(),
            email_id: id.to_string(),
            tray: c.tray.as_str().to_string(),
            probs: c.probs,
            urgency: c.urgency,
            personal: c.personal,
            action: c.action,
            unsure: c.unsure,
            model: c.model,
            input_tokens: c.input_tokens,
            classified_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            corrected_tray: None,
            applied: true,
            apply_error: None,
        })?;
        store.remove_from_classify_queue(account, id)?;
        report.classified += 1;

        match mirror_tray_move(cfg, store, account, id, c.tray).await {
            Ok(()) => report.moved += 1,
            Err(e) => report.errors.push(format!("{account}/{id}: {e}")),
        }
    }

    Ok(report)
}

/// Files `id` under the mailbox of `tray` on the local copy: adds the tray
/// mailbox and, unless the tray stays in the inbox, removes the inbox.
async fn mirror_tray_move(
    cfg: &Config,
    store: &Store,
    account: &str,
    id: &str,
    tray: Tray,
) -> Result<()> {
    let name = cfg
        .classify
        .trays
        .get(tray.as_str())
        .map_or(tray.as_str(), String::as_str);
    let target = store
        .top_level_mailbox_by_name(account, name)?
        .ok_or_else(|| Error::NotFound(format!("mailbox {name}")))?;
    let remove = if cfg
        .classify
        .keep_in_inbox
        .iter()
        .any(|t| t == tray.as_str())
    {
        Vec::new()
    } else {
        store
            .mailboxes(account)?
            .into_iter()
            .filter(|m| m.role.as_deref() == Some("inbox"))
            .map(|m| m.id)
            .collect()
    };
    let action = Action::PatchMailboxes {
        add: vec![target.id],
        remove,
    };

    match actions::apply(store, None, account, &[id.to_string()], action).await? {
        0 => Err(Error::Other("no email changed".into())),
        _ => Ok(()),
    }
}

/// Store, server connections and settings, shared through a `Signal<Backend>` context.
#[derive(Clone)]
pub struct Backend {
    pub store: Arc<Store>,
    /// One connection per account, made lazily on first use; empty offline, in
    /// UI test mode, or without credentials.
    pub clients: HashMap<String, Arc<Connection>>,
    pub cfg: Config,
    /// `POSTO_UI_TEST=1`: in-memory fixtures, no network, nothing written to disk.
    pub ui_test: bool,
    /// Bumped whenever an endpoint or credential may have changed, so a connection
    /// that was already in flight is not cached under the new settings.
    pub conn_rev: u64,
    /// Why `config.toml` could not be loaded. While set, `cfg` is a stand-in:
    /// it is never written to disk and Settings stays closed.
    pub config_error: Option<String>,
}

impl Backend {
    /// Reads `POSTO_UI_TEST` and opens the matching store.
    pub fn from_env() -> Result<Backend> {
        let ui_test = std::env::var("POSTO_UI_TEST").is_ok_and(|v| v == "1");

        if ui_test {
            let store = Store::open_memory()?;

            fixtures::seed(&store)?;

            let mut cfg = Config::default();

            cfg.accounts[0].email = "me@example.com".into();

            return Ok(Backend {
                store: Arc::new(store),
                clients: HashMap::new(),
                cfg,
                ui_test,
                conn_rev: 0,
                config_error: None,
            });
        }

        let (cfg, config_error) = config_or_stand_in(Config::load());

        Ok(Backend {
            store: Arc::new(Store::open(&Config::db_path())?),
            clients: HashMap::new(),
            cfg,
            ui_test,
            conn_rev: 0,
            config_error,
        })
    }

    /// The config to write to disk, or the load error that forbids it: a
    /// stand-in config must never overwrite the user's file.
    pub fn config_for_save(&self) -> std::result::Result<&Config, &str> {
        match &self.config_error {
            Some(e) => Err(e),
            None => Ok(&self.cfg),
        }
    }

    /// Drops every connection and invalidates any connection still being made.
    pub fn reset_client(&mut self) {
        self.clients.clear();
        self.conn_rev += 1;
    }

    /// True when the sender (or its domain) is in `cfg.blocked_senders`.
    pub fn is_blocked(&self, email: &EmailSummary) -> bool {
        email
            .from
            .iter()
            .any(|a| sender_blocked(&self.cfg.blocked_senders, &a.email))
    }
}

/// The loaded config, or defaults plus the reason it could not be loaded.
fn config_or_stand_in(loaded: Result<Config>) -> (Config, Option<String>) {
    match loaded {
        Ok(cfg) => (cfg, None),
        Err(e) => (Config::default(), Some(e.to_string())),
    }
}

/// The key of a list row and the id of its selection entry: `<account>/<id>`.
pub fn row_key(email: &EmailSummary) -> String {
    email.msg_ref().to_string()
}

/// The rows of a message list with their keys. Email ids repeat across
/// accounts, so every row key, DOM id and selection id comes from here.
pub fn rows_for_display(emails: &[EmailSummary]) -> Vec<(String, EmailSummary)> {
    emails.iter().map(|e| (row_key(e), e.clone())).collect()
}

/// Groups references by account, in first-seen order, keeping each account's ids in order.
pub fn group_by_account(refs: &[MsgRef]) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();

    for r in refs {
        match groups.iter_mut().find(|(a, _)| *a == r.account_id) {
            Some((_, ids)) => ids.push(r.id.clone()),
            None => groups.push((r.account_id.clone(), vec![r.id.clone()])),
        }
    }

    groups
}

/// True when `address` or its domain is listed (case-insensitive; `@domain` also works).
pub fn sender_blocked(blocked: &[String], address: &str) -> bool {
    let address = address.trim().to_lowercase();
    let domain = address.rsplit('@').next().unwrap_or_default();

    blocked.iter().any(|entry| {
        let entry = entry.trim().to_lowercase();

        !entry.is_empty() && (entry == address || entry.trim_start_matches('@') == domain)
    })
}

/// True when remote content may load for mail from `from` without asking.
///
/// That is the case under the `"always"` policy, or when the sender or its
/// domain is in `cfg.trusted_senders`.
pub fn remote_allowed(cfg: &Config, from: &str) -> bool {
    cfg.remote_content == "always"
        || (!from.trim().is_empty() && sender_blocked(&cfg.trusted_senders, from))
}

/// The connected client of `account_id`, connecting with its stored credentials when needed.
///
/// `Ok(None)` in UI test mode. `Error::NotConfigured` when the account has no
/// token (JMAP) or Proton login.
pub async fn client(backend: Signal<Backend>, account_id: &str) -> Result<Option<Arc<Connection>>> {
    {
        let b = backend.read();

        if b.ui_test {
            return Ok(None);
        }

        if let Some(client) = b.clients.get(account_id) {
            return Ok(Some(client.clone()));
        }

        let account = b
            .cfg
            .account(account_id)
            .ok_or_else(|| Error::NotFound(format!("account {account_id}")))?;

        if !Secrets::has_credentials(&b.cfg, account)? {
            return Err(provider::credential_error(account));
        }
    }

    connect(backend, account_id).await.map(Some)
}

/// Connects `account_id` through [`provider::connect`] and caches the client.
///
/// # Errors
///
/// Besides connection errors, fails when the settings or credentials were
/// saved while connecting: the client would belong to the old endpoint.
async fn connect(backend: Signal<Backend>, account_id: &str) -> Result<Arc<Connection>> {
    let (cfg, account) = {
        let b = backend.read();
        let account = b
            .cfg
            .account(account_id)
            .ok_or_else(|| Error::NotFound(format!("account {account_id}")))?;

        (b.cfg.clone(), account.clone())
    };

    connect_with(backend, account_id, async {
        provider::connect(&cfg, &account)
            .await?
            .ok_or_else(|| provider::credential_error(&account))
    })
    .await
}

/// Awaits `make` and caches its connection, unless the settings changed
/// (`conn_rev`) in the meantime. One connect at a time: a caller that waited
/// gets the connection the first one cached, so an account never has two
/// Proton sessions spending the same single-use refresh token.
async fn connect_with(
    mut backend: Signal<Backend>,
    account_id: &str,
    make: impl Future<Output = Result<Connection>>,
) -> Result<Arc<Connection>> {
    // ponytail: one lock for every account; a hung connect delays the others
    // until it times out. Per-account locks if that ever matters.
    static CONNECTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = CONNECTING.lock().await;

    if let Some(client) = backend.peek().clients.get(account_id) {
        return Ok(client.clone());
    }

    let rev = backend.peek().conn_rev;
    let client = Arc::new(make.await?);
    let mut b = backend.write();

    if b.conn_rev != rev {
        return Err(Error::Other(
            "settings changed while connecting; try again".into(),
        ));
    }

    b.clients.insert(account_id.to_string(), client.clone());

    Ok(client)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use dioxus::core::RuntimeGuard;

    use super::*;

    /// Serves one JMAP session response, after `release` is set.
    fn session_server(requested: Arc<AtomicBool>, release: Arc<AtomicBool>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/session", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];

            let _ = socket.read(&mut buf).unwrap();
            requested.store(true, Ordering::SeqCst);

            while !release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }

            let body = r#"{"apiUrl":"http://old.invalid/api","downloadUrl":"http://old.invalid/d","uploadUrl":"http://old.invalid/u","primaryAccounts":{"urn:ietf:params:jmap:mail":"a"}}"#;

            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        url
    }

    #[test]
    fn unreadable_config_is_never_saved() {
        let (cfg, error) = config_or_stand_in(Err(Error::Config("duplicate account id".into())));
        let broken = Backend {
            store: Arc::new(Store::open_memory().unwrap()),
            clients: HashMap::new(),
            cfg,
            ui_test: false,
            conn_rev: 0,
            config_error: error,
        };

        assert!(
            broken
                .config_error
                .as_deref()
                .unwrap()
                .contains("duplicate")
        );
        assert!(broken.config_for_save().is_err());

        let healthy = Backend {
            config_error: None,
            ..broken
        };

        assert!(healthy.config_for_save().is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_connection_is_not_cached_after_settings_save() {
        let (requested, release) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let mut cfg = Config::default();

        let session_url = session_server(requested.clone(), release.clone());
        cfg.accounts[0].session_url = session_url.clone();

        let dom = VirtualDom::new(VNode::empty);
        let _guard = RuntimeGuard::new(dom.runtime());
        let backend = Signal::new_in_scope(
            Backend {
                store: Arc::new(Store::open_memory().unwrap()),
                clients: HashMap::new(),
                cfg,
                ui_test: false,
                conn_rev: 0,
                config_error: None,
            },
            ScopeId::ROOT,
        );

        // What Settings Save does while the connection is pending.
        let mut saver = backend;
        let save = async move {
            while !requested.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            saver.write().cfg.accounts[0].session_url = "http://new.invalid/session".into();
            saver.write().reset_client();
            release.store(true, Ordering::SeqCst);
        };
        let connect = connect_with(backend, "default", async {
            Connection::connect_jmap(&session_url, "fake-token").await
        });
        let (connected, ()) = tokio::join!(connect, save);

        assert!(connected.is_err(), "stale client was returned");
        assert!(backend.peek().clients.is_empty(), "stale client was cached");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_callers_share_one_connection() {
        let (requested, release) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
        );
        let session_url = session_server(requested, release);
        let dom = VirtualDom::new(VNode::empty);
        let _guard = RuntimeGuard::new(dom.runtime());
        let backend = Signal::new_in_scope(
            Backend {
                store: Arc::new(Store::open_memory().unwrap()),
                clients: HashMap::new(),
                cfg: Config::default(),
                ui_test: false,
                conn_rev: 0,
                config_error: None,
            },
            ScopeId::ROOT,
        );
        let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make = || {
            let (made, url) = (made.clone(), session_url.clone());

            async move {
                made.fetch_add(1, Ordering::SeqCst);
                Connection::connect_jmap(&url, "fake-token").await
            }
        };

        // Two sessions holding one Proton refresh token: the second refresh
        // reuses a spent token and the server signs the account out.
        let (a, b) = tokio::join!(
            connect_with(backend, "default", make()),
            connect_with(backend, "default", make()),
        );

        assert_eq!(made.load(Ordering::SeqCst), 1, "connected twice");
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
    }

    #[tokio::test]
    async fn fake_pass_classifies_the_queue_and_files_it_locally() {
        let store = Store::open_memory().unwrap();

        fixtures::seed(&store).unwrap();

        let mut cfg = Config::default();

        cfg.classify.keep_in_inbox.push("receipts".into());

        let report = fake_classify_pass(&cfg, &store).await.unwrap();
        let row = store.classification("default", "fx-11").unwrap().unwrap();
        let receipts = store
            .top_level_mailbox_by_name("default", "Receipts")
            .unwrap()
            .unwrap();
        let email = store.email("default", "fx-11").unwrap().unwrap();
        let inbox = store
            .mailbox_by_role("default", "inbox")
            .unwrap()
            .unwrap()
            .id;

        assert_eq!((report.classified, report.moved), (1, 1), "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!((row.tray.as_str(), row.applied), ("receipts", true));
        assert_eq!(store.classify_queue_len("default").unwrap(), 0);
        assert!(email.mailbox_ids.contains(&receipts.id), "{email:?}");
        assert!(email.mailbox_ids.contains(&inbox), "{email:?}");

        // A tray that does not stay in the inbox leaves it.
        let store = Store::open_memory().unwrap();
        let mut cfg = Config::default();

        fixtures::seed(&store).unwrap();
        cfg.classify.keep_in_inbox.clear();
        fake_classify_pass(&cfg, &store).await.unwrap();

        let email = store.email("default", "fx-11").unwrap().unwrap();

        assert!(!email.mailbox_ids.contains(&inbox), "{email:?}");
    }

    #[tokio::test]
    async fn fake_jev_is_deterministic() {
        use posto_core::classify::state::{StateInput, state_block};

        let ask = |subject: &str| {
            state_block(&StateInput {
                from_name: "Ann",
                from_email: "ann@example.com",
                subject,
                received_at: "2026-10-12T10:00:00Z",
                has_unsubscribe: false,
                replied: false,
                body: "Hello? Is this an unrelated question mark in the body?",
            })
        };
        let lunch = FakeJev.classify(&ask("Lunch Thursday?")).await.unwrap();
        let invoice = FakeJev.classify(&ask("Your invoice")).await.unwrap();

        assert_eq!(lunch.tray, Tray::NEEDS_REPLY);
        assert_eq!(invoice.tray, Tray::new("receipts"));
        assert!(!lunch.unsure && !invoice.unsure);
        assert_eq!(
            lunch,
            FakeJev.classify(&ask("Lunch Thursday?")).await.unwrap()
        );
    }

    #[test]
    fn blocked_by_address_and_domain() {
        let list = vec!["Spam@Example.com".to_string(), "@junk.io".to_string()];

        assert!(sender_blocked(&list, "spam@example.com"));
        assert!(sender_blocked(&list, "anyone@junk.io"));
        assert!(!sender_blocked(&list, "other@example.com"));
        assert!(!sender_blocked(&[String::new()], "a@b.c"));
    }

    #[test]
    fn remote_policy() {
        let mut cfg = Config {
            trusted_senders: vec!["example.org".into(), "x@y.io".into()],
            ..Config::default()
        };

        assert!(remote_allowed(&cfg, "a@example.org"));
        assert!(remote_allowed(&cfg, "x@y.io"));
        assert!(!remote_allowed(&cfg, "b@other.org"));
        assert!(!remote_allowed(&cfg, "z@y.io"));
        assert!(!remote_allowed(&cfg, ""));

        cfg.remote_content = "always".into();

        assert!(remote_allowed(&cfg, "b@other.org"));
        assert!(remote_allowed(&cfg, ""));
    }

    /// A bare-bones email of `account` for key tests.
    fn email_of(account: &str, id: &str) -> EmailSummary {
        EmailSummary {
            account_id: account.into(),
            id: id.into(),
            thread_id: String::new(),
            received_at: "2026-10-12T10:00:00Z".into(),
            from: vec![],
            to: vec![],
            cc: vec![],
            subject: String::new(),
            preview: String::new(),
            size: 0,
            has_attachment: false,
            mailbox_ids: vec![],
            keywords: vec![],
        }
    }

    #[test]
    fn row_keys_are_account_scoped() {
        let rows = rows_for_display(&[email_of("a", "e1"), email_of("b", "e1")]);

        assert_eq!(rows[0].0, "a/e1");
        assert_eq!(rows[1].0, "b/e1");
        assert_ne!(rows[0].0, rows[1].0);
        assert_eq!(MsgRef::parse(&rows[0].0), Some(MsgRef::new("a", "e1")));
        assert_eq!(MsgRef::parse(&rows[1].0), Some(MsgRef::new("b", "e1")));
        assert_eq!(rows[1].1.account_id, "b");
    }

    #[test]
    fn refs_group_by_account_in_order() {
        let refs = [
            MsgRef::new("b", "e1"),
            MsgRef::new("a", "e1"),
            MsgRef::new("b", "e2"),
        ];

        assert_eq!(
            group_by_account(&refs),
            vec![
                ("b".to_string(), vec!["e1".to_string(), "e2".to_string()]),
                ("a".to_string(), vec!["e1".to_string()]),
            ]
        );
    }
}
