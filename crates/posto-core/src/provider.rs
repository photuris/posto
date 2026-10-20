//! Provider-neutral types shared by the JMAP and Proton back ends.
//!
//! The rest of the core talks to one connection type, [`Connection`], and never
//! to `jmap::Client` or `proton::Session` directly. The types here are the
//! vocabulary of that contract.

use serde::{Deserialize, Serialize};

use crate::compose::Draft;
use crate::config::{Account, Config, ProtonSecret, Provider, Secrets};
use crate::error::{Error, Result};
use crate::jmap;
use crate::proton;
use crate::store::{Attachment, EmailBody, EmailSummary, Mailbox, Store};

/// A sending identity: a JMAP `Identity` or a Proton address that may send.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Identity {
    /// Server-assigned id.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Sender address.
    pub email: String,
}

/// A Fastmail masked email address the account may send from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MaskedEmail {
    /// Server-assigned id.
    pub id: String,
    /// The masked address itself.
    pub email: String,
    /// What the address is for, as the user wrote it.
    #[serde(default)]
    pub description: String,
    /// The site the address was created for; empty when unset.
    #[serde(default)]
    pub for_domain: String,
    /// `enabled` or `disabled`; other states are never kept.
    pub state: String,
    /// RFC 3339 time of the last message received, if any.
    #[serde(default)]
    pub last_message_at: Option<String>,
}

/// What an action changes on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Replace the mailbox set with this one mailbox.
    Mailbox(String),
    /// Leave `remove`, then join `add`.
    Patch {
        /// Mailbox ids to join.
        add: Vec<String>,
        /// Mailbox ids to leave.
        remove: Vec<String>,
    },
    /// Add (`true`) or remove (`false`) a keyword: `$seen`, `$flagged` or
    /// `$answered`.
    Keyword(&'static str, bool),
}

/// The server's answer to an update.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Updated {
    /// Ids the server refused; they are neither mirrored nor counted.
    pub refused: Vec<String>,
}

/// One page of changes after a sync cursor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeltaPage {
    /// Created and updated emails, as the server has them now.
    pub emails: Vec<EmailSummary>,
    /// Ids the server destroyed.
    pub destroyed: Vec<String>,
    /// Cursor after this page; persisted together with the page.
    pub state: String,
    /// More pages follow: call again with `state`.
    pub more: bool,
    /// Mailboxes changed too: the caller runs the mailbox sync again.
    pub mailboxes_changed: bool,
}

/// What one delta call found.
#[derive(Clone, Debug, PartialEq)]
pub enum Delta {
    /// Changes since the cursor.
    Page(DeltaPage),
    /// The server cannot report changes from that cursor: the caller clears
    /// the account's emails and runs an initial sync.
    Resync,
}

/// One account's server connection; the only type the core talks to.
pub enum Connection {
    /// A JMAP account (Fastmail).
    Jmap(jmap::Client),
    /// A Proton Mail account.
    Proton(proton::Session),
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Connection::Jmap(c) => c.fmt(f),
            Connection::Proton(s) => s.fmt(f),
        }
    }
}

impl Connection {
    /// Connects a JMAP account from its session URL and bearer token.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::Auth`] on 401/403,
    /// [`crate::error::Error::Http`] on other failures and
    /// [`crate::error::Error::Jmap`] when the session has no mail account.
    pub async fn connect_jmap(session_url: &str, token: &str) -> Result<Connection> {
        Ok(Connection::Jmap(
            jmap::Client::connect(session_url, token).await?,
        ))
    }

    /// Which provider this connection speaks to.
    pub fn provider(&self) -> Provider {
        match self {
            Connection::Jmap(_) => Provider::Jmap,
            Connection::Proton(_) => Provider::Proton,
        }
    }

    /// The server's own id for the account (not the local slug).
    pub fn server_account_id(&self) -> &str {
        match self {
            Connection::Jmap(c) => &c.session().account_id,
            Connection::Proton(s) => s.server_account_id(),
        }
    }

    /// The provider name written into the account's sync state.
    pub fn provider_name(&self) -> &'static str {
        match self {
            Connection::Jmap(_) => "jmap",
            Connection::Proton(s) => s.provider_name(),
        }
    }

    /// True when the server reports no mailbox counts and the store computes
    /// them from its cache after every sync.
    pub fn local_counts(&self) -> bool {
        match self {
            Connection::Jmap(_) => false,
            Connection::Proton(s) => s.local_counts(),
        }
    }

    /// Lists the account's sending identities.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails or the list is malformed.
    pub async fn identities(&self) -> Result<Vec<Identity>> {
        match self {
            Connection::Jmap(c) => c.identities().await,
            Connection::Proton(s) => s.identities().await,
        }
    }

    /// Lists the account's masked email addresses; empty for Proton and for a
    /// JMAP server that does not advertise the masked email capability.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails or the list is malformed.
    pub async fn masked_emails(&self) -> Result<Vec<MaskedEmail>> {
        match self {
            Connection::Jmap(c) => c.masked_emails().await,
            Connection::Proton(_) => Ok(Vec::new()),
        }
    }

    /// Creates a top-level mailbox called `name`. The returned mailbox has an
    /// empty `account_id`; the caller sets the local slug before storing it.
    ///
    /// # Errors
    ///
    /// Returns an error when the server refuses or the call fails.
    pub async fn create_mailbox(&self, name: &str) -> Result<Mailbox> {
        match self {
            Connection::Jmap(c) => c.create_mailbox(name).await,
            Connection::Proton(s) => s.create_mailbox(name).await,
        }
    }

    /// Brings the store's mailboxes of `account` up to date.
    ///
    /// # Errors
    ///
    /// Returns any network, server or database error.
    pub async fn sync_mailboxes(&self, store: &Store, account: &str) -> Result<()> {
        match self {
            Connection::Jmap(c) => c.sync_mailboxes(store, account).await,
            Connection::Proton(s) => s.sync_mailboxes(store, account).await,
        }
    }

    /// The `limit` newest emails, and the cursor captured before paging so
    /// changes made during paging show up in the next [`Connection::delta`].
    ///
    /// # Errors
    ///
    /// Returns any network or server error.
    pub async fn initial(
        &self,
        account: &str,
        limit: usize,
    ) -> Result<(Vec<EmailSummary>, String)> {
        match self {
            Connection::Jmap(c) => c.initial(account, limit).await,
            Connection::Proton(s) => s.initial(account, limit).await,
        }
    }

    /// One page of changes after the cursor `since`.
    ///
    /// # Errors
    ///
    /// Returns any network or server error.
    pub async fn delta(&self, account: &str, since: &str) -> Result<Delta> {
        match self {
            Connection::Jmap(c) => c.delta(account, since).await,
            Connection::Proton(s) => s.delta(account, since).await,
        }
    }

    /// The body of `email_id`; `None` when the server does not know the email.
    ///
    /// # Errors
    ///
    /// Returns any network or server error.
    pub async fn body(&self, email_id: &str) -> Result<Option<EmailBody>> {
        match self {
            Connection::Jmap(c) => c.body(email_id).await,
            Connection::Proton(s) => s.body(email_id).await,
        }
    }

    /// Downloads attachment `att` of email `email_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the download fails.
    pub async fn download(&self, email_id: &str, att: &Attachment) -> Result<Vec<u8>> {
        match self {
            // JMAP addresses blobs by id alone.
            Connection::Jmap(c) => {
                let _ = email_id;

                c.download_blob(&att.blob_id, &att.name, &att.mime).await
            }
            Connection::Proton(s) => s.download(email_id, att).await,
        }
    }

    /// Applies `change` to `ids`; the ids the server refused are in the result.
    ///
    /// # Errors
    ///
    /// Returns any network or server error.
    pub async fn update(&self, ids: &[String], change: &Change) -> Result<Updated> {
        match self {
            Connection::Jmap(c) => c.update(ids, change).await,
            Connection::Proton(s) => s.update(ids, change).await,
        }
    }

    /// Permanently deletes `ids`; returns the ids that are gone.
    ///
    /// # Errors
    ///
    /// Returns any network or server error.
    pub async fn destroy(&self, ids: &[String]) -> Result<Vec<String>> {
        match self {
            Connection::Jmap(c) => c.destroy(ids).await,
            Connection::Proton(s) => s.destroy(ids).await,
        }
    }

    /// Whether the email has a non-empty `List-Unsubscribe` header; a failed
    /// fetch counts as no.
    pub async fn has_unsubscribe(&self, email_id: &str) -> bool {
        match self {
            Connection::Jmap(c) => c.has_unsubscribe(email_id).await,
            Connection::Proton(s) => s.has_unsubscribe(email_id).await,
        }
    }

    /// Sends `draft` as `identity` with the rendered `text` and `html` parts
    /// and returns the new email id.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::NotFound`] when the drafts or sent
    /// mailbox is unknown and a provider error when the server refuses. A mail
    /// that was sent but not filed under Sent is an
    /// [`crate::error::Error::Other`]: callers must not resend then.
    pub async fn send(
        &self,
        store: &Store,
        account: &str,
        identity: &Identity,
        draft: &Draft,
        text: &str,
        html: &str,
    ) -> Result<String> {
        match self {
            Connection::Jmap(c) => c.send(store, account, identity, draft, text, html).await,
            Connection::Proton(s) => s.send(store, account, identity, draft, text, html).await,
        }
    }
}

/// The stored credentials of one account.
#[doc(hidden)]
pub enum Credentials {
    /// A JMAP bearer token.
    Token(String),
    /// A Proton login.
    Proton(ProtonSecret),
}

/// Connects `account` with its stored credentials; `Ok(None)` when it has
/// none, so callers that work offline can carry on.
///
/// # Errors
///
/// Returns [`Error::NotConfigured`] when a Proton account has no
/// `proton.app_version`, [`Error::Auth`] on 401/403, and any other network,
/// server or secrets-file error.
pub async fn connect(cfg: &Config, account: &Account) -> Result<Option<Connection>> {
    if !Secrets::has_credentials(cfg, account)? {
        return Ok(None);
    }

    let credentials = match account.provider {
        Provider::Jmap => Secrets::token_for(cfg, &account.id)?.map(Credentials::Token),
        Provider::Proton => Secrets::proton(&account.id)?.map(Credentials::Proton),
    };

    match credentials {
        Some(credentials) => connect_with_credentials(cfg, account, credentials)
            .await
            .map(Some),
        None => Ok(None),
    }
}

/// [`connect`] with the credentials supplied instead of read from the secrets
/// file.
///
/// # Errors
///
/// As for [`connect`]; credentials of the other provider are
/// [`Error::NotConfigured`] (see [`credential_error`]).
#[doc(hidden)]
pub async fn connect_with_credentials(
    cfg: &Config,
    account: &Account,
    credentials: Credentials,
) -> Result<Connection> {
    match (account.provider, credentials) {
        (Provider::Jmap, Credentials::Token(token)) => Ok(Connection::Jmap(
            jmap::Client::connect(&account.session_url, &token).await?,
        )),
        (Provider::Proton, Credentials::Proton(secret)) => {
            let app_version = cfg
                .proton_app_version()
                .ok_or_else(|| Error::NotConfigured("proton.app_version".into()))?;
            let id = account.id.clone();
            let generation = secret.generation;
            let session = proton::connect(
                &app_version,
                &secret.session,
                &secret.mailbox_password,
                Some(Box::new(move |blob| {
                    Secrets::set_proton_session(&id, generation, blob)
                })),
            )
            .await?;

            Ok(Connection::Proton(session))
        }
        _ => Err(credential_error(account)),
    }
}

/// The error for a command that needs a connection when [`connect`] found no
/// credentials.
pub fn credential_error(account: &Account) -> Error {
    let id = &account.id;

    match account.provider {
        Provider::Jmap => Error::NotConfigured(format!("token for account {id}")),
        Provider::Proton => Error::NotConfigured(format!("proton login for account {id}")),
    }
}
