//! The state block sent to Jev, ported from jevmail's `internal/message`.

use html2text::render::TextDecorator;

use crate::store::{EmailBody, EmailSummary};

/// Everything the state block shows.
pub struct StateInput<'a> {
    /// Sender display name; the lower-cased address when the sender has none.
    pub from_name: &'a str,
    /// Sender address, lower-cased.
    pub from_email: &'a str,
    /// Subject line; empty becomes "(no subject)".
    pub subject: &'a str,
    /// RFC 3339 UTC timestamp.
    pub received_at: &'a str,
    /// Whether the message has a `List-Unsubscribe` header.
    pub has_unsubscribe: bool,
    /// Whether the user has already written in this thread.
    pub replied: bool,
    /// Cleaned body text; empty becomes "(empty)".
    pub body: &'a str,
}

/// Renders the state block, line for line as jevmail's `message.State`.
pub fn state_block(i: &StateInput) -> String {
    let subject = if i.subject.is_empty() {
        "(no subject)"
    } else {
        i.subject
    };
    let body = if i.body.is_empty() { "(empty)" } else { i.body };
    let yes_no = |b: bool| if b { "yes" } else { "no" };

    format!(
        "From: {} <{}>\nSubject: {subject}\nReceived: {}\nHas unsubscribe header: {}\n\
         User has previously replied in this thread: {}\n\nBody:\n{body}",
        i.from_name,
        i.from_email,
        i.received_at,
        yes_no(i.has_unsubscribe),
        yes_no(i.replied),
    )
}

/// Builds the state block of a stored email: sender, cleaned body (text part,
/// else HTML as text, else the preview) cut to `body_chars`.
pub fn state_for(
    email: &EmailSummary,
    body: &EmailBody,
    has_unsubscribe: bool,
    replied: bool,
    body_chars: usize,
) -> String {
    let from_email = email
        .from
        .first()
        .map(|a| a.email.to_lowercase())
        .unwrap_or_default();
    let from_name = email
        .from
        .first()
        .and_then(|a| a.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| from_email.clone());
    let raw = match (&body.text, &body.html) {
        (Some(text), _) => text.clone(),
        (None, Some(html)) => html_to_text(html),
        (None, None) => email.preview.clone(),
    };

    state_block(&StateInput {
        from_name: &from_name,
        from_email: &from_email,
        subject: &email.subject,
        received_at: &utc(&email.received_at),
        has_unsubscribe,
        replied,
        body: &clean_body(&raw, body_chars),
    })
}

/// `s` as RFC 3339 UTC to the second; text that does not parse is kept.
fn utc(s: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
        .unwrap_or_else(|_| s.to_string())
}

/// Decorator matching jaytaylor/html2text with `OmitLinks`: bold and strong
/// text is wrapped in `*`, list items start with `* `, quotes with `> `; links,
/// emphasis, code and images add nothing.
#[derive(Clone, Debug)]
struct JevDecorator;

impl TextDecorator for JevDecorator {
    type Annotation = ();

    fn decorate_link_start(&mut self, _url: &str) -> (String, ()) {
        (String::new(), ())
    }

    fn decorate_link_end(&mut self) -> String {
        String::new()
    }

    fn decorate_em_start(&self) -> (String, ()) {
        (String::new(), ())
    }

    fn decorate_em_end(&self) -> String {
        String::new()
    }

    fn decorate_strong_start(&self) -> (String, ()) {
        ("*".into(), ())
    }

    fn decorate_strong_end(&self) -> String {
        "*".into()
    }

    fn decorate_strikeout_start(&self) -> (String, ()) {
        (String::new(), ())
    }

    fn decorate_strikeout_end(&self) -> String {
        String::new()
    }

    fn decorate_code_start(&self) -> (String, ()) {
        (String::new(), ())
    }

    fn decorate_code_end(&self) -> String {
        String::new()
    }

    fn decorate_preformat_first(&self) {}

    fn decorate_preformat_cont(&self) {}

    fn decorate_image(&mut self, _src: &str, _title: &str) -> (String, ()) {
        (String::new(), ())
    }

    fn header_prefix(&self, _level: usize) -> String {
        String::new()
    }

    fn quote_prefix(&self) -> String {
        "> ".into()
    }

    fn unordered_item_prefix(&self) -> String {
        "* ".into()
    }

    fn ordered_item_prefix(&self, _i: i64) -> String {
        "* ".into()
    }

    fn make_subblock_decorator(&self) -> Self {
        JevDecorator
    }
}

/// HTML as text the way jevmail's `message.HTMLToText` renders it, with the
/// divider lines the pinned renderer draws around `h1`-`h3` headings.
fn html_to_text(html: &str) -> String {
    render(&with_heading_dividers(html))
}

/// Renders HTML through [`JevDecorator`], without heading dividers.
fn render(html: &str) -> String {
    html2text::config::with_decorator(JevDecorator)
        .string_from_read(html.as_bytes(), 10_000)
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|_| crate::html::to_text(html))
}

/// Rewrites each `h1`-`h3` element as a paragraph carrying the dividers
/// jaytaylor/html2text 1a4bdc82ecec draws: `*` above and below an `h1`, `-`
/// above and below an `h2`, `-` below an `h3`. A divider is as long as the
/// longest heading line less one, counted with the leading space the Go
/// renderer puts before a heading's first word (which it later strips, so the
/// divider ends up as long as the text).
fn with_heading_dividers(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut at = 0;

    while let Some(start) = find_heading(&lower, at) {
        let level = lower.as_bytes()[start + 2];
        let close = format!("</h{}", level as char);
        let Some(open_end) = lower[start..].find('>').map(|n| start + n + 1) else {
            break;
        };
        let Some(inner_end) = lower[open_end..].find(&close).map(|n| open_end + n) else {
            break;
        };
        let Some(close_end) = lower[inner_end..].find('>').map(|n| inner_end + n + 1) else {
            break;
        };

        out.push_str(&html[at..start]);
        out.push_str(&heading_block(level, &render(&html[open_end..inner_end])));

        at = close_end;
    }

    out.push_str(&html[at..]);

    out
}

/// Byte offset of the next `<h1`, `<h2` or `<h3` tag at or after `from`.
fn find_heading(lower: &str, from: usize) -> Option<usize> {
    let mut at = from;

    while let Some(n) = lower[at..].find("<h") {
        let start = at + n;
        let rest = &lower.as_bytes()[start + 2..];

        if matches!(rest.first(), Some(b'1'..=b'3'))
            && rest
                .get(1)
                .is_none_or(|c| c.is_ascii_whitespace() || matches!(c, b'>' | b'/'))
        {
            return Some(start);
        }

        at = start + 2;
    }

    None
}

/// The paragraph standing in for one heading of `level` (`b'1'`-`b'3'`) whose
/// rendered text is `text`; nothing for an empty heading.
fn heading_block(level: u8, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let longest = text
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let chars = line.chars().count();

            // The first line was emitted after a space, unless it starts with ".".
            if i == 0 && !line.starts_with('.') {
                chars
            } else {
                chars.saturating_sub(1)
            }
        })
        .max()
        .unwrap_or(0);
    let divider = (if level == b'1' { "*" } else { "-" }).repeat(longest);
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "<br>");

    if level == b'3' {
        format!("<p>{escaped}<br>{divider}</p>")
    } else {
        format!("<p>{divider}<br>{escaped}<br>{divider}</p>")
    }
}

/// Go's `\s`: ASCII space, tab, newline, form feed, carriage return.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')
}

/// Removes every non-overlapping, leftmost match of `matcher`, which returns
/// the byte length of a match starting at the head of its argument.
fn remove_matches(s: &str, matcher: impl Fn(&str) -> Option<usize>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(c) = rest.chars().next() {
        match matcher(rest) {
            Some(len) if len > 0 => rest = &rest[len..],
            _ => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }

    out
}

/// Length of `\S+` at the head of `s`, or `None` when it starts with a space.
fn non_space_run(s: &str) -> Option<usize> {
    let len = s.find(is_space).unwrap_or(s.len());

    (len > 0).then_some(len)
}

/// Match of `https?://\S+` at the head of `s`.
fn bare_url(s: &str) -> Option<usize> {
    let scheme = ["https://", "http://"]
        .into_iter()
        .find(|p| s.starts_with(p))?;

    non_space_run(&s[scheme.len()..]).map(|n| scheme.len() + n)
}

/// Match of `www\.\S+` at the head of `s`.
fn www(s: &str) -> Option<usize> {
    s.strip_prefix("www.")
        .and_then(non_space_run)
        .map(|n| 4 + n)
}

/// Match of `<https?://[^>]*>` at the head of `s`.
fn angle_url(s: &str) -> Option<usize> {
    let inner = s.strip_prefix('<')?;

    if !(inner.starts_with("http://") || inner.starts_with("https://")) {
        return None;
    }

    inner.find('>').map(|n| n + 2)
}

/// Match of `!\[[^\]]*\]\([^)]*\)` at the head of `s`.
fn md_image(s: &str) -> Option<usize> {
    let alt = s.strip_prefix("![")?;
    let close = alt.find(']')?;
    let target = alt[close + 1..].strip_prefix('(')?;
    let end = target.find(')')?;

    Some(2 + close + 2 + end + 1)
}

/// Collapses runs of `min` or more of `run` to the shorter `to`.
fn collapse(s: &str, is_run: impl Fn(char) -> bool, min: usize, to: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if !is_run(c) {
            out.push(c);

            continue;
        }

        let mut run = String::from(c);

        while let Some(&next) = chars.peek() {
            if !is_run(next) {
                break;
            }

            run.push(next);
            chars.next();
        }

        if run.chars().count() >= min {
            out.push_str(to);
        } else {
            out.push_str(&run);
        }
    }

    out
}

/// Port of jevmail's `message.Clean`: strips images, URLs and excess
/// whitespace, then cuts to `max_chars` characters with "…" appended when
/// longer.
pub fn clean_body(s: &str, max_chars: usize) -> String {
    let s = remove_matches(s, md_image);
    let s = remove_matches(&s, angle_url);
    let s = remove_matches(&s, bare_url);
    let s = remove_matches(&s, www);
    let s = s.replace('\r', "");
    let s = collapse(&s, |c| c == ' ' || c == '\t', 2, " ");
    let s = collapse(&s, |c| c == '\n', 3, "\n\n");
    let s = s.trim();

    if s.chars().count() <= max_chars {
        return s.to_string();
    }

    let cut: String = s.chars().take(max_chars).collect();

    cut + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Address;

    fn email(from: Vec<Address>, subject: &str) -> EmailSummary {
        EmailSummary {
            account_id: "a".into(),
            id: "e1".into(),
            thread_id: "t1".into(),
            received_at: "2026-10-01T12:30:05+02:00".into(),
            from,
            to: vec![],
            cc: vec![],
            subject: subject.into(),
            preview: "the preview".into(),
            size: 1,
            has_attachment: false,
            mailbox_ids: vec![],
            keywords: vec![],
        }
    }

    fn body(text: Option<&str>, html: Option<&str>) -> EmailBody {
        EmailBody {
            email_id: "e1".into(),
            html: html.map(String::from),
            text: text.map(String::from),
            attachments: vec![],
            in_reply_to: vec![],
            references: vec![],
            message_id: vec![],
            reply_to: vec![],
        }
    }

    fn from(name: Option<&str>, address: &str) -> Vec<Address> {
        vec![Address {
            name: name.map(String::from),
            email: address.into(),
        }]
    }

    #[test]
    fn state_block_format() {
        // No subject, empty body, unsubscribe yes, replied no.
        let e = email(from(Some("Ann"), "Ann@Example.COM"), "");
        let s = state_for(&e, &body(Some(""), None), true, false, 2000);

        assert_eq!(
            s,
            "From: Ann <ann@example.com>\n\
             Subject: (no subject)\n\
             Received: 2026-10-01T10:30:05Z\n\
             Has unsubscribe header: yes\n\
             User has previously replied in this thread: no\n\
             \n\
             Body:\n\
             (empty)"
        );

        // Empty name falls back to the lower-cased address.
        let e = email(from(None, "Me@X.io"), "Hi");
        let s = state_for(&e, &body(Some("hello"), None), false, true, 2000);

        assert_eq!(
            s,
            "From: me@x.io <me@x.io>\n\
             Subject: Hi\n\
             Received: 2026-10-01T10:30:05Z\n\
             Has unsubscribe header: no\n\
             User has previously replied in this thread: yes\n\
             \n\
             Body:\n\
             hello"
        );

        // A non-empty Unicode HTML body: tags and link markers go, text stays,
        // the URL is cleaned away and the body is cut with an ellipsis.
        let html = "<html><body><h1>Grüße, 日本語</h1><p>Visit <a href=\"https://x.example/a\">our \
                    shop</a> or www.x.example today — naïve café ☕</p></body></html>";
        let e = email(from(Some("Shop"), "shop@x.example"), "Sale");
        let s = state_for(&e, &body(None, Some(html)), false, false, 2000);
        let text = s.split_once("Body:\n").unwrap().1;

        assert_eq!(
            text,
            "**********\nGrüße, 日本語\n**********\n\nVisit our shop or today — naïve café ☕"
        );

        let cut = state_for(&e, &body(None, Some(html)), false, false, 5);

        assert!(cut.ends_with("Body:\n*****…"), "{cut}");

        // Without any body part the preview is used.
        let s = state_for(&e, &body(None, None), false, false, 2000);

        assert!(s.ends_with("Body:\nthe preview"), "{s}");

        // A sender-less message keeps jevmail's empty shape.
        let s = state_for(&email(vec![], "x"), &body(Some("b"), None), false, false, 9);

        assert!(s.starts_with("From:  <>\n"), "{s}");
    }

    #[test]
    fn html_to_text_matches_jevmail() {
        // jevmail's TestHTMLToText.
        assert_eq!(html_to_text("<p>Hi <b>there</b></p>"), "Hi *there*");
        assert_eq!(html_to_text("<p>Hi <strong>you</strong></p>"), "Hi *you*");
        // Emphasis and links add nothing.
        assert_eq!(
            html_to_text("<p>a <i>b</i> <a href=\"https://x.example\">c</a></p>"),
            "a b c"
        );
        assert_eq!(
            html_to_text("<ul><li>one</li><li>two</li></ul>"),
            "* one\n* two"
        );
    }

    #[test]
    fn headings_get_the_pinned_renderers_dividers() {
        // Divider length is the heading text length (Go counts a leading space
        // and subtracts one); h1 uses "*", h2 "-" on both sides, h3 "-" below.
        assert_eq!(
            html_to_text("<h1>Heading</h1>"),
            "*******\nHeading\n*******"
        );
        assert_eq!(
            html_to_text("<h2>Heading</h2>"),
            "-------\nHeading\n-------"
        );
        assert_eq!(html_to_text("<h3>Heading</h3>"), "Heading\n-------");
        assert_eq!(
            html_to_text("<p>Intro</p><H1 class=x>Big <b>news</b></H1><p>Body</p>"),
            "Intro\n\n**********\nBig *news*\n**********\n\nBody"
        );
        // Other heading levels and `<hr>`/`<head>` are left to the renderer.
        assert_eq!(html_to_text("<h4>Small</h4>"), "Small");
        assert_eq!(html_to_text("<h1></h1><p>x</p>"), "x");
    }

    #[test]
    fn clean_body_matches_jevmail() {
        let raw = "Hi\r\n\r\n\r\n\r\nsee ![logo](cid:1) <https://a.example/x?y=1> https://b.example/p \
                   and www.c.example/q now\t\t\tgo  on\n\n\n\nbye  ";

        assert_eq!(clean_body(raw, 2000), "Hi\n\nsee and now go on\n\nbye");
        // Exactly three newlines collapse to two; two stay.
        assert_eq!(clean_body("a\n\n\nb", 2000), "a\n\nb");
        assert_eq!(clean_body("a\n\nb", 2000), "a\n\nb");
        // A bare scheme with nothing after it is not a URL.
        assert_eq!(clean_body("http:// x", 2000), "http:// x");
        // Exactly max_chars stays whole; one more is cut.
        assert_eq!(clean_body("abcde", 5), "abcde");
        assert_eq!(clean_body("abcdef", 5), "abcde…");
    }
}
