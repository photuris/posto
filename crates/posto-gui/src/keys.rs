//! Keyboard handling: browser key names to [`Chord`]s, and shortcut text.

use posto_core::shortcuts::{Action, Chord, Keymap};

/// Every action, in the order Settings lists them.
pub const ALL_ACTIONS: [Action; 24] = [
    Action::Next,
    Action::Prev,
    Action::Open,
    Action::Back,
    Action::Escape,
    Action::Archive,
    Action::Delete,
    Action::Junk,
    Action::Reply,
    Action::ReplyAll,
    Action::Compose,
    Action::ToggleRead,
    Action::ToggleFlag,
    Action::Move,
    Action::Search,
    Action::Sync,
    Action::Send,
    Action::ExtendNext,
    Action::ExtendPrev,
    Action::SelectAll,
    Action::Classify,
    Action::Label,
    Action::DeleteConfirmed,
    Action::ToggleSidebar,
];

/// Normalises a Dioxus `Key` display string and modifier state.
///
/// Keys are lower-cased (`"Enter"` is `enter`, `"ArrowDown"` is
/// `arrowdown`, a space is `space`). A single non-letter character such as
/// `#` already carries its shift state, so `shift` is dropped for it.
pub fn chord_from_event(key: &str, ctrl: bool, shift: bool, alt: bool, meta: bool) -> Chord {
    let key = if key == " " {
        "space".to_string()
    } else {
        key.to_lowercase()
    };
    let mut chars = key.chars();
    let symbol = matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_alphabetic());

    Chord {
        key,
        ctrl,
        shift: shift && !symbol,
        alt,
        meta,
    }
}

/// The action for `chord`; a chord with Cmd held also matches its Ctrl binding,
/// since the shortcut map spells the platform modifier `ctrl`.
pub fn lookup_with_meta_fallback(keymap: &Keymap, chord: &Chord) -> Option<Action> {
    keymap.lookup(chord).or_else(|| {
        chord.meta.then(|| {
            keymap.lookup(&Chord {
                ctrl: true,
                meta: false,
                ..chord.clone()
            })
        })?
    })
}

/// True when a key press should be left to the focused text field.
pub fn should_ignore(target_is_text_input: bool, chord: &Chord) -> bool {
    target_is_text_input && chord.key != "escape" && !(chord.ctrl && chord.key == "enter")
}

/// The snake_case name of `action`, as used in `config.toml`.
pub fn action_name(action: Action) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// A readable label such as `Reply all`.
pub fn action_label(action: Action) -> String {
    let name = action_name(action).replace('_', " ");
    let mut chars = name.chars();

    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Chord text that `Keymap::parse_chord` reads back, e.g. `ctrl+enter`.
pub fn chord_text(c: &Chord) -> String {
    let mut parts: Vec<&str> = Vec::new();

    for (on, name) in [
        (c.ctrl, "ctrl"),
        (c.alt, "alt"),
        (c.shift, "shift"),
        (c.meta, "meta"),
    ] {
        if on {
            parts.push(name);
        }
    }

    parts.push(&c.key);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use posto_core::shortcuts::Keymap;

    #[test]
    fn chord_normalisation() {
        let enter = chord_from_event("Enter", true, false, false, false);
        assert_eq!((enter.key.as_str(), enter.ctrl), ("enter", true));

        let hash = chord_from_event("#", false, true, false, false);
        assert_eq!((hash.key.as_str(), hash.shift), ("#", false));

        let upper_j = chord_from_event("J", false, true, false, false);
        assert_eq!((upper_j.key.as_str(), upper_j.shift), ("j", true));

        let down = chord_from_event("ArrowDown", false, false, false, false);
        assert_eq!(down.key, "arrowdown");

        // The default Delete binding matches what a US keyboard sends for Shift+3.
        let delete = Keymap::defaults().lookup(&chord_from_event("#", false, true, false, false));
        assert_eq!(delete, Some(Action::Delete));
        assert_eq!(chord_text(&enter), "ctrl+enter");

        let del = chord_from_event("Delete", false, false, false, false);
        assert_eq!(del.key, "delete");
        let back = chord_from_event("Backspace", false, false, false, false);
        assert_eq!(back.key, "backspace");
    }

    #[test]
    fn meta_fallback() {
        let keymap = Keymap::defaults();
        let lookup = |key, ctrl, alt, meta| {
            lookup_with_meta_fallback(&keymap, &chord_from_event(key, ctrl, false, alt, meta))
        };

        assert_eq!(lookup("a", false, false, true), Some(Action::SelectAll));
        assert_eq!(lookup("a", true, false, false), Some(Action::SelectAll));
        assert_eq!(lookup("a", false, true, false), None);
        assert_eq!(lookup("a", false, false, false), Some(Action::ReplyAll));
    }

    #[test]
    fn ignore_in_text_inputs() {
        let j = chord_from_event("j", false, false, false, false);
        let escape = chord_from_event("Escape", false, false, false, false);
        let send = chord_from_event("Enter", true, false, false, false);

        assert!(should_ignore(true, &j));
        assert!(!should_ignore(true, &escape));
        assert!(!should_ignore(true, &send));

        for chord in [&j, &escape, &send] {
            assert!(!should_ignore(false, chord));
        }
    }
}
