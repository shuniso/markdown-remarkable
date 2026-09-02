//! Per-window back/forward navigation history: a plain browser-style stack
//! with a cursor, entirely independent of any window/event-loop state so it
//! can be unit-tested on its own. `app.rs`'s `WindowCtx` owns one of these
//! (behind a `Mutex`, since `routes::handle`'s `PUT /nav` reads it from the
//! WebView's own protocol-handler thread — see that module's docs), and
//! `open_file` decides whether a given switch pushes onto it.
//!
//! Deliberately mirrors a browser tab's own history semantics: navigating to
//! a new page (`push`) discards any "forward" entries past the current
//! position, `back`/`forward` move the cursor without touching the stack
//! itself, and navigating to the page already showing is a no-op rather than
//! growing the stack with a duplicate entry.

use std::path::PathBuf;

/// The most entries [`NavHistory::push`] ever keeps at once — past this,
/// the oldest entries are dropped to make room, same as a real browser tab's
/// history isn't allowed to grow forever. Chosen generously: comfortably
/// more than anyone would click through in one session, while still
/// bounding how much memory a window's history can consume no matter how
/// long it stays open.
const MAX_ENTRIES: usize = 50;

/// One window's back/forward history. `entries` is never empty once
/// constructed — [`NavHistory::new`] always seeds it with the initial path —
/// and `cursor` is always a valid index into it. `entries.len()` never
/// exceeds [`MAX_ENTRIES`] — see [`NavHistory::push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavHistory {
    entries: Vec<PathBuf>,
    cursor: usize,
}

impl NavHistory {
    /// Starts a fresh history containing only `initial`, positioned on it.
    pub fn new(initial: PathBuf) -> Self {
        NavHistory {
            entries: vec![initial],
            cursor: 0,
        }
    }

    /// Navigates to `path`: entries past the current cursor (the "forward"
    /// part of the stack, if `back` was used before this) are discarded, and
    /// `path` is appended and becomes current. A no-op if `path` is already
    /// the current entry — clicking the same link twice, or switching to the
    /// file already open, shouldn't grow the stack with a duplicate.
    pub fn push(&mut self, path: PathBuf) {
        if self.current() == Some(path.as_path()) {
            return;
        }
        self.entries.truncate(self.cursor + 1);
        self.entries.push(path);
        self.cursor = self.entries.len() - 1;
        // Drop from the *oldest* end once over the cap — same "the
        // farthest-back entries go first" rule a browser's own history
        // limit follows. `cursor` moves down by the same amount so it keeps
        // pointing at the same (now-shifted) entry.
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(0..excess);
            self.cursor -= excess;
        }
    }

    /// Moves the cursor one entry back and returns that entry, or `None`
    /// (leaving the cursor untouched) if already at the oldest entry.
    pub fn back(&mut self) -> Option<PathBuf> {
        if !self.can_back() {
            return None;
        }
        self.cursor -= 1;
        self.entries.get(self.cursor).cloned()
    }

    /// Moves the cursor one entry forward and returns that entry, or `None`
    /// (leaving the cursor untouched) if already at the newest entry.
    pub fn forward(&mut self) -> Option<PathBuf> {
        if !self.can_forward() {
            return None;
        }
        self.cursor += 1;
        self.entries.get(self.cursor).cloned()
    }

    /// `true` if [`Self::back`] would return `Some`.
    pub fn can_back(&self) -> bool {
        self.cursor > 0
    }

    /// `true` if [`Self::forward`] would return `Some`.
    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.entries.len()
    }

    /// The entry the cursor currently sits on.
    pub fn current(&self) -> Option<&std::path::Path> {
        self.entries.get(self.cursor).map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn new_starts_on_the_initial_entry_with_no_history_either_way() {
        let history = NavHistory::new(p("a.md"));
        assert_eq!(history.current(), Some(p("a.md").as_path()));
        assert!(!history.can_back());
        assert!(!history.can_forward());
    }

    #[test]
    fn push_advances_current_and_enables_back() {
        let mut history = NavHistory::new(p("a.md"));
        history.push(p("b.md"));
        assert_eq!(history.current(), Some(p("b.md").as_path()));
        assert!(history.can_back());
        assert!(!history.can_forward());
    }

    #[test]
    fn push_same_path_as_current_is_a_no_op() {
        let mut history = NavHistory::new(p("a.md"));
        history.push(p("a.md"));
        assert_eq!(history.current(), Some(p("a.md").as_path()));
        assert!(!history.can_back());
    }

    #[test]
    fn back_then_forward_round_trips() {
        let mut history = NavHistory::new(p("a.md"));
        history.push(p("b.md"));
        assert_eq!(history.back(), Some(p("a.md")));
        assert_eq!(history.current(), Some(p("a.md").as_path()));
        assert!(!history.can_back());
        assert!(history.can_forward());
        assert_eq!(history.forward(), Some(p("b.md")));
        assert_eq!(history.current(), Some(p("b.md").as_path()));
        assert!(history.can_back());
        assert!(!history.can_forward());
    }

    #[test]
    fn back_past_the_oldest_entry_returns_none_and_does_not_move() {
        let mut history = NavHistory::new(p("a.md"));
        assert_eq!(history.back(), None);
        assert_eq!(history.current(), Some(p("a.md").as_path()));
    }

    #[test]
    fn forward_past_the_newest_entry_returns_none_and_does_not_move() {
        let mut history = NavHistory::new(p("a.md"));
        assert_eq!(history.forward(), None);
        assert_eq!(history.current(), Some(p("a.md").as_path()));
    }

    #[test]
    fn push_after_back_truncates_the_forward_branch() {
        let mut history = NavHistory::new(p("a.md"));
        history.push(p("b.md"));
        history.push(p("c.md"));
        history.back(); // now on b.md, c.md still reachable via forward
        assert!(history.can_forward());
        history.push(p("d.md")); // discards c.md
        assert_eq!(history.current(), Some(p("d.md").as_path()));
        assert!(!history.can_forward());
        assert_eq!(history.back(), Some(p("b.md")));
        assert_eq!(history.back(), Some(p("a.md")));
        assert_eq!(history.back(), None);
    }

    #[test]
    fn navigating_back_to_a_previously_visited_path_via_push_does_not_dedupe_across_the_whole_stack(
    ) {
        // push() only special-cases the *current* entry, not the whole
        // stack — visiting a.md -> b.md -> a.md again is a real forward
        // step, not treated as "already been here" the way `back()` would.
        let mut history = NavHistory::new(p("a.md"));
        history.push(p("b.md"));
        history.push(p("a.md"));
        assert_eq!(history.current(), Some(p("a.md").as_path()));
        assert!(history.can_back());
        assert_eq!(history.back(), Some(p("b.md")));
    }

    #[test]
    fn push_caps_at_max_entries_dropping_the_oldest_and_keeping_the_cursor_correct() {
        // v0.md is the initial entry; push v1.md..v59.md (59 more) for 60
        // entries total, 10 over the 50 cap.
        let mut history = NavHistory::new(p("v0.md"));
        for i in 1..60 {
            history.push(p(&format!("v{i}.md")));
        }
        assert_eq!(history.current(), Some(p("v59.md").as_path()));
        // The oldest 10 (v0..v9) were dropped, leaving v10..v59 (50
        // entries) — walking all the way back should land on v10.md, not
        // v0.md, and can_back() should then be false.
        for _ in 0..49 {
            history.back();
        }
        assert_eq!(history.current(), Some(p("v10.md").as_path()));
        assert!(!history.can_back());
    }
}
