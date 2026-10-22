//! `posto`: command-line interface to the Posto email client.

mod cli;
mod commands;
mod output;

use clap::Parser;
use cli::{
    AccountCmd, AttachmentCmd, ClassifyCmd, Cli, DevCmd, EmailCmd, MailboxCmd, Noun, ShortcutsCmd,
    SyncCmd, ThemeCmd,
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
    let account = cli.account.as_deref();

    match cli.cmd {
        Noun::Account(cmd) => match cmd {
            AccountCmd::List => c::account_list(),
            AccountCmd::Add {
                id,
                email,
                name,
                session_url,
                provider,
                username,
            } => c::account_add(id, email, name, session_url, provider, username),
            AccountCmd::Remove { id } => c::account_remove(&id),
            AccountCmd::SetToken { .. } => c::account_set_token(account),
            AccountCmd::Set {
                email,
                session_url,
                name,
                username,
                enabled,
            } => c::account_set(account, email, session_url, name, username, enabled),
            AccountCmd::Login {
                totp, two_password, ..
            } => c::account_login(account, totp, two_password).await,
            AccountCmd::GoogleLogin => c::account_google_login(account).await,
            AccountCmd::Show => c::account_show(account),
            AccountCmd::Identities => c::account_identities(account),
            AccountCmd::MaskedEmails => c::account_masked_emails(account),
        },
        Noun::Sync(SyncCmd::Run) => c::sync_run(account).await,
        Noun::Mailbox(MailboxCmd::List) => c::mailbox_list(account),
        Noun::Mailbox(MailboxCmd::Empty { mailbox, yes }) => {
            c::mailbox_empty(account, &mailbox, yes).await
        }
        Noun::Email(cmd) => match cmd {
            EmailCmd::List {
                mailbox,
                limit,
                offset,
                unread,
                triage,
            } => c::email_list(account, mailbox, limit, offset, unread, triage),
            EmailCmd::Show { id, text } => c::email_show(account, &id, text).await,
            EmailCmd::Search { query, limit } => c::email_search(account, &query, limit),
            EmailCmd::Archive(a) => {
                c::email_act(account, a.ids, |_, _| Ok(Action::Archive), "archive").await
            }
            EmailCmd::Delete(a) => {
                c::email_act(account, a.ids, |_, _| Ok(Action::Delete), "delete").await
            }
            EmailCmd::Destroy { ids, yes } => c::email_destroy(account, ids.ids, yes).await,
            EmailCmd::Junk(a) => {
                c::email_act(account, a.ids, |_, _| Ok(Action::Junk), "junk").await
            }
            EmailCmd::Read(a) => {
                c::email_act(account, a.ids, |_, _| Ok(Action::MarkRead), "read").await
            }
            EmailCmd::Unread(a) => {
                c::email_act(account, a.ids, |_, _| Ok(Action::MarkUnread), "unread").await
            }
            EmailCmd::Flag(a) => {
                c::email_act(account, a.ids, |_, _| Ok(Action::Flag), "flag").await
            }
            EmailCmd::Unflag(a) => {
                c::email_act(account, a.ids, |_, _| Ok(Action::Unflag), "unflag").await
            }
            EmailCmd::Move { mailbox, ids } => c::email_move(account, &mailbox, ids).await,
            EmailCmd::Label { mailbox, ids } => c::email_label(account, &mailbox, ids, true).await,
            EmailCmd::Unlabel { mailbox, ids } => {
                c::email_label(account, &mailbox, ids, false).await
            }
            EmailCmd::Reply {
                id,
                body_file,
                markdown,
                all,
                from,
            } => c::email_reply(account, &id, &body_file, markdown, all, from).await,
            EmailCmd::Send {
                to,
                cc,
                bcc,
                subject,
                body_file,
                markdown,
                from,
            } => c::email_send(account, to, cc, bcc, subject, &body_file, markdown, from).await,
        },
        Noun::Attachment(AttachmentCmd::Save {
            email_id,
            blob_id,
            dir,
        }) => c::attachment_save(account, &email_id, &blob_id, dir).await,
        Noun::Classify(cmd) => match cmd {
            ClassifyCmd::Run {
                limit,
                backfill,
                retry_failed,
                dry_run,
            } => c::classify_run(account, limit, backfill, retry_failed, dry_run).await,
            ClassifyCmd::Status => c::classify_status(account),
            ClassifyCmd::Show { id } => c::classify_show(account, &id),
            ClassifyCmd::Correct { id, tray } => c::classify_correct(account, &id, &tray).await,
            ClassifyCmd::Key { .. } => c::classify_key(),
        },
        Noun::Theme(ThemeCmd::Show) => c::theme_show(),
        Noun::Shortcuts(ShortcutsCmd::List) => c::shortcuts_list(),
        Noun::Dev(DevCmd::Seed) => c::dev_seed(),
    }
}
