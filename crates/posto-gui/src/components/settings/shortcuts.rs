//! Shortcuts page: editable comma-separated chords per action, with validation.

use std::collections::BTreeMap;

use dioxus::prelude::*;
use posto_core::shortcuts::{Action, Chord, Keymap};

use crate::components::{focus_in, focus_out};
use crate::keys::{self, ALL_ACTIONS};

/// The effective chord text of every action, in settings order.
pub fn chord_rows(overrides: &BTreeMap<String, String>) -> Vec<(Action, String)> {
    let map = Keymap::from_config(overrides).unwrap_or_else(|_| Keymap::defaults());

    ALL_ACTIONS
        .into_iter()
        .map(|a| {
            let text: Vec<_> = map.chords_for(a).iter().map(keys::chord_text).collect();

            (a, text.join(", "))
        })
        .collect()
}

/// Parses every row's comma-separated chords; `None` marks text that is not a shortcut.
pub fn parse_rows(rows: &[(Action, String)]) -> Vec<Option<Vec<Chord>>> {
    rows.iter()
        .map(|(_, text)| Keymap::parse_chords(text).ok())
        .collect()
}

/// A message per row that is invalid or shares its chord with another action.
pub fn row_errors(rows: &[(Action, String)], parsed: &[Option<Vec<Chord>>]) -> Vec<Option<String>> {
    parsed
        .iter()
        .enumerate()
        .map(|(i, chord)| match chord {
            None => Some("Not a valid shortcut, e.g. ctrl+k".to_string()),
            Some(chords) => parsed
                .iter()
                .enumerate()
                .find(|(j, other)| {
                    *j != i
                        && other
                            .as_ref()
                            .is_some_and(|o| o.iter().any(|c| chords.contains(c)))
                })
                .map(|(j, _)| format!("Also used by {}", keys::action_label(rows[j].0))),
        })
        .collect()
}

/// Chord text may be mid-edit and invalid, so it lives in `rows` rather than
/// the draft config and reaches the config only on Save.
#[component]
pub fn ShortcutsPage(mut rows: Signal<Vec<(Action, String)>>) -> Element {
    let current = rows();
    let errors = row_errors(&current, &parse_rows(&current));

    rsx! {
        for (i, (action, text)) in current.into_iter().enumerate() {
            div { key: "{keys::action_name(action)}", class: "setting-row shortcut",
                label { {keys::action_label(action)} }
                input {
                    r#type: "text",
                    class: if errors[i].is_some() { "invalid" } else { "" },
                    value: "{text}",
                    onfocusin: focus_in,
                    onfocusout: focus_out,
                    oninput: move |evt| rows.write()[i].1 = evt.value(),
                }
                if let Some(e) = &errors[i] {
                    span { class: "error", "{e}" }
                }
            }
        }
    }
}
