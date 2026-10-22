//! Compose and reply modal.

use std::collections::BTreeMap;

use dioxus::prelude::*;
use posto_core::compose::{self, Draft};
use posto_core::config::Config;
use posto_core::provider::{Identity, MaskedEmail};
use posto_core::store::Address;

use crate::app::{
    COMPOSER, COMPOSER_CC, COMPOSER_TO, FROM_MENU, IDENTITIES, MASKED, STATUS, backend,
    close_composer, focus_root, send,
};
use crate::components::icons::Icon;
use crate::components::{focus_in, focus_out, frame_doc};

/// `data-from` of the From button.
const FROM_BUTTON: &str = "button";
/// `data-from` of the transparent layer that closes the menu on a click.
const FROM_SCRIM: &str = "scrim";
/// `data-from` of the masked-email filter input.
const FROM_FILTER: &str = "filter";

/// The row shown when the filter matches nothing. Formatted into the row, so
/// it is a text node of its own that a headless test can see.
const NO_MATCH: &str = "No matching masked emails";

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
    /// The `data-from` value of the option's row: `<account>/<email>` (account
    /// ids hold no slash).
    fn value(&self) -> String {
        format!("{}/{}", self.account_id, self.identity.email)
    }
}

/// The masked addresses one account offers, under its parent identity.
#[derive(Clone, Debug, PartialEq)]
struct MaskedGroup {
    account_id: String,
    /// Index into the options of the identity the group sits under.
    parent: usize,
    /// The account's masked emails minus any whose address is an identity of
    /// the same account, in cache order.
    list: Vec<MaskedEmail>,
}

/// What the draft's From resolves to.
#[derive(Clone, Debug, PartialEq)]
enum Resolved {
    /// `options[index]`.
    Identity(usize),
    /// `groups[group].list[index]`.
    Masked { group: usize, index: usize },
}

impl Resolved {
    /// The id of the account the resolved sender belongs to.
    fn account<'a>(&self, options: &'a [FromOption], groups: &'a [MaskedGroup]) -> &'a str {
        match self {
            Resolved::Identity(i) => &options[*i].account_id,
            Resolved::Masked { group, .. } => &groups[*group].account_id,
        }
    }
}

/// One row of the From menu, in display order.
#[derive(Clone, Debug, PartialEq)]
enum MenuRow {
    Identity(usize),
    /// The disclosure row of `groups[group]`.
    Group {
        group: usize,
        open: bool,
    },
    /// The filter input of the open group.
    Filter {
        group: usize,
    },
    /// `groups[group].list[index]`.
    Masked {
        group: usize,
        index: usize,
    },
    /// The open group's filter matches nothing.
    NoMatch {
        group: usize,
    },
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

/// One group per enabled account that has masked emails and an identity to sit
/// under: the identity of the account's configured address, else its first.
/// Masked emails that duplicate one of the account's identities are left out.
fn masked_groups(
    cfg: &Config,
    options: &[FromOption],
    masked: &BTreeMap<String, Vec<MaskedEmail>>,
) -> Vec<MaskedGroup> {
    cfg.accounts_enabled()
        .filter_map(|a| {
            let own = |o: &&FromOption| o.account_id == a.id;
            let parent = options
                .iter()
                .position(|o| own(&o) && o.identity.email.eq_ignore_ascii_case(&a.email))
                .or_else(|| options.iter().position(|o| own(&o)))?;
            let list: Vec<MaskedEmail> = masked
                .get(&a.id)?
                .iter()
                .filter(|m| {
                    !options
                        .iter()
                        .filter(own)
                        .any(|o| o.identity.email.eq_ignore_ascii_case(&m.email))
                })
                .cloned()
                .collect();

            (!list.is_empty()).then(|| MaskedGroup {
                account_id: a.id.clone(),
                parent,
                list,
            })
        })
        .collect()
}

/// The options and masked groups the caches and `cfg` give. Reads the signals,
/// so a caller inside a render or an effect subscribes to them.
fn from_model(cfg: &Config) -> (Vec<FromOption>, Vec<MaskedGroup>) {
    let options = from_options_for(cfg, &IDENTITIES());
    let groups = masked_groups(cfg, &options, &MASKED());

    (options, groups)
}

/// The indexes of the entries whose description, domain or address contains the
/// trimmed `query`, ignoring case; an empty query keeps everything.
fn filter_masked(list: &[MaskedEmail], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();

    list.iter()
        .enumerate()
        .filter(|(_, m)| {
            [&m.description, &m.for_domain, &m.email]
                .iter()
                .any(|field| field.to_lowercase().contains(&query))
        })
        .map(|(i, _)| i)
        .collect()
}

/// What the draft's From resolves to. With a `from`: the draft account's
/// identity, then its masked entry, then the same for each other enabled
/// account in config order. Without one: the same default `compose::send`
/// applies (the account's own address, else its first identity).
fn resolve_from(
    cfg: &Config,
    options: &[FromOption],
    groups: &[MaskedGroup],
    draft: &Draft,
) -> Option<Resolved> {
    let Some(from) = &draft.from else {
        let own_email = cfg
            .account(&draft.account_id)
            .map_or("", |a| a.email.as_str());
        let own = || {
            options
                .iter()
                .enumerate()
                .filter(|(_, o)| o.account_id == draft.account_id)
        };

        return own()
            .find(|(_, o)| o.identity.email.eq_ignore_ascii_case(own_email))
            .or_else(|| own().next())
            .map(|(i, _)| Resolved::Identity(i));
    };
    let identity_of = |account: &str| {
        options
            .iter()
            .position(|o| {
                o.account_id == account && o.identity.email.eq_ignore_ascii_case(&from.email)
            })
            .map(Resolved::Identity)
    };
    let masked_of = |account: &str| {
        let (group, g) = groups
            .iter()
            .enumerate()
            .find(|(_, g)| g.account_id == account)?;
        let index = g
            .list
            .iter()
            .position(|m| m.email.eq_ignore_ascii_case(&from.email))?;

        Some(Resolved::Masked { group, index })
    };

    identity_of(&draft.account_id)
        .or_else(|| masked_of(&draft.account_id))
        .or_else(|| {
            cfg.accounts_enabled()
                .filter(|a| a.id != draft.account_id)
                .find_map(|a| identity_of(&a.id).or_else(|| masked_of(&a.id)))
        })
}

/// What a masked address is called: its description, else its domain, else the
/// address.
fn masked_label(m: &MaskedEmail) -> &str {
    [&m.description, &m.for_domain]
        .into_iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or(&m.email)
}

/// The text of the From button.
fn from_label(
    cfg: &Config,
    options: &[FromOption],
    groups: &[MaskedGroup],
    resolved: Option<&Resolved>,
    draft: &Draft,
) -> String {
    match resolved {
        Some(Resolved::Identity(i)) => options[*i].label.clone(),
        Some(Resolved::Masked { group, index }) => {
            let g = &groups[*group];
            let m = &g.list[*index];
            let label = format!("{} <{}>", masked_label(m), m.email);

            match cfg.account(&g.account_id) {
                Some(a) if cfg.accounts_enabled().count() > 1 => {
                    format!("{label} — {}", a.display_name())
                }
                _ => label,
            }
        }
        None => draft
            .from
            .as_ref()
            .map_or_else(|| "Default Fastmail address".into(), |f| f.email.clone()),
    }
}

/// The menu's rows: every identity, with the account's masked group directly
/// under its parent identity; the `open` group adds its filter and the entries
/// that match `query`.
fn menu_rows(
    options: &[FromOption],
    groups: &[MaskedGroup],
    open: Option<usize>,
    query: &str,
) -> Vec<MenuRow> {
    let mut rows = Vec::new();

    for i in 0..options.len() {
        rows.push(MenuRow::Identity(i));

        for (group, g) in groups.iter().enumerate().filter(|(_, g)| g.parent == i) {
            let is_open = open == Some(group);

            rows.push(MenuRow::Group {
                group,
                open: is_open,
            });

            if !is_open {
                continue;
            }

            rows.push(MenuRow::Filter { group });

            let shown = filter_masked(&g.list, query);

            if shown.is_empty() {
                rows.push(MenuRow::NoMatch { group });
            }

            rows.extend(
                shown
                    .into_iter()
                    .map(|index| MenuRow::Masked { group, index }),
            );
        }
    }

    rows
}

/// Whether Enter in the filter picks the first match: plain Enter only, so the
/// chords with a modifier (Ctrl+Enter sends) reach the shortcut policy.
fn picks_first_match(key: &Key, modifiers: Modifiers) -> bool {
    *key == Key::Enter
        && !modifiers
            .intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::META | Modifiers::SHIFT)
}

/// Sends from `o`, an identity of its account.
fn pick_identity(o: &FromOption) {
    edit(|d| {
        d.account_id = o.account_id.clone();
        d.from = Some(Address {
            name: (!o.identity.name.is_empty()).then(|| o.identity.name.clone()),
            email: o.identity.email.clone(),
        });
    });
}

/// Sends from the masked address `m` of `account`, with no display name.
fn pick_masked(account: &str, m: &MaskedEmail) {
    edit(|d| {
        d.account_id = account.to_string();
        d.from = Some(Address {
            name: None,
            email: m.email.clone(),
        });
    });
}

/// Modal over the reader: headers, body, Markdown toggle with preview, Close / Send.
#[component]
pub fn Composer() -> Element {
    let draft = COMPOSER().unwrap_or_default();
    let mut masked_open = use_signal(|| None::<String>);
    let mut masked_query = use_signal(String::new);
    let (options, groups, resolved, from) = {
        let b = backend();
        let b = b.read();
        let (options, groups) = from_model(&b.cfg);
        let resolved = resolve_from(&b.cfg, &options, &groups, &draft);
        let from = from_label(&b.cfg, &options, &groups, resolved.as_ref(), &draft);

        (options, groups, resolved, from)
    };
    // A From of another account makes that account the sender.
    use_effect(|| {
        let draft = COMPOSER().unwrap_or_default();
        let account = {
            let b = backend();
            let b = b.read();
            let (options, groups) = from_model(&b.cfg);

            resolve_from(&b.cfg, &options, &groups, &draft)
                .map(|r| r.account(&options, &groups).to_string())
        };

        if let Some(account) = account.filter(|a| *a != draft.account_id) {
            edit(|d| d.account_id = account);
        }
    });
    // The caches reload while the composer is open: drop what no longer exists.
    use_effect(move || {
        let (empty, accounts) = {
            let b = backend();
            let b = b.read();
            let (options, groups) = from_model(&b.cfg);

            (
                options.is_empty(),
                groups.into_iter().map(|g| g.account_id).collect::<Vec<_>>(),
            )
        };
        let menu_gone = empty && *FROM_MENU.peek();
        let group_gone = masked_open
            .peek()
            .as_ref()
            .is_some_and(|a| !accounts.contains(a));

        if menu_gone {
            *FROM_MENU.write() = false;
        }

        if menu_gone || group_gone {
            masked_open.set(None);
            masked_query.set(String::new());
            focus_root();
        }
    });
    let close_menu = move || {
        *FROM_MENU.write() = false;
        focus_root();
    };
    let open_group = masked_open
        .read()
        .as_ref()
        .and_then(|a| groups.iter().position(|g| g.account_id == *a));
    let rows = menu_rows(&options, &groups, open_group, &masked_query.read());
    let resolved_group = match &resolved {
        Some(Resolved::Masked { group, .. }) => Some(groups[*group].account_id.clone()),
        _ => None,
    };
    let items: Vec<Element> = rows
        .iter()
        .map(|row| match row {
            MenuRow::Identity(i) => {
                let o = options[*i].clone();
                let current = resolved == Some(Resolved::Identity(*i));

                rsx! {
                    div {
                        key: "id:{o.value()}",
                        class: if current { "from-row current" } else { "from-row" },
                        "data-from": "id:{o.value()}",
                        onmousedown: |evt| evt.prevent_default(),
                        onclick: {
                            let o = o.clone();

                            move |_| {
                                pick_identity(&o);
                                close_menu();
                            }
                        },
                        if current {
                            Icon { name: "check" }
                        } else {
                            span { class: "from-spacer" }
                        }
                        "{o.label}"
                    }
                }
            }
            MenuRow::Group { group, open } => {
                let g = &groups[*group];
                let account = g.account_id.clone();
                let chevron = if *open {
                    "chevron-down"
                } else {
                    "chevron-right"
                };

                rsx! {
                    div {
                        key: "group:{account}",
                        class: "from-row masked-group",
                        "data-from": "group:{account}",
                        onmousedown: |evt| evt.prevent_default(),
                        onclick: {
                            let account = account.clone();

                            move |_| {
                                let was_open = masked_open.peek().as_ref() == Some(&account);

                                masked_query.set(String::new());

                                if was_open {
                                    masked_open.set(None);
                                    focus_root();
                                } else {
                                    masked_open.set(Some(account.clone()));
                                }
                            }
                        },
                        span { class: "from-spacer" }
                        span { class: "masked-group-label", "Masked emails" }
                        span { class: "count", "({g.list.len()})" }
                        Icon { name: chevron }
                    }
                }
            }
            MenuRow::Filter { group } => {
                let g = &groups[*group];
                let account = g.account_id.clone();
                let list = g.list.clone();

                rsx! {
                    input {
                        key: "filter:{account}",
                        class: "from-filter",
                        "data-from": "{FROM_FILTER}",
                        r#type: "text",
                        placeholder: "Filter masked emails",
                        value: "{masked_query}",
                        onmounted: |evt| async move {
                            let _ = evt.set_focus(true).await;
                        },
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: move |evt| masked_query.set(evt.value()),
                        onkeydown: move |evt| {
                            if !picks_first_match(&evt.key(), evt.modifiers()) {
                                return;
                            }

                            evt.prevent_default();
                            evt.stop_propagation();

                            let first = filter_masked(&list, &masked_query.peek())
                                .first()
                                .map(|i| list[*i].clone());

                            if let Some(m) = first {
                                pick_masked(&account, &m);
                                close_menu();
                            }
                        },
                    }
                }
            }
            MenuRow::Masked { group, index } => {
                let account = groups[*group].account_id.clone();
                let m = groups[*group].list[*index].clone();
                let current = resolved
                    == Some(Resolved::Masked {
                        group: *group,
                        index: *index,
                    });
                let label = masked_label(&m).to_string();

                rsx! {
                    div {
                        key: "masked:{account}/{m.email}",
                        class: if current { "from-row masked current" } else { "from-row masked" },
                        "data-from": "masked:{account}/{m.email}",
                        onmousedown: |evt| evt.prevent_default(),
                        onclick: {
                            let (account, m) = (account.clone(), m.clone());

                            move |_| {
                                pick_masked(&account, &m);
                                close_menu();
                            }
                        },
                        if current {
                            Icon { name: "check" }
                        } else {
                            span { class: "from-spacer" }
                        }
                        span { class: "masked-label", "{label}" }
                        if label != m.email {
                            span { class: "masked-email", "{m.email}" }
                        }
                        if m.state == "disabled" {
                            span { class: "tag", "disabled" }
                        }
                    }
                }
            }
            MenuRow::NoMatch { group } => rsx! {
                div { key: "nomatch:{group}", class: "from-row empty", "{NO_MATCH}" }
            },
        })
        .collect();
    let preview = draft
        .markdown
        .then(|| frame_doc(&compose::render_markdown(&draft.body)));
    let focus_body = !draft.to.is_empty();
    // The fields are uncontrolled: a re-render that pushed a value the input
    // had not reported yet (a keystroke in flight) would reset it and move the
    // caret to the end. The composer mounts per draft, so these are its own.
    let initial = use_hook(|| {
        (
            COMPOSER_TO.peek().clone(),
            COMPOSER_CC.peek().clone(),
            draft.subject.clone(),
            draft.body.clone(),
        )
    });

    rsx! {
        div { class: "overlay",
            div { class: "composer",
                div { class: "composer-title", "{draft.subject}" }
                div { class: "field",
                    label { "From:" }
                    if options.is_empty() {
                        span { class: "static", "{from}" }
                    } else {
                        div { class: "from-control",
                            button {
                                class: "from-button",
                                "data-from": "{FROM_BUTTON}",
                                onclick: move |_| {
                                    if *FROM_MENU.peek() {
                                        return close_menu();
                                    }

                                    masked_open.set(resolved_group.clone());
                                    masked_query.set(String::new());
                                    *FROM_MENU.write() = true;
                                },
                                span { class: "from-text", "{from}" }
                                Icon { name: "chevron-down" }
                            }
                            if FROM_MENU() {
                                div {
                                    class: "from-scrim",
                                    "data-from": "{FROM_SCRIM}",
                                    onclick: move |_| close_menu(),
                                }
                                div { class: "from-menu", {items.into_iter()} }
                            }
                        }
                    }
                }
                div { class: "field",
                    label { "To:" }
                    input {
                        id: "compose-to",
                        r#type: "text",
                        initial_value: "{initial.0}",
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
                        initial_value: "{initial.1}",
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: |evt| *COMPOSER_CC.write() = evt.value(),
                    }
                }
                div { class: "field",
                    label { "Subject:" }
                    input {
                        r#type: "text",
                        initial_value: "{initial.2}",
                        onfocusin: focus_in,
                        onfocusout: focus_out,
                        oninput: |evt| edit(|d| d.subject = evt.value()),
                    }
                }
                div { class: if preview.is_some() { "composer-body split" } else { "composer-body" },
                    textarea {
                        id: "compose-body",
                        initial_value: "{initial.3}",
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
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Duration;

    use dioxus::core::{AttributeValue, ElementId, Event, Mutation, RuntimeGuard};
    use dioxus::html::{
        Code, Location, PlatformEventData, SerializedFormData, SerializedKeyboardData,
        SerializedMouseData,
    };
    use posto_core::config::Account;
    use posto_core::shortcuts::Action;
    use posto_core::store::Store;

    use super::*;
    use crate::app::{App, TYPING, dispatch, open_composer};
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
        let config = |default: &str, work: &str| {
            let mut cfg = Config::default();

            cfg.accounts[0].email = default.into();
            cfg.add_account(Account::new("work", work)).unwrap();

            cfg
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
        let pick = |cfg: &Config, options: &[FromOption], draft: &Draft| {
            resolve_from(cfg, options, &[], draft).map(|r| match r {
                Resolved::Identity(i) => options[i].value(),
                Resolved::Masked { .. } => unreachable!("no masked groups"),
            })
        };
        let cfg = config("h@x.io", "w@x.io");
        let two = options(&[("default", "h@x.io"), ("work", "w@x.io")]);

        // The addressed identity belongs to the other account.
        assert_eq!(
            pick(&cfg, &two, &draft("default", "W@X.io")).as_deref(),
            Some("work/w@x.io")
        );
        assert_eq!(
            pick(&cfg, &two, &draft("work", "h@x.io")).as_deref(),
            Some("default/h@x.io")
        );

        // The draft's own account wins when both have the address.
        let shared = options(&[("default", "s@x.io"), ("work", "s@x.io")]);

        assert_eq!(
            pick(&cfg, &shared, &draft("work", "s@x.io")).as_deref(),
            Some("work/s@x.io")
        );

        // Without a From: the account's own address, else its first identity.
        let own = Draft {
            account_id: "work".into(),
            ..Draft::default()
        };

        assert_eq!(pick(&cfg, &two, &own).as_deref(), Some("work/w@x.io"));
        assert_eq!(
            pick(&config("h@x.io", "other@x.io"), &two, &own).as_deref(),
            Some("work/w@x.io")
        );
    }

    fn masked(id: &str, email: &str, description: &str, domain: &str, state: &str) -> MaskedEmail {
        MaskedEmail {
            id: id.into(),
            email: email.into(),
            description: description.into(),
            for_domain: domain.into(),
            state: state.into(),
            last_message_at: None,
        }
    }

    /// The pure-model fixture: `default` and `work` enabled, `off` disabled.
    struct Model {
        cfg: Config,
        masked: BTreeMap<String, Vec<MaskedEmail>>,
        options: Vec<FromOption>,
        groups: Vec<MaskedGroup>,
    }

    /// `default` has two identities (Home second, on purpose) and four masked
    /// emails, one of which duplicates Home; `work` has one of each; `off` is
    /// disabled; `ghost` is not in the config. `work_identity` false leaves
    /// `work` without identities.
    fn model(work_identity: bool) -> Model {
        let mut cfg = Config::default();

        cfg.accounts[0].email = "h@x.io".into();
        cfg.accounts[0].name = "Home".into();
        cfg.add_account(Account::new("work", "w@x.io")).unwrap();
        cfg.add_account(Account::new("off", "o@x.io")).unwrap();
        cfg.accounts[2].enabled = false;

        let identities = BTreeMap::from([
            (
                "default".to_string(),
                vec![identity("Other", "o2@x.io"), identity("Home", "h@x.io")],
            ),
            (
                "work".to_string(),
                if work_identity {
                    vec![identity("Work", "w@x.io")]
                } else {
                    vec![]
                },
            ),
            ("off".to_string(), vec![identity("Off", "o@x.io")]),
        ]);
        let masked = BTreeMap::from([
            (
                "default".to_string(),
                vec![
                    masked("m1", "m1@x.io", "Shop", "https://shop.example", "enabled"),
                    masked("m2", "m2@x.io", "", "", "disabled"),
                    masked("m3", "m3@x.io", "Forum", "", "enabled"),
                    masked("dup", "h@x.io", "Dup", "", "enabled"),
                ],
            ),
            (
                "work".to_string(),
                vec![masked("w1", "w1@x.io", "Work shop", "", "enabled")],
            ),
            (
                "off".to_string(),
                vec![masked("o1", "o1@x.io", "Hidden", "", "enabled")],
            ),
            (
                "ghost".to_string(),
                vec![masked("g1", "g1@x.io", "Ghost", "", "enabled")],
            ),
        ]);
        let options = from_options_for(&cfg, &identities);
        let groups = masked_groups(&cfg, &options, &masked);

        Model {
            cfg,
            masked,
            options,
            groups,
        }
    }

    #[test]
    fn masked_groups_follow_the_account_address() {
        let m = model(true);
        let summary = |groups: &[MaskedGroup]| {
            groups
                .iter()
                .map(|g| {
                    (
                        g.account_id.clone(),
                        g.parent,
                        g.list.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };

        // Options: Other (0), Home (1), Work (2); the group follows Home, not Other.
        assert_eq!(
            summary(&m.groups),
            [
                (
                    "default".to_string(),
                    1,
                    vec!["m1".into(), "m2".into(), "m3".into()]
                ),
                ("work".to_string(), 2, vec!["w1".into()]),
            ]
        );

        let bare = model(false);

        assert_eq!(
            summary(&bare.groups),
            [(
                "default".to_string(),
                1,
                vec!["m1".into(), "m2".into(), "m3".into()]
            )]
        );
    }

    #[test]
    fn masked_rows_sit_under_the_account_address() {
        let m = model(true);
        let rows = |open, query| menu_rows(&m.options, &m.groups, open, query);
        let masked_rows = |indexes: &[usize]| {
            indexes
                .iter()
                .map(|&index| MenuRow::Masked { group: 0, index })
                .collect::<Vec<_>>()
        };
        let under = |mut head: Vec<MenuRow>, middle: Vec<MenuRow>| {
            head.extend(middle);
            head.extend([
                MenuRow::Identity(2),
                MenuRow::Group {
                    group: 1,
                    open: false,
                },
            ]);

            head
        };
        let top = vec![MenuRow::Identity(0), MenuRow::Identity(1)];
        let open = |mut rest: Vec<MenuRow>| {
            let mut v = vec![
                MenuRow::Group {
                    group: 0,
                    open: true,
                },
                MenuRow::Filter { group: 0 },
            ];

            v.append(&mut rest);

            v
        };

        assert_eq!(
            rows(None, ""),
            [
                MenuRow::Identity(0),
                MenuRow::Identity(1),
                MenuRow::Group {
                    group: 0,
                    open: false
                },
                MenuRow::Identity(2),
                MenuRow::Group {
                    group: 1,
                    open: false
                },
            ]
        );
        assert_eq!(
            rows(Some(0), ""),
            under(top.clone(), open(masked_rows(&[0, 1, 2])))
        );
        assert_eq!(
            rows(Some(0), "FORUM"),
            under(top.clone(), open(masked_rows(&[2])))
        );
        assert_eq!(
            rows(Some(0), "zzz"),
            under(top, open(vec![MenuRow::NoMatch { group: 0 }]))
        );
    }

    #[test]
    fn masked_filter_matches_description_domain_and_email() {
        let list = &model(true).groups[0].list;

        assert_eq!(filter_masked(list, "shop.example"), [0]);
        assert_eq!(filter_masked(list, "m2@"), [1]);
        assert_eq!(filter_masked(list, "  "), [0, 1, 2]);
    }

    #[test]
    fn resolve_from_prefers_the_draft_account() {
        let m = model(true);
        let draft = |from: Option<&str>| Draft {
            account_id: "default".into(),
            from: from.map(|email| Address {
                name: None,
                email: email.into(),
            }),
            ..Draft::default()
        };
        let resolve =
            |from: Option<&str>| resolve_from(&m.cfg, &m.options, &m.groups, &draft(from));

        assert_eq!(
            resolve(Some("M1@x.io")),
            Some(Resolved::Masked { group: 0, index: 0 })
        );
        // An identity wins over the masked duplicate, which is not offered.
        assert_eq!(resolve(Some("h@x.io")), Some(Resolved::Identity(1)));
        assert_eq!(
            resolve(Some("w1@x.io")),
            Some(Resolved::Masked { group: 1, index: 0 })
        );
        assert_eq!(resolve(Some("o@x.io")), None);
        assert_eq!(resolve(Some("g1@x.io")), None);
        assert_eq!(resolve(None), Some(Resolved::Identity(1)));

        // Masked in `default`, an identity of `work`: the draft's account wins.
        let mut shared = m.masked.clone();

        shared
            .get_mut("default")
            .unwrap()
            .push(masked("sh", "w@x.io", "Shared", "", "enabled"));

        let groups = masked_groups(&m.cfg, &m.options, &shared);

        assert_eq!(
            resolve_from(&m.cfg, &m.options, &groups, &draft(Some("w@x.io"))),
            Some(Resolved::Masked { group: 0, index: 3 })
        );
    }

    #[test]
    fn from_label_shows_a_masked_from() {
        let m = model(true);
        let draft = |from: Option<&str>| Draft {
            account_id: "default".into(),
            from: from.map(|email| Address {
                name: None,
                email: email.into(),
            }),
            ..Draft::default()
        };
        let label = |cfg: &Config, r: Option<Resolved>, d: &Draft| {
            from_label(cfg, &m.options, &m.groups, r.as_ref(), d)
        };
        let shop = Some(Resolved::Masked { group: 0, index: 0 });
        let plain = draft(None);

        assert_eq!(label(&m.cfg, shop.clone(), &plain), "Shop <m1@x.io> — Home");

        let mut one = m.cfg.clone();

        one.accounts[1].enabled = false;

        assert_eq!(label(&one, shop, &plain), "Shop <m1@x.io>");
        assert_eq!(
            label(&one, Some(Resolved::Masked { group: 0, index: 1 }), &plain),
            "m2@x.io <m2@x.io>"
        );
        assert_eq!(label(&m.cfg, None, &draft(Some("z@x.io"))), "z@x.io");
        assert_eq!(label(&m.cfg, None, &plain), "Default Fastmail address");
    }

    #[test]
    fn only_plain_enter_picks_a_masked_address() {
        assert!(picks_first_match(&Key::Enter, Modifiers::empty()));

        for held in [
            Modifiers::CONTROL,
            Modifiers::SHIFT,
            Modifiers::ALT,
            Modifiers::META,
        ] {
            assert!(!picks_first_match(&Key::Enter, held), "{held:?}");
        }

        assert!(!picks_first_match(&Key::Escape, Modifiers::empty()));
    }

    /// An `App` over an in-memory store; `seed` fills the store before the
    /// first render, as the app's start does.
    fn mount(cfg: Config, seed: impl FnOnce(&Store)) -> VirtualDom {
        let store = Arc::new(Store::open_memory().unwrap());

        seed(&store);

        let mut dom = VirtualDom::new(App).with_root_context(Backend {
            store,
            clients: Default::default(),
            cfg,
            ui_test: true,
            conn_rev: 0,
            config_error: None,
        });

        dom.rebuild_in_place();

        dom
    }

    /// `default` (Home) and `work` (Work), the first called Home.
    fn two_accounts() -> Config {
        let mut cfg = Config::default();

        cfg.accounts[0].email = "h@x.io".into();
        cfg.accounts[0].name = "Home".into();
        cfg.add_account(Account::new("work", "w@x.io")).unwrap();
        cfg.accounts[1].name = "Work".into();

        cfg
    }

    /// Lets the app run its effects and tasks, then renders.
    async fn pump(dom: &mut VirtualDom) -> Vec<Mutation> {
        let _ = tokio::time::timeout(Duration::from_millis(50), dom.wait_for_work()).await;

        dom.render_immediate_to_vec().edits
    }

    /// The text nodes a batch of edits creates or sets.
    fn texts(edits: &[Mutation]) -> Vec<String> {
        edits
            .iter()
            .filter_map(|m| match m {
                Mutation::CreateTextNode { value, .. } | Mutation::SetText { value, .. } => {
                    Some(value.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typing_and_rerenders_never_push_a_field_value() {
        let mut dom = mount(two_accounts(), |_| {});
        let _guard = RuntimeGuard::new(dom.runtime());

        open_composer(Draft {
            account_id: "default".into(),
            subject: "Re: hi".into(),
            body: "> quoted".into(),
            ..Draft::default()
        });
        pump(&mut dom).await;
        // What a keystroke does, then a status line from a sync.
        edit(|d| d.body = "x\n> quoted".into());
        *COMPOSER_TO.write() = "a@x.io".into();
        *STATUS.write() = "Synced".into();

        let edits = pump(&mut dom).await;

        assert!(texts(&edits).contains(&"Synced".to_string()));
        assert!(
            !edits.iter().any(
                |m| matches!(m, Mutation::SetAttribute { name, .. } if name.contains("value"))
            ),
            "a re-render reset a field the user is typing in"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn escape_closes_the_from_menu_before_the_composer() {
        let mut dom = mount(two_accounts(), |_| {});
        let _guard = RuntimeGuard::new(dom.runtime());

        *COMPOSER.write() = Some(Draft {
            account_id: "default".into(),
            ..Draft::default()
        });
        pump(&mut dom).await;
        *FROM_MENU.write() = true;
        *TYPING.write() = true;
        dom.in_scope(ScopeId::ROOT, || dispatch(Action::Escape));

        assert!(COMPOSER.peek().is_some());
        assert!(!*FROM_MENU.peek());
        assert!(!*TYPING.peek());

        dom.in_scope(ScopeId::ROOT, || dispatch(Action::Escape));

        assert!(COMPOSER.peek().is_none());

        *FROM_MENU.write() = true;
        *TYPING.write() = true;
        dom.in_scope(ScopeId::ROOT, || open_composer(Draft::default()));

        assert!(!*FROM_MENU.peek());
        assert!(
            !*TYPING.peek(),
            "the replaced filter's typing state is cleared"
        );

        *FROM_MENU.write() = true;
        *COMPOSER_TO.write() = String::new();
        // Focus and `run` need a current scope, which an event handler would provide.
        dom.in_scope(ScopeId::ROOT, || send(false));

        assert!(!*FROM_MENU.peek());

        pump(&mut dom).await;
        pump(&mut dom).await;

        assert_eq!(*STATUS.peek(), "Add at least one recipient.");
        assert!(COMPOSER.peek().is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn from_menu_closes_when_identities_vanish() {
        let mut dom = mount(two_accounts(), |store| {
            store
                .set_identities("default", &[identity("Home", "h@x.io")])
                .unwrap();
        });
        let _guard = RuntimeGuard::new(dom.runtime());

        pump(&mut dom).await;
        *COMPOSER.write() = Some(Draft {
            account_id: "default".into(),
            ..Draft::default()
        });
        pump(&mut dom).await;
        *FROM_MENU.write() = true;
        *TYPING.write() = true;
        pump(&mut dom).await;

        assert!(
            *FROM_MENU.peek(),
            "the menu stays open while identities exist"
        );

        *IDENTITIES.write() = BTreeMap::new();
        pump(&mut dom).await;
        pump(&mut dom).await;

        assert!(!*FROM_MENU.peek());
        assert!(!*TYPING.peek());
    }

    /// A rendered app plus the element ids of the From control's `data-from`
    /// elements, kept current from the edits of every render.
    struct View {
        dom: VirtualDom,
        ids: HashMap<String, ElementId>,
        /// The text nodes created or set since the last event was delivered.
        shown: Vec<String>,
    }

    impl View {
        /// Pumps the app and folds the edits into `ids`.
        async fn pump(&mut self) {
            let edits = pump(&mut self.dom).await;

            self.shown.extend(texts(&edits));

            for edit in edits {
                match edit {
                    Mutation::SetAttribute {
                        name: "data-from",
                        value: AttributeValue::Text(key),
                        id,
                        ..
                    } => {
                        self.ids.insert(key, id);
                    }
                    Mutation::Remove { id } | Mutation::ReplaceWith { id, .. } => {
                        // The menu leaves with its scrim; its rows go without a mutation.
                        if self.ids.get("scrim") == Some(&id) {
                            self.ids.retain(|k, _| k == "button");
                        }

                        self.ids.retain(|_, v| *v != id);
                    }
                    _ => {}
                }
            }
        }

        fn has(&self, key: &str) -> bool {
            self.ids.contains_key(key)
        }

        /// Delivers `payload` as event `name` to the element called `key`.
        async fn send(&mut self, name: &str, key: &str, payload: PlatformEventData) {
            let id = *self
                .ids
                .get(key)
                .unwrap_or_else(|| panic!("no element {key}"));
            let data: Rc<dyn std::any::Any> = Rc::new(payload);

            self.shown.clear();
            self.dom
                .runtime()
                .handle_event(name, Event::new(data, true), id);
            self.pump().await;
            self.pump().await;
        }

        async fn click(&mut self, key: &str) {
            let payload = PlatformEventData::new(Box::new(SerializedMouseData::default()));

            self.send("click", key, payload).await;
        }

        async fn input(&mut self, key: &str, value: &str) {
            let payload =
                PlatformEventData::new(Box::new(SerializedFormData::new(value.into(), vec![])));

            self.send("input", key, payload).await;
        }

        async fn keydown(&mut self, key: &str, pressed: Key, code: Code, held: Modifiers) {
            let payload = PlatformEventData::new(Box::new(SerializedKeyboardData::new(
                pressed,
                code,
                Location::Standard,
                false,
                held,
                false,
            )));

            self.send("keydown", key, payload).await;
        }
    }

    fn sender(email: &str, name: Option<&str>) -> Option<Address> {
        Some(Address {
            name: name.map(String::from),
            email: email.into(),
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn from_menu_picks_a_masked_address_by_click_and_enter() {
        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));

        let dom = mount(two_accounts(), |store| {
            store
                .set_identities("default", &[identity("Home", "h@x.io")])
                .unwrap();
            store
                .set_identities("work", &[identity("Work", "w@x.io")])
                .unwrap();
            store
                .set_masked_emails("default", &[masked("m1", "m1@x.io", "Shop", "", "enabled")])
                .unwrap();
            store
                .set_masked_emails(
                    "work",
                    &[
                        masked("w1", "w1@x.io", "Work shop", "", "enabled"),
                        masked("w2", "w2@x.io", "Travel", "", "enabled"),
                    ],
                )
                .unwrap();
        });
        let _guard = RuntimeGuard::new(dom.runtime());
        let mut view = View {
            dom,
            ids: HashMap::new(),
            shown: Vec::new(),
        };
        let from = || COMPOSER.peek().as_ref().unwrap().from.clone();
        let account = || COMPOSER.peek().as_ref().unwrap().account_id.clone();

        *COMPOSER.write() = Some(Draft {
            account_id: "default".into(),
            ..Draft::default()
        });
        view.pump().await;

        // 1. The button opens the menu; the group row opens the work group.
        view.click("button").await;

        assert!(*FROM_MENU.peek());
        assert!(view.has("group:work"));
        assert!(!view.has("filter"));

        view.click("group:work").await;

        assert!(view.has("filter"));
        assert!(view.has("masked:work/w1@x.io"));
        assert!(view.has("masked:work/w2@x.io"));

        // 2. Typing filters the rows.
        view.input("filter", "trav").await;

        assert!(view.has("masked:work/w2@x.io"), "the match stays");
        assert!(!view.has("masked:work/w1@x.io"), "the other row goes");

        // 3. Plain Enter picks the first match.
        view.keydown("filter", Key::Enter, Code::Enter, Modifiers::empty())
            .await;

        assert_eq!(account(), "work");
        assert_eq!(from(), sender("w2@x.io", None));
        assert!(!*FROM_MENU.peek());

        // 4. The From is masked, so its group starts open; no match picks nothing.
        view.click("button").await;

        assert!(view.has("filter"));

        view.input("filter", "zzz").await;

        assert!(
            view.shown.iter().any(|t| t == "No matching masked emails"),
            "{:?}",
            view.shown
        );
        view.keydown("filter", Key::Enter, Code::Enter, Modifiers::empty())
            .await;

        assert_eq!(from(), sender("w2@x.io", None));
        assert!(*FROM_MENU.peek());

        // 5. A click outside closes the menu and leaves the draft alone.
        view.click("scrim").await;

        assert!(!*FROM_MENU.peek());
        assert!(COMPOSER.peek().is_some());
        assert_eq!(from(), sender("w2@x.io", None));

        // 6. Ctrl+Enter in the filter reaches the root and sends: the menu closes
        // first, and a validation failure leaves the composer open.
        *COMPOSER_TO.write() = String::new();
        view.click("button").await;

        assert!(
            view.has("filter"),
            "the group of the masked From starts open"
        );

        view.keydown("filter", Key::Enter, Code::Enter, Modifiers::CONTROL)
            .await;

        assert!(!*FROM_MENU.peek());
        assert_eq!(from(), sender("w2@x.io", None));
        assert_eq!(*STATUS.peek(), "Add at least one recipient.");
        assert!(COMPOSER.peek().is_some());

        // 7. Clicking a masked row picks it.
        view.click("button").await;
        view.click("masked:work/w1@x.io").await;

        assert_eq!(account(), "work");
        assert_eq!(from(), sender("w1@x.io", None));
        assert!(!*FROM_MENU.peek());

        // 8. An identity row picks the identity, with its name.
        view.click("button").await;
        view.click("id:default/h@x.io").await;

        assert_eq!(account(), "default");
        assert_eq!(from(), sender("h@x.io", Some("Home")));
        assert!(!*FROM_MENU.peek());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn composer_shows_a_masked_from_on_the_button() {
        let mut dom = mount(two_accounts(), |store| {
            store
                .set_identities("default", &[identity("Home", "h@x.io")])
                .unwrap();
            store
                .set_masked_emails("default", &[masked("m1", "m1@x.io", "Shop", "", "enabled")])
                .unwrap();
        });
        let _guard = RuntimeGuard::new(dom.runtime());

        pump(&mut dom).await;
        *COMPOSER.write() = Some(Draft {
            account_id: "default".into(),
            from: sender("m1@x.io", None),
            ..Draft::default()
        });

        let shown = texts(&pump(&mut dom).await);

        assert!(
            shown.iter().any(|t| t.contains("Shop <m1@x.io>")),
            "{shown:?}"
        );
    }

    /// The real composer, in an app of two accounts, shows the identity of a
    /// `from` that belongs to the other account on the From button, and sends
    /// from it.
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

        let work_name = cfg.account("work").unwrap().display_name().to_string();
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

        let shown = texts(&dom.render_immediate_to_vec().edits);
        let expected = format!("Work <w@x.io> — {work_name}");

        assert!(shown.contains(&expected), "{shown:?}");

        // The effect makes the From's account the sender.
        let _ = tokio::time::timeout(Duration::from_millis(200), dom.wait_for_work()).await;

        assert_eq!(COMPOSER.peek().as_ref().unwrap().account_id, "work");
    }
}
