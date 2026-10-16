//! UI components. State lives in the globals of [`crate::app`].

mod composer;
mod confirm;
mod icons;
mod menu;
mod message_list;
mod reader;
mod settings;
mod sidebar;
mod toolbar;

use dioxus::prelude::*;

pub use composer::Composer;
pub use confirm::ConfirmDialog;
pub use icons::Icon;
pub use menu::ContextMenu;
pub use message_list::MessageList;
pub use reader::Reader;
pub use settings::Settings;
pub use sidebar::Sidebar;
pub use toolbar::Toolbar;

use crate::app::{
    EMAILS, LabelState, MAILBOXES, SELECTION, SHOW_CLASSIFY_PICKER, SHOW_LABEL_PICKER,
    SHOW_MOVE_PICKER, TYPING, backend, classify_selected, enabled_ids, focus_root, label_intent,
    label_removable, label_state, move_selected, selected_emails, toggle_label, tray_label,
};
use crate::format;
use posto_core::classify::Tray;
use posto_core::store::mailbox_groups;

/// `onfocusin` for inputs: shortcuts are suspended while typing.
pub fn focus_in(_: FocusEvent) {
    *TYPING.write() = true;
}

/// `onfocusout` for inputs.
pub fn focus_out(_: FocusEvent) {
    *TYPING.write() = false;
}

/// Wraps untrusted HTML for a sandboxed `srcdoc` iframe: links target a new
/// window, colours follow the system scheme.
pub fn frame_doc(inner: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><base target=\"_blank\">\
         <style>:root{{color-scheme:light dark}}\
         body{{margin:16px;font-family:system-ui,sans-serif;overflow-wrap:anywhere}}\
         img{{max-width:100%;height:auto}}</style></head><body>{inner}</body></html>"
    )
}

/// Reader-mode wrapper: a serif, single-column stylesheet over structure-only
/// HTML (see `posto_core::html::reader`). Tables keep their cell layout and
/// scroll sideways instead of wrapping.
pub fn reader_frame_doc(inner: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><base target=\"_blank\">\
         <style>:root{{color-scheme:light dark}}\
         body{{max-width:680px;margin:0 auto;padding:28px 24px;\
         font-family:Georgia,\"Noto Serif\",serif;font-size:17px;line-height:1.6;overflow-wrap:anywhere}}\
         h1,h2,h3,h4{{line-height:1.25;margin:1.2em 0 .5em}}\
         img{{max-width:100%;height:auto}}\
         pre,code{{font-family:ui-monospace,monospace;font-size:14px}}\
         pre{{white-space:pre;overflow-x:auto}}\
         table{{border-collapse:collapse;margin:1em 0;max-width:100%;display:block;overflow-x:auto;font-size:15px}}\
         td,th{{padding:6px 10px;border-bottom:1px solid color-mix(in srgb,currentColor 25%,transparent);text-align:left;vertical-align:top;white-space:nowrap}}\
         th{{font-weight:600}}\
         hr{{border:0;border-top:1px solid color-mix(in srgb,currentColor 25%,transparent);margin:1.5em 0}}\
         blockquote{{margin:1em 0;padding-left:1em;border-left:3px solid color-mix(in srgb,currentColor 30%,transparent)}}\
         </style></head><body>{inner}</body></html>"
    )
}

/// Small filterable list of mailbox groups to move the selected emails to;
/// each message goes to the group's mailbox in its own account.
#[component]
pub fn MovePicker() -> Element {
    let ids = enabled_ids(&backend().read().cfg);
    let items: Vec<(String, String)> = mailbox_groups(&MAILBOXES(), &ids)
        .into_iter()
        .map(|g| (g.key, g.name))
        .collect();

    rsx! {
        ListPicker {
            title: "Move to…",
            placeholder: "Filter mailboxes",
            items,
            on_pick: move |id: String| move_selected(id),
            on_close: |_| *SHOW_MOVE_PICKER.write() = false,
        }
    }
}

/// A checkbox per label group (every member a mailbox without a role, or an
/// inbox); a click or Enter toggles it on every selected message, each in its
/// own account, and the picker stays open. Reads the emails, selection and
/// mailboxes during render, so a settled action refreshes the checkboxes.
#[component]
pub fn LabelPicker() -> Element {
    let mut query = use_signal(String::new);
    let mut index = use_signal(|| 0usize);
    let (emails, selection, mailboxes) = (EMAILS(), SELECTION(), MAILBOXES());
    let selected = selected_emails(&emails, &selection);
    let ids = enabled_ids(&backend().read().cfg);
    let needle = query().to_lowercase();
    let rows: Vec<(String, String, u16, LabelState, bool)> = mailbox_groups(&mailboxes, &ids)
        .into_iter()
        .filter(|g| {
            g.members.iter().all(|(account, id)| {
                mailboxes
                    .iter()
                    .find(|m| m.account_id == *account && m.id == *id)
                    .is_some_and(|m| m.role.as_deref().is_none_or(|r| r == "inbox"))
            })
        })
        .filter(|g| g.name.to_lowercase().contains(&needle))
        .map(|g| {
            let blocked = label_intent(&selected, &g.members) == Some(false)
                && !label_removable(&selected, &g.members);

            (
                g.key,
                g.name.clone(),
                format::avatar_hue(&g.name),
                label_state(&selected, &g.members),
                blocked,
            )
        })
        .collect();
    let last = rows.len().saturating_sub(1);
    let active = index().min(last);
    // A blocked row stays active: toggling it only reports why it cannot change.
    let enter_target = rows.get(active).map(|row| row.0.clone());

    rsx! {
        div {
            class: "overlay",
            onclick: move |_| {
                *SHOW_LABEL_PICKER.write() = false;
                focus_root();
            },
            div { class: "picker", onclick: |evt| evt.stop_propagation(),
                div { class: "picker-title", "Labels" }
                input {
                    r#type: "text",
                    placeholder: "Filter labels",
                    value: "{query}",
                    onmounted: |evt| async move {
                        let _ = evt.set_focus(true).await;
                    },
                    onfocusin: focus_in,
                    onfocusout: focus_out,
                    oninput: move |evt| {
                        query.set(evt.value());
                        index.set(0);
                    },
                    onkeydown: move |evt| match evt.key() {
                        Key::ArrowDown => {
                            evt.prevent_default();
                            index.set((active + 1).min(last));
                        }
                        Key::ArrowUp => {
                            evt.prevent_default();
                            index.set(active.saturating_sub(1));
                        }
                        Key::Enter => {
                            evt.prevent_default();

                            if let Some(id) = enter_target.clone() {
                                toggle_label(id);
                            }
                        }
                        _ => {}
                    },
                }
                for (i, (id, path, hue, state, blocked)) in rows.into_iter().enumerate() {
                    div {
                        key: "{id}",
                        class: match (i == active, blocked) {
                            (true, true) => "picker-row label-row active disabled",
                            (true, false) => "picker-row label-row active",
                            (false, true) => "picker-row label-row disabled",
                            (false, false) => "picker-row label-row",
                        },
                        title: if blocked { "A message must keep at least one label" } else { "" },
                        // Keep the filter focused across toggles.
                        onmousedown: |evt| evt.prevent_default(),
                        onclick: move |_| toggle_label(id.clone()),
                        Icon {
                            name: match state {
                                LabelState::All => "check-square",
                                LabelState::Some => "minus-square",
                                LabelState::None => "square",
                            },
                        }
                        span { class: "label-dot", style: "--hue: {hue}" }
                        "{path}"
                    }
                }
            }
        }
    }
}

/// The five classifier trays, labelled by mailbox name, to file the selected
/// emails under.
#[component]
pub fn ClassifyPicker() -> Element {
    let cfg = backend().read().cfg.clone();
    let items: Vec<(String, String)> = Tray::ALL
        .into_iter()
        .map(|t| (t.as_str().to_string(), tray_label(&cfg, t)))
        .collect();

    rsx! {
        ListPicker {
            title: "Classify as…",
            placeholder: "Filter trays",
            items,
            on_pick: |key: String| {
                if let Ok(tray) = key.parse::<Tray>() {
                    classify_selected(tray);
                }
            },
            on_close: |_| *SHOW_CLASSIFY_PICKER.write() = false,
        }
    }
}

/// A filterable list of `(key, label)` items; Enter or a click picks one.
#[component]
fn ListPicker(
    title: &'static str,
    placeholder: &'static str,
    items: Vec<(String, String)>,
    on_pick: EventHandler<String>,
    on_close: EventHandler<()>,
) -> Element {
    let mut query = use_signal(String::new);
    let mut index = use_signal(|| 0usize);
    let needle = query().to_lowercase();
    let shown: Vec<(String, String)> = items
        .into_iter()
        .filter(|(_, label)| label.to_lowercase().contains(&needle))
        .collect();
    let last = shown.len().saturating_sub(1);
    let active = index().min(last);
    let enter_target = shown.get(active).map(|(key, _)| key.clone());

    rsx! {
        div {
            class: "overlay",
            onclick: move |_| {
                on_close.call(());
                focus_root();
            },
            div { class: "picker", onclick: |evt| evt.stop_propagation(),
                div { class: "picker-title", "{title}" }
                input {
                    r#type: "text",
                    placeholder,
                    value: "{query}",
                    onmounted: |evt| async move {
                        let _ = evt.set_focus(true).await;
                    },
                    onfocusin: focus_in,
                    onfocusout: focus_out,
                    oninput: move |evt| {
                        query.set(evt.value());
                        index.set(0);
                    },
                    onkeydown: move |evt| match evt.key() {
                        Key::ArrowDown => index.set((active + 1).min(last)),
                        Key::ArrowUp => index.set(active.saturating_sub(1)),
                        Key::Enter => {
                            if let Some(key) = enter_target.clone() {
                                on_pick.call(key);
                            }
                        }
                        _ => {}
                    },
                }
                for (i, (key, label)) in shown.into_iter().enumerate() {
                    div {
                        key: "{key}",
                        class: if i == active { "picker-row active" } else { "picker-row" },
                        onclick: move |_| on_pick.call(key.clone()),
                        "{label}"
                    }
                }
            }
        }
    }
}
