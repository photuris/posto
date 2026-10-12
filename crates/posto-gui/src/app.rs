//! Root component, global signals, action dispatch and background work.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use posto_core::config::{Account, Config, UiConfig};
use posto_core::jmap::{Client, Identity};
use posto_core::shortcuts::{Action, Keymap};
use posto_core::store::{Attachment, EmailBody, EmailSummary, ListQuery, Mailbox, MsgRef};
use posto_core::theme::{self, Theme, Watcher};
use posto_core::{Error, Result, actions, body, compose, sync};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::components::{
    Composer, ConfirmDialog, MessageList, MovePicker, Reader, Settings, Sidebar, Toolbar,
};
use crate::selection::{Click, Selection};
use crate::state::{self, Backend, Folder, group_by_account, row_key};
use crate::{format, keys};

/// A pending yes/no question; accepting runs `on_accept`.
#[derive(Clone, Debug, PartialEq)]
pub struct Confirm {
    pub title: String,
    pub body: String,
    pub accept_label: String,
    pub on_accept: Action,
}

/// True when `action` on `selected` messages must be confirmed first.
pub fn needs_confirm(action: Action, selected: usize) -> bool {
    action == Action::Delete && selected > 1
}

/// Rows fetched per page of a list or search.
const PAGE: u32 = 200;
/// How often the background loop checks whether a sync is due.
const TICK: Duration = Duration::from_secs(15);
/// How long a status toast stays.
const TOAST: Duration = Duration::from_secs(6);

pub static SELECTED_MAILBOX: GlobalSignal<Option<Folder>> = Signal::global(|| None);
/// The mailboxes of every enabled account.
pub static MAILBOXES: GlobalSignal<Vec<Mailbox>> = Signal::global(Vec::new);
/// Each account's sending identities, as last synced.
pub static IDENTITIES: GlobalSignal<BTreeMap<String, Vec<Identity>>> =
    Signal::global(BTreeMap::new);
pub static EMAILS: GlobalSignal<Vec<EmailSummary>> = Signal::global(Vec::new);
/// The selected messages, as row keys (`<account>/<id>`); the focused one is
/// shown in the reader.
pub static SELECTION: GlobalSignal<Selection> = Signal::global(Selection::default);
/// The body of the focused message, with the message it belongs to.
pub static BODY: GlobalSignal<Option<(MsgRef, EmailBody)>> = Signal::global(|| None);
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
pub static CONFIRM: GlobalSignal<Option<Confirm>> = Signal::global(|| None);
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

/// The focused message: the one the reader shows and replies go to.
pub fn selected_ref() -> Option<MsgRef> {
    SELECTION.read().focus.as_deref().and_then(MsgRef::parse)
}

/// [`selected_ref`] without subscribing the caller to changes.
fn focus_ref() -> Option<MsgRef> {
    SELECTION.peek().focus.as_deref().and_then(MsgRef::parse)
}

/// Every selected message, in list order.
pub fn selected_refs() -> Vec<MsgRef> {
    SELECTION
        .peek()
        .ids
        .iter()
        .filter_map(|k| MsgRef::parse(k))
        .collect()
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
        ..backend().peek().cfg.ui.clone()
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

    // Menu items drive the same actions as the keys; muda shows the About panel itself.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    dioxus::desktop::use_muda_event_handler(move |event| {
        match crate::menu::actions().get(event.id()) {
            Some(crate::menu::MenuAction::Shortcut(action)) => dispatch(*action),
            Some(crate::menu::MenuAction::Settings) => {
                let open = *SHOW_SETTINGS.peek();
                *SHOW_SETTINGS.write() = !open;
            }
            Some(crate::menu::MenuAction::About) | None => {}
        }
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
        reload_identities();
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

                let action = keys::lookup_with_meta_fallback(&KEYMAP.read(), &chord);

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
            if CONFIRM.read().is_some() {
                ConfirmDialog {}
            }
            if !STATUS.read().is_empty() {
                div { class: "toast", {STATUS()} }
            }
        }
    }
}

/// Runs a keyboard or toolbar action.
pub fn dispatch(action: Action) {
    let modal = COMPOSER.peek().is_some()
        || *SHOW_SETTINGS.peek()
        || *SHOW_MOVE_PICKER.peek()
        || CONFIRM.peek().is_some();
    let selected = SELECTION.peek().len();

    if !modal && needs_confirm(action, selected) {
        *CONFIRM.write() = Some(Confirm {
            title: format!("Delete {selected} messages?"),
            body: "They will be moved to Trash.".into(),
            accept_label: "Delete".into(),
            on_accept: Action::DeleteConfirmed,
        });

        return;
    }

    match action {
        Action::Escape if !modal && !*TYPING.peek() && SELECTION.peek().len() > 1 => {
            change_selection(Selection::collapse);
        }
        Action::Back | Action::Escape => close_top(),
        Action::Send if COMPOSER.peek().is_some() => send(false),
        _ if modal => {}
        Action::Next => move_focus(1, false),
        Action::Prev => move_focus(-1, false),
        Action::ExtendNext => move_focus(1, true),
        Action::ExtendPrev => move_focus(-1, true),
        Action::SelectAll => {
            let order = list_order();

            change_selection(|s| s.all(&order));
        }
        Action::Open => focus("reader-body"),
        Action::Archive => run(act_on_selected(actions::Action::Archive)),
        Action::Delete | Action::DeleteConfirmed => {
            run(act_on_selected(actions::Action::Delete));
        }
        Action::Junk => run(act_on_selected(actions::Action::Junk)),
        Action::Reply => run(start_reply(false)),
        Action::ReplyAll => run(start_reply(true)),
        Action::Compose => open_composer(compose::Draft::default()),
        Action::ToggleRead => toggle(true),
        Action::ToggleFlag => toggle(false),
        Action::Move => {
            let refs = selected_refs();

            if refs.is_empty() {
                // Nothing to move.
            } else if group_by_account(&refs).len() > 1 {
                set_status("Select messages from one account to move them.");
            } else {
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

/// Scrolls the list row with this key into view.
fn scroll_to_row(key: &str) {
    let row = serde_json::to_string(&format!("row-{key}")).unwrap_or_default();

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
    } else if CONFIRM.peek().is_some() {
        *CONFIRM.write() = None;
    }

    // Removed inputs never send focusout, so reset the flag here.
    focus_root();
}

/// Moves the focus `delta` rows; `extend` grows the selection instead of replacing it.
fn move_focus(delta: i32, extend: bool) {
    let order = list_order();

    change_selection(|s| {
        if extend {
            s.extend(&order, delta);
        } else {
            s.step(&order, delta);
        }
    });

    if let Some(id) = SELECTION.peek().focus.clone() {
        scroll_to_row(&id);
    }
}

/// Selects a folder, leaving search.
pub fn select_mailbox(folder: Folder) {
    *SELECTED_MAILBOX.write() = Some(folder);
    SEARCH_QUERY.write().clear();
    *LIST_LIMIT.write() = PAGE;
    change_selection(|s| *s = Selection::default());
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

/// The keys of the rows on screen, in order.
fn list_order() -> Vec<String> {
    EMAILS.peek().iter().map(row_key).collect()
}

/// A click on the row with this key.
pub fn click_email(key: &str, kind: Click) {
    let order = list_order();

    change_selection(|s| s.click(&order, key, kind));
}

/// Applies `edit` to the selection. When the focused message changes, loads its
/// body and marks it read after a second.
fn change_selection(edit: impl FnOnce(&mut Selection)) {
    let before = SELECTION.peek().focus.clone();

    edit(&mut SELECTION.write());

    let after = SELECTION.peek().focus.clone();

    if before == after {
        return;
    }

    let focus = after.as_deref().and_then(MsgRef::parse);

    *ALLOW_REMOTE.write() = focus.as_ref().is_some_and(remote_trusted);
    *BODY.write() = None;
    *BODY_ERROR.write() = None;

    if let Some(r) = focus {
        run(load_body(r.clone()));
        run(mark_read_later(r));
    }
}

/// True when the policy lets the sender of the message load remote content.
fn remote_trusted(r: &MsgRef) -> bool {
    let b = backend();
    let b = b.peek();
    let from = b
        .store
        .email(&r.account_id, &r.id)
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

/// The body of a message, connecting to its account's server when it is not cached.
async fn body_for(r: &MsgRef) -> Result<EmailBody> {
    let b = backend().peek().clone();
    let cached = b.clients.get(&r.account_id).map(Arc::as_ref);

    match body::fetch_body(&b.store, cached, &r.account_id, &r.id).await {
        Err(Error::NotFound(_)) if !b.ui_test => {
            let client = get_client(&r.account_id).await?;

            body::fetch_body(&b.store, client.as_deref(), &r.account_id, &r.id).await
        }
        other => other,
    }
}

async fn load_body(r: MsgRef) {
    let result = body_for(&r).await;

    if focus_ref().as_ref() != Some(&r) {
        return;
    }

    match result {
        Ok(body) => *BODY.write() = Some((r, body)),
        Err(Error::NotFound(_)) => {
            *BODY_ERROR.write() = Some("This message body is not available offline.".into());
        }
        Err(e) => *BODY_ERROR.write() = Some(format!("Could not load message: {e}")),
    }
}

async fn mark_read_later(r: MsgRef) {
    tokio::time::sleep(Duration::from_secs(1)).await;

    if focus_ref().as_ref() != Some(&r) {
        return;
    }

    let unread = backend()
        .peek()
        .store
        .email(&r.account_id, &r.id)
        .ok()
        .flatten()
        .is_some_and(|e| e.is_unread());

    if unread
        && apply_local(std::slice::from_ref(&r), actions::Action::MarkRead)
            .await
            .is_ok()
    {
        refresh_row(&r);
    }
}

/// The server connection of `account_id`; sets the "no token" banner as a side
/// effect when it has no token.
pub async fn get_client(account_id: &str) -> Result<Option<Arc<Client>>> {
    let result = state::client(backend(), account_id).await;

    if matches!(result, Err(Error::NotConfigured(_))) && !*NEEDS_TOKEN.peek() {
        *NEEDS_TOKEN.write() = true;
    }

    result
}

/// Applies `action` to the server and store, account by account, and mirrors
/// it in the mailbox badges.
async fn apply_local(refs: &[MsgRef], action: actions::Action) -> Result<usize> {
    let b = backend().peek().clone();
    let mut applied = 0;

    for (account, ids) in group_by_account(refs) {
        let client = get_client(&account).await?;
        let before: Vec<EmailSummary> = ids
            .iter()
            .filter_map(|id| b.store.email(&account, id).ok().flatten())
            .collect();

        applied +=
            actions::apply(&b.store, client.as_deref(), &account, &ids, action.clone()).await?;

        for old in &before {
            if let Ok(Some(new)) = b.store.email(&account, &old.id) {
                bump_counts(old, &new);
            }
        }
    }

    Ok(applied)
}

/// Adjusts the mailbox badges for an email that changed from `old` to `new`.
fn bump_counts(old: &EmailSummary, new: &EmailSummary) {
    for m in MAILBOXES
        .write()
        .iter_mut()
        .filter(|m| m.account_id == old.account_id)
    {
        let was = old.mailbox_ids.contains(&m.id);
        let is = new.mailbox_ids.contains(&m.id);

        m.total += i64::from(is) - i64::from(was);
        m.unread += i64::from(is && new.is_unread()) - i64::from(was && old.is_unread());
    }
}

/// Replaces the visible row of a message with its current state.
fn refresh_row(r: &MsgRef) {
    let Ok(Some(fresh)) = backend().peek().store.email(&r.account_id, &r.id) else {
        return;
    };

    if let Some(row) = EMAILS.write().iter_mut().find(|e| e.msg_ref() == *r) {
        *row = fresh;
    }
}

/// Re-reads every account's cached sending identities.
pub fn reload_identities() {
    let b = backend().peek().clone();
    let mut all = BTreeMap::new();

    for a in &b.cfg.accounts {
        match b.store.identities(&a.id) {
            Ok(list) => {
                all.insert(a.id.clone(), list);
            }
            Err(e) => return fail("Could not read identities", e),
        }
    }

    *IDENTITIES.write() = all;
}

/// The folder to start on: the merged inbox with several accounts, else the
/// first inbox (or first mailbox).
fn default_folder(mailboxes: &[Mailbox], accounts: usize) -> Option<Folder> {
    if accounts > 1 {
        return Some(Folder::AllInboxes);
    }

    mailboxes
        .iter()
        .find(|m| m.role.as_deref() == Some("inbox"))
        .or(mailboxes.first())
        .map(|m| Folder::Mailbox {
            account_id: m.account_id.clone(),
            id: m.id.clone(),
        })
}

/// True when `folder` still exists among `mailboxes` (or is offered at all).
fn folder_exists(folder: &Folder, mailboxes: &[Mailbox], accounts: usize) -> bool {
    match folder {
        Folder::AllInboxes => accounts > 1,
        Folder::Mailbox { account_id, id } => mailboxes
            .iter()
            .any(|m| m.account_id == *account_id && m.id == *id),
    }
}

/// Re-reads the mailbox list (and server counts) of every enabled account;
/// selects a default folder when none is selected or the selection is gone.
pub fn reload_mailboxes() {
    let b = backend().peek().clone();
    let accounts = b.cfg.accounts_enabled().count();
    let mut all = Vec::new();

    for a in b.cfg.accounts_enabled() {
        match b.store.mailboxes(&a.id) {
            Ok(list) => all.extend(list),
            Err(e) => return set_status(format!("Could not read mailboxes: {e}")),
        }
    }

    let current = SELECTED_MAILBOX.peek().clone();

    if !current
        .as_ref()
        .is_some_and(|f| folder_exists(f, &all, accounts))
    {
        let next = default_folder(&all, accounts);

        if next != current {
            *SELECTED_MAILBOX.write() = next;
        }
    }

    *MAILBOXES.write() = all;
}

/// Reloads the active query (search, else the selected folder) from the store.
pub async fn reload_list() {
    let b = backend().peek().clone();
    let query = SEARCH_QUERY.peek().trim().to_string();
    let folder = SELECTED_MAILBOX.peek().clone();
    let limit = *LIST_LIMIT.peek();
    let store = b.store.clone();
    let (q, f) = (query.clone(), folder.clone());

    let loaded = tokio::task::spawn_blocking(move || {
        if !q.is_empty() {
            return store.search(None, &q, limit);
        }

        match f {
            Some(Folder::AllInboxes) => store.list_emails(
                None,
                &ListQuery {
                    role: Some("inbox".into()),
                    limit,
                    ..Default::default()
                },
            ),
            Some(Folder::Mailbox { account_id, id }) => store.list_emails(
                Some(&account_id),
                &ListQuery {
                    mailbox_id: Some(id),
                    limit,
                    ..Default::default()
                },
            ),
            None => store.list_emails(
                None,
                &ListQuery {
                    limit,
                    ..Default::default()
                },
            ),
        }
    })
    .await
    .map_err(|e| Error::Other(e.to_string()))
    .and_then(|r| r);

    // A newer query or mailbox choice made this result stale.
    if SEARCH_QUERY.peek().trim() != query || *SELECTED_MAILBOX.peek() != folder {
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
                .filter(|e| {
                    backend
                        .cfg
                        .account(&e.account_id)
                        .is_some_and(|a| a.enabled)
                })
                .collect();
        }
        Err(e) => set_status(format!("Could not load messages: {e}")),
    }
}

/// Applies `action` to every selected email.
async fn act_on_selected(action: actions::Action) {
    let Some(_busy) = Flag::take(&BUSY) else {
        return;
    };
    let refs = selected_refs();

    if refs.is_empty() {
        return;
    }

    let n = refs.len();
    let s = if n == 1 { "" } else { "s" };
    let done = match &action {
        actions::Action::Archive => format!("Archived {n} message{s}"),
        actions::Action::Delete => format!("Moved {n} message{s} to Trash"),
        actions::Action::Junk => format!("Marked {n} message{s} as junk"),
        _ => format!("Moved {n} message{s}"),
    };

    match act_on_emails(&refs, action).await {
        Ok(()) => set_status(done),
        Err(e) => fail("Action failed", e),
    }
}

/// Applies `action` to `refs`, reloads the list, and drops the ones that left it from the
/// selection. If that empties the selection, the row at the old focus's position takes over.
async fn act_on_emails(refs: &[MsgRef], action: actions::Action) -> Result<()> {
    let old_index = SELECTION
        .peek()
        .focus
        .as_ref()
        .and_then(|f| EMAILS.peek().iter().position(|e| row_key(e) == *f));
    let had_selection = !SELECTION.peek().is_empty();

    apply_local(refs, action).await?;
    reload_list().await;

    let order = list_order();

    change_selection(|s| s.retain(&order));

    if had_selection && SELECTION.peek().is_empty() {
        let next = old_index
            .and_then(|i| order.get(i).or(order.last()))
            .cloned();

        if let Some(next) = next {
            scroll_to_row(&next);
            click_email(&next, Click::Plain);
        }
    }

    Ok(())
}

/// Moves the selected emails to `mailbox_id` and closes the picker.
pub fn move_selected(mailbox_id: String) {
    *SHOW_MOVE_PICKER.write() = false;
    focus_root();
    run(act_on_selected(actions::Action::Move { mailbox_id }));
}

/// Marks every selected email read or unread, flagged or unflagged (`action`).
pub fn mark_selected(action: actions::Action) {
    run(async move {
        let Some(_busy) = Flag::take(&BUSY) else {
            return;
        };
        let refs = selected_refs();

        match apply_local(&refs, action).await {
            Ok(_) => refs.iter().for_each(refresh_row),
            Err(e) => fail("Action failed", e),
        }
    });
}

/// Toggles the selected emails' read (`read`) or flagged state: if any is unread
/// (unflagged) they all become read (flagged), else all become unread (unflagged).
fn toggle(read: bool) {
    let store = backend().peek().store.clone();
    let emails: Vec<EmailSummary> = selected_refs()
        .iter()
        .filter_map(|r| store.email(&r.account_id, &r.id).ok().flatten())
        .collect();

    if emails.is_empty() {
        return;
    }

    mark_selected(match read {
        true if emails.iter().any(|e| e.is_unread()) => actions::Action::MarkRead,
        true => actions::Action::MarkUnread,
        false if emails.iter().any(|e| !e.is_flagged()) => actions::Action::Flag,
        false => actions::Action::Unflag,
    });
}

/// Opens the composer on a reply to the selected email.
async fn start_reply(all: bool) {
    let Some(r) = focus_ref() else {
        return;
    };
    let b = backend().peek().clone();
    let Ok(Some(summary)) = b.store.email(&r.account_id, &r.id) else {
        return;
    };
    let cached = BODY
        .peek()
        .clone()
        .filter(|(key, _)| *key == r)
        .map(|(_, body)| body);
    let body = match cached {
        Some(body) => body,
        None => match body_for(&r).await {
            Ok(body) => body,
            Err(e) => return fail("Could not load the message to reply to", e),
        },
    };
    let me = b
        .cfg
        .account(&r.account_id)
        .map(|a| a.email.clone())
        .unwrap_or_default();
    let identities = IDENTITIES
        .peek()
        .get(&r.account_id)
        .cloned()
        .unwrap_or_default();

    open_composer(compose::reply(&summary, &body, &me, all, &identities));
}

/// The account a new message is written from: the selected mailbox's, else the
/// focused message's, else the default account.
fn compose_account() -> String {
    let from_folder = match SELECTED_MAILBOX.peek().as_ref() {
        Some(Folder::Mailbox { account_id, .. }) => Some(account_id.clone()),
        _ => None,
    };

    from_folder
        .or_else(|| focus_ref().map(|r| r.account_id))
        .or_else(|| {
            let b = backend();
            let b = b.peek();

            b.cfg
                .default_account()
                .or(b.cfg.accounts.first())
                .map(|a| a.id.clone())
        })
        .unwrap_or_default()
}

/// Opens the composer on `draft`; a draft without an account gets [`compose_account`].
pub fn open_composer(mut draft: compose::Draft) {
    if draft.account_id.is_empty() {
        draft.account_id = compose_account();
    }

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
        let sent = match get_client(&draft.account_id).await {
            Ok(Some(client)) => {
                compose::send(&b.cfg, &b.store, &client, &draft.account_id, &draft).await
            }
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

    let replied_to = draft
        .reply_to_email_id
        .as_deref()
        .map(|id| MsgRef::new(&draft.account_id, id));

    match replied_to.clone().filter(|_| archive) {
        Some(r) => {
            if let Err(e) = act_on_emails(std::slice::from_ref(&r), actions::Action::Archive).await
            {
                fail("Sent, but archiving failed", e);
            }
        }
        None => {
            if let Some(r) = &replied_to {
                refresh_row(r);
            }
        }
    }
}

/// Downloads an attachment of a message of `account_id` into the downloads folder.
pub fn save_attachment(account_id: String, att: Attachment) {
    run(async move {
        let b = backend().peek().clone();

        if b.ui_test {
            return set_status("Preview attachments are not available.");
        }

        let client = match get_client(&account_id).await {
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

/// The report error of an account that has no token.
const NO_TOKEN: &str = "no token";

/// One line about a sync pass: totals for one account, a per-account list for several.
fn sync_summary(cfg: &Config, reports: &[sync::SyncReport]) -> String {
    let outcome = |r: &sync::SyncReport| match r.error.as_deref() {
        Some(NO_TOKEN) => "no token configured".to_string(),
        Some(e) => format!("failed: {e}"),
        None => format!(
            "{} new, {} updated, {} removed",
            r.emails_added, r.emails_updated, r.emails_removed
        ),
    };

    match reports {
        [only] => match only.error.as_deref() {
            Some(NO_TOKEN) => "No token configured — open Settings".to_string(),
            Some(e) => format!("Sync failed: {e}"),
            None => format!("Synced: {}", outcome(only)),
        },
        _ => {
            let parts: Vec<String> = reports
                .iter()
                .map(|r| {
                    let name = cfg
                        .account(&r.account_id)
                        .map_or(r.account_id.as_str(), Account::display_name);

                    format!("{name}: {}", outcome(r))
                })
                .collect();

            format!("Synced — {}", parts.join("; "))
        }
    }
}

/// Syncs every enabled account. Manual runs report progress; background runs
/// only new mail and errors.
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
    let mut clients: HashMap<String, Arc<Client>> = HashMap::new();
    let mut unreachable: HashMap<String, String> = HashMap::new();

    for a in b.cfg.accounts_enabled() {
        match get_client(&a.id).await {
            Ok(Some(client)) => {
                clients.insert(a.id.clone(), client);
            }
            // No token: the pass reports it for this account.
            Ok(None) | Err(Error::NotConfigured(_)) => {}
            Err(e) => {
                unreachable.insert(a.id.clone(), e.to_string());
            }
        }
    }

    let cfg = b.cfg.clone();
    let store = b.store.clone();
    let result = tokio::spawn(async move { sync::sync_all(&cfg, &store, &clients).await })
        .await
        .map_err(|e| Error::Other(e.to_string()))
        .and_then(|r| r);
    let mut reports = match result {
        Ok(reports) => reports,
        Err(Error::Locked(_)) => {
            if manual {
                set_status("Sync already running");
            }

            return;
        }
        Err(e) => return fail("Sync failed", e),
    };

    for r in &mut reports {
        if let Some(e) = unreachable.remove(&r.account_id) {
            r.error = Some(e);
        }
    }

    let missing_token = reports.iter().any(|r| r.error.as_deref() == Some(NO_TOKEN));

    if *NEEDS_TOKEN.peek() != missing_token {
        *NEEDS_TOKEN.write() = missing_token;
    }

    if reports
        .iter()
        .any(|r| r.error.as_deref() == Some(&Error::Auth.to_string()))
    {
        backend().write().reset_client();
    }

    reload_mailboxes();
    reload_identities();
    reload_list().await;

    let added: usize = reports.iter().map(|r| r.emails_added).sum();
    let failed = reports
        .iter()
        .any(|r| r.error.as_deref().is_some_and(|e| e != NO_TOKEN));

    if manual || added > 0 || failed {
        set_status(sync_summary(&b.cfg, &reports));
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
    use posto_core::store::{Address, Store};

    use super::*;

    /// A backend over an empty in-memory store with `cfg`.
    fn backend_with(cfg: Config) -> Backend {
        Backend {
            store: Arc::new(Store::open_memory().unwrap()),
            clients: HashMap::new(),
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
    fn delete_prompts_only_for_many() {
        assert!(needs_confirm(Action::Delete, 2));
        assert!(!needs_confirm(Action::Delete, 1));
        assert!(!needs_confirm(Action::Archive, 5));
        assert!(!needs_confirm(Action::DeleteConfirmed, 5));
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
                account_id: "default".into(),
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
                account_id: "default".into(),
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

    /// A mailbox of `account` for folder tests.
    fn mailbox_of(account: &str, id: &str, role: Option<&str>) -> Mailbox {
        Mailbox {
            account_id: account.into(),
            id: id.into(),
            name: id.into(),
            role: role.map(String::from),
            parent_id: None,
            sort_order: 0,
            total: 0,
            unread: 0,
        }
    }

    #[test]
    fn default_folder_is_unified_only_with_several_accounts() {
        let boxes = [
            mailbox_of("a", "a-arch", Some("archive")),
            mailbox_of("a", "a-in", Some("inbox")),
            mailbox_of("b", "b-in", Some("inbox")),
        ];

        assert_eq!(default_folder(&boxes, 2), Some(Folder::AllInboxes));
        assert_eq!(
            default_folder(&boxes, 1),
            Some(Folder::Mailbox {
                account_id: "a".into(),
                id: "a-in".into()
            })
        );
        assert_eq!(default_folder(&[], 1), None);
        assert!(folder_exists(&Folder::AllInboxes, &boxes, 2));
        assert!(!folder_exists(&Folder::AllInboxes, &boxes, 1));
        assert!(!folder_exists(
            &Folder::Mailbox {
                account_id: "b".into(),
                id: "a-in".into()
            },
            &boxes,
            2
        ));
    }

    #[test]
    fn sync_summary_names_each_account() {
        let mut cfg = Config::default();

        cfg.accounts[0].name = "Home".into();
        cfg.add_account(Account::new("work", "w@x.io")).unwrap();

        let report = |id: &str, added: usize, error: Option<&str>| sync::SyncReport {
            account_id: id.into(),
            emails_added: added,
            error: error.map(String::from),
            ..Default::default()
        };

        assert_eq!(
            sync_summary(&cfg, &[report("default", 3, None)]),
            "Synced: 3 new, 0 updated, 0 removed"
        );
        assert_eq!(
            sync_summary(&cfg, &[report("default", 0, Some(NO_TOKEN))]),
            "No token configured — open Settings"
        );
        assert_eq!(
            sync_summary(
                &cfg,
                &[
                    report("default", 2, None),
                    report("work", 0, Some("auth rejected"))
                ]
            ),
            "Synced — Home: 2 new, 0 updated, 0 removed; w@x.io: failed: auth rejected"
        );
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
