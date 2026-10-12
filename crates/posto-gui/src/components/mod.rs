//! UI components. State lives in the globals of [`crate::app`].

mod composer;
mod confirm;
mod icons;
mod message_list;
mod reader;
mod settings;
mod sidebar;
mod toolbar;

use dioxus::prelude::*;

pub use composer::Composer;
pub use confirm::ConfirmDialog;
pub use icons::Icon;
pub use message_list::MessageList;
pub use reader::Reader;
pub use settings::Settings;
pub use sidebar::Sidebar;
pub use toolbar::Toolbar;

use crate::app::{
    MAILBOXES, SHOW_CLASSIFY_PICKER, SHOW_MOVE_PICKER, TYPING, backend, classify_selected,
    focus_root, move_selected, selected_refs, tray_label,
};
use posto_core::classify::Tray;

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

/// Small filterable list of mailboxes to move the selected emails to; only
/// the mailboxes of their account (a move never crosses accounts).
#[component]
pub fn MovePicker() -> Element {
    let account = selected_refs().first().map(|r| r.account_id.clone());
    let items: Vec<(String, String)> = MAILBOXES()
        .into_iter()
        .filter(|m| Some(&m.account_id) == account.as_ref())
        .map(|m| (m.id, m.name))
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
