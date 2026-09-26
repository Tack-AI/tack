//! Fullscreen (alternate screen) mode support: transcript viewport scrolling,
//! mouse text selection + OSC52 copy, transcript search, OSC-133-style prompt
//! jumping. App-side glue; the frame pipeline is tack-tui's AltScreenRenderer.

use tack_tui::{Line, Style};

/// Transcript search state (Ctrl+Shift+F). Matches are content-space rows
/// (the scroll view's content), not screen rows.
#[derive(Clone, Debug, Default)]
pub struct SearchState {
    pub query: String,
    /// Matching row indices in the scroll content.
    pub matches: Vec<usize>,
    /// Current match index (into `matches`).
    pub current: usize,
    /// Set when the query/content changed and matches need recomputing.
    pub dirty: bool,
    /// Content revision the matches were computed against:
    /// (transcript items, stream_rev, transcript_total). Streaming/appended
    /// content changes these without touching `dirty`, so the renderer
    /// treats a mismatch as stale and recomputes (otherwise matches freeze
    /// at whatever the transcript looked like when the query was typed).
    pub content_key: (usize, u64, usize),
    /// When the matches were last recomputed. Content-key recomputes are
    /// throttled through this (stream_rev ticks per delta; a full
    /// materialize+rescan per delta would melt the frame budget). `None`
    /// (never computed) always recomputes.
    pub last_compute: Option<std::time::Instant>,
}

impl SearchState {
    /// Recompute matches against content lines (content space).
    pub fn update(&mut self, content: &[Line]) {
        self.matches.clear();
        self.dirty = false;
        if self.query.is_empty() {
            return;
        }
        let query = self.query.to_lowercase();
        for (i, line) in content.iter().enumerate() {
            if line.text().to_lowercase().contains(&query) {
                self.matches.push(i);
            }
        }
        if self.current >= self.matches.len() {
            self.current = self.matches.len().saturating_sub(1);
        }
    }

    pub fn current_row(&self) -> Option<usize> {
        self.matches.get(self.current).copied()
    }

    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    pub fn previous(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + self.matches.len() - 1) % self.matches.len();
        }
    }
}

/// A drag selection in screen cells (row, col), anchor first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
}

impl Selection {
    /// Normalized so start <= end in reading order.
    pub fn normalized(self) -> Selection {
        if self.start <= self.end {
            self
        } else {
            Selection {
                start: self.end,
                end: self.start,
            }
        }
    }

    /// Extract the selected text from frame lines (cell-accurate).
    pub fn extract(&self, frame: &[Line]) -> String {
        let sel = self.normalized();
        let mut out = String::new();
        for row in sel.start.0..=sel.end.0 {
            let Some(line) = frame.get(row as usize) else {
                continue;
            };
            let from = if row == sel.start.0 {
                sel.start.1 as usize
            } else {
                0
            };
            let to = if row == sel.end.0 {
                sel.end.1 as usize
            } else {
                line.width()
            };
            let slice = line.slice_cells(from, to.saturating_sub(from));
            out.push_str(slice.text().trim_end());
            if row < sel.end.0 {
                out.push('\n');
            }
        }
        out
    }

    /// Highlight overlay: paint the selected cells with an inverse style.
    pub fn highlight(&self, frame: &mut [Line]) {
        let sel = self.normalized();
        let style = Style::new().inverse();
        for row in sel.start.0..=sel.end.0 {
            let Some(line) = frame.get_mut(row as usize) else {
                continue;
            };
            let from = if row == sel.start.0 {
                sel.start.1 as usize
            } else {
                0
            };
            let to = if row == sel.end.0 {
                sel.end.1 as usize
            } else {
                line.width()
            };
            if to <= from {
                continue;
            }
            let mut highlighted = line.slice_cells(0, from);
            let mut mid = line.slice_cells(from, to - from);
            for span in &mut mid.spans {
                span.style = span.style.merged_with(&style);
            }
            highlighted.spans.extend(mid.spans);
            highlighted
                .spans
                .extend(line.slice_cells(to, usize::MAX).spans);
            *line = highlighted;
        }
    }
}

/// Resolve the Ctrl+X copy target in fullscreen mode (TS handleCopyCommand
/// with `preferSelection`): when `fullscreenCopyOnSelect` is disabled, an
/// active non-empty selection is the copy target; returns `None` otherwise
/// so the caller falls back to copying the last assistant message.
pub fn ctrl_x_selection_text(
    copy_on_select: bool,
    selection: Option<Selection>,
    frame: &[Line],
) -> Option<String> {
    if copy_on_select {
        return None;
    }
    let text = selection?.extract(frame);
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn selection_extract_single_line() {
        let frame = vec![Line::plain("hello world")];
        let sel = Selection {
            start: (0, 6),
            end: (0, 11),
        };
        assert_eq!(sel.extract(&frame), "world");
    }

    #[test]
    fn selection_extract_multiline_reversed() {
        let frame = vec![Line::plain("abcdef"), Line::plain("ghijkl")];
        let sel = Selection {
            start: (1, 2),
            end: (0, 2),
        }; // reversed
        assert_eq!(sel.extract(&frame), "cdef\ngh");
    }

    #[test]
    fn ctrl_x_selection_text_prefers_selection_only_when_copy_on_select_off() {
        let frame = vec![Line::plain("hello world")];
        let sel = Selection {
            start: (0, 0),
            end: (0, 5),
        };
        // copy-on-select on: Ctrl+X falls back to the last assistant message.
        assert_eq!(ctrl_x_selection_text(true, Some(sel), &frame), None);
        // copy-on-select off + active selection: the selection is copied.
        assert_eq!(
            ctrl_x_selection_text(false, Some(sel), &frame),
            Some("hello".to_string())
        );
        // No selection: fall back to the last assistant message.
        assert_eq!(ctrl_x_selection_text(false, None, &frame), None);
        // Whitespace-only selection counts as no selection (TS
        // hasActiveSelection checks the extracted text).
        let blank = Selection {
            start: (1, 0),
            end: (1, 3),
        };
        assert_eq!(ctrl_x_selection_text(false, Some(blank), &frame), None);
    }

    #[test]
    fn search_finds_and_cycles() {
        let frame = vec![
            Line::plain("nothing"),
            Line::plain("a match here"),
            Line::plain("another MATCH"),
        ];
        let mut search = SearchState {
            query: "match".into(),
            ..Default::default()
        };
        search.update(&frame);
        assert_eq!(search.matches, vec![1, 2]);
        search.next();
        assert_eq!(search.current_row(), Some(2));
        search.next();
        assert_eq!(search.current_row(), Some(1));
        search.previous();
        assert_eq!(search.current_row(), Some(2));
    }
}
