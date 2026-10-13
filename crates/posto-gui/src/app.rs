//! Root component, global signals, action dispatch and background work.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use posto_core::classify::{self, Classifier, ClassifyReport, JEV_BASE_URL, JevClient, Tray};
use posto_core::config::{Account, Config, Secrets, UiConfig};
use posto_core::jmap::{Client, Identity};
use posto_core::shortcuts::{Action, Keymap};
use posto_core::store::{Attachment, EmailBody, EmailSummary, ListQuery, Mailbox, MsgRef};
use posto_core::theme::{self, Theme, Watcher};
use posto_core::{Error, Result, actions, body, compose, sync};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::components::{
    ClassifyPicker, Composer, ConfirmDialog, MessageList, MovePicker, Reader, Settings, Sidebar,
    Toolbar,
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

/// The question to ask before deleting `selected` messages, `destroying` of
/// which are already in Trash and would be deleted permanently. Permanent
/// deletion always asks; moving to Trash asks only for several messages.
pub fn delete_prompt(selected: usize, destroying: usize) -> Option<Confirm> {
    let s = |n: usize| if n == 1 { "" } else { "s" };
    let (title, body, accept_label) = if destroying > 0 {
        let moved = selected - destroying;

        (
            format!("Permanently delete {destroying} message{}?", s(destroying)),
            if moved > 0 {
                format!("{destroying} will be deleted permanently and {moved} moved to Trash.")
            } else {
                "This cannot be undone.".into()
            },
            "Delete permanently",
        )
    } else if selected > 1 {
        (
            format!("Delete {selected} messages?"),
            "They will be moved to Trash.".into(),
            "Delete",
        )
    } else {
        return None;
    };

    Some(Confirm {
        title,
        body,
        accept_label: accept_label.into(),
        on_accept: Action::DeleteConfirmed,
    })
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
pub static SHOW_CLASSIFY_PICKER: GlobalSignal<bool> = Signal::global(|| false);
/// The tray in force and whether it is unsure, for each listed message (by row key).
pub static CLASSIFIED: GlobalSignal<HashMap<String, (String, bool)>> = Signal::global(HashMap::new);
/// Unsure, uncorrected messages per account.
pub static UNSURE: GlobalSignal<BTreeMap<String, usize>> = Signal::global(BTreeMap::new);
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
    let result = match backend().peek().config_for_save() {
        Ok(cfg) => cfg.save().map_err(|e| e.to_string()),
        Err(_) => return,
    };

    if let Err(e) = result {
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
            // No WebKit context menu ("Inspect Element"); real menus come later.
            oncontextmenu: |evt| evt.prevent_default(),
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
            if let Some(e) = backend().read().config_error.clone() {
                div { class: "banner",
                    "config.toml could not be loaded; settings are read-only until it is fixed: {e}"
                }
            }
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
            if SHOW_CLASSIFY_PICKER() {
                ClassifyPicker {}
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
        || *SHOW_CLASSIFY_PICKER.peek()
        || CONFIRM.peek().is_some();

    if !modal && action == Action::Delete {
        let (destroy, trash) = split_trash(selected_refs());

        if let Some(prompt) = delete_prompt(destroy.len() + trash.len(), destroy.len()) {
            *CONFIRM.write() = Some(prompt);

            return;
        }
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
        Action::Delete | Action::DeleteConfirmed => run(delete_selected()),
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
        Action::Classify => {
            if !selected_refs().is_empty() {
                *SHOW_CLASSIFY_PICKER.write() = true;
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

/// Closes the topmost of composer, settings, pickers, then leaves the search box.
fn close_top() {
    if COMPOSER.peek().is_some() {
        *COMPOSER.write() = None;
    } else if *SHOW_SETTINGS.peek() {
        *SHOW_SETTINGS.write() = false;
    } else if *SHOW_MOVE_PICKER.peek() {
        *SHOW_MOVE_PICKER.write() = false;
    } else if *SHOW_CLASSIFY_PICKER.peek() {
        *SHOW_CLASSIFY_PICKER.write() = false;
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
            let new = b.store.email(&account, &old.id).ok().flatten();

            bump_counts(old, new.as_ref());
        }
    }

    Ok(applied)
}

/// Adjusts the mailbox badges for an email that changed from `old` to `new`
/// (`None`: it is gone).
fn bump_counts(old: &EmailSummary, new: Option<&EmailSummary>) {
    for m in MAILBOXES
        .write()
        .iter_mut()
        .filter(|m| m.account_id == old.account_id)
    {
        let was = old.mailbox_ids.contains(&m.id);
        let is = new.is_some_and(|n| n.mailbox_ids.contains(&m.id));

        m.total += i64::from(is) - i64::from(was);
        m.unread += i64::from(is && new.is_some_and(EmailSummary::is_unread))
            - i64::from(was && old.is_unread());
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
        Folder::Unsure { account_id } => mailboxes.iter().any(|m| m.account_id == *account_id),
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
    reload_unsure_counts();
}

/// Re-counts each enabled account's unsure, uncorrected messages.
fn reload_unsure_counts() {
    let b = backend().peek().clone();
    let mut counts = BTreeMap::new();

    for a in b.cfg.accounts_enabled() {
        match b.store.unsure_email_ids(&a.id) {
            Ok(ids) => {
                counts.insert(a.id.clone(), ids.len());
            }
            Err(e) => return set_status(format!("Could not read classifications: {e}")),
        }
    }

    if *UNSURE.peek() != counts {
        *UNSURE.write() = counts;
    }
}

/// Re-reads the classification of every listed message.
fn reload_classified() {
    let b = backend().peek().clone();
    let refs: Vec<MsgRef> = EMAILS.peek().iter().map(EmailSummary::msg_ref).collect();

    match b.store.classifications_for(&refs) {
        Ok(rows) => {
            *CLASSIFIED.write() = rows
                .into_iter()
                .map(|c| {
                    let key = MsgRef::new(&c.account_id, &c.email_id).to_string();

                    (key, (c.effective_tray().to_string(), c.unsure))
                })
                .collect();
        }
        Err(e) => set_status(format!("Could not read classifications: {e}")),
    }
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
            Some(Folder::Unsure { account_id }) => {
                let ids = store.unsure_email_ids(&account_id)?;

                store.list_emails_by_ids(&account_id, &ids[..ids.len().min(limit as usize)])
            }
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

            reload_classified();
            drop_disabled_accounts_from_selection();
        }
        Err(e) => set_status(format!("Could not load messages: {e}")),
    }
}

/// Forgets selected messages whose account is disabled or gone, so the reader,
/// reply and bulk actions never act on mail that is no longer shown.
fn drop_disabled_accounts_from_selection() {
    let b = backend().peek().clone();
    let shown: Vec<String> = SELECTION
        .peek()
        .ids
        .iter()
        .filter(|k| {
            MsgRef::parse(k)
                .is_some_and(|r| b.cfg.account(&r.account_id).is_some_and(|a| a.enabled))
        })
        .cloned()
        .collect();

    change_selection(|s| s.retain(&shown));
}

/// Splits `refs` into those already in Trash (to delete permanently) and the
/// rest (to move to Trash). A lookup error counts as "not in Trash".
fn split_trash(refs: Vec<MsgRef>) -> (Vec<MsgRef>, Vec<MsgRef>) {
    let b = backend().peek().clone();

    refs.into_iter()
        .partition(|r| actions::in_trash(&b.store, &r.account_id, &r.id).unwrap_or(false))
}

/// Deletes the selection: messages in Trash permanently, the rest into Trash.
async fn delete_selected() {
    let Some(_busy) = Flag::take(&BUSY) else {
        return;
    };
    let (destroy, trash) = split_trash(selected_refs());
    let (d, t) = (destroy.len(), trash.len());

    if d + t == 0 {
        return;
    }

    let result = settle_selection(async {
        apply_local(&destroy, actions::Action::Destroy).await?;
        apply_local(&trash, actions::Action::Delete).await?;

        Ok(())
    })
    .await;
    let done = match (d, t) {
        (d, 0) => format!("Permanently deleted {d}"),
        (0, t) => format!(
            "Moved {t} message{} to Trash",
            if t == 1 { "" } else { "s" }
        ),
        (d, t) => format!("Permanently deleted {d}, moved {t} to Trash"),
    };

    match result {
        Ok(()) => set_status(done),
        Err(e) => fail("Action failed", e),
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
    settle_selection(async { apply_local(refs, action).await.map(|_| ()) }).await
}

/// Runs `change`, then reloads the list and fixes the selection as described
/// at [`act_on_emails`].
async fn settle_selection(change: impl Future<Output = Result<()>>) -> Result<()> {
    let old_index = SELECTION
        .peek()
        .focus
        .as_ref()
        .and_then(|f| EMAILS.peek().iter().position(|e| row_key(e) == *f));
    let had_selection = !SELECTION.peek().is_empty();

    change.await?;
    reload_unsure_counts();
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

/// Files every selected email under `tray` by the user's decision and closes
/// the picker. Without a connection for an account, its messages change on
/// this device only.
pub fn classify_selected(tray: Tray) {
    *SHOW_CLASSIFY_PICKER.write() = false;
    focus_root();

    run(async move {
        let Some(_busy) = Flag::take(&BUSY) else {
            return;
        };
        let refs = selected_refs();

        if refs.is_empty() {
            return;
        }

        let b = backend().peek().clone();
        let mut clients: HashMap<String, Arc<Client>> = HashMap::new();

        for (account, _) in group_by_account(&refs) {
            if let Ok(Some(client)) = get_client(&account).await {
                clients.insert(account, client);
            }
        }

        let (mut filed, mut first_error) = (0, None);
        let settled = settle_selection(async {
            (filed, first_error) = correct_refs(&b, &clients, &refs, tray).await;

            Ok(())
        })
        .await;

        if let Err(e) = settled {
            return fail("Action failed", e);
        }

        if let Some(e) = first_error {
            return fail("Classify failed", e);
        }

        let s = if filed == 1 { "" } else { "s" };
        let offline = !b.ui_test && refs.iter().any(|r| !clients.contains_key(&r.account_id));

        set_status(format!(
            "Filed {filed} message{s} under {}{}",
            tray_label(&b.cfg, tray),
            if offline {
                " (no connection: not yet sent to the server)"
            } else {
                ""
            }
        ));
    });
}

/// Files each of `refs` under `tray` with [`classify::correct`] and keeps the
/// sidebar in step: the badges follow the moves and a tray folder the
/// correction created shows up, even when a later step failed. Returns how
/// many were filed and the first error.
async fn correct_refs(
    b: &Backend,
    clients: &HashMap<String, Arc<Client>>,
    refs: &[MsgRef],
    tray: Tray,
) -> (usize, Option<Error>) {
    let (mut filed, mut first_error) = (0, None);

    for r in refs {
        let before = b.store.email(&r.account_id, &r.id).ok().flatten();

        match classify::correct(&b.cfg, &b.store, clients, r, tray).await {
            Ok(()) => filed += 1,
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }

        adopt_new_mailboxes(&r.account_id);

        // Moves already made count even when a later step failed.
        if let (Some(old), Ok(Some(new))) = (before, b.store.email(&r.account_id, &r.id)) {
            bump_counts(&old, Some(&new));
        }
    }

    (filed, first_error)
}

/// Adds the mailboxes of `account` that the store has and the sidebar lacks
/// (a correction can create the tray folder), leaving the other badges alone.
fn adopt_new_mailboxes(account: &str) {
    let Ok(stored) = backend().peek().store.mailboxes(account) else {
        return;
    };
    let mut all = MAILBOXES.write();

    for m in stored {
        if all.iter().any(|x| x.account_id == account && x.id == m.id) {
            continue;
        }

        let at = all
            .iter()
            .rposition(|x| x.account_id == account)
            .map_or(all.len(), |i| i + 1);

        all.insert(at, m);
    }
}

/// The mailbox name of `tray` in `cfg`; spam always goes to Junk.
pub fn tray_label(cfg: &Config, tray: Tray) -> String {
    match tray {
        Tray::Spam => "Junk".to_string(),
        t => cfg
            .classify
            .trays
            .get(t.as_str())
            .cloned()
            .unwrap_or_else(|| t.as_str().to_string()),
    }
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
            classify_pass(HashMap::new(), manual).await;
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
    let connected = clients.clone();
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

    // The sync lock is released: new inbox mail is queued and can be classified.
    classify_pass(connected, manual).await;
}

/// Classifies what the sync queued, when any account has classification on
/// and a TypeSafe key exists (UI test mode: always, with [`FakeJev`]). A
/// failure becomes a toast; background passes stay quiet unless something was
/// classified or went wrong.
async fn classify_pass(clients: HashMap<String, Arc<Client>>, manual: bool) {
    let b = backend().peek().clone();

    if !b.ui_test
        && !b
            .cfg
            .accounts_enabled()
            .any(|a| b.cfg.classify_enabled_for(a))
    {
        return;
    }

    let report = if b.ui_test {
        match state::fake_classify_pass(&b.cfg, &b.store).await {
            Ok(report) => report,
            Err(e) => return fail("Classification", e),
        }
    } else {
        match Secrets::typesafe_key() {
            Ok(Some(key)) => {
                let jev = JevClient::new(
                    JEV_BASE_URL,
                    &key,
                    &b.cfg.classify.model,
                    b.cfg.classify.unsure_margin,
                );

                run_classifier(&b, clients, jev).await
            }
            Ok(None) => {
                if manual {
                    set_status("Classification is on but no TypeSafe key is set — open Settings");
                }

                return;
            }
            Err(e) => return fail("Classification", e),
        }
    };

    if report.classified > 0 || report.moved > 0 {
        reload_mailboxes();
        reload_list().await;
    }

    let errors: Vec<&String> = report
        .errors
        .iter()
        .filter(|e| manual || *e != ALREADY_RUNNING)
        .collect();

    match (report.classified, errors.first()) {
        (0, None) => {}
        (0, Some(e)) => set_status(format!("Classification failed: {e}")),
        (n, None) => set_status(format!("Classified {n} ({} unsure)", report.unsure)),
        (n, Some(e)) => set_status(format!(
            "Classified {n} ({} unsure); {} failed, first: {e}",
            report.unsure,
            errors.len()
        )),
    }
}

/// The report line of a run that found another one in progress.
const ALREADY_RUNNING: &str = "classifier already running";

/// Runs the classifier over the queue on a worker, so a panic or a slow
/// network never blocks the UI.
async fn run_classifier<C: Classifier + 'static>(
    b: &Backend,
    clients: HashMap<String, Arc<Client>>,
    classifier: C,
) -> ClassifyReport {
    let (cfg, store) = (b.cfg.clone(), b.store.clone());

    tokio::spawn(async move { classify::run(&cfg, &store, &clients, &classifier, None).await })
        .await
        .unwrap_or_else(|e| ClassifyReport {
            errors: vec![format!("classifier: {e}")],
            ..ClassifyReport::default()
        })
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
            config_error: None,
        }
    }

    /// Renders the real `App` once, headless, over `backend`.
    fn rendered(backend: Backend) -> VirtualDom {
        let mut dom = VirtualDom::new(App).with_root_context(backend);

        dom.rebuild_in_place();
        dom
    }

    #[test]
    fn delete_prompt_rules() {
        assert_eq!(delete_prompt(1, 0), None);

        let trash = delete_prompt(3, 0).unwrap();

        assert_eq!(trash.title, "Delete 3 messages?");
        assert_eq!(trash.body, "They will be moved to Trash.");

        let permanent = delete_prompt(1, 1).unwrap();

        assert_eq!(permanent.title, "Permanently delete 1 message?");
        assert_eq!(permanent.body, "This cannot be undone.");
        assert_eq!(permanent.accept_label, "Delete permanently");
        assert_eq!(permanent.on_accept, Action::DeleteConfirmed);

        let mixed = delete_prompt(3, 2).unwrap();

        assert_eq!(mixed.title, "Permanently delete 2 messages?");
        assert_eq!(
            mixed.body,
            "2 will be deleted permanently and 1 moved to Trash."
        );
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

    #[tokio::test(flavor = "current_thread")]
    async fn disabling_an_account_clears_its_selection_and_reader() {
        let dom = rendered(backend_with(Config::default()));
        let _guard = RuntimeGuard::new(dom.runtime());

        posto_core::fixtures::seed(&backend().peek().store).unwrap();
        reload_mailboxes();
        reload_list().await;

        let key = "default/fx-01".to_string();

        assert!(list_order().contains(&key));

        SELECTION
            .write()
            .click(std::slice::from_ref(&key), &key, Click::Plain);
        *BODY.write() = Some((
            MsgRef::parse(&key).unwrap(),
            EmailBody {
                email_id: "fx-01".into(),
                html: None,
                text: Some("hi".into()),
                attachments: vec![],
                in_reply_to: vec![],
                references: vec![],
                message_id: vec![],
            },
        ));

        backend().write().cfg.accounts[0].enabled = false;
        reload_mailboxes();
        reload_list().await;

        assert!(EMAILS.peek().is_empty());
        assert!(SELECTION.peek().is_empty(), "{:?}", SELECTION.peek().focus);
        assert!(BODY.peek().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unsure_folder_lists_unsure_mail_until_corrected() {
        let dom = rendered(backend_with(Config::default()));
        let _guard = RuntimeGuard::new(dom.runtime());
        let b = backend().peek().clone();

        posto_core::fixtures::seed(&b.store).unwrap();
        reload_mailboxes();

        assert_eq!(UNSURE.peek().get("default"), Some(&1));

        *SELECTED_MAILBOX.write() = Some(Folder::Unsure {
            account_id: "default".into(),
        });
        reload_list().await;

        let ids: Vec<String> = EMAILS.peek().iter().map(|e| e.id.clone()).collect();

        assert_eq!(ids, ["fx-05"]);
        assert_eq!(
            CLASSIFIED.peek().get("default/fx-05"),
            Some(&("needs_reply".to_string(), true))
        );
        assert_eq!(tray_label(&b.cfg, Tray::Spam), "Junk");
        assert_eq!(tray_label(&b.cfg, Tray::Updates), "Updates");

        classify::correct(
            &b.cfg,
            &b.store,
            &HashMap::new(),
            &MsgRef::new("default", "fx-05"),
            Tray::Updates,
        )
        .await
        .unwrap();
        reload_mailboxes();
        reload_list().await;

        assert!(EMAILS.peek().is_empty(), "{:?}", EMAILS.peek());
        assert_eq!(UNSURE.peek().get("default"), Some(&0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_sync_in_ui_test_mode_classifies_the_queue_offline() {
        let mut ui = backend_with(Config::default());

        ui.ui_test = true;

        let dom = rendered(ui);
        let _guard = RuntimeGuard::new(dom.runtime());
        let b = backend().peek().clone();

        posto_core::fixtures::seed(&b.store).unwrap();
        reload_mailboxes();

        // Classification is off in the default config; UI test mode runs the fake anyway.
        assert!(!b.cfg.classify.enabled);
        assert_eq!(b.store.classify_queue_len("default").unwrap(), 1);

        run_sync(true).await;

        let row = b.store.classification("default", "fx-11").unwrap().unwrap();
        let updates = b
            .store
            .top_level_mailbox_by_name("default", "Updates")
            .unwrap()
            .unwrap();

        assert!(row.applied, "{row:?}");
        assert_eq!(b.store.classify_queue_len("default").unwrap(), 0);
        assert!(
            b.store
                .email("default", "fx-11")
                .unwrap()
                .unwrap()
                .mailbox_ids
                .contains(&updates.id)
        );
        assert!(STATUS.peek().contains("Classified 1"), "{}", STATUS.peek());
    }

    /// Answers the calls a correction makes: creating a mailbox and patching mail.
    struct FakeJmap;

    impl wiremock::Respond for FakeJmap {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let call = &body["methodCalls"][0];
            let name = call[0].as_str().unwrap();
            let result = match name {
                "Mailbox/set" => {
                    serde_json::json!({ "created": { "c": { "id": "mb-statements" } } })
                }
                "Email/set" => {
                    let done: serde_json::Map<String, serde_json::Value> = call[1]["update"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(|id| (id.clone(), serde_json::Value::Null))
                        .collect();

                    serde_json::json!({ "updated": done })
                }
                other => panic!("unexpected JMAP call {other}"),
            };

            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "methodResponses": [[name, result, "0"]] }))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn correcting_into_a_new_tray_folder_adds_it_to_the_sidebar() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let session = include_str!("../../posto-core/tests/fixtures/session.json")
            .replace("{{base}}", &server.uri());

        Mock::given(method("GET"))
            .and(path("/session"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(session)
                    .insert_header("content-type", "application/json"),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api"))
            .respond_with(FakeJmap)
            .mount(&server)
            .await;

        let mut cfg = Config::default();

        // The fixture has no such folder, so the correction has to create it.
        cfg.classify
            .trays
            .insert("updates".into(), "Statements".into());

        let dom = rendered(backend_with(cfg));
        let _guard = RuntimeGuard::new(dom.runtime());
        let b = backend().peek().clone();

        posto_core::fixtures::seed(&b.store).unwrap();
        reload_mailboxes();

        assert!(!MAILBOXES.peek().iter().any(|m| m.name == "Statements"));

        let client = Client::connect(&format!("{}/session", server.uri()), "tok")
            .await
            .unwrap();
        let clients = HashMap::from([("default".to_string(), Arc::new(client))]);
        let (filed, error) = correct_refs(
            &b,
            &clients,
            &[MsgRef::new("default", "fx-01")],
            Tray::Updates,
        )
        .await;

        assert_eq!(filed, 1, "{error:?}");

        let added: Vec<Mailbox> = MAILBOXES
            .peek()
            .iter()
            .filter(|m| m.name == "Statements")
            .cloned()
            .collect();

        assert_eq!(added.len(), 1, "{added:?}");
        assert_eq!(
            added[0].total, 1,
            "the new folder's badge counts the message"
        );
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
