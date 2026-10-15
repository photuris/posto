//! Runs the built `posto` binary against a fresh data directory per test.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use posto_core::store::{EmailBody, EmailSummary, Store};
use serde_json::{Value, json};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A throwaway directory used as both the data dir and `XDG_STATE_HOME`.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Sandbox {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("posto-cli-test-{}-{n}", std::process::id()));

        std::fs::create_dir_all(&dir).unwrap();

        Sandbox(dir)
    }

    fn run_with(&self, args: &[&str], stdin: Option<&str>) -> Output {
        self.run_env(args, stdin, &[])
    }

    /// Like `run_with`, with extra environment variables set on top.
    fn run_env(&self, args: &[&str], stdin: Option<&str>, extra: &[(&str, &str)]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_posto"))
            .args(args)
            .env("POSTO_DATA_DIR", &self.0)
            .env("XDG_STATE_HOME", &self.0)
            .env_remove("POSTO_TOKEN")
            .env_remove("TYPESAFE_API_KEY")
            .env_remove("POSTO_PROTON_APP_VERSION")
            .envs(extra.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        // Dropping stdin closes it, so commands that read it see EOF.
        let mut pipe = child.stdin.take().unwrap();

        if let Some(text) = stdin {
            pipe.write_all(text.as_bytes()).unwrap();
        }

        drop(pipe);
        child.wait_with_output().unwrap()
    }

    /// Runs a command that must succeed and returns its stdout as JSON.
    fn json(&self, args: &[&str]) -> Value {
        let out = self.run_with(args, None);

        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        serde_json::from_slice(&out.stdout).expect("stdout is JSON")
    }

    fn seeded() -> Sandbox {
        let sb = Sandbox::new();

        sb.json(&["dev", "seed"]);

        sb
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `error` field of the JSON a failed command printed on stderr.
fn error_text(out: &Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    let v: Value = serde_json::from_str(err.trim())
        .unwrap_or_else(|_| panic!("stderr is not one JSON object: {err}"));

    v["error"].as_str().expect("error field").to_owned()
}

#[test]
fn account_show_defaults() {
    let sb = Sandbox::new();
    let out = sb.run_with(&["account", "show"], None);

    assert_eq!(out.status.code(), Some(0));

    let v: Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(v["has_token"], false);
    assert_eq!(v["email"], "");
}

#[test]
fn set_token_via_stdin_then_show() {
    let sb = Sandbox::new();
    let set = sb.run_with(&["account", "set-token", "--token-stdin"], Some("abc"));

    assert_eq!(set.status.code(), Some(0));

    let out = sb.run_with(&["account", "show"], None);
    let text = String::from_utf8(out.stdout).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();

    assert_eq!(v["has_token"], true);
    assert!(!text.contains("abc"));
}

#[test]
fn mailbox_list_after_seed() {
    let v = Sandbox::seeded().json(&["mailbox", "list"]);
    let list = v.as_array().unwrap();

    assert_eq!(list.len(), 13);
    assert_eq!(list[0]["role"], "inbox");
    assert_eq!(list[0]["account_id"], "default");
}

#[test]
fn account_add_list_remove_and_scoped_token() {
    let sb = Sandbox::seeded();
    let added = sb.json(&["account", "add", "--id", "work", "--email", "w@x.io"]);

    assert_eq!(added["id"], "work");
    assert_eq!(sb.json(&["account", "list"]).as_array().unwrap().len(), 2);

    let set = sb.run_with(
        &["account", "set-token", "--account", "work", "--token-stdin"],
        Some("work-secret"),
    );

    assert_eq!(set.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&set.stdout).contains("work-secret"));
    assert_eq!(
        sb.json(&["account", "show", "--account", "work"])["has_token"],
        true
    );
    assert_eq!(sb.json(&["account", "show"])["has_token"], false);

    let secrets = std::fs::read_to_string(sb.0.join("secrets.toml")).unwrap();

    assert!(secrets.contains("[tokens]"), "{secrets}");

    sb.json(&["account", "remove", "work"]);

    let left = sb.json(&["account", "list"]);

    assert_eq!(left.as_array().unwrap().len(), 1);
    assert_eq!(left[0]["id"], "default");
    assert!(
        !std::fs::read_to_string(sb.0.join("secrets.toml"))
            .unwrap()
            .contains("work-secret")
    );

    // The last account stays; an invalid or duplicate id is a config error.
    assert_eq!(
        sb.run_with(&["account", "remove", "default"], None)
            .status
            .code(),
        Some(3)
    );
    assert_eq!(
        sb.run_with(
            &["account", "add", "--id", "Bad Id", "--email", "x@y.z"],
            None
        )
        .status
        .code(),
        Some(3)
    );
    assert_eq!(
        sb.run_with(
            &["account", "add", "--id", "default", "--email", "x@y.z"],
            None
        )
        .status
        .code(),
        Some(3)
    );
}

#[test]
fn proton_account_add_show_and_sync_without_login() {
    let sb = Sandbox::new();
    let added = sb.json(&[
        "account",
        "add",
        "--id",
        "pm",
        "--email",
        "me@proton.me",
        "--provider",
        "proton",
        "--username",
        "me",
    ]);

    assert_eq!(added["provider"], "proton");
    assert_eq!(added["username"], "me");
    assert_eq!(added["has_token"], false);

    let list = sb.json(&["account", "list"]);

    assert_eq!(list[0]["id"], "default");
    assert_eq!(list[0]["provider"], "jmap");

    let sync = sb.run_with(&["sync", "run", "--account", "pm"], None);

    assert_eq!(sync.status.code(), Some(3));
    assert_eq!(
        error_text(&sync),
        "not configured: proton login for account pm"
    );

    let set = sb.run_with(
        &["account", "set-token", "--account", "pm", "--token-stdin"],
        Some("x"),
    );

    assert_eq!(set.status.code(), Some(3));
    assert_eq!(
        error_text(&set),
        "config: account pm is a Proton account: use `account login`"
    );

    let bad = sb.run_with(
        &[
            "account",
            "add",
            "--id",
            "bad",
            "--email",
            "a@b",
            "--provider",
            "imap",
        ],
        None,
    );

    assert_eq!(bad.status.code(), Some(2));

    sb.json(&["account", "remove", "pm"]);

    let left = sb.json(&["account", "list"]);

    assert_eq!(left.as_array().unwrap().len(), 1);
    assert_eq!(left[0]["id"], "default");
}

#[test]
fn proton_login_validates_inputs_before_any_network() {
    let sb = Sandbox::new();
    let env = [("POSTO_PROTON_APP_VERSION", "posto-test@0.0.0")];
    let started = std::time::Instant::now();

    sb.json(&[
        "account",
        "add",
        "--id",
        "pm",
        "--email",
        "me@proton.me",
        "--provider",
        "proton",
    ]);

    // (args, stdin, expected `error` text)
    let cases: [(&[&str], &str, &str); 3] = [
        (
            &[
                "account",
                "login",
                "--account",
                "default",
                "--password-stdin",
            ],
            "pw\n",
            "config: account default is not a Proton account",
        ),
        (
            &["account", "login", "--account", "pm", "--password-stdin"],
            "",
            "config: empty password on stdin",
        ),
        (
            &[
                "account",
                "login",
                "--account",
                "pm",
                "--password-stdin",
                "--two-password",
            ],
            "pw\n",
            "config: mailbox password missing: give it on the second stdin line",
        ),
    ];

    for (args, stdin, expected) in cases {
        let out = sb.run_env(args, Some(stdin), &env);
        let err = String::from_utf8_lossy(&out.stderr);

        assert_eq!(out.status.code(), Some(3), "{args:?}: {err}");
        assert_eq!(error_text(&out), expected, "{args:?}");
        assert!(!err.contains("transport") && !err.contains("http"), "{err}");
    }

    // An empty `proton.app_version` and no variable.
    std::fs::write(
        sb.0.join("config.toml"),
        "[[accounts]]\nid = \"default\"\n\n[[accounts]]\nid = \"pm\"\nemail = \"me@proton.me\"\nprovider = \"proton\"\n\n[proton]\napp_version = \"\"\n",
    )
    .unwrap();

    let out = sb.run_with(
        &["account", "login", "--account", "pm", "--password-stdin"],
        Some("pw\n"),
    );
    let err = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(error_text(&out), "not configured: proton.app_version");
    assert!(!err.contains("transport") && !err.contains("http"), "{err}");
    assert!(
        started.elapsed().as_secs() < 10,
        "no command waits on a network"
    );
}

#[test]
fn account_remove_deletes_its_cached_mail() {
    let sb = Sandbox::seeded();

    sb.json(&["account", "add", "--id", "work", "--email", "w@x.io"]);

    let store = Store::open(&sb.0.join("posto.db")).unwrap();

    store.upsert_emails(&[work_email("w1")]).unwrap();
    drop(store);

    assert_eq!(
        sb.json(&["email", "list", "--account", "work"])
            .as_array()
            .unwrap()
            .len(),
        1
    );

    sb.json(&["account", "remove", "work"]);

    assert_eq!(sb.json(&["email", "list"]).as_array().unwrap().len(), 14);
}

/// A message of the account `work`, for tests that need a second account.
fn work_email(id: &str) -> EmailSummary {
    EmailSummary {
        account_id: "work".into(),
        id: id.into(),
        thread_id: format!("t-{id}"),
        received_at: "2026-10-10T10:00:00Z".into(),
        from: vec![],
        to: vec![],
        cc: vec![],
        subject: "work mail".into(),
        preview: String::new(),
        size: 1,
        has_attachment: false,
        mailbox_ids: vec![],
        keywords: vec![],
    }
}

#[test]
fn email_ids_resolve_across_accounts() {
    let sb = Sandbox::seeded();

    sb.json(&["account", "add", "--id", "work", "--email", "w@x.io"]);

    let store = Store::open(&sb.0.join("posto.db")).unwrap();

    store
        .upsert_emails(&[work_email("fx-01"), work_email("w2")])
        .unwrap();

    for id in ["fx-01", "w2"] {
        store
            .put_body(
                "work",
                &EmailBody {
                    email_id: id.into(),
                    html: None,
                    text: Some("hi".into()),
                    attachments: vec![],
                    in_reply_to: vec![],
                    references: vec![],
                    message_id: vec![],
                },
            )
            .unwrap();
    }

    drop(store);

    // `fx-01` exists in both accounts: a bare id is ambiguous.
    let out = sb.run_with(&["email", "show", "fx-01"], None);
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();

    assert_eq!(out.status.code(), Some(1));
    assert!(
        err["error"].as_str().unwrap().contains("ambiguous"),
        "{err}"
    );
    assert_eq!(
        sb.json(&["email", "show", "fx-01", "--account", "work"])["summary"]["account_id"],
        "work"
    );
    assert_eq!(
        sb.json(&["email", "show", "fx-01", "--account", "default"])["summary"]["account_id"],
        "default"
    );

    // A unique bare id needs no flag.
    assert_eq!(
        sb.json(&["email", "show", "w2"])["summary"]["account_id"],
        "work"
    );

    let all = sb.json(&["email", "list", "--limit", "100"]);

    assert_eq!(all.as_array().unwrap().len(), 16);
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .all(|e| e["account_id"].is_string())
    );
    assert_eq!(
        sb.json(&["email", "list", "--account", "work"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        sb.json(&["email", "search", "work"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        sb.json(&["email", "search", "work", "--account", "default"])
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let out = sb.run_with(&["email", "show", "fx-01", "--account", "nope"], None);

    assert_eq!(out.status.code(), Some(6));
}

#[test]
fn legacy_files_load_through_the_cli() {
    let sb = Sandbox::new();

    std::fs::write(
        sb.0.join("config.toml"),
        "[account]\nemail = \"old@x.io\"\n",
    )
    .unwrap();
    std::fs::write(sb.0.join("secrets.toml"), "token = \"legacy\"\n").unwrap();

    let list = sb.json(&["account", "list"]);

    assert_eq!(list[0]["id"], "default");
    assert_eq!(list[0]["email"], "old@x.io");
    assert_eq!(list[0]["has_token"], true);
}

#[test]
fn email_list_filters_by_mailbox_name() {
    let sb = Sandbox::seeded();
    let news = sb.json(&["email", "list", "--mailbox", "Newsletters"]);
    let news = news.as_array().unwrap();

    assert!(!news.is_empty());
    assert!(news.iter().all(|e| {
        e["mailbox_ids"]
            .as_array()
            .unwrap()
            .contains(&json!("mb-newsletters"))
    }));

    let unread = sb.json(&["email", "list", "--unread"]);
    let unread = unread.as_array().unwrap();

    assert!(!unread.is_empty());
    assert!(
        unread
            .iter()
            .all(|e| !e["keywords"].as_array().unwrap().contains(&json!("$seen")))
    );
    assert_eq!(
        sb.json(&["email", "list", "--limit", "3"])
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn email_show_text_mode() {
    let v = Sandbox::seeded().json(&["email", "show", "fx-01", "--text"]);

    assert_eq!(v["summary"]["id"], "fx-01");
    assert!(v["body"]["html"].is_null());
    assert!(!v["body"]["text"].as_str().unwrap().is_empty());
}

#[test]
fn email_search() {
    let v = Sandbox::seeded().json(&["email", "search", "Linear"]);

    assert!(v.as_array().unwrap().iter().any(|e| e["id"] == "fx-01"));
}

#[test]
fn jsonl_empty_list_prints_nothing() {
    let out = Sandbox::seeded().run_with(&["email", "search", "zzzz", "--jsonl"], None);

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"", "stdout was {:?}", out.stdout);
}

#[test]
fn archive_offline_mirrors() {
    let sb = Sandbox::seeded();
    let v = sb.json(&["email", "archive", "fx-01"]);

    assert_eq!(v["applied"], 1);
    assert_eq!(v["offline"], true);
    assert_eq!(
        sb.json(&["email", "show", "fx-01"])["summary"]["mailbox_ids"],
        json!(["mb-archive"])
    );
}

#[test]
fn move_by_name_and_delete() {
    let sb = Sandbox::seeded();
    let mailbox_ids =
        |sb: &Sandbox| sb.json(&["email", "show", "fx-02"])["summary"]["mailbox_ids"].clone();

    sb.json(&["email", "move", "Receipts", "fx-02"]);
    assert_eq!(mailbox_ids(&sb), json!(["mb-receipts"]));

    sb.json(&["email", "delete", "fx-02"]);
    assert_eq!(mailbox_ids(&sb), json!(["mb-trash"]));
}

#[test]
fn label_and_unlabel_mirror_offline() {
    let sb = Sandbox::seeded();
    let mailbox_ids =
        |sb: &Sandbox| sb.json(&["email", "show", "fx-02"])["summary"]["mailbox_ids"].clone();

    let v = sb.json(&["email", "label", "mb-newsletters", "fx-02"]);

    assert_eq!(v["applied"], 1);
    assert_eq!(v["action"], "label");
    assert_eq!(v["offline"], true);

    let ids = mailbox_ids(&sb);

    assert!(
        ids.as_array().unwrap().contains(&json!("mb-inbox")),
        "{ids}"
    );
    assert!(
        ids.as_array().unwrap().contains(&json!("mb-newsletters")),
        "{ids}"
    );

    sb.json(&["email", "unlabel", "mb-newsletters", "fx-02"]);
    assert_eq!(mailbox_ids(&sb), json!(["mb-inbox"]));

    let v = sb.json(&["email", "unlabel", "mb-inbox", "fx-02"]);

    assert_eq!(v["applied"], 0);
    assert_eq!(mailbox_ids(&sb), json!(["mb-inbox"]));
}

#[test]
fn unknown_mailbox_exits_6() {
    let out = Sandbox::seeded().run_with(&["email", "move", "Nope", "fx-01"], None);
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();

    assert_eq!(out.status.code(), Some(6));
    assert_eq!(err["code"], 6);
}

#[test]
fn sync_without_token_exits_3() {
    let out = Sandbox::new().run_with(&["sync", "run"], None);
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();

    assert_eq!(out.status.code(), Some(3));
    assert_eq!(err["code"], 3);
}

#[test]
fn sync_with_several_accounts_reports_each_missing_token() {
    let sb = Sandbox::new();

    sb.json(&["account", "add", "--id", "work", "--email", "w@x.io"]);

    let out = sb.run_with(&["sync", "run"], None);
    let reports: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout is not JSON: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    let seen: Vec<_> = reports
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["account_id"].as_str().unwrap(),
                r["error"].as_str().unwrap(),
            )
        })
        .collect();

    assert_eq!(seen, [("default", "no token"), ("work", "no token")]);
}

#[test]
fn sync_without_enabled_account_says_so() {
    let sb = Sandbox::new();

    sb.json(&[
        "account",
        "set",
        "--account",
        "default",
        "--enabled",
        "false",
    ]);

    let out = sb.run_with(&["sync", "run"], None);
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();

    assert_eq!(out.status.code(), Some(3));
    assert_eq!(err["error"], "not configured: no enabled account");
}

#[test]
fn account_identities_and_send_from_flag() {
    let sb = Sandbox::seeded();
    let ids = sb.json(&["account", "identities"]);
    let ids = ids.as_array().unwrap();

    assert_eq!(ids.len(), 2);
    assert!(ids.iter().any(|i| i["email"] == "shop@example.com"));

    let body = sb.0.join("body.txt");

    std::fs::write(&body, "hi").unwrap();

    let out = sb.run_with(
        &[
            "email",
            "send",
            "--from",
            "nobody@example.com",
            "--to",
            "a@b.c",
            "--subject",
            "s",
            "--body-file",
            body.to_str().unwrap(),
        ],
        None,
    );

    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn usage_error_exits_2() {
    let out = Sandbox::new().run_with(&["email"], None);

    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn shortcuts_list_and_theme_show() {
    let sb = Sandbox::new();
    let shortcuts = sb.json(&["shortcuts", "list"]);

    assert!(
        shortcuts
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["action"] == "archive")
    );
    assert!(sb.json(&["theme", "show"]).is_null());
}

#[test]
fn help_has_output_schema() {
    let out = Sandbox::new().run_with(&["email", "show", "--help"], None);

    assert!(String::from_utf8(out.stdout).unwrap().contains("Output:"));
}

/// Enables the classifier for the sandbox's default account, pointing its
/// session URL at `session_url`.
fn enable_classifier(sb: &Sandbox, session_url: &str) {
    std::fs::write(
        sb.0.join("config.toml"),
        format!(
            "[[accounts]]\nid = \"default\"\nemail = \"me@example.com\"\n\
             session_url = \"{session_url}\"\n\n[classify]\nenabled = true\n"
        ),
    )
    .unwrap();
}

#[test]
fn classify_status_and_correct_offline() {
    let sb = Sandbox::seeded();
    let status = sb.json(&["classify", "status"]);
    let account = &status["accounts"][0];

    assert_eq!(account["id"], "default");
    assert_eq!(account["classified"], 2);
    assert_eq!(account["unsure"], 1);
    assert_eq!(account["queued"], 1);
    assert_eq!(account["failed"], 0);
    assert_eq!(account["pending_apply"], 0);
    assert_eq!(account["by_tray"]["promotional"], 1);

    let done = sb.json(&["classify", "correct", "fx-01", "updates"]);

    assert_eq!(done["offline"], true);
    assert_eq!(
        sb.json(&["classify", "show", "fx-01"])["corrected_tray"],
        "updates"
    );

    let updates = sb
        .json(&["mailbox", "list"])
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "Updates")
        .expect("seed has an Updates mailbox")["id"]
        .clone();
    let shown = sb.json(&["email", "show", "fx-01"]);

    assert!(
        shown["summary"]["mailbox_ids"]
            .as_array()
            .unwrap()
            .contains(&updates),
        "{shown}"
    );
    // Offline, the move waits for the next run.
    assert_eq!(
        sb.json(&["classify", "status"])["accounts"][0]["pending_apply"],
        1
    );

    let bad = sb.run_with(&["classify", "correct", "fx-01", "nonsense"], None);

    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn classify_run_requires_key() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/session", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let sb = Sandbox::seeded();

    // Counts every connection and hangs up on it, so a command that does
    // reach the server fails fast instead of waiting for an answer.
    let counter = hits.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });

    enable_classifier(&sb, &url);
    sb.run_with(&["account", "set-token", "--token-stdin"], Some("dummy"));

    let out = sb.run_with(&["classify", "run"], None);
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();

    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(err["error"], "not configured: typesafe key");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "connected before the key check"
    );
}

#[test]
fn classify_dry_run_writes_nothing() {
    let sb = Sandbox::seeded();

    enable_classifier(&sb, "http://127.0.0.1:9/session");
    // A pending correction, so the dry run has an entry of each kind.
    sb.json(&["classify", "correct", "fx-01", "updates"]);

    let before = sb.json(&["classify", "status"]);
    let db = sb.0.join("posto.db");
    let bytes = std::fs::read(&db).unwrap();
    let dry = sb.json(&["classify", "run", "--dry-run", "--backfill", "3"]);

    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["pending_apply"][0]["email_id"], "fx-01");
    assert!(
        dry["pending_apply"][0]["state"]
            .as_str()
            .is_some_and(|s| s.contains("Subject:")),
        "{dry}"
    );
    assert_eq!(dry["queued"][0]["email_id"], "fx-11");
    assert!(
        dry["queued"][0]["state"]
            .as_str()
            .unwrap()
            .contains("Subject: Q3 VAT")
    );
    assert_eq!(dry["backfill"].as_array().unwrap().len(), 3);
    assert!(
        std::fs::read(&db).unwrap() == bytes,
        "the dry run changed the database file"
    );
    assert_eq!(sb.json(&["classify", "status"]), before);
    assert_eq!(before["accounts"][0]["queued"], 1);
    assert_eq!(before["accounts"][0]["classified"], 2);

    // A writable open would rebuild an old cache, which the comparison above
    // cannot see on a current one (the open rewrites the same bytes).
    let old = Sandbox::seeded();

    enable_classifier(&old, "http://127.0.0.1:9/session");
    Store::open(&old.0.join("posto.db"))
        .unwrap()
        .set_state("schema_version", "1")
        .unwrap();

    let old_bytes = std::fs::read(old.0.join("posto.db")).unwrap();
    let out = old.run_with(&["classify", "run", "--dry-run"], None);
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();

    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err["error"].as_str().unwrap().contains("schema 1"), "{err}");
    assert!(
        std::fs::read(old.0.join("posto.db")).unwrap() == old_bytes,
        "the dry run rebuilt an old cache"
    );
}

#[test]
fn classify_dry_run_does_not_create_or_rebuild_the_cache() {
    let fresh = Sandbox::new();

    enable_classifier(&fresh, "http://127.0.0.1:9/session");

    let out = fresh.run_with(&["classify", "run", "--dry-run"], None);

    assert_eq!(out.status.code(), Some(6), "{out:?}");
    assert!(
        !fresh.0.join("posto.db").exists(),
        "dry run created the cache"
    );

    let old = Sandbox::seeded();

    enable_classifier(&old, "http://127.0.0.1:9/session");
    Store::open(&old.0.join("posto.db"))
        .unwrap()
        .set_state("schema_version", "2")
        .unwrap();

    let bytes = std::fs::read(old.0.join("posto.db")).unwrap();
    let out = old.run_with(&["classify", "run", "--dry-run"], None);
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();

    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err["error"].as_str().unwrap().contains("schema 2"), "{err}");
    assert!(
        std::fs::read(old.0.join("posto.db")).unwrap() == bytes,
        "the dry run rebuilt an old cache"
    );
}

#[test]
fn classify_correct_keeps_intent_when_the_server_is_unreachable() {
    let sb = Sandbox::seeded();

    enable_classifier(&sb, "http://127.0.0.1:9/session");
    sb.run_with(&["account", "set-token", "--token-stdin"], Some("dummy"));

    let done = sb.json(&["classify", "correct", "fx-01", "updates"]);

    assert_eq!(done["applied"], false, "{done}");
    assert_eq!(done["pending"], true, "{done}");
    assert_eq!(
        sb.json(&["classify", "show", "fx-01"])["corrected_tray"],
        "updates"
    );
}

#[test]
fn destroy_requires_yes_and_mirrors_offline() {
    let sb = Sandbox::seeded();
    let refused = sb.run_with(&["email", "destroy", "fx-01"], None);

    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("refusing to destroy without --yes"));
    // Refusing changed nothing.
    assert_eq!(
        sb.run_with(&["email", "show", "fx-01"], None).status.code(),
        Some(0)
    );

    let done = sb.json(&["email", "destroy", "fx-01", "--yes"]);

    assert_eq!(done["applied"], 1);
    assert_eq!(done["action"], "destroy");
    assert_eq!(done["offline"], true);
    assert_eq!(
        sb.run_with(&["email", "show", "fx-01"], None).status.code(),
        Some(6)
    );
}

/// Each provider connects with its own stored credentials. Both runs stay on
/// the machine: the JMAP session URL is a closed loopback port and the Proton
/// session blob is unreadable, so no request leaves the process.
#[test]
fn credentialed_accounts_connect_with_their_own_provider() {
    let sb = Sandbox::new();
    let env = [("POSTO_PROTON_APP_VERSION", "posto-test@0.0.0")];

    sb.json(&[
        "account",
        "add",
        "--id",
        "jm",
        "--email",
        "j@x.io",
        "--session-url",
        "http://127.0.0.1:1/session",
    ]);
    sb.json(&[
        "account",
        "add",
        "--id",
        "pm",
        "--email",
        "me@proton.me",
        "--provider",
        "proton",
    ]);
    std::fs::write(
        sb.0.join("secrets.toml"),
        "[tokens]\njm = \"tok\"\n\n[proton.pm]\nsession = \"not json\"\nmailbox_password = \"mp\"\ngeneration = 1\n",
    )
    .unwrap();

    assert_eq!(
        sb.json(&["account", "show", "--account", "jm"])["has_token"],
        true
    );
    assert_eq!(
        sb.json(&["account", "show", "--account", "pm"])["has_token"],
        true
    );

    let jmap = sb.run_env(&["sync", "run", "--account", "jm"], None, &env);

    assert_eq!(jmap.status.code(), Some(5), "{}", error_text(&jmap));
    assert!(
        error_text(&jmap).starts_with("http: "),
        "{}",
        error_text(&jmap)
    );

    let proton = sb.run_env(&["sync", "run", "--account", "pm"], None, &env);

    assert_eq!(proton.status.code(), Some(3));
    assert_eq!(
        error_text(&proton),
        "not configured: proton session for this account is unreadable; sign in again"
    );
}
