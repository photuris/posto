//! Email bodies and attachments.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::jmap::Client;
use crate::store::{Attachment, EmailBody, Store};

/// Returns the cached body of `email_id` in `account`, or fetches it with
/// `Email/get` (through that account's `client`) and caches it.
///
/// # Errors
///
/// Returns [`Error::NotFound`] when the body is not cached and either there
/// is no client or the server does not know the email.
pub async fn fetch_body(
    store: &Store,
    client: Option<&Client>,
    account: &str,
    email_id: &str,
) -> Result<EmailBody> {
    if let Some(body) = store.body(account, email_id)? {
        return Ok(body);
    }

    let client = client.ok_or_else(|| Error::NotFound(email_id.to_string()))?;
    let r = client
        .call_one(
            "Email/get",
            json!({
                "accountId": client.session().account_id,
                "ids": [email_id],
                "properties": [
                    "textBody", "htmlBody", "bodyValues", "attachments",
                    "inReplyTo", "references", "messageId",
                ],
                "fetchHTMLBodyValues": true,
                "fetchTextBodyValues": true,
                "maxBodyValueBytes": 2_000_000,
            }),
        )
        .await?;
    let email = r["list"]
        .get(0)
        .ok_or_else(|| Error::NotFound(email_id.to_string()))?;
    let body = EmailBody {
        email_id: email_id.to_string(),
        html: joined(email, "htmlBody", "text/html"),
        text: joined(email, "textBody", "text/plain"),
        attachments: email["attachments"]
            .as_array()
            .map(|a| a.iter().map(attachment).collect())
            .unwrap_or_default(),
        in_reply_to: strings(&email["inReplyTo"]),
        references: strings(&email["references"]),
        message_id: strings(&email["messageId"]),
    };

    store.put_body(account, &body)?;

    Ok(body)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn attachment(v: &Value) -> Attachment {
    Attachment {
        blob_id: v["blobId"].as_str().unwrap_or_default().to_string(),
        name: v["name"].as_str().unwrap_or("attachment").to_string(),
        mime: v["type"].as_str().unwrap_or_default().to_string(),
        size: v["size"].as_i64().unwrap_or(0),
    }
}

/// Concatenates the values of the `mime` parts listed under `key`, if any.
fn joined(email: &Value, key: &str, mime: &str) -> Option<String> {
    let parts: Vec<&Value> = email[key]
        .as_array()?
        .iter()
        .filter(|p| p["type"].as_str() == Some(mime))
        .collect();

    if parts.is_empty() {
        return None;
    }

    Some(
        parts
            .iter()
            .filter_map(|p| email["bodyValues"][p["partId"].as_str()?]["value"].as_str())
            .collect(),
    )
}

/// Makes a server-supplied file name safe to use as a single path component.
fn sanitise(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim();

    match clean {
        "" | "." | ".." => "attachment".to_string(),
        _ => clean.to_string(),
    }
}

/// Downloads `att` into `dir`, never overwriting: collisions get ` (2)`, ` (3)`, ...
///
/// # Errors
///
/// Returns an error when the download or the write fails.
pub async fn save_attachment(client: &Client, att: &Attachment, dir: &Path) -> Result<PathBuf> {
    let data = client
        .download_blob(&att.blob_id, &att.name, &att.mime)
        .await?;
    let name = PathBuf::from(sanitise(&att.name));
    let stem = name
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let ext = name
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()));
    let mut n = 1;

    loop {
        let candidate = match n {
            1 => name.clone(),
            _ => PathBuf::from(format!("{stem} ({n}){}", ext.as_deref().unwrap_or(""))),
        };
        let path = dir.join(candidate);

        // Claim the name atomically, then let tokio write the data.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => {
                tokio::fs::write(&path, &data).await?;

                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e.into()),
        }
    }
}
