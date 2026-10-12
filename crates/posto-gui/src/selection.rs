//! Multi-selection over the message list: pure, so it tests without a UI.
//!
//! Ids are row keys (`<account>/<id>`), never bare email ids, which repeat
//! across accounts.

/// Which rows are selected, plus the range anchor and the focused row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub anchor: Option<String>,
    pub focus: Option<String>,
    /// In list order, no duplicates.
    pub ids: Vec<String>,
}

/// How a row click changes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    Plain,
    Toggle,
    Range,
}

fn position(order: &[String], id: &str) -> Option<usize> {
    order.iter().position(|o| o == id)
}

impl Selection {
    fn only(&mut self, id: &str) {
        self.ids = vec![id.to_string()];
        self.anchor = Some(id.to_string());
        self.focus = Some(id.to_string());
    }

    /// Selects `anchor..=focus` (either way round) of `order`.
    fn span(&mut self, order: &[String], from: usize, to: usize) {
        let (lo, hi) = (from.min(to), from.max(to));

        self.ids = order[lo..=hi].to_vec();
        self.focus = Some(order[to].clone());
    }

    /// Applies a click on row `id`; an id missing from `order` is ignored.
    pub fn click(&mut self, order: &[String], id: &str, kind: Click) {
        let Some(at) = position(order, id) else {
            return;
        };

        match kind {
            Click::Plain => self.only(id),
            Click::Toggle => {
                if self.is_selected(id) {
                    self.ids.retain(|s| s != id);
                } else {
                    self.ids.push(id.to_string());
                    self.ids.sort_by_key(|s| position(order, s));
                    self.anchor = Some(id.to_string());
                }

                if self.ids.is_empty() {
                    self.anchor = None;
                    self.focus = None;
                } else {
                    self.focus = Some(id.to_string());
                }
            }
            Click::Range => {
                let from = self
                    .anchor
                    .as_deref()
                    .and_then(|a| position(order, a))
                    .unwrap_or(at);

                if self.anchor.is_none() {
                    self.anchor = Some(id.to_string());
                }

                self.span(order, from, at);
            }
        }
    }

    /// Where the focus would land `delta` rows away, clamped. With no focus the
    /// first row (moving down) or last row (moving up).
    fn target(&self, order: &[String], delta: i32) -> Option<usize> {
        let last = order.len().checked_sub(1)?;
        let current = self.focus.as_deref().and_then(|f| position(order, f));

        Some(match current {
            Some(i) => (i as i64 + i64::from(delta)).clamp(0, last as i64) as usize,
            None if delta > 0 => 0,
            None => last,
        })
    }

    /// Keyboard range extension: the focus moves `delta` rows and the selection
    /// becomes the range from the anchor to it.
    pub fn extend(&mut self, order: &[String], delta: i32) {
        let Some(to) = self.target(order, delta) else {
            return;
        };
        let from = self
            .anchor
            .as_deref()
            .and_then(|a| position(order, a))
            .filter(|_| self.focus.is_some());

        match from {
            Some(from) => self.span(order, from, to),
            None => self.only(&order[to]),
        }
    }

    /// Plain move: the focus moves `delta` rows and the selection collapses to it.
    pub fn step(&mut self, order: &[String], delta: i32) {
        if let Some(to) = self.target(order, delta) {
            self.only(&order[to]);
        }
    }

    /// Selects every row.
    pub fn all(&mut self, order: &[String]) {
        self.ids = order.to_vec();
        self.anchor = order.first().cloned();
        self.focus = order.last().cloned();
    }

    /// Drops everything but the focused row.
    pub fn collapse(&mut self) {
        self.ids = self.focus.iter().cloned().collect();
        self.anchor = self.focus.clone();
    }

    /// After a list reload: forgets rows that are gone. When the focus is gone
    /// it moves to the first remaining selected row, or nowhere (the caller then
    /// picks a row).
    pub fn retain(&mut self, order: &[String]) {
        self.ids.retain(|id| order.contains(id));

        let gone = |id: &Option<String>| id.as_ref().is_some_and(|i| !order.contains(i));

        if gone(&self.focus) {
            self.focus = self.ids.first().cloned();
            self.anchor = self.focus.clone();
        } else if gone(&self.anchor) {
            self.anchor = self.focus.clone();
        }
    }

    pub fn is_selected(&self, id: &str) -> bool {
        self.ids.iter().any(|s| s == id)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> Vec<String> {
        "abcdef".chars().map(String::from).collect()
    }

    fn ids(s: &Selection) -> String {
        s.ids.concat()
    }

    fn at(id: &str) -> Option<String> {
        Some(id.to_string())
    }

    #[test]
    fn plain_toggle_range() {
        let order = order();
        let mut s = Selection::default();

        s.click(&order, "c", Click::Plain);
        assert_eq!(ids(&s), "c");

        s.click(&order, "e", Click::Toggle);
        assert_eq!(
            (ids(&s).as_str(), &s.focus, &s.anchor),
            ("ce", &at("e"), &at("e"))
        );

        s.click(&order, "c", Click::Toggle);
        assert_eq!(ids(&s), "e");
        assert_eq!(s.anchor, at("e"));

        s.click(&order, "b", Click::Range);
        assert_eq!(
            (ids(&s).as_str(), &s.anchor, &s.focus),
            ("bcde", &at("e"), &at("b"))
        );

        s.click(&order, "f", Click::Range);
        assert_eq!(ids(&s), "ef");

        s.click(&order, "zzz", Click::Plain);
        assert_eq!(ids(&s), "ef");
    }

    #[test]
    fn extend_and_step() {
        let order = order();
        let mut s = Selection::default();

        s.click(&order, "c", Click::Plain);
        s.extend(&order, 2);
        assert_eq!((ids(&s).as_str(), &s.focus), ("cde", &at("e")));

        s.extend(&order, -3);
        assert_eq!(
            (ids(&s).as_str(), &s.anchor, &s.focus),
            ("bc", &at("c"), &at("b"))
        );

        s.step(&order, 1);
        assert_eq!(
            (ids(&s).as_str(), &s.anchor, &s.focus),
            ("c", &at("c"), &at("c"))
        );

        s.step(&order, -10);
        assert_eq!(ids(&s), "a");

        let mut empty = Selection::default();
        empty.extend(&order, 1);
        assert_eq!(ids(&empty), "a");
    }

    #[test]
    fn all_collapse_retain() {
        let order = order();
        let mut s = Selection::default();

        s.all(&order);
        assert_eq!((s.len(), &s.anchor, &s.focus), (6, &at("a"), &at("f")));

        s.collapse();
        assert_eq!(ids(&s), "f");

        let without = |gone: &str| -> Vec<String> {
            order
                .iter()
                .filter(|o| !gone.contains(o.as_str()))
                .cloned()
                .collect()
        };

        s.retain(&without("fd"));
        assert!(s.is_empty());
        assert_eq!(s.focus, None);

        let mut s = Selection::default();
        s.click(&order, "b", Click::Plain);
        s.click(&order, "d", Click::Range);
        assert_eq!((ids(&s).as_str(), &s.focus), ("bcd", &at("d")));

        s.retain(&without("c"));
        assert_eq!((ids(&s).as_str(), &s.focus), ("bd", &at("d")));
    }

    #[test]
    fn colliding_ids_across_accounts() {
        // The same email id in two accounts: two distinct rows.
        let order: Vec<String> = vec!["a/e1".into(), "b/e1".into()];
        let mut s = Selection::default();

        s.click(&order, "b/e1", Click::Plain);
        assert_eq!(s.ids, ["b/e1"]);
        assert!(s.is_selected("b/e1"));
        assert!(!s.is_selected("a/e1"));
        assert_eq!(s.focus, at("b/e1"));

        s.click(&order, "a/e1", Click::Plain);
        assert_eq!(s.ids, ["a/e1"]);

        s.click(&order, "b/e1", Click::Range);
        assert_eq!(s.ids, ["a/e1", "b/e1"]);
    }
}
