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
/// remote resources from `html`. Links open in a new window.
pub fn sanitize(html: &str, allow_remote: bool) -> Sanitised {
    let had_remote = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&had_remote);

    let mut builder = ammonia::Builder::default();
    builder
        .tags(TAGS.iter().copied().collect())
        .clean_content_tags(["script", "style"].into_iter().collect())
        .generic_attributes(GENERIC_ATTRIBUTES.iter().copied().collect())
        .tag_attributes(HashMap::from([
            ("a", HashSet::from(["href"])),
            ("img", HashSet::from(["src"])),
        ]))
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
                let lower = value.trim_start().to_ascii_lowercase();
                if lower.starts_with("cid:") || lower.starts_with("data:image/") {
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
