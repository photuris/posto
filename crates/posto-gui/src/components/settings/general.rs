//! General page: accounts (provider, credentials, sender, endpoint) and refresh.

use dioxus::prelude::*;
use posto_core::config::{Account, Config, ProtonSecret, Provider, Secrets};
use posto_core::proton::{self, Login};

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

/// The provider as the user knows it.
fn provider_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Jmap => "Fastmail (JMAP)",
        Provider::Proton => "Proton Mail",
    }
}

/// The Proton sign-in panel and the sign-in it may have running.
///
/// `attempt` numbers sign-ins: it is bumped when one starts and whenever the
/// panel is closed or the selected account changes, so a login that returns
/// after that finds its number stale and stores nothing.
#[derive(Clone, Copy)]
struct SignIn {
    open: Signal<bool>,
    password: Signal<String>,
    totp: Signal<String>,
    separate: Signal<bool>,
    mailbox: Signal<String>,
    error: Signal<Option<String>>,
    busy: Signal<bool>,
    attempt: Signal<u32>,
    /// Bumped after a stored login, to re-read the signed-in state.
    rev: Signal<u32>,
}

impl SignIn {
    /// Creates the signals in the calling component's scope.
    fn new() -> SignIn {
        SignIn {
            open: use_signal(|| false),
            password: use_signal(String::new),
            totp: use_signal(String::new),
            separate: use_signal(|| false),
            mailbox: use_signal(String::new),
            error: use_signal(|| None),
            busy: use_signal(|| false),
            attempt: use_signal(|| 0),
            rev: use_signal(|| 0),
        }
    }

    /// Closes the panel, forgets what was typed and discards a pending sign-in.
    fn reset(mut self) {
        self.attempt += 1;
        self.open.set(false);
        self.busy.set(false);
        self.error.set(None);
        self.clear_inputs();
    }

    /// Opens the empty panel, or closes it.
    fn toggle(mut self) {
        let was_open = *self.open.peek();

        self.reset();
        self.open.set(!was_open);
    }

    fn clear_inputs(mut self) {
        self.password.set(String::new());
        self.totp.set(String::new());
        self.separate.set(false);
        self.mailbox.set(String::new());
    }

    /// Signs `account` in with what is typed; the result lands in the secrets
    /// file, unless the attempt was discarded meanwhile.
    fn start(mut self, draft: Signal<Config>, account: Account) {
        if (self.busy)() {
            return;
        }

        if backend().peek().ui_test {
            set_status("UI test mode: nothing is stored.");
            return self.reset();
        }

        let password = (self.password)();

        if password.is_empty() {
            return self.error.set(Some("Enter the password".into()));
        }

        let Some(version) = backend().peek().cfg.proton_app_version() else {
            return self
                .error
                .set(Some("Set [proton] app_version in config.toml first".into()));
        };
        let totp = Some((self.totp)().trim().to_string()).filter(|t| !t.is_empty());
        let mailbox = (self.separate)()
            .then(|| (self.mailbox)())
            .filter(|m| !m.is_empty());

        self.attempt += 1;
        self.busy.set(true);
        self.error.set(None);

        let mine = *self.attempt.peek();

        // Component-owned: closing Settings drops it before it can store anything.
        spawn(async move {
            let result = proton::login(
                &version,
                account.login_name(),
                &password,
                totp.as_deref(),
                mailbox.as_deref(),
            )
            .await;

            if *self.attempt.peek() != mine {
                return;
            }

            self.busy.set(false);
            self.clear_inputs();

            match result.and_then(|login| {
                let Login {
                    session,
                    mailbox_password,
                    email,
                } = login;

                Secrets::set_proton(
                    &account.id,
                    &ProtonSecret {
                        session,
                        mailbox_password,
                        generation: 0,
                    },
                )
                .map(|_| email)
            }) {
                Ok(email) => self.signed_in(draft, &account.id, &email),
                Err(e) => self.error.set(Some(e.to_string())),
            }
        });
    }

    /// After the login was stored: closes the panel, drops old connections
    /// and fills in a missing sender address.
    fn signed_in(mut self, mut draft: Signal<Config>, id: &str, email: &str) {
        self.open.set(false);
        self.error.set(None);
        self.rev += 1;
        *NEEDS_TOKEN.write() = false;

        if !email.is_empty()
            && let Some(a) = draft.write().accounts.iter_mut().find(|a| a.id == id)
            && a.email.trim().is_empty()
        {
            a.email = email.to_string();
        }

        let mut shared = backend();
        let mut b = shared.write();
        let filled = !email.is_empty()
            && b.cfg
                .accounts
                .iter_mut()
                .find(|a| a.id == id)
                .is_some_and(|a| {
                    let empty = a.email.is_empty();

                    if empty {
                        a.email = email.to_string();
                    }

                    empty
                });
        let save_error = if filled && b.config_error.is_none() {
            b.cfg.save().err()
        } else {
            None
        };
        let name = b
            .cfg
            .account(id)
            .map_or(id, Account::display_name)
            .to_string();

        b.reset_client();
        drop(b);

        match save_error {
            Some(e) => set_status(format!("Signed in, but config.toml was not saved: {e}")),
            None => set_status(format!("Signed in to {name}")),
        }

        sync_soon();
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
    let sign_in = SignIn::new();

    // Re-read the credential state after a save.
    let _ = token_rev();
    let _ = (sign_in.rev)();

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
    let signed_in = !ui_test && saved && matches!(Secrets::proton(&account.id), Ok(Some(_)));
    let proton = account.provider == Provider::Proton;
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
                            sign_in.reset();
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
                        sign_in.reset();
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
                    label { "Provider" }
                    if saved {
                        span { class: "muted", "{provider_label(account.provider)}" }
                    } else {
                        select {
                            value: if proton { "proton" } else { "jmap" },
                            onchange: move |evt| {
                                edit(
                                    draft,
                                    at,
                                    |a| {
                                        a.provider = if evt.value() == "proton" {
                                            Provider::Proton
                                        } else {
                                            Provider::Jmap
                                        };
                                        a.session_url = Account::default().session_url;
                                    },
                                );
                            },
                            option { value: "jmap", "{provider_label(Provider::Jmap)}" }
                            option { value: "proton", "{provider_label(Provider::Proton)}" }
                        }
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
                div { class: "hint",
                    if proton {
                        "Replies and drafts use the Proton address matching this address."
                    } else {
                        "Replies and drafts use the Fastmail identity matching this address."
                    }
                }
                if proton {
                    div { class: "setting-row",
                        label { "Username" }
                        input {
                            r#type: "text",
                            placeholder: "login name; empty = the email address",
                            value: "{account.username}",
                            onfocusin: focus_in,
                            onfocusout: focus_out,
                            oninput: move |evt| edit(draft, at, |a| a.username = evt.value()),
                        }
                    }
                } else {
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
                        if proton {
                            "Lower-case letters, digits and '-'. Save the settings, then sign in."
                        } else {
                            "Lower-case letters, digits and '-'. Save the settings, then set the token."
                        }
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

                if proton {
                    div { class: "setting-row",
                        span { "Proton sign-in" }
                        span { class: "muted",
                            if signed_in {
                                "Signed in"
                            } else {
                                "Not signed in"
                            }
                        }
                        button {
                            disabled: !saved,
                            onclick: move |_| sign_in.toggle(),
                            "Sign in…"
                        }
                    }
                    if (sign_in.open)() && saved {
                        div { class: "setting-row",
                            input {
                                r#type: "password",
                                placeholder: "Password",
                                value: "{sign_in.password}",
                                onmounted: |evt| async move {
                                    let _ = evt.set_focus(true).await;
                                },
                                onfocusin: focus_in,
                                onfocusout: focus_out,
                                oninput: move |evt| {
                                    let mut password = sign_in.password;

                                    password.set(evt.value());
                                },
                            }
                        }
                        div { class: "setting-row",
                            input {
                                r#type: "text",
                                placeholder: "Two-factor code (if enabled)",
                                value: "{sign_in.totp}",
                                onfocusin: focus_in,
                                onfocusout: focus_out,
                                oninput: move |evt| {
                                    let mut totp = sign_in.totp;

                                    totp.set(evt.value());
                                },
                            }
                        }
                        div { class: "setting-row",
                            label { "Separate mailbox password" }
                            input {
                                r#type: "checkbox",
                                checked: (sign_in.separate)(),
                                onchange: move |evt| {
                                    let mut separate = sign_in.separate;

                                    separate.set(evt.checked());
                                },
                            }
                        }
                        if (sign_in.separate)() {
                            div { class: "setting-row",
                                input {
                                    r#type: "password",
                                    placeholder: "Mailbox password",
                                    value: "{sign_in.mailbox}",
                                    onfocusin: focus_in,
                                    onfocusout: focus_out,
                                    oninput: move |evt| {
                                        let mut mailbox = sign_in.mailbox;

                                        mailbox.set(evt.value());
                                    },
                                }
                            }
                        }
                        div { class: "setting-row",
                            button {
                                disabled: (sign_in.busy)(),
                                onclick: {
                                    let account = account.clone();

                                    move |_| sign_in.start(draft, account.clone())
                                },
                                "Sign in"
                            }
                        }
                    }
                    if let Some(e) = (sign_in.error)() {
                        div { class: "error", "{e}" }
                    }
                    div { class: "hint",
                        "Posto stores the session and the mailbox password in secrets.toml (mode 0600)."
                    }
                } else {
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

                }

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
                                sign_in.reset();
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
