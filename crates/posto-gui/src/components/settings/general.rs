//! General page: authentication, sending and refresh.

use dioxus::prelude::*;
use posto_core::config::{Config, Secrets};

use crate::app::{NEEDS_TOKEN, backend, set_status, sync_soon};
use crate::components::{focus_in, focus_out};

#[component]
pub fn GeneralPage(mut draft: Signal<Config>) -> Element {
    let ui_test = backend().peek().ui_test;
    let mut token_open = use_signal(|| false);
    let mut token_value = use_signal(String::new);
    let mut token_set = use_signal(|| !ui_test && Secrets::token().ok().flatten().is_some());
    let mut token_error = use_signal(|| None::<String>);

    rsx! {
        h3 { "Authentication" }
        div { class: "setting-row",
            span { "Fastmail API token" }
            span { class: "muted",
                if token_set() { "Token configured" } else { "No token configured" }
            }
            button { onclick: move |_| token_open.set(!token_open()), "Set Token…" }
        }
        if token_open() {
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
                    onclick: move |_| {
                        let token = token_value().trim().to_string();

                        if token.is_empty() {
                            return token_error.set(Some("Enter a token first".into()));
                        }

                        if backend().peek().ui_test {
                            set_status("UI test mode: the token is not stored.");
                            token_open.set(false);
                            return;
                        }

                        match Secrets::set_token(&token) {
                            Ok(()) => {
                                token_set.set(true);
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
                    },
                    "Save"
                }
            }
        }
        if let Some(e) = token_error() {
            div { class: "error", "{e}" }
        }
        div { class: "hint", "Stored in secrets.toml in the Posto data folder (mode 0600)." }

        h3 { "Sending" }
        div { class: "setting-row",
            label { "Sender Email" }
            input {
                r#type: "text",
                value: "{draft.read().account.email}",
                onfocusin: focus_in,
                onfocusout: focus_out,
                oninput: move |evt| draft.write().account.email = evt.value(),
            }
        }
        div { class: "hint", "Replies and drafts use the Fastmail identity matching this address." }
        div { class: "setting-row",
            label { "Session URL" }
            input {
                r#type: "text",
                value: "{draft.read().account.session_url}",
                onfocusin: focus_in,
                onfocusout: focus_out,
                oninput: move |evt| draft.write().account.session_url = evt.value(),
            }
        }

        h3 { "Refresh" }
        div { class: "setting-row",
            label { "Refresh every" }
            input {
                class: "number",
                r#type: "number",
                min: "0",
                value: "{draft.read().sync.refresh_minutes}",
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
