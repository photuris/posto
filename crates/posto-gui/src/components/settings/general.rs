//! General page: accounts (token, sender, endpoint) and refresh.

use dioxus::prelude::*;
use posto_core::config::{Account, Config, Secrets};

use crate::app::{NEEDS_TOKEN, backend, set_status, sync_soon};
use crate::components::{focus_in, focus_out};

/// The first `account-N` id (N from 2) that no account of `cfg` uses.
fn fresh_id(cfg: &Config) -> String {
    (2..)
        .map(|n| format!("account-{n}"))
        .find(|id| cfg.account(id).is_none())
        .unwrap_or_default()
}

/// Edits the selected account of the draft; a stale index is ignored.
fn edit(mut draft: Signal<Config>, at: usize, f: impl FnOnce(&mut Account)) {
    if let Some(account) = draft.write().accounts.get_mut(at) {
        f(account);
    }
}

#[component]
pub fn GeneralPage(mut draft: Signal<Config>) -> Element {
    let ui_test = backend().peek().ui_test;
    let mut selected = use_signal(|| 0usize);
    let mut token_open = use_signal(|| false);
    let mut token_value = use_signal(String::new);
    let mut token_rev = use_signal(|| 0u32);
    let mut token_error = use_signal(|| None::<String>);
    let mut confirm_remove = use_signal(|| false);

    // Re-read the token state after a save.
    let _ = token_rev();

    let cfg = draft();
    let at = selected().min(cfg.accounts.len().saturating_sub(1));
    let Some(account) = cfg.accounts.get(at).cloned() else {
        return rsx! {};
    };
    // A new account has no secrets entry or cached mail until Save.
    let saved = backend().peek().cfg.account(&account.id).is_some();
    let token_set = !ui_test
        && saved
        && Secrets::token_for(&backend().peek().cfg, &account.id)
            .ok()
            .flatten()
            .is_some();
    let removable = cfg.accounts.len() > 1;

    rsx! {
        h3 { "Accounts" }
        div { class: "accounts",
            div { class: "account-list",
                for (i , a) in cfg.accounts.iter().enumerate() {
                    button {
                        key: "{i}",
                        class: if i == at { "account-item active" } else { "account-item" },
                        onclick: move |_| {
                            selected.set(i);
                            token_open.set(false);
                            confirm_remove.set(false);
                        },
                        "{a.display_name()}"
                        if !a.enabled {
                            span { class: "muted", " (off)" }
                        }
                    }
                }
                button {
                    class: "account-item",
                    onclick: move |_| {
                        let id = fresh_id(&draft.peek());
                        let mut d = draft.write();

                        d.accounts.push(Account::new(&id, ""));
                        selected.set(d.accounts.len() - 1);
                        token_open.set(false);
                        confirm_remove.set(false);
                    },
                    "+ Add account"
                }
            }
            div { class: "account-form",
                div { class: "setting-row",
                    label { "Name" }
                    input {
                        r#type: "text",
                        value: "{account.name}",
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: move |evt| edit(draft, at, |a| a.name = evt.value()),
                    }
                }
                div { class: "setting-row",
                    label { "Sender Email" }
                    input {
                        r#type: "text",
                        value: "{account.email}",
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: move |evt| edit(draft, at, |a| a.email = evt.value()),
                    }
                }
                div { class: "hint", "Replies and drafts use the Fastmail identity matching this address." }
                div { class: "setting-row",
                    label { "Session URL" }
                    input {
                        r#type: "text",
                        value: "{account.session_url}",
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: move |evt| edit(draft, at, |a| a.session_url = evt.value()),
                    }
                }
                div { class: "setting-row",
                    label { "Account id" }
                    if saved {
                        span { class: "muted", "{account.id}" }
                    } else {
                        input {
                            r#type: "text",
                            value: "{account.id}",
                            onfocusin: focus_in,
                            onfocusout: focus_out,
                            oninput: move |evt| edit(draft, at, |a| a.id = evt.value()),
                        }
                    }
                }
                if !saved {
                    div { class: "hint",
                        "Lower-case letters, digits and '-'. Save the settings, then set the token."
                    }
                }
                div { class: "setting-row",
                    label { "Enabled" }
                    input {
                        r#type: "checkbox",
                        checked: account.enabled,
                        onchange: move |evt| edit(draft, at, |a| a.enabled = evt.checked()),
                    }
                    span { class: "muted", "Disabled accounts are not synced or shown." }
                }

                div { class: "setting-row",
                    span { "Fastmail API token" }
                    span { class: "muted",
                        if token_set { "Token configured" } else { "No token configured" }
                    }
                    button {
                        disabled: !saved,
                        onclick: move |_| token_open.set(!token_open()),
                        "Set Token…"
                    }
                }
                if token_open() && saved {
                    div { class: "setting-row",
                        input {
                            r#type: "password",
                            placeholder: "Paste API token",
                            value: "{token_value}",
                            onmounted: |evt| async move {
                                let _ = evt.set_focus(true).await;
                            },
                            onfocusin: focus_in,
                            onfocusout: focus_out,
                            oninput: move |evt| token_value.set(evt.value()),
                        }
                        button {
                            onclick: {
                                let id = account.id.clone();

                                move |_| {
                                    let token = token_value().trim().to_string();

                                    if token.is_empty() {
                                        return token_error.set(Some("Enter a token first".into()));
                                    }

                                    if backend().peek().ui_test {
                                        set_status("UI test mode: the token is not stored.");
                                        token_open.set(false);
                                        return;
                                    }

                                    match Secrets::set_token(&id, &token) {
                                        Ok(()) => {
                                            token_rev += 1;
                                            token_error.set(None);
                                            token_open.set(false);
                                            token_value.set(String::new());
                                            *NEEDS_TOKEN.write() = false;

                                            let mut shared = backend();

                                            shared.write().reset_client();
                                            sync_soon();
                                        }
                                        Err(e) => token_error.set(Some(e.to_string())),
                                    }
                                }
                            },
                            "Save"
                        }
                    }
                }
                if let Some(e) = token_error() {
                    div { class: "error", "{e}" }
                }
                div { class: "hint", "Stored in secrets.toml in the Posto data folder (mode 0600)." }

                div { class: "setting-row",
                    if confirm_remove() {
                        button {
                            class: "danger",
                            onclick: move |_| {
                                let mut d = draft.write();

                                if at < d.accounts.len() && d.accounts.len() > 1 {
                                    d.accounts.remove(at);
                                }

                                selected.set(0);
                                confirm_remove.set(false);
                            },
                            "Remove {account.display_name()} and its cached mail"
                        }
                        button { onclick: move |_| confirm_remove.set(false), "Cancel" }
                    } else {
                        button {
                            disabled: !removable,
                            title: if removable { "" } else { "The last account cannot be removed" },
                            onclick: move |_| confirm_remove.set(true),
                            "Remove Account…"
                        }
                    }
                }
            }
        }

        h3 { "Refresh" }
        div { class: "setting-row",
            label { "Refresh every" }
            input {
                class: "number",
                r#type: "number",
                min: "0",
                value: "{cfg.sync.refresh_minutes}",
                onfocusin: focus_in,
                onfocusout: focus_out,
                oninput: move |evt| {
                    if let Ok(n) = evt.value().trim().parse::<u32>() {
                        draft.write().sync.refresh_minutes = n;
                    }
                },
            }
            span { "minutes" }
        }
        div { class: "hint", "Set to 0 to disable background refresh." }
    }
}
