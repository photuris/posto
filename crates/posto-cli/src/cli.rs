//! Command-line grammar. Doc comments are the `--help` text; every leaf command
//! ends with an `Output:` paragraph giving the shape of its JSON.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Posto email client CLI. JSON on stdout, errors as JSON on stderr.
#[derive(Parser)]
#[command(name = "posto", version)]
pub struct Cli {
    /// Data directory (overrides POSTO_DATA_DIR).
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
    /// Print tables and `key: value` lines instead of JSON.
    #[arg(long, global = true)]
    pub plain: bool,
    /// Print each list element as one JSON object per line.
    #[arg(long, global = true)]
    pub jsonl: bool,
    #[command(subcommand)]
    pub cmd: Noun,
}

#[derive(Subcommand)]
pub enum Noun {
    /// Account settings.
    ///
    /// Output: see each verb's --help.
    #[command(subcommand)]
    Account(AccountCmd),
    /// Synchronise with the server.
    ///
    /// Output: see each verb's --help.
    #[command(subcommand)]
    Sync(SyncCmd),
    /// Mailboxes.
    ///
    /// Output: see each verb's --help.
    #[command(subcommand)]
    Mailbox(MailboxCmd),
    /// Emails.
    ///
    /// Output: see each verb's --help.
    #[command(subcommand)]
    Email(EmailCmd),
    /// Attachments.
    ///
    /// Output: see each verb's --help.
    #[command(subcommand)]
    Attachment(AttachmentCmd),
    /// Desktop theme.
    ///
    /// Output: see each verb's --help.
    #[command(subcommand)]
    Theme(ThemeCmd),
    /// Keyboard shortcuts.
    ///
    /// Output: see each verb's --help.
    #[command(subcommand)]
    Shortcuts(ShortcutsCmd),
    /// Developer helpers.
    #[command(subcommand, hide = true)]
    Dev(DevCmd),
}

#[derive(Subcommand)]
pub enum AccountCmd {
    /// Read the API token from stdin and store it.
    ///
    /// Output: {"email": string, "session_url": string, "has_token": bool, "refresh_minutes": number}
    SetToken {
        /// Required: read the token from stdin (the CLI never prompts).
        #[arg(long, required = true)]
        token_stdin: bool,
    },
    /// Update the sender email and/or JMAP session URL.
    ///
    /// Output: {"email": string, "session_url": string, "has_token": bool, "refresh_minutes": number}
    Set {
        /// Sender email address.
        #[arg(long)]
        email: Option<String>,
        /// JMAP session URL.
        #[arg(long)]
        session_url: Option<String>,
    },
    /// Show the account settings; never the token itself.
    ///
    /// Output: {"email": string, "session_url": string, "has_token": bool, "refresh_minutes": number}
    Show,
    /// List the cached sending identities (filled by `sync run`).
    ///
    /// Output: [{"id": string, "name": string, "email": string}]
    Identities,
}

#[derive(Subcommand)]
pub enum SyncCmd {
    /// Sync mailboxes and emails from the server.
    ///
    /// Output: {"mailboxes": number, "emails_added": number, "emails_updated": number, "emails_removed": number, "initial": bool, "identities": number}
    Run,
}

#[derive(Subcommand)]
pub enum MailboxCmd {
    /// List mailboxes with counts.
    ///
    /// Output: [{"id": string, "name": string, "role": string|null, "parent_id": string|null, "sort_order": number, "total": number, "unread": number}]
    List,
}

/// One or more email ids.
#[derive(Args)]
pub struct Ids {
    /// Email ids.
    #[arg(required = true)]
    pub ids: Vec<String>,
}

#[derive(Subcommand)]
pub enum EmailCmd {
    /// List emails, newest first.
    ///
    /// Output: [{"id": string, "thread_id": string, "received_at": string, "from": [{"name": string|null, "email": string}], "to": [address], "cc": [address], "subject": string, "preview": string, "size": number, "has_attachment": bool, "mailbox_ids": [string], "keywords": [string]}]
    List {
        /// Mailbox name or id.
        #[arg(long)]
        mailbox: Option<String>,
        /// Maximum number of emails.
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Number of emails to skip.
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Only unread emails.
        #[arg(long)]
        unread: bool,
    },
    /// Show one email with its body (fetched and cached if needed).
    ///
    /// Output: {"summary": email summary (see `email list`), "body": {"text": string|null, "html": string|null, "attachments": [{"blob_id": string, "name": string, "mime": string, "size": number}]}}
    Show {
        /// Email id.
        id: String,
        /// Plain text only: html becomes null and text is filled from the html when empty.
        #[arg(long)]
        text: bool,
    },
    /// Full-text search over cached emails.
    ///
    /// Output: [email summary] (see `email list`)
    Search {
        /// FTS5 query.
        query: String,
        /// Maximum number of emails.
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Move emails to the archive mailbox.
    ///
    /// Output: {"applied": number, "action": "archive", "offline": bool}
    Archive(Ids),
    /// Move emails to the trash mailbox.
    ///
    /// Output: {"applied": number, "action": "delete", "offline": bool}
    Delete(Ids),
    /// Move emails to the junk mailbox.
    ///
    /// Output: {"applied": number, "action": "junk", "offline": bool}
    Junk(Ids),
    /// Mark emails read.
    ///
    /// Output: {"applied": number, "action": "read", "offline": bool}
    Read(Ids),
    /// Mark emails unread.
    ///
    /// Output: {"applied": number, "action": "unread", "offline": bool}
    Unread(Ids),
    /// Flag emails.
    ///
    /// Output: {"applied": number, "action": "flag", "offline": bool}
    Flag(Ids),
    /// Remove the flag from emails.
    ///
    /// Output: {"applied": number, "action": "unflag", "offline": bool}
    Unflag(Ids),
    /// Move emails to a mailbox.
    ///
    /// Output: {"applied": number, "action": "move", "offline": bool}
    Move {
        /// Target mailbox name or id.
        mailbox: String,
        /// Email ids.
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Reply to an email and send it; the original is quoted after the body file.
    ///
    /// Output: {"id": string}
    Reply {
        /// Email id to reply to.
        id: String,
        /// File holding the reply text.
        #[arg(long)]
        body_file: PathBuf,
        /// Treat the body as Markdown.
        #[arg(long)]
        markdown: bool,
        /// Reply to everyone.
        #[arg(long)]
        all: bool,
        /// Send from this identity address instead of the one the original was sent to.
        #[arg(long)]
        from: Option<String>,
    },
    /// Send a new email.
    ///
    /// Output: {"id": string}
    Send {
        /// Recipient addresses; repeatable, comma lists allowed.
        #[arg(long, required = true)]
        to: Vec<String>,
        /// Cc addresses; repeatable, comma lists allowed.
        #[arg(long)]
        cc: Vec<String>,
        /// Bcc addresses; repeatable, comma lists allowed.
        #[arg(long)]
        bcc: Vec<String>,
        /// Subject line.
        #[arg(long)]
        subject: String,
        /// File holding the message text.
        #[arg(long)]
        body_file: PathBuf,
        /// Treat the body as Markdown.
        #[arg(long)]
        markdown: bool,
        /// Send from this identity address (default: the account address).
        #[arg(long)]
        from: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum AttachmentCmd {
    /// Download an attachment without overwriting existing files.
    ///
    /// Output: {"path": string}
    Save {
        /// Email id.
        email_id: String,
        /// Attachment blob id (see `email show`).
        blob_id: String,
        /// Target directory.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum ThemeCmd {
    /// Show the detected Omarchy theme.
    ///
    /// Output: {"mode": "dark"|"light", "colors": {key: "#rrggbb"}} or null when no theme is found
    Show,
}

#[derive(Subcommand)]
pub enum ShortcutsCmd {
    /// List the effective keyboard shortcuts, sorted by action.
    ///
    /// Output: [{"action": string, "chord": string}]
    List,
}

#[derive(Subcommand)]
pub enum DevCmd {
    /// Fill the store with fixture data.
    ///
    /// Output: {"seeded": true}
    Seed,
}
