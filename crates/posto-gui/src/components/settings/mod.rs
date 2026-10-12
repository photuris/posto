//! Settings modal: side tabs on the left, one page at a time on the right.
//!
//! The draft config lives here, so edits survive switching pages and one
//! Save writes everything.

mod appearance;
mod general;
mod reading;
mod shortcuts;

use dioxus::prelude::*;
use posto_core::config::{Account, Config, Secrets};
use posto_core::shortcuts::Keymap;

use crate::app::{
    KEYMAP, SHOW_SETTINGS, backend, focus_root, reload_identities, reload_list, reload_mailboxes,
    run, sync_soon,
};
use crate::components::Icon;
use crate::keys;
use appearance::AppearancePage;
use general::GeneralPage;
use reading::ReadingPage;
use shortcuts::{ShortcutsPage, chord_rows, parse_rows, row_errors};

/// A settings tab. Adding a page: a variant here, its label and icon, and a
/// component in the `match` of [`Settings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    General,
    Reading,
    Shortcuts,
    Appearance,
}

impl Page {
    pub const ALL: [Page; 4] = [
        Page::General,
        Page::Reading,
        Page::Shortcuts,
        Page::Appearance,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Reading => "Reading",
            Page::Shortcuts => "Shortcuts",
            Page::Appearance => "Appearance",
        }
    }

    /// A name known to [`Icon`].
    pub fn icon(self) -> &'static str {
        match self {
            Page::General => "settings",
            Page::Reading => "book-open",
            Page::Shortcuts => "menu",
            Page::Appearance => "sun",
        }
    }
}

fn close() {
    *SHOW_SETTINGS.write() = false;
    focus_root();
}

/// Account, token, refresh, blocked senders, remote content, shortcuts and appearance.
#[component]
pub fn Settings() -> Element {
    let ui_test = backend().peek().ui_test;
    let mut draft = use_signal(|| backend().peek().cfg.clone());
    let mut rows = use_signal(|| chord_rows(&draft.peek().shortcuts));
    let mut page = use_signal(|| Page::General);
    let mut save_error = use_signal(|| None::<String>);
    let mut confirm_reset = use_signal(|| false);

    // A chord shared with another action is an error too.
    let current = rows();
    let parsed = parse_rows(&current);
    let valid = row_errors(&current, &parsed).iter().all(Option::is_none);

    let save = move |_| {
        let mut cfg: Config = draft();

        for a in &mut cfg.accounts {
            let url = a.session_url.trim().to_string();

            a.id = a.id.trim().to_string();
            a.name = a.name.trim().to_string();
            a.email = a.email.trim().to_string();
            a.session_url = if url.is_empty() {
                Account::default().session_url
            } else {
                url
            };
        }

        if let Err(e) = cfg.validate() {
            return save_error.set(Some(e.to_string()));
        }

        cfg.shortcuts.clear();

        let defaults = Keymap::defaults();

        for ((action, _), chords) in rows().iter().zip(&parsed) {
            if let Some(chords) = chords
                && defaults.chords_for(*action) != *chords
            {
                let text: Vec<_> = chords.iter().map(keys::chord_text).collect();

                cfg.shortcuts
                    .insert(keys::action_name(*action), text.join(", "));
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
        let removed: Vec<String> = shared
            .peek()
            .cfg
            .accounts
            .iter()
            .filter(|a| cfg.account(&a.id).is_none())
            .map(|a| a.id.clone())
            .collect();

        // A removed account takes its cached mail and token with it.
        for id in &removed {
            if let Err(e) = shared.peek().store.delete_account(id) {
                return save_error.set(Some(e.to_string()));
            }

            if !ui_test && let Err(e) = Secrets::clear_token(id) {
                return save_error.set(Some(e.to_string()));
            }
        }

        {
            let mut b = shared.write();

            b.cfg = cfg;
            b.reset_client();
        }

        reload_mailboxes();
        reload_identities();
        *KEYMAP.write() = keymap;
        sync_soon();
        run(reload_list());
        close();
    };

    if let Some(e) = backend().read().config_error.clone() {
        return rsx! {
            div { class: "overlay", onclick: |_| close(),
                div { class: "settings", onclick: |evt| evt.stop_propagation(),
                    div { class: "error pad settings-wide",
                        "Settings are disabled: config.toml could not be loaded ({e}). Fix the file and restart."
                    }
                    div { class: "settings-foot settings-wide",
                        span { class: "spacer" }
                        button { onclick: |_| close(), "Close" }
                    }
                }
            }
        };
    }

    rsx! {
        div { class: "overlay", onclick: |_| close(),
            div { class: "settings", onclick: |evt| evt.stop_propagation(),
                nav { class: "settings-nav",
                    for p in Page::ALL {
                        button {
                            key: "{p.label()}",
                            class: if p == page() { "nav-item active" } else { "nav-item" },
                            onclick: move |_| page.set(p),
                            Icon { name: p.icon() }
                            "{p.label()}"
                        }
                    }
                }
                div { class: "settings-page",
                    h2 { "{page().label()}" }
                    match page() {
                        Page::General => rsx! { GeneralPage { draft } },
                        Page::Reading => rsx! { ReadingPage { draft } },
                        Page::Shortcuts => rsx! { ShortcutsPage { rows } },
                        Page::Appearance => rsx! { AppearancePage { draft } },
                    }
                }
                if let Some(e) = save_error() {
                    div { class: "error pad settings-wide", "{e}" }
                }
                div { class: "settings-foot settings-wide",
                    if ui_test {
                        span { class: "ui-test", "UI test mode" }
                    }
                    span { class: "spacer" }
                    if confirm_reset() {
                        button {
                            class: "danger",
                            onclick: move |_| {
                                {
                                    let mut d = draft.write();

                                    d.blocked_senders.clear();
                                    d.trusted_senders.clear();
                                    d.remote_content = "ask".to_string();
                                    d.ui.decorations = "auto".to_string();
                                }
                                rows.set(chord_rows(&Default::default()));
                                confirm_reset.set(false);
                            },
                            "Clear shortcuts, senders and decorations"
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::components::icons::NAMES;

    #[test]
    fn pages_are_complete() {
        let distinct: HashSet<Page> = Page::ALL.into_iter().collect();

        assert_eq!(distinct.len(), 4);

        for page in Page::ALL {
            assert!(!page.label().is_empty(), "{page:?} has no label");
            assert!(
                NAMES.contains(&page.icon()),
                "{page:?}: unknown icon {}",
                page.icon()
            );
        }
    }
}
