//! Root component, global signals, action dispatch and background work.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use posto_core::config::UiConfig;
use posto_core::jmap::Client;
use posto_core::shortcuts::{Action, Keymap};
use posto_core::store::{Attachment, EmailBody, EmailSummary, ListQuery, Mailbox};
use posto_core::theme::{self, Theme, Watcher};
use posto_core::{Error, Result, actions, body, compose, sync};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::components::{Composer, MessageList, MovePicker, Reader, Settings, Sidebar, Toolbar};
use crate::state::{self, Backend};
use crate::{format, keys};

/// Rows fetched per page of a list or search.
const PAGE: u32 = 200;
/// How often the background loop checks whether a sync is due.
const TICK: Duration = Duration::from_secs(15);
/// How long a status toast stays.
const TOAST: Duration = Duration::from_secs(6);

pub static SELECTED_MAILBOX: GlobalSignal<Option<String>> = Signal::global(|| None);
pub static MAILBOXES: GlobalSignal<Vec<Mailbox>> = Signal::global(Vec::new);
pub static EMAILS: GlobalSignal<Vec<EmailSummary>> = Signal::global(Vec::new);
pub static SELECTED_EMAIL: GlobalSignal<Option<String>> = Signal::global(|| None);
pub static BODY: GlobalSignal<Option<EmailBody>> = Signal::global(|| None);
pub static BODY_ERROR: GlobalSignal<Option<String>> = Signal::global(|| None);
pub static ALLOW_REMOTE: GlobalSignal<bool> = Signal::global(|| false);
pub static READER_MODE: GlobalSignal<bool> = Signal::global(|| false);
pub static SEARCH_QUERY: GlobalSignal<String> = Signal::global(String::new);
pub static LIST_LIMIT: GlobalSignal<u32> = Signal::global(|| PAGE);
/// True when the last query filled its page, so older mail may remain.
pub static HAS_MORE: GlobalSignal<bool> = Signal::global(|| false);
pub static COMPOSER: GlobalSignal<Option<compose::Draft>> = Signal::global(|| None);
pub static COMPOSER_TO: GlobalSignal<String> = Signal::global(String::new);
pub static COMPOSER_CC: GlobalSignal<String> = Signal::global(String::new);
pub static SHOW_SETTINGS: GlobalSignal<bool> = Signal::global(|| false);
pub static SHOW_MOVE_PICKER: GlobalSignal<bool> = Signal::global(|| false);
pub static STATUS: GlobalSignal<String> = Signal::global(String::new);
pub static KEYMAP: GlobalSignal<Keymap> = Signal::global(Keymap::defaults);
pub static THEME_CSS: GlobalSignal<String> = Signal::global(|| {
    theme::detect()
        .map(|t| theme::css_vars(&t))
        .unwrap_or_default()
});
pub static SIDEBAR_OPEN: GlobalSignal<bool> = Signal::global(|| true);
/// True while an `<input>` or `<textarea>` has focus.
pub static TYPING: GlobalSignal<bool> = Signal::global(|| false);
pub static SIDEBAR_WIDTH: GlobalSignal<u32> = Signal::global(|| UiConfig::default().sidebar_width);
pub static LIST_WIDTH: GlobalSignal<u32> = Signal::global(|| UiConfig::default().list_width);
/// The column divider being dragged, if any.
static DRAG: GlobalSignal<Option<Drag>> = Signal::global(|| None);
pub static NEEDS_TOKEN: GlobalSignal<bool> = Signal::global(|| false);
/// Set while a mutating action runs, so a second key press cannot race it.
static BUSY: GlobalSignal<bool> = Signal::global(|| false);
static SYNCING: GlobalSignal<bool> = Signal::global(|| false);
static LAST_SYNC: GlobalSignal<Option<Instant>> = Signal::global(|| None);

thread_local! {
    /// The `Signal<Backend>` provided to the tree, reachable from detached tasks.
    static BACKEND: Cell<Option<Signal<Backend>>> = const { Cell::new(None) };
}

/// The shared backend (see [`Backend`]).
pub fn backend() -> Signal<Backend> {
    BACKEND
        .get()
        .expect("App provides the backend before anything reads it")
}

/// Starts `fut` so that it outlives the component whose handler started it.
pub fn run(fut: impl Future<Output = ()> + 'static) {
    dioxus::core::spawn_forever(fut);
}

/// Which column a divider resizes.
#[derive(Clone, Copy, PartialEq)]
pub enum Divider {
    Sidebar,
    List,
}

/// A divider drag in progress: where it started and the width it started from.
#[derive(Clone, Copy)]
struct Drag {
    divider: Divider,
    start_x: f64,
    start_width: u32,
}

/// Mailbox column width limits, in pixels.
pub fn clamp_sidebar(width: u32) -> u32 {
    width.clamp(140, 480)
}

/// Message-list column width limits, in pixels.
pub fn clamp_list(width: u32) -> u32 {
    width.clamp(240, 800)
}

/// Starts dragging `divider` from screen column `x`.
pub fn begin_drag(divider: Divider, x: f64) {
    let start_width = match divider {
        Divider::Sidebar => *SIDEBAR_WIDTH.peek(),
        Divider::List => *LIST_WIDTH.peek(),
    };

    *DRAG.write() = Some(Drag {
        divider,
        start_x: x,
        start_width,
    });
}

/// Moves the dragged divider to screen column `x`; a no-op when not dragging.
pub fn drag_to(x: f64) {
    let Some(drag) = *DRAG.peek() else {
        return;
    };
    let width = (f64::from(drag.start_width) + x - drag.start_x).max(0.0) as u32;

    match drag.divider {
        Divider::Sidebar => *SIDEBAR_WIDTH.write() = clamp_sidebar(width),
        Divider::List => *LIST_WIDTH.write() = clamp_list(width),
    }
}

/// Ends a drag and saves the widths when they changed.
pub fn end_drag() {
    if DRAG.peek().is_none() {
        return;
    }

    *DRAG.write() = None;

    let ui = UiConfig {
        sidebar_width: *SIDEBAR_WIDTH.peek(),
        list_width: *LIST_WIDTH.peek(),
    };

    if backend().peek().cfg.ui == ui {
        return;
    }

    backend().write().cfg.ui = ui;

    if !backend().peek().ui_test {
        save_config();
    }
}

/// Writes the current config to disk, reporting a failure in the toast.
pub fn save_config() {
    if let Err(e) = backend().peek().cfg.save() {
        set_status(format!("Could not save settings: {e}"));
    }
}

/// Resets a `GlobalSignal<bool>` to false when dropped.
struct Flag(&'static GlobalSignal<bool>);

impl Flag {
    fn take(signal: &'static GlobalSignal<bool>) -> Option<Flag> {
        if *signal.peek() {
            return None;
        }

        *signal.write() = true;

        Some(Flag(signal))
    }
}

impl Drop for Flag {
    fn drop(&mut self) {
        *self.0.write() = false;
    }
}

/// Keeps the theme watcher alive and hands theme changes to the UI.
struct ThemeWatch {
    _watcher: Option<Watcher>,
    rx: RefCell<Option<UnboundedReceiver<Theme>>>,
}

/// Root component.
#[component]
pub fn App() -> Element {
    use_context_provider(|| {
        let backend = consume_context::<Backend>();

        // Saved shortcuts apply from the first key press, not only after Settings.
        *KEYMAP.write() =
            Keymap::from_config(&backend.cfg.shortcuts).unwrap_or_else(|_| Keymap::defaults());

        *SIDEBAR_WIDTH.write() = clamp_sidebar(backend.cfg.ui.sidebar_width);
        *LIST_WIDTH.write() = clamp_list(backend.cfg.ui.list_width);

        // Owned by the root scope: detached tasks (`run`) use it from there and
        // outlive this component's scope.
        let signal = Signal::new_in_scope(backend, ScopeId::ROOT);

        BACKEND.set(Some(signal));
        signal
    });

    // The watcher callback runs on notify's thread; a tokio channel carries
    // the Theme to a task on the UI thread, which awaits it (no polling).
    let watch = use_hook(|| {
        let (tx, rx) = unbounded_channel();
        let watcher = theme::watch(move |t| {
            let _ = tx.send(t);
        })
        .ok()
        .flatten();

        Rc::new(ThemeWatch {
            _watcher: watcher,
            rx: RefCell::new(Some(rx)),
        })
    });

    use_future(move || {
        let watch = watch.clone();

        async move {
            let Some(mut rx) = watch.rx.borrow_mut().take() else {
                return;
            };

            while let Some(t) = rx.recv().await {
                *THEME_CSS.write() = theme::css_vars(&t);
            }
        }
    });

    use_future(|| async {
        reload_mailboxes();
        reload_list().await;
        focus_root();
        refresh_loop().await;
    });

    use_future(|| async {
        let exit_after = std::env::var("POSTO_EXIT_AFTER_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());

        if let Some(ms) = exit_after {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            std::process::exit(0);
        }
    });

    // A collapsed sidebar takes no column and no divider.
    let columns = if SIDEBAR_OPEN() {
        format!("{}px 6px {}px 6px 1fr", SIDEBAR_WIDTH(), LIST_WIDTH())
    } else {
        format!("{}px 6px 1fr", LIST_WIDTH())
    };

    rsx! {
        BaseStyle {}
        div {
            id: "app",
            class: "app",
            tabindex: "0",
            autofocus: true,
            onkeydown: |evt: KeyboardEvent| {
                let m = evt.modifiers();
                let chord = keys::chord_from_event(
                    &evt.key().to_string(),
                    m.contains(Modifiers::CONTROL),
                    m.contains(Modifiers::SHIFT),
                    m.contains(Modifiers::ALT),
                    m.contains(Modifiers::META),
                );

                if keys::should_ignore(*TYPING.peek(), &chord) {
                    return;
                }

                let action = KEYMAP.read().lookup(&chord);

                if let Some(action) = action {
                    evt.prevent_default();
                    dispatch(action);
                }
            },
            style { {THEME_CSS()} }
            if NEEDS_TOKEN() {
                div { class: "banner",
                    "No token configured — open Settings"
                    button { onclick: |_| *SHOW_SETTINGS.write() = true, "Settings" }
                }
            }
            div {
                class: if DRAG().is_some() { "panes dragging" } else { "panes" },
                style: "grid-template-columns: {columns}",
                onmousemove: |evt| drag_to(evt.client_coordinates().x),
                onmouseup: |_| end_drag(),
                onmouseleave: |_| end_drag(),
                if SIDEBAR_OPEN() {
                    Sidebar {}
                    div {
                        class: "resizer",
                        onmousedown: |evt| {
                            evt.prevent_default();
                            begin_drag(Divider::Sidebar, evt.client_coordinates().x);
                        },
                    }
                }
                MessageList {}
                div {
                    class: "resizer",
                    onmousedown: |evt| {
                        evt.prevent_default();
                        begin_drag(Divider::List, evt.client_coordinates().x);
                    },
                }
                div { class: "right",
                    Toolbar {}
                    Reader {}
                }
            }
            if COMPOSER.read().is_some() {
                Composer {}
            }
            if SHOW_SETTINGS() {
                Settings {}
            }
            if SHOW_MOVE_PICKER() {
                MovePicker {}
            }
            if !STATUS.read().is_empty() {
                div { class: "toast", {STATUS()} }
            }
        }
    }
}

/// Runs a keyboard or toolbar action.
pub fn dispatch(action: Action) {
    let modal = COMPOSER.peek().is_some() || *SHOW_SETTINGS.peek() || *SHOW_MOVE_PICKER.peek();

    match action {
        Action::Back | Action::Escape => close_top(),
        Action::Send if COMPOSER.peek().is_some() => send(false),
        _ if modal => {}
        Action::Next => step(1),
        Action::Prev => step(-1),
        Action::Open => focus("reader-body"),
        Action::Archive => run(act_on_selected(actions::Action::Archive)),
        Action::Delete => run(act_on_selected(actions::Action::Delete)),
        Action::Junk => run(act_on_selected(actions::Action::Junk)),
        Action::Reply => run(start_reply(false)),
        Action::ReplyAll => run(start_reply(true)),
        Action::Compose => open_composer(compose::Draft::default()),
        Action::ToggleRead => run(toggle(true)),
        Action::ToggleFlag => run(toggle(false)),
        Action::Move => {
            if SELECTED_EMAIL.peek().is_some() {
                *SHOW_MOVE_PICKER.write() = true;
            }
        }
        Action::Search => focus("search"),
        Action::Sync => run(run_sync(true)),
        Action::Send => {}
    }
}

/// Shows `msg` in the toast for a few seconds.
pub fn set_status(msg: impl Into<String>) {
    let msg = msg.into();

    *STATUS.write() = msg.clone();

    run(async move {
        tokio::time::sleep(TOAST).await;

        let unchanged = *STATUS.peek() == msg;

        if unchanged {
            STATUS.write().clear();
        }
    });
}

/// Reports a failed operation; a rejected token drops the connection.
fn fail(what: &str, e: Error) {
    match e {
        Error::NotConfigured(_) => set_status("No token configured — open Settings"),
        Error::Auth => {
            backend().write().reset_client();
            set_status(format!("{what}: the server rejected the token"));
        }
        e => set_status(format!("{what}: {e}")),
    }
}

/// Moves keyboard focus to the element with `id`.
pub fn focus(id: &str) {
    let id = serde_json::to_string(id).unwrap_or_default();

    let _ = document::eval(&format!("document.getElementById({id})?.focus()"));
}

/// Hands keyboard focus back to the root element.
pub fn focus_root() {
    *TYPING.write() = false;

    let _ =
        document::eval("document.activeElement?.blur?.(); document.getElementById('app')?.focus()");
}

/// Scrolls the list row for `id` into view.
fn scroll_to_row(id: &str) {
    let row = serde_json::to_string(&format!("row-{id}")).unwrap_or_default();

    let _ = document::eval(&format!(
        "document.getElementById({row})?.scrollIntoView({{block: 'nearest'}})"
    ));
}

/// Closes the topmost of composer, settings, move picker, then leaves the search box.
fn close_top() {
    if COMPOSER.peek().is_some() {
        *COMPOSER.write() = None;
    } else if *SHOW_SETTINGS.peek() {
        *SHOW_SETTINGS.write() = false;
    } else if *SHOW_MOVE_PICKER.peek() {
        *SHOW_MOVE_PICKER.write() = false;
    }

    // Removed inputs never send focusout, so reset the flag here.
    focus_root();
}

/// Moves the selection `delta` rows.
fn step(delta: isize) {
    let next = {
        let list = EMAILS.peek();
        let Some(last) = list.len().checked_sub(1) else {
            return;
        };
        let current = SELECTED_EMAIL
            .peek()
            .as_ref()
            .and_then(|id| list.iter().position(|e| &e.id == id));
        let index = match current {
            Some(i) => i.saturating_add_signed(delta).min(last),
            None if delta > 0 => 0,
            None => last,
        };

        list[index].id.clone()
    };

    if SELECTED_EMAIL.peek().as_deref() != Some(next.as_str()) {
        scroll_to_row(&next);
        select_email(Some(next));
    }
}

/// Selects a mailbox, leaving search.
pub fn select_mailbox(id: String) {
    *SELECTED_MAILBOX.write() = Some(id);
    SEARCH_QUERY.write().clear();
    *LIST_LIMIT.write() = PAGE;
    select_email(None);
    run(reload_list());
}

/// Shows the next page of the current list.
pub fn load_more() {
    *LIST_LIMIT.write() += PAGE;
    run(reload_list());
}

/// Search box input, applied after a 250 ms pause.
pub fn search_input(query: String) {
    *SEARCH_QUERY.write() = query.clone();
    *LIST_LIMIT.write() = PAGE;

    run(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;

        if *SEARCH_QUERY.peek() == query {
            reload_list().await;
        }
    });
}

/// Selects `id` (or nothing), loads its body and marks it read after a second.
pub fn select_email(id: Option<String>) {
    *ALLOW_REMOTE.write() = id.as_deref().is_some_and(remote_trusted);
    *BODY.write() = None;
    *BODY_ERROR.write() = None;
    *SELECTED_EMAIL.write() = id.clone();

    if let Some(id) = id {
        run(load_body(id.clone()));
        run(mark_read_later(id));
    }
}

/// True when the policy lets the sender of email `id` load remote content.
fn remote_trusted(id: &str) -> bool {
    let b = backend();
    let b = b.peek();
    let from = b
        .store
        .email(id)
        .ok()
        .flatten()
        .and_then(|e| e.from.first().map(|a| a.email.clone()))
        .unwrap_or_default();

    state::remote_allowed(&b.cfg, &from)
}

/// Adds `domain` to the trusted senders, saves, and loads remote content now.
pub fn trust_domain(domain: &str) {
    let domain = domain.to_lowercase();
    let mut shared = backend();

    {
        let mut b = shared.write();

        if !b.cfg.trusted_senders.contains(&domain) {
            b.cfg.trusted_senders.push(domain);
        }
    }

    if !shared.peek().ui_test {
        save_config();
    }

    *ALLOW_REMOTE.write() = true;
}

/// The body of `id`, connecting to the server when it is not cached.
async fn body_for(id: &str) -> Result<EmailBody> {
    let b = backend().peek().clone();

    match body::fetch_body(&b.store, b.client.as_deref(), id).await {
        Err(Error::NotFound(_)) if !b.ui_test => {
            let client = get_client().await?;

            body::fetch_body(&b.store, client.as_deref(), id).await
        }
        other => other,
    }
}

async fn load_body(id: String) {
    let result = body_for(&id).await;

    if SELECTED_EMAIL.peek().as_deref() != Some(id.as_str()) {
        return;
    }

    match result {
        Ok(body) => *BODY.write() = Some(body),
        Err(Error::NotFound(_)) => {
            *BODY_ERROR.write() = Some("This message body is not available offline.".into());
        }
        Err(e) => *BODY_ERROR.write() = Some(format!("Could not load message: {e}")),
    }
}

async fn mark_read_later(id: String) {
    tokio::time::sleep(Duration::from_secs(1)).await;

    if SELECTED_EMAIL.peek().as_deref() != Some(id.as_str()) {
        return;
    }

    let unread = backend()
        .peek()
        .store
        .email(&id)
        .ok()
        .flatten()
        .is_some_and(|e| e.is_unread());

    if unread
        && apply_local(std::slice::from_ref(&id), actions::Action::MarkRead)
            .await
            .is_ok()
    {
        refresh_row(&id);
    }
}

/// The server connection; sets the "no token" banner as a side effect.
pub async fn get_client() -> Result<Option<Arc<Client>>> {
    let result = state::client(backend()).await;
    let missing = matches!(result, Err(Error::NotConfigured(_)));

    if *NEEDS_TOKEN.peek() != missing {
        *NEEDS_TOKEN.write() = missing;
    }

    result
}

/// Applies `action` to the server and store, and mirrors it in the mailbox badges.
async fn apply_local(ids: &[String], action: actions::Action) -> Result<usize> {
    let b = backend().peek().clone();
    let client = get_client().await?;
    let before: Vec<EmailSummary> = ids
        .iter()
        .filter_map(|id| b.store.email(id).ok().flatten())
        .collect();
    let applied = actions::apply(&b.store, client.as_deref(), ids, action).await?;

    for old in &before {
        if let Ok(Some(new)) = b.store.email(&old.id) {
            bump_counts(old, &new);
        }
    }

    Ok(applied)
}

/// Adjusts the mailbox badges for an email that changed from `old` to `new`.
fn bump_counts(old: &EmailSummary, new: &EmailSummary) {
    for m in MAILBOXES.write().iter_mut() {
        let was = old.mailbox_ids.contains(&m.id);
        let is = new.mailbox_ids.contains(&m.id);

        m.total += i64::from(is) - i64::from(was);
        m.unread += i64::from(is && new.is_unread()) - i64::from(was && old.is_unread());
    }
}

/// Replaces the visible row for `id` with its current state.
fn refresh_row(id: &str) {
    let Ok(Some(fresh)) = backend().peek().store.email(id) else {
        return;
    };

    if let Some(row) = EMAILS.write().iter_mut().find(|e| e.id == id) {
        *row = fresh;
    }
}

/// Re-reads the mailbox list (and server counts); selects the inbox the first time.
pub fn reload_mailboxes() {
    match backend().peek().store.mailboxes() {
        Ok(list) => {
            if SELECTED_MAILBOX.peek().is_none() {
                let first = list
                    .iter()
                    .find(|m| m.role.as_deref() == Some("inbox"))
                    .or(list.first());

                if let Some(m) = first {
                    *SELECTED_MAILBOX.write() = Some(m.id.clone());
                }
            }

            *MAILBOXES.write() = list;
        }
        Err(e) => set_status(format!("Could not read mailboxes: {e}")),
    }
}

/// Reloads the active query (search, else the selected mailbox) from the store.
pub async fn reload_list() {
    let b = backend().peek().clone();
    let query = SEARCH_QUERY.peek().trim().to_string();
    let mailbox = SELECTED_MAILBOX.peek().clone();
    let limit = *LIST_LIMIT.peek();
    let store = b.store.clone();
    let (q, mb) = (query.clone(), mailbox.clone());

    let loaded = tokio::task::spawn_blocking(move || {
        if q.is_empty() {
            store.list_emails(&ListQuery {
                mailbox_id: mb,
                limit,
                ..Default::default()
            })
        } else {
            store.search(&q, limit)
        }
    })
    .await
    .map_err(|e| Error::Other(e.to_string()))
    .and_then(|r| r);

    // A newer query or mailbox choice made this result stale.
    if SEARCH_QUERY.peek().trim() != query || *SELECTED_MAILBOX.peek() != mailbox {
        return;
    }

    match loaded {
        Ok(list) => {
            let backend = backend().peek().clone();

            // Before blocked senders are removed: a full page may still have more behind it.
            *HAS_MORE.write() = list.len() >= limit as usize;
            *EMAILS.write() = list
                .into_iter()
                .filter(|e| !backend.is_blocked(e))
                .collect();
        }
        Err(e) => set_status(format!("Could not load messages: {e}")),
    }
}

/// Applies `action` to the selected email.
async fn act_on_selected(action: actions::Action) {
    let Some(_busy) = Flag::take(&BUSY) else {
        return;
    };
    let Some(id) = SELECTED_EMAIL.peek().clone() else {
        return;
    };
    let done = match &action {
        actions::Action::Archive => "Archived",
        actions::Action::Delete => "Moved to Trash",
        actions::Action::Junk => "Marked as junk",
        _ => "Moved",
    };

    match act_on_email(&id, action).await {
        Ok(()) => set_status(done),
        Err(e) => fail("Action failed", e),
    }
}

/// Applies `action` to `id`, reloads the list, and moves the selection on if `id`
/// was selected and left the list.
async fn act_on_email(id: &str, action: actions::Action) -> Result<()> {
    let old_index = EMAILS.peek().iter().position(|e| e.id == id);

    apply_local(&[id.to_string()], action).await?;
    reload_list().await;

    let selected = SELECTED_EMAIL.peek().as_deref() == Some(id);
    let still_listed = EMAILS.peek().iter().any(|e| e.id == id);

    if selected && !still_listed {
        let next = {
            let list = EMAILS.peek();

            old_index
                .and_then(|i| list.get(i).or(list.last()))
                .map(|e| e.id.clone())
        };

        if let Some(next) = &next {
            scroll_to_row(next);
        }

        select_email(next);
    }

    Ok(())
}

/// Moves the selected email to `mailbox_id` and closes the picker.
pub fn move_selected(mailbox_id: String) {
    *SHOW_MOVE_PICKER.write() = false;
    focus_root();
    run(act_on_selected(actions::Action::Move { mailbox_id }));
}

/// Toggles the selected email's read (`read`) or flagged state.
async fn toggle(read: bool) {
    let Some(_busy) = Flag::take(&BUSY) else {
        return;
    };
    let Some(id) = SELECTED_EMAIL.peek().clone() else {
        return;
    };
    let Ok(Some(email)) = backend().peek().store.email(&id) else {
        return;
    };
    let action = match (read, email.is_unread(), email.is_flagged()) {
        (true, true, _) => actions::Action::MarkRead,
        (true, false, _) => actions::Action::MarkUnread,
        (false, _, true) => actions::Action::Unflag,
        (false, _, false) => actions::Action::Flag,
    };

    match apply_local(std::slice::from_ref(&id), action).await {
        Ok(_) => refresh_row(&id),
        Err(e) => fail("Action failed", e),
    }
}

/// Opens the composer on a reply to the selected email.
async fn start_reply(all: bool) {
    let Some(id) = SELECTED_EMAIL.peek().clone() else {
        return;
    };
    let b = backend().peek().clone();
    let Ok(Some(summary)) = b.store.email(&id) else {
        return;
    };
    let cached = BODY.peek().clone().filter(|body| body.email_id == id);
    let body = match cached {
        Some(body) => body,
        None => match body_for(&id).await {
            Ok(body) => body,
            Err(e) => return fail("Could not load the message to reply to", e),
        },
    };

    open_composer(compose::reply(&summary, &body, &b.cfg.account.email, all));
}

/// Opens the composer on `draft`.
pub fn open_composer(draft: compose::Draft) {
    *COMPOSER_TO.write() = format::addresses_text(&draft.to);
    *COMPOSER_CC.write() = format::addresses_text(&draft.cc);
    *COMPOSER.write() = Some(draft);
}

/// Discards the open draft.
pub fn close_composer() {
    *COMPOSER.write() = None;
    focus_root();
}

/// Sends the open draft; `archive` also archives the email it replies to.
pub fn send(archive: bool) {
    run(send_draft(archive));
}

async fn send_draft(archive: bool) {
    let Some(_busy) = Flag::take(&BUSY) else {
        return;
    };
    let Some(mut draft) = COMPOSER.peek().clone() else {
        return;
    };

    draft.to = compose::parse_addresses(&COMPOSER_TO.peek());
    draft.cc = compose::parse_addresses(&COMPOSER_CC.peek());

    if draft.to.is_empty() {
        return set_status("Add at least one recipient.");
    }

    let b = backend().peek().clone();

    if !b.ui_test {
        let sent = match get_client().await {
            Ok(Some(client)) => compose::send(&b.cfg, &b.store, &client, &draft).await,
            Ok(None) => Err(Error::Other("no connection".into())),
            Err(e) => Err(e),
        };

        if let Err(e) = sent {
            return fail("Send failed", e);
        }
    }

    *COMPOSER.write() = None;
    focus_root();
    set_status(if b.ui_test {
        "Sent (UI test mode)"
    } else {
        "Sent"
    });

    match draft.reply_to_email_id.clone().filter(|_| archive) {
        Some(id) => {
            if let Err(e) = act_on_email(&id, actions::Action::Archive).await {
                fail("Sent, but archiving failed", e);
            }
        }
        None => {
            if let Some(id) = &draft.reply_to_email_id {
                refresh_row(id);
            }
        }
    }
}

/// Downloads an attachment into the downloads folder.
pub fn save_attachment(att: Attachment) {
    run(async move {
        let b = backend().peek().clone();

        if b.ui_test {
            return set_status("Preview attachments are not available.");
        }

        let client = match get_client().await {
            Ok(Some(client)) => client,
            Ok(None) => return,
            Err(e) => return fail("Download failed", e),
        };
        let dir = dirs::download_dir().unwrap_or_else(posto_core::config::data_dir);

        match body::save_attachment(&client, &att, &dir).await {
            Ok(path) => set_status(format!("Saved to {}", path.display())),
            Err(e) => fail("Download failed", e),
        }
    });
}

/// Syncs with the server. Manual runs report progress; background runs only errors.
pub async fn run_sync(manual: bool) {
    if backend().peek().ui_test {
        if manual {
            set_status("Nothing to sync in UI test mode.");
        }

        return;
    }

    let Some(_syncing) = Flag::take(&SYNCING) else {
        return;
    };

    // Set up front so a failing server is retried once per interval, not per tick.
    *LAST_SYNC.write() = Some(Instant::now());

    if manual {
        set_status("Syncing…");
    }

    let b = backend().peek().clone();
    let result = match get_client().await {
        Ok(Some(client)) => {
            let (cfg, store) = (b.cfg.clone(), b.store.clone());

            tokio::spawn(async move { sync::sync(&cfg, &store, &client).await })
                .await
                .map_err(|e| Error::Other(e.to_string()))
                .and_then(|r| r)
        }
        Ok(None) => return,
        Err(e) => Err(e),
    };

    match result {
        Ok(report) => {
            reload_mailboxes();
            reload_list().await;

            if manual || report.emails_added > 0 {
                set_status(format!(
                    "Synced: {} new, {} updated, {} removed",
                    report.emails_added, report.emails_updated, report.emails_removed
                ));
            }
        }
        Err(Error::Locked(_)) => {
            if manual {
                set_status("Sync already running");
            }
        }
        Err(Error::NotConfigured(_)) if !manual => {}
        Err(e) => fail("Sync failed", e),
    }
}

/// Syncs whenever `refresh_minutes` (re-read each tick) have passed since the last sync.
async fn refresh_loop() {
    loop {
        let (minutes, ui_test) = {
            let b = backend();
            let b = b.peek();

            (b.cfg.sync.refresh_minutes, b.ui_test)
        };
        let interval = Duration::from_secs(u64::from(minutes) * 60);
        let due = LAST_SYNC
            .peek()
            .is_none_or(|last| last.elapsed() >= interval);

        if !ui_test && minutes > 0 && due {
            run_sync(false).await;
        }

        tokio::time::sleep(TICK).await;
    }
}

/// Makes the refresh loop sync on its next tick (after settings or token changes).
pub fn sync_soon() {
    *LAST_SYNC.write() = None;
}

#[cfg(test)]
mod tests {
    use dioxus::core::RuntimeGuard;
    use posto_core::config::Config;
    use posto_core::store::{Address, Store};

    use super::*;

    /// A backend over an empty in-memory store with `cfg`.
    fn backend_with(cfg: Config) -> Backend {
        Backend {
            store: Arc::new(Store::open_memory().unwrap()),
            client: None,
            cfg,
            ui_test: false,
            conn_rev: 0,
        }
    }

    /// Renders the real `App` once, headless, over `backend`.
    fn rendered(backend: Backend) -> VirtualDom {
        let mut dom = VirtualDom::new(App).with_root_context(backend);

        dom.rebuild_in_place();
        dom
    }

    #[test]
    fn clamp_widths() {
        assert_eq!(clamp_sidebar(139), 140);
        assert_eq!(clamp_sidebar(481), 480);
        assert_eq!(clamp_sidebar(300), 300);
        assert_eq!(clamp_list(239), 240);
        assert_eq!(clamp_list(801), 800);
        assert_eq!(clamp_list(300), 300);
    }

    #[test]
    fn startup_keymap_uses_saved_shortcuts() {
        let mut cfg = Config::default();

        cfg.shortcuts.insert("archive".into(), "y".into());

        let dom = rendered(backend_with(cfg));
        let (archive, old) = dom.in_scope(ScopeId::ROOT, || {
            let keymap = KEYMAP.peek();

            (
                keymap.lookup(&keys::chord_from_event("y", false, false, false, false)),
                keymap.lookup(&keys::chord_from_event("e", false, false, false, false)),
            )
        });

        assert_eq!(archive, Some(Action::Archive));
        assert_eq!(old, None);
    }

    #[test]
    fn backend_signal_is_owned_by_root_scope() {
        let dom = rendered(backend_with(Config::default()));
        let owner = dom.in_scope(ScopeId::ROOT, || backend().origin_scope());

        assert_eq!(owner, ScopeId::ROOT);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_more_survives_blocked_sender_in_full_page() {
        let mut cfg = Config::default();

        cfg.sync.refresh_minutes = 0;
        cfg.blocked_senders = vec!["blocked@review.invalid".into()];

        let dom = rendered(backend_with(cfg));
        let _guard = RuntimeGuard::new(dom.runtime());
        let store = backend().peek().store.clone();

        store
            .upsert_mailboxes(&[Mailbox {
                id: "box".into(),
                name: "Box".into(),
                role: Some("inbox".into()),
                parent_id: None,
                sort_order: 0,
                total: 0,
                unread: 0,
            }])
            .unwrap();

        // PAGE + 1 emails, the newest of them blocked.
        let rows: Vec<EmailSummary> = (0..=PAGE)
            .map(|i| EmailSummary {
                id: format!("e{i:03}"),
                thread_id: format!("t{i:03}"),
                received_at: format!("2026-10-12T{:02}:{:02}:00Z", i / 60, i % 60),
                from: vec![Address {
                    name: None,
                    email: if i == PAGE {
                        "blocked@review.invalid".into()
                    } else {
                        "allowed@review.invalid".into()
                    },
                }],
                to: vec![],
                cc: vec![],
                subject: String::new(),
                preview: String::new(),
                size: 0,
                has_attachment: false,
                mailbox_ids: vec!["box".into()],
                keywords: vec![],
            })
            .collect();

        store.upsert_emails(&rows).unwrap();
        reload_mailboxes();
        reload_list().await;

        assert_eq!(EMAILS.peek().len(), PAGE as usize - 1);
        assert!(*HAS_MORE.peek(), "older allowed mail remains");
    }
}

/// The static stylesheet. Lives in its own component that reads no signals, so it renders once;
/// re-rendering `document::Style` from `App` on every state change logs a Dioxus warning.
#[component]
fn BaseStyle() -> Element {
    rsx! {
        document::Style { {include_str!("../assets/style.css")} }
    }
}
