//! Keyboard shortcut map.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Everything a key chord can trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Next,
    Prev,
    Open,
    Back,
    Archive,
    Delete,
    Reply,
    ReplyAll,
    Compose,
    ToggleRead,
    ToggleFlag,
    Move,
    Search,
    Sync,
    Junk,
    Send,
    Escape,
    ExtendNext,
    ExtendPrev,
    SelectAll,
    /// Deletes the selection without asking; what the Delete confirmation runs.
    DeleteConfirmed,
}

/// A key plus modifiers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    /// Lower-case key name as the browser gives it: "j", "enter", "escape", "/", "#".
    pub key: String,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.meta, "Meta"),
        ] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        match self.key.as_str() {
            "escape" => f.write_str("Esc"),
            "delete" => f.write_str("Del"),

            key => {
                let mut chars = key.chars();
                if let Some(first) = chars.next() {
                    write!(f, "{}{}", first.to_uppercase(), chars.as_str())?;
                }
                Ok(())
            }
        }
    }
}

/// Chord to action bindings, in config order (an action may have several chords).
pub struct Keymap {
    bindings: Vec<(Chord, Action)>,
}

/// Default bindings, as comma-separated chord strings.
const DEFAULTS: &[(Action, &str)] = &[
    (Action::Next, "j"),
    (Action::Prev, "k"),
    (Action::Open, "enter"),
    (Action::Escape, "escape"),
    (Action::Back, "backspace"),
    (Action::Archive, "e"),
    (Action::Delete, "#, delete"),
    (Action::Reply, "r"),
    (Action::ReplyAll, "a"),
    (Action::Compose, "c"),
    (Action::ToggleRead, "u"),
    (Action::ToggleFlag, "s"),
    (Action::Move, "m"),
    (Action::Search, "/"),
    (Action::Sync, "g"),
    (Action::Junk, "!"),
    (Action::Send, "ctrl+enter"),
    (Action::ExtendNext, "shift+j"),
    (Action::ExtendPrev, "shift+k"),
    (Action::SelectAll, "ctrl+a"),
    (Action::DeleteConfirmed, "ctrl+shift+delete"),
];

impl Keymap {
    /// The built-in bindings.
    pub fn defaults() -> Keymap {
        let mut map = Keymap {
            bindings: Vec::new(),
        };

        for (action, chords) in DEFAULTS {
            map.bind(
                *action,
                Keymap::parse_chords(chords).expect("default chords are valid"),
            );
        }

        map
    }

    /// Defaults with each `action name -> chords` override replacing that
    /// action's bindings; chords are comma-separated. A chord already bound to
    /// another action is taken over.
    pub fn from_config(overrides: &BTreeMap<String, String>) -> Result<Keymap> {
        let mut map = Keymap::defaults();
        for (name, chords) in overrides {
            let action: Action = serde_json::from_value(serde_json::json!(name))
                .map_err(|_| Error::Config(format!("unknown action {name}")))?;
            map.bind(action, Keymap::parse_chords(chords)?);
        }
        Ok(map)
    }

    /// Replaces `action`'s chords with `chords`, unbinding them from any other action.
    fn bind(&mut self, action: Action, chords: Vec<Chord>) {
        self.bindings
            .retain(|(c, bound)| *bound != action && !chords.contains(c));
        self.bindings
            .extend(chords.into_iter().map(|c| (c, action)));
    }

    /// Parses a comma-separated list such as `"#, delete"`.
    pub fn parse_chords(s: &str) -> Result<Vec<Chord>> {
        s.split(',').map(Keymap::parse_chord).collect()
    }

    /// Parses `"ctrl+shift+k"`, `"#"`, `"enter"`; modifiers in any order.
    pub fn parse_chord(s: &str) -> Result<Chord> {
        let mut chord = Chord {
            key: String::new(),
            ctrl: false,
            shift: false,
            alt: false,
            meta: false,
        };
        let mut keys = 0;
        for token in s.split('+').map(|t| t.trim().to_lowercase()) {
            match token.as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "shift" => chord.shift = true,
                "alt" => chord.alt = true,
                "meta" | "cmd" | "super" => chord.meta = true,
                _ => {
                    keys += 1;
                    chord.key = token;
                }
            }
        }
        if keys != 1 || chord.key.is_empty() {
            return Err(Error::Config(format!(
                "chord {s:?} needs exactly one non-empty key, found {keys}"
            )));
        }
        Ok(chord)
    }

    /// The action bound to `c`, if any.
    pub fn lookup(&self, c: &Chord) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(chord, _)| chord == c)
            .map(|(_, a)| *a)
    }

    /// The first chord bound to `a`, if any.
    pub fn chord_for(&self, a: Action) -> Option<Chord> {
        self.chords_for(a).into_iter().next()
    }

    /// Every chord bound to `a`, in config order.
    pub fn chords_for(&self, a: Action) -> Vec<Chord> {
        self.bindings
            .iter()
            .filter(|(_, bound)| *bound == a)
            .map(|(chord, _)| chord.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(s: &str) -> Chord {
        Keymap::parse_chord(s).unwrap()
    }

    #[test]
    fn defaults_cover_all_actions() {
        let map = Keymap::defaults();
        // Exhaustive match: adding an Action without listing it here fails to compile.
        let all = [
            Action::Next,
            Action::Prev,
            Action::Open,
            Action::Back,
            Action::Archive,
            Action::Delete,
            Action::Reply,
            Action::ReplyAll,
            Action::Compose,
            Action::ToggleRead,
            Action::ToggleFlag,
            Action::Move,
            Action::Search,
            Action::Sync,
            Action::Junk,
            Action::Send,
            Action::Escape,
            Action::ExtendNext,
            Action::ExtendPrev,
            Action::SelectAll,
            Action::DeleteConfirmed,
        ];
        for action in all {
            match action {
                Action::Next
                | Action::Prev
                | Action::Open
                | Action::Back
                | Action::Archive
                | Action::Delete
                | Action::Reply
                | Action::ReplyAll
                | Action::Compose
                | Action::ToggleRead
                | Action::ToggleFlag
                | Action::Move
                | Action::Search
                | Action::Sync
                | Action::Junk
                | Action::Send
                | Action::Escape
                | Action::ExtendNext
                | Action::ExtendPrev
                | Action::SelectAll
                | Action::DeleteConfirmed => {}
            }
            assert!(map.chord_for(action).is_some(), "{action:?} unbound");
        }
        assert_eq!(map.chord_for(Action::Back), Some(chord("backspace")));
        assert_eq!(map.chord_for(Action::Escape), Some(chord("escape")));
        assert_eq!(map.lookup(&chord("e")), Some(Action::Archive));
        assert_eq!(map.lookup(&chord("shift+j")), Some(Action::ExtendNext));
        assert_eq!(map.lookup(&chord("shift+k")), Some(Action::ExtendPrev));
        assert_eq!(map.lookup(&chord("ctrl+a")), Some(Action::SelectAll));
        assert_eq!(map.lookup(&chord("#")), Some(Action::Delete));
        assert_eq!(map.lookup(&chord("delete")), Some(Action::Delete));
        assert_eq!(map.lookup(&chord("backspace")), Some(Action::Back));
    }

    #[test]
    fn multiple_chords() {
        let over =
            |name: &str, chord: &str| BTreeMap::from([(name.to_string(), chord.to_string())]);

        let map = Keymap::from_config(&over("delete", "#, delete")).unwrap();
        assert_eq!(map.lookup(&chord("#")), Some(Action::Delete));
        assert_eq!(map.lookup(&chord("delete")), Some(Action::Delete));
        assert_eq!(map.chords_for(Action::Delete).len(), 2);
        assert_eq!(map.chord_for(Action::Delete), Some(chord("#")));

        let map = Keymap::from_config(&over("archive", "e, y")).unwrap();
        assert_eq!(map.lookup(&chord("y")), Some(Action::Archive));
        assert_eq!(map.lookup(&chord("#")), Some(Action::Delete));

        assert!(matches!(
            Keymap::from_config(&over("archive", "e, ctrl+")),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn parse_chord_forms() {
        assert_eq!(chord("Ctrl+Shift+k"), chord("shift+ctrl+K"));
        assert!(chord("Ctrl+Shift+k").ctrl && chord("Ctrl+Shift+k").shift);
        assert_eq!(chord("#").key, "#");
        assert!(Keymap::parse_chord("").is_err());
        assert!(Keymap::parse_chord("ctrl+a+b").is_err());
        assert!(Keymap::parse_chord("ctrl").is_err());

        for bad in ["ctrl++a", "ctrl+a+", "+a", "ctrl+", "+", "ctrl+ +a"] {
            assert!(
                matches!(Keymap::parse_chord(bad), Err(Error::Config(_))),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn overrides_replace_binding() {
        let over =
            |name: &str, chord: &str| BTreeMap::from([(name.to_string(), chord.to_string())]);

        let map = Keymap::from_config(&over("archive", "y")).unwrap();
        assert_eq!(map.lookup(&chord("e")), None);
        assert_eq!(map.lookup(&chord("y")), Some(Action::Archive));

        assert!(matches!(
            Keymap::from_config(&over("bogus", "x")),
            Err(Error::Config(_))
        ));

        let map = Keymap::from_config(&over("compose", "e")).unwrap();
        assert_eq!(map.lookup(&chord("e")), Some(Action::Compose));
        assert_eq!(map.chord_for(Action::Archive), None);
    }

    #[test]
    fn chord_display() {
        assert_eq!(chord("ctrl+shift+k").to_string(), "Ctrl+Shift+K");
        assert_eq!(chord("escape").to_string(), "Esc");
        assert_eq!(chord("enter").to_string(), "Enter");
        assert_eq!(chord("#").to_string(), "#");
        assert_eq!(chord("delete").to_string(), "Del");
    }
}
