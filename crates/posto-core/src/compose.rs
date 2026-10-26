//! Drafts: address parsing, replies, rendering and sending.

use serde::{Deserialize, Serialize};

use crate::actions::{self, Action};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::html;
use crate::provider::{Connection, Identity, MaskedEmail};
use crate::store::{Address, EmailBody, EmailSummary, Store};

/// A message being composed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Draft {
    /// Local slug of the account the message is sent from.
    #[serde(default)]
    pub account_id: String,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    /// Plain text, or Markdown when `markdown` is set.
    pub body: String,
    pub markdown: bool,
    /// Message-ID of the email being replied to.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub reply_to_email_id: Option<String>,
    /// Local account slug the replied-to email belongs to; `None` means
    /// `account_id`. Differs when a reply is sent from another account.
    #[serde(default)]
    pub reply_to_account_id: Option<String>,
    /// Sender; must be one of the account's identities or masked emails.
    /// `None` means the configured account address.
    #[serde(default)]
    pub from: Option<Address>,
}

/// Parses `"Name <a@b>, c@d"`. Parts are separated by `,` or `;` outside
/// quotes; invalid parts are dropped.
pub fn parse_addresses(s: &str) -> Vec<Address> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            ',' | ';' if !in_quotes => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);

    parts.iter().filter_map(|p| parse_address(p)).collect()
}

fn parse_address(part: &str) -> Option<Address> {
    let part = part.trim();
    let (name, email) = match (part.rfind('<'), part.strip_suffix('>')) {
        (Some(open), Some(rest)) => {
            let name = part[..open].trim().trim_matches('"').trim();
            (
                (!name.is_empty()).then(|| name.to_string()),
                &rest[open + 1..],
            )
        }
        _ => (None, part),
    };
    let email = email.trim();
    let mut sides = email.split('@');
    let valid = matches!(
        (sides.next(), sides.next(), sides.next()),
        (Some(local), Some(domain), None) if !local.is_empty() && !domain.is_empty()
    ) && !email.contains(char::is_whitespace);
    valid.then(|| Address {
        name,
        email: email.to_string(),
    })
}

/// The sender address of `identity`: its display name, if it has one.
fn identity_address(identity: &Identity) -> Address {
    Address {
        name: (!identity.name.is_empty()).then(|| identity.name.clone()),
        email: identity.email.clone(),
    }
}

/// The sender a reply to `summary` should come from: an identity the original
/// was addressed to, then a masked address it was addressed to, then the same
/// two among its Cc recipients, then the identity matching `me`. A masked
/// address carries no display name. Addresses compare ASCII-case-insensitively.
pub fn pick_identity(
    identities: &[Identity],
    masked: &[MaskedEmail],
    summary: &EmailSummary,
    me: &str,
) -> Option<Address> {
    let listed =
        |list: &[Address], email: &str| list.iter().any(|a| a.email.eq_ignore_ascii_case(email));
    let identity_in = |list: &[Address]| {
        identities
            .iter()
            .find(|i| listed(list, &i.email))
            .map(identity_address)
    };
    let masked_in = |list: &[Address]| {
        masked
            .iter()
            .find(|m| listed(list, &m.email))
            .map(|m| Address {
                name: None,
                email: m.email.clone(),
            })
    };

    identity_in(&summary.to)
        .or_else(|| masked_in(&summary.to))
        .or_else(|| identity_in(&summary.cc))
        .or_else(|| masked_in(&summary.cc))
        .or_else(|| {
            identities
                .iter()
                .find(|i| i.email.eq_ignore_ascii_case(me))
                .map(identity_address)
        })
}

/// Builds a reply to `summary`, sent from the account the original arrived in.
/// `me` is the sender's own address; `identities` and `masked` are that
/// account's sending identities and masked email addresses.
///
/// The recipients are the original recipients when every author is me;
/// otherwise the message's `Reply-To` addresses (deduplicated, mine left out
/// unless they are all mine) when it has any; otherwise the original authors.
/// A reply-all through `Reply-To` never copies the original authors.
pub fn reply(
    summary: &EmailSummary,
    body: &EmailBody,
    me: &str,
    reply_all: bool,
    identities: &[Identity],
    masked: &[MaskedEmail],
) -> Draft {
    let is_mine = |a: &Address| {
        a.email.eq_ignore_ascii_case(me)
            || identities
                .iter()
                .any(|i| i.email.eq_ignore_ascii_case(&a.email))
            || masked
                .iter()
                .any(|m| m.email.eq_ignore_ascii_case(&a.email))
    };

    // Own sent mail only when every author is me; a co-author stays a recipient.
    let mut via_reply_to = false;
    let to: Vec<Address> = if !summary.from.is_empty() && summary.from.iter().all(is_mine) {
        summary.to.clone()
    } else if !body.reply_to.is_empty() {
        let mut rt: Vec<Address> = Vec::new();
        for a in &body.reply_to {
            if !rt.iter().any(|x| x.email.eq_ignore_ascii_case(&a.email)) {
                rt.push(a.clone());
            }
        }
        let ext: Vec<Address> = rt.iter().filter(|a| !is_mine(a)).cloned().collect();

        via_reply_to = true;
        if ext.is_empty() { rt } else { ext }
    } else {
        summary.from.clone()
    };

    let mut cc: Vec<Address> = Vec::new();
    if reply_all {
        for a in summary.to.iter().chain(&summary.cc) {
            let known = |x: &Address| x.email.eq_ignore_ascii_case(&a.email);
            if !is_mine(a)
                && !to.iter().any(known)
                && !cc.iter().any(known)
                && !(via_reply_to && summary.from.iter().any(known))
            {
                cc.push(a.clone());
            }
        }
    }

    let subject = if summary
        .subject
        .trim_start()
        .to_lowercase()
        .starts_with("re:")
    {
        summary.subject.clone()
    } else {
        format!("Re: {}", summary.subject)
    };

    let mut references = body.references.clone();
    references.extend(body.message_id.iter().cloned());

    Draft {
        account_id: summary.account_id.clone(),
        to,
        cc,
        bcc: Vec::new(),
        subject,
        body: quote(summary, body),
        markdown: false,
        in_reply_to: body.message_id.first().cloned(),
        references,
        reply_to_email_id: Some(summary.id.clone()),
        reply_to_account_id: Some(summary.account_id.clone()),
        from: pick_identity(identities, masked, summary, me),
    }
}

/// The "On <date>, <who> wrote:" header and the quoted original.
fn quote(summary: &EmailSummary, body: &EmailBody) -> String {
    let text = match (&body.text, &body.html) {
        (Some(text), _) if !text.trim().is_empty() => text.clone(),
        (_, Some(html)) => html::to_text(html),
        _ => String::new(),
    };
    let quoted = text
        .trim_end()
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    let date = chrono::DateTime::parse_from_rfc3339(&summary.received_at)
        .map(|d| d.format("%a, %d %b %Y %H:%M:%S %z").to_string())
        .unwrap_or_else(|_| summary.received_at.clone());
    let who = summary
        .from
        .first()
        .map(|a| match a.name.as_deref() {
            Some(name) if !name.is_empty() => name,
            _ => &a.email,
        })
        .unwrap_or("unknown sender");

    format!("\n\nOn {date}, {who} wrote:\n{quoted}")
}

/// GitHub-flavoured Markdown to sanitised HTML.
pub fn render_markdown(md: &str) -> String {
    use pulldown_cmark::{Options, Parser};

    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, Parser::new_ext(md, options));
    html::sanitize(&out, true).html
}

/// The `(text, html)` parts of the outgoing message.
pub fn render(draft: &Draft) -> (String, String) {
    let html = if draft.markdown {
        render_markdown(&draft.body)
    } else {
        format!(
            "<pre style=\"white-space:pre-wrap;font-family:inherit\">{}</pre>",
            html::escape(&draft.body)
        )
    };
    (draft.body.clone(), html)
}

/// The identity to send as: the one matching `from`, else (for a masked
/// address of the account) the account's own identity with `from`'s address
/// and no display name. Without `from`, the identity matching `me`, else the
/// first one. Masked emails are fetched only when `from` is no identity.
async fn sender(
    client: &Connection,
    identities: &[Identity],
    from: Option<&Address>,
    me: &str,
) -> Result<Identity> {
    let base = || {
        identities
            .iter()
            .find(|i| i.email.eq_ignore_ascii_case(me))
            .or(identities.first())
            .ok_or_else(|| Error::NotConfigured("no sending identity".into()))
    };
    let Some(from) = from else {
        return base().cloned();
    };

    if let Some(identity) = identities
        .iter()
        .find(|i| i.email.eq_ignore_ascii_case(&from.email))
    {
        return Ok(identity.clone());
    }

    let masked = client.masked_emails().await?;
    let masked = masked
        .iter()
        .find(|m| m.email.eq_ignore_ascii_case(&from.email))
        .ok_or_else(|| Error::NotConfigured(format!("no identity for {}", from.email)))?;

    Ok(Identity {
        id: base()?.id.clone(),
        name: String::new(),
        email: masked.email.clone(),
    })
}

/// Sends `draft` through `client` and files it under Sent. Marks the
/// replied-to email `$answered` when it lives in `account` (a reply sent from
/// another account leaves the original to the caller). Returns the new email id.
///
/// `client` must be the connection of `account`. The sent email is not
/// inserted into the store; the next sync picks it up.
///
/// # Errors
///
/// Returns [`Error::NotConfigured`] for an unknown account or without a sending identity,
/// [`Error::NotFound`] when the Drafts or Sent mailbox is unknown, and a
/// provider error when the server refuses to create or submit the email.
/// Once the mail is submitted, a failed move to Sent is reported as
/// [`Error::Other`]; callers must not resend then.
pub async fn send(
    cfg: &Config,
    store: &Store,
    client: &Connection,
    account: &str,
    draft: &Draft,
) -> Result<String> {
    let me = cfg
        .account(account)
        .ok_or_else(|| Error::NotConfigured(format!("account {account}")))?
        .email
        .as_str();
    let identities = client.identities().await?;
    let identity = sender(client, &identities, draft.from.as_ref(), me).await?;
    let (text, html) = render(draft);
    let id = client
        .send(store, account, &identity, draft, &text, &html)
        .await?;

    if let Some(reply_to) = &draft.reply_to_email_id
        && draft
            .reply_to_account_id
            .as_deref()
            .is_none_or(|a| a == account)
        && let Err(e) = actions::apply(
            store,
            Some(client),
            account,
            std::slice::from_ref(reply_to),
            Action::MarkAnswered,
        )
        .await
    {
        tracing::warn!("sent {id}, but marking {reply_to} answered failed: {e}");
    }

    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: Option<&str>, email: &str) -> Address {
        Address {
            name: name.map(str::to_string),
            email: email.to_string(),
        }
    }

    fn summary(from: Address, to: Vec<Address>, cc: Vec<Address>) -> EmailSummary {
        EmailSummary {
            account_id: "a".into(),
            id: "e1".into(),
            thread_id: "t1".into(),
            received_at: "2026-10-02T09:30:00Z".into(),
            from: vec![from],
            to,
            cc,
            subject: "Lunch".into(),
            preview: String::new(),
            size: 0,
            has_attachment: false,
            mailbox_ids: vec![],
            keywords: vec![],
        }
    }

    fn body(text: &str) -> EmailBody {
        EmailBody {
            email_id: "e1".into(),
            html: None,
            text: Some(text.into()),
            attachments: vec![],
            in_reply_to: vec![],
            references: vec!["r0@x".into()],
            message_id: vec!["m1@x".into()],
            reply_to: vec![],
            unsubscribe: None,
        }
    }

    #[test]
    fn pick_identity_prefers_to_then_cc_then_me() {
        let ids = [("i1", "me@x.io"), ("i2", "shop@x.io")].map(|(id, email)| Identity {
            id: id.into(),
            name: String::new(),
            email: email.into(),
        });
        let pick = |s: &EmailSummary, ids: &[Identity]| {
            pick_identity(ids, &[], s, "me@x.io").map(|a| a.email)
        };
        let from = addr(None, "bob@y.io");
        let other = addr(None, "other@y.io");
        let shop = addr(None, "SHOP@x.io");

        let s = summary(from.clone(), vec![shop.clone()], vec![]);
        assert_eq!(pick(&s, &ids).as_deref(), Some("shop@x.io"));

        let s = summary(from.clone(), vec![other.clone()], vec![shop]);
        assert_eq!(pick(&s, &ids).as_deref(), Some("shop@x.io"));

        let s = summary(from, vec![other], vec![]);
        assert_eq!(pick(&s, &ids).as_deref(), Some("me@x.io"));
        assert_eq!(pick(&s, &[]), None);
    }

    fn mask(email: &str) -> MaskedEmail {
        MaskedEmail {
            id: email.into(),
            email: email.into(),
            description: String::new(),
            for_domain: String::new(),
            state: "enabled".into(),
            last_message_at: None,
        }
    }

    #[test]
    fn pick_identity_prefers_a_masked_address_the_mail_was_sent_to() {
        let ids = [Identity {
            id: "i1".into(),
            name: String::new(),
            email: "me@x.io".into(),
        }];
        let masked = [mask("m1@x.io"), mask("m2@x.io")];
        let pick = |to: &str, cc: &str| {
            let list = |e: &str| {
                if e.is_empty() {
                    vec![]
                } else {
                    vec![addr(None, e)]
                }
            };
            let s = summary(addr(None, "bob@y.io"), list(to), list(cc));

            pick_identity(&ids, &masked, &s, "me@x.io")
        };

        assert_eq!(pick("M1@X.IO", ""), Some(addr(None, "m1@x.io")));
        assert_eq!(pick("me@x.io", "m2@x.io"), Some(addr(None, "me@x.io")));
        assert_eq!(pick("m2@x.io", "me@x.io"), Some(addr(None, "m2@x.io")));
        assert_eq!(pick("", "M2@x.io"), Some(addr(None, "m2@x.io")));
        assert_eq!(pick("stranger@y.io", ""), Some(addr(None, "me@x.io")));
    }

    #[test]
    fn reply_all_leaves_out_my_masked_address() {
        let masked = [mask("m1@x.io")];
        let s = summary(
            addr(None, "bob@y.io"),
            vec![addr(None, "m1@x.io"), addr(None, "carol@z.io")],
            vec![addr(None, "me@x.io")],
        );
        let d = reply(&s, &body("x"), "me@x.io", true, &[], &masked);

        assert_eq!(d.to, vec![addr(None, "bob@y.io")]);
        assert_eq!(d.cc, vec![addr(None, "carol@z.io")]);
        assert_eq!(d.from, Some(addr(None, "m1@x.io")));

        let own = summary(addr(None, "M1@x.io"), vec![addr(None, "bob@y.io")], vec![]);

        assert_eq!(
            reply(&own, &body("x"), "me@x.io", false, &[], &masked).to,
            vec![addr(None, "bob@y.io")]
        );
    }

    #[test]
    fn parse_addresses_mixed() {
        let got = parse_addresses("Ann <ann@x.io>, bob@y.io; bad, \"C, D\" <cd@z.io>");
        assert_eq!(
            got,
            vec![
                addr(Some("Ann"), "ann@x.io"),
                addr(None, "bob@y.io"),
                addr(Some("C, D"), "cd@z.io"),
            ]
        );
        assert!(parse_addresses("a@b@c, @x, y@, a b@c.io").is_empty());
    }

    #[test]
    fn reply_quotes_and_addresses() {
        let from = addr(Some("Bob"), "bob@y.io");
        let s = summary(from.clone(), vec![addr(None, "me@x.io")], vec![]);
        let d = reply(&s, &body("hello\nworld"), "me@x.io", false, &[], &[]);

        assert_eq!(d.subject, "Re: Lunch");
        assert_eq!(d.to, vec![from]);
        assert!(d.cc.is_empty());
        assert!(
            d.body
                .contains("On Fri, 02 Oct 2026 09:30:00 +0000, Bob wrote:"),
            "{}",
            d.body
        );
        assert!(d.body.contains("\n> hello\n> world"), "{}", d.body);
        assert_eq!(d.in_reply_to.as_deref(), Some("m1@x"));
        assert_eq!(d.references, vec!["r0@x", "m1@x"]);
        assert_eq!(d.reply_to_email_id.as_deref(), Some("e1"));
        assert_eq!(d.account_id, "a");
        assert!(!d.markdown);

        let mut again = s.clone();
        again.subject = "RE: Lunch".into();
        assert_eq!(
            reply(&again, &body("x"), "me@x.io", false, &[], &[]).subject,
            "RE: Lunch"
        );

        // Replying to my own mail goes to the original recipients.
        let mine = summary(addr(None, "ME@x.io"), vec![addr(None, "bob@y.io")], vec![]);
        assert_eq!(
            reply(&mine, &body("x"), "me@x.io", false, &[], &[]).to,
            vec![addr(None, "bob@y.io")]
        );
    }

    #[test]
    fn reply_to_mixed_senders_keeps_original_from() {
        let mut s = summary(
            addr(None, "me@x.io"),
            vec![addr(None, "carol@x.io")],
            vec![],
        );
        s.from.push(addr(None, "bob@x.io"));

        let d = reply(&s, &body("x"), "me@x.io", false, &[], &[]);

        assert_eq!(d.to, s.from, "a co-author must stay a recipient");

        s.from = vec![addr(None, "me@x.io"), addr(None, "ME@x.io")];

        assert_eq!(
            reply(&s, &body("x"), "me@x.io", false, &[], &[]).to,
            s.to,
            "all authors are me: reply to the recipients"
        );
    }

    #[test]
    fn reply_all_excludes_me_and_dedupes() {
        let me = addr(None, "Me@X.io");
        let carol = addr(None, "carol@z.io");
        let dave = addr(None, "dave@z.io");
        let s = summary(
            addr(None, "bob@y.io"),
            vec![me, carol.clone()],
            vec![
                dave.clone(),
                addr(None, "CAROL@z.io"),
                addr(None, "bob@y.io"),
            ],
        );
        let d = reply(&s, &body("x"), "me@x.io", true, &[], &[]);
        assert_eq!(d.to, vec![addr(None, "bob@y.io")]);
        assert_eq!(d.cc, vec![carol, dave]);
    }

    #[test]
    fn reply_goes_to_reply_to() {
        let ids = ["me@x.io", "shop@x.io"].map(|email| Identity {
            id: email.into(),
            name: String::new(),
            email: email.into(),
        });
        let masked = [mask("m1@x.io")];
        let go = |from: &[&str], to: &str, reply_to: Vec<Address>| {
            let mut s = summary(addr(None, "x@y.io"), vec![addr(None, to)], vec![]);
            let mut b = body("x");

            s.from = from.iter().map(|e| addr(None, e)).collect();
            b.reply_to = reply_to;

            reply(&s, &b, "me@x.io", false, &ids, &masked)
        };
        let bob = addr(None, "bob@y.io");
        let ra = addr(None, "ra@passmail.com");

        let d = go(&["bob@y.io"], "me@x.io", vec![ra.clone()]);
        assert_eq!(d.to, vec![ra.clone()]);
        assert!(d.cc.is_empty());

        assert_eq!(go(&["bob@y.io"], "me@x.io", vec![]).to, vec![bob.clone()]);

        let two = vec![addr(Some("R1"), "r1@lists.io"), addr(None, "r2@lists.io")];
        assert_eq!(go(&["bob@y.io"], "me@x.io", two.clone()).to, two);

        let messy = vec![
            addr(None, "ME@X.IO"),
            addr(None, "Ra@Passmail.com"),
            ra.clone(),
        ];
        assert_eq!(
            go(&["bob@y.io"], "me@x.io", messy).to,
            vec![addr(None, "Ra@Passmail.com")]
        );

        for mine in [
            vec![addr(None, "me@x.io")],
            vec![addr(None, "shop@x.io")],
            vec![addr(None, "m1@x.io")],
            vec![addr(None, "shop@x.io"), addr(None, "m1@x.io")],
        ] {
            assert_eq!(go(&["bob@y.io"], "me@x.io", mine.clone()).to, mine);
        }

        assert_eq!(
            go(&["bob@y.io"], "me@x.io", vec![bob.clone()]).to,
            vec![bob]
        );

        let carol = addr(None, "carol@z.io");
        for from in [
            vec!["me@x.io"],
            vec!["shop@x.io"],
            vec!["m1@x.io"],
            vec!["me@x.io", "shop@x.io"],
        ] {
            assert_eq!(
                go(&from, "carol@z.io", vec![ra.clone()]).to,
                vec![carol.clone()]
            );
        }

        assert_eq!(
            go(&["me@x.io", "bob@y.io"], "me@x.io", vec![ra.clone()]).to,
            vec![ra.clone()]
        );
        assert_eq!(go(&[], "carol@z.io", vec![ra.clone()]).to, vec![ra]);
    }

    #[test]
    fn reply_all_with_reply_to_leaves_out_the_original_sender() {
        let s = summary(
            addr(None, "bob@y.io"),
            vec![
                addr(None, "me@x.io"),
                addr(None, "carol@z.io"),
                addr(None, "BOB@y.io"),
            ],
            vec![
                addr(None, "dave@z.io"),
                addr(None, "Bob@Y.io"),
                addr(None, "RA@passmail.com"),
            ],
        );
        let mut b = body("x");

        b.reply_to = vec![addr(None, "ra@passmail.com")];

        let d = reply(&s, &b, "me@x.io", true, &[], &[]);
        assert_eq!(d.to, vec![addr(None, "ra@passmail.com")]);
        assert_eq!(
            d.cc,
            vec![addr(None, "carol@z.io"), addr(None, "dave@z.io")]
        );

        let d = reply(&s, &body("x"), "me@x.io", true, &[], &[]);
        assert_eq!(d.to, vec![addr(None, "bob@y.io")]);
        assert_eq!(
            d.cc,
            vec![
                addr(None, "carol@z.io"),
                addr(None, "dave@z.io"),
                addr(None, "RA@passmail.com")
            ]
        );
    }

    #[test]
    fn render_markdown_sanitised() {
        let out =
            render_markdown("# Hi\n\n**b** <script>x</script>\n\n| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(out.contains("<h1>"), "{out}");
        assert!(out.contains("<strong>"), "{out}");
        assert!(out.contains("<table>"), "{out}");
        assert!(!out.contains("<script"), "{out}");
    }

    #[test]
    fn render_markdown_keeps_strikethrough_and_task_state() {
        let out = render_markdown("~~gone~~\n\n- [x] done\n- [ ] todo");
        assert!(out.contains("<del>gone</del>"), "{out}");

        let items: Vec<&str> = out.split("<li>").skip(1).collect();
        assert_eq!(items.len(), 2, "{out}");
        assert!(
            items[0].contains("checked") && items[0].contains("done"),
            "{out}"
        );
        assert!(
            !items[1].contains("checked") && items[1].contains("todo"),
            "{out}"
        );
        assert!(
            items.iter().all(|i| i.contains(r#"type="checkbox""#)),
            "{out}"
        );
    }

    #[test]
    fn render_plain_wraps_pre() {
        let draft = Draft {
            body: "a<b".into(),
            ..Draft::default()
        };
        let (text, html) = render(&draft);
        assert_eq!(text, "a<b");
        assert!(html.contains("<pre"), "{html}");
        assert!(html.contains("a&lt;b"), "{html}");
    }
}
