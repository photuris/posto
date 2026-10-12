//! `posto`: command-line interface to the Posto email client.

use clap::{Parser, Subcommand};
use posto_core::config::{Config, Secrets};

/// Top-level command line.
#[derive(Parser)]
#[command(about = "Posto email client CLI")]
struct Cli {
    /// Command to run.
    #[command(subcommand)]
    command: Command,
}

/// Top-level nouns.
#[derive(Subcommand)]
enum Command {
    /// Account settings.
    Account {
        /// Verb to run.
        #[command(subcommand)]
        action: AccountCommand,
    },
}

/// Verbs of `posto account`.
#[derive(Subcommand)]
enum AccountCommand {
    /// Print email, session URL and whether a token exists, as JSON.
    Show,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Account {
            action: AccountCommand::Show,
        } => {
            let cfg = Config::load()?;
            let out = serde_json::json!({
                "email": cfg.account.email,
                "session_url": cfg.account.session_url,
                "has_token": Secrets::token()?.is_some(),
            });

            println!("{out}");
        }
    }

    Ok(())
}
