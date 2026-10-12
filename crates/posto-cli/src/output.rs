//! Rendering of command results: JSON (default), JSON lines, or plain text.

use std::io::Write;

use serde::Serialize;
use serde_json::Value;

/// Prints `value` to stdout. `jsonl` prints one compact line per list element;
/// `plain` prints aligned tables for mailbox/email lists and `key: value` lines
/// for objects (best effort, layout not stable).
pub fn emit<T: Serialize>(value: &T, plain: bool, jsonl: bool) {
    let value = serde_json::to_value(value).expect("command output is serializable");
    let text = if jsonl {
        if let Value::Array(items) = &value {
            // One line per element; an empty list writes nothing at all.
            let mut out = std::io::stdout().lock();

            for item in items {
                let _ = writeln!(out, "{item}");
            }

            return;
        }

        value.to_string()
    } else if plain {
        plain_text(&value)
    } else {
        serde_json::to_string_pretty(&value).expect("Value always serializes")
    };

    // A closed pipe (`posto ... | head`) is not an error worth reporting.
    let _ = writeln!(std::io::stdout().lock(), "{text}");
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn plain_text(value: &Value) -> String {
    match value {
        Value::Array(items) => match items.first() {
            None => String::new(),
            Some(first) if first.get("subject").is_some() => table(items.iter().map(email_row)),
            Some(first) if first.get("unread").is_some() => table(items.iter().map(mailbox_row)),
            Some(_) => items
                .iter()
                .map(plain_text)
                .collect::<Vec<_>>()
                .join("\n\n"),
        },
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}: {}", cell(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        other => cell(other),
    }
}

fn email_row(e: &Value) -> Vec<String> {
    let from = &e["from"][0];
    let sender = from["name"]
        .as_str()
        .or(from["email"].as_str())
        .unwrap_or_default();
    let has = |k: &str| {
        e["keywords"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == k))
    };
    let flags = [
        (!has("$seen"), "unread"),
        (has("$flagged"), "flagged"),
        (e["has_attachment"] == true, "attachment"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, name)| *name)
    .collect::<Vec<_>>()
    .join(",");

    vec![
        cell(&e["id"]),
        cell(&e["received_at"]),
        sender.into(),
        cell(&e["subject"]),
        flags,
    ]
}

fn mailbox_row(m: &Value) -> Vec<String> {
    ["id", "name", "role", "unread", "total"]
        .iter()
        .map(|k| cell(&m[k]))
        .collect()
}

/// Left-aligns every column but the last on its widest cell.
fn table(rows: impl Iterator<Item = Vec<String>>) -> String {
    let rows: Vec<_> = rows.collect();
    let cols = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..cols)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();

    rows.iter()
        .map(|r| {
            let line: Vec<String> = r
                .iter()
                .zip(&widths)
                .map(|(s, w)| format!("{s:<w$}"))
                .collect();
            line.join("  ").trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
