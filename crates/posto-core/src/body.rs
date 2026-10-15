//! Email bodies and attachments.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::provider::Connection;
use crate::store::{Attachment, EmailBody, Store};

/// Returns the cached body of `email_id` in `account`, or fetches it through
/// that account's `client` and caches it.
///
/// # Errors
///
/// Returns [`Error::NotFound`] when the body is not cached and either there
/// is no client or the server does not know the email.
pub async fn fetch_body(
    store: &Store,
    client: Option<&Connection>,
    account: &str,
    email_id: &str,
) -> Result<EmailBody> {
    if let Some(body) = store.body(account, email_id)? {
        return Ok(body);
    }

    let client = client.ok_or_else(|| Error::NotFound(email_id.to_string()))?;
    let body = client
        .body(email_id)
        .await?
        .ok_or_else(|| Error::NotFound(email_id.to_string()))?;

    store.put_body(account, &body)?;

    Ok(body)
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

/// Downloads `att` of email `email_id` into `dir`, never overwriting:
/// collisions get ` (2)`, ` (3)`, ...
///
/// # Errors
///
/// Returns an error when the download or the write fails.
pub async fn save_attachment(
    client: &Connection,
    email_id: &str,
    att: &Attachment,
    dir: &Path,
) -> Result<PathBuf> {
    let data = client.download(email_id, att).await?;
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
