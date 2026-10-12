//! HTML sanitising and conversion for message bodies.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Sanitised HTML plus whether remote content was removed.
pub struct Sanitised {
    /// Safe HTML.
    pub html: String,
    /// True when a remote resource reference was dropped.
    pub had_remote: bool,
}

const TAGS: &[&str] = &[
    "img",
    "table",
    "tr",
    "td",
    "th",
    "thead",
    "tbody",
    "a",
    "p",
    "br",
    "div",
    "span",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "blockquote",
    "pre",
    "code",
    "em",
    "strong",
    "b",
    "i",
    "u",
    "hr",
    "font",
    "center",
    "del",
    "s",
    "input",
];

const GENERIC_ATTRIBUTES: &[&str] = &[
    "style",
    "class",
    "width",
    "height",
    "align",
    "valign",
    "bgcolor",
    "color",
    "border",
    "cellpadding",
    "cellspacing",
    "colspan",
    "rowspan",
    "title",
    "alt",
    "dir",
];

/// Attributes that load a resource when the browser renders them.
const RESOURCE_ATTRIBUTES: &[&str] = &["src", "srcset", "background", "poster"];

/// Removes active content, event handlers and (unless `allow_remote`)
/// remote resources from `html`. Links open in a new window. `del`, `s` and
/// inert (always `disabled`) checkbox `input`s survive so GFM strikethrough
/// and task lists render.
pub fn sanitize(html: &str, allow_remote: bool) -> Sanitised {
    // Ammonia drops disallowed attributes and schemes before the filter below
    // runs, so look for resource references in the raw input first.
    let had_remote = Arc::new(AtomicBool::new(!allow_remote && has_remote_resource(html)));
    let flag = Arc::clone(&had_remote);

    let mut builder = ammonia::Builder::default();
    builder
        .tags(TAGS.iter().copied().collect())
        .clean_content_tags(["script", "style"].into_iter().collect())
        .generic_attributes(GENERIC_ATTRIBUTES.iter().copied().collect())
        .tag_attributes(HashMap::from([
            ("a", HashSet::from(["href"])),
            ("img", HashSet::from(["src"])),
            // Task-list checkboxes only; `type` is limited to "checkbox" below.
            ("input", HashSet::from(["checked", "disabled"])),
        ]))
        .tag_attribute_values(HashMap::from([(
            "input",
            HashMap::from([("type", HashSet::from(["checkbox"]))]),
        )]))
        .set_tag_attribute_value("input", "disabled", "")
        .url_schemes(
            ["http", "https", "mailto", "cid", "data"]
                .into_iter()
                .collect(),
        )
        .link_rel(Some("noopener noreferrer"))
        .attribute_filter(move |_element, attribute, value| {
            if allow_remote {
                return Some(value.into());
            }
            if RESOURCE_ATTRIBUTES.contains(&attribute) {
                if is_local_resource(value) {
                    return Some(value.into());
                }
                flag.store(true, Ordering::Relaxed);
                return None;
            }
            if attribute == "style" {
                let (kept, dropped) = strip_style_urls(value);
                if dropped {
                    flag.store(true, Ordering::Relaxed);
                }
                return (!kept.is_empty()).then(|| kept.into());
            }
            Some(value.into())
        });

    // `attribute_filter` cannot add attributes, so add the target afterwards.
    // Text can't contain a literal "<a " (it is escaped), so this only hits tags.
    let html = builder
        .clean(html)
        .to_string()
        .replace("<a ", "<a target=\"_blank\" ");

    Sanitised {
        html,
        had_remote: had_remote.load(Ordering::Relaxed),
    }
}

/// Reader-mode variant: `sanitize`, then strip the sender's presentation
/// (inline styles, classes, colours, sizes, alignment, `font`/`center`) so
/// the viewer's own stylesheet governs typography while the document
/// structure (headings, lists, tables, links, images) survives intact.
pub fn reader(html: &str, allow_remote: bool) -> Sanitised {
    let safe = sanitize(html, allow_remote);
    let mut builder = ammonia::Builder::default();
    builder
        .tags(
            TAGS.iter()
                .copied()
                .filter(|t| !matches!(*t, "font" | "center"))
                .collect(),
        )
        .generic_attributes(
            ["colspan", "rowspan", "title", "alt", "dir"]
                .into_iter()
                .collect(),
        )
        .tag_attributes(HashMap::from([
            ("a", HashSet::from(["href", "target"])),
            ("img", HashSet::from(["src"])),
            ("input", HashSet::from(["type", "checked", "disabled"])),
        ]))
        .url_schemes(
            ["http", "https", "mailto", "cid", "data"]
                .into_iter()
                .collect(),
        )
        .link_rel(Some("noopener noreferrer"));
    Sanitised {
        html: builder.clean(&safe.html).to_string(),
        had_remote: safe.had_remote,
    }
}

/// True when a tag in `html` has a resource attribute whose value is not
/// `cid:` or `data:image/`.
///
/// Detection only: the sanitiser output never depends on this. It is a
/// tolerant scan that follows the HTML tag syntax (case-insensitive names,
/// `/` between attributes, quoted or bare values) and does not special-case
/// comments or raw-text elements, so it may flag more than ammonia drops,
/// never less for markup ammonia would parse as a tag.
fn has_remote_resource(html: &str) -> bool {
    let mut rest = html;

    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];

        if !rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
            continue;
        }

        let name_end = rest
            .find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')
            .unwrap_or(rest.len());

        rest = &rest[name_end..];

        loop {
            rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');

            if rest.is_empty() || rest.starts_with('>') {
                break;
            }

            // The first character is always part of the name, even `=`; step
            // over all of its bytes so the slice stays on a char boundary.
            let first = rest.chars().next().map_or(0, char::len_utf8);
            let name_end = rest[first..]
                .find(|c: char| c.is_ascii_whitespace() || matches!(c, '=' | '/' | '>'))
                .map_or(rest.len(), |i| i + first);
            let name = rest[..name_end].to_ascii_lowercase();

            rest = rest[name_end..].trim_start_matches(|c: char| c.is_ascii_whitespace());

            let mut value = "";

            if let Some(after) = rest.strip_prefix('=') {
                let after = after.trim_start_matches(|c: char| c.is_ascii_whitespace());
                let (v, tail) = match after.chars().next() {
                    Some(q @ ('"' | '\'')) => {
                        let body = &after[1..];

                        body.find(q)
                            .map_or((body, ""), |i| (&body[..i], &body[i + 1..]))
                    }
                    _ => {
                        let end = after
                            .find(|c: char| c.is_ascii_whitespace() || c == '>')
                            .unwrap_or(after.len());

                        (&after[..end], &after[end..])
                    }
                };

                value = v;
                rest = tail;
            }

            if RESOURCE_ATTRIBUTES.contains(&name.as_str())
                && !is_local_resource(&decode_char_refs(value))
            {
                return true;
            }
        }
    }

    false
}

/// Decodes the character references that can spell a `cid:` or
/// `data:image/` prefix: numeric ones (`&#105;`, `&#x69;`) and `&colon;`,
/// `&sol;`, `&Tab;`, `&NewLine;`, `&amp;`. Anything else stays as written,
/// which can only make the value look remote.
fn decode_char_refs(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];

        let (decoded, len) = decode_char_ref(rest).unwrap_or(('&', 1));

        out.push(decoded);
        rest = &rest[len..];
    }

    out.push_str(rest);

    out
}

/// Decodes the reference at the start of `s` (which begins with `&`) and
/// returns the character and the byte length consumed.
fn decode_char_ref(s: &str) -> Option<(char, usize)> {
    const NAMED: &[(&str, char)] = &[
        ("&colon;", ':'),
        ("&sol;", '/'),
        ("&Tab;", '\t'),
        ("&NewLine;", '\n'),
        ("&amp;", '&'),
    ];

    if let Some(&(name, c)) = NAMED.iter().find(|(name, _)| s.starts_with(name)) {
        return Some((c, name.len()));
    }

    let num = s.strip_prefix("&#")?;
    let (radix, digits) = match num.strip_prefix(['x', 'X']) {
        Some(hex) => (16, hex),
        None => (10, num),
    };
    let count = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    let c = char::from_u32(u32::from_str_radix(&digits[..count], radix).ok()?)?;
    let prefix = s.len() - digits.len();
    let semi = usize::from(digits[count..].starts_with(';'));

    Some((c, prefix + count + semi))
}

/// True for the only resource values allowed without `allow_remote`.
fn is_local_resource(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();

    lower.starts_with("cid:") || lower.starts_with("data:image/")
}

/// Drops every CSS declaration that could load a resource. Escapes and
/// comments are dropped too since they can hide a `url(`.
fn strip_style_urls(style: &str) -> (String, bool) {
    let mut dropped = false;
    let kept: Vec<&str> = style
        .split(';')
        .filter(|decl| {
            let lower = decl.to_ascii_lowercase();
            let remote = ["url", "image-set", "\\", "/*"]
                .iter()
                .any(|needle| lower.contains(needle));
            dropped |= remote;
            !remote
        })
        .collect();
    (kept.join(";"), dropped)
}

/// Renders HTML as plain text wrapped at 78 columns.
pub fn to_text(html: &str) -> String {
    match html2text::from_read(html.as_bytes(), 78) {
        Ok(text) => text,
        Err(_) => strip_tags(html),
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Escapes `&`, `<`, `>`, `"` and `'` for HTML text and attribute values.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_strips_presentation_keeps_structure() {
        let src = r##"<table style="width:600px" bgcolor="#fff" cellpadding="4"><tr><td align="left" style="color:red"><font color="blue">Electricity</font></td><td colspan="2">$18</td></tr></table><center><h3 class="x">Hi</h3></center><script>x()</script>"##;
        let out = reader(src, false).html;
        assert!(
            out.contains("<table><tbody><tr><td>Electricity</td><td colspan=\"2\">$18</td></tr></tbody></table>"),
            "{out}"
        );
        assert!(out.contains("<h3>Hi</h3>"), "{out}");
        assert!(
            !out.contains("style=")
                && !out.contains("bgcolor")
                && !out.contains("<font")
                && !out.contains("<script"),
            "{out}"
        );
        let img = reader(r#"<img src="https://x/y.png" width="1">"#, false);
        assert!(img.had_remote && !img.html.contains("src="), "{}", img.html);
    }

    #[test]
    fn sanitize_strips_script_and_handlers() {
        let out = sanitize(
            r#"<p onclick="evil()">Hello</p><script>alert(1)</script><a href="javascript:evil()">link</a>"#,
            false,
        )
        .html;
        assert!(out.contains("Hello"));
        assert!(out.contains("link"));
        assert!(!out.contains("script"));
        assert!(!out.contains("alert"));
        assert!(!out.contains("onclick"));
        assert!(!out.contains("javascript"));
    }

    #[test]
    fn sanitize_blocks_remote_images_by_default() {
        for src in ["https://x/y.png", "//x/y.png", "HTTP://x/y.png"] {
            let s = sanitize(&format!(r#"<img src="{src}">"#), false);
            assert!(!s.html.contains("src"), "{src} kept: {}", s.html);
            assert!(s.had_remote, "{src} not flagged");
        }

        for src in ["cid:abc", "data:image/png;base64,AA=="] {
            let s = sanitize(&format!(r#"<img src="{src}">"#), false);
            assert!(s.html.contains(src), "{src} dropped: {}", s.html);
            assert!(!s.had_remote);
        }

        let s = sanitize(r#"<img src="https://x/y.png">"#, true);
        assert!(s.html.contains("https://x/y.png"));
        assert!(!s.had_remote);
    }

    #[test]
    fn sanitize_strips_remote_css_url() {
        let s = sanitize(
            r#"<p style="background:url(https://x/a.png);color:red">a</p>"#,
            false,
        );
        assert!(s.html.contains("color:red"), "{}", s.html);
        assert!(!s.html.contains("url"), "{}", s.html);
        assert!(s.had_remote);

        for style in [
            "background: URL ( //x/a.png )",
            "background:u\\72 l(https://x/a.png)",
        ] {
            let s = sanitize(&format!(r#"<p style="{style}">a</p>"#), false);
            assert!(!s.html.contains("background"), "{style}: {}", s.html);
            assert!(s.had_remote);
        }
    }

    #[test]
    fn sanitize_flags_resources_ammonia_drops_first() {
        for html in [
            r#"<img srcset="https://example.com/pixel 1x">"#,
            r#"<img src="ftp://example.com/pixel">"#,
            r#"<td background="https://example.com/b.png">x</td>"#,
            r#"<img poster="//example.com/p.png">"#,
            "<IMG SRC=//example.com/y.png>",
            r#"<img/alt="a"/src='http://example.com/y.png'>"#,
            r#"<img src=" ">"#,
        ] {
            let s = sanitize(html, false);
            assert!(s.had_remote, "{html} not flagged: {}", s.html);
            for attr in ["src", "srcset", "background", "poster"] {
                assert!(!s.html.contains(attr), "{html} kept {attr}: {}", s.html);
            }
            assert!(
                !sanitize(html, true).had_remote,
                "{html} flagged when allowed"
            );
        }

        for html in [
            r#"<img src="cid:abc" alt="x">"#,
            r#"<img src=" DATA:image/png;base64,AA==">"#,
            r#"<p title="src=http://example.com">text</p>"#,
            "<p>&lt;img src=\"http://example.com/y.png\"&gt;</p>",
            "<p>a < b and src=http://example.com</p>",
        ] {
            let s = sanitize(html, false);
            assert!(!s.had_remote, "{html} flagged: {}", s.html);
        }
    }

    #[test]
    fn sanitize_keeps_encoded_local_resources() {
        for src in [
            "c&#105;d:abc",
            "&#x63;id:abc",
            "cid&colon;abc",
            "data&#58;image/png;base64,AA==",
            "&#100;ata&colon;image&sol;png;base64,AA==",
        ] {
            let s = sanitize(&format!(r#"<img src="{src}">"#), false);
            assert!(s.html.contains("src="), "{src} dropped: {}", s.html);
            assert!(!s.had_remote, "{src} flagged: {}", s.html);
        }

        for src in ["h&#116;tps://x/y.png", "&#x68;ttp&colon;&sol;&sol;x/y.png"] {
            let s = sanitize(&format!(r#"<img src="{src}">"#), false);
            assert!(!s.html.contains("src"), "{src} kept: {}", s.html);
            assert!(s.had_remote, "{src} not flagged");
        }
    }

    #[test]
    fn sanitize_survives_unicode_markup() {
        for html in [
            "<p é=x>hello</p>",
            "<p\u{a0}src=x>hello</p>",
            "<p \u{a0}é=\"ü\" title=ü>hello</p>",
            "<p 💥=1 💥>hello</p>",
            "<p =é>hello</p>",
            "<p é",
            "<p é=",
            "<p é='",
            "<p title=é",
            "<é src=http://x/y.png>hello",
            "<💥>hello</💥>",
            "é<",
            "<é",
            "<",
        ] {
            let s = sanitize(html, false);
            assert!(!s.had_remote, "{html:?} flagged: {}", s.html);
            assert!(
                !s.html.contains('<') || s.html.contains("hello"),
                "{html:?}: {}",
                s.html
            );
        }

        let s = sanitize("<p é=x>hello</p>", false);
        assert_eq!(s.html, "<p>hello</p>");

        // A non-ASCII attribute name does not hide a following remote image.
        for html in [
            "<img é=1 src=http://x/y.png>",
            "<img\u{a0}é=1 src='http://x/y.png'>",
            "<img 💥 src=\"http://x/y.png\">",
            "<p é=\"ü\"><img src=http://x/y.png>",
        ] {
            let s = sanitize(html, false);
            assert!(s.had_remote, "{html:?} not flagged: {}", s.html);
            assert!(!s.html.contains("src"), "{html:?} kept src: {}", s.html);
        }
    }

    #[test]
    fn sanitize_keeps_inert_checkboxes_and_strikethrough() {
        let s = sanitize(
            r#"<del>a</del><s>b</s><input type="checkbox" checked name="n" onclick="x()"><input type="text" value="v">"#,
            false,
        );
        assert!(s.html.contains("<del>a</del>"), "{}", s.html);
        assert!(s.html.contains("<s>b</s>"), "{}", s.html);
        assert!(s.html.contains(r#"type="checkbox""#), "{}", s.html);
        assert!(s.html.contains("checked"), "{}", s.html);
        assert!(!s.html.contains("name="), "{}", s.html);
        assert!(!s.html.contains("onclick"), "{}", s.html);
        assert!(!s.html.contains("value="), "{}", s.html);
        assert!(!s.html.contains(r#"type="text""#), "{}", s.html);
        assert_eq!(s.html.matches("disabled").count(), 2, "{}", s.html);
    }

    #[test]
    fn links_open_in_new_window() {
        let out = sanitize(r#"<a href="https://example.com">x</a>"#, false).html;
        assert!(out.contains(r#"target="_blank""#), "{out}");
        assert!(out.contains(r#"rel="noopener noreferrer""#), "{out}");
    }

    #[test]
    fn to_text_and_escape() {
        let text = to_text("<p>Hi<br>there</p>");
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        assert!(lines.contains(&"Hi"), "{text:?}");
        assert!(lines.contains(&"there"), "{text:?}");
        assert_eq!(escape("<a&b>"), "&lt;a&amp;b&gt;");
        assert_eq!(escape(r#""it's""#), "&quot;it&#39;s&quot;");
    }
}
