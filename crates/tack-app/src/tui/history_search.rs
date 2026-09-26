//! Ctrl+R incremental reverse search through the editor history
//! (bash reverse-i-search): the query filters as you type, the current
//! hit previews live in the editor, ctrl+r/↑ cycle to older hits, ↓ back
//! to newer ones, Enter accepts into the editor, Esc restores the draft.

use tack_tui::{Line, Span};

use super::theme::Theme;

/// Matching history indices, newest first (reverse-i-search order).
/// Case-insensitive substring match; an empty query matches nothing (the
/// search bar shows the usage hint instead).
pub fn find_matches(history: &[String], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    (0..history.len())
        .rev()
        .filter(|&i| history[i].to_lowercase().contains(&needle))
        .collect()
}

/// Incremental history search state (at most one active search).
#[derive(Debug)]
pub struct HistorySearch {
    pub query: String,
    /// Editor content when the search started (restored on Esc).
    saved: String,
    /// `find_matches` result for the current query.
    matches: Vec<usize>,
    /// Index into `matches` (0 = newest hit).
    cursor: usize,
}

impl HistorySearch {
    pub fn new(saved: String) -> Self {
        HistorySearch {
            query: String::new(),
            saved,
            matches: Vec::new(),
            cursor: 0,
        }
    }

    /// The pre-search draft (Esc restores it).
    pub fn saved(&self) -> &str {
        &self.saved
    }

    /// Replace the query and re-match from the newest hit.
    pub fn set_query(&mut self, query: String, history: &[String]) {
        self.query = query;
        self.matches = find_matches(history, &self.query);
        self.cursor = 0;
    }

    /// Text the editor should preview right now: the current hit, or the
    /// saved draft while nothing matches (empty query included).
    pub fn preview<'a>(&'a self, history: &'a [String]) -> &'a str {
        match self.matches.get(self.cursor) {
            Some(&i) => history[i].as_str(),
            None => &self.saved,
        }
    }

    /// Cycle to an older hit (wraps around to the newest).
    pub fn older(&mut self) {
        if !self.matches.is_empty() {
            self.cursor = (self.cursor + 1) % self.matches.len();
        }
    }

    /// Cycle back to a newer hit (wraps around to the oldest).
    pub fn newer(&mut self) {
        if !self.matches.is_empty() {
            self.cursor = (self.cursor + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// The one-line search bar rendered above the editor.
    pub fn bar_line(&self, theme: &Theme) -> Line {
        let mut line = Line::new();
        line.push(Span::styled("(reverse-i-search)`", theme.accent));
        line.push(Span::plain(self.query.clone()));
        line.push(Span::styled("'", theme.accent));
        if self.query.is_empty() {
            line.push(Span::styled(
                format!("  {}", crate::i18n::tr("search.history_hint")),
                theme.dim,
            ));
        } else if self.matches.is_empty() {
            line.push(Span::styled(
                format!("  {}", crate::i18n::tr("search.history_no_match")),
                theme.error,
            ));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn history() -> Vec<String> {
        vec![
            "cargo build".to_string(),
            "git status".to_string(),
            "cargo test".to_string(),
            "git commit".to_string(),
        ]
    }

    #[test]
    fn matches_are_newest_first_and_case_insensitive() {
        let h = history();
        assert_eq!(find_matches(&h, "git"), vec![3, 1]);
        assert_eq!(find_matches(&h, "CARGO"), vec![2, 0]);
        assert_eq!(find_matches(&h, "status"), vec![1]);
        assert!(find_matches(&h, "nope").is_empty());
        // Empty / whitespace-only query: no filtering, the bar shows the hint.
        assert!(find_matches(&h, "").is_empty());
        assert!(find_matches(&h, "   ").is_empty());
    }

    #[test]
    fn older_and_newer_cycle_through_matches() {
        let h = history();
        let mut search = HistorySearch::new(String::new());
        search.set_query("cargo".to_string(), &h);
        // Newest hit first.
        assert_eq!(search.preview(&h), "cargo test");
        search.older();
        assert_eq!(search.preview(&h), "cargo build");
        // Wraps back to the newest.
        search.older();
        assert_eq!(search.preview(&h), "cargo test");
        search.newer();
        assert_eq!(search.preview(&h), "cargo build");
        search.newer();
        assert_eq!(search.preview(&h), "cargo test");
    }

    #[test]
    fn preview_falls_back_to_the_saved_draft() {
        let h = history();
        let mut search = HistorySearch::new("draft text".to_string());
        // Empty query: draft preview.
        assert_eq!(search.preview(&h), "draft text");
        // No match: draft preview (bar shows "no match").
        search.set_query("zzz".to_string(), &h);
        assert_eq!(search.preview(&h), "draft text");
        // Cycling with no matches is a no-op (never panics).
        search.older();
        search.newer();
        assert_eq!(search.preview(&h), "draft text");
    }

    #[test]
    fn requerying_resets_to_the_newest_hit() {
        let h = history();
        let mut search = HistorySearch::new(String::new());
        search.set_query("git".to_string(), &h);
        search.older();
        assert_eq!(search.preview(&h), "git status");
        search.set_query("git c".to_string(), &h);
        assert_eq!(search.preview(&h), "git commit");
    }

    #[test]
    fn bar_line_shows_hint_and_no_match() {
        let theme = Theme::dark();
        let mut search = HistorySearch::new(String::new());
        let bar = search.bar_line(&theme).text();
        assert!(bar.contains("reverse-i-search"), "{bar}");
        assert!(bar.contains("history"), "{bar}"); // hint (en)
        search.set_query("zzz".to_string(), &history());
        let bar = search.bar_line(&theme).text();
        assert!(bar.contains("no match"), "{bar}");
        // A matching query shows neither hint nor "no match".
        search.set_query("git".to_string(), &history());
        let bar = search.bar_line(&theme).text();
        assert!(!bar.contains("no match"), "{bar}");
    }
}
