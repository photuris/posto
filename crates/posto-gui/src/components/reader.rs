//! Right pane: the selected message.

use dioxus::prelude::*;
use posto_core::actions;
use posto_core::html;
use posto_core::shortcuts::Action;
use posto_core::store::{Address, EmailSummary};

use crate::app::{
    ALLOW_REMOTE, BODY, BODY_ERROR, EMAILS, MAILBOXES, READER_MODE, SELECTION, backend, dispatch,
    mark_selected, save_attachment, selected_ref, trust_domain,
};
use crate::components::{Icon, frame_doc, reader_frame_doc};
use crate::format;

fn people(list: &[Address]) -> String {
    list.iter().map(format::who).collect::<Vec<_>>().join(", ")
}

/// Subject, sender, remote-content bar, sandboxed body and attachments.
#[component]
pub fn Reader() -> Element {
    // Sanitised on each change of body or the allow-remote switch, not per render.
    let sanitised = use_memo(|| {
        BODY()
            .and_then(|(_, b)| b.html)
            .map(|h| html::sanitize(&h, ALLOW_REMOTE()))
            .map(|s| (s.html, s.had_remote))
    });
    let picked = SELECTION.read().len();

    if picked > 1 {
        return rsx! {
            div { class: "reader empty",
                div { class: "bulk-panel",
                    h2 { "{picked} messages selected" }
                    div { class: "bulk-buttons",
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| dispatch(Action::Archive),
                            "Archive"
                        }
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| dispatch(Action::Delete),
                            "Delete"
                        }
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| dispatch(Action::Junk),
                            "Junk"
                        }
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| dispatch(Action::Move),
                            "Move…"
                        }
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| dispatch(Action::Label),
                            Icon { name: "tag" }
                            " Labels…"
                        }
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| dispatch(Action::Classify),
                            "Classify as…"
                        }
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| mark_selected(actions::Action::MarkRead),
                            "Mark read"
                        }
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| mark_selected(actions::Action::MarkUnread),
                            "Mark unread"
                        }
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| mark_selected(actions::Action::Flag),
                            "Flag"
                        }
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| mark_selected(actions::Action::Unflag),
                            "Unflag"
                        }
                    }
                    button {
                        class: "link",
                        onmousedown: |evt| evt.prevent_default(),
                        onclick: |_| dispatch(Action::Escape),
                        "Clear selection"
                    }
                }
            }
        };
    }

    let Some(r) = selected_ref() else {
        return rsx! {
            div { class: "reader empty", "Select a message" }
        };
    };
    let listed = EMAILS().into_iter().find(|e| e.msg_ref() == r);
    let email: Option<EmailSummary> = listed.or_else(|| {
        backend()
            .peek()
            .store
            .email(&r.account_id, &r.id)
            .ok()
            .flatten()
    });
    let Some(email) = email else {
        return rsx! {
            div { class: "reader empty", "Select a message" }
        };
    };
    let body = BODY().map(|(_, body)| body);
    let reader_mode = READER_MODE();
    let allow_remote = ALLOW_REMOTE();
    let sender = email.from.first().cloned();
    let name = sender
        .as_ref()
        .map_or("(unknown)".to_string(), |a| format::who(a).to_string());
    let address = sender.as_ref().map(|a| a.email.clone()).unwrap_or_default();
    let hue = format::avatar_hue(&address);
    let domain = address
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let chips: Vec<String> = MAILBOXES()
        .into_iter()
        .filter(|m| m.account_id == email.account_id && email.mailbox_ids.contains(&m.id))
        .map(|m| m.name)
        .collect();
    let (html_doc, had_remote) = match sanitised() {
        Some((h, remote)) => (Some(frame_doc(&h)), remote),
        None => (None, false),
    };
    // Reader mode re-renders the HTML with the sender's presentation stripped and our own
    // serif stylesheet, so tables and rules keep their layout instead of being flattened
    // to wrapped text. Plain-text mail still shows as text.
    let reader_doc = body
        .as_ref()
        .and_then(|b| b.html.as_deref())
        .map(|h| reader_frame_doc(&html::reader(h, allow_remote).html));
    let text = body.as_ref().and_then(|b| {
        b.text
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| b.html.as_deref().map(html::to_text))
    });
    let attachments = body.map(|b| b.attachments).unwrap_or_default();

    rsx! {
        div { class: "reader",
            div { class: "reader-head",
                h1 { class: "reader-subject", "{email.subject}" }
                div { class: "reader-meta",
                    div {
                        class: "avatar big",
                        style: "background: hsl({hue} 55% 45%)",
                        {format::initials(&name)}
                    }
                    div { class: "reader-from",
                        div {
                            span { class: "from-name", "{name}" }
                            " "
                            span { class: "from-email", "{address}" }
                        }
                        div { class: "to-line", "To: {people(&email.to)}" }
                        if !email.cc.is_empty() {
                            div { class: "to-line", "Cc: {people(&email.cc)}" }
                        }
                        div { class: "chips",
                            for chip in chips {
                                span { key: "{chip}", class: "chip", style: "--hue: {format::avatar_hue(&chip)}", "{chip}" }
                            }
                        }
                    }
                    div { class: "reader-side",
                        div { class: "long-date", {format::long_date(&email.received_at)} }
                        button {
                            class: if reader_mode { "link active" } else { "link" },
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| {
                                let on = *READER_MODE.peek();

                                *READER_MODE.write() = !on;
                            },
                            title: "Reader Mode",
                            Icon { name: "book-open" }
                            " Reader Mode"
                        }
                        button {
                            class: "link",
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| dispatch(Action::Label),
                            title: "Add or remove labels",
                            Icon { name: "tag" }
                            " Labels…"
                        }
                        button {
                            class: "link",
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: |_| dispatch(Action::Classify),
                            title: "File this message under a classifier tray",
                            Icon { name: "flag" }
                            " Classify as…"
                        }
                    }
                }
            }
            if had_remote && !allow_remote && !reader_mode {
                div { class: "remote-bar",
                    span { class: "grow", "This message contains remote content." }
                    button {
                        onmousedown: |evt| evt.prevent_default(),
                        onclick: |_| *ALLOW_REMOTE.write() = true,
                        "Load Remote Content"
                    }
                    if !domain.is_empty() {
                        button {
                            onmousedown: |evt| evt.prevent_default(),
                            onclick: {
                                let domain = domain.clone();

                                move |_| trust_domain(&domain)
                            },
                            "Always from {domain}"
                        }
                    }
                }
            }
            div { id: "reader-body", class: "reader-body", tabindex: "-1",
                if let Some(error) = BODY_ERROR() {
                    div { class: "empty", "{error}" }
                } else if let Some(doc) = reader_doc.filter(|_| reader_mode) {
                    iframe { class: "mail", "sandbox": "", srcdoc: "{doc}" }
                } else if reader_mode || html_doc.is_none() {
                    match text {
                        Some(text) => rsx! {
                            pre { class: "reader-text", "{text}" }
                        },
                        None => rsx! {
                            div { class: "empty", "Loading…" }
                        },
                    }
                } else if let Some(doc) = html_doc {
                    iframe { class: "mail", "sandbox": "", srcdoc: "{doc}" }
                }
            }
            if !attachments.is_empty() {
                div { class: "attachments",
                    div { class: "attachments-title",
                        Icon { name: "paperclip", size: 14 }
                        " {attachments.len()} "
                        if attachments.len() == 1 { "Attachment" } else { "Attachments" }
                    }
                    div { class: "attachment-list",
                        for att in attachments {
                            button {
                                key: "{att.blob_id}",
                                class: "attachment",
                                title: "Download",
                                onmousedown: |evt| evt.prevent_default(),
                                onclick: {
                                    let att = att.clone();
                                    let account_id = email.account_id.clone();
                                    let email_id = email.id.clone();

                                    move |_| {
                                        save_attachment(
                                            account_id.clone(),
                                            email_id.clone(),
                                            att.clone(),
                                        )
                                    }
                                },
                                span { class: "att-name", {format::clip(&att.name, 34)} }
                                span { class: "att-size", {format::size(att.size)} }
                            }
                        }
                    }
                }
            }
        }
    }
}
