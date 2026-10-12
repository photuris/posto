//! Configuration and secrets stored in the Posto data directory.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Fastmail's JMAP session endpoint.
const DEFAULT_SESSION_URL: &str = "https://api.fastmail.com/jmap/session";

/// Id of the account that legacy single-account files migrate to.
pub const DEFAULT_ACCOUNT_ID: &str = "default";

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
    /// The mail accounts, in display order. Never empty after [`Config::load`].
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// The pre-multi-account `[account]` section; read so old files load,
    /// converted by [`Config::load`] and never written back.
    #[serde(rename = "account", skip_serializing)]
    pub legacy_account: Option<LegacyAccount>,
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
            accounts: vec![Account::default()],
            legacy_account: None,
            sync: SyncConfig::default(),
            shortcuts: BTreeMap::new(),
            blocked_senders: Vec::new(),
            ui: UiConfig::default(),
            remote_content: "ask".to_string(),
            trusted_senders: Vec::new(),
        }
    }
}

/// One mail account.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Account {
    /// Slug used in the CLI, the secrets file and the store: `[a-z0-9][a-z0-9-]{0,31}`.
    pub id: String,
    /// Display name; empty falls back to the email, then the id.
    pub name: String,
    /// Sender identity; empty means unset.
    pub email: String,
    /// JMAP session URL.
    pub session_url: String,
    /// Disabled accounts are neither synced nor shown.
    pub enabled: bool,
    /// Per-account classifier opt-in; absent follows the global switch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classify: Option<bool>,
}

impl Default for Account {
    fn default() -> Self {
        Account {
            id: DEFAULT_ACCOUNT_ID.to_string(),
            name: "Default".to_string(),
            email: String::new(),
            session_url: DEFAULT_SESSION_URL.to_string(),
            enabled: true,
            classify: None,
        }
    }
}

impl Account {
    /// A new enabled account on the default session URL.
    pub fn new(id: &str, email: &str) -> Account {
        Account {
            id: id.to_string(),
            name: String::new(),
            email: email.to_string(),
            ..Account::default()
        }
    }

    /// The name to show: the display name, else the email, else the id.
    pub fn display_name(&self) -> &str {
        [&self.name, &self.email, &self.id]
            .into_iter()
            .map(String::as_str)
            .find(|s| !s.trim().is_empty())
            .unwrap_or_default()
    }
}

/// The single `[account]` section of pre-multi-account config files.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LegacyAccount {
    /// Sender identity; empty means unset.
    pub email: String,
    /// JMAP session URL.
    pub session_url: String,
}

impl Default for LegacyAccount {
    fn default() -> Self {
        LegacyAccount {
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

/// Checks an account id against `^[a-z0-9][a-z0-9-]{0,31}$`.
///
/// # Errors
///
/// Returns [`Error::Config`] when the id does not match.
pub fn validate_account_id(id: &str) -> Result<()> {
    let mut chars = id.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');

    if valid {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "invalid account id {id:?}: use 1-32 of a-z, 0-9, '-', starting with a letter or digit"
        )))
    }
}

impl Config {
    /// Reads `config.toml`, creating the data directory first.
    ///
    /// A missing file yields the defaults. A legacy `[account]` section
    /// becomes the account `default`; the file is rewritten by the next
    /// [`Config::save`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on read or directory-creation failure and [`Error::Config`] when the
    /// file is not valid TOML or an account id is invalid or duplicated.
    pub fn load() -> Result<Config> {
        let path = data_dir().join("config.toml");

        std::fs::create_dir_all(data_dir())?;

        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<Config>(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?
                .normalised(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Converts a legacy account, guarantees one account, and validates.
    fn normalised(mut self) -> Result<Config> {
        let legacy = self.legacy_account.take();

        if self.accounts.is_empty() {
            self.accounts.push(match legacy {
                Some(l) => Account {
                    name: if l.email.is_empty() {
                        Account::default().name
                    } else {
                        l.email.clone()
                    },
                    email: l.email,
                    session_url: l.session_url,
                    ..Account::default()
                },
                None => Account::default(),
            });
        }

        self.validate()?;

        Ok(self)
    }

    /// Checks that every account id is valid and unique.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] naming the offending id.
    pub fn validate(&self) -> Result<()> {
        for (i, a) in self.accounts.iter().enumerate() {
            validate_account_id(&a.id)?;

            if self.accounts[..i].iter().any(|b| b.id == a.id) {
                return Err(Error::Config(format!("duplicate account id {:?}", a.id)));
            }
        }

        Ok(())
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

    /// The account with this id.
    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// The first enabled account: the target of commands without `--account`
    /// and of `POSTO_TOKEN`.
    pub fn default_account(&self) -> Option<&Account> {
        self.accounts_enabled().next()
    }

    /// Enabled accounts, in config order.
    pub fn accounts_enabled(&self) -> impl Iterator<Item = &Account> {
        self.accounts.iter().filter(|a| a.enabled)
    }

    /// Appends an account.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the id is invalid or already used.
    pub fn add_account(&mut self, account: Account) -> Result<()> {
        validate_account_id(&account.id)?;

        if self.account(&account.id).is_some() {
            return Err(Error::Config(format!(
                "duplicate account id {:?}",
                account.id
            )));
        }

        self.accounts.push(account);

        Ok(())
    }

    /// Removes an account from the config (the caller cleans store and secrets).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] for an unknown id and [`Error::Config`]
    /// when it is the last account.
    pub fn remove_account(&mut self, id: &str) -> Result<Account> {
        let at = self
            .accounts
            .iter()
            .position(|a| a.id == id)
            .ok_or_else(|| Error::NotFound(format!("account {id}")))?;

        if self.accounts.len() == 1 {
            return Err(Error::Config("cannot remove the last account".into()));
        }

        Ok(self.accounts.remove(at))
    }
}

/// On-disk shape of `secrets.toml`.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SecretsFile {
    /// Legacy single bearer token; moves to `tokens.default` on the next write.
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    /// Bearer tokens by account id.
    tokens: BTreeMap<String, String>,
    /// TypeSafe API key.
    #[serde(skip_serializing_if = "Option::is_none")]
    typesafe_key: Option<String>,
}

/// Access to the API tokens and keys. Every setter reads the whole file,
/// changes one key and writes it back, so keys never clobber each other.
pub struct Secrets;

impl Secrets {
    /// Reads `secrets.toml`; `None` when it does not exist.
    fn read() -> Result<Option<SecretsFile>> {
        let path = data_dir().join("secrets.toml");

        match std::fs::read_to_string(&path) {
            // The parser's message quotes the offending source line,
            // which holds the secret, so it is dropped on purpose.
            Ok(text) => toml::from_str(&text)
                .map(Some)
                .map_err(|_| Error::Config(SECRETS_PARSE_ERROR.into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Migrates the legacy token, applies `change`, and writes the file (mode 0600 on unix).
    fn update(change: impl FnOnce(&mut SecretsFile)) -> Result<()> {
        let mut file = Self::read()?.unwrap_or_default();

        if let Some(legacy) = file.token.take() {
            file.tokens
                .entry(DEFAULT_ACCOUNT_ID.to_string())
                .or_insert(legacy);
        }

        change(&mut file);

        let text = toml::to_string(&file).map_err(|e| Error::Config(e.to_string()))?;
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

    /// The token stored for `account_id`, ignoring the environment. The legacy
    /// top-level `token` counts as the token of the account `default`.
    ///
    /// # Errors
    ///
    /// Returns an error when `secrets.toml` exists but cannot be read or
    /// parsed.
    pub fn token(account_id: &str) -> Result<Option<String>> {
        let Some(file) = Self::read()? else {
            return Ok(None);
        };
        let legacy = file.token.filter(|_| account_id == DEFAULT_ACCOUNT_ID);

        Ok(file
            .tokens
            .get(account_id)
            .cloned()
            .or(legacy)
            .filter(|t| !t.is_empty()))
    }

    /// The token callers use: `$POSTO_TOKEN` for the default account (the
    /// first enabled one), else [`Secrets::token`].
    ///
    /// # Errors
    ///
    /// See [`Secrets::token`].
    pub fn token_for(cfg: &Config, account_id: &str) -> Result<Option<String>> {
        if let Ok(token) = std::env::var("POSTO_TOKEN")
            && !token.is_empty()
            && cfg.default_account().is_some_and(|a| a.id == account_id)
        {
            return Ok(Some(token));
        }

        Self::token(account_id)
    }

    /// Stores the token of `account_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or written.
    pub fn set_token(account_id: &str, token: &str) -> Result<()> {
        Self::update(|f| {
            f.tokens.insert(account_id.to_string(), token.to_string());
        })
    }

    /// Forgets the token of `account_id`; a missing file is fine.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or written.
    pub fn clear_token(account_id: &str) -> Result<()> {
        if Self::read()?.is_none() {
            return Ok(());
        }

        Self::update(|f| {
            f.tokens.remove(account_id);
        })
    }

    /// `$TYPESAFE_API_KEY` if set, else the key in `secrets.toml`.
    ///
    /// # Errors
    ///
    /// Returns an error when `secrets.toml` exists but cannot be read or
    /// parsed.
    pub fn typesafe_key() -> Result<Option<String>> {
        if let Ok(key) = std::env::var("TYPESAFE_API_KEY")
            && !key.is_empty()
        {
            return Ok(Some(key));
        }

        Ok(Self::read()?
            .and_then(|f| f.typesafe_key)
            .filter(|k| !k.is_empty()))
    }

    /// Stores the TypeSafe API key.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or written.
    pub fn set_typesafe_key(key: &str) -> Result<()> {
        Self::update(|f| f.typesafe_key = Some(key.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Serialises the tests that mutate process environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Points the data dir at a fresh tempdir and clears the secret env.
    fn isolate() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        // SAFETY: ENV_LOCK is held by every caller; no other test thread
        // touches the environment.
        unsafe {
            std::env::set_var("POSTO_DATA_DIR", dir.path());
            std::env::remove_var("POSTO_TOKEN");
            std::env::remove_var("TYPESAFE_API_KEY");
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
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].session_url, DEFAULT_SESSION_URL);
        assert_eq!(cfg.sync.refresh_minutes, 5);
        assert_eq!(cfg.sync.initial_limit, 1000);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = isolate();
        let mut cfg = Config::default();

        cfg.accounts[0].email = "me@example.com".into();
        cfg.add_account(Account::new("work", "me@work.example"))
            .unwrap();
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
    fn legacy_account_migrates() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = isolate();
        let config = dir.path().join("config.toml");
        let secrets = dir.path().join("secrets.toml");

        std::fs::write(
            &config,
            "[account]\nemail = \"a@b.c\"\nsession_url = \"http://jmap.test/session\"\n",
        )
        .unwrap();
        std::fs::write(&secrets, "token = \"t\"\n").unwrap();

        let cfg = Config::load().unwrap();

        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].id, "default");
        assert_eq!(cfg.accounts[0].email, "a@b.c");
        assert_eq!(cfg.accounts[0].name, "a@b.c");
        assert_eq!(cfg.accounts[0].session_url, "http://jmap.test/session");
        assert!(cfg.accounts[0].enabled);
        assert_eq!(Secrets::token("default").unwrap().as_deref(), Some("t"));
        assert_eq!(Secrets::token("other").unwrap(), None);

        cfg.save().unwrap();
        Secrets::set_token("default", "t2").unwrap();

        let config_text = std::fs::read_to_string(&config).unwrap();
        let secrets_text = std::fs::read_to_string(&secrets).unwrap();

        assert!(config_text.contains("[[accounts]]"), "{config_text}");
        assert!(!config_text.contains("[account]"), "{config_text}");
        assert!(secrets_text.contains("[tokens]"), "{secrets_text}");
        assert!(
            !secrets_text.lines().any(|l| l.starts_with("token ")),
            "{secrets_text}"
        );
        assert_eq!(Secrets::token("default").unwrap().as_deref(), Some("t2"));

        // No `[account]` and no `[[accounts]]`: one fresh account.
        std::fs::write(&config, "remote_content = \"always\"\n").unwrap();

        let fresh = Config::load().unwrap();

        assert_eq!(fresh.accounts.len(), 1);
        assert_eq!(fresh.accounts[0].id, "default");
        assert_eq!(fresh.accounts[0].email, "");
        assert_eq!(fresh.remote_content, "always");
    }

    #[test]
    fn legacy_token_survives_first_write_of_another_account() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = isolate();

        std::fs::write(dir.path().join("secrets.toml"), "token = \"old\"\n").unwrap();
        Secrets::set_token("work", "w").unwrap();

        assert_eq!(Secrets::token("default").unwrap().as_deref(), Some("old"));
        assert_eq!(Secrets::token("work").unwrap().as_deref(), Some("w"));
    }

    #[test]
    fn account_id_validation() {
        let mut cfg = Config::default();

        for bad in ["Fast", "a b", "", "-lead", &"x".repeat(33)] {
            assert!(
                matches!(
                    cfg.add_account(Account::new(bad, "")),
                    Err(Error::Config(_))
                ),
                "accepted {bad:?}"
            );
        }

        assert!(cfg.add_account(Account::new("default", "")).is_err());
        assert!(cfg.add_account(Account::new("fastmail-2", "")).is_ok());
        assert!(cfg.add_account(Account::new("fastmail-2", "")).is_err());
        assert!(cfg.add_account(Account::new(&"x".repeat(32), "")).is_ok());

        // `load` applies the same rules to a hand-edited file.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = isolate();
        let config = dir.path().join("config.toml");

        std::fs::write(&config, "[[accounts]]\nid = \"Fast\"\n").unwrap();
        assert!(matches!(Config::load(), Err(Error::Config(_))));

        std::fs::write(
            &config,
            "[[accounts]]\nid = \"a\"\n[[accounts]]\nid = \"a\"\n",
        )
        .unwrap();
        assert!(matches!(Config::load(), Err(Error::Config(_))));
    }

    #[test]
    fn last_account_cannot_be_removed() {
        let mut cfg = Config::default();

        assert!(matches!(
            cfg.remove_account("default"),
            Err(Error::Config(_))
        ));
        assert!(matches!(
            cfg.remove_account("nope"),
            Err(Error::NotFound(_))
        ));

        cfg.add_account(Account::new("work", "")).unwrap();

        assert_eq!(cfg.remove_account("default").unwrap().id, "default");
        assert_eq!(cfg.default_account().unwrap().id, "work");
    }

    #[test]
    fn token_env_overrides_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = isolate();
        let cfg = Config::default();

        Secrets::set_token("default", "file").unwrap();

        // SAFETY: ENV_LOCK is held.
        unsafe { std::env::set_var("POSTO_TOKEN", "env") };
        assert_eq!(
            Secrets::token_for(&cfg, "default").unwrap().as_deref(),
            Some("env")
        );

        // SAFETY: ENV_LOCK is held.
        unsafe { std::env::remove_var("POSTO_TOKEN") };
        assert_eq!(
            Secrets::token_for(&cfg, "default").unwrap().as_deref(),
            Some("file")
        );

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

        let err = Secrets::token("default").unwrap_err();

        assert!(matches!(err, Error::Config(_)), "got {err:?}");
        assert!(
            !err.to_string().contains("review_dummy_secret"),
            "error leaks token: {err}"
        );

        std::fs::remove_file(dir.path().join("secrets.toml")).unwrap();
        Secrets::set_token("default", "again").unwrap();
        Secrets::clear_token("default").unwrap();
        assert_eq!(Secrets::token("default").unwrap(), None);
    }

    #[test]
    fn secrets_preserve_other_keys() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = isolate();

        Secrets::set_token("a", "ta").unwrap();
        Secrets::set_token("b", "tb").unwrap();
        Secrets::set_typesafe_key("key").unwrap();

        assert_eq!(Secrets::token("a").unwrap().as_deref(), Some("ta"));
        assert_eq!(Secrets::token("b").unwrap().as_deref(), Some("tb"));
        assert_eq!(Secrets::typesafe_key().unwrap().as_deref(), Some("key"));

        Secrets::clear_token("a").unwrap();

        assert_eq!(Secrets::token("a").unwrap(), None);
        assert_eq!(Secrets::token("b").unwrap().as_deref(), Some("tb"));
        assert_eq!(Secrets::typesafe_key().unwrap().as_deref(), Some("key"));

        // SAFETY: ENV_LOCK is held.
        unsafe { std::env::set_var("TYPESAFE_API_KEY", "env-key") };
        assert_eq!(Secrets::typesafe_key().unwrap().as_deref(), Some("env-key"));
        // SAFETY: ENV_LOCK is held.
        unsafe { std::env::remove_var("TYPESAFE_API_KEY") };
    }

    #[test]
    fn env_token_targets_default_account() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = isolate();
        let mut cfg = Config::default();

        cfg.add_account(Account::new("second", "")).unwrap();
        Secrets::set_token("default", "file-default").unwrap();
        Secrets::set_token("second", "file-second").unwrap();

        // SAFETY: ENV_LOCK is held.
        unsafe { std::env::set_var("POSTO_TOKEN", "env") };

        let token = |cfg: &Config, id: &str| Secrets::token_for(cfg, id).unwrap();

        assert_eq!(token(&cfg, "default").as_deref(), Some("env"));
        assert_eq!(token(&cfg, "second").as_deref(), Some("file-second"));

        // The first account is disabled: the second one is the default now.
        cfg.accounts[0].enabled = false;

        assert_eq!(token(&cfg, "default").as_deref(), Some("file-default"));
        assert_eq!(token(&cfg, "second").as_deref(), Some("env"));

        // No enabled account: the variable attaches to nothing.
        cfg.accounts[1].enabled = false;

        assert_eq!(token(&cfg, "default").as_deref(), Some("file-default"));
        assert_eq!(token(&cfg, "second").as_deref(), Some("file-second"));

        // SAFETY: ENV_LOCK is held.
        unsafe { std::env::remove_var("POSTO_TOKEN") };
    }
}
