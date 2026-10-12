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
    /// Bumped whenever the endpoint or token may have changed, so a connection
    /// that was already in flight is not cached under the new settings.
    pub conn_rev: u64,
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
                conn_rev: 0,
            });
        }

        Ok(Backend {
            store: Arc::new(Store::open(&Config::db_path())?),
            client: None,
            // A broken config.toml must not lock the user out of Settings.
            cfg: Config::load().unwrap_or_default(),
            ui_test,
            conn_rev: 0,
        })
    }

    /// Drops the connection and invalidates any connection still being made.
    pub fn reset_client(&mut self) {
        self.client = None;
        self.conn_rev += 1;
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

/// True when remote content may load for mail from `from` without asking.
///
/// That is the case under the `"always"` policy, or when the sender or its
/// domain is in `cfg.trusted_senders`.
pub fn remote_allowed(cfg: &Config, from: &str) -> bool {
    cfg.remote_content == "always"
        || (!from.trim().is_empty() && sender_blocked(&cfg.trusted_senders, from))
}

/// The connected client, connecting with the stored token when needed.
///
/// `Ok(None)` in UI test mode. `Error::NotConfigured` when there is no token.
pub async fn client(backend: Signal<Backend>) -> Result<Option<Arc<Client>>> {
    {
        let b = backend.read();

        if b.ui_test || b.client.is_some() {
            return Ok(b.client.clone());
        }
    }

    let token = Secrets::token()?.ok_or_else(|| Error::NotConfigured("token".into()))?;

    connect(backend, &token).await.map(Some)
}

/// Connects with the current session URL and caches the client.
///
/// # Errors
///
/// Besides connection errors, fails when the settings or token were saved
/// while connecting: the client would belong to the old endpoint.
async fn connect(mut backend: Signal<Backend>, token: &str) -> Result<Arc<Client>> {
    let (session_url, rev) = {
        let b = backend.read();

        (b.cfg.account.session_url.clone(), b.conn_rev)
    };
    let client = Arc::new(Client::connect(&session_url, token).await?);
    let mut b = backend.write();

    if b.conn_rev != rev {
        return Err(Error::Other(
            "settings changed while connecting; try again".into(),
        ));
    }

    b.client = Some(client.clone());

    Ok(client)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use dioxus::core::RuntimeGuard;

    use super::*;

    /// Serves one JMAP session response, after `release` is set.
    fn session_server(requested: Arc<AtomicBool>, release: Arc<AtomicBool>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/session", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];

            let _ = socket.read(&mut buf).unwrap();
            requested.store(true, Ordering::SeqCst);

            while !release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }

            let body = r#"{"apiUrl":"http://old.invalid/api","downloadUrl":"http://old.invalid/d","uploadUrl":"http://old.invalid/u","primaryAccounts":{"urn:ietf:params:jmap:mail":"a"}}"#;

            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        url
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_connection_is_not_cached_after_settings_save() {
        let (requested, release) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let mut cfg = Config::default();

        cfg.account.session_url = session_server(requested.clone(), release.clone());

        let dom = VirtualDom::new(VNode::empty);
        let _guard = RuntimeGuard::new(dom.runtime());
        let backend = Signal::new_in_scope(
            Backend {
                store: Arc::new(Store::open_memory().unwrap()),
                client: None,
                cfg,
                ui_test: false,
                conn_rev: 0,
            },
            ScopeId::ROOT,
        );

        // What Settings Save does while the connection is pending.
        let mut saver = backend;
        let save = async move {
            while !requested.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            saver.write().cfg.account.session_url = "http://new.invalid/session".into();
            saver.write().reset_client();
            release.store(true, Ordering::SeqCst);
        };
        let (connected, ()) = tokio::join!(connect(backend, "fake-token"), save);

        assert!(connected.is_err(), "stale client was returned");
        assert!(backend.peek().client.is_none(), "stale client was cached");
    }

    #[test]
    fn blocked_by_address_and_domain() {
        let list = vec!["Spam@Example.com".to_string(), "@junk.io".to_string()];

        assert!(sender_blocked(&list, "spam@example.com"));
        assert!(sender_blocked(&list, "anyone@junk.io"));
        assert!(!sender_blocked(&list, "other@example.com"));
        assert!(!sender_blocked(&[String::new()], "a@b.c"));
    }

    #[test]
    fn remote_policy() {
        let mut cfg = Config {
            trusted_senders: vec!["example.org".into(), "x@y.io".into()],
            ..Config::default()
        };

        assert!(remote_allowed(&cfg, "a@example.org"));
        assert!(remote_allowed(&cfg, "x@y.io"));
        assert!(!remote_allowed(&cfg, "b@other.org"));
        assert!(!remote_allowed(&cfg, "z@y.io"));
        assert!(!remote_allowed(&cfg, ""));

        cfg.remote_content = "always".into();

        assert!(remote_allowed(&cfg, "b@other.org"));
        assert!(remote_allowed(&cfg, ""));
    }
}
