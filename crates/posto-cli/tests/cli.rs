//! Runs the built `posto` binary against a fresh data directory per test.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

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
