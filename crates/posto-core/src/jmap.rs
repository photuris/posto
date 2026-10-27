//! Minimal JMAP client: session discovery, method calls and blob transfer.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Response, StatusCode};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::compose::Draft;
use crate::error::{Error, Result};
use crate::provider::{Change, Delta, DeltaPage, Identity, MaskedEmail, Updated};
use crate::store::{Address, Attachment, EmailBody, EmailSummary, Mailbox, Store};
use crate::sync::get_state;
use crate::unsubscribe;

/// The `List-Unsubscribe` header, as `Email/get` names it. The Raw form:
/// RFC 8621 forbids `asText` for RFC 2369 headers and Fastmail rejects the
/// whole `Email/get` with `invalidArguments`.
const UNSUBSCRIBE: &str = "header:List-Unsubscribe";

/// The `List-Unsubscribe-Post` header (RFC 8058), as `Email/get` names it.
const UNSUBSCRIBE_POST: &str = "header:List-Unsubscribe-Post:asText";

/// Capabilities sent in every request's `using`.
const USING: [&str; 3] = [
    "urn:ietf:params:jmap:core",
    "urn:ietf:params:jmap:mail",
    "urn:ietf:params:jmap:submission",
];

/// Capability of Fastmail's masked email methods; sent in `using` only for
/// the one call that needs it, and only when the session advertises it.
const MASKED: &str = "https://www.fastmail.com/dev/maskedemail";

/// Capability key whose primary account is the mail account.
const MAIL: &str = "urn:ietf:params:jmap:mail";

/// Everything except the RFC 3986 unreserved characters is escaped.
const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Email properties kept in the store.
const EMAIL_PROPS: [&str; 12] = [
    "id",
    "threadId",
    "mailboxIds",
    "keywords",
    "receivedAt",
    "from",
    "to",
    "cc",
    "subject",
    "preview",
    "size",
    "hasAttachment",
];

/// Largest page for `Email/query` and chunk for `Email/get`.
const PAGE: usize = 100;

/// Times a paged query restarts after the server's query state moved.
const MAX_RESTARTS: usize = 3;

/// One `[name, args, call_id]` triple of a JMAP request or response.
type Reply = (String, Value, String);

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
    /// The session's top-level `capabilities`, keyed by capability URN.
    #[serde(default)]
    pub capabilities: Map<String, Value>,
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
        self.call_using(&USING, calls).await
    }

    /// [`Client::call`] with an explicit `using` list.
    async fn call_using(
        &self,
        using: &[&str],
        calls: Vec<(String, Value, String)>,
    ) -> Result<Vec<(String, Value, String)>> {
        let body = json!({ "using": using, "methodCalls": calls });
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
        self.call_one_using(&USING, name, args).await
    }

    /// [`Client::call_one`] with an explicit `using` list.
    async fn call_one_using(&self, using: &[&str], name: &str, args: Value) -> Result<Value> {
        let responses = self
            .call_using(using, vec![(name.into(), args, "0".into())])
            .await?;
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

    /// Lists the account's masked email addresses, newest activity first.
    ///
    /// Returns an empty list without a request when the session does not
    /// advertise the masked email capability: an unknown capability in `using`
    /// would fail the whole request on such a server.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Jmap`] when the server answers with an `error` or the
    /// list is malformed, plus everything [`Client::call`] does.
    pub async fn masked_emails(&self) -> Result<Vec<MaskedEmail>> {
        if !self.session.capabilities.contains_key(MASKED) {
            return Ok(Vec::new());
        }

        let using = [USING.as_slice(), &[MASKED]].concat();
        let r = self
            .call_one_using(
                &using,
                "MaskedEmail/get",
                json!({ "accountId": self.session.account_id, "ids": null }),
            )
            .await?;

        masked_from(&r["list"])
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

/// Maps the `list` of a `MaskedEmail/get` response to the masked emails worth
/// keeping: `enabled` and `disabled` ones, newest `lastMessageAt` first (none
/// last), then by lower-cased address.
fn masked_from(list: &Value) -> Result<Vec<MaskedEmail>> {
    // Static message: the server's values never go into the error.
    let malformed = || Error::Jmap("malformed masked emails".into());
    let mut kept = Vec::new();

    for v in list.as_array().ok_or_else(malformed)? {
        let text = |key: &str| v[key].as_str().map(String::from);
        let (Some(id), Some(email), Some(state)) = (text("id"), text("email"), text("state"))
        else {
            return Err(malformed());
        };

        if state != "enabled" && state != "disabled" {
            continue;
        }

        kept.push(MaskedEmail {
            id,
            email,
            description: text("description").unwrap_or_default(),
            for_domain: text("forDomain").unwrap_or_default(),
            state,
            last_message_at: text("lastMessageAt"),
        });
    }

    // `None` sorts before `Some`, so the descending order puts it last.
    kept.sort_by(|a, b| {
        b.last_message_at
            .cmp(&a.last_message_at)
            .then_with(|| a.email.to_lowercase().cmp(&b.email.to_lowercase()))
    });

    Ok(kept)
}

/// Ids from a JSON string array; anything else yields an empty list.
fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Keys of a JSON object whose value is `true`.
fn true_keys(v: &Value) -> Vec<String> {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, v)| v.as_bool() == Some(true))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Reads a required string property.
fn str_of(v: &Value, key: &str) -> Result<String> {
    v[key]
        .as_str()
        .map(String::from)
        .ok_or_else(|| Error::Jmap(format!("response missing {key}")))
}

fn addresses(v: &Value) -> Vec<Address> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(Address {
                        name: x["name"].as_str().map(String::from),
                        email: x["email"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn mailbox_from(account: &str, v: &Value) -> Result<Mailbox> {
    Ok(Mailbox {
        account_id: account.to_string(),
        id: str_of(v, "id")?,
        name: v["name"].as_str().unwrap_or_default().to_string(),
        role: v["role"].as_str().map(String::from),
        parent_id: v["parentId"].as_str().map(String::from),
        sort_order: v["sortOrder"].as_i64().unwrap_or(0),
        total: v["totalEmails"].as_i64().unwrap_or(0),
        unread: v["unreadEmails"].as_i64().unwrap_or(0),
    })
}

/// Collapses runs of whitespace (including newlines) into single spaces, trims the ends,
/// and drops invisible format characters (zero-width spaces/joiners, soft hyphen, BOM,
/// combining grapheme joiner) that newsletters use to pad preheader text.
fn squash_ws(s: &str) -> String {
    let invisible = |c: char| {
        matches!(
            c,
            '\u{00AD}' | '\u{034F}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}'
        )
    };
    s.replace(invisible, "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn email_from(account: &str, v: &Value) -> Result<EmailSummary> {
    Ok(EmailSummary {
        account_id: account.to_string(),
        id: str_of(v, "id")?,
        thread_id: v["threadId"].as_str().unwrap_or_default().to_string(),
        received_at: v["receivedAt"].as_str().unwrap_or_default().to_string(),
        from: addresses(&v["from"]),
        to: addresses(&v["to"]),
        cc: addresses(&v["cc"]),
        subject: v["subject"].as_str().unwrap_or_default().to_string(),
        preview: squash_ws(v["preview"].as_str().unwrap_or_default()),
        size: v["size"].as_i64().unwrap_or(0),
        has_attachment: v["hasAttachment"].as_bool().unwrap_or(false),
        mailbox_ids: true_keys(&v["mailboxIds"]),
        keywords: true_keys(&v["keywords"]),
    })
}

fn attachment(v: &Value) -> Attachment {
    Attachment {
        blob_id: v["blobId"].as_str().unwrap_or_default().to_string(),
        name: v["name"].as_str().unwrap_or("attachment").to_string(),
        mime: v["type"].as_str().unwrap_or_default().to_string(),
        size: v["size"].as_i64().unwrap_or(0),
    }
}

/// Concatenates the values of the `mime` parts listed under `key`, if any.
fn joined(email: &Value, key: &str, mime: &str) -> Option<String> {
    let parts: Vec<&Value> = email[key]
        .as_array()?
        .iter()
        .filter(|p| p["type"].as_str() == Some(mime))
        .collect();

    if parts.is_empty() {
        return None;
    }

    Some(
        parts
            .iter()
            .filter_map(|p| email["bodyValues"][p["partId"].as_str()?]["value"].as_str())
            .collect(),
    )
}

/// Finds the first response named `name` with call id `id`, with its index.
fn find<'a>(rs: &'a [Reply], name: &str, id: &str) -> Option<(usize, &'a Value)> {
    rs.iter()
        .enumerate()
        .find(|(_, (n, _, i))| n == name && i == id)
        .map(|(idx, (_, args, _))| (idx, args))
}

/// Fetches mailboxes by id; `None` fetches all of them.
async fn get_mailboxes(
    client: &Client,
    account: &str,
    ids: Option<&[String]>,
) -> Result<(Vec<Mailbox>, String)> {
    let r = client
        .call_one(
            "Mailbox/get",
            json!({ "accountId": client.session().account_id, "ids": ids }),
        )
        .await?;
    let list = r["list"].as_array().map(Vec::as_slice).unwrap_or_default();

    Ok((
        list.iter()
            .map(|v| mailbox_from(account, v))
            .collect::<Result<_>>()?,
        str_of(&r, "state")?,
    ))
}

/// Replaces the mailbox table of `account` with the server's full list.
async fn replace_mailboxes(store: &Store, account: &str, client: &Client) -> Result<()> {
    let (boxes, state) = get_mailboxes(client, account, None).await?;
    let stale: Vec<String> = store
        .mailboxes(account)?
        .into_iter()
        .map(|m| m.id)
        .filter(|id| !boxes.iter().any(|m| m.id == *id))
        .collect();

    store.delete_mailboxes(account, &stale)?;
    store.upsert_mailboxes(&boxes)?;
    store.set_account_state(account, "mailbox_state", &state)
}

/// Fetches summaries for `ids` in chunks, in request order.
async fn fetch_summaries(
    client: &Client,
    account: &str,
    ids: &[String],
) -> Result<Vec<EmailSummary>> {
    let mut emails = Vec::with_capacity(ids.len());

    for chunk in ids.chunks(PAGE) {
        let r = client
            .call_one(
                "Email/get",
                json!({
                    "accountId": client.session().account_id,
                    "ids": chunk,
                    "properties": EMAIL_PROPS,
                }),
            )
            .await?;
        let list = r["list"].as_array().map(Vec::as_slice).unwrap_or_default();

        for v in list {
            emails.push(email_from(account, v)?);
        }
    }

    Ok(emails)
}

/// Collects up to `limit` newest email ids, restarting if the query changes.
async fn query_ids(client: &Client, limit: usize) -> Result<Vec<String>> {
    let mut restarts = 0;

    'restart: loop {
        let mut ids: Vec<String> = Vec::new();
        let mut first_state: Option<String> = None;

        while ids.len() < limit {
            let r = client
                .call_one(
                    "Email/query",
                    json!({
                        "accountId": client.session().account_id,
                        "sort": [{ "property": "receivedAt", "isAscending": false }],
                        "calculateTotal": false,
                        "position": ids.len(),
                        "limit": PAGE.min(limit - ids.len()),
                    }),
                )
                .await?;
            let query_state = str_of(&r, "queryState")?;

            match &first_state {
                None => first_state = Some(query_state),
                Some(s) if *s != query_state && restarts < MAX_RESTARTS => {
                    restarts += 1;
                    continue 'restart;
                }
                // ponytail: after the restart budget, keep paging this pass even
                // though the snapshot moved; a later delta repairs any drift
                Some(_) => {}
            }

            let page = strings(&r["ids"]);

            // A server may clamp the page size, so only an empty page ends paging.
            if page.is_empty() {
                break;
            }

            ids.extend(page);
        }

        ids.truncate(limit);

        return Ok(ids);
    }
}

/// Step (a) of a send: the id of the email created in Drafts.
fn created_id(client: &Client, rs: &[Reply]) -> Result<String> {
    let Some((_, r)) = find(rs, "Email/set", "0") else {
        return Err(Error::Jmap(match find(rs, "error", "0") {
            Some((_, e)) => client.describe(e),
            None => "no create response".into(),
        }));
    };

    if let Some(e) = r["notCreated"].get("d") {
        return Err(Error::Jmap(format!("create: {}", client.describe(e))));
    }

    r["created"]["d"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Jmap("create: no id returned".into()))
}

/// Step (b) of a send: the submission was accepted.
fn check_submission(client: &Client, rs: &[Reply]) -> Result<()> {
    let Some((_, r)) = find(rs, "EmailSubmission/set", "1") else {
        return Err(Error::Jmap(match find(rs, "error", "1") {
            Some((_, e)) => client.redact(e["type"].as_str().unwrap_or("unknown")),
            None => "no submission response".into(),
        }));
    };

    match r["notCreated"].get("s") {
        Some(e) => Err(Error::Jmap(format!("submit: {}", client.describe(e)))),
        None => Ok(()),
    }
}

/// Step (c) of a send: the implicit `Email/set` after the submission moved the
/// email. The mail is already sent, so every failure here is [`Error::Other`].
fn check_moved_to_sent(client: &Client, rs: &[Reply], id: &str) -> Result<()> {
    let fail = |detail: String| {
        Err(Error::Other(format!(
            "sent as {id}, but moving to Sent failed: {detail}"
        )))
    };
    let submission = find(rs, "EmailSubmission/set", "1").map_or(0, |(idx, _)| idx);
    let after = |name: &str| {
        rs.iter()
            .skip(submission + 1)
            .find(|(n, _, i)| n == name && i == "1")
            .map(|(_, args, _)| args)
    };

    if let Some(r) = after("Email/set") {
        return match r["notUpdated"].get(id) {
            Some(e) => fail(client.describe(e)),
            None => Ok(()),
        };
    }

    match after("error") {
        Some(e) => fail(client.describe(e)),
        None => fail("no update response".into()),
    }
}

/// The provider operations behind [`crate::provider::Connection`].
impl Client {
    /// Mirrors the server's mailboxes into the store: `Mailbox/changes` from
    /// the `mailbox_state` cursor, or a full replace when there is none or the
    /// server cannot calculate the changes.
    pub(crate) async fn sync_mailboxes(&self, store: &Store, account: &str) -> Result<()> {
        let Some(mut since) = get_state(store, account, "mailbox_state")? else {
            return replace_mailboxes(store, account, self).await;
        };

        loop {
            let r = match self
                .call_one(
                    "Mailbox/changes",
                    json!({ "accountId": self.session().account_id, "sinceState": since }),
                )
                .await
            {
                Err(Error::Jmap(m)) if m.starts_with("cannotCalculateChanges") => {
                    return replace_mailboxes(store, account, self).await;
                }
                other => other?,
            };
            let changed: Vec<String> = [&r["created"], &r["updated"]]
                .into_iter()
                .flat_map(strings)
                .collect();

            if !changed.is_empty() {
                store.upsert_mailboxes(&get_mailboxes(self, account, Some(&changed)).await?.0)?;
            }

            store.delete_mailboxes(account, &strings(&r["destroyed"]))?;

            since = str_of(&r, "newState")?;
            store.set_account_state(account, "mailbox_state", &since)?;

            if r["hasMoreChanges"].as_bool() != Some(true) {
                return Ok(());
            }
        }
    }

    /// The `limit` newest emails and the email state captured before paging, so
    /// changes made during paging show up in the next delta.
    pub(crate) async fn initial(
        &self,
        account: &str,
        limit: usize,
    ) -> Result<(Vec<EmailSummary>, String)> {
        let s0 = self
            .call_one(
                "Email/get",
                json!({ "accountId": self.session().account_id, "ids": [] }),
            )
            .await?;
        let s0 = str_of(&s0, "state")?;
        let ids = query_ids(self, limit).await?;

        Ok((fetch_summaries(self, account, &ids).await?, s0))
    }

    /// One `Email/changes` page after `since`; [`Delta::Resync`] when the
    /// server can no longer calculate changes from it.
    pub(crate) async fn delta(&self, account: &str, since: &str) -> Result<Delta> {
        let r = match self
            .call_one(
                "Email/changes",
                json!({
                    "accountId": self.session().account_id,
                    "sinceState": since,
                    "maxChanges": 500,
                }),
            )
            .await
        {
            Err(Error::Jmap(m)) if m.starts_with("cannotCalculateChanges") => {
                return Ok(Delta::Resync);
            }
            other => other?,
        };
        let changed: Vec<String> = [&r["created"], &r["updated"]]
            .into_iter()
            .flat_map(strings)
            .collect();

        Ok(Delta::Page(DeltaPage {
            emails: fetch_summaries(self, account, &changed).await?,
            destroyed: strings(&r["destroyed"]),
            state: str_of(&r, "newState")?,
            more: r["hasMoreChanges"].as_bool() == Some(true),
            mailboxes_changed: false,
        }))
    }

    /// The body of `email_id`; `None` when the server does not know the email.
    pub(crate) async fn body(&self, email_id: &str) -> Result<Option<EmailBody>> {
        let r = self
            .call_one(
                "Email/get",
                json!({
                    "accountId": self.session().account_id,
                    "ids": [email_id],
                    "properties": [
                        "textBody", "htmlBody", "bodyValues", "attachments",
                        "inReplyTo", "references", "messageId", "replyTo",
                        UNSUBSCRIBE, UNSUBSCRIBE_POST,
                    ],
                    "fetchHTMLBodyValues": true,
                    "fetchTextBodyValues": true,
                    "maxBodyValueBytes": 2_000_000,
                }),
            )
            .await?;
        let Some(email) = r["list"].get(0) else {
            return Ok(None);
        };

        Ok(Some(EmailBody {
            email_id: email_id.to_string(),
            html: joined(email, "htmlBody", "text/html"),
            text: joined(email, "textBody", "text/plain"),
            attachments: email["attachments"]
                .as_array()
                .map(|a| a.iter().map(attachment).collect())
                .unwrap_or_default(),
            in_reply_to: strings(&email["inReplyTo"]),
            references: strings(&email["references"]),
            message_id: strings(&email["messageId"]),
            reply_to: addresses(&email["replyTo"]),
            unsubscribe: email[UNSUBSCRIBE]
                .as_str()
                .and_then(|h| unsubscribe::parse(h, email[UNSUBSCRIBE_POST].as_str())),
        }))
    }

    /// Applies `change` to `ids` with one `Email/set`; `refused` holds the
    /// keys of `notUpdated`.
    pub(crate) async fn update(&self, ids: &[String], change: &Change) -> Result<Updated> {
        let patch = match change {
            Change::Mailbox(id) => json!({ "mailboxIds": { id: true } }),
            Change::Patch { add, remove } => {
                // Removals first so an id in both lists ends up added.
                let mut patch = Map::new();

                for id in remove {
                    patch.insert(format!("mailboxIds/{id}"), Value::Null);
                }

                for id in add {
                    patch.insert(format!("mailboxIds/{id}"), Value::Bool(true));
                }

                Value::Object(patch)
            }
            Change::Keyword(k, on) => json!({ format!("keywords/{k}"): on.then_some(true) }),
        };
        let update: Map<String, Value> = ids.iter().map(|id| (id.clone(), patch.clone())).collect();
        let r = self
            .call_one(
                "Email/set",
                json!({ "accountId": self.session().account_id, "update": update }),
            )
            .await?;

        Ok(Updated {
            refused: r["notUpdated"]
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default(),
        })
    }

    /// Destroys `ids` with `Email/set`; returns those the server did not refuse.
    pub(crate) async fn destroy(&self, ids: &[String]) -> Result<Vec<String>> {
        let r = self
            .call_one(
                "Email/set",
                json!({ "accountId": self.session().account_id, "destroy": ids }),
            )
            .await?;
        let mut gone = ids.to_vec();

        if let Some(refused) = r["notDestroyed"].as_object() {
            gone.retain(|id| !refused.contains_key(id));
        }

        Ok(gone)
    }

    /// Whether the email has a non-empty `List-Unsubscribe` header; a failed
    /// fetch counts as no.
    pub(crate) async fn has_unsubscribe(&self, id: &str) -> bool {
        self.call_one(
            "Email/get",
            json!({
                "accountId": self.session().account_id,
                "ids": [id],
                "properties": [UNSUBSCRIBE],
            }),
        )
        .await
        .ok()
        .and_then(|r| {
            r["list"][0][UNSUBSCRIBE]
                .as_str()
                .map(|h| !h.trim().is_empty())
        })
        .unwrap_or(false)
    }

    /// Creates the email in Drafts, submits it, and moves it to Sent, in one
    /// request. Returns the new email id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when the Drafts or Sent mailbox is unknown,
    /// and [`Error::Jmap`] when the server refuses to create or submit the
    /// email. Once the mail is submitted, a failed move to Sent is reported as
    /// [`Error::Other`]; callers must not resend then.
    pub(crate) async fn send(
        &self,
        store: &Store,
        account: &str,
        identity: &Identity,
        draft: &Draft,
        text: &str,
        html: &str,
    ) -> Result<String> {
        let drafts = store
            .mailbox_by_role(account, "drafts")?
            .ok_or_else(|| Error::NotFound("drafts mailbox".into()))?;
        let sent = store
            .mailbox_by_role(account, "sent")?
            .ok_or_else(|| Error::NotFound("sent mailbox".into()))?;
        let server_account = &self.session().account_id;

        let mut email = json!({
            "mailboxIds": { &drafts.id: true },
            "keywords": { "$seen": true, "$draft": true },
            "from": [{ "name": identity.name, "email": identity.email }],
            "to": draft.to,
            "subject": draft.subject,
            "bodyStructure": {
                "type": "multipart/alternative",
                "subParts": [
                    { "partId": "t", "type": "text/plain" },
                    { "partId": "h", "type": "text/html" },
                ],
            },
            "bodyValues": { "t": { "value": text }, "h": { "value": html } },
        });

        if !draft.cc.is_empty() {
            email["cc"] = json!(draft.cc);
        }

        if !draft.bcc.is_empty() {
            email["bcc"] = json!(draft.bcc);
        }

        if let Some(id) = &draft.in_reply_to {
            email["inReplyTo"] = json!([id]);
        }

        if !draft.references.is_empty() {
            email["references"] = json!(draft.references);
        }

        let responses = self
            .call(vec![
                (
                    "Email/set".into(),
                    json!({ "accountId": server_account, "create": { "d": email } }),
                    "0".into(),
                ),
                (
                    "EmailSubmission/set".into(),
                    json!({
                        "accountId": server_account,
                        "create": { "s": { "identityId": identity.id, "emailId": "#d" } },
                        "onSuccessUpdateEmail": { "#s": {
                            format!("mailboxIds/{}", drafts.id): null,
                            format!("mailboxIds/{}", sent.id): true,
                            "keywords/$draft": null,
                        } },
                    }),
                    "1".into(),
                ),
            ])
            .await?;

        let id = created_id(self, &responses)?;

        check_submission(self, &responses)?;
        check_moved_to_sent(self, &responses, &id)?;

        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_whitespace_is_squashed() {
        let v = json!({"id": "e1", "preview": "COMING UP      \n\t soon   "});
        assert_eq!(email_from("a", &v).unwrap().preview, "COMING UP soon");
        let v = json!({"id": "e2", "preview": "\u{034f}   \u{200b}\u{feff}🚨 Joshua, polls\u{00ad} close"});
        assert_eq!(
            email_from("a", &v).unwrap().preview,
            "🚨 Joshua, polls close"
        );
    }

    #[test]
    fn masked_emails_keep_enabled_and_disabled_newest_first() {
        let day = |d: u8| format!("2026-10-0{d}T00:00:00Z");
        let list = json!([
            { "id": "1", "email": "p@x.io", "state": "pending", "lastMessageAt": day(9) },
            { "id": "2", "email": "x@x.io", "state": "deleted", "lastMessageAt": day(9) },
            { "id": "3", "email": "B@x.io", "state": "enabled", "lastMessageAt": day(2) },
            { "id": "4", "email": "a@x.io", "state": "enabled", "lastMessageAt": day(2) },
            { "id": "5", "email": "D@x.io", "state": "enabled",
              "description": null, "forDomain": null },
            { "id": "6", "email": "c@x.io", "state": "enabled", "lastMessageAt": null },
            { "id": "7", "email": "e@x.io", "state": "disabled", "lastMessageAt": day(3) },
        ]);

        let got = masked_from(&list).unwrap();
        let emails: Vec<&str> = got.iter().map(|m| m.email.as_str()).collect();

        assert_eq!(emails, ["e@x.io", "a@x.io", "B@x.io", "c@x.io", "D@x.io"]);

        let d = &got[4];

        assert_eq!(d.description, "");
        assert_eq!(d.for_domain, "");
        assert_eq!(d.last_message_at, None);

        let bad = json!([
            { "id": "1", "state": "enabled", "description": "SENTINEL-7f3a" },
        ]);
        let err = masked_from(&bad).unwrap_err();

        assert!(
            matches!(&err, Error::Jmap(m) if m == "malformed masked emails"),
            "{err}"
        );
        assert!(!err.to_string().contains("SENTINEL-7f3a"), "{err}");
        assert!(!format!("{err:?}").contains("SENTINEL-7f3a"), "{err:?}");
    }
}
