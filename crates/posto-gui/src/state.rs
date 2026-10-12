//! Everything the UI needs from `posto_core`, built once at startup.

use std::sync::Arc;

use dioxus::prelude::*;
use posto_core::config::{Config, Secrets};
use posto_core::jmap::Client;
use posto_core::store::{EmailSummary, Store};
use posto_core::{Error, Result, fixtures};

/// Store, server connection and settings, shared through a `Signal<Backend>` context.
#[derive(Clone)]
pub struct Backend {
    pub store: Arc<Store>,
    /// Connected lazily on first sync; `None` offline, in UI test mode, or without a token.
    pub client: Option<Arc<Client>>,
    pub cfg: Config,
    /// `POSTO_UI_TEST=1`: in-memory fixtures, no network, nothing written to disk.
    pub ui_test: bool,
}

impl Backend {
    /// Reads `POSTO_UI_TEST` and opens the matching store.
    pub fn from_env() -> Result<Backend> {
        let ui_test = std::env::var("POSTO_UI_TEST").is_ok_and(|v| v == "1");

        if ui_test {
            let store = Store::open_memory()?;

            fixtures::seed(&store)?;

            let mut cfg = Config::default();

            cfg.account.email = "me@example.com".into();

            return Ok(Backend {
                store: Arc::new(store),
                client: None,
                cfg,
                ui_test,
            });
        }

        Ok(Backend {
            store: Arc::new(Store::open(&Config::db_path())?),
            client: None,
            // A broken config.toml must not lock the user out of Settings.
            cfg: Config::load().unwrap_or_default(),
            ui_test,
        })
    }

    /// True when the sender (or its domain) is in `cfg.blocked_senders`.
    pub fn is_blocked(&self, email: &EmailSummary) -> bool {
        email
            .from
            .iter()
            .any(|a| sender_blocked(&self.cfg.blocked_senders, &a.email))
    }
}

/// True when `address` or its domain is listed (case-insensitive; `@domain` also works).
pub fn sender_blocked(blocked: &[String], address: &str) -> bool {
    let address = address.trim().to_lowercase();
    let domain = address.rsplit('@').next().unwrap_or_default();

    blocked.iter().any(|entry| {
        let entry = entry.trim().to_lowercase();

        !entry.is_empty() && (entry == address || entry.trim_start_matches('@') == domain)
    })
}

/// The connected client, connecting with the stored token when needed.
///
/// `Ok(None)` in UI test mode. `Error::NotConfigured` when there is no token.
pub async fn client(mut backend: Signal<Backend>) -> Result<Option<Arc<Client>>> {
    let (existing, ui_test, session_url) = {
        let b = backend.read();

        (
            b.client.clone(),
            b.ui_test,
            b.cfg.account.session_url.clone(),
        )
    };

    if ui_test || existing.is_some() {
        return Ok(existing);
    }

    let token = Secrets::token()?.ok_or_else(|| Error::NotConfigured("token".into()))?;
    let client = Arc::new(Client::connect(&session_url, &token).await?);

    backend.write().client = Some(client.clone());

    Ok(Some(client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_by_address_and_domain() {
        let list = vec!["Spam@Example.com".to_string(), "@junk.io".to_string()];

        assert!(sender_blocked(&list, "spam@example.com"));
        assert!(sender_blocked(&list, "anyone@junk.io"));
        assert!(!sender_blocked(&list, "other@example.com"));
        assert!(!sender_blocked(&[String::new()], "a@b.c"));
    }
}
