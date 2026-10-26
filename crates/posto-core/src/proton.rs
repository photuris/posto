//! Proton Mail back end over `proton-mail-api`: login, resumed sessions, and
//! the provider operations mapped onto Proton's labels, folders and events.

use std::fmt;
use std::future::Future;
use std::sync::Mutex;

use proton_mail_api::compose::BodyType;
use proton_mail_api::error::Error as PmError;
use proton_mail_api::events::{EventAction, MessageEvent};
use proton_mail_api::keys::UnlockedKeys;
use proton_mail_api::labels::{Label, LabelType, system};
use proton_mail_api::messages::{Message, MessageMetadata, MessagePage, MessageQuery, Recipient};
use proton_mail_api::send::{OutgoingMessage, ParentAction, ParentRef};
use proton_mail_api::session::{self as pm, PasswordMode, PersistedSession};

use crate::compose::Draft;
use crate::error::{Error, Result};
use crate::provider::{Change, Delta, DeltaPage, Identity, Updated};
use crate::store::{Address, Attachment, EmailBody, EmailSummary, Mailbox, Store};
use crate::unsubscribe;

/// Messages per page of the initial sync (the API maximum).
const PAGE_SIZE: usize = 150;

/// Extra requests for a page the server flags as stale, before giving up.
const STALE_RETRIES: usize = 3;

/// Time stamp used when a server time cannot be represented.
const EPOCH: &str = "1970-01-01T00:00:00Z";

/// The built-in mailboxes as `(label id, role, default name, sort order)`.
const BUILT_IN: [(&str, &str, &str, i64); 6] = [
    ("0", "inbox", "Inbox", 0),
    ("8", "drafts", "Drafts", 1),
    ("7", "sent", "Sent", 2),
    ("6", "archive", "Archive", 3),
    ("4", "junk", "Spam", 4),
    ("3", "trash", "Trash", 5),
];

/// What a successful login hands back for the caller to store.
pub struct Login {
    /// The serialised crate session, key password included.
    pub session: String,
    /// The password that unlocked the keys.
    pub mailbox_password: String,
    /// The primary sending address, or empty when the account has none.
    pub email: String,
}

impl fmt::Debug for Login {
    /// Prints every field as `<redacted>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Login")
            .field("session", &"<redacted>")
            .field("mailbox_password", &"<redacted>")
            .field("email", &"<redacted>")
            .finish()
    }
}

/// Called with the new session blob whenever the tokens changed. Returns
/// `Ok(true)` when it was written.
pub type Persist = Box<dyn Fn(&str) -> Result<bool> + Send + Sync>;

/// A signed-in Proton account: the crate session, its unlocked keys and the
/// bookkeeping that keeps the stored session blob current.
pub struct Session {
    /// The crate session.
    inner: pm::Session,
    /// Keys that decrypt bodies and sign outgoing mail.
    keys: UnlockedKeys,
    /// The server's user id.
    user_id: String,
    /// Where rotated tokens go.
    persist: Option<Persist>,
    /// The last session blob known to be stored.
    last_blob: Mutex<String>,
}

impl fmt::Debug for Session {
    /// Prints the user id and key counts only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("user_keys", &self.keys.user_keys.len())
            .field("address_keys", &self.keys.address_keys.len())
            .finish()
    }
}

// ── Errors and small mappers ────────────────────────────────────────────────

/// Maps a crate error; only the crate's own `Display` text is used, never
/// server text.
pub(crate) fn map_err(e: PmError) -> Error {
    match e {
        PmError::SessionExpired | PmError::WrongPassword => Error::Auth,
        // Insufficient scope: since the crate unlocks from the stored key
        // password, this means that password no longer works (changed
        // password); only a new sign-in fixes it.
        PmError::Api { code: 9101, .. } => Error::Auth,
        PmError::SendOutcomeUnknown { draft_id } => Error::Other(format!(
            "sent? outcome unknown for draft {draft_id}; do not resend"
        )),
        e => Error::Proton(e.to_string()),
    }
}

/// The steps of a sign-in, named in its errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoginStep {
    /// The SRP password exchange.
    Password,
    /// Submitting the TOTP code.
    TwoFactor,
    /// Fetching and unlocking the keys.
    Keys,
    /// Listing the addresses.
    Addresses,
}

/// Maps a sign-in failure, naming the step. Only a password Proton refused is
/// `Error::Auth`; every other refusal says where it happened, because
/// "auth rejected" alone cannot tell a wrong password from a session Proton
/// will not authorise.
pub(crate) fn login_err(step: LoginStep, e: PmError) -> Error {
    match (step, e) {
        (LoginStep::Password, PmError::WrongPassword) => Error::Auth,
        (LoginStep::Password, PmError::SessionExpired) => {
            Error::Proton("Proton refused the sign-in request (HTTP 401)".into())
        }
        (LoginStep::TwoFactor, PmError::WrongPassword | PmError::SessionExpired) => {
            Error::Proton("Proton rejected the two-factor code".into())
        }
        (step, PmError::WrongPassword | PmError::SessionExpired) => Error::Proton(format!(
            "the password was accepted, but Proton refused the session while {}; \
             the account may need a second factor Posto cannot provide (such as a security key)",
            match step {
                LoginStep::Keys => "reading the keys",
                _ => "listing the addresses",
            }
        )),
        (step, e) => match map_err(e) {
            Error::Proton(text) => Error::Proton(format!("{} step: {text}", step_name(step))),
            other => other,
        },
    }
}

fn step_name(step: LoginStep) -> &'static str {
    match step {
        LoginStep::Password => "password",
        LoginStep::TwoFactor => "two-factor",
        LoginStep::Keys => "key unlock",
        LoginStep::Addresses => "address",
    }
}

/// True for the HTTP statuses that mean "that item does not exist".
fn is_gone(e: &PmError) -> bool {
    matches!(
        e,
        PmError::Api {
            status: 404 | 422,
            ..
        }
    )
}

/// Formats Unix seconds as RFC 3339 UTC.
pub(crate) fn rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0).map_or_else(
        || EPOCH.to_owned(),
        |t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

/// True for Proton's built-in label ids, which are small integers.
pub(crate) fn is_system(id: &str) -> bool {
    id.parse::<u32>().is_ok()
}

/// True for a system id that is not one of the six built-in mailboxes.
pub(crate) fn is_hidden(id: &str) -> bool {
    is_system(id) && !BUILT_IN.iter().any(|b| b.0 == id)
}

/// Maps one Proton label to a mailbox; `None` for hidden system views and
/// kinds the core does not model.
pub(crate) fn mailbox_from_label(account: &str, label: &Label, index: usize) -> Option<Mailbox> {
    let custom_order = 100 + index as i64;
    let (role, parent_id, sort_order) = match LabelType::from_i64(label.r#type)? {
        LabelType::System => {
            let built_in = BUILT_IN.iter().find(|b| b.0 == label.id)?;

            (Some(built_in.1.to_owned()), None, built_in.3)
        }
        LabelType::Folder => (
            Some("folder".to_owned()),
            label.parent_id.clone(),
            custom_order,
        ),
        LabelType::Label => (None, None, custom_order),
    };

    Some(Mailbox {
        account_id: account.to_owned(),
        id: label.id.clone(),
        name: label.name.clone(),
        role,
        parent_id,
        sort_order,
        total: 0,
        unread: 0,
    })
}

/// The mapped labels plus every built-in the server did not list.
pub(crate) fn mailboxes_from_labels(account: &str, labels: &[Label]) -> Vec<Mailbox> {
    let mut mailboxes: Vec<Mailbox> = labels
        .iter()
        .enumerate()
        .filter_map(|(i, l)| mailbox_from_label(account, l, i))
        .collect();

    for (id, role, name, sort_order) in BUILT_IN {
        if mailboxes.iter().any(|m| m.id == id) {
            continue;
        }

        mailboxes.push(Mailbox {
            account_id: account.to_owned(),
            id: id.to_owned(),
            name: name.to_owned(),
            role: Some(role.to_owned()),
            parent_id: None,
            sort_order,
            total: 0,
            unread: 0,
        });
    }

    mailboxes
}

/// Converts a Proton recipient; an empty name becomes `None`.
fn address_from(r: &Recipient) -> Address {
    Address {
        name: Some(r.name.clone()).filter(|n| !n.is_empty()),
        email: r.address.clone(),
    }
}

/// Maps listed message metadata to a summary.
pub(crate) fn summary_from(account: &str, m: &MessageMetadata) -> EmailSummary {
    let in_label = |id: &str| m.label_ids.iter().any(|l| l == id);
    let flags = [
        (!m.unread.0, "$seen"),
        (in_label(system::STARRED), "$flagged"),
        (m.is_replied.0 || m.is_replied_all.0, "$answered"),
        (
            in_label(system::DRAFTS) || in_label(system::ALL_DRAFTS),
            "$draft",
        ),
        (m.is_forwarded.0, "$forwarded"),
    ];

    EmailSummary {
        account_id: account.to_owned(),
        id: m.id.clone(),
        thread_id: m
            .conversation_id
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| m.id.clone()),
        received_at: rfc3339(m.time),
        from: m.sender.iter().map(address_from).collect(),
        to: m.to_list.iter().map(address_from).collect(),
        cc: m.cc_list.iter().map(address_from).collect(),
        subject: m.subject.clone(),
        preview: String::new(),
        size: m.size,
        has_attachment: m.num_attachments > 0,
        mailbox_ids: m
            .label_ids
            .iter()
            .filter(|l| !is_hidden(l))
            .cloned()
            .collect(),
        keywords: flags
            .into_iter()
            .filter(|(on, _)| *on)
            .map(|(_, k)| k.to_owned())
            .collect(),
    }
}

/// The values of every header line named `name` in a header block, folded
/// lines joined and each value trimmed.
pub(crate) fn header_values(header: &str, name: &str) -> Vec<String> {
    let mut logical: Vec<String> = Vec::new();

    for line in header.lines() {
        if line.is_empty() {
            break;
        }

        match logical.last_mut() {
            Some(last) if line.starts_with([' ', '\t']) => {
                last.push(' ');
                last.push_str(line.trim());
            }
            _ => logical.push(line.to_owned()),
        }
    }

    logical
        .iter()
        .filter_map(|l| l.split_once(':'))
        .filter(|(n, _)| n.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_owned())
        .collect()
}

/// The message ids of every header named `name`, without angle brackets.
pub(crate) fn header_ids(header: &str, name: &str) -> Vec<String> {
    header_values(header, name)
        .iter()
        .flat_map(|v| v.split_whitespace())
        .map(|id| id.trim_matches(['<', '>']).to_owned())
        .filter(|id| !id.is_empty())
        .collect()
}

/// Builds the body record from a fetched message and its decrypted parts.
pub(crate) fn body_from(
    email_id: &str,
    message: &Message,
    text: Option<String>,
    html: Option<String>,
) -> EmailBody {
    EmailBody {
        email_id: email_id.to_owned(),
        html,
        text,
        attachments: message
            .attachments
            .iter()
            .map(|a| Attachment {
                blob_id: a.id.clone(),
                name: a.name.clone(),
                mime: a.mime_type.clone(),
                size: a.size,
            })
            .collect(),
        in_reply_to: header_ids(&message.header, "In-Reply-To"),
        references: header_ids(&message.header, "References"),
        message_id: header_ids(&message.header, "Message-ID"),
        reply_to: message.meta.reply_tos.iter().map(address_from).collect(),
        unsubscribe: header_values(&message.header, "List-Unsubscribe")
            .first()
            .and_then(|h| {
                let post = header_values(&message.header, "List-Unsubscribe-Post");

                unsubscribe::parse(h, post.first().map(String::as_str))
            }),
    }
}

/// Builds the crate's outgoing message from a draft and its rendered HTML.
pub(crate) fn outgoing_from(identity: &Identity, draft: &Draft, html: &str) -> OutgoingMessage {
    let recipient = |a: &Address| Recipient {
        name: a.name.clone().unwrap_or_default(),
        address: a.email.clone(),
    };

    OutgoingMessage {
        from: Recipient {
            name: identity.name.clone(),
            address: identity.email.clone(),
        },
        to: draft.to.iter().map(recipient).collect(),
        cc: draft.cc.iter().map(recipient).collect(),
        bcc: draft.bcc.iter().map(recipient).collect(),
        subject: draft.subject.clone(),
        body: html.to_owned(),
        body_type: BodyType::Html,
        parent: draft.reply_to_email_id.as_ref().map(|id| ParentRef {
            id: id.clone(),
            action: if draft.cc.is_empty() {
                ParentAction::Reply
            } else {
                ParentAction::ReplyAll
            },
            forward_attachments: vec![],
        }),
        attachments: vec![],
    }
}

// ── Session persistence ─────────────────────────────────────────────────────

/// Hands the session blob to `persist` when it differs from the last blob
/// known to be stored. A failed write is logged and retried by the next call.
fn persist_if_changed(inner: &pm::Session, persist: &Option<Persist>, last_blob: &Mutex<String>) {
    let Some(persist) = persist else {
        return;
    };
    let Ok(now) = serde_json::to_string(&inner.persist()) else {
        return;
    };
    let mut last = last_blob.lock().unwrap_or_else(|e| e.into_inner());

    if now == *last {
        return;
    }

    match persist(&now) {
        Ok(_) => *last = now,
        Err(e) => tracing::warn!("proton session not saved: {e}"),
    }
}

// ── Login and connect ───────────────────────────────────────────────────────

/// Builds the crate client for `app_version`.
fn client(app_version: &str) -> Result<pm::Client> {
    if app_version.trim().is_empty() {
        return Err(Error::NotConfigured("proton.app_version".into()));
    }

    pm::Client::builder()
        .app_version(app_version)
        .user_agent(format!(
            "posto/{} ({})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        ))
        .build()
        .map_err(map_err)
}

/// Signs in to Proton.
///
/// # Errors
///
/// `Error::Auth` for a wrong password, `Error::Proton` when a second factor
/// or the mailbox password is needed and missing, and the mapped crate
/// errors otherwise.
pub async fn login(
    app_version: &str,
    username: &str,
    password: &str,
    totp: Option<&str>,
    mailbox_password: Option<&str>,
) -> Result<Login> {
    login_with(
        client(app_version)?,
        username,
        password,
        totp,
        mailbox_password,
    )
    .await
}

/// [`login`] with a prepared client.
///
/// # Errors
///
/// As for [`login`].
#[doc(hidden)]
pub async fn login_with(
    client: pm::Client,
    username: &str,
    password: &str,
    totp: Option<&str>,
    mailbox_password: Option<&str>,
) -> Result<Login> {
    let outcome = client
        .login(username, password)
        .await
        .map_err(|e| login_err(LoginStep::Password, e))?;
    let session = outcome.session;

    if outcome.needs_totp {
        let code = totp.ok_or_else(|| Error::Proton("two-factor code required".into()))?;

        session
            .submit_totp(code)
            .await
            .map_err(|e| login_err(LoginStep::TwoFactor, e))?;
    }

    let unlock_with = match (outcome.password_mode, mailbox_password) {
        (PasswordMode::Two, None) => {
            return Err(Error::Proton("mailbox password required".into()));
        }
        (PasswordMode::Two, Some(mailbox)) => mailbox,
        (PasswordMode::Single, _) => password,
    };

    session
        .unlock_keys(unlock_with)
        .await
        .map_err(|e| login_err(LoginStep::Keys, e))?;

    let addresses = session
        .addresses()
        .await
        .map_err(|e| login_err(LoginStep::Addresses, e))?;
    let email = addresses
        .iter()
        .find(|a| a.send.0 && a.status == 1)
        .or(addresses.first())
        .map(|a| a.email.clone())
        .unwrap_or_default();
    let blob = serde_json::to_string(&session.persist())
        .map_err(|_| Error::Proton("session cannot be serialised".into()))?;

    Ok(Login {
        session: blob,
        mailbox_password: unlock_with.to_owned(),
        email,
    })
}

/// Resumes a stored session and unlocks its keys.
///
/// # Errors
///
/// `Error::NotConfigured` when the stored session is unreadable, otherwise
/// the mapped crate errors.
pub async fn connect(
    app_version: &str,
    session: &str,
    mailbox_password: &str,
    persist: Option<Persist>,
) -> Result<Session> {
    connect_with(client(app_version)?, session, mailbox_password, persist).await
}

/// [`connect`] with a prepared client.
///
/// # Errors
///
/// As for [`connect`].
#[doc(hidden)]
pub async fn connect_with(
    client: pm::Client,
    session: &str,
    mailbox_password: &str,
    persist: Option<Persist>,
) -> Result<Session> {
    let blob: PersistedSession = serde_json::from_str(session).map_err(|_| {
        Error::NotConfigured("proton session for this account is unreadable; sign in again".into())
    })?;
    let inner = client.resume(blob).map_err(map_err)?;
    let last_blob = Mutex::new(session.to_owned());

    // A refresh that happens during a failing call is stored before the error
    // is returned.
    let keys = inner.unlock_keys(mailbox_password).await;

    persist_if_changed(&inner, &persist, &last_blob);

    let keys = keys.map_err(map_err)?;
    let user = inner.user().await;

    persist_if_changed(&inner, &persist, &last_blob);

    Ok(Session {
        inner,
        keys,
        user_id: user.map_err(map_err)?.id,
        persist,
        last_blob,
    })
}

// ── The provider operations ─────────────────────────────────────────────────

/// Treats a refusal of some ids as data: they join `refused`, the call goes on.
fn absorb(result: std::result::Result<(), PmError>, refused: &mut Vec<String>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(PmError::ActionFailed { ids }) => {
            for id in ids {
                if !refused.contains(&id) {
                    refused.push(id);
                }
            }

            Ok(())
        }
        Err(e) => Err(map_err(e)),
    }
}

/// Records `id` in `list` unless it is already there.
fn add_id(list: &mut Vec<String>, id: &str) {
    if !list.iter().any(|i| i == id) {
        list.push(id.to_owned());
    }
}

/// Splits event entries into ids that changed and ids that were destroyed,
/// keeping the last action per id.
fn classify_events<'a>(
    entries: impl Iterator<Item = &'a MessageEvent>,
) -> (Vec<String>, Vec<String>) {
    let mut changed: Vec<String> = Vec::new();
    let mut destroyed: Vec<String> = Vec::new();

    for entry in entries {
        match EventAction::from_i64(entry.action) {
            Some(EventAction::Delete) => {
                changed.retain(|i| *i != entry.id);
                add_id(&mut destroyed, &entry.id);
            }
            Some(_) => {
                destroyed.retain(|i| *i != entry.id);
                add_id(&mut changed, &entry.id);
            }
            None => {}
        }
    }

    (changed, destroyed)
}

impl Session {
    /// A session over an existing crate session, for tests.
    #[doc(hidden)]
    pub fn for_tests(
        inner: pm::Session,
        keys: UnlockedKeys,
        user_id: &str,
        persist: Option<Persist>,
    ) -> Session {
        let last_blob = serde_json::to_string(&inner.persist()).unwrap_or_default();

        Session {
            inner,
            keys,
            user_id: user_id.to_owned(),
            persist,
            last_blob: Mutex::new(last_blob),
        }
    }

    /// Stores the session blob if the tokens changed since the last write.
    fn persisted(&self) {
        persist_if_changed(&self.inner, &self.persist, &self.last_blob);
    }

    /// Awaits `op`, then stores rotated tokens whether it succeeded or not.
    async fn tracked<T>(&self, op: impl Future<Output = Result<T>>) -> Result<T> {
        let result = op.await;

        self.persisted();

        result
    }

    /// One page of the initial listing, asked again while the server calls it
    /// stale.
    async fn fresh_page(&self, page: u32) -> Result<MessagePage> {
        let query = MessageQuery {
            label_id: Some(system::ALL_MAIL.to_owned()),
            page,
            page_size: PAGE_SIZE as u32,
            desc: true,
        };

        for _ in 0..=STALE_RETRIES {
            let reply = self.inner.list_messages(&query).await.map_err(map_err)?;

            if !reply.stale {
                return Ok(reply);
            }
        }

        Err(Error::Proton(
            "message index is stale; try again later".into(),
        ))
    }

    /// The server's user id.
    pub fn server_account_id(&self) -> &str {
        &self.user_id
    }

    /// Always `"proton"`.
    pub fn provider_name(&self) -> &'static str {
        "proton"
    }

    /// Proton lists no message counts, so the core counts locally.
    pub fn local_counts(&self) -> bool {
        true
    }

    /// The addresses that may send.
    ///
    /// # Errors
    ///
    /// The mapped crate errors.
    pub async fn identities(&self) -> Result<Vec<Identity>> {
        self.tracked(async {
            let addresses = self.inner.addresses().await.map_err(map_err)?;

            Ok(addresses
                .into_iter()
                .filter(|a| a.send.0 && a.status == 1)
                .map(|a| Identity {
                    id: a.id,
                    name: a.display_name,
                    email: a.email,
                })
                .collect())
        })
        .await
    }

    /// Not available on Proton.
    ///
    /// # Errors
    ///
    /// Always `Error::Unsupported`.
    pub async fn create_mailbox(&self, _name: &str) -> Result<Mailbox> {
        self.tracked(async {
            Err(Error::Unsupported(
                "creating mailboxes on Proton accounts".into(),
            ))
        })
        .await
    }

    /// Refreshes the account's mailboxes from Proton's labels, keeping stored
    /// counts and dropping mailboxes the server no longer has.
    ///
    /// # Errors
    ///
    /// The mapped crate errors or a store failure.
    pub async fn sync_mailboxes(&self, store: &Store, account: &str) -> Result<()> {
        self.tracked(async {
            let labels = self.inner.labels().await.map_err(map_err)?;
            let mut mapped = mailboxes_from_labels(account, &labels);
            let stored = store.mailboxes(account)?;

            for mailbox in &mut mapped {
                if let Some(old) = stored.iter().find(|s| s.id == mailbox.id) {
                    mailbox.total = old.total;
                    mailbox.unread = old.unread;
                }
            }

            let gone: Vec<String> = stored
                .iter()
                .filter(|s| !mapped.iter().any(|m| m.id == s.id))
                .map(|s| s.id.clone())
                .collect();

            store.delete_mailboxes(account, &gone)?;
            store.upsert_mailboxes(&mapped)
        })
        .await
    }

    /// The newest `limit` messages and the event cursor captured before
    /// paging, so arrivals during paging are picked up by the next delta.
    ///
    /// # Errors
    ///
    /// The mapped crate errors, or `Error::Proton` when the index stays stale.
    pub async fn initial(
        &self,
        account: &str,
        limit: usize,
    ) -> Result<(Vec<EmailSummary>, String)> {
        self.tracked(async {
            let cursor = self.inner.latest_event_id().await.map_err(map_err)?;
            let mut summaries: Vec<EmailSummary> = Vec::new();
            let mut page = 0;

            while summaries.len() < limit {
                let reply = self.fresh_page(page).await?;
                let full = reply.messages.len() == PAGE_SIZE;

                summaries.extend(reply.messages.iter().map(|m| summary_from(account, m)));

                if !full {
                    break;
                }

                page += 1;
            }

            summaries.truncate(limit);

            Ok((summaries, cursor))
        })
        .await
    }

    /// The changes after the event id `since`.
    ///
    /// # Errors
    ///
    /// The mapped crate errors; an expired cursor is `Delta::Resync`.
    pub async fn delta(&self, account: &str, since: &str) -> Result<Delta> {
        self.tracked(async {
            let batch = match self.inner.events(since).await {
                Ok(batch) => batch,
                Err(PmError::Api {
                    status: 400 | 422, ..
                }) => return Ok(Delta::Resync),
                Err(e) => return Err(map_err(e)),
            };

            if batch.events.iter().any(|e| e.requires_resync()) {
                return Ok(Delta::Resync);
            }

            let (changed, mut destroyed) =
                classify_events(batch.events.iter().flat_map(|e| &e.messages));
            let mut emails = Vec::new();

            // The event's inline message is partial for flag updates, so every
            // changed message is fetched whole.
            for id in changed {
                match self.inner.message(&id).await {
                    Ok(m) => emails.push(summary_from(account, &m.meta)),
                    Err(e) if is_gone(&e) => add_id(&mut destroyed, &id),
                    Err(e) => return Err(map_err(e)),
                }
            }

            Ok(Delta::Page(DeltaPage {
                emails,
                destroyed,
                state: batch.latest_event_id,
                more: batch.more,
                mailboxes_changed: batch.events.iter().any(|e| !e.labels.is_empty()),
            }))
        })
        .await
    }

    /// Fetches and decrypts a body; `None` when the message is gone.
    ///
    /// # Errors
    ///
    /// The mapped crate errors, including a body no key decrypts.
    pub async fn body(&self, email_id: &str) -> Result<Option<EmailBody>> {
        self.tracked(async {
            let message = match self.inner.message(email_id).await {
                Ok(m) => m,
                Err(e) if is_gone(&e) => return Ok(None),
                Err(e) => return Err(map_err(e)),
            };
            let decrypted = message.decrypt_body(&self.keys).map_err(map_err)?;
            let (text, html) = decrypted.text_and_html();

            Ok(Some(body_from(email_id, &message, text, html)))
        })
        .await
    }

    /// Downloads and decrypts one attachment of a message.
    ///
    /// # Errors
    ///
    /// `Error::NotFound` when the message has no such attachment, otherwise
    /// the mapped crate errors.
    pub async fn download(&self, email_id: &str, att: &Attachment) -> Result<Vec<u8>> {
        self.tracked(async {
            let message = self.inner.message(email_id).await.map_err(map_err)?;
            let found = message
                .attachments
                .iter()
                .find(|a| a.id == att.blob_id)
                .ok_or_else(|| Error::NotFound(format!("attachment {}", att.blob_id)))?;

            self.inner
                .attachment(found, &self.keys)
                .await
                .map_err(map_err)
        })
        .await
    }

    /// Applies `change` to `ids`; ids Proton refuses come back as `refused`.
    ///
    /// # Errors
    ///
    /// The mapped crate errors; the first one stops the sequence.
    pub async fn update(&self, ids: &[String], change: &Change) -> Result<Updated> {
        self.tracked(async {
            let mut refused = Vec::new();

            if ids.is_empty() {
                return Ok(Updated::default());
            }

            match change {
                Change::Mailbox(target) => {
                    absorb(self.inner.move_to(ids, target).await, &mut refused)?;
                }
                Change::Patch { add, remove } => {
                    for label in remove {
                        absorb(self.inner.remove_label(ids, label).await, &mut refused)?;
                    }

                    for label in add {
                        absorb(self.inner.add_label(ids, label).await, &mut refused)?;
                    }
                }
                Change::Keyword("$seen", true) => {
                    self.inner.mark_read(ids).await.map_err(map_err)?;
                }
                Change::Keyword("$seen", false) => {
                    self.inner.mark_unread(ids).await.map_err(map_err)?;
                }
                Change::Keyword("$flagged", true) => {
                    let result = self.inner.add_label(ids, system::STARRED).await;

                    absorb(result, &mut refused)?;
                }
                Change::Keyword("$flagged", false) => {
                    let result = self.inner.remove_label(ids, system::STARRED).await;

                    absorb(result, &mut refused)?;
                }
                // Proton sets `$answered` itself when a reply goes through it.
                Change::Keyword(..) => {}
            }

            Ok(Updated { refused })
        })
        .await
    }

    /// Permanently deletes `ids`; returns those the server did not refuse.
    ///
    /// # Errors
    ///
    /// Any transport or server error; a refusal of some ids is not an error.
    pub async fn destroy(&self, ids: &[String]) -> Result<Vec<String>> {
        self.tracked(async {
            let mut refused = Vec::new();

            absorb(self.inner.delete_messages(ids).await, &mut refused)?;

            Ok(ids
                .iter()
                .filter(|id| !refused.contains(id))
                .cloned()
                .collect())
        })
        .await
    }

    /// True when the message carries a `List-Unsubscribe` header; a failed
    /// fetch is `false`.
    pub async fn has_unsubscribe(&self, email_id: &str) -> bool {
        let found = self
            .inner
            .message(email_id)
            .await
            .ok()
            .is_some_and(|m| !header_values(&m.header, "List-Unsubscribe").is_empty());

        self.persisted();

        found
    }

    /// Sends the draft through Proton and returns the sent message's id.
    ///
    /// # Errors
    ///
    /// The mapped crate errors; an unknown send outcome is `Error::Other` so
    /// callers never resend.
    pub async fn send(
        &self,
        _store: &Store,
        _account: &str,
        identity: &Identity,
        draft: &Draft,
        _text: &str,
        html: &str,
    ) -> Result<String> {
        self.tracked(async {
            let message = outgoing_from(identity, draft, html);
            let sent = self
                .inner
                .send(&self.keys, &message)
                .await
                .map_err(map_err)?;

            Ok(sent.id)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INBOX: &str = include_str!("../tests/fixtures/proton/messages_inbox.json");

    /// Compile-time check that `Session` can live in shared state.
    fn assert_send_sync<T: Send + Sync>() {}

    /// Builds a label from JSON.
    fn label(id: &str, name: &str, kind: i64) -> Label {
        serde_json::from_value(serde_json::json!({"ID": id, "Name": name, "Type": kind})).unwrap()
    }

    /// Builds message metadata from JSON.
    fn meta(value: serde_json::Value) -> MessageMetadata {
        serde_json::from_value(value).unwrap()
    }

    /// Builds a draft with the given recipients.
    fn draft(cc: &[&str], reply: Option<&str>) -> Draft {
        let addr = |e: &str, name: Option<&str>| Address {
            name: name.map(str::to_owned),
            email: e.to_owned(),
        };

        Draft {
            to: vec![addr("bob@x", Some("Bob")), addr("eve@x", None)],
            cc: cc.iter().map(|e| addr(e, None)).collect(),
            subject: "Hi".into(),
            reply_to_email_id: reply.map(str::to_owned),
            ..Draft::default()
        }
    }

    #[test]
    fn header_ids_unfold_and_strip() {
        let header =
            "Subject: x\r\nreferences: <a@x>\r\n <b@x>\r\n\t<c@x>\r\nMessage-ID: <m@x>\r\n";

        assert_eq!(header_ids(header, "References"), ["a@x", "b@x", "c@x"]);
        assert_eq!(header_ids(header, "Message-Id"), ["m@x"]);
        assert!(header_ids(header, "In-Reply-To").is_empty());
        assert_eq!(header_values(header, "Subject"), ["x"]);
    }

    #[test]
    fn body_from_keeps_reply_tos() {
        let message = |reply_tos: Option<serde_json::Value>| -> Message {
            let mut v = serde_json::json!({"ID": "m1"});

            if let Some(r) = reply_tos {
                v["ReplyTos"] = r;
            }

            serde_json::from_value(v).unwrap()
        };
        let reply_to = |m: &Message| body_from("m1", m, None, None).reply_to;
        let some = message(Some(serde_json::json!([
            {"Name": "R", "Address": "ra@passmail.com"},
            {"Address": "r2@x.io"},
        ])));

        assert_eq!(
            reply_to(&some),
            [
                Address {
                    name: Some("R".into()),
                    email: "ra@passmail.com".into()
                },
                Address {
                    name: None,
                    email: "r2@x.io".into()
                },
            ]
        );
        assert!(reply_to(&message(Some(serde_json::Value::Null))).is_empty());
        assert!(reply_to(&message(Some(serde_json::json!([])))).is_empty());
        assert!(reply_to(&message(None)).is_empty());
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(i64::MAX), EPOCH);
    }

    #[test]
    fn mailbox_from_label_hides_system_views_and_fills_built_ins() {
        assert!(mailbox_from_label("a", &label("99", "Future", 4), 0).is_none());
        assert!(mailbox_from_label("a", &label("5", "All Mail", 4), 0).is_none());

        let custom = mailbox_from_label("a", &label("99zz", "Mine", 1), 3).unwrap();

        assert_eq!(custom.role, None);
        assert_eq!(custom.sort_order, 103);

        let all = mailboxes_from_labels("a", &[label("0", "Posteingang", 4)]);

        assert_eq!(all.len(), 6);
        assert_eq!(all[0].name, "Posteingang");
        assert!(all.iter().any(|m| m.id == "3" && m.name == "Trash"));
    }

    #[test]
    fn summary_mapping() {
        let page: serde_json::Value = serde_json::from_str(INBOX).unwrap();

        for entry in page["Messages"].as_array().unwrap() {
            let s = summary_from("a", &meta(entry.clone()));

            assert_eq!(s.mailbox_ids, ["0"]);
            assert!(!s.keywords.contains(&"$seen".to_owned()));
        }

        let s = summary_from(
            "a",
            &meta(serde_json::json!({
                "ID": "m1", "ConversationID": "c1", "LabelIDs": ["0", "10"], "Unread": 0,
                "IsReplied": 1, "Time": 1_700_000_000,
            })),
        );

        assert_eq!(s.keywords, ["$seen", "$flagged", "$answered"]);
        assert_eq!(s.received_at, "2023-11-14T22:13:20Z");
        assert_eq!(s.mailbox_ids, ["0"]);
        assert_eq!(s.thread_id, "c1");

        let s = summary_from(
            "a",
            &meta(serde_json::json!({
                "ID": "m2", "LabelIDs": ["0", "99", "customid", "16"], "Unread": 1,
            })),
        );

        assert_eq!(s.mailbox_ids, ["0", "customid"]);
        assert_eq!(s.thread_id, "m2");
        assert!(s.keywords.is_empty());
    }

    #[test]
    fn outgoing_from_maps_recipients_and_parent() {
        let identity = Identity {
            id: "a1".into(),
            name: "Me".into(),
            email: "me@x".into(),
        };
        let reply = outgoing_from(&identity, &draft(&[], Some("m1")), "<p>x</p>");

        assert_eq!(reply.parent.as_ref().unwrap().action, ParentAction::Reply);
        assert_eq!(reply.to[0].name, "Bob");
        assert_eq!(reply.to[1].name, "");
        assert_eq!(reply.from.address, "me@x");
        assert_eq!(reply.body_type, BodyType::Html);

        let all = outgoing_from(&identity, &draft(&["c@x"], Some("m1")), "");

        assert_eq!(all.parent.as_ref().unwrap().action, ParentAction::ReplyAll);
        assert!(
            outgoing_from(&identity, &draft(&[], None), "")
                .parent
                .is_none()
        );
    }

    #[test]
    fn login_err_names_the_step() {
        assert!(matches!(
            login_err(LoginStep::Password, PmError::WrongPassword),
            Error::Auth
        ));

        let text = |step, e| match login_err(step, e) {
            Error::Proton(t) => t,
            other => panic!("expected Error::Proton, got {other}"),
        };

        assert!(text(LoginStep::Password, PmError::SessionExpired).contains("401"));
        assert!(text(LoginStep::TwoFactor, PmError::WrongPassword).contains("two-factor"));
        assert!(text(LoginStep::Keys, PmError::SessionExpired).contains("reading the keys"));
        assert!(
            text(LoginStep::Addresses, PmError::SessionExpired).contains("listing the addresses")
        );
        assert!(text(LoginStep::Keys, PmError::NoUsableKey).starts_with("key unlock step: "));
    }

    #[test]
    fn map_err_classifies() {
        assert!(matches!(map_err(PmError::SessionExpired), Error::Auth));
        assert!(matches!(map_err(PmError::WrongPassword), Error::Auth));
        assert!(matches!(
            map_err(PmError::Api {
                status: 403,
                code: 9101,
                message: "secret-text".into(),
            }),
            Error::Auth
        ));

        let api = map_err(PmError::Api {
            status: 429,
            code: 0,
            message: "secret-text".into(),
        });

        assert!(matches!(&api, Error::Proton(t) if t.contains("429") && !t.contains("secret")));

        let blocked = map_err(PmError::Blocked {
            message: "secret-text".into(),
        });

        assert!(matches!(&blocked, Error::Proton(t) if !t.contains("secret")));

        let unknown = map_err(PmError::SendOutcomeUnknown {
            draft_id: "d-9".into(),
        });

        assert!(matches!(&unknown, Error::Other(t) if t.contains("d-9")));
    }

    #[test]
    fn session_is_send_and_sync() {
        assert_send_sync::<Session>();
    }
}
