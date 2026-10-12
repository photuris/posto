//! Runs the built `posto` binary against a fresh data directory per test.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

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
        let mut child = Command::new(env!("CARGO_BIN_EXE_posto"))
            .args(args)
            .env("POSTO_DATA_DIR", &self.0)
            .env("XDG_STATE_HOME", &self.0)
            .env_remove("POSTO_TOKEN")
            .env_remove("TYPESAFE_API_KEY")
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

    assert_eq!(list.len(), 9);
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
