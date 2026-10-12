//! Configuration and secrets stored in the Posto data directory.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Fastmail's JMAP session endpoint.
const DEFAULT_SESSION_URL: &str = "https://api.fastmail.com/jmap/session";

/// Fixed message for an unparsable `secrets.toml`; carries no file text.
const SECRETS_PARSE_ERROR: &str = "secrets.toml is not valid TOML";

/// Returns `$POSTO_DATA_DIR`, else `dirs::data_dir()/posto`.
///
/// The environment is read on every call so tests can change it.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("POSTO_DATA_DIR") {
        return PathBuf::from(dir);
    }

    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("posto")
}

/// Whole application configuration, stored as `config.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Account identity and endpoint.
    pub account: Account,
    /// Sync tuning.
    pub sync: SyncConfig,
    /// Action name to key chord; empty means defaults.
    pub shortcuts: BTreeMap<String, String>,
    /// Blocked addresses or domains, lower-case.
    pub blocked_senders: Vec<String>,
    /// Layout the GUI restores at startup.
    pub ui: UiConfig,
    /// Remote-content policy: `"ask"` (default) or `"always"`.
    pub remote_content: String,
    /// Senders whose remote content always loads: lower-case addresses or domains.
    pub trusted_senders: Vec<String>,
}

/// Column widths of the GUI, in pixels.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// Width of the mailbox column.
    pub sidebar_width: u32,
    /// Width of the message-list column.
    pub list_width: u32,
    /// Window decorations: "auto" (off under tiling compositors, on elsewhere), "on", or "off".
    pub decorations: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            sidebar_width: 220,
            list_width: 360,
            decorations: "auto".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            account: Account::default(),
            sync: SyncConfig::default(),
            shortcuts: BTreeMap::new(),
            blocked_senders: Vec::new(),
            ui: UiConfig::default(),
            remote_content: "ask".to_string(),
            trusted_senders: Vec::new(),
        }
    }
}

/// The one mail account.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Account {
    /// Sender identity; empty means unset.
    pub email: String,
    /// JMAP session URL.
    pub session_url: String,
}

impl Default for Account {
    fn default() -> Self {
        Account {
            email: String::new(),
            session_url: DEFAULT_SESSION_URL.to_string(),
        }
    }
}

/// Sync tuning knobs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SyncConfig {
    /// Minutes between background syncs; 0 turns it off.
    pub refresh_minutes: u32,
    /// Number of newest emails fetched on the first sync.
    pub initial_limit: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            refresh_minutes: 5,
            initial_limit: 1000,
        }
    }
}

impl Config {
    /// Reads `config.toml`, creating the data directory first; a missing
    /// file yields the defaults.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on read or directory-creation failure and [`Error::Config`] when the
    /// file is not valid TOML.
    pub fn load() -> Result<Config> {
        let path = data_dir().join("config.toml");

        std::fs::create_dir_all(data_dir())?;

        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes `config.toml`, creating the data directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on write failure and [`Error::Config`] if the
    /// config cannot be serialised.
    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;

        std::fs::create_dir_all(data_dir())?;
        std::fs::write(data_dir().join("config.toml"), text)?;

        Ok(())
    }

    /// Path of the SQLite database.
    pub fn db_path() -> PathBuf {
        data_dir().join("posto.db")
    }
}

/// On-disk shape of `secrets.toml`.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SecretsFile {
    /// The bearer token.
    token: Option<String>,
}

/// Access to the API token.
pub struct Secrets;

impl Secrets {
    /// Returns `$POSTO_TOKEN` if set, else the token in `secrets.toml`.
    ///
    /// # Errors
    ///
    /// Returns an error when `secrets.toml` exists but cannot be read or
    /// parsed.
    pub fn token() -> Result<Option<String>> {
        if let Ok(token) = std::env::var("POSTO_TOKEN")
            && !token.is_empty()
        {
            return Ok(Some(token));
        }

        let path = data_dir().join("secrets.toml");

        match std::fs::read_to_string(&path) {
            Ok(text) => {
                // The parser's message quotes the offending source line,
                // which holds the token, so it is dropped on purpose.
                let file: SecretsFile =
                    toml::from_str(&text).map_err(|_| Error::Config(SECRETS_PARSE_ERROR.into()))?;

                Ok(file.token.filter(|t| !t.is_empty()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes `secrets.toml` (mode 0600 on unix), creating the data dir.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written.
    pub fn set_token(token: &str) -> Result<()> {
        let text = toml::to_string(&SecretsFile {
            token: Some(token.to_string()),
        })
        .map_err(|e| Error::Config(e.to_string()))?;
        let path = data_dir().join("secrets.toml");

        std::fs::create_dir_all(data_dir())?;
        std::fs::write(&path, text)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }

    /// Removes `secrets.toml`; a missing file is fine.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be removed.
    pub fn clear_token() -> Result<()> {
        match std::fs::remove_file(data_dir().join("secrets.toml")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Serialises the tests that mutate process environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Points the data dir at a fresh tempdir and clears the token env.
    fn isolate() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        // SAFETY: ENV_LOCK is held by every caller; no other test thread
        // touches the environment.
        unsafe {
            std::env::set_var("POSTO_DATA_DIR", dir.path());
            std::env::remove_var("POSTO_TOKEN");
        }

        dir
    }

    #[test]
    fn defaults_when_missing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = isolate();
        let nested = dir.path().join("not").join("yet");

        // SAFETY: ENV_LOCK is held.
        unsafe { std::env::set_var("POSTO_DATA_DIR", &nested) };

        let cfg = Config::load().unwrap();

        assert!(nested.is_dir());
        assert_eq!(cfg.account.session_url, DEFAULT_SESSION_URL);
        assert_eq!(cfg.sync.refresh_minutes, 5);
        assert_eq!(cfg.sync.initial_limit, 1000);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = isolate();
        let mut cfg = Config::default();

        cfg.account.email = "me@example.com".into();
        cfg.shortcuts.insert("archive".into(), "ctrl+e".into());
        cfg.blocked_senders.push("spam.example".into());
        cfg.ui.list_width = 512;
        cfg.remote_content = "always".into();
        cfg.trusted_senders.push("news.example".into());
        cfg.save().unwrap();

        let loaded = Config::load().unwrap();

        assert_eq!(loaded, cfg);
        assert_eq!(loaded.ui.list_width, 512);
        assert_eq!(loaded.remote_content, "always");
        assert_eq!(loaded.trusted_senders, ["news.example"]);
    }

    #[test]
    fn token_env_overrides_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = isolate();

        Secrets::set_token("file").unwrap();

        // SAFETY: ENV_LOCK is held.
        unsafe { std::env::set_var("POSTO_TOKEN", "env") };
        assert_eq!(Secrets::token().unwrap().as_deref(), Some("env"));

        // SAFETY: ENV_LOCK is held.
        unsafe { std::env::remove_var("POSTO_TOKEN") };
        assert_eq!(Secrets::token().unwrap().as_deref(), Some("file"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(dir.path().join("secrets.toml"))
                .unwrap()
                .permissions()
                .mode();

            assert_eq!(mode & 0o777, 0o600);
        }

        // A malformed secrets file must not echo the token back.
        std::fs::write(
            dir.path().join("secrets.toml"),
            "token = \"review_dummy_secret\n",
        )
        .unwrap();

        let err = Secrets::token().unwrap_err();

        assert!(matches!(err, Error::Config(_)), "got {err:?}");
        assert!(
            !err.to_string().contains("review_dummy_secret"),
            "error leaks token: {err}"
        );

        Secrets::clear_token().unwrap();
        assert_eq!(Secrets::token().unwrap(), None);
    }
}
