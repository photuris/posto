//! Omarchy theme detection and change watching.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher as _};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// How long to wait for a burst of file events to settle.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Colour scheme read from an Omarchy `colors.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Theme {
    /// "dark" or "light".
    pub mode: String,
    /// Key to "#rrggbb".
    pub colors: BTreeMap<String, String>,
}

/// `$XDG_STATE_HOME` or `~/.local/state`, plus `omarchy/current/theme/colors.toml`.
pub fn omarchy_theme_path() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from(".local/state"))
        .join("omarchy/current/theme/colors.toml")
}

/// Parses the current Omarchy theme; `None` when absent or unparsable.
pub fn detect() -> Option<Theme> {
    detect_at(&omarchy_theme_path())
}

/// Parses the theme file at `path`; `None` when absent or unparsable.
pub fn detect_at(path: &Path) -> Option<Theme> {
    let table: toml::Table = std::fs::read_to_string(path).ok()?.parse().ok()?;
    let mut theme = Theme {
        mode: "dark".to_string(),
        colors: BTreeMap::new(),
    };
    for (key, value) in table {
        let Some(value) = value.as_str() else {
            continue;
        };
        if key == "mode" {
            theme.mode = value.to_string();
        } else {
            theme.colors.insert(key, value.to_string());
        }
    }
    Some(theme)
}

/// CSS custom properties for `t`, one `--posto-<key>` per colour.
pub fn css_vars(t: &Theme) -> String {
    let mut css = format!(":root{{color-scheme:{};", t.mode);
    for (key, value) in &t.colors {
        css.push_str(&format!("--posto-{}:{value};", key.replace('_', "-")));
    }
    css.push('}');
    css
}

/// Watches the current Omarchy theme; see [`watch_at`].
pub fn watch(on_change: impl Fn(Theme) + Send + 'static) -> Result<Option<Watcher>> {
    watch_at(&omarchy_theme_path(), on_change)
}

/// Calls `on_change` whenever the theme at `path` changes.
///
/// Watches the theme directory and its parent, since Omarchy switches
/// themes by swapping the `theme` symlink in the parent. Returns `None`
/// when `path` does not exist.
pub fn watch_at(
    path: &Path,
    on_change: impl Fn(Theme) + Send + 'static,
) -> Result<Option<Watcher>> {
    if !path.exists() {
        return Ok(None);
    }
    let (Some(theme_dir), Some(current_dir)) =
        (path.parent(), path.parent().and_then(Path::parent))
    else {
        return Ok(None);
    };

    // Read before watching so a change during setup is not lost.
    let last = detect_at(path);

    let (tx, rx) = mpsc::channel();
    let event_tx = tx.clone();
    // The callback only signals: calling watch/unwatch from inside it deadlocks.
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && !matches!(event.kind, EventKind::Access(_))
        {
            let _ = event_tx.send(Message::Changed);
        }
    })
    .map_err(notify_error)?;

    let resolved = theme_dir.canonicalize()?;
    watcher
        .watch(&resolved, RecursiveMode::NonRecursive)
        .map_err(notify_error)?;
    watcher
        .watch(current_dir, RecursiveMode::NonRecursive)
        .map_err(notify_error)?;

    let worker = Worker {
        path: path.to_path_buf(),
        watched: resolved,
        last,
        watcher,
        rx,
    };
    let handle = std::thread::spawn(move || worker.run(on_change));
    Ok(Some(Watcher {
        stop: tx,
        worker: Some(handle),
    }))
}

fn notify_error(e: notify::Error) -> Error {
    Error::Other(format!("watch theme: {e}"))
}

enum Message {
    Changed,
    Stop,
}

/// Stops watching when dropped.
pub struct Watcher {
    stop: Sender<Message>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.stop.send(Message::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Worker {
    path: PathBuf,
    /// The resolved theme directory currently watched.
    watched: PathBuf,
    last: Option<Theme>,
    /// Owned here so dropping the worker stops the watches.
    watcher: notify::RecommendedWatcher,
    rx: Receiver<Message>,
}

impl Worker {
    fn run(mut self, on_change: impl Fn(Theme)) {
        loop {
            match self.rx.recv() {
                Ok(Message::Changed) => {}
                Ok(Message::Stop) | Err(_) => return,
            }
            // Debounce: swallow the rest of the burst.
            loop {
                match self.rx.recv_timeout(DEBOUNCE) {
                    Ok(Message::Changed) => {}
                    Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => break,
                }
            }
            self.follow_symlink();
            if let Some(theme) = detect_at(&self.path)
                && self.last.as_ref() != Some(&theme)
            {
                self.last = Some(theme.clone());
                on_change(theme);
            }
        }
    }

    /// Re-attaches the watch when the `theme` symlink now points elsewhere.
    fn follow_symlink(&mut self) {
        let Some(dir) = self.path.parent().and_then(|d| d.canonicalize().ok()) else {
            return;
        };
        if dir == self.watched {
            return;
        }
        let _ = self.watcher.unwatch(&self.watched);
        match self.watcher.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => self.watched = dir,
            Err(e) => tracing::warn!("watch {}: {e}", dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn colors(accent: &str) -> String {
        format!("mode = \"dark\"\naccent = \"{accent}\"\n")
    }

    /// Waits for a theme whose accent is `accent`; earlier ones are skipped.
    fn expect_accent(rx: &mpsc::Receiver<Theme>, accent: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let theme = rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no theme with accent {accent} within 5s"));
            if theme.colors.get("accent").map(String::as_str) == Some(accent) {
                return;
            }
        }
    }

    #[test]
    fn css_vars_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colors.toml");
        std::fs::write(
            &path,
            "mode = \"dark\"\naccent = \"#CC785C\"\ndark_background = \"#141311\"\nsize = 3\n",
        )
        .unwrap();
        let theme = detect_at(&path).unwrap();
        assert_eq!(theme.colors.len(), 2, "non-string values are ignored");
        let css = css_vars(&theme);
        assert!(css.starts_with(":root{"), "{css}");
        assert!(css.contains("color-scheme:dark;"), "{css}");
        assert!(css.contains("--posto-accent:#CC785C;"), "{css}");
        assert!(css.contains("--posto-dark-background:#141311;"), "{css}");
        assert!(css.ends_with('}'), "{css}");
    }

    #[test]
    fn detect_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("omarchy/current/theme/colors.toml");
        assert_eq!(detect_at(&path), None);
        assert!(watch_at(&path, |_| {}).unwrap().is_none());
    }

    /// `themes/a` with accent #111111 and `current/theme -> ../themes/a`.
    #[cfg(unix)]
    fn omarchy_layout(root: &Path) -> PathBuf {
        let themes = root.join("themes");
        let current = root.join("current");
        std::fs::create_dir_all(themes.join("a")).unwrap();
        std::fs::create_dir_all(themes.join("b")).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(themes.join("a/colors.toml"), colors("#111111")).unwrap();
        std::fs::write(themes.join("b/colors.toml"), colors("#333333")).unwrap();
        std::os::unix::fs::symlink("../themes/a", current.join("theme")).unwrap();
        current.join("theme/colors.toml")
    }

    #[cfg(unix)]
    #[test]
    fn watch_fires_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = omarchy_layout(dir.path());
        let (tx, rx) = mpsc::channel();
        let _watcher = watch_at(&path, move |t| {
            let _ = tx.send(t);
        })
        .unwrap()
        .expect("theme exists");

        std::fs::write(dir.path().join("themes/a/colors.toml"), colors("#222222")).unwrap();
        expect_accent(&rx, "#222222");
    }

    #[cfg(unix)]
    #[test]
    fn watch_follows_symlink_swap() {
        let dir = tempfile::tempdir().unwrap();
        let path = omarchy_layout(dir.path());
        let (tx, rx) = mpsc::channel();
        let _watcher = watch_at(&path, move |t| {
            let _ = tx.send(t);
        })
        .unwrap()
        .expect("theme exists");

        let current = dir.path().join("current");
        std::os::unix::fs::symlink("../themes/b", current.join("theme.tmp")).unwrap();
        std::fs::rename(current.join("theme.tmp"), current.join("theme")).unwrap();
        expect_accent(&rx, "#333333");

        std::fs::write(dir.path().join("themes/b/colors.toml"), colors("#444444")).unwrap();
        expect_accent(&rx, "#444444");
    }
}
