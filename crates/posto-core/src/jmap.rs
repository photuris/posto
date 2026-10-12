//! Minimal JMAP client: session discovery, method calls and blob transfer.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::store::Mailbox;

/// Capabilities sent in every request's `using`.
const USING: [&str; 3] = [
    "urn:ietf:params:jmap:core",
    "urn:ietf:params:jmap:mail",
    "urn:ietf:params:jmap:submission",
];

/// Capability key whose primary account is the mail account.
const MAIL: &str = "urn:ietf:params:jmap:mail";

/// Everything except the RFC 3986 unreserved characters is escaped.
const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The parts of the JMAP session resource Posto needs.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// Endpoint for method calls.
    pub api_url: String,
    /// Blob download URL template.
    pub download_url: String,
    /// Blob upload URL template.
    pub upload_url: String,
    /// Primary mail account id.
    #[serde(default)]
    pub account_id: String,
    /// Login name.
    #[serde(default)]
    pub username: String,
}

/// A sending identity.
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

/// An authenticated JMAP connection.
pub struct Client {
    /// Shared HTTP client.
    http: reqwest::Client,
    /// Bearer token; never logged or included in errors.
    token: String,
    /// Session fetched by [`Client::connect`].
    session: Session,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// Replaces every occurrence of `token` in `text` with `<redacted>`.
fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }

    text.replace(token, "<redacted>")
}

/// Maps a transport failure to [`Error::Http`] without the request URL or
/// the token.
fn transport(token: &str) -> impl Fn(reqwest::Error) -> Error + '_ {
    move |e| Error::Http(scrub(&e.without_url().to_string(), token))
}

/// Maps non-2xx statuses to errors; 401 and 403 mean the token is rejected.
fn check(resp: Response) -> Result<Response> {
    match resp.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::Auth),
        s if !s.is_success() => Err(Error::Http(format!("status {}", s.as_u16()))),
        _ => Ok(resp),
    }
}

/// Percent-encodes one URL template value.
fn enc(s: &str) -> String {
    utf8_percent_encode(s, ENCODE).to_string()
}

impl Client {
    /// Fetches the session resource and returns a ready client.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] on 401/403, [`Error::Http`] on other failures
    /// and [`Error::Jmap`] when the session has no mail account.
    pub async fn connect(session_url: &str, token: &str) -> Result<Client> {
        let http = reqwest::Client::new();
        let resp = http
            .get(session_url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(transport(token))?;
        let raw: Value = check(resp)?.json().await.map_err(transport(token))?;
        // Static message: serde errors can quote the response.
        let mut session: Session = serde_json::from_value(raw.clone())
            .map_err(|_| Error::Jmap("malformed session".into()))?;

        session.account_id = raw["primaryAccounts"][MAIL]
            .as_str()
            .ok_or_else(|| Error::Jmap("no mail account".into()))?
            .to_string();

        Ok(Client {
            http,
            token: token.to_string(),
            session,
        })
    }

    /// The session this client was built from.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// `text` with the bearer token replaced by `<redacted>`. Run server-supplied
    /// text through this before it goes into an error.
    pub fn redact(&self, text: &str) -> String {
        scrub(text, &self.token)
    }

    /// Sends one JMAP request of `[name, args, call_id]` triples.
    ///
    /// A method-level `error` response is returned as-is; so are
    /// `notCreated`/`notUpdated` maps. Callers inspect them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] on 401/403 and [`Error::Http`] on other
    /// transport or status failures.
    pub async fn call(
        &self,
        calls: Vec<(String, Value, String)>,
    ) -> Result<Vec<(String, Value, String)>> {
        let body = json!({ "using": USING, "methodCalls": calls });
        let resp = self
            .http
            .post(&self.session.api_url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(transport(&self.token))?;
        let mut raw: Value = check(resp)?.json().await.map_err(transport(&self.token))?;

        // Static message: serde errors can quote the response.
        serde_json::from_value(raw["methodResponses"].take())
            .map_err(|_| Error::Jmap("malformed response".into()))
    }

    /// Sends a single method call and returns its response arguments.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Jmap`] with `type: description` when the server
    /// answers with an `error` response, plus everything [`Client::call`] does.
    pub async fn call_one(&self, name: &str, args: Value) -> Result<Value> {
        let responses = self.call(vec![(name.into(), args, "0".into())]).await?;
        let (rname, rargs, _) = responses
            .into_iter()
            .next()
            .ok_or_else(|| Error::Jmap("empty response".into()))?;

        if rname == "error" {
            return Err(Error::Jmap(self.describe(&rargs)));
        }

        Ok(rargs)
    }

    /// `type: description` of a JMAP error object (a method error or a
    /// `SetError`), with the token redacted.
    pub fn describe(&self, error: &Value) -> String {
        let kind = error["type"].as_str().unwrap_or("unknown");

        self.redact(&match error["description"].as_str() {
            Some(d) => format!("{kind}: {d}"),
            None => kind.to_string(),
        })
    }

    /// Downloads a blob through the session's download template.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] or [`Error::Http`] on failure.
    pub async fn download_blob(&self, blob_id: &str, name: &str, mime: &str) -> Result<Vec<u8>> {
        let url = self
            .session
            .download_url
            .replace("{accountId}", &enc(&self.session.account_id))
            .replace("{blobId}", &enc(blob_id))
            .replace("{name}", &enc(name))
            .replace("{type}", &enc(mime));
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(transport(&self.token))?;

        Ok(check(resp)?
            .bytes()
            .await
            .map_err(transport(&self.token))?
            .to_vec())
    }

    /// Uploads a blob and returns its blob id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`], [`Error::Http`], or [`Error::Jmap`] when the
    /// response lacks a `blobId`.
    pub async fn upload_blob(&self, mime: &str, data: Vec<u8>) -> Result<String> {
        let url = self
            .session
            .upload_url
            .replace("{accountId}", &enc(&self.session.account_id));
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header("Content-Type", mime)
            .body(data)
            .send()
            .await
            .map_err(transport(&self.token))?;
        let raw: Value = check(resp)?.json().await.map_err(transport(&self.token))?;

        raw["blobId"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Jmap("upload response without blobId".into()))
    }

    /// Lists the account's sending identities.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails or the list is malformed.
    pub async fn identities(&self) -> Result<Vec<Identity>> {
        let r = self
            .call_one(
                "Identity/get",
                json!({ "accountId": self.session.account_id, "ids": null }),
            )
            .await?;

        serde_json::from_value(r["list"].clone())
            .map_err(|_| Error::Jmap("malformed identities".into()))
    }

    /// Creates a top-level mailbox called `name`.
    ///
    /// The returned [`Mailbox`] is rebuilt from the request and the server's
    /// `created` reply (which may hold only the id); its `account_id` is
    /// empty and the caller sets the local slug before storing it. A create
    /// is never retried: it may have been committed before a failure.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Jmap`] when the server refuses the create or does not
    /// return an id, plus everything [`Client::call_one`] does.
    pub async fn create_mailbox(&self, name: &str) -> Result<Mailbox> {
        let r = self
            .call_one(
                "Mailbox/set",
                json!({
                    "accountId": self.session.account_id,
                    "create": { "c": { "name": name, "parentId": null } },
                }),
            )
            .await?;

        if let Some(e) = r["notCreated"].get("c") {
            return Err(Error::Jmap(format!(
                "create mailbox {name:?}: {}",
                self.describe(e)
            )));
        }

        let created = &r["created"]["c"];
        let id = created["id"]
            .as_str()
            .ok_or_else(|| Error::Jmap(format!("create mailbox {name:?}: no id returned")))?;

        Ok(Mailbox {
            account_id: String::new(),
            id: id.to_string(),
            name: created["name"].as_str().unwrap_or(name).to_string(),
            role: created["role"].as_str().map(String::from),
            parent_id: created["parentId"].as_str().map(String::from),
            sort_order: created["sortOrder"].as_i64().unwrap_or(0),
            total: created["totalEmails"].as_i64().unwrap_or(0),
            unread: created["unreadEmails"].as_i64().unwrap_or(0),
        })
    }
}
