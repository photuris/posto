//! Compose and reply modal.

use dioxus::prelude::*;
use posto_core::compose::{self, Draft};
use posto_core::provider::Identity;
use posto_core::store::Address;

use crate::app::{
    COMPOSER, COMPOSER_CC, COMPOSER_TO, IDENTITIES, STATUS, backend, close_composer, send,
};
use crate::components::{focus_in, focus_out, frame_doc};

/// Edits the open draft.
fn edit(f: impl FnOnce(&mut Draft)) {
    if let Some(draft) = COMPOSER.write().as_mut() {
        f(draft);
    }
}

/// A sending identity of one account, as a From option.
#[derive(Clone, PartialEq)]
struct FromOption {
    account_id: String,
    identity: Identity,
    /// What the option shows.
    label: String,
}

impl FromOption {
    /// The `<select>` value: `<account>/<email>` (account ids hold no slash).
    fn value(&self) -> String {
        format!("{}/{}", self.account_id, self.identity.email)
    }
}

/// The From choices: the identities of every enabled account, or only the
/// draft's own account when it replies (a reply never changes account).
fn from_options(draft: &Draft) -> Vec<FromOption> {
    let cfg = backend().read().cfg.clone();
    let identities = IDENTITIES();
    let several = cfg.accounts_enabled().count() > 1;

    cfg.accounts_enabled()
        .filter(|a| draft.reply_to_email_id.is_none() || a.id == draft.account_id)
        .flat_map(|a| {
            identities
                .get(&a.id)
                .into_iter()
                .flatten()
                .map(|i| FromOption {
                    account_id: a.id.clone(),
                    identity: i.clone(),
                    label: if several {
                        format!("{} <{}> — {}", i.name, i.email, a.display_name())
                    } else {
                        format!("{} <{}>", i.name, i.email)
                    },
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Modal over the reader: headers, body, Markdown toggle with preview, Close / Send.
#[component]
pub fn Composer() -> Element {
    let draft = COMPOSER().unwrap_or_default();
    let account = backend()
        .read()
        .cfg
        .account(&draft.account_id)
        .map(|a| a.email.clone())
        .unwrap_or_default();
    let options = from_options(&draft);
    // The same default `compose::send` applies when `from` is unset.
    let selected_email = draft
        .from
        .as_ref()
        .map(|a| a.email.clone())
        .or_else(|| {
            let own = || options.iter().filter(|o| o.account_id == draft.account_id);

            own()
                .find(|o| o.identity.email.eq_ignore_ascii_case(&account))
                .or(own().next())
                .map(|o| o.identity.email.clone())
        })
        .unwrap_or_default();
    let selected = format!("{}/{}", draft.account_id, selected_email.to_lowercase());
    let from = if account.is_empty() {
        "Default Fastmail address".to_string()
    } else {
        account
    };
    let preview = draft
        .markdown
        .then(|| frame_doc(&compose::render_markdown(&draft.body)));
    let focus_body = !draft.to.is_empty();
    let picker = options.clone();

    rsx! {
        div { class: "overlay",
            div { class: "composer",
                div { class: "composer-title", "{draft.subject}" }
                div { class: "field",
                    label { "From:" }
                    if options.is_empty() {
                        span { class: "static", "{from}" }
                    } else {
                        select {
                            onchange: move |evt| {
                                let value = evt.value();
                                let picked = picker.iter().find(|o| o.value() == value).cloned();

                                if let Some(o) = picked {
                                    edit(|d| {
                                        d.account_id = o.account_id.clone();
                                        d.from = Some(Address {
                                            name: (!o.identity.name.is_empty())
                                                .then(|| o.identity.name.clone()),
                                            email: o.identity.email.clone(),
                                        });
                                    });
                                }
                            },
                            for o in options.iter() {
                                option {
                                    key: "{o.value()}",
                                    value: "{o.value()}",
                                    selected: o.value().to_lowercase() == selected,
                                    "{o.label}"
                                }
                            }
                        }
                    }
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
