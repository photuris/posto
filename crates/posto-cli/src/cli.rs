//! Command-line grammar. Doc comments are the `--help` text; every leaf command
//! ends with an `Output:` paragraph giving the shape of its JSON.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use posto_core::config::Provider;

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
    /// Account id to act on (default: the first enabled account; reads across
    /// accounts when omitted).
    #[arg(long, global = true)]
    pub account: Option<String>,
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
    /// Jev mail classifier: file new mail into trays, review and correct it.
    ///
    /// Output: see each verb's --help.
    #[command(subcommand)]
    Classify(ClassifyCmd),
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
    /// List the configured accounts.
    ///
    /// `has_token` means "has credentials": a token, or a Proton login.
    ///
    /// Output: [{"id": string, "name": string, "email": string, "provider": "jmap"|"proton", "username": string, "session_url": string, "enabled": bool, "has_token": bool}]
    List,
    /// Add an account (store its token with `account set-token --account <id>`, or sign in to a Proton account with `account login --account <id>`).
    ///
    /// Output: {"id": string, "name": string, "email": string, "provider": "jmap"|"proton", "username": string, "session_url": string, "enabled": bool, "has_token": bool, "refresh_minutes": number}
    Add {
        /// New account id: lower-case letters, digits and '-', at most 32.
        #[arg(long)]
        id: String,
        /// Sender email address.
        #[arg(long)]
        email: String,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
        /// JMAP session URL (default: Fastmail).
        #[arg(long)]
        session_url: Option<String>,
        /// Mail provider: jmap or proton.
        #[arg(long, default_value = "jmap")]
        provider: Provider,
        /// Proton login name (default: the email address).
        #[arg(long)]
        username: Option<String>,
    },
    /// Remove an account with its token and cached mail (never the last one).
    ///
    /// Output: {"removed": string}
    Remove {
        /// Account id.
        id: String,
    },
    /// Read the API token from stdin and store it for the account.
    ///
    /// Output: {"id": string, "name": string, "email": string, "provider": "jmap"|"proton", "username": string, "session_url": string, "enabled": bool, "has_token": bool, "refresh_minutes": number}
    SetToken {
        /// Required: read the token from stdin (the CLI never prompts).
        #[arg(long, required = true)]
        token_stdin: bool,
    },
    /// Update the account's name, sender email, Proton login name, JMAP session URL or enabled flag.
    ///
    /// Output: {"id": string, "name": string, "email": string, "provider": "jmap"|"proton", "username": string, "session_url": string, "enabled": bool, "has_token": bool, "refresh_minutes": number}
    Set {
        /// Sender email address.
        #[arg(long)]
        email: Option<String>,
        /// JMAP session URL.
        #[arg(long)]
        session_url: Option<String>,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
        /// Proton login name.
        #[arg(long)]
        username: Option<String>,
        /// Sync and show the account (true or false).
        #[arg(long)]
        enabled: Option<bool>,
    },
    /// Show the account settings; never the token itself. `has_token` means "has credentials": a token, or a Proton login.
    ///
    /// Output: {"id": string, "name": string, "email": string, "provider": "jmap"|"proton", "username": string, "session_url": string, "enabled": bool, "has_token": bool, "refresh_minutes": number}
    Show,
    /// Sign in to a Proton account: reads the password from the first line of stdin and, with --two-password, the mailbox password from the second line.
    ///
    /// Output: the account as `account show` prints it.
    Login {
        /// Current two-factor code, when the account has one.
        #[arg(long)]
        totp: Option<String>,
        /// The account has a separate mailbox password: read it from the second stdin line.
        #[arg(long)]
        two_password: bool,
        /// Required: read the password from stdin (the CLI never prompts).
        #[arg(long, required = true)]
        password_stdin: bool,
    },
    /// List the account's cached sending identities (filled by `sync run`).
    ///
    /// Output: [{"id": string, "name": string, "email": string}]
    Identities,
}

#[derive(Subcommand)]
pub enum SyncCmd {
    /// Sync mailboxes and emails: every enabled account, or only `--account`.
    ///
    /// A failing account is reported in its own `error` field and does not stop the others; the command fails only when it syncs a single account that fails or when no account has a token.
    ///
    /// Output: [{"account_id": string, "mailboxes": number, "emails_added": number, "emails_updated": number, "emails_removed": number, "initial": bool, "identities": number, "inbox_added": [string], "error": string|null}]
    Run,
}

#[derive(Subcommand)]
pub enum MailboxCmd {
    /// List mailboxes with counts, for every account or only `--account`.
    ///
    /// Output: [{"account_id": string, "id": string, "name": string, "role": string|null, "parent_id": string|null, "sort_order": number, "total": number, "unread": number}]
    List,
    /// Permanently delete every message in a mailbox (meant for Trash and
    /// Spam), in every account that has it or only `--account`.
    ///
    /// Output: {"applied": number, "offline": bool}
    Empty {
        /// Mailbox name or id.
        mailbox: String,
        /// Required: this cannot be undone.
        #[arg(long)]
        yes: bool,
    },
}

/// One or more email ids.
#[derive(Args)]
pub struct Ids {
    /// Email ids; an id found in several accounts needs `--account`.
    #[arg(required = true)]
    pub ids: Vec<String>,
}

#[derive(Subcommand)]
pub enum EmailCmd {
    /// List emails, newest first, across all accounts or only `--account`.
    ///
    /// Output: [{"account_id": string, "id": string, "thread_id": string, "received_at": string, "from": [{"name": string|null, "email": string}], "to": [address], "cc": [address], "subject": string, "preview": string, "size": number, "has_attachment": bool, "mailbox_ids": [string], "keywords": [string]}]
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
    /// Show one email with its body (fetched and cached if needed); an id found in several accounts needs `--account`.
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
    /// Permanently delete emails (`Email/set destroy`); `delete` never does this.
    ///
    /// Output: {"applied": number, "action": "destroy", "offline": bool}
    Destroy {
        #[command(flatten)]
        ids: Ids,
        /// Confirm the permanent deletion; refused without it.
        #[arg(long)]
        yes: bool,
    },
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
    /// Add a label (any mailbox) to emails, keeping their other mailboxes.
    ///
    /// Output: {"applied": number, "action": "label", "offline": bool}
    Label {
        /// Mailbox name or id.
        mailbox: String,
        /// Email ids.
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Remove a label from emails; an email never loses its last mailbox (it is not counted as applied).
    ///
    /// Output: {"applied": number, "action": "unlabel", "offline": bool}
    Unlabel {
        /// Mailbox name or id.
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
    /// Send a new email from `--account` (default: the first enabled account).
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

/// The tray names `classify correct` accepts.
const TRAYS: [&str; 5] = ["needs_reply", "updates", "promotional", "social", "spam"];

#[derive(Subcommand)]
pub enum ClassifyCmd {
    /// Classify queued mail. Without `--backfill` it first syncs, then drains the queue of new inbox arrivals; accounts count only where classification is enabled (`[classify] enabled`, or the account's own `classify`).
    ///
    /// Needs the TypeSafe key (`classify key --key-stdin` or TYPESAFE_API_KEY), except with `--dry-run`. `--dry-run` is read-only: it syncs nothing, asks Jev nothing and writes nothing; it lists the answers still to apply, the queued emails and, with `--backfill`, the emails that would be queued, each with the state block that would be sent (built from the cached body, assuming no List-Unsubscribe header).
    ///
    /// Output: {"sync": [sync report (see `sync run`)]|null, "reset": number, "enqueued": number, "classify": {"classified": number, "moved": number, "flagged": number, "unsure": number, "skipped": number, "errors": [string], "input_tokens": number}} or, with --dry-run, {"dry_run": true, "pending_apply": [{"account_id": string, "email_id": string, "tray": string}], "queued": [{"account_id": string, "email_id": string, "attempts": number, "state": string}], "backfill": [{"account_id": string, "email_id": string, "state": string}]}
    Run {
        /// Classify at most this many emails (default: `[classify] max_per_run`).
        #[arg(long)]
        limit: Option<usize>,
        /// Instead of syncing, queue the newest N unclassified inbox emails per account and classify them.
        #[arg(long)]
        backfill: Option<usize>,
        /// Give emails that failed three times a fresh start first.
        #[arg(long)]
        retry_failed: bool,
        /// Show what would happen; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Counts per account: classified, unsure, queued, failed (three attempts used), answers not yet applied, per tray, last time.
    ///
    /// Output: {"accounts": [{"id": string, "classified": number, "unsure": number, "queued": number, "failed": number, "pending_apply": number, "by_tray": {tray: number}, "last_classified_at": string|null}]}
    Status,
    /// Show the classification of one email.
    ///
    /// Output: {"account_id": string, "email_id": string, "tray": string, "probs": {tray: number}, "urgency": number, "personal": number, "unsure": bool, "model": string, "input_tokens": number, "classified_at": string, "corrected_tray": string|null, "applied": bool, "apply_error": string|null}
    Show {
        /// Email id; an id found in several accounts needs `--account`.
        id: String,
    },
    /// File an email under a tray yourself: moves it per the tray rules and records the correction. Without a token the local copy changes and the move is sent by the next `classify run`.
    ///
    /// Output: {"account_id": string, "id": string, "tray": string, "offline": bool}
    Correct {
        /// Email id; an id found in several accounts needs `--account`.
        id: String,
        /// Tray to file it under.
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(TRAYS))]
        tray: String,
    },
    /// Read the TypeSafe API key from stdin and store it (mode 0600).
    ///
    /// Output: {"has_key": true}
    Key {
        /// Required: read the key from stdin (the CLI never prompts).
        #[arg(long, required = true)]
        key_stdin: bool,
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
