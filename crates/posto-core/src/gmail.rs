//! Gmail back end: Google's OAuth desktop sign-in and the Gmail REST API v1.
//!
//! [`Session`] implements the read side of the provider contract: identities,
//! label creation, mailbox sync, initial sync and delta sync. Gmail has labels
//! only, so the six built-in mailboxes are mapped from system labels and a
//! Posto-side `ARCHIVE` (see [`visible`]); every other user label is a Posto
//! label. Nothing here puts a token, the client secret or a response body into
//! an error or a log line.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::alphabet::URL_SAFE;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{
    GeneralPurpose, GeneralPurposeConfig, URL_SAFE_NO_PAD as B64,
};
use futures_util::stream::{self, StreamExt};
use mail_parser::MimeHeaders;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rand::RngCore;
use reqwest::{Method, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::compose::Draft;
use crate::error::{Error, Result};
use crate::provider::{Change, Delta, DeltaPage, Identity, Updated};
use crate::store::{Address, Attachment, EmailBody, EmailSummary, Mailbox, Store};

/// The OAuth scope Posto asks for: full access, because permanent delete needs
/// it.
pub const SCOPE: &str = "https://mail.google.com/";

/// Google's OAuth authorisation endpoint.
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Google's OAuth token endpoint.
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The Gmail API root for the signed-in user.
const API_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// Posto's six built-in mailboxes: Gmail-side id, role, fixed name, sort
/// order.
const BUILT_IN: [(&str, &str, &str, i64); 6] = [
    ("INBOX", "inbox", "Inbox", 0),
    ("DRAFT", "drafts", "Drafts", 1),
    ("SENT", "sent", "Sent", 2),
    ("ARCHIVE", "archive", "Archive", 3),
    ("SPAM", "junk", "Spam", 4),
    ("TRASH", "trash", "Trash", 5),
];

/// Posto's own id for the archive: a message that is in no folder-like
/// mailbox. Gmail has no such label and never sends it.
pub(crate) const ARCHIVE: &str = "ARCHIVE";

/// The headers the list view needs, requested with `format=metadata`.
const METADATA_HEADERS: [&str; 5] = ["From", "To", "Cc", "Subject", "Content-Type"];

/// The most ids Gmail takes in one `batchModify` or `batchDelete`.
const BATCH_MAX: usize = 1000;

/// Base64url that accepts padded and unpadded input, for `format=raw`.
const B64_ANY: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The folders a message sits in at most one of; a move adds one and removes
/// the other two. `ARCHIVE` is Posto's own and never sent to Gmail.
const FOLDERS: [&str; 3] = ["INBOX", "SPAM", "TRASH"];

/// How many metadata requests run at once.
const IN_FLIGHT: usize = 4;

/// The most ids Gmail returns per list page.
const PAGE_MAX: usize = 500;

/// How long one request may take before it counts as a transport error.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a client may stay silent on the loopback listener.
const CALLBACK_IDLE: Duration = Duration::from_secs(5);

/// The most request bytes the loopback listener reads.
const CALLBACK_MAX: usize = 8 * 1024;

/// The error for a reply that is not the JSON the API documents.
fn malformed() -> Error {
    Error::Http("gmail: malformed response".into())
}

/// Maps a transport failure; the text never holds the URL.
fn transport(e: reqwest::Error) -> Error {
    Error::Http(format!("gmail: {}", e.without_url()))
}

/// The non-empty string at `key`, or the malformed-response error.
fn required(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(malformed)
}

/// The non-empty string at `key`, if any.
fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str().filter(|s| !s.is_empty())
}

// ── Client and endpoints ────────────────────────────────────────────────────

/// The OAuth desktop client Posto signs in with.
#[derive(Clone)]
pub struct GoogleClient {
    /// The client id.
    pub client_id: String,
    /// The client secret; never printed.
    pub client_secret: String,
}

impl fmt::Debug for GoogleClient {
    /// Prints the client id; the secret stays hidden.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl GoogleClient {
    /// Reads Google's client JSON (`{"installed": {"client_id", ..}}`).
    ///
    /// # Errors
    ///
    /// [`Error::NotConfigured`] when the file is missing, [`Error::Io`] when it
    /// cannot be read and [`Error::Config`] when it is not a desktop client
    /// file. The error never holds file text.
    pub fn load(path: &Path) -> Result<GoogleClient> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotConfigured(format!(
                    "Google OAuth client: save the desktop client JSON as {}",
                    path.display()
                )));
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let invalid =
            || Error::Config("google-client.json is not a desktop OAuth client file".into());
        let file: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
        let installed = file
            .get("installed")
            .filter(|v| v.is_object())
            .ok_or_else(invalid)?;

        match (
            non_empty(installed, "client_id"),
            non_empty(installed, "client_secret"),
        ) {
            (Some(client_id), Some(client_secret)) => Ok(GoogleClient {
                client_id: client_id.to_owned(),
                client_secret: client_secret.to_owned(),
            }),
            _ => Err(invalid()),
        }
    }
}

/// Where requests go. Production by default; tests point it at wiremock.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// The authorisation page the browser opens.
    pub auth: String,
    /// The token endpoint.
    pub token: String,
    /// The API root, without a trailing slash.
    pub api: String,
    /// The first back-off of a retried request; it doubles each time.
    pub retry_base: Duration,
}

impl Default for Endpoints {
    /// Google's production endpoints and a one-second back-off.
    fn default() -> Self {
        Endpoints {
            auth: AUTH_URL.to_owned(),
            token: TOKEN_URL.to_owned(),
            api: API_URL.to_owned(),
            retry_base: Duration::from_secs(1),
        }
    }
}

/// What a sign-in hands back for the caller to store.
pub struct Login {
    /// The long-lived refresh token.
    pub refresh_token: String,
    /// The signed-in account's own address (the profile address).
    pub email: String,
}

impl fmt::Debug for Login {
    /// Prints the address; the token stays hidden.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Login")
            .field("refresh_token", &"<redacted>")
            .field("email", &self.email)
            .finish()
    }
}

// ── Sign-in ─────────────────────────────────────────────────────────────────

/// The PKCE challenge for `verifier`: base64url of its SHA-256, no padding.
fn challenge(verifier: &str) -> String {
    B64.encode(Sha256::digest(verifier.as_bytes()))
}

/// `n` random bytes as base64url without padding.
fn random_token(n: usize) -> String {
    let mut bytes = vec![0u8; n];

    rand::rngs::OsRng.fill_bytes(&mut bytes);

    B64.encode(bytes)
}

/// Signs a Gmail account in: PKCE and a loopback redirect, at most five
/// minutes in total. `open` receives the address the user must visit.
///
/// `email` is the address configured for the account; empty accepts any
/// Google account.
///
/// # Errors
///
/// [`Error::Other`] when the sign-in times out, is refused or returns no
/// refresh token, [`Error::Auth`] on `invalid_grant`, [`Error::Config`] when
/// the Google account cannot send as `email`, and any transport error.
pub async fn login(client: &GoogleClient, email: &str, open: impl FnOnce(&str)) -> Result<Login> {
    login_with(
        &Endpoints::default(),
        client,
        email,
        Duration::from_secs(300),
        open,
    )
    .await
}

/// [`login`] with the endpoints and the overall timeout supplied.
///
/// # Errors
///
/// As for [`login`].
#[doc(hidden)]
pub async fn login_with(
    endpoints: &Endpoints,
    client: &GoogleClient,
    email: &str,
    timeout: Duration,
    open: impl FnOnce(&str),
) -> Result<Login> {
    let verifier = random_token(32);
    let state = random_token(16);
    let email = email.trim();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let code_challenge = challenge(&verifier);
    let mut params = vec![
        ("client_id", client.client_id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("code_challenge", code_challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
    ];

    if !email.is_empty() {
        params.push(("login_hint", email));
    }

    let url = Url::parse_with_params(&endpoints.auth, &params)
        .map_err(|_| Error::Config("invalid Google authorisation address".into()))?;

    tokio::time::timeout(timeout, async {
        open(url.as_str());

        let code = wait_for_code(&listener, &state).await?;

        finish_login(endpoints, client, email, &redirect, &code, &verifier).await
    })
    .await
    .unwrap_or_else(|_| Err(Error::Other("Google sign-in timed out".into())))
}

/// Exchanges the authorisation `code`, reads the profile and checks that the
/// account may send as `email`.
async fn finish_login(
    endpoints: &Endpoints,
    client: &GoogleClient,
    email: &str,
    redirect: &str,
    code: &str,
    verifier: &str,
) -> Result<Login> {
    let http = http_client()?;
    let reply = token_request(
        &http,
        endpoints,
        &[
            ("code", code),
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("redirect_uri", redirect),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ],
    )
    .await?;
    let refresh_token = non_empty(&reply, "refresh_token").ok_or_else(|| {
        Error::Other(
            "Google returned no refresh token; remove Posto's access at \
             https://myaccount.google.com/permissions and sign in again"
                .into(),
        )
    })?;
    let session = Session {
        http,
        endpoints: endpoints.clone(),
        client: client.clone(),
        refresh_token: refresh_token.to_owned(),
        access: Mutex::new(Some(access_from(&reply)?)),
        email: String::new(),
    };
    let address = required(&session.profile().await?, "emailAddress")?;

    if !email.is_empty() && !address.eq_ignore_ascii_case(email) {
        let identities = session.identities().await?;

        if !identities
            .iter()
            .any(|i| i.email.eq_ignore_ascii_case(email))
        {
            return Err(Error::Config(format!(
                "signed in to a Google account that cannot send as {email}"
            )));
        }
    }

    Ok(Login {
        refresh_token: refresh_token.to_owned(),
        email: address,
    })
}

/// What the loopback listener made of one connection.
enum Callback {
    /// The redirect carried our state and a code.
    Code(String),
    /// The redirect carried our state and an error: the user said no.
    Refused,
    /// Not the redirect; keep waiting.
    Ignored,
}

/// Serves the loopback listener until Google's redirect arrives.
///
/// Connections are handled one at a time.
// ponytail: a browser that opens a silent connection first delays the real
// redirect by up to CALLBACK_IDLE; handle connections concurrently if that
// ever shows.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let (stream, _) = listener.accept().await?;

        match handle_connection(stream, state).await {
            Callback::Code(code) => return Ok(code),
            Callback::Refused => return Err(Error::Other("Google sign-in was refused".into())),
            Callback::Ignored => {}
        }
    }
}

/// Reads one request head from `stream` and answers it.
async fn handle_connection(mut stream: TcpStream, state: &str) -> Callback {
    let Some((method, target)) = read_request_line(&mut stream).await else {
        return Callback::Ignored;
    };
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .ok()
        .filter(|u| method == "GET" && target.starts_with('/') && u.path() == "/");
    let params: HashMap<String, String> = url
        .map(|u| u.query_pairs().into_owned().collect())
        .unwrap_or_default();

    if params.get("state").is_some_and(|s| s == state) {
        if let Some(code) = params.get("code").filter(|c| !c.is_empty()) {
            respond(
                &mut stream,
                "200 OK",
                "<p>Signed in. You can close this tab and return to Posto.</p>",
            )
            .await;

            return Callback::Code(code.clone());
        }

        if params.contains_key("error") {
            respond(
                &mut stream,
                "200 OK",
                "<p>Sign-in cancelled. You can close this tab.</p>",
            )
            .await;

            return Callback::Refused;
        }
    }

    respond(&mut stream, "404 Not Found", "").await;

    Callback::Ignored
}

/// Reads up to the end of the request head and returns the method and the
/// request target of its first line; `None` when the client stays silent for
/// too long or sends nothing usable.
async fn read_request_line(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = vec![0u8; CALLBACK_MAX];
    let mut len = 0;

    while len < CALLBACK_MAX && !buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
        match tokio::time::timeout(CALLBACK_IDLE, stream.read(&mut buf[len..])).await {
            Err(_) => return None,
            Ok(Ok(0) | Err(_)) => break,
            Ok(Ok(n)) => len += n,
        }
    }

    let head = String::from_utf8_lossy(&buf[..len]);
    let mut words = head.lines().next()?.split_whitespace();

    Some((words.next()?.to_owned(), words.next()?.to_owned()))
}

/// Writes a complete HTTP/1.1 answer and closes the connection.
async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let answer = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    // The browser may already have gone; there is nobody to tell.
    let _ = stream.write_all(answer.as_bytes()).await;
    let _ = stream.shutdown().await;
}

// ── Tokens ──────────────────────────────────────────────────────────────────

/// Builds the HTTP client every request goes through.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(transport)
}

/// Posts `form` to the token endpoint and returns the JSON reply.
///
/// `invalid_grant` is [`Error::Auth`]; any other failure names the status
/// number only.
async fn token_request(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    form: &[(&str, &str)],
) -> Result<Value> {
    let reply = http
        .post(&endpoints.token)
        .form(form)
        .send()
        .await
        .map_err(transport)?;
    let status = reply.status();
    let bytes = reply.bytes().await.map_err(transport)?;
    let body: Option<Value> = serde_json::from_slice(&bytes).ok();

    if !status.is_success() {
        if body.as_ref().and_then(|b| b["error"].as_str()) == Some("invalid_grant") {
            return Err(Error::Auth);
        }

        return Err(Error::Http(format!(
            "google token endpoint {}",
            status.as_u16()
        )));
    }

    body.ok_or_else(|| Error::Http("google token endpoint: malformed response".into()))
}

/// The access token of a token reply and the instant it expires.
fn access_from(reply: &Value) -> Result<(String, Instant)> {
    let token = non_empty(reply, "access_token")
        .ok_or_else(|| Error::Http("google token endpoint: malformed response".into()))?;
    let ttl = reply["expires_in"].as_u64().unwrap_or(3600);

    Ok((token.to_owned(), Instant::now() + Duration::from_secs(ttl)))
}

/// True when a 403 body names a rate limit, which is worth retrying.
fn is_rate_limited(body: &[u8]) -> bool {
    let Ok(body) = serde_json::from_slice::<Value>(body) else {
        return false;
    };

    body["error"]["errors"].as_array().is_some_and(|errors| {
        errors.iter().any(|e| {
            matches!(
                e["reason"].as_str(),
                Some("rateLimitExceeded" | "userRateLimitExceeded")
            )
        })
    })
}

// ── Session ─────────────────────────────────────────────────────────────────

/// Connects from a stored refresh token.
///
/// # Errors
///
/// [`Error::Auth`] when Google rejects the refresh token, [`Error::Http`] on
/// transport or protocol failures.
pub async fn connect(client: GoogleClient, refresh_token: &str) -> Result<Session> {
    connect_with(Endpoints::default(), client, refresh_token).await
}

/// [`connect`] with the endpoints supplied.
///
/// # Errors
///
/// As for [`connect`].
#[doc(hidden)]
pub async fn connect_with(
    endpoints: Endpoints,
    client: GoogleClient,
    refresh_token: &str,
) -> Result<Session> {
    let mut session = Session {
        http: http_client()?,
        endpoints,
        client,
        refresh_token: refresh_token.to_owned(),
        access: Mutex::new(None),
        email: String::new(),
    };

    session.email = required(&session.profile().await?, "emailAddress")?;

    Ok(session)
}

/// One signed-in Gmail account.
pub struct Session {
    /// The shared HTTP client.
    http: reqwest::Client,
    /// Where requests go.
    endpoints: Endpoints,
    /// The OAuth client, for refreshing.
    client: GoogleClient,
    /// The long-lived refresh token.
    refresh_token: String,
    /// The cached access token and its expiry; held across a refresh so
    /// concurrent callers refresh once.
    access: Mutex<Option<(String, Instant)>>,
    /// The profile address.
    email: String,
}

impl fmt::Debug for Session {
    /// Prints the type name only: every field is a credential or close to it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("gmail::Session")
    }
}

impl Session {
    /// The server's own id for the account: the profile address.
    pub fn server_account_id(&self) -> &str {
        &self.email
    }

    /// The provider name written into the account's sync state.
    pub fn provider_name(&self) -> &'static str {
        "gmail"
    }

    /// True: Gmail reports no counts, the store computes them.
    pub fn local_counts(&self) -> bool {
        true
    }

    /// The account's sending identities: send-as entries that are primary or
    /// accepted.
    ///
    /// # Errors
    ///
    /// Any request error.
    pub async fn identities(&self) -> Result<Vec<Identity>> {
        let reply = self
            .call(Method::GET, "/settings/sendAs", &[], None, true)
            .await?;

        Ok(reply["sendAs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["isPrimary"] == true || e["verificationStatus"] == "accepted")
            .filter_map(|e| {
                let email = non_empty(e, "sendAsEmail")?;

                Some(Identity {
                    id: email.to_owned(),
                    name: e["displayName"].as_str().unwrap_or_default().to_owned(),
                    email: email.to_owned(),
                })
            })
            .collect())
    }

    /// Creates a visible user label called `name`. The mailbox has an empty
    /// `account_id`; the caller sets the local slug.
    ///
    /// # Errors
    ///
    /// Any request error, or a reply without an id.
    pub async fn create_mailbox(&self, name: &str) -> Result<Mailbox> {
        let body = json!({
            "name": name,
            "labelListVisibility": "labelShow",
            "messageListVisibility": "show",
        });
        let reply = self
            .call(Method::POST, "/labels", &[], Some(body), true)
            .await?;

        Ok(Mailbox {
            account_id: String::new(),
            id: required(&reply, "id")?,
            name: non_empty(&reply, "name").unwrap_or(name).to_owned(),
            role: None,
            parent_id: None,
            sort_order: 100,
            total: 0,
            unread: 0,
        })
    }

    /// Refreshes the account's mailboxes from Gmail's labels, keeping stored
    /// counts and dropping mailboxes the server no longer has.
    ///
    /// # Errors
    ///
    /// Any request error or a store failure.
    pub async fn sync_mailboxes(&self, store: &Store, account: &str) -> Result<()> {
        let reply = self.call(Method::GET, "/labels", &[], None, true).await?;
        let labels = reply["labels"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut mapped = mailboxes_from_labels(account, labels);
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
    }

    /// The `limit` newest messages and the history cursor, captured before
    /// listing so changes made while paging show up in the next delta.
    ///
    /// # Errors
    ///
    /// Any request error.
    pub async fn initial(
        &self,
        account: &str,
        limit: usize,
    ) -> Result<(Vec<EmailSummary>, String)> {
        let cursor = required(&self.profile().await?, "historyId")?;

        if limit == 0 {
            return Ok((Vec::new(), cursor));
        }

        let mut ids: Vec<String> = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut query = vec![
                ("includeSpamTrash", "true".to_owned()),
                ("q", "-in:chats".to_owned()),
                ("maxResults", limit.min(PAGE_MAX).to_string()),
            ];

            if let Some(token) = &page_token {
                query.push(("pageToken", token.clone()));
            }

            let page = self
                .call(Method::GET, "/messages", &query, None, true)
                .await?;

            ids.extend(
                page["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| non_empty(m, "id"))
                    .map(str::to_owned),
            );

            page_token = non_empty(&page, "nextPageToken").map(str::to_owned);

            if ids.len() >= limit || page_token.is_none() {
                break;
            }
        }

        ids.truncate(limit);

        let summaries = self
            .summaries(account, &ids)
            .await?
            .into_iter()
            .filter_map(|(_, summary)| summary)
            .collect();

        Ok((summaries, cursor))
    }

    /// What changed after the history cursor `since`, from every history page.
    ///
    /// # Errors
    ///
    /// Any request error other than a 404, which is [`Delta::Resync`].
    pub async fn delta(&self, account: &str, since: &str) -> Result<Delta> {
        let mut statuses = Statuses::default();
        let mut page_token: Option<String> = None;

        let state = loop {
            let mut query = vec![
                ("startHistoryId", since.to_owned()),
                ("maxResults", PAGE_MAX.to_string()),
            ];

            if let Some(token) = &page_token {
                query.push(("pageToken", token.clone()));
            }

            let page = match self.call(Method::GET, "/history", &query, None, true).await {
                Err(Error::NotFound(_)) => return Ok(Delta::Resync),
                other => other?,
            };

            for record in page["history"].as_array().into_iter().flatten() {
                statuses.record(record);
            }

            page_token = non_empty(&page, "nextPageToken").map(str::to_owned);

            if page_token.is_none() {
                break required(&page, "historyId")?;
            }
        };
        let changed: Vec<String> = statuses.ids(true);
        let mut destroyed = statuses.ids(false);
        let mut emails = Vec::new();

        for (id, summary) in self.summaries(account, &changed).await? {
            match summary {
                Some(summary) => emails.push(summary),
                None => destroyed.push(id),
            }
        }

        Ok(Delta::Page(DeltaPage {
            emails,
            destroyed,
            state,
            more: false,
            mailboxes_changed: false,
        }))
    }

    /// `GET /profile`.
    async fn profile(&self) -> Result<Value> {
        self.call(Method::GET, "/profile", &[], None, true).await
    }

    /// The metadata of message `id`: the headers the list view needs.
    async fn metadata(&self, id: &str) -> Result<Value> {
        let mut query = vec![("format", "metadata".to_owned())];

        query.extend(METADATA_HEADERS.map(|h| ("metadataHeaders", h.to_owned())));

        let path = format!("/messages/{}", utf8_percent_encode(id, NON_ALPHANUMERIC));

        self.call(Method::GET, &path, &query, None, true).await
    }

    /// Fetches the metadata of `ids`, four at a time, in order. An id the
    /// server no longer knows, or one that is a chat, comes back with `None`.
    async fn summaries(
        &self,
        account: &str,
        ids: &[String],
    ) -> Result<Vec<(String, Option<EmailSummary>)>> {
        stream::iter(ids)
            .map(|id| async move {
                match self.metadata(id).await {
                    Ok(message) => Ok((id.clone(), summary_from(account, &message))),
                    Err(Error::NotFound(_)) => Ok((id.clone(), None)),
                    Err(e) => Err(e),
                }
            })
            .buffered(IN_FLIGHT)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect()
    }

    /// A valid access token: the cached one, or a fresh one from the refresh
    /// token when none is cached or it expires within a minute.
    async fn access_token(&self) -> Result<String> {
        let mut cached = self.access.lock().await;

        if let Some((token, expiry)) = cached.as_ref()
            && *expiry > Instant::now() + Duration::from_secs(60)
        {
            return Ok(token.clone());
        }

        let reply = token_request(
            &self.http,
            &self.endpoints,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &self.refresh_token),
                ("client_id", &self.client.client_id),
                ("client_secret", &self.client.client_secret),
            ],
        )
        .await?;
        let (token, expiry) = access_from(&reply)?;

        *cached = Some((token.clone(), expiry));

        Ok(token)
    }

    /// Forgets the cached token if it is still `used`; another caller may
    /// already have refreshed.
    async fn forget(&self, used: &str) {
        let mut cached = self.access.lock().await;

        if cached.as_ref().is_some_and(|(token, _)| token == used) {
            *cached = None;
        }
    }

    /// Sends one API request and returns its JSON reply (`Null` for an empty
    /// one). `path` is relative to the API root.
    ///
    /// A 401 is replayed once with a fresh token. With `retry`, rate limits
    /// and server errors are retried up to three times with doubling
    /// back-off. Without it (a send) any outcome that may have reached Gmail
    /// is an [`Error::Other`] telling the caller not to resend.
    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        retry: bool,
    ) -> Result<Value> {
        let url = format!("{}{path}", self.endpoints.api);
        let uncertain = || {
            Error::Other(
                "gmail: the message may have been sent; check Sent before resending".into(),
            )
        };
        let mut replayed = false;
        let mut attempt = 0u32;

        loop {
            let token = self.access_token().await?;
            let mut request = self.http.request(method.clone(), &url).bearer_auth(&token);

            if !query.is_empty() {
                request = request.query(query);
            }

            if let Some(body) = &body {
                request = request.json(body);
            }

            let reply = match request.send().await {
                Ok(reply) => reply,
                Err(e) if retry || e.is_connect() => return Err(transport(e)),
                Err(_) => return Err(uncertain()),
            };
            let status = reply.status();

            if status.is_success() {
                let bytes = match reply.bytes().await {
                    Ok(bytes) => bytes,
                    Err(e) if retry => return Err(transport(e)),
                    Err(_) => return Err(uncertain()),
                };

                if bytes.is_empty() {
                    return Ok(Value::Null);
                }

                return serde_json::from_slice(&bytes)
                    .map_err(|_| if retry { malformed() } else { uncertain() });
            }

            // The status alone decides what happens next; only a 403 needs its
            // body, and a body that cannot be read is a plain 403.
            match status.as_u16() {
                401 if !replayed => {
                    replayed = true;

                    self.forget(&token).await;
                }
                401 => return Err(Error::Auth),
                429 | 500 | 502 | 503 | 504 => {
                    self.back_off(retry, &mut attempt, status.as_u16()).await?;
                }
                403 if is_rate_limited(&reply.bytes().await.unwrap_or_default()) => {
                    self.back_off(retry, &mut attempt, 403).await?;
                }
                403 => return Err(Error::Auth),
                404 => return Err(Error::NotFound(format!("gmail {path}"))),
                other => return Err(Error::Http(format!("gmail {other}"))),
            }
        }
    }

    /// The raw RFC 822 bytes of message `id`.
    async fn raw(&self, id: &str) -> Result<Vec<u8>> {
        let path = format!("/messages/{}", utf8_percent_encode(id, NON_ALPHANUMERIC));
        let query = [("format", "raw".to_owned())];
        let message = self.call(Method::GET, &path, &query, None, true).await?;

        decode_raw(message["raw"].as_str().ok_or_else(malformed)?)
    }

    /// The body of message `id`, parsed from its raw form; `None` when the
    /// server no longer has it.
    ///
    /// # Errors
    ///
    /// Any request error other than a 404, or an unreadable message.
    pub async fn body(&self, email_id: &str) -> Result<Option<EmailBody>> {
        let bytes = match self.raw(email_id).await {
            Err(Error::NotFound(_)) => return Ok(None),
            other => other?,
        };

        Ok(Some(body_from(email_id, &parse(&bytes)?)))
    }

    /// The bytes of attachment `att` of message `email_id`; its `blob_id` is
    /// the index among the message's attachments.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the message or the attachment is gone, any
    /// other request error, or an unreadable message.
    pub async fn download(&self, email_id: &str, att: &Attachment) -> Result<Vec<u8>> {
        let missing = || Error::NotFound(format!("attachment {}", att.blob_id));
        let bytes = match self.raw(email_id).await {
            Err(Error::NotFound(_)) => return Err(missing()),
            other => other?,
        };
        let index = att.blob_id.parse::<usize>().map_err(|_| missing())?;
        let message = parse(&bytes)?;
        let part = message.attachments().nth(index).ok_or_else(missing)?;

        Ok(part.contents().to_vec())
    }

    /// Applies `change` to `ids` with `batchModify`, 1000 ids per request.
    ///
    /// Gmail refuses a batch as a whole, so nothing is reported as refused.
    /// An error aborts; earlier chunks stay applied and the next delta
    /// reconciles.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a change to `SENT` or `DRAFT` (before any
    /// request), or any request error.
    pub async fn update(&self, ids: &[String], change: &Change) -> Result<Updated> {
        let Some((add, remove)) = modify_lists(change)? else {
            return Ok(Updated::default());
        };

        for chunk in ids.chunks(BATCH_MAX) {
            let body = json!({"ids": chunk, "addLabelIds": add, "removeLabelIds": remove});

            self.call(Method::POST, "/messages/batchModify", &[], Some(body), true)
                .await?;
        }

        Ok(Updated::default())
    }

    /// Deletes `ids` for good with `batchDelete`, 1000 ids per request, and
    /// returns them all.
    ///
    /// # Errors
    ///
    /// Any request error; earlier chunks stay deleted.
    pub async fn destroy(&self, ids: &[String]) -> Result<Vec<String>> {
        for chunk in ids.chunks(BATCH_MAX) {
            self.call(
                Method::POST,
                "/messages/batchDelete",
                &[],
                Some(json!({"ids": chunk})),
                true,
            )
            .await?;
        }

        Ok(ids.to_vec())
    }

    /// True when message `email_id` carries a non-blank `List-Unsubscribe`
    /// header; any error counts as no.
    pub async fn has_unsubscribe(&self, email_id: &str) -> bool {
        let path = format!(
            "/messages/{}",
            utf8_percent_encode(email_id, NON_ALPHANUMERIC)
        );
        let query = [
            ("format", "metadata".to_owned()),
            ("metadataHeaders", "List-Unsubscribe".to_owned()),
        ];
        let Ok(message) = self.call(Method::GET, &path, &query, None, true).await else {
            return false;
        };

        message["payload"]["headers"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|h| {
                h["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case("List-Unsubscribe"))
                    && h["value"].as_str().is_some_and(|v| !v.trim().is_empty())
            })
    }

    /// Sends the draft with `messages.send` and returns the new message id.
    ///
    /// A reply goes into the original's thread when the original is a
    /// message of this account in the store. The request is never retried:
    /// an outcome that may have reached Gmail is [`Error::Other`] telling the
    /// caller not to resend.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a draft without recipients, a store failure
    /// while looking up the thread, [`Error::Auth`] when Google rejects the
    /// token twice, and any request error.
    pub async fn send(
        &self,
        store: &Store,
        account: &str,
        identity: &Identity,
        draft: &Draft,
        text: &str,
        html: &str,
    ) -> Result<String> {
        let raw = B64.encode(outgoing(identity, draft, text, html)?);
        let mut body = json!({"raw": raw});
        let same_account = draft
            .reply_to_account_id
            .as_deref()
            .is_none_or(|a| a == account);

        if let (true, Some(original)) = (same_account, &draft.reply_to_email_id)
            && let Some(email) = store.email(account, original)?
        {
            body["threadId"] = json!(email.thread_id);
        }

        let reply = self
            .call(Method::POST, "/messages/send", &[], Some(body), false)
            .await?;

        non_empty(&reply, "id").map(str::to_owned).ok_or_else(|| {
            Error::Other("gmail: sent, but the reply has no id; do not resend".into())
        })
    }

    /// Sleeps before the next try of a transient failure, or gives up with
    /// the status when `retry` is off or three retries are spent.
    async fn back_off(&self, retry: bool, attempt: &mut u32, status: u16) -> Result<()> {
        if !retry || *attempt >= 3 {
            return Err(Error::Http(format!("gmail {status}")));
        }

        tokio::time::sleep(self.endpoints.retry_base * 2u32.pow(*attempt)).await;

        *attempt += 1;

        Ok(())
    }
}

// ── Mapping ─────────────────────────────────────────────────────────────────

/// Message ids in first-seen order, each with its latest status.
#[derive(Default)]
struct Statuses {
    /// `(id, changed)`: `changed` is false when the last event deleted it.
    order: Vec<(String, bool)>,
    /// Position of each id in `order`.
    index: HashMap<String, usize>,
}

impl Statuses {
    /// Folds one `history[]` record in: adds and label changes first, then
    /// deletions. Entries without an id are ignored.
    fn record(&mut self, record: &Value) {
        for (key, changed) in [
            ("messagesAdded", true),
            ("labelsAdded", true),
            ("labelsRemoved", true),
            ("messagesDeleted", false),
        ] {
            for entry in record[key].as_array().into_iter().flatten() {
                if let Some(id) = non_empty(&entry["message"], "id") {
                    self.note(id, changed);
                }
            }
        }
    }

    /// Sets the status of `id`, keeping the position of its first event.
    fn note(&mut self, id: &str, changed: bool) {
        match self.index.get(id) {
            Some(&at) => self.order[at].1 = changed,
            None => {
                self.index.insert(id.to_owned(), self.order.len());
                self.order.push((id.to_owned(), changed));
            }
        }
    }

    /// The ids whose final status is `changed`, in first-seen order.
    fn ids(&self, changed: bool) -> Vec<String> {
        self.order
            .iter()
            .filter(|(_, c)| *c == changed)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// The six built-in mailboxes, then every Gmail user label in server order.
///
/// Labels of other types, with an empty id, with a built-in id (`ARCHIVE`
/// included) or repeating an earlier one are skipped.
pub(crate) fn mailboxes_from_labels(account: &str, labels: &[Value]) -> Vec<Mailbox> {
    let mailbox = |id: &str, name: &str, role: Option<&str>, sort_order: i64| Mailbox {
        account_id: account.to_owned(),
        id: id.to_owned(),
        name: name.to_owned(),
        role: role.map(str::to_owned),
        parent_id: None,
        sort_order,
        total: 0,
        unread: 0,
    };
    let mut mailboxes: Vec<Mailbox> = BUILT_IN
        .iter()
        .map(|&(id, role, name, order)| mailbox(id, name, Some(role), order))
        .collect();
    let mut taken: HashSet<&str> = BUILT_IN.iter().map(|b| b.0).collect();
    let mut users = 0;

    for label in labels {
        let Some(id) = non_empty(label, "id") else {
            continue;
        };

        if label["type"] != "user" || !taken.insert(id) {
            continue;
        }

        let name = label["name"].as_str().unwrap_or_default();

        mailboxes.push(mailbox(id, name, None, 100 + users));

        users += 1;
    }

    mailboxes
}

/// Posto mailbox ids of a message from the ids it holds, order kept: Trash and
/// Spam hide Inbox, Drafts and Sent; `ARCHIVE` is present exactly when no
/// folder-like built-in is; repeats are dropped.
pub fn visible(ids: &[String]) -> Vec<String> {
    let has = |id: &str| ids.iter().any(|i| i == id);
    let in_bin = has("SPAM") || has("TRASH");
    let mut out: Vec<String> = ids
        .iter()
        .filter(|i| !(in_bin && matches!(i.as_str(), "INBOX" | "DRAFT" | "SENT")))
        .cloned()
        .collect();
    let in_folder = out
        .iter()
        .any(|i| matches!(i.as_str(), "INBOX" | "DRAFT" | "SENT" | "SPAM" | "TRASH"));

    if in_folder {
        out.retain(|i| i != ARCHIVE);
    } else if !out.iter().any(|i| i == ARCHIVE) {
        out.push(ARCHIVE.to_owned());
    }

    let mut seen = HashSet::new();

    out.retain(|i| seen.insert(i.clone()));

    out
}

/// True for the labels Posto never stores as mailboxes.
fn is_hidden(id: &str) -> bool {
    matches!(id, "UNREAD" | "STARRED" | "IMPORTANT" | "CHAT")
        || id.starts_with("CATEGORY_")
        || id.ends_with("_STAR")
}

/// The mailbox ids of a message with these Gmail label ids.
fn mailbox_ids(label_ids: &[String]) -> Vec<String> {
    let shown: Vec<String> = label_ids
        .iter()
        .filter(|i| !is_hidden(i) && *i != ARCHIVE)
        .cloned()
        .collect();

    visible(&shown)
}

/// The keywords of a message with these Gmail label ids.
fn keywords(label_ids: &[String]) -> Vec<String> {
    let has = |id: &str| label_ids.iter().any(|i| i == id);

    [
        (!has("UNREAD"), "$seen"),
        (has("STARRED"), "$flagged"),
        (has("DRAFT"), "$draft"),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, keyword)| keyword.to_owned())
    .collect()
}

/// Decodes the five entities Gmail puts in a snippet; `&amp;` goes last so
/// `&amp;lt;` becomes `&lt;`.
fn unescape_snippet(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The value of header `name` (ASCII case-insensitive) of a metadata reply.
fn header<'a>(message: &'a Value, name: &str) -> Option<&'a str> {
    message["payload"]["headers"]
        .as_array()?
        .iter()
        .find(|h| {
            h["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|h| h["value"].as_str())
}

/// Parses the address headers and the subject of a metadata reply with
/// `mail-parser`, which decodes RFC 2047 words.
fn parse_headers(message: &Value) -> Option<mail_parser::Message<'static>> {
    let mut raw = String::new();

    for name in ["From", "To", "Cc", "Subject"] {
        if let Some(value) = header(message, name) {
            // A line break in a value would start a header of its own.
            raw.push_str(&format!("{name}: {}\r\n", value.replace(['\r', '\n'], " ")));
        }
    }

    raw.push_str("\r\n");

    mail_parser::MessageParser::default()
        .parse(raw.as_bytes())
        .map(mail_parser::Message::into_owned)
}

/// The addresses of a parsed header, groups flattened; entries without an
/// email are dropped and an empty name becomes `None`.
fn addresses(parsed: Option<&mail_parser::Address<'_>>) -> Vec<Address> {
    parsed
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|a| {
            let email = a.address.as_deref().filter(|e| !e.is_empty())?;

            Some(Address {
                name: a
                    .name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned),
                email: email.to_owned(),
            })
        })
        .collect()
}

/// Maps a `format=metadata` reply to a summary; `None` when it has no id or
/// is a chat.
fn summary_from(account: &str, m: &Value) -> Option<EmailSummary> {
    let id = non_empty(m, "id")?;
    let label_ids: Vec<String> = m["labelIds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().map(str::to_owned))
        .collect();

    if label_ids.iter().any(|l| l == "CHAT") {
        return None;
    }

    let parsed = parse_headers(m);
    let secs = m["internalDate"]
        .as_str()
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(0)
        / 1000;

    Some(EmailSummary {
        account_id: account.to_owned(),
        id: id.to_owned(),
        thread_id: non_empty(m, "threadId").unwrap_or(id).to_owned(),
        received_at: crate::proton::rfc3339(secs),
        from: addresses(parsed.as_ref().and_then(|p| p.from())),
        to: addresses(parsed.as_ref().and_then(|p| p.to())),
        cc: addresses(parsed.as_ref().and_then(|p| p.cc())),
        subject: parsed
            .as_ref()
            .and_then(|p| p.subject())
            .unwrap_or_default()
            .to_owned(),
        preview: unescape_snippet(m["snippet"].as_str().unwrap_or_default()),
        size: m["sizeEstimate"].as_i64().unwrap_or(0),
        has_attachment: header(m, "Content-Type").is_some_and(|v| {
            v.trim_start()
                .get(.."multipart/mixed".len())
                .is_some_and(|p| p.eq_ignore_ascii_case("multipart/mixed"))
        }),
        mailbox_ids: mailbox_ids(&label_ids),
        keywords: keywords(&label_ids),
    })
}

// ── Bodies and outgoing mail ────────────────────────────────────────────────

/// Decodes a `raw` field: base64url, padding optional.
pub(crate) fn decode_raw(raw: &str) -> Result<Vec<u8>> {
    B64_ANY.decode(raw).map_err(|_| malformed())
}

/// Parses raw RFC 822 bytes.
fn parse(bytes: &[u8]) -> Result<mail_parser::Message<'_>> {
    mail_parser::MessageParser::default()
        .parse(bytes)
        .ok_or_else(|| Error::Http("gmail: unreadable message".into()))
}

/// The text of the first part in `ids` whose content type is `want`
/// (`text/plain` also matches a part without one).
fn first_text(message: &mail_parser::Message<'_>, ids: &[u32], want: &str) -> Option<String> {
    ids.iter()
        .filter_map(|&id| message.part(id))
        .find(|part| match part.content_type() {
            Some(ct) => {
                ct.ctype().eq_ignore_ascii_case("text")
                    && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case(want))
            }
            None => want == "plain",
        })
        .and_then(|part| part.text_contents())
        .map(str::to_owned)
}

/// Every id of a message-id header, without angle brackets.
fn id_list(value: &mail_parser::HeaderValue<'_>) -> Vec<String> {
    value
        .as_text_list()
        .into_iter()
        .flatten()
        .map(|id| id.to_string())
        .collect()
}

/// The body, attachments and threading headers of a parsed message.
/// Attachment blob ids are indexes into `message.attachments()`.
pub(crate) fn body_from(email_id: &str, message: &mail_parser::Message<'_>) -> EmailBody {
    EmailBody {
        email_id: email_id.to_owned(),
        html: first_text(message, &message.html_body, "html"),
        text: first_text(message, &message.text_body, "plain"),
        attachments: message
            .attachments()
            .enumerate()
            .map(|(i, part)| Attachment {
                blob_id: i.to_string(),
                name: part.attachment_name().unwrap_or("attachment").to_owned(),
                mime: part.content_type().map_or_else(
                    || "application/octet-stream".to_owned(),
                    |ct| match ct.subtype() {
                        Some(sub) => format!("{}/{sub}", ct.ctype()),
                        None => "application/octet-stream".to_owned(),
                    },
                ),
                size: i64::try_from(part.contents().len()).unwrap_or(i64::MAX),
            })
            .collect(),
        in_reply_to: id_list(message.in_reply_to()),
        references: id_list(message.references()),
        message_id: message
            .message_id()
            .map(str::to_owned)
            .into_iter()
            .collect(),
        reply_to: addresses(message.reply_to()),
    }
}

/// The `(addLabelIds, removeLabelIds)` of the `batchModify` that applies
/// `change`; `None` when Gmail needs no request.
///
/// A folder move adds only the last folder and removes the other two of
/// `INBOX`, `SPAM` and `TRASH`; `ARCHIVE` is never sent.
///
/// # Errors
///
/// [`Error::Unsupported`] for a change to `SENT` or `DRAFT`, which Gmail sets
/// itself.
pub(crate) fn modify_lists(change: &Change) -> Result<Option<(Vec<String>, Vec<String>)>> {
    let (add, remove): (&[String], &[String]) = match change {
        Change::Keyword(keyword, on) => {
            let (label, adds) = match *keyword {
                "$seen" => ("UNREAD", !on),
                "$flagged" => ("STARRED", *on),
                _ => return Ok(None),
            };
            let list = vec![label.to_owned()];

            return Ok(Some(if adds { (list, vec![]) } else { (vec![], list) }));
        }
        Change::Mailbox(target) => (std::slice::from_ref(target), &[]),
        Change::Patch { add, remove } => (add, remove),
    };

    if add
        .iter()
        .chain(remove)
        .any(|id| id == "SENT" || id == "DRAFT")
    {
        return Err(Error::Unsupported(
            "changing Sent or Drafts on Gmail accounts".into(),
        ));
    }

    let is_folder = |id: &str| id == ARCHIVE || FOLDERS.contains(&id);
    let folder = add.iter().rfind(|id| is_folder(id));
    let mut added: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();

    for id in add {
        if is_folder(id) && (id == ARCHIVE || Some(id) != folder) {
            continue;
        }

        if !added.contains(id) {
            added.push(id.clone());
        }
    }

    for id in remove {
        if id != ARCHIVE && !added.contains(id) && !removed.contains(id) {
            removed.push(id.clone());
        }
    }

    if let Some(folder) = folder {
        for other in FOLDERS {
            if other != folder && !removed.iter().any(|r| r == other) {
                removed.push(other.to_owned());
            }
        }
    }

    Ok((!added.is_empty() || !removed.is_empty()).then_some((added, removed)))
}

/// The RFC 822 message for a draft: `text` and `html` as alternatives.
///
/// # Errors
///
/// [`Error::Config`] when the draft has no recipient.
pub(crate) fn outgoing(
    identity: &Identity,
    draft: &Draft,
    text: &str,
    html: &str,
) -> Result<Vec<u8>> {
    use mail_builder::MessageBuilder;
    use mail_builder::headers::address::Address as Mailbox;

    if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
        return Err(Error::Config(
            "a message needs at least one recipient".into(),
        ));
    }

    let mailbox = |name: Option<&str>, email: &str| {
        Mailbox::new_address(
            name.filter(|n| !n.is_empty()).map(str::to_owned),
            email.to_owned(),
        )
    };
    let list = |people: &[Address]| -> Vec<Mailbox<'static>> {
        people
            .iter()
            .map(|a| mailbox(a.name.as_deref(), &a.email))
            .collect()
    };
    let mut message = MessageBuilder::new()
        .from(mailbox(Some(&identity.name), &identity.email))
        .subject(draft.subject.clone())
        .text_body(text.to_owned())
        .html_body(html.to_owned());

    for (set, people) in [
        (MessageBuilder::to as fn(_, _) -> _, &draft.to),
        (MessageBuilder::cc, &draft.cc),
        (MessageBuilder::bcc, &draft.bcc),
    ] {
        if !people.is_empty() {
            message = set(message, list(people));
        }
    }

    if let Some(id) = &draft.in_reply_to {
        message = message.in_reply_to(id.clone());
    }

    if !draft.references.is_empty() {
        message = message.references(draft.references.clone());
    }

    Ok(message.write_to_vec()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand for owned id lists.
    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// The `(add, remove)` label lists of a `batchModify`.
    type Lists = (Vec<String>, Vec<String>);

    /// Compile-time check that `Session` can live in shared state.
    fn assert_send_sync<T: Send + Sync>() {}

    /// A metadata reply with the given headers.
    fn message(headers: &[(&str, &str)]) -> Value {
        let headers: Vec<Value> = headers
            .iter()
            .map(|(name, value)| json!({"name": name, "value": value}))
            .collect();

        json!({
            "id": "m1",
            "threadId": "t1",
            "labelIds": ["INBOX", "UNREAD"],
            "snippet": "Tom &amp; Jerry&#39;s",
            "internalDate": "1700000000000",
            "sizeEstimate": 1234,
            "payload": {"headers": headers},
        })
    }

    #[test]
    fn mailbox_ids_follow_the_gmail_rules() {
        let cases: [(&[&str], &[&str]); 9] = [
            (&["UNREAD", "CATEGORY_UPDATES", "INBOX"], &["INBOX"]),
            (&["Label_1", "IMPORTANT"], &["Label_1", "ARCHIVE"]),
            (&["SENT", "TRASH", "Label_2"], &["TRASH", "Label_2"]),
            (&["SENT"], &["SENT"]),
            (&[], &["ARCHIVE"]),
            (&["SENT", "INBOX"], &["SENT", "INBOX"]),
            (&["DRAFT"], &["DRAFT"]),
            (&["INBOX", "SPAM"], &["SPAM"]),
            (&["Lbl-opaque", "YELLOW_STAR"], &["Lbl-opaque", "ARCHIVE"]),
        ];

        for (labels, want) in cases {
            assert_eq!(mailbox_ids(&ids(labels)), ids(want), "labels {labels:?}");
        }

        let direct: [(&[&str], &[&str]); 4] = [
            (&["ARCHIVE", "SENT"], &["SENT"]),
            (&["TRASH", "SENT", "Label_1"], &["TRASH", "Label_1"]),
            (&["Label_1", "ARCHIVE"], &["Label_1", "ARCHIVE"]),
            (&["Label_1"], &["Label_1", "ARCHIVE"]),
        ];

        for (given, want) in direct {
            assert_eq!(visible(&ids(given)), ids(want), "ids {given:?}");
        }
    }

    #[test]
    fn keywords_from_labels() {
        assert_eq!(
            keywords(&ids(&["INBOX", "UNREAD", "STARRED"])),
            ids(&["$flagged"])
        );
        assert_eq!(keywords(&ids(&["DRAFT"])), ids(&["$seen", "$draft"]));
        assert_eq!(keywords(&[]), ids(&["$seen"]));
    }

    #[test]
    fn summary_from_metadata() {
        let m = message(&[
            ("From", "=?UTF-8?B?w4lsaXNl?= <e@example.org>"),
            ("To", "a@example.org, \"B\" <b@example.org>"),
            ("Subject", "=?UTF-8?Q?Caf=C3=A9?="),
            ("Content-Type", "multipart/mixed; boundary=x"),
        ]);
        let s = summary_from("a", &m).unwrap();

        assert_eq!(s.account_id, "a");
        assert_eq!(s.id, "m1");
        assert_eq!(s.thread_id, "t1");
        assert_eq!(s.from.len(), 1);
        assert_eq!(s.from[0].name.as_deref(), Some("Élise"));
        assert_eq!(s.from[0].email, "e@example.org");
        assert_eq!(s.to.len(), 2);
        assert_eq!(s.to[0].name, None);
        assert_eq!(s.to[0].email, "a@example.org");
        assert_eq!(s.to[1].name.as_deref(), Some("B"));
        assert!(s.cc.is_empty());
        assert_eq!(s.subject, "Café");
        assert_eq!(s.received_at, "2023-11-14T22:13:20Z");
        assert_eq!(s.preview, "Tom & Jerry's");
        assert_eq!(s.size, 1234);
        assert!(s.has_attachment);
        assert_eq!(s.mailbox_ids, ids(&["INBOX"]));
        assert_eq!(s.keywords, Vec::<String>::new());

        let plain = message(&[("Content-Type", "text/plain")]);

        assert!(!summary_from("a", &plain).unwrap().has_attachment);

        let mut no_id = message(&[]);

        no_id.as_object_mut().unwrap().remove("id");

        assert_eq!(summary_from("a", &no_id), None);

        let mut empty_id = message(&[]);

        empty_id["id"] = json!("");

        assert_eq!(summary_from("a", &empty_id), None);

        let mut chat = message(&[]);

        chat["labelIds"] = json!(["CHAT", "Label_1"]);

        assert_eq!(summary_from("a", &chat), None);
    }

    #[test]
    fn unescape_snippet_order() {
        assert_eq!(unescape_snippet("&amp;lt;"), "&lt;");
        assert_eq!(unescape_snippet("&lt;b&gt; &quot;x&quot;"), "<b> \"x\"");
    }

    #[test]
    fn mailboxes_from_labels_always_has_the_built_ins() {
        let mailboxes = mailboxes_from_labels("a", &[]);
        let got: Vec<(&str, Option<&str>, &str, i64)> = mailboxes
            .iter()
            .map(|m| {
                (
                    m.id.as_str(),
                    m.role.as_deref(),
                    m.name.as_str(),
                    m.sort_order,
                )
            })
            .collect();

        assert_eq!(
            got,
            vec![
                ("INBOX", Some("inbox"), "Inbox", 0),
                ("DRAFT", Some("drafts"), "Drafts", 1),
                ("SENT", Some("sent"), "Sent", 2),
                ("ARCHIVE", Some("archive"), "Archive", 3),
                ("SPAM", Some("junk"), "Spam", 4),
                ("TRASH", Some("trash"), "Trash", 5),
            ]
        );
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn google_client_load() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();

        let write = |name: &str, text: &str| {
            let path = dir.join(name);

            std::fs::write(&path, text).unwrap();

            path
        };
        let good = write(
            "good.json",
            r#"{"installed": {"client_id": "cid", "client_secret": "csecret", "x": 1}}"#,
        );
        let client = GoogleClient::load(&good).unwrap();

        assert_eq!(client.client_id, "cid");
        assert_eq!(client.client_secret, "csecret");
        assert!(!format!("{client:?}").contains("csecret"));
        assert!(matches!(
            GoogleClient::load(&dir.join("missing.json")),
            Err(Error::NotConfigured(_))
        ));

        for text in [
            r#"{"web": {"client_id": "cid", "client_secret": "csecret"}}"#,
            r#"{"installed": {"client_id": "x"}}"#,
            r#"{"installed": {"client_id": "x", "client_secret": ""}}"#,
            "not json csecret",
        ] {
            let err = GoogleClient::load(&write("bad.json", text)).unwrap_err();

            assert!(matches!(err, Error::Config(_)), "{text}");
            assert!(!err.to_string().contains("csecret"));
        }
    }

    #[test]
    fn modify_lists_table() {
        use Change::{Keyword, Mailbox as Move, Patch};

        let patch = |add: &[&str], remove: &[&str]| Patch {
            add: ids(add),
            remove: ids(remove),
        };
        let cases: Vec<(Change, Option<Lists>)> = vec![
            (Keyword("$seen", true), Some((ids(&[]), ids(&["UNREAD"])))),
            (Keyword("$seen", false), Some((ids(&["UNREAD"]), ids(&[])))),
            (
                Keyword("$flagged", true),
                Some((ids(&["STARRED"]), ids(&[]))),
            ),
            (
                Keyword("$flagged", false),
                Some((ids(&[]), ids(&["STARRED"]))),
            ),
            (Keyword("$answered", true), None),
            (
                Move("ARCHIVE".into()),
                Some((ids(&[]), ids(&["INBOX", "SPAM", "TRASH"]))),
            ),
            (
                Move("TRASH".into()),
                Some((ids(&["TRASH"]), ids(&["INBOX", "SPAM"]))),
            ),
            (
                Move("INBOX".into()),
                Some((ids(&["INBOX"]), ids(&["SPAM", "TRASH"]))),
            ),
            (
                patch(&["Label_1"], &["INBOX"]),
                Some((ids(&["Label_1"]), ids(&["INBOX"]))),
            ),
            (
                patch(&["INBOX", "Label_1"], &[]),
                Some((ids(&["INBOX", "Label_1"]), ids(&["SPAM", "TRASH"]))),
            ),
            (
                patch(&["ARCHIVE"], &["INBOX"]),
                Some((ids(&[]), ids(&["INBOX", "SPAM", "TRASH"]))),
            ),
            (patch(&[], &["ARCHIVE"]), None),
            (
                patch(&["SPAM", "TRASH"], &[]),
                Some((ids(&["TRASH"]), ids(&["INBOX", "SPAM"]))),
            ),
            (
                patch(&["INBOX", "ARCHIVE"], &[]),
                Some((ids(&[]), ids(&["INBOX", "SPAM", "TRASH"]))),
            ),
            (
                patch(&["INBOX"], &["INBOX"]),
                Some((ids(&["INBOX"]), ids(&["SPAM", "TRASH"]))),
            ),
            (
                patch(&["Label_1", "Label_1"], &[]),
                Some((ids(&["Label_1"]), ids(&[]))),
            ),
        ];

        for (change, want) in cases {
            assert_eq!(modify_lists(&change).unwrap(), want, "{change:?}");
        }

        for change in [
            Move("SENT".into()),
            Move("DRAFT".into()),
            patch(&[], &["SENT"]),
        ] {
            assert!(
                matches!(modify_lists(&change), Err(Error::Unsupported(_))),
                "{change:?}"
            );
        }
    }

    #[test]
    fn decode_raw_accepts_both_paddings() {
        assert_eq!(decode_raw("aGk").unwrap(), b"hi");
        assert_eq!(decode_raw("aGk=").unwrap(), b"hi");
        assert_eq!(decode_raw("_-8").unwrap(), [0xff, 0xef]);
        assert!(matches!(decode_raw("a$b"), Err(Error::Http(_))));
    }

    #[test]
    fn outgoing_needs_a_recipient() {
        let identity = Identity {
            id: "me@gmail.com".into(),
            name: String::new(),
            email: "me@gmail.com".into(),
        };
        let mut draft = Draft::default();

        assert!(matches!(
            outgoing(&identity, &draft, "t", "<p>h</p>"),
            Err(Error::Config(_))
        ));

        draft.bcc.push(Address {
            name: None,
            email: "b@example.net".into(),
        });

        let bytes = outgoing(&identity, &draft, "t", "<p>h</p>").unwrap();
        let message = parse(&bytes).unwrap();
        let from = message.from().and_then(|a| a.first()).unwrap();

        assert_eq!(from.name, None);
        assert_eq!(from.address.as_deref(), Some("me@gmail.com"));
        assert!(message.to().is_none());
        assert_eq!(addresses(message.bcc()).len(), 1);
    }

    #[test]
    fn session_is_send_and_sync() {
        assert_send_sync::<Session>();
    }
}
