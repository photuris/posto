//! SQLite-backed local cache of mailboxes, emails and bodies, for every account.
//!
//! Every row is scoped by `account_id`, the local account slug from the
//! config (not the server's account id). The database is a cache: when the
//! schema version is not [`SCHEMA_VERSION`], the cache tables are rebuilt.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::jmap::Identity;

/// Version of the table layout below; stored in `sync_state` as `schema_version`.
const SCHEMA_VERSION: u32 = 4;

/// Database schema; every statement is idempotent.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sync_state(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS mailboxes(account_id TEXT NOT NULL, id TEXT NOT NULL, name TEXT NOT NULL, role TEXT, parent_id TEXT, sort_order INTEGER NOT NULL DEFAULT 0, total INTEGER NOT NULL DEFAULT 0, unread INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(account_id, id));
CREATE TABLE IF NOT EXISTS emails(account_id TEXT NOT NULL, id TEXT NOT NULL, thread_id TEXT NOT NULL, received_at TEXT NOT NULL, from_json TEXT NOT NULL, to_json TEXT NOT NULL, cc_json TEXT NOT NULL, subject TEXT NOT NULL, preview TEXT NOT NULL, size INTEGER NOT NULL, has_attachment INTEGER NOT NULL, mailbox_ids_json TEXT NOT NULL, keywords_json TEXT NOT NULL, PRIMARY KEY(account_id, id));
CREATE INDEX IF NOT EXISTS emails_received ON emails(received_at DESC);
CREATE TABLE IF NOT EXISTS email_mailboxes(account_id TEXT NOT NULL, email_id TEXT NOT NULL, mailbox_id TEXT NOT NULL, PRIMARY KEY(account_id, email_id, mailbox_id));
CREATE INDEX IF NOT EXISTS email_mailboxes_box ON email_mailboxes(account_id, mailbox_id);
CREATE TABLE IF NOT EXISTS bodies(account_id TEXT NOT NULL, email_id TEXT NOT NULL, html TEXT, text TEXT, attachments_json TEXT NOT NULL, in_reply_to_json TEXT NOT NULL, references_json TEXT NOT NULL, message_id_json TEXT NOT NULL, PRIMARY KEY(account_id, email_id));
CREATE VIRTUAL TABLE IF NOT EXISTS emails_fts USING fts5(account_id UNINDEXED, email_id UNINDEXED, subject, from_text, preview, body_text);
CREATE TABLE IF NOT EXISTS classified(account_id TEXT NOT NULL, email_id TEXT NOT NULL, tray TEXT NOT NULL, probs_json TEXT NOT NULL, urgency INTEGER NOT NULL, personal REAL NOT NULL, unsure INTEGER NOT NULL, model TEXT NOT NULL, input_tokens INTEGER NOT NULL, classified_at TEXT NOT NULL, corrected_tray TEXT, applied INTEGER NOT NULL DEFAULT 0, apply_error TEXT, owned_trays TEXT NOT NULL DEFAULT '', PRIMARY KEY(account_id, email_id));
CREATE TABLE IF NOT EXISTS classify_queue(account_id TEXT NOT NULL, email_id TEXT NOT NULL, added_at TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, PRIMARY KEY(account_id, email_id));
";

/// Tables that hold cached data, dropped when the schema version changes.
/// `classified` belongs to the classifier and may not exist yet.
const CACHE_TABLES: [&str; 7] = [
    "mailboxes",
    "emails",
    "email_mailboxes",
    "bodies",
    "emails_fts",
    "classified",
    "classify_queue",
];

/// Pragmas applied to every connection (journal mode is set separately for
/// in-memory databases, which cannot use WAL).
const PRAGMAS: &str = "
PRAGMA busy_timeout=5000;
PRAGMA foreign_keys=ON;
PRAGMA synchronous=NORMAL;
";

/// Column list matching [`email_from_row`].
const EMAIL_COLS: &str = "e.account_id, e.id, e.thread_id, e.received_at, e.from_json, \
     e.to_json, e.cc_json, e.subject, e.preview, e.size, e.has_attachment, \
     e.mailbox_ids_json, e.keywords_json";

/// SQL predicate: true when the email e has no `$seen` keyword.
const UNREAD_SQL: &str = "NOT EXISTS (SELECT 1 FROM json_each(e.keywords_json) \
     WHERE value = '$seen')";

/// Attempts after which a queued email is left alone until reset.
pub const CLASSIFY_MAX_ATTEMPTS: u32 = 3;

/// Page size used when a query passes limit 0.
const DEFAULT_LIMIT: u32 = 200;

/// Identifies one message across accounts: email ids are only unique per account.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MsgRef {
    /// Local account slug.
    pub account_id: String,
    /// Server-assigned email id.
    pub id: String,
}

impl MsgRef {
    /// Builds a reference from its two parts.
    pub fn new(account_id: &str, id: &str) -> MsgRef {
        MsgRef {
            account_id: account_id.to_string(),
            id: id.to_string(),
        }
    }

    /// Parses `<account>/<id>`; the account id never contains a slash.
    pub fn parse(s: &str) -> Option<MsgRef> {
        s.split_once('/')
            .filter(|(account, id)| !account.is_empty() && !id.is_empty())
            .map(|(account, id)| MsgRef::new(account, id))
    }
}

impl std::fmt::Display for MsgRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.account_id, self.id)
    }
}

impl FromStr for MsgRef {
    type Err = Error;

    fn from_str(s: &str) -> Result<MsgRef> {
        MsgRef::parse(s).ok_or_else(|| Error::Other(format!("not a message reference: {s}")))
    }
}

/// A mailbox (folder or label) with its cached counts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Mailbox {
    /// Local account slug the mailbox belongs to.
    pub account_id: String,
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
    /// Local account slug the email belongs to.
    pub account_id: String,
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

    /// The account-qualified reference of this email.
    pub fn msg_ref(&self) -> MsgRef {
        MsgRef::new(&self.account_id, &self.id)
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

/// What the classifier recorded about one email.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Classified {
    /// Local account slug.
    pub account_id: String,
    /// Server-assigned email id.
    pub email_id: String,
    /// Tray key Jev chose (or the user, for a manual row).
    pub tray: String,
    /// Probability of every tray key; empty for a manual row.
    pub probs: BTreeMap<String, f64>,
    /// Urgency, 1 to 5; 0 for a manual row.
    pub urgency: u8,
    /// Probability the email was written by a person to the recipient.
    pub personal: f64,
    /// True while the top two trays were too close to call and nobody corrected it.
    pub unsure: bool,
    /// Model name from the Jev response, or `manual`.
    pub model: String,
    /// Input tokens Jev billed.
    pub input_tokens: u64,
    /// RFC 3339 UTC time of the answer.
    pub classified_at: String,
    /// Tray the user chose instead, if any.
    pub corrected_tray: Option<String>,
    /// False while the mailbox changes are not yet confirmed by the server.
    pub applied: bool,
    /// Why the last attempt to apply the moves failed.
    pub apply_error: Option<String>,
}

impl Classified {
    /// The tray in force: the correction, else Jev's choice.
    pub fn effective_tray(&self) -> &str {
        self.corrected_tray.as_deref().unwrap_or(&self.tray)
    }
}

/// One email waiting for (or failing) classification.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QueueRow {
    /// Local account slug.
    pub account_id: String,
    /// Server-assigned email id.
    pub email_id: String,
    /// RFC 3339 UTC time it was queued.
    pub added_at: String,
    /// Attempts made so far.
    pub attempts: u32,
    /// Why the last attempt failed.
    pub last_error: Option<String>,
}

/// Parameters of [`Store::list_emails`].
#[derive(Clone, Debug, Default)]
pub struct ListQuery {
    /// Restrict to one mailbox; only meaningful together with an account.
    pub mailbox_id: Option<String>,
    /// Restrict to emails in a mailbox with this role in their own account
    /// (the unified inbox is `role = "inbox"` over all accounts).
    pub role: Option<String>,
    /// Maximum rows; 0 means 200.
    pub limit: u32,
    /// Rows to skip.
    pub offset: u32,
    /// Only emails without `$seen`.
    pub unread_only: bool,
}

/// One async lock per `(account, email)` being changed.
type MessageLocks = HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>;

/// Handle to the local database; safe to share between threads.
pub struct Store {
    /// The single connection, serialised by a mutex.
    conn: Mutex<Connection>,
    /// Locks that keep the mailbox changes of one message in order.
    message_locks: Mutex<MessageLocks>,
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
        account_id: row.get(0)?,
        id: row.get(1)?,
        thread_id: row.get(2)?,
        received_at: row.get(3)?,
        from: json_col(row, 4)?,
        to: json_col(row, 5)?,
        cc: json_col(row, 6)?,
        subject: row.get(7)?,
        preview: row.get(8)?,
        size: row.get(9)?,
        has_attachment: row.get(10)?,
        mailbox_ids: json_col(row, 11)?,
        keywords: json_col(row, 12)?,
    })
}

/// Column list matching [`mailbox_from_row`].
const MAILBOX_COLS: &str = "account_id, id, name, role, parent_id, sort_order, total, unread";

/// Maps a row selected with [`MAILBOX_COLS`] to a [`Mailbox`].
fn mailbox_from_row(row: &Row) -> rusqlite::Result<Mailbox> {
    Ok(Mailbox {
        account_id: row.get(0)?,
        id: row.get(1)?,
        name: row.get(2)?,
        role: row.get(3)?,
        parent_id: row.get(4)?,
        sort_order: row.get(5)?,
        total: row.get(6)?,
        unread: row.get(7)?,
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
    account: &str,
    email_id: &str,
    mailbox_ids: &[String],
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM email_mailboxes WHERE account_id = ?1 AND email_id = ?2",
        [account, email_id],
    )?;

    for mb in mailbox_ids {
        conn.execute(
            "INSERT OR IGNORE INTO email_mailboxes(account_id, email_id, mailbox_id) \
             VALUES (?1, ?2, ?3)",
            [account, email_id, mb],
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

/// Inserts or updates one email, its mailbox links and its FTS row.
fn upsert_email(conn: &Connection, m: &EmailSummary) -> Result<()> {
    conn.execute(
        "INSERT INTO emails(account_id, id, thread_id, received_at, from_json, \
         to_json, cc_json, subject, preview, size, has_attachment, \
         mailbox_ids_json, keywords_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
         ON CONFLICT(account_id, id) DO UPDATE SET thread_id = excluded.thread_id, \
         received_at = excluded.received_at, \
         from_json = excluded.from_json, to_json = excluded.to_json, \
         cc_json = excluded.cc_json, subject = excluded.subject, \
         preview = excluded.preview, size = excluded.size, \
         has_attachment = excluded.has_attachment, \
         mailbox_ids_json = excluded.mailbox_ids_json, \
         keywords_json = excluded.keywords_json",
        params![
            m.account_id,
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
    write_email_mailboxes(conn, &m.account_id, &m.id, &m.mailbox_ids)?;

    // Keep indexed body text if a body is already cached.
    let body_text = conn
        .query_row(
            "SELECT text, html FROM bodies WHERE account_id = ?1 AND email_id = ?2",
            [&m.account_id, &m.id],
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
    conn.execute(
        "DELETE FROM emails_fts WHERE account_id = ?1 AND email_id = ?2",
        [&m.account_id, &m.id],
    )?;
    conn.execute(
        "INSERT INTO emails_fts(account_id, email_id, subject, from_text, preview, \
         body_text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            m.account_id,
            m.id,
            m.subject,
            from_text(&m.from),
            m.preview,
            body_text
        ],
    )?;

    Ok(())
}

/// Column list matching [`classified_from_row`].
const CLASSIFIED_COLS: &str = "account_id, email_id, tray, probs_json, urgency, personal, \
     unsure, model, input_tokens, classified_at, corrected_tray, applied, apply_error";

/// Maps a row selected with [`CLASSIFIED_COLS`] to a [`Classified`].
fn classified_from_row(row: &Row) -> rusqlite::Result<Classified> {
    Ok(Classified {
        account_id: row.get(0)?,
        email_id: row.get(1)?,
        tray: row.get(2)?,
        probs: json_col(row, 3)?,
        urgency: row.get(4)?,
        personal: row.get(5)?,
        unsure: row.get(6)?,
        model: row.get(7)?,
        input_tokens: row.get::<_, i64>(8)?.max(0) as u64,
        classified_at: row.get(9)?,
        corrected_tray: row.get(10)?,
        applied: row.get(11)?,
        apply_error: row.get(12)?,
    })
}

/// Column list matching [`queue_row_from_row`].
const QUEUE_COLS: &str = "account_id, email_id, added_at, attempts, last_error";

/// Maps a row selected with [`QUEUE_COLS`] to a [`QueueRow`].
fn queue_row_from_row(row: &Row) -> rusqlite::Result<QueueRow> {
    Ok(QueueRow {
        account_id: row.get(0)?,
        email_id: row.get(1)?,
        added_at: row.get(2)?,
        attempts: row.get(3)?,
        last_error: row.get(4)?,
    })
}

/// Deletes one email and everything hanging off it, including its queue and
/// classification rows.
fn delete_email(conn: &Connection, account: &str, id: &str) -> rusqlite::Result<()> {
    for table in [
        "emails",
        "bodies",
        "emails_fts",
        "classify_queue",
        "classified",
    ] {
        let key = if table == "emails" { "id" } else { "email_id" };

        conn.execute(
            &format!("DELETE FROM {table} WHERE account_id = ?1 AND {key} = ?2"),
            [account, id],
        )?;
    }

    conn.execute(
        "DELETE FROM email_mailboxes WHERE account_id = ?1 AND email_id = ?2",
        [account, id],
    )?;

    Ok(())
}

/// Queues emails for classification; ids already queued keep their attempts.
fn enqueue(conn: &Connection, account: &str, ids: &[String]) -> rusqlite::Result<()> {
    for id in ids {
        conn.execute(
            "INSERT OR IGNORE INTO classify_queue(account_id, email_id, added_at) \
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            [account, id],
        )?;
    }

    Ok(())
}

/// True when a table of this name exists.
fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE name = ?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

/// Brings a fresh or old database to [`SCHEMA_VERSION`].
///
/// An older or unversioned database is a cache: its tables are dropped and
/// every cursor and cached identity with them, then recreated empty.
fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(PRAGMAS)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_state(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;

    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM sync_state WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let number = version.as_deref().and_then(|v| v.parse::<u32>().ok());

    if number.is_some_and(|n| n > SCHEMA_VERSION) {
        return Err(Error::Config(
            "the database was written by a newer posto".into(),
        ));
    }

    if number != Some(SCHEMA_VERSION) {
        for table in CACHE_TABLES {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
        }

        conn.execute("DELETE FROM sync_state", [])?;
    }

    conn.execute_batch(SCHEMA)?;
    conn.execute(
        "INSERT INTO sync_state(key, value) VALUES ('schema_version', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [SCHEMA_VERSION.to_string()],
    )?;

    Ok(())
}

// ── Store ───────────────────────────────────────────────────────────────────

impl Store {
    /// Opens (creating if needed) the database at `path` and applies the
    /// schema, rebuilding an old cache.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or the schema fails,
    /// and [`Error::Config`] when the database is from a newer posto.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        init(&conn)?;

        Ok(Store {
            conn: Mutex::new(conn),
            message_locks: Mutex::default(),
        })
    }

    /// Opens the existing database at `path` read-only: nothing is created,
    /// migrated, rebuilt or written, so a command that must not change the
    /// cache can use it.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when there is no database yet and
    /// [`Error::Config`] when its schema version is not [`SCHEMA_VERSION`]
    /// (a normal open would rebuild or refuse it).
    pub fn open_read_only(path: &Path) -> Result<Store> {
        if !path.exists() {
            return Err(Error::NotFound(format!(
                "cache {} (sync once first)",
                path.display()
            )));
        }

        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let store = Store {
            conn: Mutex::new(conn),
            message_locks: Mutex::default(),
        };
        let version = store.state("schema_version").unwrap_or(None);

        if version.as_deref() != Some(&SCHEMA_VERSION.to_string()) {
            return Err(Error::Config(format!(
                "the cache has schema {} but posto needs {SCHEMA_VERSION}; \
                 run a command that opens it for writing to rebuild it",
                version.as_deref().unwrap_or("none")
            )));
        }

        Ok(store)
    }

    /// Opens a private in-memory database (tests and UI test mode).
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to initialise.
    pub fn open_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;

        init(&conn)?;

        Ok(Store {
            conn: Mutex::new(conn),
            message_locks: Mutex::default(),
        })
    }

    /// Locks the connection; a poisoned lock is still usable.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads a global sync-state value (only `schema_version` is global).
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

    /// Writes a global sync-state value.
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

    /// Reads a sync-state value of `account` (stored under `<account>/<key>`).
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn account_state(&self, account: &str, key: &str) -> Result<Option<String>> {
        self.state(&format!("{account}/{key}"))
    }

    /// Writes a sync-state value of `account`.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_account_state(&self, account: &str, key: &str, value: &str) -> Result<()> {
        self.set_state(&format!("{account}/{key}"), value)
    }

    /// Replaces the cached sending identities of `account`.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_identities(&self, account: &str, identities: &[Identity]) -> Result<()> {
        let json = serde_json::to_string(identities).map_err(|e| Error::Other(e.to_string()))?;

        self.set_account_state(account, "identities", &json)
    }

    /// The cached sending identities of `account`; empty before its first sync.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure or when the cache is corrupt.
    pub fn identities(&self, account: &str) -> Result<Vec<Identity>> {
        match self.account_state(account, "identities")? {
            Some(json) => serde_json::from_str(&json)
                .map_err(|_| Error::Other("corrupt identity cache".into())),
            None => Ok(Vec::new()),
        }
    }

    /// Inserts or updates mailboxes (each carries its own account).
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn upsert_mailboxes(&self, m: &[Mailbox]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for mb in m {
            tx.execute(
                "INSERT INTO mailboxes(account_id, id, name, role, parent_id, \
                 sort_order, total, unread) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(account_id, id) DO UPDATE SET name = excluded.name, \
                 role = excluded.role, parent_id = excluded.parent_id, \
                 sort_order = excluded.sort_order, total = excluded.total, \
                 unread = excluded.unread",
                params![
                    mb.account_id,
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

    /// Deletes mailboxes of `account` by id.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn delete_mailboxes(&self, account: &str, ids: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for id in ids {
            tx.execute(
                "DELETE FROM mailboxes WHERE account_id = ?1 AND id = ?2",
                [account, id],
            )?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Mailboxes of `account`: role rank (inbox, drafts, sent, archive, junk,
    /// trash, other role, no role), then sort order, then name.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn mailboxes(&self, account: &str) -> Result<Vec<Mailbox>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MAILBOX_COLS} FROM mailboxes WHERE account_id = ?1 \
             ORDER BY {MAILBOX_ORDER}"
        ))?;
        let rows = stmt.query_map([account], mailbox_from_row)?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Finds the mailbox of `account` with the given role.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn mailbox_by_role(&self, account: &str, role: &str) -> Result<Option<Mailbox>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {MAILBOX_COLS} FROM mailboxes \
                     WHERE account_id = ?1 AND role = ?2 LIMIT 1"
                ),
                [account, role],
                mailbox_from_row,
            )
            .optional()?)
    }

    /// Finds a mailbox of `account` by name, ignoring (Unicode) case.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn mailbox_by_name(&self, account: &str, name: &str) -> Result<Option<Mailbox>> {
        // SQLite's NOCASE folds ASCII only, so compare in Rust.
        let wanted = name.to_lowercase();

        Ok(self
            .mailboxes(account)?
            .into_iter()
            .find(|m| m.name.to_lowercase() == wanted))
    }

    /// Inserts or updates emails, their mailbox links and their FTS rows in
    /// one transaction (each email carries its own account).
    ///
    /// # Errors
    ///
    /// Returns an error on database failure; nothing is written then.
    pub fn upsert_emails(&self, e: &[EmailSummary]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for m in e {
            upsert_email(&tx, m)?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Applies one page of `Email/changes` atomically: upserts, deletes
    /// (including the deleted ids' queue rows), classification queue rows for
    /// `inbox_added`, and the new `email_state` cursor, all in one
    /// transaction. Queue rows that already exist keep their attempts.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure, and [`Error::Other`] when an
    /// upserted email belongs to another account; nothing is written then.
    pub fn apply_delta_page(
        &self,
        account: &str,
        upserts: &[EmailSummary],
        destroyed: &[String],
        inbox_added: &[String],
        new_state: &str,
    ) -> Result<()> {
        if let Some(other) = upserts.iter().find(|e| e.account_id != account) {
            return Err(Error::Other(format!(
                "email {} belongs to account {}, not {account}",
                other.id, other.account_id
            )));
        }

        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for m in upserts {
            upsert_email(&tx, m)?;
        }

        for id in destroyed {
            delete_email(&tx, account, id)?;
        }

        enqueue(&tx, account, inbox_added)?;
        tx.execute(
            "INSERT INTO sync_state(key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [&format!("{account}/email_state"), new_state],
        )?;
        tx.commit()?;

        Ok(())
    }

    /// Queues emails of `account` for classification; already queued ids keep
    /// their attempts.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn enqueue_for_classification(&self, account: &str, ids: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        enqueue(&tx, account, ids)?;
        tx.commit()?;

        Ok(())
    }

    /// Number of emails of `account` waiting in the classification queue.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn classify_queue_len(&self, account: &str) -> Result<usize> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM classify_queue WHERE account_id = ?1",
            [account],
            |r| r.get(0),
        )?;

        Ok(n as usize)
    }

    /// Deletes emails of `account` and everything hanging off them.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn delete_emails(&self, account: &str, ids: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for id in ids {
            delete_email(&tx, account, id)?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Deletes every email, mailbox link, body, FTS and classify-queue row of
    /// `account`; other accounts keep theirs.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn clear_emails(&self, account: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        for table in [
            "emails",
            "email_mailboxes",
            "bodies",
            "emails_fts",
            "classify_queue",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE account_id = ?1"),
                [account],
            )?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Deletes everything the store holds for `account`: emails, bodies, FTS
    /// rows, mailboxes, queue and classification rows, cursors and identities.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn delete_account(&self, account: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let tables = [
            "emails",
            "email_mailboxes",
            "bodies",
            "emails_fts",
            "mailboxes",
            "classify_queue",
            "classified",
        ];

        for table in tables {
            if table_exists(&tx, table)? {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE account_id = ?1"),
                    [account],
                )?;
            }
        }

        // Account ids hold no LIKE wildcards, but a prefix compare needs none.
        let prefix = format!("{account}/");

        tx.execute(
            "DELETE FROM sync_state WHERE substr(key, 1, length(?1)) = ?1",
            [prefix],
        )?;
        tx.commit()?;

        Ok(())
    }

    /// Finds a top-level mailbox of `account` by name, ignoring (Unicode)
    /// case; when several match, the lowest sort order wins.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn top_level_mailbox_by_name(&self, account: &str, name: &str) -> Result<Option<Mailbox>> {
        let wanted = name.to_lowercase();

        Ok(self
            .mailboxes(account)?
            .into_iter()
            .filter(|m| m.parent_id.is_none() && m.name.to_lowercase() == wanted)
            .min_by(|a, b| (a.sort_order, &a.id).cmp(&(b.sort_order, &b.id))))
    }

    /// Whether any email of `account` in `thread_id`, other than
    /// `exclude_email_id`, was sent from one of `addresses` (compared
    /// ignoring case).
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn thread_has_reply_from(
        &self,
        account: &str,
        thread_id: &str,
        exclude_email_id: &str,
        addresses: &[String],
    ) -> Result<bool> {
        let wanted: Vec<String> = addresses.iter().map(|a| a.to_lowercase()).collect();
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT from_json FROM emails \
             WHERE account_id = ?1 AND thread_id = ?2 AND id <> ?3",
        )?;
        let rows = stmt.query_map([account, thread_id, exclude_email_id], |r| {
            json_col::<Vec<Address>>(r, 0)
        })?;

        for from in rows {
            if from?
                .iter()
                .any(|a| wanted.contains(&a.email.to_lowercase()))
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Records a classification, replacing any earlier row of the same email.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn put_classification(&self, c: &Classified) -> Result<()> {
        self.write_classified("INSERT OR REPLACE", c).map(drop)
    }

    /// Records a classifier answer unless the email already has a row (a
    /// correction got there first); returns whether the answer was stored.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn insert_answer(&self, c: &Classified) -> Result<bool> {
        self.write_classified("INSERT OR IGNORE", c).map(|n| n > 0)
    }

    /// Writes one `classified` row with the given SQL insert verb; returns the
    /// number of rows written.
    fn write_classified(&self, verb: &str, c: &Classified) -> Result<usize> {
        Ok(self.conn().execute(
            &format!(
                "{verb} INTO classified({CLASSIFIED_COLS}, owned_trays) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ),
            params![
                c.account_id,
                c.email_id,
                c.tray,
                to_json(&c.probs)?,
                c.urgency,
                c.personal,
                c.unsure,
                c.model,
                i64::try_from(c.input_tokens).unwrap_or(i64::MAX),
                c.classified_at,
                c.corrected_tray,
                c.applied,
                c.apply_error,
                if c.applied { c.effective_tray() } else { "" },
            ],
        )?)
    }

    /// The trays whose mailboxes the classifier may have put an email in and
    /// not yet taken it out of: the confirmed one and any it has since tried
    /// to add.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn owned_trays(&self, account: &str, id: &str) -> Result<Vec<String>> {
        let owned: Option<String> = self
            .conn()
            .query_row(
                "SELECT owned_trays FROM classified WHERE account_id = ?1 AND email_id = ?2",
                [account, id],
                |r| r.get(0),
            )
            .optional()?;

        Ok(owned
            .unwrap_or_default()
            .split(',')
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect())
    }

    /// Records that `tray`'s mailbox is about to be added to an email, so a
    /// later correction removes it even if this change fails half way.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn own_tray(&self, account: &str, id: &str, tray: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE classified SET owned_trays = CASE \
             WHEN ',' || owned_trays || ',' LIKE '%,' || ?3 || ',%' THEN owned_trays \
             WHEN owned_trays = '' THEN ?3 ELSE owned_trays || ',' || ?3 END \
             WHERE account_id = ?1 AND email_id = ?2",
            [account, id, tray],
        )?;

        Ok(())
    }

    /// Waits for, then holds, the lock that orders the classifier's mailbox
    /// changes for one message within this process.
    pub async fn lock_message(&self, account: &str, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.message_locks.lock().unwrap_or_else(|e| e.into_inner());

            // Idle locks (nobody else holds a handle) are forgotten.
            locks.retain(|_, l| Arc::strong_count(l) > 1);
            locks
                .entry((account.to_string(), id.to_string()))
                .or_default()
                .clone()
        };

        lock.lock_owned().await
    }

    /// Whether the email is still queued and has no classification yet, i.e.
    /// whether asking Jev about it is still wanted.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn awaits_classification(&self, account: &str, id: &str) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM classify_queue q \
                 WHERE q.account_id = ?1 AND q.email_id = ?2) \
             AND NOT EXISTS (SELECT 1 FROM classified c \
                 WHERE c.account_id = ?1 AND c.email_id = ?2)",
            [account, id],
            |r| r.get(0),
        )?)
    }

    /// The classification of one email.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn classification(&self, account: &str, id: &str) -> Result<Option<Classified>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {CLASSIFIED_COLS} FROM classified \
                     WHERE account_id = ?1 AND email_id = ?2"
                ),
                [account, id],
                classified_from_row,
            )
            .optional()?)
    }

    /// Classifications, newest first; `account` `None` spans every account.
    /// `unsure_only` keeps the ones still unsure (a correction clears that).
    /// A `limit` of 0 means no limit.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn classifications(
        &self,
        account: Option<&str>,
        unsure_only: bool,
        limit: u32,
    ) -> Result<Vec<Classified>> {
        let limit = if limit == 0 { -1 } else { i64::from(limit) };
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CLASSIFIED_COLS} FROM classified \
             WHERE (?1 IS NULL OR account_id = ?1) AND (?2 = 0 OR unsure = 1) \
             ORDER BY classified_at DESC, account_id, email_id LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![account, unsure_only, limit], classified_from_row)?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The classifications of the referenced emails, in no particular order.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn classifications_for(&self, refs: &[MsgRef]) -> Result<Vec<Classified>> {
        let keys: Vec<String> = refs.iter().map(MsgRef::to_string).collect();
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CLASSIFIED_COLS} FROM classified \
             WHERE account_id || '/' || email_id IN (SELECT value FROM json_each(?1))"
        ))?;
        let rows = stmt.query_map([to_json(&keys)?], classified_from_row)?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Ids of the cached emails of `account` whose classification is unsure
    /// and uncorrected, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn unsure_email_ids(&self, account: &str) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT e.id FROM emails e \
             JOIN classified c ON c.account_id = e.account_id AND c.email_id = e.id \
             WHERE e.account_id = ?1 AND c.unsure = 1 AND c.corrected_tray IS NULL \
             ORDER BY e.received_at DESC, e.id",
        )?;
        let rows = stmt.query_map([account], |r| r.get(0))?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The emails of `account` with these ids, newest first; unknown ids are
    /// skipped.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn list_emails_by_ids(&self, account: &str, ids: &[String]) -> Result<Vec<EmailSummary>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EMAIL_COLS} FROM emails e \
             WHERE e.account_id = ?1 AND e.id IN (SELECT value FROM json_each(?2)) \
             ORDER BY e.received_at DESC, e.id"
        ))?;
        let rows = stmt.query_map(params![account, to_json(ids)?], email_from_row)?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Classifications whose mailbox changes the server has not confirmed,
    /// oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn pending_classifications(&self) -> Result<Vec<Classified>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CLASSIFIED_COLS} FROM classified WHERE applied = 0 \
             ORDER BY classified_at, account_id, email_id"
        ))?;
        let rows = stmt.query_map([], classified_from_row)?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records the outcome of applying a classification: `None` confirms it,
    /// `Some(error)` leaves it pending with the reason.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_classification_applied(
        &self,
        account: &str,
        id: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE classified SET applied = ?3, apply_error = ?4, \
             owned_trays = CASE WHEN ?3 THEN COALESCE(corrected_tray, tray) ELSE owned_trays END \
             WHERE account_id = ?1 AND email_id = ?2",
            params![account, id, error.is_none(), error],
        )?;

        Ok(())
    }

    /// Records that the server holds the mailbox changes for `tray` and
    /// confirms the row, unless the correction changed since `seen_correction`
    /// was read; then the row stays pending for the newer choice. Returns
    /// whether the row was confirmed.
    ///
    /// Ownership is merged into the row as it is now, in one immediate
    /// transaction: `tray` is added and only the `removed` trays (those this
    /// operation took the mailbox of) are dropped, so a stale confirmation,
    /// or one from another handle on the same database, never discards
    /// ownership another operation recorded.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn confirm_classification(
        &self,
        account: &str,
        id: &str,
        tray: &str,
        removed: &[String],
        seen_correction: Option<&str>,
    ) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owned: Option<String> = tx
            .query_row(
                "SELECT owned_trays FROM classified WHERE account_id = ?1 AND email_id = ?2",
                [account, id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(owned) = owned else {
            return Ok(false);
        };
        let mut merged: Vec<&str> = owned
            .split(',')
            .filter(|t| !t.is_empty() && *t != tray && !removed.iter().any(|r| r == t))
            .collect();

        merged.push(tray);

        let applied: bool = tx.query_row(
            "UPDATE classified SET owned_trays = ?3, \
             applied = CASE WHEN corrected_tray IS ?4 THEN 1 ELSE 0 END, \
             apply_error = CASE WHEN corrected_tray IS ?4 THEN NULL ELSE apply_error END \
             WHERE account_id = ?1 AND email_id = ?2 RETURNING applied",
            params![account, id, merged.join(","), seen_correction],
            |r| r.get(0),
        )?;

        tx.commit()?;

        Ok(applied)
    }

    /// Records the user's choice of `tray` for an email and leaves the
    /// classification pending (`applied = 0`), clearing `unsure`. An email
    /// without a row gets a manual one.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_correction(&self, account: &str, id: &str, tray: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO classified(account_id, email_id, tray, probs_json, urgency, \
             personal, unsure, model, input_tokens, classified_at, corrected_tray, applied) \
             VALUES (?1, ?2, ?3, '{}', 0, 0, 0, 'manual', 0, \
             strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), ?3, 0) \
             ON CONFLICT(account_id, email_id) DO UPDATE SET corrected_tray = ?3, \
             unsure = 0, applied = 0, apply_error = NULL",
            params![account, id, tray],
        )?;

        Ok(())
    }

    /// Queue rows, oldest first; `account` `None` spans every account.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn classify_queue(&self, account: Option<&str>) -> Result<Vec<QueueRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {QUEUE_COLS} FROM classify_queue \
             WHERE (?1 IS NULL OR account_id = ?1) \
             ORDER BY added_at, account_id, email_id"
        ))?;
        let rows = stmt.query_map([account], queue_row_from_row)?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The oldest `limit` queue rows that are still worth a try: fewer than
    /// [`CLASSIFY_MAX_ATTEMPTS`] attempts, an account in `accounts`, and no
    /// classification yet. Eligibility is applied before the limit.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn next_classify_batch(&self, accounts: &[String], limit: usize) -> Result<Vec<QueueRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {QUEUE_COLS} FROM classify_queue q \
             WHERE q.attempts < ?2 \
             AND q.account_id IN (SELECT value FROM json_each(?1)) \
             AND NOT EXISTS (SELECT 1 FROM classified c \
                 WHERE c.account_id = q.account_id AND c.email_id = q.email_id) \
             ORDER BY q.added_at, q.account_id, q.email_id LIMIT ?3"
        ))?;
        let rows = stmt.query_map(
            params![to_json(accounts)?, CLASSIFY_MAX_ATTEMPTS, limit as i64],
            queue_row_from_row,
        )?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Counts one more attempt on a queue row and clears its last error.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn begin_classify_attempt(&self, account: &str, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE classify_queue SET attempts = attempts + 1, last_error = NULL \
             WHERE account_id = ?1 AND email_id = ?2",
            [account, id],
        )?;

        Ok(())
    }

    /// Records why the last attempt on a queue row failed.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_queue_error(&self, account: &str, id: &str, error: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE classify_queue SET last_error = ?3 WHERE account_id = ?1 AND email_id = ?2",
            [account, id, error],
        )?;

        Ok(())
    }

    /// Takes an email out of the queue.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn remove_from_classify_queue(&self, account: &str, id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM classify_queue WHERE account_id = ?1 AND email_id = ?2",
            [account, id],
        )?;

        Ok(())
    }

    /// Drops queue rows whose email already has a confirmed classification
    /// (it came back to the inbox) and returns how many.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn prune_classified_queue(&self) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM classify_queue WHERE EXISTS (SELECT 1 FROM classified c \
             WHERE c.account_id = classify_queue.account_id \
             AND c.email_id = classify_queue.email_id AND c.applied = 1)",
            [],
        )?)
    }

    /// Gives failed queue rows (`attempts` at the limit) a fresh start;
    /// `account` `None` spans every account. Returns how many were reset.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn reset_failed_classifications(&self, account: Option<&str>) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE classify_queue SET attempts = 0 \
             WHERE attempts >= ?1 AND (?2 IS NULL OR account_id = ?2)",
            params![CLASSIFY_MAX_ATTEMPTS, account],
        )?)
    }

    /// Ids of the newest `n` emails in the inbox of `account` that have no
    /// classification and are not queued, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn unclassified_inbox_ids(&self, account: &str, n: usize) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT e.id FROM emails e \
             JOIN email_mailboxes em ON em.account_id = e.account_id AND em.email_id = e.id \
             JOIN mailboxes m ON m.account_id = em.account_id AND m.id = em.mailbox_id \
             WHERE e.account_id = ?1 AND m.role = 'inbox' \
             AND NOT EXISTS (SELECT 1 FROM classified c \
                 WHERE c.account_id = e.account_id AND c.email_id = e.id) \
             AND NOT EXISTS (SELECT 1 FROM classify_queue q \
                 WHERE q.account_id = e.account_id AND q.email_id = e.id) \
             ORDER BY e.received_at DESC, e.id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account, n as i64], |r| r.get(0))?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Lists emails newest first; `account` `None` lists every account.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn list_emails(&self, account: Option<&str>, q: &ListQuery) -> Result<Vec<EmailSummary>> {
        let limit = if q.limit == 0 { DEFAULT_LIMIT } else { q.limit };
        let unread = if q.unread_only {
            format!("AND {UNREAD_SQL}")
        } else {
            String::new()
        };
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EMAIL_COLS} FROM emails e \
             WHERE (?1 IS NULL OR e.account_id = ?1) \
             AND (?2 IS NULL OR EXISTS (SELECT 1 FROM email_mailboxes em \
                 WHERE em.account_id = e.account_id AND em.email_id = e.id \
                 AND em.mailbox_id = ?2)) \
             AND (?3 IS NULL OR EXISTS (SELECT 1 FROM email_mailboxes em \
                 JOIN mailboxes m ON m.account_id = em.account_id AND m.id = em.mailbox_id \
                 WHERE em.account_id = e.account_id AND em.email_id = e.id \
                 AND m.role = ?3)) \
             {unread} \
             ORDER BY e.received_at DESC, e.account_id, e.id LIMIT ?4 OFFSET ?5"
        ))?;
        let rows = stmt.query_map(
            params![account, q.mailbox_id, q.role, limit, q.offset],
            email_from_row,
        )?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Fetches one email of `account`.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn email(&self, account: &str, id: &str) -> Result<Option<EmailSummary>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {EMAIL_COLS} FROM emails e \
                     WHERE e.account_id = ?1 AND e.id = ?2"
                ),
                [account, id],
                email_from_row,
            )
            .optional()?)
    }

    /// Replaces an email's keywords. Unknown ids are a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_keywords(&self, account: &str, id: &str, keywords: &[String]) -> Result<()> {
        self.conn().execute(
            "UPDATE emails SET keywords_json = ?3 WHERE account_id = ?1 AND id = ?2",
            params![account, id, to_json(keywords)?],
        )?;

        Ok(())
    }

    /// Replaces an email's mailboxes. Unknown ids are a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn set_mailbox_ids(&self, account: &str, id: &str, mailbox_ids: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE emails SET mailbox_ids_json = ?3 WHERE account_id = ?1 AND id = ?2",
            params![account, id, to_json(mailbox_ids)?],
        )?;

        if changed > 0 {
            write_email_mailboxes(&tx, account, id, mailbox_ids)?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Caches a body of `account` and refreshes the email's FTS body text.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn put_body(&self, account: &str, b: &EmailBody) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT OR REPLACE INTO bodies(account_id, email_id, html, text, \
             attachments_json, in_reply_to_json, references_json, \
             message_id_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                account,
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
            "UPDATE emails_fts SET body_text = ?3 WHERE account_id = ?1 AND email_id = ?2",
            params![
                account,
                b.email_id,
                searchable_body(b.text.as_deref(), b.html.as_deref())
            ],
        )?;
        tx.commit()?;

        Ok(())
    }

    /// Fetches a cached body of `account`.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn body(&self, account: &str, email_id: &str) -> Result<Option<EmailBody>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT email_id, html, text, attachments_json, \
                 in_reply_to_json, references_json, message_id_json \
                 FROM bodies WHERE account_id = ?1 AND email_id = ?2",
                [account, email_id],
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

    /// Full-text search, newest first; `account` `None` searches every
    /// account. Unparseable queries match nothing.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure other than a bad query.
    pub fn search(
        &self,
        account: Option<&str>,
        query: &str,
        limit: u32,
    ) -> Result<Vec<EmailSummary>> {
        let fts = fts_query(query);

        if fts.is_empty() {
            return Ok(vec![]);
        }

        let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EMAIL_COLS} FROM emails_fts \
             JOIN emails e ON e.account_id = emails_fts.account_id \
                 AND e.id = emails_fts.email_id \
             WHERE emails_fts MATCH ?1 AND (?3 IS NULL OR e.account_id = ?3) \
             ORDER BY e.received_at DESC, e.account_id, e.id LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![fts, limit, account], email_from_row)
            .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>());

        match rows {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::SqliteFailure(..)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    /// Per-mailbox `(id, total, unread)` of `account`, computed from the emails table.
    ///
    /// # Errors
    ///
    /// Returns an error on database failure.
    pub fn counts(&self, account: &str) -> Result<Vec<(String, i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT em.mailbox_id, COUNT(*), \
             COALESCE(SUM(CASE WHEN {UNREAD_SQL} THEN 1 ELSE 0 END), 0) \
             FROM email_mailboxes em JOIN emails e \
                 ON e.account_id = em.account_id AND e.id = em.email_id \
             WHERE em.account_id = ?1 \
             GROUP BY em.mailbox_id ORDER BY em.mailbox_id"
        ))?;
        let rows = stmt.query_map([account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal email of account `account` for tests.
    fn email_in(
        account: &str,
        id: &str,
        at: &str,
        subject: &str,
        mailboxes: &[&str],
    ) -> EmailSummary {
        EmailSummary {
            account_id: account.into(),
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

    /// Builds a minimal email of account `a` for tests.
    fn email(id: &str, at: &str, subject: &str, mailboxes: &[&str]) -> EmailSummary {
        email_in("a", id, at, subject, mailboxes)
    }

    /// Builds a mailbox of account `a` for tests.
    fn mailbox(id: &str, name: &str, role: Option<&str>) -> Mailbox {
        Mailbox {
            account_id: "a".into(),
            id: id.into(),
            name: name.into(),
            role: role.map(String::from),
            parent_id: None,
            sort_order: 0,
            total: 0,
            unread: 0,
        }
    }

    /// A body with the given text.
    fn body(id: &str, text: &str) -> EmailBody {
        EmailBody {
            email_id: id.into(),
            html: None,
            text: Some(text.into()),
            attachments: vec![],
            in_reply_to: vec![],
            references: vec![],
            message_id: vec![],
        }
    }

    /// Row count of a table, for orphan checks.
    fn raw_rows(store: &Store, table: &str, account: &str) -> i64 {
        store
            .conn()
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE account_id = ?1"),
                [account],
                |r| r.get(0),
            )
            .unwrap()
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
    fn schema_v1_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");

        {
            let old = Connection::open(&path).unwrap();

            old.execute_batch(
                "CREATE TABLE sync_state(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE emails(id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, received_at TEXT NOT NULL, from_json TEXT NOT NULL, to_json TEXT NOT NULL, cc_json TEXT NOT NULL, subject TEXT NOT NULL, preview TEXT NOT NULL, size INTEGER NOT NULL, has_attachment INTEGER NOT NULL, mailbox_ids_json TEXT NOT NULL, keywords_json TEXT NOT NULL);
                 INSERT INTO emails VALUES ('e1', 't', '2026-10-01T10:00:00Z', '[]', '[]', '[]', 's', 'p', 1, 0, '[]', '[]');
                 INSERT INTO sync_state VALUES ('a/email_state', 's7');
                 INSERT INTO sync_state VALUES ('email_state', 'old');
                 INSERT INTO sync_state VALUES ('identities', '[]');",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();

        assert!(
            store
                .list_emails(None, &ListQuery::default())
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.state("schema_version").unwrap().as_deref(), Some("4"));
        assert_eq!(store.account_state("a", "email_state").unwrap(), None);
        assert_eq!(store.state("email_state").unwrap(), None);
        assert_eq!(store.state("identities").unwrap(), None);

        // Reopening a v2 database keeps its data.
        store
            .upsert_emails(&[email("e1", "2026-10-01T10:00:00Z", "kept", &["m1"])])
            .unwrap();
        drop(store);

        let again = Store::open(&path).unwrap();

        assert_eq!(
            again
                .list_emails(None, &ListQuery::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.db");

        {
            let conn = Connection::open(&path).unwrap();

            conn.execute_batch(
                "CREATE TABLE sync_state(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO sync_state VALUES ('schema_version', '5');",
            )
            .unwrap();
        }

        assert!(matches!(Store::open(&path), Err(Error::Config(_))));
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
            .list_emails(
                Some("a"),
                &ListQuery {
                    mailbox_id: Some("m1".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let all = store.list_emails(Some("a"), &ListQuery::default()).unwrap();

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

        assert_eq!(store.email("a", "a").unwrap().unwrap().subject, "second");
        assert_eq!(
            store
                .list_emails(None, &ListQuery::default())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.search(None, "first", 0).unwrap().len(), 0);
        assert_eq!(store.search(None, "second", 0).unwrap().len(), 1);
    }

    #[test]
    fn keywords_and_mailbox_updates() {
        let store = Store::open_memory().unwrap();

        store
            .upsert_emails(&[email("a", "2026-10-01T10:00:00Z", "s", &["m1"])])
            .unwrap();
        assert!(store.email("a", "a").unwrap().unwrap().is_unread());

        store.set_keywords("a", "a", &["$seen".into()]).unwrap();
        assert!(!store.email("a", "a").unwrap().unwrap().is_unread());

        store.set_mailbox_ids("a", "a", &["m2".into()]).unwrap();

        let old = store
            .list_emails(
                Some("a"),
                &ListQuery {
                    mailbox_id: Some("m1".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(old.len(), 0);
        assert_eq!(store.counts("a").unwrap(), vec![("m2".to_string(), 1, 0)]);
        assert_eq!(store.email("a", "a").unwrap().unwrap().mailbox_ids, ["m2"]);
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

        assert_eq!(store.search(None, "budget", 0).unwrap().len(), 1);
        assert_eq!(store.search(None, "zebrafish", 0).unwrap().len(), 0);

        store
            .put_body("a", &body("a", "the zebrafish report"))
            .unwrap();

        assert_eq!(store.search(None, "zebrafish", 0).unwrap().len(), 1);
        assert!(store.search(None, "\"(((", 0).unwrap().is_empty());
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
            .mailboxes("a")
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();

        assert_eq!(names, ["Inbox", "Archive", "Trash", "Alpha", "Zeta"]);
    }

    #[test]
    fn mailbox_by_name_ignores_unicode_case() {
        let store = Store::open_memory().unwrap();

        store
            .upsert_mailboxes(&[
                mailbox("i", "Inbox", Some("inbox")),
                mailbox("r", "Årsrapporter", None),
            ])
            .unwrap();

        let id = |name: &str| store.mailbox_by_name("a", name).unwrap().map(|m| m.id);

        assert_eq!(id("INBOX").as_deref(), Some("i"));
        assert_eq!(id("årsrapporter").as_deref(), Some("r"));
        assert_eq!(id("ÅRSRAPPORTER").as_deref(), Some("r"));
        assert_eq!(id("missing"), None);
        assert!(store.mailbox_by_name("b", "Inbox").unwrap().is_none());
        assert!(store.mailbox_by_role("b", "inbox").unwrap().is_none());
    }

    #[test]
    fn delete_emails_removes_fts_and_joins() {
        let store = Store::open_memory().unwrap();

        store
            .upsert_emails(&[
                email("a", "2026-10-01T10:00:00Z", "alphasubject", &["m1"]),
                email("b", "2026-10-02T10:00:00Z", "betasubject", &["m2"]),
            ])
            .unwrap();
        store.put_body("a", &body("a", "body")).unwrap();
        store.put_body("a", &body("b", "body")).unwrap();
        store
            .enqueue_for_classification("a", &["a".into()])
            .unwrap();
        store.delete_emails("a", &["a".into()]).unwrap();

        let orphan_rows: i64 = store
            .conn()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM email_mailboxes WHERE email_id = 'a') \
                 + (SELECT COUNT(*) FROM emails_fts WHERE email_id = 'a') \
                 + (SELECT COUNT(*) FROM classify_queue WHERE email_id = 'a')",
                [],
                |r| r.get(0),
            )
            .unwrap();

        assert!(store.search(None, "alphasubject", 0).unwrap().is_empty());
        assert!(store.counts("a").unwrap().iter().all(|(id, ..)| id != "m1"));
        assert_eq!(orphan_rows, 0);
        assert!(store.body("a", "a").unwrap().is_none());
        assert!(store.body("a", "b").unwrap().is_some());

        // Raw table counts: the joined queries below hide orphan rows.
        let tables = ["emails", "email_mailboxes", "bodies", "emails_fts"];

        for table in tables {
            assert_eq!(raw_rows(&store, table, "a"), 1, "{table} before clear");
        }

        store.clear_emails("a").unwrap();

        for table in tables {
            assert_eq!(raw_rows(&store, table, "a"), 0, "{table} after clear");
        }

        assert!(
            store
                .list_emails(None, &ListQuery::default())
                .unwrap()
                .is_empty()
        );
        assert!(store.search(None, "betasubject", 0).unwrap().is_empty());
        assert!(store.counts("a").unwrap().is_empty());
        assert!(store.body("a", "b").unwrap().is_none());
    }

    #[test]
    fn accounts_are_isolated() {
        let store = Store::open_memory().unwrap();
        let at = "2026-10-01T10:00:00Z";

        store
            .upsert_emails(&[
                email_in("a", "e1", at, "alpha apple", &["m1"]),
                email_in("b", "e1", at, "bravo apple", &["m1"]),
            ])
            .unwrap();
        store.put_body("a", &body("e1", "one")).unwrap();
        store.put_body("b", &body("e1", "two")).unwrap();

        assert_ne!(
            store.email("a", "e1").unwrap().unwrap().subject,
            store.email("b", "e1").unwrap().unwrap().subject
        );
        assert_eq!(
            store.body("a", "e1").unwrap().unwrap().text.as_deref(),
            Some("one")
        );
        assert_eq!(
            store.body("b", "e1").unwrap().unwrap().text.as_deref(),
            Some("two")
        );
        assert_eq!(
            store
                .list_emails(Some("a"), &ListQuery::default())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .list_emails(None, &ListQuery::default())
                .unwrap()
                .len(),
            2
        );
        assert_eq!(store.counts("a").unwrap(), vec![("m1".to_string(), 1, 1)]);

        store.set_keywords("a", "e1", &["$seen".into()]).unwrap();

        assert!(store.email("b", "e1").unwrap().unwrap().is_unread());

        store.delete_emails("a", &["e1".into()]).unwrap();

        assert!(store.email("a", "e1").unwrap().is_none());
        assert!(store.email("b", "e1").unwrap().is_some());
        assert!(store.body("b", "e1").unwrap().is_some());
        assert_eq!(store.counts("b").unwrap().len(), 1);

        store
            .upsert_emails(&[email_in("a", "e1", at, "alpha apple", &["m1"])])
            .unwrap();

        assert_eq!(store.search(None, "apple", 0).unwrap().len(), 2);
        assert_eq!(store.search(Some("b"), "apple", 0).unwrap().len(), 1);
        assert_eq!(
            store.search(Some("b"), "apple", 0).unwrap()[0].account_id,
            "b"
        );
        assert_eq!(store.search(Some("b"), "alpha", 0).unwrap().len(), 0);
    }

    #[test]
    fn list_by_role_spans_accounts() {
        let store = Store::open_memory().unwrap();
        let at = "2026-10-01T10:00:00Z";
        let boxed = |account: &str, id: &str, role: &str| Mailbox {
            account_id: account.into(),
            ..mailbox(id, id, Some(role))
        };

        store
            .upsert_mailboxes(&[
                boxed("a", "ai", "inbox"),
                boxed("a", "aa", "archive"),
                boxed("b", "bi", "inbox"),
            ])
            .unwrap();
        store
            .upsert_emails(&[
                email_in("a", "e1", at, "in a", &["ai"]),
                email_in("a", "e2", at, "archived a", &["aa"]),
                email_in("b", "e3", "2026-10-02T10:00:00Z", "in b", &["bi"]),
            ])
            .unwrap();

        let inboxes = store
            .list_emails(
                None,
                &ListQuery {
                    role: Some("inbox".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(
            inboxes
                .iter()
                .map(|e| e.msg_ref().to_string())
                .collect::<Vec<_>>(),
            ["b/e3", "a/e1"]
        );
    }

    #[test]
    fn delete_account_removes_everything() {
        let store = Store::open_memory().unwrap();
        let at = "2026-10-01T10:00:00Z";

        for account in ["a", "b"] {
            store
                .upsert_mailboxes(&[Mailbox {
                    account_id: account.into(),
                    ..mailbox("m1", "Inbox", Some("inbox"))
                }])
                .unwrap();
            store
                .upsert_emails(&[email_in(account, "e1", at, "subject", &["m1"])])
                .unwrap();
            store.put_body(account, &body("e1", "text")).unwrap();
            store
                .set_account_state(account, "email_state", "s1")
                .unwrap();
            store.set_identities(account, &[]).unwrap();
            store
                .enqueue_for_classification(account, &["e1".into()])
                .unwrap();
        }

        store.delete_account("a").unwrap();

        for table in [
            "emails",
            "email_mailboxes",
            "bodies",
            "emails_fts",
            "mailboxes",
            "classify_queue",
        ] {
            assert_eq!(raw_rows(&store, table, "a"), 0, "{table} of a");
            assert_eq!(raw_rows(&store, table, "b"), 1, "{table} of b");
        }

        assert_eq!(store.account_state("a", "email_state").unwrap(), None);
        assert_eq!(store.account_state("a", "identities").unwrap(), None);
        assert_eq!(
            store.account_state("b", "email_state").unwrap().as_deref(),
            Some("s1")
        );
        assert_eq!(store.state("schema_version").unwrap().as_deref(), Some("4"));
        assert_eq!(store.search(None, "subject", 0).unwrap().len(), 1);
    }

    #[test]
    fn delta_page_is_atomic() {
        let store = Store::open_memory().unwrap();
        let at = "2026-10-01T10:00:00Z";

        store
            .upsert_emails(&[
                email("old", at, "old", &["mb-inbox"]),
                email("gone", at, "gone", &["mb-inbox"]),
            ])
            .unwrap();
        store
            .enqueue_for_classification("a", &["gone".into()])
            .unwrap();
        store
            .apply_delta_page(
                "a",
                &[email("new", at, "fresh arrival", &["mb-inbox"])],
                &["gone".into()],
                &["new".into()],
                "s2",
            )
            .unwrap();

        assert!(store.email("a", "new").unwrap().is_some());
        assert!(store.email("a", "gone").unwrap().is_none());
        assert!(store.email("a", "old").unwrap().is_some());
        assert_eq!(store.search(None, "fresh", 0).unwrap().len(), 1);
        assert_eq!(
            store.account_state("a", "email_state").unwrap().as_deref(),
            Some("s2")
        );
        // `gone` left the queue, `new` joined it.
        let queued: Vec<_> = store
            .classify_queue(Some("a"))
            .unwrap()
            .into_iter()
            .map(|r| (r.account_id, r.email_id))
            .collect();

        assert_eq!(queued, [("a".to_string(), "new".to_string())]);
        assert_eq!(store.classify_queue_len("a").unwrap(), 1);
        assert_eq!(raw_rows(&store, "classify_queue", "a"), 1);

        // An email of another account is rejected before anything is written.
        let err = store
            .apply_delta_page(
                "a",
                &[email_in("b", "x", at, "wrong", &["mb-inbox"])],
                &["old".into()],
                &[],
                "s3",
            )
            .unwrap_err();

        assert!(matches!(err, Error::Other(_)), "{err:?}");
        assert!(store.email("a", "old").unwrap().is_some());
        assert_eq!(
            store.account_state("a", "email_state").unwrap().as_deref(),
            Some("s2")
        );
    }

    #[test]
    fn queue_keeps_attempts_on_reenqueue() {
        let store = Store::open_memory().unwrap();

        store
            .enqueue_for_classification("a", &["e1".into()])
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE classify_queue SET attempts = 2, last_error = 'boom' \
                 WHERE account_id = 'a' AND email_id = 'e1'",
                [],
            )
            .unwrap();
        store
            .enqueue_for_classification("a", &["e1".into(), "e2".into()])
            .unwrap();
        store
            .apply_delta_page("a", &[], &[], &["e1".into()], "s")
            .unwrap();

        let (attempts, error): (i64, Option<String>) = store
            .conn()
            .query_row(
                "SELECT attempts, last_error FROM classify_queue \
                 WHERE account_id = 'a' AND email_id = 'e1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        assert_eq!((attempts, error.as_deref()), (2, Some("boom")));
        assert_eq!(store.classify_queue_len("a").unwrap(), 2);
        assert_eq!(store.classify_queue_len("b").unwrap(), 0);
    }

    /// An email of `account` in thread `thread` sent by `from`.
    fn in_thread(account: &str, id: &str, thread: &str, from: &str) -> EmailSummary {
        let mut e = email_in(account, id, "2026-10-01T10:00:00Z", "s", &["inbox"]);

        e.thread_id = thread.into();
        e.from = vec![Address {
            name: None,
            email: from.into(),
        }];

        e
    }

    #[test]
    fn replied_in_thread_detected() {
        let store = Store::open_memory().unwrap();
        let own = vec!["me@example.com".to_string(), "shop@example.com".to_string()];

        store
            .upsert_emails(&[
                in_thread("a", "cand", "t1", "ann@example.com"),
                in_thread("a", "stranger", "t1", "bob@example.com"),
                in_thread("a", "mine", "t2", "ME@Example.com"),
                in_thread("a", "cand2", "t2", "ann@example.com"),
                in_thread("b", "theirs", "t3", "me@example.com"),
                in_thread("a", "cand3", "t3", "ann@example.com"),
            ])
            .unwrap();

        let replied =
            |thread: &str, id: &str| store.thread_has_reply_from("a", thread, id, &own).unwrap();

        // Alone, and with only strangers: no reply.
        assert!(!replied("t1", "cand"));
        // Another message from the user's address, in a different case.
        assert!(replied("t2", "cand2"));
        // The candidate itself never counts, even from an own address.
        assert!(!replied("t2", "mine"));
        // The same thread id under another account does not count.
        assert!(!replied("t3", "cand3"));
    }

    #[test]
    fn top_level_mailbox_lookup_prefers_lowest_sort_order() {
        let store = Store::open_memory().unwrap();
        let mut late = mailbox("m2", "Updates", None);
        let mut first = mailbox("m1", "UPDATES", None);
        let mut nested = mailbox("m3", "Updates", None);

        late.sort_order = 9;
        first.sort_order = 2;
        nested.sort_order = 0;
        nested.parent_id = Some("m1".into());
        store.upsert_mailboxes(&[late, first, nested]).unwrap();

        assert_eq!(
            store
                .top_level_mailbox_by_name("a", "updates")
                .unwrap()
                .unwrap()
                .id,
            "m1"
        );
        assert!(
            store
                .top_level_mailbox_by_name("b", "updates")
                .unwrap()
                .is_none()
        );
    }

    /// A classification of `id` with the given state.
    fn classified(account: &str, id: &str, unsure: bool, applied: bool) -> Classified {
        Classified {
            account_id: account.into(),
            email_id: id.into(),
            tray: "updates".into(),
            probs: BTreeMap::from([("updates".to_string(), 0.8)]),
            urgency: 2,
            personal: 0.25,
            unsure,
            model: "jev-test".into(),
            input_tokens: 77,
            classified_at: "2026-10-01T10:00:00Z".into(),
            corrected_tray: None,
            applied,
            apply_error: None,
        }
    }

    #[test]
    fn unsure_ids_skip_corrected_and_list_by_ids() {
        let store = Store::open_memory().unwrap();

        store
            .upsert_emails(&[
                email("e1", "2026-10-01T10:00:00Z", "one", &[]),
                email("e2", "2026-10-02T10:00:00Z", "two", &[]),
                email("e3", "2026-10-03T10:00:00Z", "three", &[]),
                email_in("b", "e2", "2026-10-04T10:00:00Z", "other", &[]),
            ])
            .unwrap();

        for (account, id, unsure) in [
            ("a", "e1", true),
            ("a", "e2", true),
            ("a", "e3", false),
            ("a", "gone", true),
            ("b", "e2", true),
        ] {
            store
                .put_classification(&classified(account, id, unsure, true))
                .unwrap();
        }

        store.set_correction("a", "e1", "social").unwrap();

        // Corrected, sure, uncached and other-account rows are out.
        assert_eq!(store.unsure_email_ids("a").unwrap(), ["e2"]);

        let ids = ["e3".to_string(), "e1".to_string(), "nope".to_string()];
        let listed: Vec<String> = store
            .list_emails_by_ids("a", &ids)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();

        assert_eq!(listed, ["e3", "e1"]);

        let refs = [MsgRef::new("a", "e3"), MsgRef::new("b", "e2")];
        let mut found: Vec<String> = store
            .classifications_for(&refs)
            .unwrap()
            .iter()
            .map(|c| format!("{}/{}", c.account_id, c.email_id))
            .collect();

        found.sort();

        assert_eq!(found, ["a/e3", "b/e2"]);
    }

    #[test]
    fn classification_rows_round_trip_and_correct() {
        let store = Store::open_memory().unwrap();

        store
            .put_classification(&classified("a", "e1", true, false))
            .unwrap();
        store
            .put_classification(&classified("a", "e2", false, true))
            .unwrap();

        assert_eq!(
            store.classification("a", "e1").unwrap().unwrap(),
            classified("a", "e1", true, false)
        );
        assert_eq!(store.classification("b", "e1").unwrap(), None);
        assert_eq!(store.classifications(Some("a"), true, 0).unwrap().len(), 1);
        assert_eq!(store.classifications(None, false, 1).unwrap().len(), 1);
        assert_eq!(store.pending_classifications().unwrap().len(), 1);

        store
            .set_classification_applied("a", "e1", Some("nope"))
            .unwrap();
        assert_eq!(
            store
                .classification("a", "e1")
                .unwrap()
                .unwrap()
                .apply_error
                .as_deref(),
            Some("nope")
        );
        store.set_classification_applied("a", "e1", None).unwrap();
        assert!(store.pending_classifications().unwrap().is_empty());

        // A correction clears unsure and goes pending again.
        store.set_correction("a", "e1", "social").unwrap();

        let c = store.classification("a", "e1").unwrap().unwrap();

        assert_eq!(c.corrected_tray.as_deref(), Some("social"));
        assert_eq!(c.effective_tray(), "social");
        assert!(!c.unsure && !c.applied);
        assert_eq!(c.apply_error, None);

        // An unseen email gets a manual row.
        store.set_correction("a", "e9", "spam").unwrap();

        let m = store.classification("a", "e9").unwrap().unwrap();

        assert_eq!(
            (m.model.as_str(), m.urgency, m.applied),
            ("manual", 0, false)
        );
        assert_eq!(m.corrected_tray.as_deref(), Some("spam"));

        // Destroying the email drops its classification.
        store.delete_emails("a", &["e2".to_string()]).unwrap();
        assert_eq!(store.classification("a", "e2").unwrap(), None);
    }

    #[test]
    fn queue_batch_filters_before_limit() {
        let store = Store::open_memory().unwrap();

        for (account, id) in [("a", "old"), ("b", "new1"), ("b", "new2"), ("b", "tried")] {
            store
                .enqueue_for_classification(account, &[id.to_string()])
                .unwrap();
        }

        // Spread the timestamps so the order is defined.
        for (i, id) in ["old", "new1", "new2", "tried"].iter().enumerate() {
            store
                .conn()
                .execute(
                    "UPDATE classify_queue SET added_at = ?1 WHERE email_id = ?2",
                    params![format!("2026-10-01T10:00:0{i}Z"), id],
                )
                .unwrap();
        }

        for _ in 0..CLASSIFY_MAX_ATTEMPTS {
            store.begin_classify_attempt("b", "tried").unwrap();
        }

        let ids = |limit| -> Vec<String> {
            store
                .next_classify_batch(&["b".to_string()], limit)
                .unwrap()
                .into_iter()
                .map(|r| r.email_id)
                .collect()
        };

        // The older row of the ineligible account and the exhausted row never fill the limit.
        assert_eq!(ids(1), ["new1"]);
        assert_eq!(ids(5), ["new1", "new2"]);

        // A classification takes a row out of the batch; confirmed ones are pruned.
        store
            .put_classification(&classified("b", "new1", false, true))
            .unwrap();
        assert_eq!(ids(5), ["new2"]);
        assert_eq!(store.prune_classified_queue().unwrap(), 1);
        assert_eq!(store.classify_queue_len("b").unwrap(), 2);

        // Resetting gives the exhausted row another go.
        assert_eq!(store.reset_failed_classifications(None).unwrap(), 1);
        assert_eq!(ids(5), ["new2", "tried"]);
    }

    #[test]
    fn unclassified_inbox_ids_skips_classified_and_queued() {
        let store = Store::open_memory().unwrap();

        store
            .upsert_mailboxes(&[
                mailbox("inbox", "Inbox", Some("inbox")),
                mailbox("x", "X", None),
            ])
            .unwrap();
        store
            .upsert_emails(&[
                email("e1", "2026-10-01T10:00:01Z", "s", &["inbox"]),
                email("e2", "2026-10-01T10:00:02Z", "s", &["inbox"]),
                email("e3", "2026-10-01T10:00:03Z", "s", &["inbox"]),
                email("e4", "2026-10-01T10:00:04Z", "s", &["inbox"]),
                email("e5", "2026-10-01T10:00:05Z", "s", &["x"]),
            ])
            .unwrap();
        store
            .put_classification(&classified("a", "e4", false, true))
            .unwrap();
        store
            .enqueue_for_classification("a", &["e3".to_string()])
            .unwrap();

        assert_eq!(store.unclassified_inbox_ids("a", 10).unwrap(), ["e2", "e1"]);
        assert_eq!(store.unclassified_inbox_ids("a", 1).unwrap(), ["e2"]);
    }

    #[test]
    fn msg_ref_round_trips() {
        let r = MsgRef::new("work", "e1");

        assert_eq!(r.to_string(), "work/e1");
        assert_eq!(MsgRef::parse("work/e1"), Some(r.clone()));
        assert_eq!("work/e1".parse::<MsgRef>().unwrap(), r);
        assert_eq!(MsgRef::parse("no-slash"), None);
        assert_eq!(MsgRef::parse("/e1"), None);
        assert!("x".parse::<MsgRef>().is_err());
    }
}
