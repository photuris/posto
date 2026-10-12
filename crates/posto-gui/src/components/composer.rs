//! Compose and reply modal.

use dioxus::prelude::*;
use posto_core::compose::{self, Draft};

use crate::app::{COMPOSER, COMPOSER_CC, COMPOSER_TO, STATUS, backend, close_composer, send};
use crate::components::{focus_in, focus_out, frame_doc};

/// Edits the open draft.
fn edit(f: impl FnOnce(&mut Draft)) {
    if let Some(draft) = COMPOSER.write().as_mut() {
        f(draft);
    }
}

/// Modal over the reader: headers, body, Markdown toggle with preview, Close / Send.
#[component]
pub fn Composer() -> Element {
    let draft = COMPOSER().unwrap_or_default();
    let from = {
        let email = backend().read().cfg.account.email.clone();

        if email.is_empty() {
            "Default Fastmail address".to_string()
        } else {
            email
        }
    };
    let preview = draft
        .markdown
        .then(|| frame_doc(&compose::render_markdown(&draft.body)));
    let focus_body = !draft.to.is_empty();

    rsx! {
        div { class: "overlay",
            div { class: "composer",
                div { class: "composer-title", "{draft.subject}" }
                div { class: "field",
                    label { "From:" }
                    span { class: "static", "{from}" }
                }
                div { class: "field",
                    label { "To:" }
                    input {
                        id: "compose-to",
                        r#type: "text",
                        value: "{COMPOSER_TO}",
                        onmounted: move |evt| async move {
                            if !focus_body {
                                let _ = evt.set_focus(true).await;
                            }
                        },
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: |evt| *COMPOSER_TO.write() = evt.value(),
                    }
                }
                div { class: "field",
                    label { "Cc:" }
                    input {
                        r#type: "text",
                        value: "{COMPOSER_CC}",
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: |evt| *COMPOSER_CC.write() = evt.value(),
                    }
                }
                div { class: "field",
                    label { "Subject:" }
                    input {
                        r#type: "text",
                        value: "{draft.subject}",
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: |evt| edit(|d| d.subject = evt.value()),
                    }
                }
                div { class: if preview.is_some() { "composer-body split" } else { "composer-body" },
                    textarea {
                        id: "compose-body",
                        value: "{draft.body}",
                        onmounted: move |evt| async move {
                            if focus_body {
                                let _ = evt.set_focus(true).await;
                            }
                        },
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: |evt| edit(|d| d.body = evt.value()),
                    }
                    if let Some(doc) = &preview {
                        iframe { class: "preview-frame", "sandbox": "", srcdoc: "{doc}" }
                    }
                }
                div { class: "composer-foot",
                    label { class: "switch",
                        input {
                            r#type: "checkbox",
                            checked: draft.markdown,
                            onchange: |evt| edit(|d| d.markdown = evt.checked()),
                        }
                        " Markdown"
                    }
                    span { class: "composer-status", {STATUS()} }
                    button { onclick: |_| close_composer(), "Close" }
                    button { title: "Send (ctrl+enter)", onclick: |_| send(false), "Send" }
                    button { class: "primary", onclick: |_| send(true), "Send + Archive" }
                }
            }
        }
    }
}
