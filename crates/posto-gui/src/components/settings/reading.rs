//! Reading page: remote content and blocked senders.

use dioxus::prelude::*;
use posto_core::config::Config;

use crate::components::{focus_in, focus_out};

#[component]
pub fn ReadingPage(mut draft: Signal<Config>) -> Element {
    let mut blocked_input = use_signal(String::new);
    let mut trusted_input = use_signal(String::new);
    let remote_always = draft.read().remote_content == "always";

    rsx! {
        h3 { "Remote Content" }
        div { class: "setting-row",
            label { "Remote images and styles" }
            select {
                value: if remote_always { "always" } else { "ask" },
                onchange: move |evt| {
                    draft.write().remote_content =
                        if evt.value() == "always" { "always" } else { "ask" }.to_string();
                },
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

                    if !entry.is_empty() && !draft.read().trusted_senders.contains(&entry) {
                        draft.write().trusted_senders.push(entry);
                    }

                    trusted_input.set(String::new());
                },
                "Add"
            }
        }
        for (i, entry) in draft.read().trusted_senders.clone().into_iter().enumerate() {
            div { key: "{entry}", class: "setting-row blocked",
                span { "{entry}" }
                button {
                    onclick: move |_| {
                        draft.write().trusted_senders.remove(i);
                    },
                    "Remove"
                }
            }
        }

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

                    if !entry.is_empty() && !draft.read().blocked_senders.contains(&entry) {
                        draft.write().blocked_senders.push(entry);
                    }

                    blocked_input.set(String::new());
                },
                "Add"
            }
        }
        for (i, entry) in draft.read().blocked_senders.clone().into_iter().enumerate() {
            div { key: "{entry}", class: "setting-row blocked",
                span { "{entry}" }
                button {
                    onclick: move |_| {
                        draft.write().blocked_senders.remove(i);
                    },
                    "Remove"
                }
            }
        }
    }
}
