//! `posto`: command-line interface to the Posto email client.

mod cli;
mod commands;
mod output;

use clap::Parser;
use cli::{
    AccountCmd, AttachmentCmd, Cli, DevCmd, EmailCmd, MailboxCmd, Noun, ShortcutsCmd, SyncCmd,
    ThemeCmd,
};
use commands as c;
use posto_core::Result;
use posto_core::actions::Action;

fn main() {
    let cli = Cli::parse();

    // SAFETY: no thread or runtime exists yet; nothing else touches the environment.
    if let Some(dir) = &cli.data_dir {
        unsafe { std::env::set_var("POSTO_DATA_DIR", dir) };
    }

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let (plain, jsonl) = (cli.plain, cli.jsonl);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    match rt.block_on(run(cli)) {
        Ok(value) => output::emit(&value, plain, jsonl),
        Err(e) => {
            eprintln!(
                "{}",
                serde_json::json!({ "error": e.to_string(), "code": e.exit_code() })
            );
            std::process::exit(e.exit_code());
        }
    }
}

async fn run(cli: Cli) -> Result<serde_json::Value> {
    match cli.cmd {
        Noun::Account(AccountCmd::SetToken { .. }) => c::account_set_token(),
        Noun::Account(AccountCmd::Set { email, session_url }) => c::account_set(email, session_url),
        Noun::Account(AccountCmd::Show) => c::account_show(),
        Noun::Sync(SyncCmd::Run) => c::sync_run().await,
        Noun::Mailbox(MailboxCmd::List) => c::mailbox_list(),
        Noun::Email(cmd) => match cmd {
            EmailCmd::List {
                mailbox,
                limit,
                offset,
                unread,
            } => c::email_list(mailbox, limit, offset, unread),
            EmailCmd::Show { id, text } => c::email_show(&id, text).await,
            EmailCmd::Search { query, limit } => c::email_search(&query, limit),
            EmailCmd::Archive(a) => c::email_act(a.ids, Action::Archive, "archive").await,
            EmailCmd::Delete(a) => c::email_act(a.ids, Action::Delete, "delete").await,
            EmailCmd::Junk(a) => c::email_act(a.ids, Action::Junk, "junk").await,
            EmailCmd::Read(a) => c::email_act(a.ids, Action::MarkRead, "read").await,
            EmailCmd::Unread(a) => c::email_act(a.ids, Action::MarkUnread, "unread").await,
            EmailCmd::Flag(a) => c::email_act(a.ids, Action::Flag, "flag").await,
            EmailCmd::Unflag(a) => c::email_act(a.ids, Action::Unflag, "unflag").await,
            EmailCmd::Move { mailbox, ids } => c::email_move(&mailbox, ids).await,
            EmailCmd::Reply {
                id,
                body_file,
                markdown,
                all,
            } => c::email_reply(&id, &body_file, markdown, all).await,
            EmailCmd::Send {
                to,
                cc,
                bcc,
                subject,
                body_file,
                markdown,
            } => c::email_send(to, cc, bcc, subject, &body_file, markdown).await,
        },
        Noun::Attachment(AttachmentCmd::Save {
            email_id,
            blob_id,
            dir,
        }) => c::attachment_save(&email_id, &blob_id, dir).await,
        Noun::Theme(ThemeCmd::Show) => c::theme_show(),
        Noun::Shortcuts(ShortcutsCmd::List) => c::shortcuts_list(),
        Noun::Dev(DevCmd::Seed) => c::dev_seed(),
    }
}
