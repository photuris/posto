//! Classification page: the Jev switch and key, tuning, tray mailboxes,
//! keep-in-inbox and the per-account opt-in.

use dioxus::prelude::*;
use posto_core::classify::Tray;
use posto_core::classify::jev::tray_criteria;
use posto_core::config::{ClassifyConfig, Config, Secrets};

use crate::app::{backend, set_status};
use crate::components::{focus_in, focus_out};

/// The trays with a mailbox of their own; spam always goes to Junk.
fn filed(c: &ClassifyConfig) -> Vec<Tray> {
    Tray::all(c)
        .into_iter()
        .filter(|t| *t != Tray::SPAM)
        .collect()
}

/// What is wrong with the classifier settings, if anything.
pub fn problems(c: &ClassifyConfig) -> Option<String> {
    if c.model.trim().is_empty() {
        return Some("Enter the Jev model name.".into());
    }

    if !(0.0..=1.0).contains(&c.unsure_margin) {
        return Some("The unsure margin must be between 0 and 1.".into());
    }

    if !(1..=5).contains(&c.flag_urgency_at) {
        return Some("The flag threshold must be between 1 and 5.".into());
    }

    if c.trays.contains_key(Tray::SPAM.as_str()) {
        return Some("Spam always goes to Junk; it cannot have a tray mailbox.".into());
    }

    if let Err(e) = tray_criteria(c) {
        return Some(e.to_string());
    }

    let filed = filed(c);
    let names: Vec<String> = filed
        .iter()
        .map(|t| {
            c.trays
                .get(t.as_str())
                .map(|n| n.trim().to_lowercase())
                .unwrap_or_default()
        })
        .collect();
    let unsure = c.unsure_mailbox.trim().to_lowercase();

    for (i, name) in names.iter().enumerate() {
        if name.is_empty() {
            return Some("Every tray needs a mailbox name.".into());
        }

        if *name == "inbox" || *name == unsure {
            return Some(format!(
                "{:?} cannot be a tray mailbox.",
                c.trays[filed[i].as_str()].trim()
            ));
        }

        if names[..i].contains(name) {
            return Some(format!(
                "Two trays use the mailbox {:?}.",
                c.trays[filed[i].as_str()].trim()
            ));
        }
    }

    None
}

/// Parses the two numeric fields as typed and stores them in `c`.
///
/// The page keeps the raw text of both inputs, so text that is not a number
/// is an error here instead of being dropped and leaving the old value.
pub fn apply_numbers(c: &mut ClassifyConfig, margin: &str, urgency: &str) -> Result<(), String> {
    c.unsure_margin = margin
        .trim()
        .parse()
        .map_err(|_| "The unsure margin must be a number between 0 and 1.".to_string())?;
    c.flag_urgency_at = urgency
        .trim()
        .parse()
        .map_err(|_| "The flag threshold must be a whole number between 1 and 5.".to_string())?;

    Ok(())
}

/// Strips stray spaces from the names the user typed.
pub fn tidy(c: &mut ClassifyConfig) {
    c.model = c.model.trim().to_string();
    c.unsure_mailbox = c.unsure_mailbox.trim().to_string();

    for name in c.trays.values_mut() {
        *name = name.trim().to_string();
    }
}

/// The next opt-in state when an account's checkbox is clicked: follow the
/// global switch, then on, then off, then back.
fn cycle(state: Option<bool>) -> Option<bool> {
    match state {
        None => Some(true),
        Some(true) => Some(false),
        Some(false) => None,
    }
}

#[component]
pub fn ClassificationPage(
    mut draft: Signal<Config>,
    mut margin_text: Signal<String>,
    mut urgency_text: Signal<String>,
) -> Element {
    let ui_test = backend().peek().ui_test;
    let mut key_open = use_signal(|| false);
    let mut key_value = use_signal(String::new);
    let mut key_rev = use_signal(|| 0u32);
    let mut key_error = use_signal(|| None::<String>);

    // Re-read the key state after a save.
    let _ = key_rev();

    let cfg = draft();
    let c = &cfg.classify;
    let key_set = !ui_test && Secrets::typesafe_key().ok().flatten().is_some();
    let mut typed = c.clone();
    let error = apply_numbers(&mut typed, &margin_text(), &urgency_text())
        .err()
        .or_else(|| problems(&typed));

    rsx! {
        h3 { "Jev" }
        div { class: "setting-row",
            label { "Classify new mail" }
            input {
                r#type: "checkbox",
                checked: c.enabled,
                onchange: move |evt| draft.write().classify.enabled = evt.checked(),
            }
            span { class: "muted", "After each sync, for the accounts switched on below." }
        }
        div { class: "setting-row",
            span { "TypeSafe API key" }
            span { class: "muted",
                if ui_test {
                    "Not needed (UI test mode)"
                } else if key_set {
                    "Key configured"
                } else {
                    "No key configured"
                }
            }
            button { onclick: move |_| key_open.set(!key_open()), "Set Key…" }
        }
        if key_open() {
            div { class: "setting-row",
                input {
                    r#type: "password",
                    placeholder: "Paste API key",
                    value: "{key_value}",
                    onmounted: |evt| async move {
                        let _ = evt.set_focus(true).await;
                    },
                    onfocusin: focus_in,
                    onfocusout: focus_out,
                    oninput: move |evt| key_value.set(evt.value()),
                }
                button {
                    onclick: move |_| {
                        let key = key_value().trim().to_string();

                        if key.is_empty() {
                            return key_error.set(Some("Enter a key first".into()));
                        }

                        if backend().peek().ui_test {
                            set_status("UI test mode: the key is not stored.");
                            key_open.set(false);
                            return;
                        }

                        match Secrets::set_typesafe_key(&key) {
                            Ok(()) => {
                                key_rev += 1;
                                key_error.set(None);
                                key_open.set(false);
                                key_value.set(String::new());
                            }
                            Err(e) => key_error.set(Some(e.to_string())),
                        }
                    },
                    "Save"
                }
            }
        }
        if let Some(e) = key_error() {
            div { class: "error", "{e}" }
        }
        div { class: "hint",
            "Stored in secrets.toml in the Posto data folder (mode 0600); TYPESAFE_API_KEY overrides it."
        }
        div { class: "setting-row",
            label { "Model" }
            input {
                r#type: "text",
                value: "{c.model}",
                onfocusin: focus_in,
                onfocusout: focus_out,
                oninput: move |evt| draft.write().classify.model = evt.value(),
            }
        }
        div { class: "setting-row",
            label { "Unsure margin" }
            input {
                class: "number",
                r#type: "text",
                value: "{margin_text}",
                onfocusin: focus_in,
                onfocusout: focus_out,
                oninput: move |evt| margin_text.set(evt.value()),
            }
            span { class: "muted", "0 to 1" }
        }
        div { class: "hint",
            "When Jev's two best trays are closer than this, the message stays in the inbox and is marked Unsure."
        }
        div { class: "setting-row",
            label { "Flag urgent replies at" }
            input {
                class: "number",
                r#type: "number",
                min: "1",
                max: "5",
                value: "{urgency_text}",
                onfocusin: focus_in,
                onfocusout: focus_out,
                oninput: move |evt| urgency_text.set(evt.value()),
            }
            span { class: "muted", "urgency 1 to 5" }
        }

        h3 { "Trays" }
        for tray in filed(c) {
            div { key: "{tray}", class: "setting-row tray",
                label { {tray_title(&tray)} }
                input {
                    r#type: "text",
                    value: "{c.trays.get(tray.as_str()).cloned().unwrap_or_default()}",
                    onfocusin: focus_in,
                    onfocusout: focus_out,
                    oninput: {
                        let tray = tray.clone();
                        move |evt: FormEvent| {
                        draft
                            .write()
                            .classify
                            .trays
                            .insert(tray.as_str().to_string(), evt.value());
                        }
                    },
                }
                label { class: "check",
                    input {
                        r#type: "checkbox",
                        checked: c.keep_in_inbox.iter().any(|k| k == tray.as_str()),
                        onchange: {
                            let tray = tray.clone();
                            move |evt: FormEvent| {
                            let mut d = draft.write();
                            let keep = &mut d.classify.keep_in_inbox;

                            keep.retain(|k| k != tray.as_str());

                            if evt.checked() {
                                keep.push(tray.as_str().to_string());
                            }
                            }
                        },
                    }
                    "Keep in inbox"
                }
            }
        }
        div { class: "setting-row tray",
            label { "Spam" }
            span { class: "muted", "Junk (fixed)" }
        }
        div { class: "hint",
            "Each tray is a top-level mailbox, created when first needed. Unsure messages always stay in the inbox. To add a tray, give it a mailbox under [classify.trays] and a description under [classify.criteria] in config.toml."
        }

        h3 { "Accounts" }
        for (i , a) in cfg.accounts.iter().enumerate() {
            {
                let state = a.classify;
                let effective = state.unwrap_or(c.enabled);

                rsx! {
                    div { key: "{i}-{state:?}-{effective}", class: "setting-row",
                        input {
                            r#type: "checkbox",
                            checked: effective,
                            onchange: move |_| {
                                if let Some(a) = draft.write().accounts.get_mut(i) {
                                    a.classify = cycle(a.classify);
                                }
                            },
                        }
                        label { "{a.display_name()}" }
                        span { class: "muted",
                            match state {
                                None if effective => "follow global (on)",
                                None => "follow global (off)",
                                Some(true) => "on",
                                Some(false) => "off",
                            }
                        }
                    }
                }
            }
        }
        if let Some(e) = error {
            div { class: "error", "{e}" }
        }
    }
}

/// The page's label for a tray: its key with spaces, capitalised
/// (`needs_reply` is "Needs reply").
fn tray_title(tray: &Tray) -> String {
    let words = tray.as_str().replace('_', " ");
    let mut chars = words.chars();

    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_bad_settings() {
        let ok = ClassifyConfig::default();

        assert_eq!(problems(&ok), None);

        let mut empty = ok.clone();

        empty.trays.insert("social".into(), "  ".into());
        assert!(problems(&empty).unwrap().contains("mailbox name"));

        let mut twice = ok.clone();

        twice.trays.insert("social".into(), "RECEIPTS".into());
        assert!(problems(&twice).unwrap().contains("Two trays"));

        let mut clash = ok.clone();

        clash.trays.insert("social".into(), "unsure".into());
        assert!(problems(&clash).unwrap().contains("cannot be"));

        let mut spam = ok.clone();

        spam.trays.insert("spam".into(), "Spam".into());
        assert!(problems(&spam).unwrap().contains("Junk"));

        let mut undescribed = ok.clone();

        undescribed.trays.insert("bills".into(), "Bills".into());
        assert!(
            problems(&undescribed)
                .unwrap()
                .contains("[classify.criteria]")
        );

        let mut margin = ok.clone();

        margin.unsure_margin = 1.5;
        assert!(problems(&margin).is_some());

        let mut urgency = ok;

        urgency.flag_urgency_at = 0;
        assert!(problems(&urgency).is_some());
    }

    #[test]
    fn typed_numbers_must_parse() {
        let mut c = ClassifyConfig::default();

        assert_eq!(apply_numbers(&mut c, " 0.3 ", "4"), Ok(()));
        assert_eq!((c.unsure_margin, c.flag_urgency_at), (0.3, 4));

        for (margin, urgency) in [
            ("nonsense", "4"),
            ("", "4"),
            ("0.3", ""),
            ("0.3", "x"),
            ("0.3", "300"),
        ] {
            let mut c = ClassifyConfig::default();

            assert!(
                apply_numbers(&mut c, margin, urgency).is_err(),
                "{margin:?} {urgency:?}"
            );
        }
    }

    #[test]
    fn account_toggle_cycles_through_follow_on_off() {
        assert_eq!(cycle(None), Some(true));
        assert_eq!(cycle(Some(true)), Some(false));
        assert_eq!(cycle(Some(false)), None);
    }
}
