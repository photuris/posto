//! Unsubscribing from mailing lists (`List-Unsubscribe`, RFC 2369, and its
//! one-click POST, RFC 8058).
//!
//! The headers are read once, when a body is fetched, into
//! [`Unsubscribe`]; what to do with it is decided here for every provider.

use std::net::IpAddr;
use std::time::Duration;

use reqwest::{Url, redirect};
use serde::{Deserialize, Serialize};

use crate::compose::{self, Draft};
use crate::error::{Error, Result};
use crate::provider::{Identity, MaskedEmail};
use crate::store::EmailSummary;

/// How long the one-click POST may take.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The ways a message offers to unsubscribe its recipient.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Unsubscribe {
    /// The first `https:` link of `List-Unsubscribe`.
    pub https: Option<String>,
    /// The first `mailto:` link of `List-Unsubscribe`.
    pub mailto: Option<String>,
    /// True when `List-Unsubscribe-Post` asks for the RFC 8058 one-click POST
    /// to `https`.
    pub one_click: bool,
}

/// What unsubscribing from a message takes, best first.
#[derive(Clone, Debug, PartialEq)]
pub enum Method {
    /// POST to this URL with [`one_click`]; nothing else needed.
    OneClick(String),
    /// Send this draft (a mailto link turned into a message).
    Mail(Box<Draft>),
    /// Open this https page in the browser.
    Web(String),
}

/// Reads the raw `List-Unsubscribe` and `List-Unsubscribe-Post` header
/// values; `None` when neither an https nor a mailto link is offered.
///
/// Plain `http:` links are ignored: RFC 8058 requires https for one-click,
/// and a page over http leaks the subscriber token on the way.
pub fn parse(list_unsubscribe: &str, post: Option<&str>) -> Option<Unsubscribe> {
    let mut found = Unsubscribe::default();

    for part in list_unsubscribe.split('<').skip(1) {
        let Some((uri, _)) = part.split_once('>') else {
            continue;
        };
        // Folded headers leave whitespace inside the brackets.
        let uri: String = uri.split_whitespace().collect();
        let lower = uri.to_ascii_lowercase();

        if lower.starts_with("https://") && found.https.is_none() {
            found.https = Some(uri);
        } else if lower.starts_with("mailto:") && found.mailto.is_none() {
            found.mailto = Some(uri);
        }
    }

    if found.https.is_none() && found.mailto.is_none() {
        return None;
    }

    found.one_click = found.https.is_some()
        && post.is_some_and(|p| {
            p.split_whitespace()
                .collect::<String>()
                .eq_ignore_ascii_case("List-Unsubscribe=One-Click")
        });

    Some(found)
}

/// The best way to unsubscribe from `summary`: one-click, then mail, then
/// the web page. The mail is sent from the account the message arrived in,
/// from the address it was sent to (see [`compose::pick_identity`]); `me`,
/// `identities` and `masked` are that account's.
pub fn method(
    u: &Unsubscribe,
    summary: &EmailSummary,
    me: &str,
    identities: &[Identity],
    masked: &[MaskedEmail],
) -> Option<Method> {
    if let (true, Some(url)) = (u.one_click, &u.https) {
        return Some(Method::OneClick(url.clone()));
    }

    let draft = u.mailto.as_deref().and_then(|m| {
        let mut draft = mailto_draft(m)?;

        draft.account_id = summary.account_id.clone();
        draft.from = compose::pick_identity(identities, masked, summary, me);

        Some(Box::new(draft))
    });

    draft
        .map(Method::Mail)
        .or_else(|| u.https.clone().map(Method::Web))
}

/// Turns a `mailto:` link into a draft (To, Subject and body); `None` when
/// it names no valid recipient. A missing subject becomes `unsubscribe`.
fn mailto_draft(mailto: &str) -> Option<Draft> {
    let url = Url::parse(mailto).ok()?;
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8_lossy()
            .into_owned()
    };
    let mut to = compose::parse_addresses(&decode(url.path()));
    let mut subject = String::new();
    let mut body = String::new();

    for (key, value) in url.query_pairs() {
        match key.to_ascii_lowercase().as_str() {
            "to" => to.extend(compose::parse_addresses(&value)),
            "subject" => subject = value.into_owned(),
            "body" => body = value.into_owned(),
            _ => {}
        }
    }

    if to.is_empty() {
        return None;
    }

    if subject.trim().is_empty() {
        subject = "unsubscribe".to_owned();
    }

    Some(Draft {
        to,
        subject,
        body,
        ..Draft::default()
    })
}

/// Sends the RFC 8058 one-click POST to `url`.
///
/// The request carries only the form body: no cookies, no credentials, no
/// redirects followed (a 3xx counts as done). `url` must be https with a
/// host name; IP literals and `localhost` are refused so a message cannot
/// aim the POST at the local network.
///
/// # Errors
///
/// [`Error::Config`] for a refused URL; [`Error::Http`] with the status
/// number only (the URL holds the subscriber's token and is never echoed).
pub async fn one_click(url: &str) -> Result<()> {
    let url = checked(url)?;

    post(url).await
}

/// `url` parsed, when it is https and its host is a public name.
fn checked(url: &str) -> Result<Url> {
    let refused = || Error::Config("unsubscribe link is not a public https address".into());
    let url = Url::parse(url).map_err(|_| refused())?;
    let host = url.host_str().ok_or_else(refused)?;
    // ponytail: a name that resolves to a private address still passes; pin
    // the resolved address (reqwest `resolve`) if that ever matters.
    let local = host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok()
        || host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost");

    if url.scheme() != "https" || local {
        return Err(refused());
    }

    Ok(url)
}

/// The POST itself, without the URL check (the tests aim it at wiremock).
async fn post(url: Url) -> Result<()> {
    let failed = || Error::Http("unsubscribe request failed".into());
    let client = reqwest::Client::builder()
        .redirect(redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| failed())?;
    let response = client
        .post(url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .map_err(|_| failed())?;
    let status = response.status();

    if status.is_success() || status.is_redirection() {
        return Ok(());
    }

    Err(Error::Http(format!(
        "unsubscribe refused: HTTP {}",
        status.as_u16()
    )))
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string, header, method as http_method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn summary() -> EmailSummary {
        EmailSummary {
            account_id: "work".into(),
            id: "e1".into(),
            thread_id: "t1".into(),
            received_at: "2026-10-23T09:00:00Z".into(),
            from: Vec::new(),
            to: vec![crate::store::Address {
                name: None,
                email: "me@example.org".into(),
            }],
            cc: Vec::new(),
            subject: "Weekly deals".into(),
            preview: String::new(),
            size: 0,
            has_attachment: false,
            mailbox_ids: Vec::new(),
            keywords: Vec::new(),
        }
    }

    #[test]
    fn parse_keeps_the_first_https_and_mailto() {
        let u = parse(
            "<http://x.example/u>, <mailto:u@x.example?subject=stop>,\r\n \
             <https://x.example/u?t=1>, <https://y.example/>",
            Some("List-Unsubscribe=One-Click"),
        )
        .unwrap();

        assert_eq!(u.https.as_deref(), Some("https://x.example/u?t=1"));
        assert_eq!(u.mailto.as_deref(), Some("mailto:u@x.example?subject=stop"));
        assert!(u.one_click);
    }

    #[test]
    fn parse_needs_https_for_one_click_and_a_link_at_all() {
        let mail = parse("<mailto:u@x.example>", Some("List-Unsubscribe=One-Click")).unwrap();

        assert!(!mail.one_click);
        assert!(parse("<http://x.example/u>", None).is_none());
        assert!(parse("garbage", None).is_none());
        assert!(
            !parse("<https://x.example/u>", Some("other"))
                .unwrap()
                .one_click
        );
    }

    #[test]
    fn method_prefers_one_click_then_mail_then_web() {
        let me = Identity {
            id: "i1".into(),
            name: "Me".into(),
            email: "me@example.org".into(),
        };
        let both = |one_click| Unsubscribe {
            https: Some("https://x.example/u".into()),
            mailto: Some("mailto:list%2Bu@x.example?subject=Remove%20me&body=bye".into()),
            one_click,
        };

        assert_eq!(
            method(&both(true), &summary(), "me@example.org", &[], &[]),
            Some(Method::OneClick("https://x.example/u".into()))
        );

        let Some(Method::Mail(draft)) = method(
            &both(false),
            &summary(),
            "other@example.org",
            std::slice::from_ref(&me),
            &[],
        ) else {
            panic!("expected a mail");
        };

        assert_eq!(draft.account_id, "work");
        assert_eq!(draft.to[0].email, "list+u@x.example");
        assert_eq!(draft.subject, "Remove me");
        assert_eq!(draft.body, "bye");
        assert_eq!(draft.from.unwrap().email, "me@example.org");

        let web = Unsubscribe {
            mailto: None,
            ..both(false)
        };

        assert_eq!(
            method(&web, &summary(), "me@example.org", &[], &[]),
            Some(Method::Web("https://x.example/u".into()))
        );
    }

    #[test]
    fn mailto_without_a_recipient_falls_back_to_the_page() {
        let u = Unsubscribe {
            https: Some("https://x.example/u".into()),
            mailto: Some("mailto:?subject=x".into()),
            one_click: false,
        };

        assert_eq!(
            method(&u, &summary(), "me@example.org", &[], &[]),
            Some(Method::Web("https://x.example/u".into()))
        );
        assert_eq!(
            mailto_draft("mailto:u@x.example").unwrap().subject,
            "unsubscribe"
        );
    }

    #[test]
    fn checked_refuses_non_https_and_local_hosts() {
        for bad in [
            "http://x.example/u",
            "https://127.0.0.1/u",
            "https://[::1]/u",
            "https://192.168.1.1/u",
            "https://localhost/u",
            "https://a.localhost/u",
            "not a url",
        ] {
            assert!(checked(bad).is_err(), "{bad} passed");
        }

        assert!(checked("https://x.example/u?t=1").is_ok());
    }

    #[tokio::test]
    async fn post_sends_only_the_one_click_form() {
        let server = MockServer::start().await;

        Mock::given(http_method("POST"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string("List-Unsubscribe=One-Click"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        post(Url::parse(&format!("{}/u?t=secret", server.uri())).unwrap())
            .await
            .unwrap();

        let request = &server.received_requests().await.unwrap()[0];

        assert!(request.headers.get("cookie").is_none());
        assert!(request.headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn post_takes_a_redirect_as_done_and_hides_the_url_on_failure() {
        let server = MockServer::start().await;

        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(303).insert_header("location", "/elsewhere"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/u?t=secret", server.uri())).unwrap();

        post(url.clone()).await.unwrap();

        let error = post(url).await.unwrap_err().to_string();

        assert_eq!(error, "http: unsubscribe refused: HTTP 500");
        // Only the two POSTs: the redirect was not followed.
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
