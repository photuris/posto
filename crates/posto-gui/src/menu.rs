//! Native application menu (macOS and Windows) whose items drive the same actions as the
//! keyboard shortcuts.
//!
//! Double dispatch: with the webview focused, muda handles a menu accelerator and the DOM may
//! also see the same keydown. Only chords that include ctrl, alt or meta get an accelerator;
//! bare-letter and shift-only shortcuts (j, k, e, c, `#`, ...) stay DOM-only and their menu items
//! show no accelerator.
//!
//! The menu is built once at start-up from the loaded keymap; changing shortcuts in Settings
//! takes effect in the menu after a restart.

use std::collections::HashMap;
use std::sync::OnceLock;

use dioxus::desktop::muda::accelerator::{Accelerator, Code, Modifiers};
use dioxus::desktop::muda::{
    AboutMetadata, IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use posto_core::shortcuts::{Action, Chord, Keymap};

/// What a menu item does when chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Shortcut(Action),
    Settings,
    About,
}

static ACTIONS: OnceLock<HashMap<MenuId, MenuAction>> = OnceLock::new();

/// The item id to action table of the installed menu; empty when no menu was installed.
pub fn actions() -> &'static HashMap<MenuId, MenuAction> {
    ACTIONS.get_or_init(HashMap::new)
}

/// Builds the menu for `keymap` and publishes its action table for [`actions`].
pub fn install(keymap: &Keymap) -> Menu {
    let (menu, map) = build(keymap);
    let _ = ACTIONS.set(map);
    menu
}

/// The accelerator for `chord` on the running platform, if it has one.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", test)),
    allow(dead_code)
)]
pub fn accelerator_for(chord: &Chord) -> Option<Accelerator> {
    accelerator_for_platform(chord, cfg!(target_os = "macos"))
}

/// `accelerator_for` with the platform made explicit. On macOS `ctrl` means the Command key.
/// Chords without any modifier, or with a key that has no `Code`, give `None`.
pub fn accelerator_for_platform(chord: &Chord, macos: bool) -> Option<Accelerator> {
    let mut mods = Modifiers::empty();
    if chord.ctrl {
        mods |= if macos {
            Modifiers::SUPER
        } else {
            Modifiers::CONTROL
        };
    }
    if chord.meta {
        mods |= Modifiers::SUPER;
    }
    if chord.shift {
        mods |= Modifiers::SHIFT;
    }
    if chord.alt {
        mods |= Modifiers::ALT;
    }

    let code = match chord.key.as_str() {
        "enter" => Code::Enter,
        "escape" => Code::Escape,
        "backspace" => Code::Backspace,
        "/" => Code::Slash,
        "," => Code::Comma,
        "#" => {
            mods |= Modifiers::SHIFT;
            Code::Digit3
        }
        "!" => {
            mods |= Modifiers::SHIFT;
            Code::Digit1
        }
        key => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_lowercase() => {
                    format!("Key{}", c.to_ascii_uppercase()).parse().ok()?
                }
                (Some(c), None) if c.is_ascii_digit() => format!("Digit{c}").parse().ok()?,
                _ => return None,
            }
        }
    };

    // The chord as written must carry a modifier; "#" and "!" gain SHIFT above and must not count.
    if !(chord.ctrl || chord.shift || chord.alt || chord.meta) {
        return None;
    }
    Some(Accelerator::new(Some(mods), code))
}

/// Accelerator for a shortcut-backed item: only chords with ctrl, alt or meta (see module docs).
fn menu_accelerator(keymap: &Keymap, action: Action) -> Option<Accelerator> {
    let chord = keymap.chord_for(action)?;
    if !(chord.ctrl || chord.alt || chord.meta) {
        return None;
    }
    accelerator_for(&chord)
}

/// Accumulates items into submenus while recording what each one does.
struct Builder<'a> {
    keymap: &'a Keymap,
    actions: HashMap<MenuId, MenuAction>,
}

impl Builder<'_> {
    fn shortcut(&mut self, sub: &Submenu, label: &str, action: Action) {
        let item = MenuItem::new(label, true, menu_accelerator(self.keymap, action));
        self.actions
            .insert(item.id().clone(), MenuAction::Shortcut(action));
        append(sub, &item);
    }
}

fn append(sub: &Submenu, item: &dyn IsMenuItem) {
    sub.append(item)
        .expect("appending to a freshly built submenu cannot fail");
}

fn separator(sub: &Submenu) {
    append(sub, &PredefinedMenuItem::separator());
}

fn submenu(label: &str) -> Submenu {
    Submenu::new(label, true)
}

/// Builds the menu bar and the table mapping item ids to what they do.
pub fn build(keymap: &Keymap) -> (Menu, HashMap<MenuId, MenuAction>) {
    let menu = Menu::new();
    let mut b = Builder {
        keymap,
        actions: HashMap::new(),
    };

    if cfg!(target_os = "macos") {
        let app = submenu("Posto");
        let about = PredefinedMenuItem::about(
            Some("About Posto"),
            Some(AboutMetadata {
                name: Some("Posto".into()),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                ..Default::default()
            }),
        );
        b.actions.insert(about.id().clone(), MenuAction::About);
        append(&app, &about);
        separator(&app);
        let settings = MenuItem::new(
            "Settings…",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::Comma)),
        );
        b.actions
            .insert(settings.id().clone(), MenuAction::Settings);
        append(&app, &settings);
        separator(&app);
        append(&app, &PredefinedMenuItem::services(None));
        separator(&app);
        append(&app, &PredefinedMenuItem::hide(None));
        append(&app, &PredefinedMenuItem::hide_others(None));
        append(&app, &PredefinedMenuItem::show_all(None));
        separator(&app);
        append(&app, &PredefinedMenuItem::quit(None));
        append_submenu(&menu, &app);
    }

    let file = submenu("File");
    b.shortcut(&file, "New Message", Action::Compose);
    b.shortcut(&file, "Reply", Action::Reply);
    b.shortcut(&file, "Reply All", Action::ReplyAll);
    separator(&file);
    b.shortcut(&file, "Archive", Action::Archive);
    b.shortcut(&file, "Move to Junk", Action::Junk);
    b.shortcut(&file, "Delete", Action::Delete);
    separator(&file);
    b.shortcut(&file, "Mark Read/Unread", Action::ToggleRead);
    b.shortcut(&file, "Flag/Unflag", Action::ToggleFlag);
    b.shortcut(&file, "Move…", Action::Move);
    separator(&file);
    b.shortcut(&file, "Sync Now", Action::Sync);
    if cfg!(target_os = "windows") {
        separator(&file);
        append(&file, &PredefinedMenuItem::quit(None));
    }
    append_submenu(&menu, &file);

    let edit = submenu("Edit");
    append(&edit, &PredefinedMenuItem::undo(None));
    append(&edit, &PredefinedMenuItem::redo(None));
    separator(&edit);
    append(&edit, &PredefinedMenuItem::cut(None));
    append(&edit, &PredefinedMenuItem::copy(None));
    append(&edit, &PredefinedMenuItem::paste(None));
    append(&edit, &PredefinedMenuItem::select_all(None));
    append_submenu(&menu, &edit);

    let view = submenu("View");
    b.shortcut(&view, "Search", Action::Search);
    b.shortcut(&view, "Next Message", Action::Next);
    b.shortcut(&view, "Previous Message", Action::Prev);
    b.shortcut(&view, "Open", Action::Open);
    b.shortcut(&view, "Back", Action::Back);
    append_submenu(&menu, &view);

    if cfg!(target_os = "macos") {
        let window = submenu("Window");
        append(&window, &PredefinedMenuItem::minimize(None));
        append(&window, &PredefinedMenuItem::maximize(None));
        separator(&window);
        append(&window, &PredefinedMenuItem::close_window(None));
        append_submenu(&menu, &window);
    }

    (menu, b.actions)
}

fn append_submenu(menu: &Menu, sub: &Submenu) {
    menu.append(sub)
        .expect("appending a submenu to a fresh menu cannot fail");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(s: &str) -> Chord {
        Keymap::parse_chord(s).unwrap()
    }

    fn acc(mods: Modifiers, code: Code) -> Option<Accelerator> {
        Some(Accelerator::new(Some(mods), code))
    }

    #[test]
    fn accelerator_mapping() {
        let send = chord("ctrl+enter");
        assert_eq!(
            accelerator_for_platform(&send, true),
            acc(Modifiers::SUPER, Code::Enter)
        );
        assert_eq!(
            accelerator_for_platform(&send, false),
            acc(Modifiers::CONTROL, Code::Enter)
        );
        assert_eq!(
            accelerator_for_platform(&chord("shift+k"), false),
            acc(Modifiers::SHIFT, Code::KeyK)
        );
        assert_eq!(accelerator_for_platform(&chord("#"), false), None);
        assert_eq!(accelerator_for_platform(&chord("j"), true), None);
        assert_eq!(
            accelerator_for_platform(&chord("ctrl+/"), false),
            acc(Modifiers::CONTROL, Code::Slash)
        );
        assert_eq!(
            accelerator_for_platform(&chord("ctrl+/"), true),
            acc(Modifiers::SUPER, Code::Slash)
        );
        assert_eq!(accelerator_for_platform(&chord("ctrl+f13"), false), None);
    }

    #[test]
    fn build_covers_actions() {
        let (menu, map) = build(&Keymap::defaults());
        let values: Vec<_> = map.values().collect();
        for action in [
            Action::Compose,
            Action::Archive,
            Action::Sync,
            Action::Search,
        ] {
            assert!(
                values.contains(&&MenuAction::Shortcut(action)),
                "{action:?}"
            );
        }
        assert_eq!(
            values.contains(&&MenuAction::Settings),
            cfg!(target_os = "macos")
        );
        assert!(menu.items().len() >= 3);
    }
}
