//! Root component, global signals, action dispatch and background work.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use posto_core::classify::{self, Classifier, ClassifyReport, JEV_BASE_URL, JevClient, Tray};
use posto_core::config::{Account, Config, Secrets, UiConfig};
use posto_core::provider::{Connection, Identity, MaskedEmail};
use posto_core::shortcuts::{Action, Keymap};
use posto_core::store::{
    Attachment, EmailBody, EmailSummary, ListQuery, Mailbox, MailboxGroup, MsgRef, mailbox_groups,
    mailbox_path,
};
use posto_core::theme::{self, Theme, Watcher};
use posto_core::{Error, Result, actions, body, compose, sync};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::components::{
    ClassifyPicker, Composer, ConfirmDialog, ContextMenu, LabelPicker, MessageList, MovePicker,
    Reader, Settings, Sidebar, Toolbar,
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
    pub on_accept: Accept,
}

/// What accepting a [`Confirm`] does.
#[derive(Clone, Debug, PartialEq)]
pub enum Accept {
    /// Runs a keyboard action.
    Action(Action),
    /// Permanently deletes everything in a Trash or Spam folder.
    Empty(Folder),
}

/// Runs what an accepted [`Confirm`] asked for.
pub fn accept(what: Accept) {
    match what {
        Accept::Action(a) => dispatch(a),
        Accept::Empty(folder) => run(empty_folder(folder)),
    }
}

/// A right-click menu on a Trash or Spam row.
#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
    /// Viewport position of the click.
    pub x: f64,
    pub y: f64,
    /// The folder to empty and its display name.
    pub folder: Folder,
    pub name: String,
}

/// Opens the right-click menu for a Trash or Spam row.
pub fn open_empty_menu(folder: Folder, name: String, x: f64, y: f64) {
    *MENU.write() = Some(Menu { x, y, folder, name });
}

/// The question to ask before emptying `name`, which holds `total` messages.
pub fn empty_prompt(folder: Folder, name: &str, total: i64) -> Confirm {
    Confirm {
        title: format!("Empty {name}?"),
        body: format!(
            "{total} message{} will be deleted permanently. This cannot be undone.",
            if total == 1 { "" } else { "s" }
        ),
        accept_label: format!("Empty {name}"),
        on_accept: Accept::Empty(folder),
    }
}

/// The `(account, mailbox)` pairs a folder stands for; none for Unsure and Triage.
fn folder_members(folder: &Folder) -> Vec<(String, String)> {
    match folder {
        Folder::Merged { key } => group_by_key(key).map(|g| g.members).unwrap_or_default(),
        Folder::Mailbox { account_id, id } => vec![(account_id.clone(), id.clone())],
        Folder::Unsure | Folder::Triage => Vec::new(),
    }
}

/// Closes the menu and asks before emptying its folder.
pub fn confirm_empty() {
    let Some(menu) = MENU.write().take() else {
        return;
    };
    let members = folder_members(&menu.folder);
    let total = MAILBOXES
        .peek()
        .iter()
        .filter(|m| {
            members
                .iter()
                .any(|(a, id)| *a == m.account_id && *id == m.id)
        })
        .map(|m| m.total)
        .sum();

    *CONFIRM.write() = Some(empty_prompt(menu.folder, &menu.name, total));
}

/// Permanently deletes everything in `folder`, account by account; every
/// account is attempted even when one fails.
async fn empty_folder(folder: Folder) {
    let Some(_busy) = Flag::take(&BUSY) else {
        return;
    };
    let b = backend().peek().clone();
    let (mut deleted, mut failures) = (0, Vec::new());

    for (account, mailbox) in folder_members(&folder) {
        let before: Vec<EmailSummary> = b
            .store
            .email_ids_in_mailbox(&account, &mailbox)
            .unwrap_or_default()
            .iter()
            .filter_map(|id| b.store.email(&account, id).ok().flatten())
            .collect();
        let result = match get_client(&account).await {
            Ok(client) => {
                actions::empty_mailbox(&b.store, client.as_deref(), &account, &mailbox).await
            }
            Err(e) => Err(e),
        };

        for old in &before {
            let new = b.store.email(&account, &old.id).ok().flatten();

            bump_counts(old, new.as_ref());
        }

        match result {
            Ok(n) => deleted += n,
            Err(e) => failures.push(format!("{account}: {e}")),
        }
    }

    change_selection(|s| *s = Selection::default());
    reload_list().await;

    let done = format!(
        "Permanently deleted {deleted} message{}",
        if deleted == 1 { "" } else { "s" }
    );

    if failures.is_empty() {
        set_status(done);
    } else {
        set_status(format!("{done}; failed in {}", failures.join("; ")));
    }
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
        on_accept: Accept::Action(Action::DeleteConfirmed),
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
/// Keys of the mailbox groups whose per-account members the sidebar lists.
pub static EXPANDED: GlobalSignal<BTreeSet<String>> = Signal::global(BTreeSet::new);
/// Each account's sending identities, as last synced.
pub static IDENTITIES: GlobalSignal<BTreeMap<String, Vec<Identity>>> =
    Signal::global(BTreeMap::new);
/// Each account's masked email addresses, as last synced.
pub static MASKED: GlobalSignal<BTreeMap<String, Vec<MaskedEmail>>> = Signal::global(BTreeMap::new);
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
/// Whether the composer's From menu is open.
pub static FROM_MENU: GlobalSignal<bool> = Signal::global(|| false);
pub static SHOW_SETTINGS: GlobalSignal<bool> = Signal::global(|| false);
pub static SHOW_MOVE_PICKER: GlobalSignal<bool> = Signal::global(|| false);
/// True while the label picker is open.
pub static SHOW_LABEL_PICKER: GlobalSignal<bool> = Signal::global(|| false);
pub static SHOW_CLASSIFY_PICKER: GlobalSignal<bool> = Signal::global(|| false);
/// The tray in force and whether it is unsure, for each listed message (by row key).
pub static CLASSIFIED: GlobalSignal<HashMap<String, (String, bool)>> = Signal::global(HashMap::new);
/// Unsure, uncorrected messages per account.
pub static UNSURE: GlobalSignal<BTreeMap<String, usize>> = Signal::global(BTreeMap::new);
/// Owed emails over the enabled accounts (`Store::triage_email_ids`).
pub static TRIAGE: GlobalSignal<usize> = Signal::global(|| 0);
pub static CONFIRM: GlobalSignal<Option<Confirm>> = Signal::global(|| None);
/// The open right-click menu, if any.
pub static MENU: GlobalSignal<Option<Menu>> = Signal::global(|| None);
pub static STATUS: GlobalSignal<String> = Signal::global(String::new);
pub static KEYMAP: GlobalSignal<Keymap> = Signal::global(Keymap::defaults);
pub static THEME_CSS: GlobalSignal<String> = Signal::global(|| {
    theme::detect()
        .map(|t| theme::css_vars(&t))
        .unwrap_or_default()
});
pub static SIDEBAR_OPEN: GlobalSignal<bool> = Signal::global(|| true);
/// The window folded the sidebar away (too narrow); widening brings it back.
static SIDEBAR_FOLDED: GlobalSignal<bool> = Signal::global(|| false);
/// Whether the window was narrower than the panes need at the last resize.
static NARROW: GlobalSignal<bool> = Signal::global(|| false);
/// The search box takes focus when it next mounts (the sidebar was hidden).
pub static FOCUS_SEARCH: GlobalSignal<bool> = Signal::global(|| false);
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

/// Narrowest reader, in pixels, before the sidebar folds away.
const READER_MIN: u32 = 360;

/// Shows or hides the sidebar by hand.
pub fn toggle_sidebar() {
    let open = *SIDEBAR_OPEN.peek();

    *SIDEBAR_OPEN.write() = !open;
    *SIDEBAR_FOLDED.write() = false;
}

/// Shows the sidebar if needed and focuses its search box.
fn open_search() {
    if *SIDEBAR_OPEN.peek() {
        focus("search");
    } else {
        *FOCUS_SEARCH.write() = true;
        *SIDEBAR_OPEN.write() = true;
        *SIDEBAR_FOLDED.write() = false;
    }
}

/// Opens Settings in the main pane.
pub fn open_settings() {
    *SHOW_SETTINGS.write() = true;
}

/// Takes the window width in pixels. Becoming narrower than sidebar, list and
/// [`READER_MIN`] hides the sidebar; widening past that again shows it, unless
/// it was shown or hidden by hand in between.
pub fn set_window_width(width: f64) {
    let need = *SIDEBAR_WIDTH.peek() + *LIST_WIDTH.peek() + 12 + READER_MIN;
    let narrow = width < f64::from(need);

    if narrow == *NARROW.peek() {
        return;
    }

    *NARROW.write() = narrow;

    if narrow && *SIDEBAR_OPEN.peek() {
        *SIDEBAR_OPEN.write() = false;
        *SIDEBAR_FOLDED.write() = true;
    } else if !narrow && *SIDEBAR_FOLDED.peek() {
        *SIDEBAR_OPEN.write() = true;
        *SIDEBAR_FOLDED.write() = false;
    }
}

/// Mailbox column width limits, in pixels.
pub fn clamp_sidebar(width: u32) -> u32 {
    width.clamp(180, 480)
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

    // The webview reports its width on every resize. Headless tests have no
    // webview; the eval fails there and the loop ends.
    use_future(|| async {
        let mut width = document::eval(
            "const send = () => dioxus.send(window.innerWidth);
             window.addEventListener('resize', send);
             send();
             await new Promise(() => {});",
        );

        while let Ok(w) = width.recv::<f64>().await {
            set_window_width(w);
        }
    });

    // A collapsed sidebar takes no column and no divider; Settings takes the
    // list's and the reader's place.
    let settings = SHOW_SETTINGS();
    let main = if settings {
        "1fr".to_string()
    } else {
        format!("{}px 6px 1fr", LIST_WIDTH())
    };
    let columns = if SIDEBAR_OPEN() {
        format!("{}px 6px {main}", SIDEBAR_WIDTH())
    } else {
        main
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
                    "Not signed in — open Settings"
                    button { onclick: |_| open_settings(), "Settings" }
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
                if settings {
                    Settings {}
                } else {
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
            }
            if COMPOSER.read().is_some() {
                Composer {}
            }
            if SHOW_MOVE_PICKER() {
                MovePicker {}
            }
            if SHOW_LABEL_PICKER() {
                LabelPicker {}
            }
            if SHOW_CLASSIFY_PICKER() {
                ClassifyPicker {}
            }
            if CONFIRM.read().is_some() {
                ConfirmDialog {}
            }
            if MENU.read().is_some() {
                ContextMenu {}
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
        || *SHOW_LABEL_PICKER.peek()
        || *SHOW_CLASSIFY_PICKER.peek()
        || CONFIRM.peek().is_some()
        || MENU.peek().is_some();

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
        Action::ToggleSidebar => toggle_sidebar(),
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
            if !selected_refs().is_empty() {
                *SHOW_MOVE_PICKER.write() = true;
            }
        }
        Action::Label => {
            if !selected_refs().is_empty() {
                *SHOW_LABEL_PICKER.write() = true;
            }
        }
        Action::Classify => {
            if !selected_refs().is_empty() {
                *SHOW_CLASSIFY_PICKER.write() = true;
            }
        }
        Action::Search => open_search(),
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

/// Reports a failed operation; rejected credentials drop the connection.
fn fail(what: &str, e: Error) {
    match e {
        Error::NotConfigured(_) => set_status("Not signed in — open Settings"),
        Error::Auth => {
            backend().write().reset_client();
            set_status(format!("{what}: the server rejected the credentials"));
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
    if *FROM_MENU.peek() && COMPOSER.peek().is_some() {
        *FROM_MENU.write() = false;
        focus_root();

        return;
    }

    if COMPOSER.peek().is_some() {
        *COMPOSER.write() = None;
    } else if *SHOW_SETTINGS.peek() {
        *SHOW_SETTINGS.write() = false;
    } else if *SHOW_MOVE_PICKER.peek() {
        *SHOW_MOVE_PICKER.write() = false;
    } else if *SHOW_LABEL_PICKER.peek() {
        *SHOW_LABEL_PICKER.write() = false;
    } else if *SHOW_CLASSIFY_PICKER.peek() {
        *SHOW_CLASSIFY_PICKER.write() = false;
    } else if CONFIRM.peek().is_some() {
        *CONFIRM.write() = None;
    } else if MENU.peek().is_some() {
        *MENU.write() = None;
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

/// The server connection of `account_id`; sets the "not signed in" banner as a side
/// effect when it has no credentials.
pub async fn get_client(account_id: &str) -> Result<Option<Arc<Connection>>> {
    let result = state::client(backend(), account_id).await;

    if matches!(result, Err(Error::NotConfigured(_))) && !*NEEDS_TOKEN.peek() {
        *NEEDS_TOKEN.write() = true;
    }

    result
}

/// Applies `action` to the server and store, account by account, and mirrors
/// it in the mailbox badges.
async fn apply_local(refs: &[MsgRef], action: actions::Action) -> Result<usize> {
    let mut applied = 0;

    for (account, ids) in group_by_account(refs) {
        applied += apply_one_account(&account, &ids, action.clone()).await?;
    }

    Ok(applied)
}

/// Applies `action` to `ids` of one `account` and mirrors it in the mailbox
/// badges; returns how many emails the core reports as changed.
async fn apply_one_account(
    account: &str,
    ids: &[String],
    action: actions::Action,
) -> Result<usize> {
    let b = backend().peek().clone();
    let client = get_client(account).await?;
    let before: Vec<EmailSummary> = ids
        .iter()
        .filter_map(|id| b.store.email(account, id).ok().flatten())
        .collect();
    let applied = actions::apply(&b.store, client.as_deref(), account, ids, action).await?;

    for old in &before {
        let new = b.store.email(account, &old.id).ok().flatten();

        bump_counts(old, new.as_ref());
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

/// Re-reads every account's cached sending identities and masked emails.
pub fn reload_identities() {
    let b = backend().peek().clone();
    let mut all = BTreeMap::new();
    let mut masked = BTreeMap::new();

    for a in &b.cfg.accounts {
        match b.store.identities(&a.id) {
            Ok(list) => {
                all.insert(a.id.clone(), list);
            }
            Err(e) => return fail("Could not read identities", e),
        }

        match b.store.masked_emails(&a.id) {
            Ok(list) => {
                masked.insert(a.id.clone(), list);
            }
            Err(e) => return fail("Could not read masked emails", e),
        }
    }

    *IDENTITIES.write() = all;
    *MASKED.write() = masked;
}

/// The folder to start on: the merged inbox with several accounts, else the
/// first inbox (or first mailbox).
fn default_folder(mailboxes: &[Mailbox], accounts: &[String]) -> Option<Folder> {
    if accounts.len() > 1 {
        return Some(Folder::Merged {
            key: "role:inbox".into(),
        });
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

/// True when `folder` still exists among `mailboxes` of the enabled `accounts`.
fn folder_exists(folder: &Folder, mailboxes: &[Mailbox], accounts: &[String]) -> bool {
    match folder {
        Folder::Merged { key } => mailbox_groups(mailboxes, accounts)
            .iter()
            .any(|g| g.key == *key),
        Folder::Mailbox { account_id, id } => mailboxes
            .iter()
            .any(|m| m.account_id == *account_id && m.id == *id),
        Folder::Unsure | Folder::Triage => true,
    }
}

/// The ids of the enabled accounts, in config order.
pub fn enabled_ids(cfg: &Config) -> Vec<String> {
    cfg.accounts_enabled().map(|a| a.id.clone()).collect()
}

/// The mailbox group with `key` among the enabled accounts' current mailboxes.
fn group_by_key(key: &str) -> Option<MailboxGroup> {
    let accounts = enabled_ids(&backend().peek().cfg);

    mailbox_groups(&MAILBOXES.peek(), &accounts)
        .into_iter()
        .find(|g| g.key == key)
}

/// Shows or hides the per-account members of the group with `key`.
pub fn toggle_expanded(key: &str) {
    let mut expanded = EXPANDED.write();

    if !expanded.remove(key) {
        expanded.insert(key.to_string());
    }
}

/// One line of the multi-account sidebar's group section.
#[derive(Clone, Debug, PartialEq)]
pub enum SidebarRow {
    /// A mailbox group with its summed unread count.
    Group {
        /// Group key.
        key: String,
        /// Group display name.
        name: String,
        /// The group's role, for its icon.
        role: Option<String>,
        /// Unread messages in all members.
        unread: i64,
        /// Whether the members follow.
        expanded: bool,
    },
    /// One member of an expanded group.
    Member {
        /// The folder a click selects.
        folder: Folder,
        /// Account name, plus the mailbox path when the account has several members.
        label: String,
        /// Unread messages in the member.
        unread: i64,
    },
}

/// The rows of the multi-account sidebar: one per group, each followed by its
/// members when `expanded` holds its key. `accounts` are `(id, display name)`.
pub fn sidebar_rows(
    groups: &[MailboxGroup],
    expanded: &BTreeSet<String>,
    accounts: &[(String, String)],
    mailboxes: &[Mailbox],
) -> Vec<SidebarRow> {
    let mut rows = Vec::new();

    for g in groups {
        let open = expanded.contains(&g.key);

        rows.push(SidebarRow::Group {
            key: g.key.clone(),
            name: g.name.clone(),
            role: g.role.clone(),
            unread: g.unread,
            expanded: open,
        });

        if !open {
            continue;
        }

        for (account, id) in &g.members {
            let name = accounts
                .iter()
                .find(|(a, _)| a == account)
                .map_or(account.as_str(), |(_, name)| name.as_str());
            let several = g.members.iter().filter(|(a, _)| a == account).count() > 1;
            let label = match mailbox_path(mailboxes, account, id) {
                Some(path) if several => format!("{name} › {path}"),
                _ => name.to_string(),
            };

            rows.push(SidebarRow::Member {
                folder: Folder::Mailbox {
                    account_id: account.clone(),
                    id: id.clone(),
                },
                label,
                unread: mailboxes
                    .iter()
                    .find(|m| m.account_id == *account && m.id == *id)
                    .map_or(0, |m| m.unread),
            });
        }
    }

    rows
}

/// Re-reads the mailbox list (and server counts) of every enabled account;
/// selects a default folder when none is selected or the selection is gone.
pub fn reload_mailboxes() {
    let b = backend().peek().clone();
    let accounts = enabled_ids(&b.cfg);
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
        .is_some_and(|f| folder_exists(f, &all, &accounts))
    {
        let next = default_folder(&all, &accounts);

        if next != current {
            *SELECTED_MAILBOX.write() = next;
        }
    }

    *MAILBOXES.write() = all;
    reload_unsure_counts();
    reload_triage_counts();
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

/// Re-counts the owed emails of the enabled accounts.
fn reload_triage_counts() {
    let b = backend().peek().clone();
    let mut total = 0;

    for a in b.cfg.accounts_enabled() {
        match b.store.triage_email_ids(&a.id) {
            Ok(ids) => total += ids.len(),
            Err(e) => return set_status(format!("Could not read triage: {e}")),
        }
    }

    if *TRIAGE.peek() != total {
        *TRIAGE.write() = total;
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
    let members = match &folder {
        Some(Folder::Merged { key }) => group_by_key(key).map(|g| g.members).unwrap_or_default(),
        _ => Vec::new(),
    };
    let accounts = enabled_ids(&b.cfg);

    let loaded = tokio::task::spawn_blocking(move || {
        if !q.is_empty() {
            return store.search(None, &q, limit);
        }

        match f {
            Some(Folder::Merged { .. }) => store.list_emails_in(
                &members,
                &ListQuery {
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
            Some(Folder::Triage) => store.list_triage(&accounts, limit, 0),
            Some(Folder::Unsure) => {
                let mut all = Vec::new();

                for account in &accounts {
                    let ids = store.unsure_email_ids(account)?;

                    all.extend(
                        store.list_emails_by_ids(account, &ids[..ids.len().min(limit as usize)])?,
                    );
                }

                all.sort_by(|a, b| {
                    b.received_at
                        .cmp(&a.received_at)
                        .then_with(|| a.account_id.cmp(&b.account_id))
                        .then_with(|| a.id.cmp(&b.id))
                });
                all.truncate(limit as usize);

                Ok(all)
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
    settle_selection(apply_local(refs, action))
        .await
        .map(|_| ())
}

/// Runs `change`, then reloads the list and fixes the selection as described
/// at [`act_on_emails`]; hands back what `change` returned.
async fn settle_selection<T>(change: impl Future<Output = Result<T>>) -> Result<T> {
    let old_index = SELECTION
        .peek()
        .focus
        .as_ref()
        .and_then(|f| EMAILS.peek().iter().position(|e| row_key(e) == *f));
    let had_selection = !SELECTION.peek().is_empty();

    let done = change.await?;

    reload_unsure_counts();
    reload_triage_counts();
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

    Ok(done)
}

/// How many of the selected emails are in a mailbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelState {
    /// Every selected email.
    All,
    /// Some, not all.
    Some,
    /// None (also when nothing is selected).
    None,
}

/// The listed emails whose row is selected, in list order.
pub fn selected_emails<'a>(
    emails: &'a [EmailSummary],
    selection: &Selection,
) -> Vec<&'a EmailSummary> {
    emails
        .iter()
        .filter(|e| selection.is_selected(&row_key(e)))
        .collect()
}

/// The mailbox ids of `account` among the group `members`.
fn account_members<'a>(
    members: &'a [(String, String)],
    account: &'a str,
) -> impl Iterator<Item = &'a str> {
    members
        .iter()
        .filter(move |(a, _)| a == account)
        .map(|(_, id)| id.as_str())
}

/// True when `email` is in any member of the group in its own account.
fn has_label(email: &EmailSummary, members: &[(String, String)]) -> bool {
    account_members(members, &email.account_id).any(|id| email.mailbox_ids.iter().any(|m| m == id))
}

/// Whether `selected` have the group `members`: all, some or none of them,
/// each message judged by the members of its own account (a message of an
/// account with no member does not have it).
pub fn label_state(selected: &[&EmailSummary], members: &[(String, String)]) -> LabelState {
    let has = selected.iter().filter(|e| has_label(e, members)).count();

    if has == 0 {
        LabelState::None
    } else if has == selected.len() {
        LabelState::All
    } else {
        LabelState::Some
    }
}

/// What toggling the group would do to the messages that can take it (those
/// whose account has a member): `None` when no message can, else `Some(true)`
/// to add it and `Some(false)` to remove it because every such message has it.
pub fn label_intent(selected: &[&EmailSummary], members: &[(String, String)]) -> Option<bool> {
    let mut eligible = selected
        .iter()
        .filter(|e| account_members(members, &e.account_id).next().is_some())
        .peekable();

    eligible.peek()?;

    Some(!eligible.all(|e| has_label(e, members)))
}

/// False when removing every member of the group from its account would leave
/// one of `selected` in no mailbox.
pub fn label_removable(selected: &[&EmailSummary], members: &[(String, String)]) -> bool {
    !selected.iter().any(|e| {
        let own: Vec<&str> = account_members(members, &e.account_id).collect();

        !own.is_empty()
            && !e.mailbox_ids.is_empty()
            && e.mailbox_ids.iter().all(|m| own.contains(&m.as_str()))
    })
}

/// An account, its first group member and the email ids to act on there.
type Target = (String, String, Vec<String>);

/// The selected messages split by account: for each account in first-seen
/// order its first member of the group and its email ids, then the ids of the
/// selection's accounts that have no member, in `accounts` order.
pub fn member_targets(
    members: &[(String, String)],
    refs: &[MsgRef],
    accounts: &[String],
) -> (Vec<Target>, Vec<String>) {
    let mut targets = Vec::new();
    let mut without = Vec::new();

    for (account, ids) in group_by_account(refs) {
        let first = account_members(members, &account).next().map(String::from);

        match first {
            Some(first) => targets.push((account, first, ids)),
            None => without.push(account),
        }
    }

    let missing = accounts
        .iter()
        .filter(|a| without.contains(a))
        .cloned()
        .collect();

    (targets, missing)
}

/// The toast after a group action: `verb` ("Moved", "Labelled", …) over how many
/// of `total` selected messages were `applied`; `attempted` counts those whose
/// account had a member of group `name`, `missing` names the accounts without
/// one and `failures` the `(account, error text)` of refused ones.
pub fn action_toast(
    verb: &str,
    applied: usize,
    total: usize,
    attempted: usize,
    missing: &[String],
    failures: &[(String, String)],
    name: &str,
) -> String {
    let s = if total == 1 { "" } else { "s" };

    if applied == total && missing.is_empty() && failures.is_empty() {
        return format!("{verb} {applied} message{s}");
    }

    if attempted == 0 && failures.is_empty() {
        return format!("No \"{name}\" in {}", missing.join(", "));
    }

    let mut toast = format!("{verb} {applied} of {total} message{s}");

    if !missing.is_empty() {
        toast.push_str(&format!("; no \"{name}\" in {}", missing.join(", ")));
    }

    for (account, error) in failures {
        toast.push_str(&format!("; {account}: {error}"));
    }

    toast
}

/// What a group action did, summed over the accounts it was sent to.
#[derive(Default)]
struct GroupOutcome {
    /// Emails the core reports as changed.
    applied: usize,
    /// Messages whose account was attempted.
    attempted: usize,
    /// `(account display name, error text)` of each account that failed.
    failures: Vec<(String, String)>,
}

/// The display name of account `id`.
fn account_name(id: &str) -> String {
    let b = backend();
    let b = b.peek();

    b.cfg
        .account(id)
        .map_or(id, Account::display_name)
        .to_string()
}

/// Display names of account ids, in order.
fn account_names(ids: &[String]) -> Vec<String> {
    ids.iter().map(|id| account_name(id)).collect()
}

/// Applies each `(account, email ids, action)` on its own: a failing account
/// does not stop the others and is recorded in the outcome.
async fn apply_to_accounts(jobs: Vec<(String, Vec<String>, actions::Action)>) -> GroupOutcome {
    let mut outcome = GroupOutcome::default();

    for (account, ids, action) in jobs {
        outcome.attempted += ids.len();

        match apply_one_account(&account, &ids, action).await {
            Ok(n) => outcome.applied += n,
            Err(e) => {
                if matches!(e, Error::Auth) {
                    backend().write().reset_client();
                }

                outcome
                    .failures
                    .push((account_name(&account), e.to_string()));
            }
        }
    }

    outcome
}

/// The group `key` and the selected messages, unless nothing is selected or
/// the group is gone.
fn group_selection(key: &str) -> Option<(MailboxGroup, Vec<MsgRef>)> {
    let refs = selected_refs();

    if refs.is_empty() {
        return None;
    }

    Some((group_by_key(key)?, refs))
}

/// Toggles the group `key` on every selected message and keeps the picker
/// open (see [`label_group`]).
pub fn toggle_label(key: String) {
    run(label_group(key));
}

/// Adds the group `key` to the selected messages unless every message that can
/// take it has it, in which case it is removed from them; ignored while
/// another action is pending. Each account is handled on its own, through its
/// first member (add) or all its members (remove). The picker closes only when
/// the selection ends up empty.
pub async fn label_group(key: String) {
    let Some(_busy) = Flag::take(&BUSY) else {
        return;
    };
    let Some((group, refs)) = group_selection(&key) else {
        return;
    };
    let (intent, removable) = {
        let (emails, selection) = (EMAILS.peek(), SELECTION.peek());
        let picked = selected_emails(&emails, &selection);

        (
            label_intent(&picked, &group.members),
            label_removable(&picked, &group.members),
        )
    };

    if intent == Some(false) && !removable {
        return set_status("A message must keep at least one label.");
    }

    let add = intent.unwrap_or(true);
    let (targets, missing) =
        member_targets(&group.members, &refs, &enabled_ids(&backend().peek().cfg));
    let jobs = targets
        .into_iter()
        .map(|(account, first, ids)| {
            let patch = if add {
                (vec![first], vec![])
            } else {
                let all = account_members(&group.members, &account);

                (vec![], all.map(String::from).collect())
            };

            (
                account,
                ids,
                actions::Action::PatchMailboxes {
                    add: patch.0,
                    remove: patch.1,
                },
            )
        })
        .collect();
    let verb = if add {
        "Labelled"
    } else {
        "Removed label from"
    };

    match settle_selection(async { Ok(apply_to_accounts(jobs).await) }).await {
        Ok(o) => set_status(action_toast(
            verb,
            o.applied,
            refs.len(),
            o.attempted,
            &account_names(&missing),
            &o.failures,
            &group.name,
        )),
        Err(e) => fail("Action failed", e),
    }

    if selected_refs().is_empty() {
        *SHOW_LABEL_PICKER.write() = false;
        focus_root();
    }
}

/// Moves the selected emails to the group `key` and closes the picker.
pub fn move_selected(key: String) {
    *SHOW_MOVE_PICKER.write() = false;
    focus_root();
    run(move_to_group(key));
}

/// Moves each selected email to the first member of the group `key` in its own
/// account. Accounts without a member are named in the toast, never given one;
/// a refusing account does not stop the others.
pub async fn move_to_group(key: String) {
    let Some(_busy) = Flag::take(&BUSY) else {
        return;
    };
    let Some((group, refs)) = group_selection(&key) else {
        return;
    };
    let (targets, missing) =
        member_targets(&group.members, &refs, &enabled_ids(&backend().peek().cfg));
    let jobs = targets
        .into_iter()
        .map(|(account, mailbox_id, ids)| (account, ids, actions::Action::Move { mailbox_id }))
        .collect();

    match settle_selection(async { Ok(apply_to_accounts(jobs).await) }).await {
        Ok(o) => set_status(action_toast(
            "Moved",
            o.applied,
            refs.len(),
            o.attempted,
            &account_names(&missing),
            &o.failures,
            &group.name,
        )),
        Err(e) => fail("Action failed", e),
    }
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
        let mut clients: HashMap<String, Arc<Connection>> = HashMap::new();

        for (account, _) in group_by_account(&refs) {
            if let Ok(Some(client)) = get_client(&account).await {
                clients.insert(account, client);
            }
        }

        let (mut filed, mut first_error) = (0, None);
        let settled = settle_selection(async {
            (filed, first_error) = correct_refs(&b, &clients, &refs, tray.clone()).await;

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
            tray_label(&b.cfg, &tray),
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
    clients: &HashMap<String, Arc<Connection>>,
    refs: &[MsgRef],
    tray: Tray,
) -> (usize, Option<Error>) {
    let (mut filed, mut first_error) = (0, None);

    for r in refs {
        let before = b.store.email(&r.account_id, &r.id).ok().flatten();

        match classify::correct(&b.cfg, &b.store, clients, r, tray.clone()).await {
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
pub fn tray_label(cfg: &Config, tray: &Tray) -> String {
    if *tray == Tray::SPAM {
        return "Junk".to_string();
    }

    cfg.classify
        .trays
        .get(tray.as_str())
        .cloned()
        .unwrap_or_else(|| tray.as_str().to_string())
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

    let masked = MASKED
        .peek()
        .get(&r.account_id)
        .cloned()
        .unwrap_or_default();

    open_composer(compose::reply(
        &summary,
        &body,
        &me,
        all,
        &identities,
        &masked,
    ));
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
    // A replaced draft takes its focused filter along without a focusout.
    if *FROM_MENU.peek() {
        *FROM_MENU.write() = false;
        focus_root();
    }

    *COMPOSER.write() = Some(draft);
}

/// Discards the open draft.
pub fn close_composer() {
    *FROM_MENU.write() = false;
    *COMPOSER.write() = None;
    focus_root();
}

/// Sends the open draft; `archive` also archives the email it replies to.
pub fn send(archive: bool) {
    // The filter, if mounted, never sends focusout once the menu is gone.
    if *FROM_MENU.peek() {
        *FROM_MENU.write() = false;
        focus_root();
    }

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

    let replied_to = replied_to(&draft);
    let cross_account = draft
        .reply_to_account_id
        .as_deref()
        .is_some_and(|a| a != draft.account_id);
    let toast = finish_send(
        replied_to.clone(),
        cross_account,
        archive,
        async |r: &MsgRef, action| {
            act_on_emails(std::slice::from_ref(r), action)
                .await
                .inspect_err(|e| {
                    if matches!(e, Error::Auth) {
                        backend().write().reset_client();
                    }
                })
        },
    )
    .await;

    if !archive && let Some(r) = &replied_to {
        refresh_row(r);
        let _ = settle_selection(async { Ok(()) }).await;
    }

    if toast != "Sent" {
        set_status(toast);
    }
}

/// The message `draft` replies to: its id in the account it was received by,
/// which is not the sending account for a reply from another identity.
fn replied_to(draft: &compose::Draft) -> Option<MsgRef> {
    let id = draft.reply_to_email_id.as_deref()?;
    let account = draft
        .reply_to_account_id
        .as_deref()
        .unwrap_or(&draft.account_id);

    Some(MsgRef::new(account, id))
}

/// What follows a submitted mail, through `act`: a reply from another account
/// marks the original answered (`compose::send` only does so in its own
/// account), then `archive` archives it. A failed step does not stop the next
/// one. Returns the toast: `Sent`, or the last failure.
async fn finish_send(
    replied_to: Option<MsgRef>,
    cross_account: bool,
    archive: bool,
    act: impl AsyncFn(&MsgRef, actions::Action) -> Result<()>,
) -> String {
    let mut toast = "Sent";
    let Some(r) = replied_to else {
        return toast.into();
    };

    if cross_account && act(&r, actions::Action::MarkAnswered).await.is_err() {
        toast = "Sent, but marking the original answered failed";
    }

    if archive && act(&r, actions::Action::Archive).await.is_err() {
        toast = "Sent, but archiving failed";
    }

    toast.into()
}

/// Downloads an attachment of a message of `account_id` into the downloads folder.
pub fn save_attachment(account_id: String, email_id: String, att: Attachment) {
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

        match body::save_attachment(&client, &email_id, &att, &dir).await {
            Ok(path) => set_status(format!("Saved to {}", path.display())),
            Err(e) => fail("Download failed", e),
        }
    });
}

/// The report error of an account that has no credentials.
const NO_TOKEN: &str = "no token";

/// One line about a sync pass: totals for one account, a per-account list for several.
fn sync_summary(cfg: &Config, reports: &[sync::SyncReport]) -> String {
    let outcome = |r: &sync::SyncReport| match r.error.as_deref() {
        Some(NO_TOKEN) => "not signed in".to_string(),
        Some(e) => format!("failed: {e}"),
        None => format!(
            "{} new, {} updated, {} removed",
            r.emails_added, r.emails_updated, r.emails_removed
        ),
    };

    match reports {
        [only] => match only.error.as_deref() {
            Some(NO_TOKEN) => "Not signed in — open Settings".to_string(),
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
    let mut clients: HashMap<String, Arc<Connection>> = HashMap::new();
    let mut unreachable: HashMap<String, String> = HashMap::new();

    for a in b.cfg.accounts_enabled() {
        match get_client(&a.id).await {
            Ok(Some(client)) => {
                clients.insert(a.id.clone(), client);
            }
            // No credentials: the pass reports it for this account.
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
async fn classify_pass(clients: HashMap<String, Arc<Connection>>, manual: bool) {
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
            Ok(Some(key)) => match JevClient::new(JEV_BASE_URL, &key, &b.cfg.classify) {
                Ok(jev) => run_classifier(&b, clients, jev).await,
                Err(e) => return fail("Classification", e),
            },
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
    clients: HashMap<String, Arc<Connection>>,
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
    use posto_core::store::{Address, Classified, Store};

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
        assert_eq!(permanent.on_accept, Accept::Action(Action::DeleteConfirmed));

        let mixed = delete_prompt(3, 2).unwrap();

        assert_eq!(mixed.title, "Permanently delete 2 messages?");
        assert_eq!(
            mixed.body,
            "2 will be deleted permanently and 1 moved to Trash."
        );
    }

    #[test]
    fn clamp_widths() {
        assert_eq!(clamp_sidebar(179), 180);
        assert_eq!(clamp_sidebar(481), 480);
        assert_eq!(clamp_sidebar(300), 300);
        assert_eq!(clamp_list(239), 240);
        assert_eq!(clamp_list(801), 800);
        assert_eq!(clamp_list(300), 300);
    }

    #[test]
    fn sidebar_folds_in_a_narrow_window_unless_toggled_by_hand() {
        let dom = rendered(backend_with(Config::default()));

        dom.in_scope(ScopeId::ROOT, || {
            // Default widths: 220 + 360 + 12 + 360 = 952.
            set_window_width(1400.0);
            assert!(*SIDEBAR_OPEN.peek());

            set_window_width(900.0);
            assert!(!*SIDEBAR_OPEN.peek(), "too narrow folds it");

            set_window_width(1400.0);
            assert!(
                *SIDEBAR_OPEN.peek(),
                "widening brings a folded sidebar back"
            );

            toggle_sidebar();
            set_window_width(900.0);
            set_window_width(1400.0);
            assert!(!*SIDEBAR_OPEN.peek(), "hidden by hand stays hidden");
        });
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
                reply_to: vec![],
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
    async fn empty_menu_asks_then_destroys_the_folder() {
        let mut ui = backend_with(Config::default());

        ui.ui_test = true;

        let dom = rendered(ui);
        let _guard = RuntimeGuard::new(dom.runtime());
        let b = backend().peek().clone();

        posto_core::fixtures::seed(&b.store).unwrap();
        reload_mailboxes();

        let trash = b
            .store
            .mailbox_by_role("default", "trash")
            .unwrap()
            .unwrap();
        let trashed = b.store.email_ids_in_mailbox("default", &trash.id).unwrap();
        let folder = Folder::Mailbox {
            account_id: "default".into(),
            id: trash.id.clone(),
        };

        assert_eq!(trashed.len(), 1, "the fixture has one trashed message");

        open_empty_menu(folder.clone(), "Trash".into(), 1.0, 2.0);
        confirm_empty();

        let prompt = CONFIRM.peek().clone().unwrap();

        assert!(MENU.peek().is_none());
        assert_eq!(prompt.title, "Empty Trash?");
        assert_eq!(
            prompt.body,
            "1 message will be deleted permanently. This cannot be undone."
        );
        assert_eq!(prompt.accept_label, "Empty Trash");
        assert_eq!(prompt.on_accept, Accept::Empty(folder.clone()));
        assert_eq!(
            empty_prompt(Folder::Unsure, "Spam", 3).body,
            "3 messages will be deleted permanently. This cannot be undone."
        );

        empty_folder(folder).await;

        assert!(
            b.store
                .email_ids_in_mailbox("default", &trash.id)
                .unwrap()
                .is_empty()
        );
        assert!(b.store.email("default", "fx-01").unwrap().is_some());
        assert_eq!(
            MAILBOXES
                .peek()
                .iter()
                .find(|m| m.id == trash.id)
                .unwrap()
                .total,
            0
        );
        assert_eq!(*STATUS.peek(), "Permanently deleted 1 message");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unsure_folder_lists_unsure_mail_until_corrected() {
        let dom = rendered(backend_with(Config::default()));
        let _guard = RuntimeGuard::new(dom.runtime());
        let b = backend().peek().clone();

        posto_core::fixtures::seed(&b.store).unwrap();
        reload_mailboxes();

        assert_eq!(UNSURE.peek().get("default"), Some(&1));

        *SELECTED_MAILBOX.write() = Some(Folder::Unsure);
        reload_list().await;

        let ids: Vec<String> = EMAILS.peek().iter().map(|e| e.id.clone()).collect();

        assert_eq!(ids, ["fx-05"]);
        assert_eq!(
            CLASSIFIED.peek().get("default/fx-05"),
            Some(&("needs_reply".to_string(), true))
        );
        assert_eq!(tray_label(&b.cfg, &Tray::SPAM), "Junk");
        assert_eq!(tray_label(&b.cfg, &Tray::new("receipts")), "Receipts");

        classify::correct(
            &b.cfg,
            &b.store,
            &HashMap::new(),
            &MsgRef::new("default", "fx-05"),
            Tray::new("receipts"),
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
        let receipts = b
            .store
            .top_level_mailbox_by_name("default", "Receipts")
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
                .contains(&receipts.id)
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
            .insert("receipts".into(), "Statements".into());

        let dom = rendered(backend_with(cfg));
        let _guard = RuntimeGuard::new(dom.runtime());
        let b = backend().peek().clone();

        posto_core::fixtures::seed(&b.store).unwrap();
        reload_mailboxes();

        assert!(!MAILBOXES.peek().iter().any(|m| m.name == "Statements"));

        let client = Connection::connect_jmap(&format!("{}/session", server.uri()), "tok")
            .await
            .unwrap();
        let clients = HashMap::from([("default".to_string(), Arc::new(client))]);
        let (filed, error) = correct_refs(
            &b,
            &clients,
            &[MsgRef::new("default", "fx-01")],
            Tray::new("receipts"),
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

    /// Account ids for tests.
    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_folder_is_merged_inbox_with_several_accounts() {
        let boxes = [
            mailbox_of("a", "a-arch", Some("archive")),
            mailbox_of("a", "a-in", Some("inbox")),
            mailbox_of("b", "b-in", Some("inbox")),
        ];
        let inbox = Folder::Merged {
            key: "role:inbox".into(),
        };

        assert_eq!(
            default_folder(&boxes, &ids(&["a", "b"])),
            Some(inbox.clone())
        );
        assert_eq!(
            default_folder(&boxes, &ids(&["a"])),
            Some(Folder::Mailbox {
                account_id: "a".into(),
                id: "a-in".into()
            })
        );
        assert_eq!(default_folder(&[], &ids(&["a"])), None);
        assert!(folder_exists(&inbox, &boxes, &ids(&["a", "b"])));
        assert!(folder_exists(&inbox, &boxes, &ids(&["a"])));
        assert!(!folder_exists(
            &Folder::Merged {
                key: "name:nope".into()
            },
            &boxes,
            &ids(&["a", "b"])
        ));
        assert!(!folder_exists(
            &Folder::Mailbox {
                account_id: "b".into(),
                id: "a-in".into()
            },
            &boxes,
            &ids(&["a", "b"])
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
            "Not signed in — open Settings"
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

    /// An email of account `default` in `mailboxes`.
    fn labelled(id: &str, mailboxes: &[&str]) -> EmailSummary {
        labelled_in("default", id, mailboxes)
    }

    /// An email of `account` in `mailboxes`.
    fn labelled_in(account: &str, id: &str, mailboxes: &[&str]) -> EmailSummary {
        EmailSummary {
            account_id: account.into(),
            id: id.into(),
            thread_id: format!("t-{id}"),
            received_at: "2026-10-12T10:00:00Z".into(),
            from: vec![],
            to: vec![],
            cc: vec![],
            subject: String::new(),
            preview: String::new(),
            size: 0,
            has_attachment: false,
            mailbox_ids: mailboxes.iter().map(|m| m.to_string()).collect(),
            keywords: vec![],
        }
    }

    /// Polls `fut` inside the root scope, as the scheduler does for a task, so
    /// that `document::eval` and friends find a scope.
    async fn in_root<T>(dom: &VirtualDom, fut: impl Future<Output = T>) -> T {
        let runtime = dom.runtime();
        let mut fut = std::pin::pin!(fut);

        std::future::poll_fn(|cx| runtime.in_scope(ScopeId::ROOT, || fut.as_mut().poll(cx))).await
    }

    #[test]
    fn replied_to_names_the_original_account() {
        let mut draft = compose::Draft {
            account_id: "work".into(),
            reply_to_email_id: Some("e1".into()),
            reply_to_account_id: Some("default".into()),
            ..Default::default()
        };

        assert_eq!(replied_to(&draft), Some(MsgRef::new("default", "e1")));

        draft.reply_to_account_id = None;

        assert_eq!(replied_to(&draft), Some(MsgRef::new("work", "e1")));

        draft.reply_to_email_id = None;

        assert_eq!(replied_to(&draft), None);
    }

    #[tokio::test]
    async fn finish_send_archives_after_a_failed_mark_and_reports_it() {
        let calls = RefCell::new(Vec::new());
        let act = async |r: &MsgRef, action: actions::Action| {
            calls.borrow_mut().push((r.clone(), action.clone()));

            match action {
                actions::Action::MarkAnswered => Err(Error::Other("x".into())),
                _ => Ok(()),
            }
        };
        let original = MsgRef::new("default", "e1");

        let toast = finish_send(Some(original.clone()), true, true, act).await;

        assert_eq!(toast, "Sent, but marking the original answered failed");
        assert_eq!(
            *calls.borrow(),
            [
                (original.clone(), actions::Action::MarkAnswered),
                (original.clone(), actions::Action::Archive),
            ]
        );

        calls.borrow_mut().clear();

        let toast = finish_send(Some(original.clone()), false, false, act).await;

        assert_eq!(toast, "Sent");
        assert!(calls.borrow().is_empty());

        let ok = async |r: &MsgRef, action: actions::Action| {
            calls.borrow_mut().push((r.clone(), action));
            Ok(())
        };
        let toast = finish_send(Some(original.clone()), true, false, ok).await;

        assert_eq!(toast, "Sent");
        assert_eq!(
            *calls.borrow(),
            [(original.clone(), actions::Action::MarkAnswered)]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_draft_acts_on_the_original_in_its_own_account() {
        let mut cfg = Config::default();

        cfg.add_account(Account::new("work", "w@x.io")).unwrap();

        let mut ui = backend_with(cfg);

        ui.ui_test = true;

        let dom = rendered(ui);
        let _guard = RuntimeGuard::new(dom.runtime());
        let store = backend().peek().store.clone();
        let mailbox = |account: &str, id: &str| Mailbox {
            account_id: account.into(),
            id: id.into(),
            name: id.into(),
            role: Some(id.into()),
            parent_id: None,
            sort_order: 0,
            total: 0,
            unread: 0,
        };

        for account in ["default", "work"] {
            store
                .upsert_mailboxes(&[mailbox(account, "inbox"), mailbox(account, "archive")])
                .unwrap();

            for id in ["e1", "e2"] {
                store
                    .upsert_emails(&[labelled_in(account, id, &["inbox"])])
                    .unwrap();
            }
        }

        let to = "a@b.io";
        let reply_to = |id: &str| compose::Draft {
            account_id: "work".into(),
            reply_to_email_id: Some(id.into()),
            reply_to_account_id: Some("default".into()),
            to: vec![Address {
                name: None,
                email: to.into(),
            }],
            ..Default::default()
        };
        let email = |account: &str, id: &str| store.email(account, id).unwrap().unwrap();
        let answered = |e: &EmailSummary| e.keywords.contains(&"$answered".to_string());

        *COMPOSER.write() = Some(reply_to("e1"));
        *COMPOSER_TO.write() = to.into();
        in_root(&dom, send_draft(false)).await;

        assert!(answered(&email("default", "e1")));
        assert!(!answered(&email("work", "e1")));
        assert!(COMPOSER.peek().is_none());
        assert_eq!(email("default", "e1").mailbox_ids, ["inbox"]);

        *COMPOSER.write() = Some(reply_to("e2"));
        *COMPOSER_TO.write() = to.into();
        in_root(&dom, send_draft(true)).await;

        assert!(COMPOSER.peek().is_none());
        assert!(answered(&email("default", "e2")));
        assert_eq!(email("default", "e2").mailbox_ids, ["archive"]);
        assert!(!answered(&email("work", "e2")));
        assert_eq!(email("work", "e2").mailbox_ids, ["inbox"]);
    }

    /// Group members `(account, mailbox)` for tests.
    fn members(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(a, m)| (a.to_string(), m.to_string()))
            .collect()
    }

    #[test]
    fn label_state_and_removable() {
        let (e1, e2, e3) = (
            labelled("e1", &["inbox", "promos"]),
            labelled("e2", &["promos"]),
            labelled("e3", &["inbox"]),
        );
        let promos = members(&[("default", "promos")]);
        let inbox = members(&[("default", "inbox")]);

        assert_eq!(label_state(&[&e1, &e2], &promos), LabelState::All);
        assert_eq!(label_state(&[&e1, &e3], &promos), LabelState::Some);
        assert_eq!(label_state(&[&e3], &promos), LabelState::None);
        assert_eq!(label_state(&[], &promos), LabelState::None);
        assert!(!label_removable(&[&e1, &e2], &promos));
        assert!(!label_removable(&[&e1, &e3], &inbox));
        assert!(label_removable(&[&e1], &promos));
    }

    #[test]
    fn label_state_intent_and_removable_use_each_accounts_members() {
        let group = members(&[("a", "a-l1"), ("a", "a-l2"), ("b", "b-l")]);
        let a1 = labelled_in("a", "1", &["a-l2", "a-in"]);
        let b2 = labelled_in("b", "2", &["b-l"]);
        let b3 = labelled_in("b", "3", &["b-in"]);
        let c4 = labelled_in("c", "4", &["b-l"]);

        assert_eq!(label_state(&[&a1, &b2], &group), LabelState::All);
        assert_eq!(label_state(&[&a1, &b2, &b3], &group), LabelState::Some);
        assert_eq!(label_state(&[&c4], &group), LabelState::None);
        assert_eq!(label_intent(&[&a1, &b2, &b3], &group), Some(true));
        assert_eq!(label_intent(&[&a1, &b2], &group), Some(false));
        assert_eq!(label_intent(&[&a1, &c4], &group), Some(false));
        assert_eq!(label_intent(&[&c4], &group), None);
        assert!(!label_removable(&[&a1, &b2], &group));
        assert!(label_removable(&[&a1], &group));
        assert!(label_removable(&[&b3], &group));
    }

    #[test]
    fn member_targets_split_by_account() {
        let group = members(&[("a", "a-r1"), ("a", "a-r2"), ("b", "b-r")]);
        let refs = [
            MsgRef::new("c", "1"),
            MsgRef::new("b", "2"),
            MsgRef::new("a", "3"),
            MsgRef::new("a", "4"),
            MsgRef::new("d", "5"),
        ];
        let (targets, missing) = member_targets(&group, &refs, &ids(&["a", "b", "d", "c"]));

        assert_eq!(
            targets,
            [
                ("b".to_string(), "b-r".to_string(), ids(&["2"])),
                ("a".to_string(), "a-r1".to_string(), ids(&["3", "4"])),
            ]
        );
        assert_eq!(missing, ids(&["d", "c"]));
    }

    #[test]
    fn action_toast_covers_partial_missing_and_failed() {
        let refused = "not available for this account: moving to a label on Proton accounts";
        let names = |list: &[&str]| ids(list);
        let fail = |account: &str, text: &str| vec![(account.to_string(), text.to_string())];

        assert_eq!(
            action_toast("Moved", 5, 5, 5, &[], &[], "Receipts"),
            "Moved 5 messages"
        );
        assert_eq!(
            action_toast("Moved", 1, 1, 1, &[], &[], "X"),
            "Moved 1 message"
        );
        assert_eq!(
            action_toast("Moved", 3, 5, 5, &[], &[], "R"),
            "Moved 3 of 5 messages"
        );
        assert_eq!(
            action_toast("Moved", 0, 5, 5, &[], &[], "R"),
            "Moved 0 of 5 messages"
        );
        assert_eq!(
            action_toast("Moved", 3, 5, 3, &names(&["Work"]), &[], "Receipts"),
            "Moved 3 of 5 messages; no \"Receipts\" in Work"
        );
        assert_eq!(
            action_toast(
                "Labelled",
                0,
                2,
                0,
                &names(&["Work", "Proton"]),
                &[],
                "Receipts"
            ),
            "No \"Receipts\" in Work, Proton"
        );
        assert_eq!(
            action_toast("Moved", 0, 2, 0, &names(&["Work"]), &[], "Receipts"),
            "No \"Receipts\" in Work"
        );
        assert_eq!(
            action_toast("Moved", 0, 2, 1, &names(&["Work"]), &[], "Receipts"),
            "Moved 0 of 2 messages; no \"Receipts\" in Work"
        );
        assert_eq!(
            action_toast("Moved", 1, 2, 2, &[], &fail("Proton", refused), "Receipts"),
            format!("Moved 1 of 2 messages; Proton: {refused}")
        );
        assert_eq!(
            action_toast(
                "Moved",
                0,
                3,
                2,
                &names(&["Home"]),
                &fail("Proton", "boom"),
                "R"
            ),
            "Moved 0 of 3 messages; no \"R\" in Home; Proton: boom"
        );
    }

    #[test]
    fn sidebar_rows_list_groups_then_expanded_members() {
        let boxed = |account: &str,
                     id: &str,
                     name: &str,
                     role: Option<&str>,
                     parent: Option<&str>,
                     order,
                     unread| Mailbox {
            name: name.into(),
            parent_id: parent.map(String::from),
            sort_order: order,
            unread,
            ..mailbox_of(account, id, role)
        };
        let boxes = [
            boxed("a", "a-in", "Inbox", Some("inbox"), None, 0, 2),
            boxed("b", "b-in", "Inbox", Some("inbox"), None, 0, 3),
            boxed("a", "a-r1", "Receipts", None, None, 0, 0),
            boxed("a", "a-r2", "Receipts", None, Some("a-in"), 1, 0),
            boxed("b", "b-r", "Receipts", None, None, 0, 0),
        ];
        let accounts = [
            ("a".to_string(), "Home".to_string()),
            ("b".to_string(), "Work".to_string()),
        ];
        let groups = mailbox_groups(&boxes, &ids(&["a", "b"]));
        let rows = sidebar_rows(&groups, &BTreeSet::new(), &accounts, &boxes);

        assert_eq!(
            rows,
            [
                SidebarRow::Group {
                    key: "role:inbox".into(),
                    name: "Inbox".into(),
                    role: Some("inbox".into()),
                    unread: 5,
                    expanded: false,
                },
                SidebarRow::Group {
                    key: "name:receipts".into(),
                    name: "Receipts".into(),
                    role: None,
                    unread: 0,
                    expanded: false,
                },
            ]
        );

        let open = BTreeSet::from(["name:receipts".to_string()]);
        let rows = sidebar_rows(&groups, &open, &accounts, &boxes);
        let member = |account: &str, id: &str, label: &str| SidebarRow::Member {
            folder: Folder::Mailbox {
                account_id: account.into(),
                id: id.into(),
            },
            label: label.into(),
            unread: 0,
        };

        assert_eq!(rows.len(), 5);
        assert!(
            matches!(&rows[1], SidebarRow::Group { key, expanded: true, .. } if key == "name:receipts")
        );
        assert_eq!(
            rows[2..],
            [
                member("a", "a-r1", "Home › Receipts"),
                member("a", "a-r2", "Home › Inbox / Receipts"),
                member("b", "b-r", "Work"),
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn label_group_refreshes_rows_and_closes_when_selection_leaves() {
        let mut ui = backend_with(Config::default());

        ui.ui_test = true;

        let dom = rendered(ui);
        let _guard = RuntimeGuard::new(dom.runtime());
        let store = backend().peek().store.clone();
        let mailbox = |id: &str, role: Option<&str>| Mailbox {
            account_id: "default".into(),
            id: id.into(),
            name: id.into(),
            role: role.map(String::from),
            parent_id: None,
            sort_order: 0,
            total: 0,
            unread: 0,
        };

        store
            .upsert_mailboxes(&[mailbox("inbox", Some("inbox")), mailbox("promos", None)])
            .unwrap();
        store
            .upsert_emails(&[labelled("e1", &["inbox"]), labelled("e2", &["inbox"])])
            .unwrap();
        reload_mailboxes();
        *SELECTED_MAILBOX.write() = Some(Folder::Mailbox {
            account_id: "default".into(),
            id: "inbox".into(),
        });
        reload_list().await;

        let order = list_order();

        assert_eq!(order.len(), 2);

        *SELECTION.write() = Selection {
            anchor: order.first().cloned(),
            focus: order.first().cloned(),
            ids: order,
        };
        *SHOW_LABEL_PICKER.write() = true;

        let state_of = |mailbox: &str| {
            let (emails, selection) = (EMAILS.peek(), SELECTION.peek());

            label_state(
                &selected_emails(&emails, &selection),
                &members(&[("default", mailbox)]),
            )
        };

        in_root(&dom, label_group("name:promos".into())).await;

        for e in EMAILS.peek().iter() {
            assert_eq!(e.mailbox_ids, ["inbox", "promos"], "{}", e.id);
        }

        assert_eq!(state_of("promos"), LabelState::All);
        assert!(*SHOW_LABEL_PICKER.peek());

        in_root(&dom, label_group("role:inbox".into())).await;

        assert!(EMAILS.peek().is_empty(), "{:?}", EMAILS.peek());
        assert!(selected_refs().is_empty());
        assert!(!*SHOW_LABEL_PICKER.peek());

        // An action pending: the activation is ignored.
        *SHOW_LABEL_PICKER.write() = true;
        *SELECTED_MAILBOX.write() = Some(Folder::Mailbox {
            account_id: "default".into(),
            id: "promos".into(),
        });
        reload_list().await;

        let order = list_order();

        *SELECTION.write() = Selection {
            anchor: order.first().cloned(),
            focus: order.first().cloned(),
            ids: order,
        };

        let busy = Flag::take(&BUSY).unwrap();

        in_root(&dom, label_group("role:inbox".into())).await;

        drop(busy);

        for e in EMAILS.peek().iter() {
            assert_eq!(e.mailbox_ids, ["promos"], "{}", e.id);
        }

        assert_eq!(SELECTION.peek().ids.len(), 2);
        assert!(*SHOW_LABEL_PICKER.peek());
    }

    /// A rendered UI-test backend with accounts `default` (Home) and `work` (Work).
    fn two_accounts() -> (VirtualDom, Arc<posto_core::store::Store>) {
        let mut cfg = Config::default();

        cfg.accounts[0].name = "Home".into();
        cfg.add_account(Account::new("work", "w@x.io")).unwrap();
        cfg.accounts[1].name = "Work".into();

        let mut ui = backend_with(cfg);

        ui.ui_test = true;

        let dom = rendered(ui);
        let store = dom.in_scope(ScopeId::ROOT, || backend().peek().store.clone());

        (dom, store)
    }

    /// A named mailbox of `account` for group tests.
    fn named_box(
        account: &str,
        id: &str,
        name: &str,
        role: Option<&str>,
        parent: Option<&str>,
    ) -> Mailbox {
        Mailbox {
            name: name.into(),
            parent_id: parent.map(String::from),
            ..mailbox_of(account, id, role)
        }
    }

    /// Opens the merged view `key`, loads it and selects every row (in `reverse` order if asked).
    async fn select_merged(key: &str, reverse: bool) {
        reload_mailboxes();
        *SELECTED_MAILBOX.write() = Some(Folder::Merged { key: key.into() });
        reload_list().await;

        let mut order = list_order();

        if reverse {
            order.reverse();
        }

        *SELECTION.write() = Selection {
            anchor: order.first().cloned(),
            focus: order.first().cloned(),
            ids: order,
        };
    }

    #[tokio::test(flavor = "current_thread")]
    async fn move_from_merged_view_attempts_every_account_and_settles() {
        let (dom, store) = two_accounts();
        let _guard = RuntimeGuard::new(dom.runtime());

        store
            .upsert_mailboxes(&[
                named_box("default", "inbox", "Inbox", Some("inbox"), None),
                named_box("default", "receipts", "Receipts", None, None),
                named_box("work", "inbox", "Inbox", Some("inbox"), None),
            ])
            .unwrap();
        store
            .upsert_emails(&[
                labelled_in("default", "e1", &["inbox"]),
                labelled_in("work", "e1", &["inbox"]),
            ])
            .unwrap();
        select_merged("role:inbox", false).await;

        assert_eq!(selected_refs().len(), 2);

        in_root(&dom, move_to_group("name:receipts".into())).await;

        let email = |account: &str| store.email(account, "e1").unwrap().unwrap();

        assert_eq!(email("default").mailbox_ids, ["receipts"]);
        assert_eq!(email("work").mailbox_ids, ["inbox"]);
        assert_eq!(
            *STATUS.peek(),
            "Moved 1 of 2 messages; no \"Receipts\" in Work"
        );

        let listed = list_order();

        assert_eq!(listed, ["work/e1"]);
        assert!(!SELECTION.peek().is_selected("default/e1"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn move_from_merged_view_reports_a_refused_account_in_either_order() {
        let (dom, store) = two_accounts();
        let _guard = RuntimeGuard::new(dom.runtime());

        for account in ["default", "work"] {
            store
                .upsert_mailboxes(&[
                    named_box(account, "inbox", "Inbox", Some("inbox"), None),
                    named_box(account, "receipts", "Receipts", None, None),
                ])
                .unwrap();
            store
                .upsert_emails(&[labelled_in(account, "e1", &["inbox"])])
                .unwrap();
        }

        store
            .set_account_state("work", "provider", "proton")
            .unwrap();

        for reverse in [false, true] {
            store
                .upsert_emails(&[labelled_in("default", "e1", &["inbox"])])
                .unwrap();
            STATUS.write().clear();
            select_merged("role:inbox", reverse).await;

            let first = if reverse { "work/e1" } else { "default/e1" };

            assert_eq!(SELECTION.peek().ids[0], first);

            in_root(&dom, move_to_group("name:receipts".into())).await;

            let email = |account: &str| store.email(account, "e1").unwrap().unwrap();

            assert_eq!(email("default").mailbox_ids, ["receipts"], "{reverse}");
            assert_eq!(email("work").mailbox_ids, ["inbox"], "{reverse}");
            assert_eq!(
                *STATUS.peek(),
                "Moved 1 of 2 messages; Work: not available for this account: moving to a label on Proton accounts",
                "{reverse}"
            );

            let listed = list_order();

            assert_eq!(listed, ["work/e1"], "{reverse}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn label_from_merged_view_removes_every_member_and_names_missing_accounts() {
        let (dom, store) = two_accounts();
        let _guard = RuntimeGuard::new(dom.runtime());

        store
            .upsert_mailboxes(&[
                named_box("default", "inbox", "Inbox", Some("inbox"), None),
                named_box("default", "rc1", "Receipts", None, None),
                named_box("default", "rc2", "Receipts", None, Some("inbox")),
                named_box("work", "inbox", "Inbox", Some("inbox"), None),
            ])
            .unwrap();
        store
            .upsert_emails(&[
                labelled_in("default", "e1", &["inbox", "rc1", "rc2"]),
                labelled_in("default", "e2", &["rc1"]),
                labelled_in("work", "e1", &["inbox"]),
            ])
            .unwrap();
        select_merged("role:inbox", false).await;
        *SHOW_LABEL_PICKER.write() = true;

        in_root(&dom, label_group("name:receipts".into())).await;

        let email = |account: &str, id: &str| store.email(account, id).unwrap().unwrap();

        assert_eq!(email("default", "e1").mailbox_ids, ["inbox"]);
        assert_eq!(email("work", "e1").mailbox_ids, ["inbox"]);
        assert_eq!(
            *STATUS.peek(),
            "Removed label from 1 of 2 messages; no \"Receipts\" in Work"
        );
        assert!(*SHOW_LABEL_PICKER.peek());

        *SELECTED_MAILBOX.write() = Some(Folder::Mailbox {
            account_id: "default".into(),
            id: "rc1".into(),
        });
        reload_list().await;

        let only = "default/e2".to_string();

        *SELECTION.write() = Selection {
            anchor: Some(only.clone()),
            focus: Some(only.clone()),
            ids: vec![only],
        };

        in_root(&dom, label_group("name:receipts".into())).await;

        assert_eq!(email("default", "e2").mailbox_ids, ["rc1"]);
        assert_eq!(*STATUS.peek(), "A message must keep at least one label.");
    }

    /// Queues the scopes that signal writes marked dirty, renders them and
    /// returns the edits.
    fn edits(dom: &mut VirtualDom) -> Vec<dioxus::core::Mutation> {
        dom.process_events();
        dom.render_immediate_to_vec().edits
    }

    #[tokio::test(flavor = "current_thread")]
    async fn merged_view_hides_the_classification_chip_of_the_open_group() {
        use dioxus::core::Mutation;

        let (mut dom, store) = two_accounts();
        let _guard = RuntimeGuard::new(dom.runtime());

        // Tray `receipts` is filed under a mailbox called Promos; the first
        // account spells it with spaces, which grouping ignores.
        dom.in_scope(ScopeId::ROOT, || {
            backend()
                .write()
                .cfg
                .classify
                .trays
                .insert("receipts".into(), "Promos".into());
        });
        store
            .upsert_mailboxes(&[
                named_box("default", "inbox", "Inbox", Some("inbox"), None),
                named_box("default", "promos", " Promos ", None, None),
                named_box("work", "inbox", "Inbox", Some("inbox"), None),
                named_box("work", "promos", "Promos", None, None),
            ])
            .unwrap();
        store
            .upsert_emails(&[labelled_in("work", "p1", &["promos"])])
            .unwrap();
        reload_mailboxes();
        *SELECTED_MAILBOX.write() = Some(Folder::Merged {
            key: "name:promos".into(),
        });
        reload_list().await;
        edits(&mut dom);

        assert_eq!(list_order(), ["work/p1"]);

        // Classified as `receipts`, which this config files under Promos.
        store
            .put_classification(&Classified {
                account_id: "work".into(),
                email_id: "p1".into(),
                tray: "receipts".into(),
                probs: Default::default(),
                urgency: 1,
                personal: 0.1,
                action: 0.0,
                unsure: false,
                model: "test".into(),
                input_tokens: 0,
                classified_at: "2026-10-12T10:00:00Z".into(),
                corrected_tray: None,
                applied: true,
                apply_error: None,
            })
            .unwrap();
        reload_list().await;

        assert!(CLASSIFIED.peek().contains_key("work/p1"));

        let reappeared = edits(&mut dom)
            .into_iter()
            .any(|m| matches!(m, Mutation::CreateTextNode { value, .. } if value == "Promos"));

        assert!(
            !reappeared,
            "the open group's name came back as a tray chip"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_label_row_reports_why_on_enter_and_click() {
        use dioxus::core::Mutation;
        use dioxus::html::{Code, Key, Location, Modifiers, PlatformEventData};
        use dioxus::html::{SerializedKeyboardData, SerializedMouseData};

        // The webview normally installs the converter that reads these events.
        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));

        let (mut dom, store) = two_accounts();
        let _guard = RuntimeGuard::new(dom.runtime());

        store
            .upsert_mailboxes(&[named_box("default", "receipts", "Receipts", None, None)])
            .unwrap();
        // default/e1 has only the label, so removing it is refused; work has no such mailbox.
        store
            .upsert_emails(&[
                labelled_in("default", "e1", &["receipts"]),
                labelled_in("work", "e1", &["inbox"]),
            ])
            .unwrap();
        select_merged("name:receipts", false).await;
        *EMAILS.write() = vec![
            store.email("default", "e1").unwrap().unwrap(),
            store.email("work", "e1").unwrap().unwrap(),
        ];
        *SELECTION.write() = Selection {
            anchor: Some("default/e1".into()),
            focus: Some("default/e1".into()),
            ids: vec!["default/e1".into(), "work/e1".into()],
        };
        edits(&mut dom);
        *SHOW_LABEL_PICKER.write() = true;

        let opened = edits(&mut dom);
        let listener = |name: &str| {
            opened
                .iter()
                .rev()
                .find_map(|m| match m {
                    Mutation::NewEventListener { name: n, id } if *n == name => Some(*id),
                    _ => None,
                })
                .unwrap()
        };
        let enter = || {
            Rc::new(PlatformEventData::new(Box::new(
                SerializedKeyboardData::new(
                    Key::Enter,
                    Code::Enter,
                    Location::Standard,
                    false,
                    Modifiers::empty(),
                    false,
                ),
            ))) as Rc<dyn std::any::Any>
        };
        let click = || {
            Rc::new(PlatformEventData::new(Box::new(
                SerializedMouseData::default(),
            ))) as Rc<dyn std::any::Any>
        };
        let toast = "A message must keep at least one label.";

        // Receipts is the only label group, so it is the active row; the filter box
        // is the only keydown listener the picker adds and the row the last click one.
        for (event, name, id) in [
            (enter(), "keydown", listener("keydown")),
            (click(), "click", listener("click")),
        ] {
            STATUS.write().clear();
            dom.runtime()
                .handle_event(name, dioxus::core::Event::new(event, true), id);

            // Tasks are polled once no scope is dirty.
            edits(&mut dom);
            dom.process_events();

            assert_eq!(*STATUS.peek(), toast, "{name}");
            assert_eq!(
                store.email("default", "e1").unwrap().unwrap().mailbox_ids,
                ["receipts"],
                "{name}"
            );
        }
    }

    /// The ids of the listed emails, in list order.
    fn listed_ids() -> Vec<String> {
        EMAILS.peek().iter().map(|e| e.id.clone()).collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn triage_folder_lists_owed_mail_oldest_first() {
        let mut ui = backend_with(Config::default());

        ui.ui_test = true;

        let dom = rendered(ui);
        let _guard = RuntimeGuard::new(dom.runtime());

        posto_core::fixtures::seed(&backend().peek().store).unwrap();
        reload_mailboxes();

        assert_eq!(*TRIAGE.peek(), 4);

        *SELECTED_MAILBOX.write() = Some(Folder::Triage);
        reload_list().await;

        assert_eq!(listed_ids(), ["fx-07", "fx-05", "fx-03", "fx-11"]);

        act_on_emails(&[MsgRef::new("default", "fx-07")], actions::Action::Archive)
            .await
            .unwrap();

        assert_eq!(listed_ids(), ["fx-05", "fx-03", "fx-11"]);
        assert_eq!(*TRIAGE.peek(), 3);

        act_on_emails(&[MsgRef::new("default", "fx-03")], actions::Action::Unflag)
            .await
            .unwrap();

        assert_eq!(listed_ids(), ["fx-05", "fx-11"]);
        assert_eq!(*TRIAGE.peek(), 2);

        for id in ["fx-05", "fx-11"] {
            act_on_emails(&[MsgRef::new("default", id)], actions::Action::Archive)
                .await
                .unwrap();
        }

        assert!(EMAILS.peek().is_empty());
        assert_eq!(*TRIAGE.peek(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn triage_row_is_always_shown_and_titles_the_list() {
        use dioxus::core::Mutation;

        let mut dom = VirtualDom::new(App).with_root_context(backend_with(Config::default()));
        let first = dom.rebuild_to_vec();
        let _guard = RuntimeGuard::new(dom.runtime());

        assert!(
            first
                .edits
                .iter()
                .any(|m| matches!(m, Mutation::CreateTextNode { value, .. } if value == "Triage"))
        );
        assert_eq!(*TRIAGE.peek(), 0);

        posto_core::fixtures::seed(&backend().peek().store).unwrap();
        reload_mailboxes();
        *SELECTED_MAILBOX.write() = Some(Folder::Triage);
        reload_list().await;

        assert!(edits(&mut dom).iter().any(|m| matches!(
            m,
            Mutation::CreateTextNode { value, .. } | Mutation::SetText { value, .. }
                if value == "4 to deal with"
        )));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn triage_drops_a_replied_original_after_send() {
        let mut ui = backend_with(Config::default());

        ui.ui_test = true;

        let dom = rendered(ui);
        let _guard = RuntimeGuard::new(dom.runtime());
        let store = backend().peek().store.clone();

        posto_core::fixtures::seed(&store).unwrap();
        reload_mailboxes();
        *SELECTED_MAILBOX.write() = Some(Folder::Triage);
        reload_list().await;

        assert_eq!(listed_ids(), ["fx-07", "fx-05", "fx-03", "fx-11"]);

        // UI test mode skips `compose::send`, which marks the original answered.
        store
            .set_keywords("default", "fx-05", &["$seen".into(), "$answered".into()])
            .unwrap();

        assert_eq!(listed_ids(), ["fx-07", "fx-05", "fx-03", "fx-11"]);

        let to = "a@b.io";

        *COMPOSER.write() = Some(compose::Draft {
            account_id: "default".into(),
            reply_to_email_id: Some("fx-05".into()),
            reply_to_account_id: Some("default".into()),
            to: vec![Address {
                name: None,
                email: to.into(),
            }],
            ..Default::default()
        });
        *COMPOSER_TO.write() = to.into();
        in_root(&dom, send_draft(false)).await;

        assert_eq!(listed_ids(), ["fx-07", "fx-03", "fx-11"]);
        assert_eq!(*TRIAGE.peek(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn triage_tags_accounts_with_several_accounts() {
        use dioxus::core::Mutation;

        let (mut dom, store) = two_accounts();
        let _guard = RuntimeGuard::new(dom.runtime());
        let flagged = |account: &str, id: &str, received_at: &str| EmailSummary {
            received_at: received_at.into(),
            keywords: vec!["$flagged".into()],
            ..labelled_in(account, id, &["inbox"])
        };

        store
            .upsert_mailboxes(&[
                named_box("default", "inbox", "Inbox", Some("inbox"), None),
                named_box("work", "inbox", "Inbox", Some("inbox"), None),
            ])
            .unwrap();
        store
            .upsert_emails(&[
                flagged("default", "e1", "2026-10-12T10:00:00Z"),
                flagged("work", "e2", "2026-10-11T10:00:00Z"),
            ])
            .unwrap();
        reload_mailboxes();
        *SELECTED_MAILBOX.write() = Some(Folder::Triage);
        reload_list().await;

        assert_eq!(list_order(), ["work/e2", "default/e1"]);
        assert_eq!(*TRIAGE.peek(), 2);
        assert!(
            edits(&mut dom)
                .iter()
                .any(|m| matches!(m, Mutation::CreateTextNode { value, .. } if value == "Work"))
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
