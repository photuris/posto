//! Drafts: address parsing, replies and rendering.
//!
//! Sending lives in a later task because it needs the JMAP client.

use serde::{Deserialize, Serialize};

use crate::html;
use crate::store::{Address, EmailBody, EmailSummary};

/// A message being composed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Draft {
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

/// Builds a reply to `summary`. `me` is the sender's own address.
pub fn reply(summary: &EmailSummary, body: &EmailBody, me: &str, reply_all: bool) -> Draft {
    let is_me = |a: &Address| a.email.eq_ignore_ascii_case(me);

    let to: Vec<Address> = if summary.from.iter().any(is_me) {
        summary.to.clone()
    } else {
        summary.from.clone()
    };

    let mut cc: Vec<Address> = Vec::new();
    if reply_all {
        for a in summary.to.iter().chain(&summary.cc) {
            let known = |x: &Address| x.email.eq_ignore_ascii_case(&a.email);
            if !is_me(a) && !to.iter().any(known) && !cc.iter().any(known) {
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
        to,
        cc,
        bcc: Vec::new(),
        subject,
        body: quote(summary, body),
        markdown: false,
        in_reply_to: body.message_id.first().cloned(),
        references,
        reply_to_email_id: Some(summary.id.clone()),
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
        }
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
        let d = reply(&s, &body("hello\nworld"), "me@x.io", false);

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
        assert!(!d.markdown);

        let mut again = s.clone();
        again.subject = "RE: Lunch".into();
        assert_eq!(
            reply(&again, &body("x"), "me@x.io", false).subject,
            "RE: Lunch"
        );

        // Replying to my own mail goes to the original recipients.
        let mine = summary(addr(None, "ME@x.io"), vec![addr(None, "bob@y.io")], vec![]);
        assert_eq!(
            reply(&mine, &body("x"), "me@x.io", false).to,
            vec![addr(None, "bob@y.io")]
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
        let d = reply(&s, &body("x"), "me@x.io", true);
        assert_eq!(d.to, vec![addr(None, "bob@y.io")]);
        assert_eq!(d.cc, vec![carol, dave]);
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
