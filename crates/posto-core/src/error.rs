//! Error type shared by every posto-core module.

/// Everything that can go wrong in posto-core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A required setting such as the token or email is missing.
    #[error("not configured: {0}")]
    NotConfigured(String),
    /// Filesystem failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite failure.
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    /// Malformed or unusable configuration.
    #[error("config: {0}")]
    Config(String),
    /// Transport failure.
    #[error("http: {0}")]
    Http(String),
    /// The server answered 401/403.
    #[error("auth rejected")]
    Auth,
    /// The server reported a JMAP error.
    #[error("jmap: {0}")]
    Jmap(String),
    /// A requested item does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Another process holds the sync lock.
    #[error("locked: {0}")]
    Locked(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Result alias using [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Process exit code for this error (2 is reserved for CLI usage errors).
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::NotConfigured(_) | Error::Config(_) => 3,
            Error::Auth => 4,
            Error::Http(_) | Error::Jmap(_) => 5,
            Error::NotFound(_) => 6,
            Error::Locked(_) => 7,
            _ => 1,
        }
    }
}
