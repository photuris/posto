//! General page: accounts (provider, credentials, sender, endpoint) and refresh.

use dioxus::core::Task;
use dioxus::prelude::*;
use posto_core::config::{Account, Config, ProtonSecret, Provider, Secrets};
use posto_core::gmail::{self, GoogleClient};
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
        Provider::Gmail => "Gmail",
    }
}

/// A checked sign-in, ready to run.
struct Job {
    version: String,
    password: String,
    totp: Option<String>,
    mailbox: Option<String>,
    attempt: u32,
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

    fn clear_passwords(mut self) {
        self.password.set(String::new());
        self.mailbox.set(String::new());
    }

    fn clear_inputs(mut self) {
        self.clear_passwords();
        self.totp.set(String::new());
        self.separate.set(false);
    }

    /// Signs `account` in with what is typed; the result lands in the secrets
    /// file, unless the attempt was discarded meanwhile.
    fn start(self, draft: Signal<Config>, account: Account) {
        if (self.busy)() {
            return;
        }

        if backend().peek().ui_test {
            set_status("UI test mode: nothing is stored.");
            return self.reset();
        }

        let Some(job) = self.prepare(backend().peek().cfg.proton_app_version()) else {
            return;
        };

        // Component-owned: closing Settings drops it before it can store anything.
        spawn(async move {
            let result = proton::login(
                &job.version,
                account.login_name(),
                &job.password,
                job.totp.as_deref(),
                job.mailbox.as_deref(),
            )
            .await;

            self.finish(draft, &account.id, job.attempt, result, Secrets::set_proton);
        });
    }

    /// Checks what is typed and, when a login can run, numbers it and marks
    /// the panel busy. A refusal shows its error with the panel left open and
    /// both passwords cleared.
    fn prepare(mut self, version: Option<String>) -> Option<Job> {
        match self.preflight(version) {
            Ok(job) => {
                self.attempt += 1;
                self.busy.set(true);
                self.error.set(None);

                Some(Job {
                    attempt: *self.attempt.peek(),
                    ..job
                })
            }
            Err(e) => {
                self.clear_passwords();
                self.error.set(Some(e.into()));
                None
            }
        }
    }

    fn preflight(self, version: Option<String>) -> std::result::Result<Job, &'static str> {
        let password = (self.password)();

        if password.is_empty() {
            return Err("Enter the password");
        }

        let version = version.ok_or("Set [proton] app_version in config.toml first")?;

        Ok(Job {
            version,
            password,
            totp: Some((self.totp)().trim().to_string()).filter(|t| !t.is_empty()),
            mailbox: (self.separate)()
                .then(|| (self.mailbox)())
                .filter(|m| !m.is_empty()),
            attempt: 0,
        })
    }

    /// Lands the result of login number `attempt`: stores the login through
    /// `store` and shows it, or shows the error. Does nothing when the
    /// attempt was discarded meanwhile.
    fn finish(
        mut self,
        draft: Signal<Config>,
        id: &str,
        attempt: u32,
        result: posto_core::Result<Login>,
        store: impl FnOnce(&str, &ProtonSecret) -> posto_core::Result<u64>,
    ) {
        if *self.attempt.peek() != attempt {
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

            store(
                id,
                &ProtonSecret {
                    session,
                    mailbox_password,
                    generation: 0,
                },
            )
            .map(|_| email)
        }) {
            Ok(email) => self.signed_in(draft, id, &email),
            Err(e) => self.error.set(Some(e.to_string())),
        }
    }

    /// After the login was stored: closes the panel, then the shared ending.
    fn signed_in(mut self, draft: Signal<Config>, id: &str, email: &str) {
        self.open.set(false);
        self.error.set(None);
        self.rev += 1;
        finish_sign_in(draft, id, email);
    }
}

/// The ending every sign-in shares once its credential is stored: clears the
/// missing-token banner, fills an empty sender address (draft and backend,
/// saved when the config file is writable), drops old connections and starts
/// a sync.
fn finish_sign_in(mut draft: Signal<Config>, id: &str, email: &str) {
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

/// Opens `url` in the user's browser. A failure is ignored: the panel shows
/// the address too.
fn open_browser(url: &str) {
    let child = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    if let Ok(mut child) = child {
        // Reaps the child so it does not stay a zombie.
        std::thread::spawn(move || child.wait());
    }
}

/// The Google sign-in panel and the sign-in it may have running.
///
/// `attempt` numbers sign-ins like [`SignIn`]'s does; `task` holds the running
/// login so Cancel drops its future and with it the loopback listener.
#[derive(Clone, Copy)]
struct GoogleSignIn {
    busy: Signal<bool>,
    /// The consent address, once the login has built it.
    url: Signal<Option<String>>,
    error: Signal<Option<String>>,
    attempt: Signal<u32>,
    /// Bumped after a stored login, to re-read the signed-in state.
    rev: Signal<u32>,
    task: Signal<Option<Task>>,
}

impl GoogleSignIn {
    /// Creates the signals in the calling component's scope.
    fn new() -> GoogleSignIn {
        GoogleSignIn {
            busy: use_signal(|| false),
            url: use_signal(|| None),
            error: use_signal(|| None),
            attempt: use_signal(|| 0),
            rev: use_signal(|| 0),
            task: use_signal(|| None),
        }
    }

    /// Discards a running sign-in: its result will be stale and its future is
    /// dropped.
    fn cancel(mut self) {
        self.attempt += 1;
        self.busy.set(false);
        self.url.set(None);
        self.error.set(None);

        let task = self.task.write().take();

        if let Some(task) = task {
            task.cancel();
        }
    }

    /// Signs in the saved Gmail account `id`: opens Google's consent page and
    /// stores the refresh token the login returns, unless the attempt was
    /// discarded meanwhile.
    fn start(mut self, draft: Signal<Config>, id: &str) {
        if *self.busy.peek() {
            return;
        }

        // The saved account, not the draft: an unsaved edit must not choose
        // the Google account.
        let saved = backend().peek().cfg.account(id).cloned();
        let Some(saved) = saved.filter(|a| a.provider == Provider::Gmail) else {
            return;
        };

        if backend().peek().ui_test {
            return set_status("UI test mode: nothing is stored.");
        }

        let client = match GoogleClient::load(&posto_core::config::google_client_path()) {
            Ok(client) => client,
            Err(e) => return self.error.set(Some(e.to_string())),
        };

        self.attempt += 1;
        self.busy.set(true);
        self.error.set(None);
        self.url.set(None);

        let attempt = *self.attempt.peek();
        let id = saved.id.clone();
        let mut url = self.url;
        // Component-owned: closing Settings drops it before it can store anything.
        let task = spawn(async move {
            let result = gmail::login(&client, &saved.email, |u| {
                url.set(Some(u.to_string()));
                open_browser(u);
            })
            .await;

            self.finish(draft, &id, attempt, result, Secrets::set_gmail);
        });

        self.task.set(Some(task));
    }

    /// Lands the result of login number `attempt`: stores the refresh token
    /// through `store` and finishes the sign-in, or shows the error. Does
    /// nothing when the attempt was discarded meanwhile.
    fn finish(
        mut self,
        draft: Signal<Config>,
        id: &str,
        attempt: u32,
        result: posto_core::Result<gmail::Login>,
        store: impl FnOnce(&str, &str) -> posto_core::Result<()>,
    ) {
        if *self.attempt.peek() != attempt {
            return;
        }

        self.busy.set(false);
        self.url.set(None);
        self.task.set(None);

        let still_gmail = backend()
            .peek()
            .cfg
            .account(id)
            .is_some_and(|a| a.provider == Provider::Gmail);

        if !still_gmail {
            return self.error.set(Some(
                "The account changed during sign-in; nothing was stored.".into(),
            ));
        }

        match result.and_then(|login| store(id, &login.refresh_token).map(|_| login.email)) {
            Ok(email) => {
                self.error.set(None);
                self.rev += 1;
                finish_sign_in(draft, id, &email);
            }
            Err(e) => self.error.set(Some(e.to_string())),
        }
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
    let google = GoogleSignIn::new();

    // Re-read the credential state after a save.
    let _ = token_rev();
    let _ = (sign_in.rev)();
    let _ = (google.rev)();

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
    let google_signed_in = !ui_test && saved && matches!(Secrets::gmail(&account.id), Ok(Some(_)));
    let proton = account.provider == Provider::Proton;
    let gmail = account.provider == Provider::Gmail;
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
                            google.cancel();
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
                        google.cancel();
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
                            value: account.provider.as_str(),
                            onchange: move |evt| {
                                edit(
                                    draft,
                                    at,
                                    |a| {
                                        a.provider = match evt.value().as_str() {
                                            "proton" => Provider::Proton,
                                            "gmail" => Provider::Gmail,
                                            _ => Provider::Jmap,
                                        };
                                        a.session_url = Account::default().session_url;
                                    },
                                );
                            },
                            option { value: "jmap", "{provider_label(Provider::Jmap)}" }
                            option { value: "proton", "{provider_label(Provider::Proton)}" }
                            option { value: "gmail", "{provider_label(Provider::Gmail)}" }
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
                    } else if gmail {
                        "Replies and drafts use the Gmail send-as address matching this address. Empty: filled in at sign-in."
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
                } else if !gmail {
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
                        } else if gmail {
                            "Lower-case letters, digits and '-'. Save the settings, then sign in with Google."
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
                } else if gmail {
                    div { class: "setting-row",
                        span { "Google sign-in" }
                        span { class: "muted",
                            if google_signed_in {
                                "Signed in"
                            } else {
                                "Not signed in"
                            }
                        }
                        button {
                            disabled: !saved || (google.busy)(),
                            onclick: {
                                let id = account.id.clone();

                                move |_| google.start(draft, &id)
                            },
                            "Sign in with Google…"
                        }
                    }
                    if (google.busy)() {
                        div { class: "hint",
                            "Finish signing in in your browser. If it did not open, open this address:"
                        }
                        div { class: "setting-row",
                            if let Some(u) = (google.url)() {
                                input {
                                    r#type: "text",
                                    readonly: true,
                                    value: "{u}",
                                    onfocusin: focus_in,
                                    onfocusout: focus_out,
                                }
                            }
                            button { onclick: move |_| google.cancel(), "Cancel" }
                        }
                    }
                    if let Some(e) = (google.error)() {
                        div { class: "error", "{e}" }
                    }
                    div { class: "hint",
                        "Posto stores the refresh token in secrets.toml (mode 0600)."
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
                                google.cancel();
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::Arc;

    use posto_core::Error;
    use posto_core::store::Store;

    use super::*;
    use crate::app::{App, STATUS};
    use crate::state::Backend;

    /// A headless `App` over an empty backend whose config has the Proton
    /// account `pm` (no sender address). `config_error` is set so that a
    /// sign-in can never write `config.toml`.
    fn rendered() -> VirtualDom {
        rendered_with(Account::proton("pm", ""))
    }

    /// Like [`rendered`], with `account` as the only account.
    fn rendered_with(account: Account) -> VirtualDom {
        let mut cfg = Config::default();

        cfg.accounts[0] = account;

        let mut dom = VirtualDom::new(App).with_root_context(Backend {
            store: Arc::new(Store::open_memory().unwrap()),
            clients: HashMap::new(),
            cfg,
            ui_test: false,
            conn_rev: 0,
            config_error: Some("test: never write config.toml".into()),
        });

        dom.rebuild_in_place();
        dom
    }

    /// The sign-in signals of a mounted `GeneralPage`, with a password typed.
    fn typed() -> SignIn {
        fn new<T: 'static>(value: T) -> Signal<T> {
            Signal::new_in_scope(value, ScopeId::ROOT)
        }

        let mut s = SignIn {
            open: new(true),
            password: new(String::new()),
            totp: new("123456".to_string()),
            separate: new(true),
            mailbox: new(String::new()),
            error: new(None),
            busy: new(false),
            attempt: new(0),
            rev: new(0),
        };

        s.password.set("pw".into());
        s.mailbox.set("mb".into());
        s
    }

    fn login() -> posto_core::Result<Login> {
        Ok(Login {
            session: "{}".into(),
            mailbox_password: "mb".into(),
            email: "me@proton.example".into(),
        })
    }

    fn draft() -> Signal<Config> {
        Signal::new_in_scope(backend().peek().cfg.clone(), ScopeId::ROOT)
    }

    #[test]
    fn a_refused_sign_in_clears_both_passwords() {
        let dom = rendered();

        dom.in_scope(ScopeId::ROOT, || {
            let mut s = typed();

            // No app version: refused before any login can run.
            assert!(s.prepare(None).is_none());
            assert_eq!(
                (s.error)().as_deref(),
                Some("Set [proton] app_version in config.toml first")
            );
            assert_eq!(
                ((s.password)(), (s.mailbox)()),
                (String::new(), String::new())
            );
            assert_eq!(
                ((s.open)(), (s.busy)(), *s.attempt.peek()),
                (true, false, 0)
            );
            assert_eq!((s.totp)(), "123456");

            // No password, but a mailbox password typed.
            s.mailbox.set("mb".into());

            assert!(s.prepare(Some("v".into())).is_none());
            assert_eq!((s.error)().as_deref(), Some("Enter the password"));
            assert_eq!(
                ((s.password)(), (s.mailbox)()),
                (String::new(), String::new())
            );
            assert!((s.open)());
        });
    }

    #[test]
    fn a_stale_login_stores_nothing_and_changes_no_ui() {
        let dom = rendered();

        dom.in_scope(ScopeId::ROOT, || {
            let (mut s, draft) = (typed(), draft());
            let job = s.prepare(Some("v".into())).unwrap();

            assert!(job.attempt > 0 && (s.busy)());

            // The account selection changes while the login runs; the panel
            // is opened again for the next account.
            s.reset();
            s.open.set(true);
            s.password.set("next".into());
            *crate::app::NEEDS_TOKEN.write() = true;

            for result in [login(), Err(Error::Auth)] {
                let stored = Cell::new(false);

                s.finish(draft, "pm", job.attempt, result, |_, _| {
                    stored.set(true);
                    Ok(1)
                });

                assert!(!stored.get(), "a stale login was stored");
            }

            assert_eq!(((s.open)(), (s.busy)(), (s.rev)()), (true, false, 0));
            assert_eq!((s.password)(), "next");
            assert_eq!((s.error)(), None);
            assert_eq!(draft.peek().accounts[0].email, "");
            assert_eq!(backend().peek().cfg.accounts[0].email, "");
            assert_eq!(backend().peek().conn_rev, 0);
            assert_eq!(*STATUS.peek(), "");
            assert!(*crate::app::NEEDS_TOKEN.peek());
        });
    }

    #[test]
    fn a_current_login_is_stored_and_shown() {
        let dom = rendered();

        dom.in_scope(ScopeId::ROOT, || {
            let (s, draft) = (typed(), draft());
            let job = s.prepare(Some("v".into())).unwrap();
            let stored = Cell::new(None);

            *crate::app::NEEDS_TOKEN.write() = true;
            s.finish(draft, "pm", job.attempt, login(), |id, secret| {
                stored.set(Some((id.to_string(), secret.mailbox_password.clone())));
                Ok(1)
            });

            assert_eq!(stored.take(), Some(("pm".to_string(), "mb".to_string())));
            assert_eq!(((s.open)(), (s.busy)(), (s.rev)()), (false, false, 1));
            assert_eq!(
                ((s.password)(), (s.mailbox)()),
                (String::new(), String::new())
            );
            assert_eq!(draft.peek().accounts[0].email, "me@proton.example");
            assert_eq!(backend().peek().conn_rev, 1);
            assert_eq!(*STATUS.peek(), "Signed in to me@proton.example");
            assert!(!*crate::app::NEEDS_TOKEN.peek());
        });
    }

    /// The Google sign-in signals of a mounted `GeneralPage`: busy, attempt 1.
    fn google_busy() -> GoogleSignIn {
        fn new<T: 'static>(value: T) -> Signal<T> {
            Signal::new_in_scope(value, ScopeId::ROOT)
        }

        GoogleSignIn {
            busy: new(true),
            url: new(Some("https://accounts.example/consent".to_string())),
            error: new(None),
            attempt: new(1),
            rev: new(0),
            task: new(None),
        }
    }

    fn google_login() -> posto_core::Result<gmail::Login> {
        Ok(gmail::Login {
            refresh_token: "rt".into(),
            email: "me@gmail.com".into(),
        })
    }

    /// Runs `finish` with a `store` that records its arguments.
    fn google_finish(
        g: GoogleSignIn,
        attempt: u32,
        result: posto_core::Result<gmail::Login>,
    ) -> Vec<(String, String)> {
        let stored = std::cell::RefCell::new(Vec::new());

        g.finish(draft(), "gm", attempt, result, |id, token| {
            stored
                .borrow_mut()
                .push((id.to_string(), token.to_string()));
            Ok(())
        });

        stored.into_inner()
    }

    #[test]
    fn a_google_sign_in_stores_the_token_and_fills_the_sender() {
        let dom = rendered_with(Account::gmail("gm", ""));

        dom.in_scope(ScopeId::ROOT, || {
            let (g, draft) = (google_busy(), draft());
            let stored = std::cell::RefCell::new(Vec::new());

            *crate::app::NEEDS_TOKEN.write() = true;
            g.finish(draft, "gm", 1, google_login(), |id, token| {
                stored
                    .borrow_mut()
                    .push((id.to_string(), token.to_string()));
                Ok(())
            });

            assert_eq!(
                stored.into_inner(),
                vec![("gm".to_string(), "rt".to_string())]
            );
            assert_eq!(((g.busy)(), (g.url)(), (g.error)()), (false, None, None));
            assert_eq!(draft.peek().accounts[0].email, "me@gmail.com");
            assert_eq!(backend().peek().cfg.accounts[0].email, "me@gmail.com");
            assert!(!*crate::app::NEEDS_TOKEN.peek());
            assert_eq!((g.rev)(), 1);
        });
    }

    #[test]
    fn a_failed_google_sign_in_shows_its_error() {
        let dom = rendered_with(Account::gmail("gm", ""));

        dom.in_scope(ScopeId::ROOT, || {
            let g = google_busy();
            let stored = google_finish(g, 1, Err(Error::Other("Google sign-in timed out".into())));

            assert!(stored.is_empty());
            assert!(!(g.busy)());
            assert!((g.error)().is_some_and(|e| e.contains("timed out")));
        });
    }

    #[test]
    fn a_stale_google_sign_in_is_ignored() {
        let dom = rendered_with(Account::gmail("gm", ""));

        dom.in_scope(ScopeId::ROOT, || {
            let mut g = google_busy();

            g.attempt.set(2);

            assert!(google_finish(g, 1, google_login()).is_empty());
            assert!((g.busy)());
            assert_eq!((g.error)(), None);
            assert_eq!((g.rev)(), 0);
        });
    }

    #[test]
    fn cancel_discards_a_running_google_sign_in() {
        struct Flag(std::rc::Rc<Cell<bool>>);

        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let dom = rendered_with(Account::gmail("gm", ""));
        let dropped = std::rc::Rc::new(Cell::new(false));
        let guard = Flag(dropped.clone());
        let mut g = dom.in_scope(ScopeId::ROOT, google_busy);

        dom.in_scope(ScopeId::ROOT, || {
            let task = spawn(async move {
                let _guard = guard;

                std::future::pending::<()>().await;
            });

            g.task.set(Some(task));
            g.cancel();

            assert_eq!(((g.busy)(), (g.url)(), (g.error)()), (false, None, None));
            assert_eq!(*g.attempt.peek(), 2);
            assert!(g.task.peek().is_none());
        });

        assert!(dropped.get(), "the login future was not dropped");

        dom.in_scope(ScopeId::ROOT, || {
            assert!(google_finish(g, 1, google_login()).is_empty());
            assert!(!(g.busy)());
        });
    }

    #[test]
    fn a_google_sign_in_for_a_removed_account_stores_nothing() {
        let dom = rendered_with(Account::gmail("other", ""));

        dom.in_scope(ScopeId::ROOT, || {
            let g = google_busy();

            assert!(google_finish(g, 1, google_login()).is_empty());
            assert!(!(g.busy)());
            assert!((g.error)().is_some_and(|e| e.contains("nothing was stored")));
        });
    }

    #[test]
    fn a_refused_store_shows_its_error() {
        let dom = rendered_with(Account::gmail("gm", ""));

        dom.in_scope(ScopeId::ROOT, || {
            let (g, draft) = (google_busy(), draft());

            *crate::app::NEEDS_TOKEN.write() = true;
            g.finish(draft, "gm", 1, google_login(), |_, _| {
                Err(Error::Other("disk full".into()))
            });

            assert!((g.error)().is_some_and(|e| e.contains("disk full")));
            assert!(*crate::app::NEEDS_TOKEN.peek());
            assert_eq!(draft.peek().accounts[0].email, "");
            assert_eq!(backend().peek().cfg.accounts[0].email, "");
            assert!(!(g.busy)());
            assert_eq!((g.rev)(), 0);
        });
    }
}
