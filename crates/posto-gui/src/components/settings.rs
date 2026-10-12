//! Settings modal.

use std::collections::BTreeMap;

use dioxus::prelude::*;
use posto_core::config::{Account, Secrets};
use posto_core::shortcuts::{Action, Chord, Keymap};

use crate::app::{
    KEYMAP, NEEDS_TOKEN, SHOW_SETTINGS, backend, focus_root, reload_list, run, set_status,
    sync_soon,
};
use crate::components::{focus_in, focus_out};
use crate::keys::{self, ALL_ACTIONS};

/// The effective chord text of every action, in settings order.
fn chord_rows(overrides: &BTreeMap<String, String>) -> Vec<(Action, String)> {
    let map = Keymap::from_config(overrides).unwrap_or_else(|_| Keymap::defaults());

    ALL_ACTIONS
        .into_iter()
        .map(|a| {
            let text = map.chord_for(a).map(|c| keys::chord_text(&c));

            (a, text.unwrap_or_default())
        })
        .collect()
}

fn close() {
    *SHOW_SETTINGS.write() = false;
    focus_root();
}

/// Account, token, refresh, blocked senders, remote content and shortcuts.
#[component]
pub fn Settings() -> Element {
    let cfg = backend().peek().cfg.clone();
    let ui_test = backend().peek().ui_test;
    let mut email = use_signal(|| cfg.account.email.clone());
    let mut session_url = use_signal(|| cfg.account.session_url.clone());
    let mut refresh = use_signal(|| cfg.sync.refresh_minutes);
    let mut blocked = use_signal(|| cfg.blocked_senders.clone());
    let mut blocked_input = use_signal(String::new);
    let mut remote_always = use_signal(|| cfg.remote_content == "always");
    let mut trusted = use_signal(|| cfg.trusted_senders.clone());
    let mut trusted_input = use_signal(String::new);
    let mut shortcuts = use_signal(|| chord_rows(&cfg.shortcuts));
    let mut token_open = use_signal(|| false);
    let mut token_value = use_signal(String::new);
    let mut token_set = use_signal(|| !ui_test && Secrets::token().ok().flatten().is_some());
    let mut token_error = use_signal(|| None::<String>);
    let mut save_error = use_signal(|| None::<String>);
    let mut confirm_reset = use_signal(|| false);

    // Parse every chord; a chord shared with another action is an error too.
    let rows = shortcuts();
    let parsed: Vec<Option<Chord>> = rows
        .iter()
        .map(|(_, text)| Keymap::parse_chord(text).ok())
        .collect();
    let errors: Vec<Option<String>> = parsed
        .iter()
        .enumerate()
        .map(|(i, chord)| match chord {
            None => Some("Not a valid shortcut, e.g. ctrl+k".to_string()),
            Some(chord) => parsed
                .iter()
                .enumerate()
                .find(|(j, other)| *j != i && other.as_ref() == Some(chord))
                .map(|(j, _)| format!("Also used by {}", keys::action_label(rows[j].0))),
        })
        .collect();
    let valid = errors.iter().all(Option::is_none);

    let save = move |_| {
        let mut cfg = backend().peek().cfg.clone();
        let url = session_url().trim().to_string();

        cfg.account.email = email().trim().to_string();
        cfg.account.session_url = if url.is_empty() {
            Account::default().session_url
        } else {
            url
        };
        cfg.sync.refresh_minutes = refresh();
        cfg.blocked_senders = blocked();
        cfg.remote_content = if remote_always() { "always" } else { "ask" }.to_string();
        cfg.trusted_senders = trusted();
        cfg.shortcuts = BTreeMap::new();

        let defaults = Keymap::defaults();

        for ((action, _), chord) in shortcuts().iter().zip(&parsed) {
            if let Some(chord) = chord
                && defaults.chord_for(*action).as_ref() != Some(chord)
            {
                cfg.shortcuts
                    .insert(keys::action_name(*action), keys::chord_text(chord));
            }
        }

        let keymap = match Keymap::from_config(&cfg.shortcuts) {
            Ok(keymap) => keymap,
            Err(e) => return save_error.set(Some(e.to_string())),
        };

        // UI test mode runs on fixtures and must not touch the user's files.
        if !backend().peek().ui_test
            && let Err(e) = cfg.save()
        {
            return save_error.set(Some(e.to_string()));
        }

        let mut shared = backend();

        {
            let mut b = shared.write();

            b.cfg = cfg;
            b.reset_client();
        }

        *KEYMAP.write() = keymap;
        sync_soon();
        run(reload_list());
        close();
    };

    rsx! {
        div { class: "overlay", onclick: |_| close(),
            div { class: "settings", onclick: |evt| evt.stop_propagation(),
                div { class: "settings-title", "Posto Settings" }
                div { class: "settings-body",
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
                            value: "{email}",
                            onfocusin: focus_in,
                            onfocusout: focus_out,
                            oninput: move |evt| email.set(evt.value()),
                        }
                    }
                    div { class: "hint", "Replies and drafts use the Fastmail identity matching this address." }
                    div { class: "setting-row",
                        label { "Session URL" }
                        input {
                            r#type: "text",
                            value: "{session_url}",
                            onfocusin: focus_in,
                            onfocusout: focus_out,
                            oninput: move |evt| session_url.set(evt.value()),
                        }
                    }

                    h3 { "Refresh" }
                    div { class: "setting-row",
                        label { "Refresh every" }
                        input {
                            class: "number",
                            r#type: "number",
                            min: "0",
                            value: "{refresh}",
                            onfocusin: focus_in,
                            onfocusout: focus_out,
                            oninput: move |evt| {
                                if let Ok(n) = evt.value().trim().parse::<u32>() {
                                    refresh.set(n);
                                }
                            },
                        }
                        span { "minutes" }
                    }
                    div { class: "hint", "Set to 0 to disable background refresh." }

                    h3 { "Blocked Senders" }
                    div { class: "setting-row",
                        input {
                            r#type: "text",
                            placeholder: "Address or domain",
                            value: "{blocked_input}",
                            onfocusin: focus_in,
                            onfocusout: focus_out,
                            oninput: move |evt| blocked_input.set(evt.value()),
                        }
                        button {
                            disabled: blocked_input().trim().is_empty(),
                            onclick: move |_| {
                                let entry = blocked_input().trim().to_lowercase();

                                if !entry.is_empty() && !blocked().contains(&entry) {
                                    blocked.write().push(entry);
                                }

                                blocked_input.set(String::new());
                            },
                            "Add"
                        }
                    }
                    for (i, entry) in blocked().into_iter().enumerate() {
                        div { key: "{entry}", class: "setting-row blocked",
                            span { "{entry}" }
                            button { onclick: move |_| { blocked.write().remove(i); }, "Remove" }
                        }
                    }

                    h3 { "Remote Content" }
                    div { class: "setting-row",
                        label { "Remote images and styles" }
                        select {
                            value: if remote_always() { "always" } else { "ask" },
                            onchange: move |evt| remote_always.set(evt.value() == "always"),
                            option { value: "ask", "Ask per message" }
                            option { value: "always", "Always load" }
                        }
                    }
                    div { class: "setting-row",
                        input {
                            r#type: "text",
                            placeholder: "Trusted address or domain",
                            value: "{trusted_input}",
                            onfocusin: focus_in,
                            onfocusout: focus_out,
                            oninput: move |evt| trusted_input.set(evt.value()),
                        }
                        button {
                            disabled: trusted_input().trim().is_empty(),
                            onclick: move |_| {
                                let entry = trusted_input().trim().to_lowercase();

                                if !entry.is_empty() && !trusted().contains(&entry) {
                                    trusted.write().push(entry);
                                }

                                trusted_input.set(String::new());
                            },
                            "Add"
                        }
                    }
                    for (i, entry) in trusted().into_iter().enumerate() {
                        div { key: "{entry}", class: "setting-row blocked",
                            span { "{entry}" }
                            button { onclick: move |_| { trusted.write().remove(i); }, "Remove" }
                        }
                    }

                    h3 { "Shortcuts" }
                    for (i, (action, text)) in rows.iter().cloned().enumerate() {
                        div { key: "{keys::action_name(action)}", class: "setting-row shortcut",
                            label { {keys::action_label(action)} }
                            input {
                                r#type: "text",
                                class: if errors[i].is_some() { "invalid" } else { "" },
                                value: "{text}",
                                onfocusin: focus_in,
                                onfocusout: focus_out,
                                oninput: move |evt| shortcuts.write()[i].1 = evt.value(),
                            }
                            if let Some(e) = &errors[i] {
                                span { class: "error", "{e}" }
                            }
                        }
                    }
                }
                if let Some(e) = save_error() {
                    div { class: "error pad", "{e}" }
                }
                div { class: "settings-foot",
                    if ui_test {
                        span { class: "ui-test", "UI test mode" }
                    }
                    span { class: "spacer" }
                    if confirm_reset() {
                        button {
                            class: "danger",
                            onclick: move |_| {
                                shortcuts.set(chord_rows(&BTreeMap::new()));
                                blocked.set(Vec::new());
                                trusted.set(Vec::new());
                                remote_always.set(false);
                                confirm_reset.set(false);
                            },
                            "Clear shortcuts, blocked and trusted senders"
                        }
                        button { onclick: move |_| confirm_reset.set(false), "Cancel" }
                    } else {
                        button { onclick: move |_| confirm_reset.set(true), "Reset to Defaults…" }
                    }
                    button { onclick: |_| close(), "Close" }
                    button { class: "primary", disabled: !valid, onclick: save, "Save" }
                }
            }
        }
    }
}
