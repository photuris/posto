//! Compose and reply modal.

use std::collections::BTreeMap;

use dioxus::prelude::*;
use posto_core::compose::{self, Draft};
use posto_core::config::Config;
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

/// The From choices: the identities of every enabled account, replies included
/// (a reply may go out from any account).
fn from_options_for(cfg: &Config, identities: &BTreeMap<String, Vec<Identity>>) -> Vec<FromOption> {
    let several = cfg.accounts_enabled().count() > 1;

    cfg.accounts_enabled()
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

/// The option the From list shows selected: the identity of `draft.from`, in the
/// draft's account when it has it there, else in another account; without a
/// `from`, the same default `compose::send` applies (the account's own address,
/// else its first identity). `account_email` is the draft account's address.
fn selected_option<'a>(
    options: &'a [FromOption],
    draft: &Draft,
    account_email: &str,
) -> Option<&'a FromOption> {
    let own = |o: &&FromOption| o.account_id == draft.account_id;
    let email = |o: &&FromOption, email: &str| o.identity.email.eq_ignore_ascii_case(email);

    match &draft.from {
        Some(from) => options
            .iter()
            .filter(own)
            .find(|o| email(o, &from.email))
            .or_else(|| options.iter().find(|o| email(o, &from.email))),
        None => options
            .iter()
            .filter(own)
            .find(|o| email(o, account_email))
            .or_else(|| options.iter().find(own)),
    }
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
    let options = from_options_for(&backend().read().cfg, &IDENTITIES());
    let selected = selected_option(&options, &draft, &account)
        .map(|o| o.value().to_lowercase())
        .unwrap_or_default();
    // A From of another account makes that account the sender.
    use_effect(|| {
        let draft = COMPOSER().unwrap_or_default();
        let own = backend()
            .read()
            .cfg
            .account(&draft.account_id)
            .map(|a| a.email.clone())
            .unwrap_or_default();
        let options = from_options_for(&backend().read().cfg, &IDENTITIES());
        let account = selected_option(&options, &draft, &own).map(|o| o.account_id.clone());

        if let Some(account) = account.filter(|a| *a != draft.account_id) {
            edit(|d| d.account_id = account);
        }
    });
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use dioxus::core::{AttributeValue, Mutation, RuntimeGuard};
    use posto_core::config::Account;
    use posto_core::store::Store;

    use super::*;
    use crate::app::App;
    use crate::state::Backend;

    fn identity(name: &str, email: &str) -> Identity {
        Identity {
            id: email.into(),
            name: name.into(),
            email: email.into(),
        }
    }

    #[test]
    fn reply_offers_every_account() {
        let mut cfg = Config::default();

        cfg.add_account(Account::new("work", "w@x.io")).unwrap();

        let identities = BTreeMap::from([
            ("default".to_string(), vec![identity("Home", "h@x.io")]),
            ("work".to_string(), vec![identity("Work", "w@x.io")]),
        ]);
        let options = from_options_for(&cfg, &identities);
        let accounts: Vec<_> = cfg.accounts_enabled().collect();

        assert_eq!(options.len(), 2);

        for (o, a) in options.iter().zip(&accounts) {
            assert_eq!(o.account_id, a.id);
            assert!(
                o.label.ends_with(&format!(" — {}", a.display_name())),
                "{}",
                o.label
            );
        }

        let single = from_options_for(&Config::default(), &identities);

        assert_eq!(single.len(), 1);
        assert!(!single[0].label.contains(" — "), "{}", single[0].label);
    }

    #[test]
    fn from_of_another_account_is_selected() {
        let options = |accounts: &[(&str, &str)]| {
            accounts
                .iter()
                .map(|(account, email)| FromOption {
                    account_id: account.to_string(),
                    identity: identity("N", email),
                    label: String::new(),
                })
                .collect::<Vec<_>>()
        };
        let from = |email: &str| {
            Some(Address {
                name: None,
                email: email.into(),
            })
        };
        let draft = |account: &str, email: &str| Draft {
            account_id: account.into(),
            from: from(email),
            ..Draft::default()
        };
        let pick = |options: &[FromOption], draft: &Draft| {
            selected_option(options, draft, "").map(FromOption::value)
        };
        let two = options(&[("default", "h@x.io"), ("work", "w@x.io")]);

        // The addressed identity belongs to the other account.
        assert_eq!(
            pick(&two, &draft("default", "W@X.io")).as_deref(),
            Some("work/w@x.io")
        );
        assert_eq!(
            pick(&two, &draft("work", "h@x.io")).as_deref(),
            Some("default/h@x.io")
        );

        // The draft's own account wins when both have the address.
        let shared = options(&[("default", "s@x.io"), ("work", "s@x.io")]);

        assert_eq!(
            pick(&shared, &draft("work", "s@x.io")).as_deref(),
            Some("work/s@x.io")
        );

        // Without a From: the account's own address, else its first identity.
        let own = Draft {
            account_id: "work".into(),
            ..Draft::default()
        };

        assert_eq!(
            selected_option(&two, &own, "w@x.io")
                .map(FromOption::value)
                .as_deref(),
            Some("work/w@x.io")
        );
        assert_eq!(
            selected_option(&two, &own, "other@x.io")
                .map(FromOption::value)
                .as_deref(),
            Some("work/w@x.io")
        );
    }

    /// The real composer, in an app of two accounts, shows the identity of a
    /// `from` that belongs to the other account as selected, and sends from it.
    #[tokio::test(flavor = "current_thread")]
    async fn composer_selects_the_from_of_another_account() {
        let mut cfg = Config::default();

        cfg.add_account(Account::new("work", "w@x.io")).unwrap();

        let store = Arc::new(Store::open_memory().unwrap());

        // The app reloads the identities from the store when it starts.
        store
            .set_identities("default", &[identity("Home", "h@x.io")])
            .unwrap();
        store
            .set_identities("work", &[identity("Work", "w@x.io")])
            .unwrap();

        let mut dom = VirtualDom::new(App).with_root_context(Backend {
            store,
            clients: Default::default(),
            cfg,
            ui_test: true,
            conn_rev: 0,
            config_error: None,
        });

        dom.rebuild_in_place();

        let _guard = RuntimeGuard::new(dom.runtime());

        *COMPOSER.write() = Some(Draft {
            account_id: "default".into(),
            reply_to_email_id: Some("e1".into()),
            reply_to_account_id: Some("default".into()),
            from: Some(Address {
                name: None,
                email: "w@x.io".into(),
            }),
            ..Draft::default()
        });

        let _ = tokio::time::timeout(Duration::from_millis(200), dom.wait_for_work()).await;

        let selected: Vec<_> = dom
            .render_immediate_to_vec()
            .edits
            .into_iter()
            .filter_map(|m| match m {
                Mutation::SetAttribute {
                    name: "selected",
                    value,
                    ..
                } => Some(value),
                _ => None,
            })
            .collect();

        assert_eq!(
            selected,
            [AttributeValue::Bool(false), AttributeValue::Bool(true)]
        );

        // The effect makes the From's account the sender.
        let _ = tokio::time::timeout(Duration::from_millis(200), dom.wait_for_work()).await;

        assert_eq!(COMPOSER.peek().as_ref().unwrap().account_id, "work");
    }
}
