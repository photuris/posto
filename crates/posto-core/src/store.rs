//! SQLite-backed local cache of mailboxes, emails and bodies.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Database schema; every statement is idempotent.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sync_state(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS mailboxes(id TEXT PRIMARY KEY, name TEXT NOT NULL, role TEXT, parent_id TEXT, sort_order INTEGER NOT NULL DEFAULT 0, total INTEGER NOT NULL DEFAULT 0, unread INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS emails(id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, received_at TEXT NOT NULL, from_json TEXT NOT NULL, to_json TEXT NOT NULL, cc_json TEXT NOT NULL, subject TEXT NOT NULL, preview TEXT NOT NULL, size INTEGER NOT NULL, has_attachment INTEGER NOT NULL, mailbox_ids_json TEXT NOT NULL, keywords_json TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS emails_received ON emails(received_at DESC);
CREATE TABLE IF NOT EXISTS email_mailboxes(email_id TEXT NOT NULL, mailbox_id TEXT NOT NULL, PRIMARY KEY(email_id, mailbox_id));
CREATE TABLE IF NOT EXISTS bodies(email_id TEXT PRIMARY KEY, html TEXT, text TEXT, attachments_json TEXT NOT NULL, in_reply_to_json TEXT NOT NULL, references_json TEXT NOT NULL, message_id_json TEXT NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS emails_fts USING fts5(email_id UNINDEXED, subject, from_text, preview, body_text);
";

/// Pragmas applied to every connection (journal mode is set separately for
/// in-memory databases, which cannot use WAL).
const PRAGMAS: &str = "
PRAGMA busy_timeout=5000;
PRAGMA foreign_keys=ON;
PRAGMA synchronous=NORMAL;
";

/// Column list matching [`email_from_row`].
const EMAIL_COLS: &str = "e.id, e.thread_id, e.received_at, e.from_json, \
     e.to_json, e.cc_json, e.subject, e.preview, e.size, e.has_attachment, \
     e.mailbox_ids_json, e.keywords_json";

/// SQL predicate: true when the email e has no `$seen` keyword.
const UNREAD_SQL: &str = "NOT EXISTS (SELECT 1 FROM json_each(e.keywords_json) \
     WHERE value = '$seen')";

/// Page size used when a query passes limit 0.
const DEFAULT_LIMIT: u32 = 200;

/// A mailbox (folder or label) with its cached counts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Mailbox {
    /// Server-assigned id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// JMAP role such as `inbox`, if any.
    pub role: Option<String>,
    /// Parent mailbox id, if nested.
    pub parent_id: Option<String>,
    /// Server sort order.
    pub sort_order: i64,
    /// Total emails.
    pub total: i64,
    /// Unread emails.
    pub unread: i64,
}

/// One name/address pair.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Address {
    /// Display name, if any.
    pub name: Option<String>,
    /// Email address.
    pub email: String,
}

/// List-level view of an email.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EmailSummary {
    /// Server-assigned id.
    pub id: String,
    /// Thread id.
    pub thread_id: String,
    /// RFC 3339 UTC timestamp, as JMAP gives it.
    pub received_at: String,
    /// Senders.
    pub from: Vec<Address>,
    /// Recipients.
    pub to: Vec<Address>,
    /// Carbon-copy recipients.
    pub cc: Vec<Address>,
    /// Subject line.
    pub subject: String,
    /// Server-generated preview text.
    pub preview: String,
    /// Size in bytes.
    pub size: i64,
    /// Whether the email has attachments.
    pub has_attachment: bool,
    /// Mailboxes the email is in.
    pub mailbox_ids: Vec<String>,
    /// JMAP keywords such as `$seen`, `$flagged`, `$draft`.
    pub keywords: Vec<String>,
}

impl EmailSummary {
    /// True when the email lacks the `$seen` keyword.
    pub fn is_unread(&self) -> bool {
        !self.keywords.iter().any(|k| k == "$seen")
    }

    /// True when the email carries the `$flagged` keyword.
    pub fn is_flagged(&self) -> bool {
        self.keywords.iter().any(|k| k == "$flagged")
    }
}

/// One attachment of an email.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    /// Blob id used for download.
    pub blob_id: String,
    /// File name.
    pub name: String,
    /// MIME type.
    pub mime: String,
    /// Size in bytes.
    pub size: i64,
}

/// Fetched body of an email.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EmailBody {
    /// The email this body belongs to.
    pub email_id: String,
    /// Raw, unsanitised HTML body.
    pub html: Option<String>,
    /// Plain-text body.
    pub text: Option<String>,
    /// Attachments.
    pub attachments: Vec<Attachment>,
    /// `In-Reply-To` ids (JMAP form, no angle brackets).
    pub in_reply_to: Vec<String>,
    /// `References` ids (JMAP form).
    pub references: Vec<String>,
    /// `Message-ID` values (JMAP form).
    pub message_id: Vec<String>,
}

/// Parameters of [`Store::list_emails`].
#[derive(Clone, Debug, Default)]
pub struct ListQuery {
    /// Restrict to one mailbox.
    pub mailbox_id: Option<String>,
    /// Maximum rows; 0 means 200.
    pub limit: u32,
    /// Rows to skip.
    pub offset: u32,
    /// Only emails without `$seen`.
    pub unread_only: bool,
}

/// Handle to the local database; safe to share between threads.
pub struct Store {
    /// The single connection, serialised by a mutex.
    conn: Mutex<Connection>,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Serialises a value to a JSON string column.
fn to_json<T: Serialize + ?Sized>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Other(e.to_string()))
}

/// Reads and deserialises a JSON string column.
fn json_col<T: DeserializeOwned>(row: &Row, idx: usize) -> rusqlite::Result<T> {
    let text: String = row.get(idx)?;

    serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Maps a row selected with [`EMAIL_COLS`] to an [`EmailSummary`].
fn email_from_row(row: &Row) -> rusqlite::Result<EmailSummary> {
    Ok(EmailSummary {
        id: row.get(0)?,
        thread_id: row.get(1)?,
        received_at: row.get(2)?,
        from: json_col(row, 3)?,
        to: json_col(row, 4)?,
        cc: json_col(row, 5)?,
        subject: row.get(6)?,
        preview: row.get(7)?,
        size: row.get(8)?,
        has_attachment: row.get(9)?,
        mailbox_ids: json_col(row, 10)?,
        keywords: json_col(row, 11)?,
    })
}

/// Maps a `mailboxes` row to a [`Mailbox`].
fn mailbox_from_row(row: &Row) -> rusqlite::Result<Mailbox> {
    Ok(Mailbox {
        id: row.get(0)?,
        name: row.get(1)?,
        role: row.get(2)?,
        parent_id: row.get(3)?,
        sort_order: row.get(4)?,
        total: row.get(5)?,
        unread: row.get(6)?,
    })
}

/// Drops everything between `<` and `>`.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;

    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }

    out
}

/// Text indexed for full-text search: plain text, else stripped HTML.
fn searchable_body(text: Option<&str>, html: Option<&str>) -> String {
    match (text, html) {
        (Some(t), _) => t.to_string(),
        (None, Some(h)) => strip_tags(h),
        (None, None) => String::new(),
    }
}

/// Turns user input into an FTS5 query: every word quoted, prefix-matched.
fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `ORDER BY` for mailboxes: role rank, then sort order, then name.
const MAILBOX_ORDER: &str = "CASE WHEN role IS NULL THEN 7 ELSE \
     CASE role WHEN 'inbox' THEN 0 WHEN 'drafts' THEN 1 WHEN 'sent' THEN 2 \
     WHEN 'archive' THEN 3 WHEN 'junk' THEN 4 WHEN 'trash' THEN 5 ELSE 6 END \
     END, sort_order, name";

/// Replaces the `email_mailboxes` rows of one email.
fn write_email_mailboxes(
    conn: &Connection,
    email_id: &str,
    mailbox_ids: &[String],
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM email_mailboxes WHERE email_id = ?1",
        [email_id],
    )?;

    for mb in mailbox_ids {
        conn.execute(
            "INSERT OR IGNORE INTO email_mailboxes(email_id, mailbox_id) \
             VALUES (?1, ?2)",
            [email_id, mb],
        )?;
    }

    Ok(())
}

/// Joins names and addresses for the FTS `from_text` column.
fn from_text(from: &[Address]) -> String {
    from.iter()
        .flat_map(|a| a.name.iter().map(String::as_str).chain([a.email.as_str()]))
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Store ───────────────────────────────────────────────────────────────────

impl Store {
    /// Opens (creating if needed) the database at `path` and applies the
    /// schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or the schema fails.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch(PRAGMAS)?;
        conn.execute_batch(SCHEMA)?;

        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Opens a private in-memory database (tests and UI test mode).
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to initialise.
    pub fn open_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;

        conn.execute_batch(PRAGMAS)?;
        conn.execute_batch(SCHEMA)?;

        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Locks the connection; a poisoned lock is still usable.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads a sync-state value.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Writes a sync-state value.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO sync_state(key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;

        Ok(())
    }

    /// Inserts or updates mailboxes.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn upsert_mailboxes(&self, m: &[Mailbox]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for mb in m {
            tx.execute(
                "INSERT INTO mailboxes(id, name, role, parent_id, sort_order, \
                 total, unread) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, \
                 role = excluded.role, parent_id = excluded.parent_id, \
                 sort_order = excluded.sort_order, total = excluded.total, \
                 unread = excluded.unread",
                params![
                    mb.id,
                    mb.name,
                    mb.role,
                    mb.parent_id,
                    mb.sort_order,
                    mb.total,
                    mb.unread
                ],
            )?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Deletes mailboxes by id.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn delete_mailboxes(&self, ids: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for id in ids {
            tx.execute("DELETE FROM mailboxes WHERE id = ?1", [id])?;
        }

        tx.commit()?;

        Ok(())
    }

    /// All mailboxes: role rank (inbox, drafts, sent, archive, junk, trash,
    /// other role, no role), then sort order, then name.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn mailboxes(&self) -> Result<Vec<Mailbox>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, name, role, parent_id, sort_order, total, unread \
             FROM mailboxes ORDER BY {MAILBOX_ORDER}"
        ))?;
        let rows = stmt.query_map([], mailbox_from_row)?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Finds the mailbox with the given role.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn mailbox_by_role(&self, role: &str) -> Result<Option<Mailbox>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, name, role, parent_id, sort_order, total, unread \
                 FROM mailboxes WHERE role = ?1 LIMIT 1",
                [role],
                mailbox_from_row,
            )
            .optional()?)
    }

    /// Finds a mailbox by name, ignoring case.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn mailbox_by_name(&self, name: &str) -> Result<Option<Mailbox>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, name, role, parent_id, sort_order, total, unread \
                 FROM mailboxes WHERE name = ?1 COLLATE NOCASE LIMIT 1",
                [name],
                mailbox_from_row,
            )
            .optional()?)
    }

    /// Inserts or updates emails, their mailbox links and their FTS rows in
    /// one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure; nothing is written then.
    pub fn upsert_emails(&self, e: &[EmailSummary]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for m in e {
            tx.execute(
                "INSERT INTO emails(id, thread_id, received_at, from_json, \
                 to_json, cc_json, subject, preview, size, has_attachment, \
                 mailbox_ids_json, keywords_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                 ON CONFLICT(id) DO UPDATE SET thread_id = excluded.thread_id, \
                 received_at = excluded.received_at, \
                 from_json = excluded.from_json, to_json = excluded.to_json, \
                 cc_json = excluded.cc_json, subject = excluded.subject, \
                 preview = excluded.preview, size = excluded.size, \
                 has_attachment = excluded.has_attachment, \
                 mailbox_ids_json = excluded.mailbox_ids_json, \
                 keywords_json = excluded.keywords_json",
                params![
                    m.id,
                    m.thread_id,
                    m.received_at,
                    to_json(&m.from)?,
                    to_json(&m.to)?,
                    to_json(&m.cc)?,
                    m.subject,
                    m.preview,
                    m.size,
                    m.has_attachment,
                    to_json(&m.mailbox_ids)?,
                    to_json(&m.keywords)?,
                ],
            )?;
            write_email_mailboxes(&tx, &m.id, &m.mailbox_ids)?;

            // Keep indexed body text if a body is already cached.
            let body_text = tx
                .query_row(
                    "SELECT text, html FROM bodies WHERE email_id = ?1",
                    [&m.id],
                    |r| {
                        Ok(searchable_body(
                            r.get::<_, Option<String>>(0)?.as_deref(),
                            r.get::<_, Option<String>>(1)?.as_deref(),
                        ))
                    },
                )
                .optional()?
                .unwrap_or_default();

            // ponytail: delete by UNINDEXED email_id scans the FTS table;
            // use rowid-keyed rows if mailboxes reach 100k+ emails.
            tx.execute("DELETE FROM emails_fts WHERE email_id = ?1", [&m.id])?;
            tx.execute(
                "INSERT INTO emails_fts(email_id, subject, from_text, preview, \
                 body_text) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![m.id, m.subject, from_text(&m.from), m.preview, body_text],
            )?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Deletes emails and everything hanging off them.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn delete_emails(&self, ids: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for id in ids {
            tx.execute("DELETE FROM emails WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM email_mailboxes WHERE email_id = ?1", [id])?;
            tx.execute("DELETE FROM bodies WHERE email_id = ?1", [id])?;
            tx.execute("DELETE FROM emails_fts WHERE email_id = ?1", [id])?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Deletes every email, mailbox link, body and FTS row.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn clear_emails(&self) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for table in ["emails", "email_mailboxes", "bodies", "emails_fts"] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Lists emails newest first.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn list_emails(&self, q: &ListQuery) -> Result<Vec<EmailSummary>> {
        let limit = if q.limit == 0 { DEFAULT_LIMIT } else { q.limit };
        let unread = if q.unread_only {
            format!("AND {UNREAD_SQL}")
        } else {
            String::new()
        };
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EMAIL_COLS} FROM emails e \
             WHERE (?1 IS NULL OR e.id IN \
                 (SELECT email_id FROM email_mailboxes WHERE mailbox_id = ?1)) \
             {unread} \
             ORDER BY e.received_at DESC, e.id LIMIT ?2 OFFSET ?3"
        ))?;
        let rows = stmt.query_map(params![q.mailbox_id, limit, q.offset], email_from_row)?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Fetches one email.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn email(&self, id: &str) -> Result<Option<EmailSummary>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {EMAIL_COLS} FROM emails e WHERE e.id = ?1"),
                [id],
                email_from_row,
            )
            .optional()?)
    }

    /// Replaces an email's keywords. Unknown ids are a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_keywords(&self, id: &str, keywords: &[String]) -> Result<()> {
        self.conn().execute(
            "UPDATE emails SET keywords_json = ?2 WHERE id = ?1",
            params![id, to_json(keywords)?],
        )?;

        Ok(())
    }

    /// Replaces an email's mailboxes. Unknown ids are a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_mailbox_ids(&self, id: &str, mailbox_ids: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE emails SET mailbox_ids_json = ?2 WHERE id = ?1",
            params![id, to_json(mailbox_ids)?],
        )?;

        if changed > 0 {
            write_email_mailboxes(&tx, id, mailbox_ids)?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Caches a body and refreshes the email's FTS body text.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn put_body(&self, b: &EmailBody) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT OR REPLACE INTO bodies(email_id, html, text, \
             attachments_json, in_reply_to_json, references_json, \
             message_id_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                b.email_id,
                b.html,
                b.text,
                to_json(&b.attachments)?,
                to_json(&b.in_reply_to)?,
                to_json(&b.references)?,
                to_json(&b.message_id)?,
            ],
        )?;
        tx.execute(
            "UPDATE emails_fts SET body_text = ?2 WHERE email_id = ?1",
            params![
                b.email_id,
                searchable_body(b.text.as_deref(), b.html.as_deref())
            ],
        )?;
        tx.commit()?;

        Ok(())
    }

    /// Fetches a cached body.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn body(&self, email_id: &str) -> Result<Option<EmailBody>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT email_id, html, text, attachments_json, \
                 in_reply_to_json, references_json, message_id_json \
                 FROM bodies WHERE email_id = ?1",
                [email_id],
                |r| {
                    Ok(EmailBody {
                        email_id: r.get(0)?,
                        html: r.get(1)?,
                        text: r.get(2)?,
                        attachments: json_col(r, 3)?,
                        in_reply_to: json_col(r, 4)?,
                        references: json_col(r, 5)?,
                        message_id: json_col(r, 6)?,
                    })
                },
            )
            .optional()?)
    }

    /// Full-text search, newest first. Unparseable queries match nothing.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure other than a bad query.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<EmailSummary>> {
        let fts = fts_query(query);

        if fts.is_empty() {
            return Ok(vec![]);
        }

        let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EMAIL_COLS} FROM emails_fts \
             JOIN emails e ON e.id = emails_fts.email_id \
             WHERE emails_fts MATCH ?1 \
             ORDER BY e.received_at DESC, e.id LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![fts, limit], email_from_row)
            .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>());

        match rows {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::SqliteFailure(..)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    /// Per-mailbox `(id, total, unread)` computed from the emails table.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn counts(&self) -> Result<Vec<(String, i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT em.mailbox_id, COUNT(*), \
             COALESCE(SUM(CASE WHEN {UNREAD_SQL} THEN 1 ELSE 0 END), 0) \
             FROM email_mailboxes em JOIN emails e ON e.id = em.email_id \
             GROUP BY em.mailbox_id ORDER BY em.mailbox_id"
        ))?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal email for tests.
    fn email(id: &str, at: &str, subject: &str, mailboxes: &[&str]) -> EmailSummary {
        EmailSummary {
            id: id.into(),
            thread_id: format!("th-{id}"),
            received_at: at.into(),
            from: vec![Address {
                name: Some("Ann Sender".into()),
                email: "ann@example.com".into(),
            }],
            to: vec![],
            cc: vec![],
            subject: subject.into(),
            preview: "preview text".into(),
            size: 100,
            has_attachment: false,
            mailbox_ids: mailboxes.iter().map(|s| s.to_string()).collect(),
            keywords: vec![],
        }
    }

    /// Builds a mailbox for tests.
    fn mailbox(id: &str, name: &str, role: Option<&str>) -> Mailbox {
        Mailbox {
            id: id.into(),
            name: name.into(),
            role: role.map(String::from),
            parent_id: None,
            sort_order: 0,
            total: 0,
            unread: 0,
        }
    }

    #[test]
    fn open_sets_wal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        let mode: String = store
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();

        assert_eq!(mode, "wal");
    }

    #[test]
    fn upsert_and_list_by_mailbox() {
        let store = Store::open_memory().unwrap();

        store
            .upsert_emails(&[
                email("a", "2026-10-01T10:00:00Z", "older", &["m1"]),
                email("b", "2026-10-02T10:00:00Z", "newer", &["m2"]),
            ])
            .unwrap();

        let filtered = store
            .list_emails(&ListQuery {
                mailbox_id: Some("m1".into()),
                ..Default::default()
            })
            .unwrap();
        let all = store.list_emails(&ListQuery::default()).unwrap();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "a");
        assert_eq!(
            all.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
    }

    #[test]
    fn upsert_is_idempotent() {
        let store = Store::open_memory().unwrap();
        let mut e = email("a", "2026-10-01T10:00:00Z", "first", &["m1"]);

        store.upsert_emails(&[e.clone()]).unwrap();
        e.subject = "second".into();
        store.upsert_emails(&[e]).unwrap();

        assert_eq!(store.email("a").unwrap().unwrap().subject, "second");
        assert_eq!(store.list_emails(&ListQuery::default()).unwrap().len(), 1);
        assert_eq!(store.search("first", 0).unwrap().len(), 0);
        assert_eq!(store.search("second", 0).unwrap().len(), 1);
    }

    #[test]
    fn keywords_and_mailbox_updates() {
        let store = Store::open_memory().unwrap();

        store
            .upsert_emails(&[email("a", "2026-10-01T10:00:00Z", "s", &["m1"])])
            .unwrap();
        assert!(store.email("a").unwrap().unwrap().is_unread());

        store.set_keywords("a", &["$seen".into()]).unwrap();
        assert!(!store.email("a").unwrap().unwrap().is_unread());

        store.set_mailbox_ids("a", &["m2".into()]).unwrap();

        let old = store
            .list_emails(&ListQuery {
                mailbox_id: Some("m1".into()),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(old.len(), 0);
        assert_eq!(store.counts().unwrap(), vec![("m2".to_string(), 1, 0)]);
    }

    #[test]
    fn search_finds_subject_and_body() {
        let store = Store::open_memory().unwrap();

        store
            .upsert_emails(&[email(
                "a",
                "2026-10-01T10:00:00Z",
                "Quarterly budget",
                &["m1"],
            )])
            .unwrap();

        assert_eq!(store.search("budget", 0).unwrap().len(), 1);
        assert_eq!(store.search("zebrafish", 0).unwrap().len(), 0);

        store
            .put_body(&EmailBody {
                email_id: "a".into(),
                html: None,
                text: Some("the zebrafish report".into()),
                attachments: vec![],
                in_reply_to: vec![],
                references: vec![],
                message_id: vec![],
            })
            .unwrap();

        assert_eq!(store.search("zebrafish", 0).unwrap().len(), 1);
        assert!(store.search("\"(((", 0).unwrap().is_empty());
    }

    #[test]
    fn mailboxes_ordered_by_role() {
        let store = Store::open_memory().unwrap();

        store
            .upsert_mailboxes(&[
                mailbox("t", "Trash", Some("trash")),
                mailbox("i", "Inbox", Some("inbox")),
                mailbox("z", "Zeta", None),
                mailbox("a", "Archive", Some("archive")),
                mailbox("al", "Alpha", None),
            ])
            .unwrap();

        let names: Vec<String> = store
            .mailboxes()
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();

        assert_eq!(names, ["Inbox", "Archive", "Trash", "Alpha", "Zeta"]);
    }

    #[test]
    fn delete_emails_removes_fts_and_joins() {
        let store = Store::open_memory().unwrap();
        let body = |id: &str| EmailBody {
            email_id: id.into(),
            html: None,
            text: Some("body".into()),
            attachments: vec![],
            in_reply_to: vec![],
            references: vec![],
            message_id: vec![],
        };

        store
            .upsert_emails(&[
                email("a", "2026-10-01T10:00:00Z", "alphasubject", &["m1"]),
                email("b", "2026-10-02T10:00:00Z", "betasubject", &["m2"]),
            ])
            .unwrap();
        store.put_body(&body("a")).unwrap();
        store.put_body(&body("b")).unwrap();
        store.delete_emails(&["a".into()]).unwrap();

        let orphan_rows: i64 = store
            .conn()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM email_mailboxes WHERE email_id = 'a') \
                 + (SELECT COUNT(*) FROM emails_fts WHERE email_id = 'a')",
                [],
                |r| r.get(0),
            )
            .unwrap();

        assert!(store.search("alphasubject", 0).unwrap().is_empty());
        assert!(store.counts().unwrap().iter().all(|(id, ..)| id != "m1"));
        assert_eq!(orphan_rows, 0);
        assert!(store.body("a").unwrap().is_none());
        assert!(store.body("b").unwrap().is_some());

        store.clear_emails().unwrap();

        assert!(store.list_emails(&ListQuery::default()).unwrap().is_empty());
        assert!(store.search("betasubject", 0).unwrap().is_empty());
        assert!(store.counts().unwrap().is_empty());
        assert!(store.body("b").unwrap().is_none());
    }
}
