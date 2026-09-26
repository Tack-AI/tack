//! Multi-line editor (port of tack-tui's `editor.ts`, core subset):
//! grapheme-based cursor, word ops, kill-ring, undo, history, bracketed-paste
//! markers, wrapping render with hardware cursor marker.
//!
//! T3 adds autocomplete providers and jump-to-char.

use unicode_segmentation::UnicodeSegmentation;

use crate::component::Component;
use crate::input::{InputEvent, Key, KeyEvent, Modifiers};
use crate::line::{Line, Span};
use crate::style::{Color, Style};

/// A large paste stored out-of-line, shown as a marker.
#[derive(Clone, Debug)]
struct PasteBlock {
    id: usize,
    text: String,
}

#[derive(Clone, Debug)]
struct Snapshot {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    /// Approximate retained bytes (content + String headers), for the
    /// undo byte budget.
    bytes: usize,
}

fn lines_bytes(lines: &[String]) -> usize {
    lines.iter().map(|l| l.len() + 24).sum()
}

/// Maximum number of submitted prompts kept in history. Older entries
/// are dropped on push, so long sessions keep the most recent 500
/// instead of accumulating every prompt forever.
const HISTORY_MAX: usize = 500;
/// Maximum submitted-prompt history bytes. Pasted prompts can be huge;
/// a count-only cap would pin them all (the session transcript has its
/// own copy — history is just the Ctrl+R/Up-arrow index).
const HISTORY_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Undo retention: at most UNDO_MAX snapshots AND UNDO_MAX_BYTES of
/// content. After a large paste every snapshot is itself large, so a
/// count-only cap pins paste-size × 200.
const UNDO_MAX: usize = 200;
const UNDO_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Kill-ring retention (Emacs-style cycling). Giant kills (e.g. a killed
/// huge pasted line) are not retained at all.
const KILL_RING_MAX: usize = 60;
const KILL_MAX_BYTES: usize = 256 * 1024;

/// The editor widget.
#[derive(Debug)]
pub struct Editor {
    lines: Vec<String>,
    /// Cursor position in grapheme indices.
    cursor_line: usize,
    cursor_col: usize,
    history: Vec<String>,
    history_bytes: usize,
    history_index: Option<usize>,
    history_draft: String,
    kill_ring: Vec<String>,
    kill_ring_index: usize,
    undo: Vec<Snapshot>,
    undo_bytes: usize,
    pastes: Vec<PasteBlock>,
    next_paste_id: usize,
    /// Set on Enter; the app drains it (submit).
    pub submitted: Option<String>,
    pub focused: bool,
    pub border_style: Style,
    pub text_style: Style,
    /// Gutter shown before each visual line (e.g. "> ").
    pub gutter: String,
    pub gutter_style: Style,
    /// Horizontal padding inside the editor area.
    pub padding_x: u16,
    /// Max visible lines before the editor scrolls internally.
    pub max_height: u16,
    scroll_offset: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Editor {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            history: Vec::new(),
            history_bytes: 0,
            history_index: None,
            history_draft: String::new(),
            kill_ring: Vec::new(),
            kill_ring_index: 0,
            undo: Vec::new(),
            undo_bytes: 0,
            pastes: Vec::new(),
            next_paste_id: 1,
            submitted: None,
            focused: false,
            border_style: Style::new().fg(Color::Indexed(8)),
            text_style: Style::default(),
            gutter: "> ".to_string(),
            gutter_style: Style::new().fg(Color::Indexed(2)),
            padding_x: 0,
            max_height: 8,
            scroll_offset: 0,
        }
    }
}

impl Editor {
    pub fn new() -> Self {
        Editor::default()
    }

    /// Current text with paste markers expanded.
    pub fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        for paste in &self.pastes {
            text = text.replace(&paste_marker(paste), &paste.text);
        }
        text
    }

    pub fn set_text(&mut self, text: &str) {
        self.snapshot();
        self.lines = text.split('\n').map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = graphemes(&self.lines[self.cursor_line]).count();
        self.history_index = None;
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    pub fn add_to_history(&mut self, text: &str) {
        if !text.trim().is_empty() && self.history.last().is_none_or(|last| last != text) {
            self.history_bytes += text.len() + 24;
            self.history.push(text.to_string());
            // Cap at HISTORY_MAX entries / HISTORY_MAX_BYTES, dropping
            // the oldest. Safe to trim while a navigation session is
            // live: `history_index` is reset below, and any index held
            // by the caller is invalidated by the push anyway.
            while self.history.len() > HISTORY_MAX
                || (self.history_bytes > HISTORY_MAX_BYTES && self.history.len() > 1)
            {
                let old = self.history.remove(0);
                self.history_bytes = self.history_bytes.saturating_sub(old.len() + 24);
            }
        }
        self.history_index = None;
    }

    /// Submitted prompt history, oldest first (drives the app's Ctrl+R
    /// reverse search).
    pub fn history(&self) -> &[String] {
        &self.history
    }

    fn snapshot(&mut self) {
        let bytes = lines_bytes(&self.lines);
        self.undo.push(Snapshot {
            lines: self.lines.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
            bytes,
        });
        self.undo_bytes += bytes;
        while self.undo.len() > UNDO_MAX
            || (self.undo_bytes > UNDO_MAX_BYTES && self.undo.len() > 1)
        {
            let old = self.undo.remove(0);
            self.undo_bytes = self.undo_bytes.saturating_sub(old.bytes);
        }
    }

    fn undo_pop(&mut self) {
        if let Some(snap) = self.undo.pop() {
            self.undo_bytes = self.undo_bytes.saturating_sub(snap.bytes);
            self.lines = snap.lines;
            self.cursor_line = snap.cursor_line.min(self.lines.len() - 1);
            self.cursor_col = snap
                .cursor_col
                .min(graphemes(&self.lines[self.cursor_line]).count());
        }
    }

    fn current_line(&self) -> &str {
        &self.lines[self.cursor_line]
    }

    fn cursor_byte(&self) -> usize {
        graphemes(self.current_line())
            .nth(self.cursor_col)
            .map(|(i, _)| i)
            .unwrap_or(self.current_line().len())
    }

    fn insert_str(&mut self, text: &str) {
        self.snapshot();
        self.insert_str_raw(text);
    }

    fn insert_str_raw(&mut self, text: &str) {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                // Split the current line at the cursor.
                let byte = self.cursor_byte();
                let tail = self.lines[self.cursor_line][byte..].to_string();
                self.lines[self.cursor_line].truncate(byte);
                self.lines.insert(self.cursor_line + 1, tail);
                self.cursor_line += 1;
                self.cursor_col = 0;
            }
            let byte = self.cursor_byte();
            self.lines[self.cursor_line].insert_str(byte, part);
            self.cursor_col += graphemes(part).count();
        }
        self.history_index = None;
    }

    fn newline(&mut self) {
        self.snapshot();
        self.insert_str_raw("\n");
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.snapshot();
            let byte = self.cursor_byte();
            let prev_byte = graphemes(self.current_line())
                .nth(self.cursor_col - 1)
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.lines[self.cursor_line].replace_range(prev_byte..byte, "");
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.snapshot();
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = graphemes(&self.lines[self.cursor_line]).count();
            self.lines[self.cursor_line].push_str(&current);
        }
    }

    fn delete_forward(&mut self) {
        let len = graphemes(self.current_line()).count();
        if self.cursor_col < len {
            self.snapshot();
            let byte = self.cursor_byte();
            let next_byte = graphemes(self.current_line())
                .nth(self.cursor_col + 1)
                .map(|(i, _)| i)
                .unwrap_or(self.current_line().len());
            self.lines[self.cursor_line].replace_range(byte..next_byte, "");
        } else if self.cursor_line + 1 < self.lines.len() {
            self.snapshot();
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next);
        }
    }

    fn move_word(&mut self, direction: i32) {
        let line = self.current_line().to_string();
        let clusters: Vec<(usize, &str)> = graphemes(&line).collect();
        if direction < 0 {
            // Find previous word start before cursor.
            let mut col = self.cursor_col;
            while col > 0 && clusters[col - 1].1.trim().is_empty() {
                col -= 1;
            }
            while col > 0 && !clusters[col - 1].1.trim().is_empty() {
                col -= 1;
            }
            self.cursor_col = col;
        } else {
            let mut col = self.cursor_col;
            while col < clusters.len() && !clusters[col].1.trim().is_empty() {
                col += 1;
            }
            while col < clusters.len() && clusters[col].1.trim().is_empty() {
                col += 1;
            }
            self.cursor_col = col;
        }
    }

    fn kill_to_line_end(&mut self) {
        self.snapshot();
        let byte = self.cursor_byte();
        let killed = self.lines[self.cursor_line][byte..].to_string();
        self.lines[self.cursor_line].truncate(byte);
        self.push_kill(killed);
    }

    fn kill_to_line_start(&mut self) {
        self.snapshot();
        let byte = self.cursor_byte();
        let killed = self.lines[self.cursor_line][..byte].to_string();
        self.lines[self.cursor_line].replace_range(..byte, "");
        self.cursor_col = 0;
        self.push_kill(killed);
    }

    fn kill_word_backward(&mut self) {
        self.snapshot();
        let from = {
            let line = self.current_line().to_string();
            let clusters: Vec<(usize, &str)> = graphemes(&line).collect();
            let mut col = self.cursor_col;
            while col > 0 && clusters[col - 1].1.trim().is_empty() {
                col -= 1;
            }
            while col > 0 && !clusters[col - 1].1.trim().is_empty() {
                col -= 1;
            }
            clusters.get(col).map(|(i, _)| *i).unwrap_or(0)
        };
        let to = self.cursor_byte();
        let killed = self.lines[self.cursor_line][from..to].to_string();
        self.lines[self.cursor_line].replace_range(from..to, "");
        self.cursor_col = graphemes(&self.lines[self.cursor_line][..from]).count();
        self.push_kill(killed);
    }

    fn push_kill(&mut self, text: String) {
        if text.is_empty() || text.len() > KILL_MAX_BYTES {
            return;
        }
        self.kill_ring.push(text);
        if self.kill_ring.len() > KILL_RING_MAX {
            self.kill_ring.remove(0);
        }
        self.kill_ring_index = self.kill_ring.len();
    }

    fn yank(&mut self) {
        if let Some(text) = self.kill_ring.last().cloned() {
            self.snapshot();
            self.insert_str_raw(&text);
            self.kill_ring_index = self.kill_ring.len();
        }
    }

    fn yank_pop(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }
        // Replace the last yank with the previous kill-ring entry.
        self.kill_ring_index =
            (self.kill_ring_index + self.kill_ring.len() - 1) % self.kill_ring.len();
        if let Some(text) = self.kill_ring.get(self.kill_ring_index).cloned() {
            self.undo_pop(); // undo the previous yank
            self.snapshot();
            self.insert_str_raw(&text);
        }
    }

    fn history_move(&mut self, direction: i32) {
        if self.history.is_empty() {
            return;
        }
        match direction {
            -1 => {
                let next = match self.history_index {
                    None => {
                        self.history_draft = self.lines.join("\n");
                        self.history.len() - 1
                    }
                    Some(i) if i > 0 => i - 1,
                    Some(i) => i,
                };
                self.history_index = Some(next);
                let text = self.history[next].clone();
                self.set_text(&text);
                self.history_index = Some(next);
            }
            1 => match self.history_index {
                Some(i) if i + 1 < self.history.len() => {
                    let text = self.history[i + 1].clone();
                    self.set_text(&text);
                    self.history_index = Some(i + 1);
                }
                Some(_) => {
                    let draft = self.history_draft.clone();
                    self.set_text(&draft);
                    self.history_index = None;
                }
                None => {}
            },
            _ => {}
        }
    }

    /// Bracketed paste: large pastes collapse to a `[paste #N …]` marker.
    pub fn handle_paste(&mut self, text: &str) {
        // Windows/HTML clipboards carry CRLF (occasionally lone CR);
        // without normalization every pasted line ends with an invisible
        // \r that leaks into the submitted text.
        let normalized;
        let text = if text.contains('\r') {
            normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            &normalized
        } else {
            text
        };
        let line_count = text.matches('\n').count() + 1;
        if line_count > 10 || text.len() > 1000 {
            self.snapshot();
            let id = self.next_paste_id;
            self.next_paste_id += 1;
            let paste = PasteBlock {
                id,
                text: text.to_string(),
            };
            let marker = paste_marker(&paste);
            self.pastes.push(paste);
            self.insert_str_raw(&marker);
        } else {
            self.insert_str(text);
        }
    }

    fn submit(&mut self) {
        let text = self.text();
        if text.trim().is_empty() {
            return;
        }
        self.add_to_history(&text);
        self.submitted = Some(text);
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.pastes.clear();
        self.undo.clear();
        self.undo_bytes = 0;
    }
}

fn graphemes(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.grapheme_indices(true)
}

fn paste_marker(paste: &PasteBlock) -> String {
    let lines = paste.text.matches('\n').count();
    if lines > 0 {
        format!("[paste #{} +{} lines]", paste.id, lines)
    } else {
        format!("[paste #{} {} chars]", paste.id, paste.text.len())
    }
}

impl Component for Editor {
    fn render(&mut self, width: u16) -> Vec<Line> {
        // Cells, not bytes: a non-ASCII (e.g. wide) gutter mismeasured by
        // .len() would skew every wrapped line.
        let gutter_w = crate::line::grapheme_width(&self.gutter) as u16;
        let inner = width.saturating_sub(gutter_w + self.padding_x * 2) as usize;
        let mut visual: Vec<(Line, bool)> = Vec::new(); // (line, is_gutter_row)
        let mut cursor_visual_row: Option<usize> = None;

        for (line_idx, text) in self.lines.iter().enumerate() {
            let cursor_here = self.focused && line_idx == self.cursor_line;
            let line = if cursor_here {
                // The cursor marker must be its own zero-width span: injected
                // into the text string it gets fragmented by grapheme-based
                // wrapping (its 5 visible chars count toward the width), the
                // pieces leak to the terminal as a dangling APC sequence, and
                // the terminal swallows all subsequent output (screen freeze).
                let byte = self.cursor_byte();
                Line::from_spans(vec![
                    Span::styled(text[..byte].to_string(), self.text_style),
                    Span::raw(crate::component::CURSOR_MARKER),
                    Span::styled(text[byte..].to_string(), self.text_style),
                ])
            } else {
                Line::styled(text.clone(), self.text_style)
            };
            let wrapped = line.wrap(inner.max(1));
            for (i, mut wrapped_line) in wrapped.into_iter().enumerate() {
                if cursor_here && cursor_visual_row.is_none() {
                    // Locate the marker row within the wrapped output
                    // (span-wise contains: Line::text() would allocate a
                    // concatenated copy per wrapped line).
                    if wrapped_line
                        .spans
                        .iter()
                        .any(|s| s.text.contains(crate::component::CURSOR_MARKER))
                    {
                        cursor_visual_row = Some(visual.len());
                    }
                }
                wrapped_line.spans.insert(
                    0,
                    Span::styled(
                        if i == 0 {
                            self.gutter.clone()
                        } else {
                            " ".repeat(gutter_w as usize)
                        },
                        self.gutter_style,
                    ),
                );
                visual.push((wrapped_line, i == 0));
            }
        }

        // Keep the cursor row visible within max_height.
        let max = self.max_height.max(1) as usize;
        let cursor_visual_row = cursor_visual_row.unwrap_or(0);
        if cursor_visual_row < self.scroll_offset {
            self.scroll_offset = cursor_visual_row;
        } else if cursor_visual_row >= self.scroll_offset + max {
            self.scroll_offset = cursor_visual_row + 1 - max;
        }
        visual
            .drain(self.scroll_offset.min(visual.len())..)
            .take(max)
            .map(|(l, _)| l)
            .collect()
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        match event {
            InputEvent::Paste(text) => {
                self.handle_paste(text);
                true
            }
            InputEvent::Key(key) => self.handle_key(key),
            _ => false,
        }
    }

    fn invalidate(&mut self) {}
}

impl Editor {
    fn handle_key(&mut self, key: &KeyEvent) -> bool {
        if key.is_release {
            return false;
        }
        // Navigation/editing table (TS `tui.editor.*` defaults).
        if key.matches("enter") && !key.modifiers.shift && !key.modifiers.ctrl && !key.modifiers.alt
        {
            self.submit();
            return true;
        }
        if key.matches("shift+enter")
            || key.matches("ctrl+enter")
            || key.matches("ctrl+j")
            || key.matches("alt+enter")
        {
            self.newline();
            return true;
        }
        if key.matches("backspace") {
            self.backspace();
            return true;
        }
        if key.matches("delete") {
            self.delete_forward();
            return true;
        }
        if key.matches("left") && !key.modifiers.ctrl && !key.modifiers.alt {
            if self.cursor_col > 0 {
                self.cursor_col -= 1;
            } else if self.cursor_line > 0 {
                self.cursor_line -= 1;
                self.cursor_col = graphemes(&self.lines[self.cursor_line]).count();
            }
            return true;
        }
        if key.matches("right") && !key.modifiers.ctrl && !key.modifiers.alt {
            let len = graphemes(self.current_line()).count();
            if self.cursor_col < len {
                self.cursor_col += 1;
            } else if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.cursor_col = 0;
            }
            return true;
        }
        if key.matches("ctrl+left") || key.matches("alt+left") || key.matches("alt+b") {
            self.move_word(-1);
            return true;
        }
        if key.matches("ctrl+right") || key.matches("alt+right") || key.matches("alt+f") {
            self.move_word(1);
            return true;
        }
        if key.matches("home") || key.matches("ctrl+a") {
            self.cursor_col = 0;
            return true;
        }
        if key.matches("end") || key.matches("ctrl+e") {
            self.cursor_col = graphemes(self.current_line()).count();
            return true;
        }
        if key.matches("ctrl+home") {
            self.cursor_line = 0;
            self.cursor_col = 0;
            return true;
        }
        if key.matches("ctrl+end") {
            self.cursor_line = self.lines.len() - 1;
            self.cursor_col = graphemes(self.current_line()).count();
            return true;
        }
        if key.matches("up") {
            if self.cursor_line == 0 {
                self.history_move(-1);
            } else {
                self.cursor_line -= 1;
                self.cursor_col = self
                    .cursor_col
                    .min(graphemes(&self.lines[self.cursor_line]).count());
            }
            return true;
        }
        if key.matches("down") {
            if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.cursor_col = self
                    .cursor_col
                    .min(graphemes(&self.lines[self.cursor_line]).count());
            } else {
                self.history_move(1);
            }
            return true;
        }
        if key.matches("ctrl+w") || key.matches("alt+backspace") {
            self.kill_word_backward();
            return true;
        }
        if key.matches("ctrl+u") {
            self.kill_to_line_start();
            return true;
        }
        if key.matches("ctrl+k") {
            self.kill_to_line_end();
            return true;
        }
        if key.matches("ctrl+y") {
            self.yank();
            return true;
        }
        if key.matches("alt+y") {
            self.yank_pop();
            return true;
        }
        if key.matches("ctrl+-") {
            self.undo_pop();
            return true;
        }
        if let KeyEvent {
            key: Key::Char(c),
            modifiers,
            is_release: false,
        } = *key
            && !modifiers.ctrl
            && !modifiers.alt
        {
            let mut buf = [0u8; 4];
            self.insert_str(c.encode_utf8(&mut buf));
            return true;
        }
        let _ = Modifiers::SHIFT;
        false
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn key(c: char) -> InputEvent {
        InputEvent::Key(KeyEvent::plain(Key::Char(c)))
    }

    #[test]
    fn history_capped_at_max_dropping_oldest() {
        let mut editor = Editor::new();
        for i in 0..(HISTORY_MAX + 10) {
            editor.add_to_history(&format!("prompt {i}"));
        }
        assert_eq!(editor.history().len(), HISTORY_MAX);
        // Oldest entries dropped; the most recent are kept in order.
        assert_eq!(editor.history()[0], "prompt 10");
        assert_eq!(
            editor.history().last().unwrap(),
            &format!("prompt {}", HISTORY_MAX + 9)
        );
        // Navigation still walks back from the most recent entry.
        editor.history_move(-1);
        assert_eq!(editor.text(), format!("prompt {}", HISTORY_MAX + 9));
        editor.history_move(-1);
        assert_eq!(editor.text(), format!("prompt {}", HISTORY_MAX + 8));
        // Consecutive duplicates are still coalesced, not counted twice.
        let before = editor.history().len();
        editor.add_to_history(editor.history().last().unwrap().clone().as_str());
        assert_eq!(editor.history().len(), before);
    }

    #[test]
    fn typing_and_submit() {
        let mut editor = Editor::new();
        editor.focused = true;
        for c in "hello".chars() {
            editor.handle_input(&key(c));
        }
        assert_eq!(editor.text(), "hello");
        editor.handle_input(&InputEvent::Key(KeyEvent::plain(Key::Enter)));
        assert_eq!(editor.submitted.as_deref(), Some("hello"));
        assert_eq!(editor.text(), "");
        // Entered text is in history; up at first line restores it.
        editor.handle_input(&InputEvent::Key(KeyEvent::plain(Key::Up)));
        assert_eq!(editor.text(), "hello");
    }

    #[test]
    fn multiline_editing() {
        let mut editor = Editor::new();
        editor.set_text("ab");
        editor.handle_input(&InputEvent::Key(KeyEvent::new(
            Key::Enter,
            Modifiers::SHIFT,
        )));
        editor.handle_input(&key('c'));
        assert_eq!(editor.text(), "ab\nc");
        editor.handle_input(&InputEvent::Key(KeyEvent::plain(Key::Up)));
        editor.handle_input(&InputEvent::Key(KeyEvent::plain(Key::End)));
        editor.handle_input(&InputEvent::Key(KeyEvent::plain(Key::Backspace)));
        assert_eq!(editor.text(), "a\nc");
    }

    #[test]
    fn kill_ring_and_undo() {
        let mut editor = Editor::new();
        editor.set_text("one two three");
        editor.handle_input(&InputEvent::Key(KeyEvent::ctrl(Key::Char('w'))));
        assert_eq!(editor.text(), "one two ");
        editor.handle_input(&InputEvent::Key(KeyEvent::ctrl(Key::Char('y'))));
        assert_eq!(editor.text(), "one two three");
        // One undo reverts the yank.
        editor.handle_input(&InputEvent::Key(KeyEvent::ctrl(Key::Char('-'))));
        assert_eq!(editor.text(), "one two ");
        // Second undo reverts the kill.
        editor.handle_input(&InputEvent::Key(KeyEvent::ctrl(Key::Char('-'))));
        assert_eq!(editor.text(), "one two three");
    }

    #[test]
    fn word_navigation() {
        let mut editor = Editor::new();
        editor.set_text("foo bar  baz");
        editor.cursor_col = 0;
        editor.move_word(1);
        assert_eq!(editor.cursor_col, 4); // start of "bar"
        editor.move_word(1);
        assert_eq!(editor.cursor_col, 9); // start of "baz" (double space)
        editor.move_word(-1);
        assert_eq!(editor.cursor_col, 4); // back to start of "bar"
    }

    #[test]
    fn wide_char_gutter_measured_in_cells_not_bytes() {
        // Regression: gutter width used .len() (bytes); a wide gutter like
        // "→ " (3 bytes, 2 cells) shrank the wrap width by one per line.
        let mut editor = Editor::new();
        editor.gutter = "→\u{3000}".to_string(); // 4 bytes, 3 cells
        editor.set_text("abcdefghij");
        let lines = editor.render(8); // inner = 8 - 3 = 5 cells
        let first = lines[0].text();
        let content = first.strip_prefix("→\u{3000}").unwrap_or(&first);
        assert_eq!(crate::line::grapheme_width(content), 5, "{first:?}");
        assert!(first.starts_with("→\u{3000}"), "{first:?}");
    }

    #[test]
    fn large_paste_becomes_marker() {
        let mut editor = Editor::new();
        let big = "line\n".repeat(20);
        editor.handle_input(&InputEvent::Paste(big.clone()));
        let shown = editor.lines.join("\n");
        assert!(shown.contains("[paste #1 +20 lines]"), "{shown}");
        assert_eq!(editor.text(), big);
    }

    #[test]
    fn paste_normalizes_crlf_and_lone_cr() {
        // Small paste: inserted directly, line endings normalized.
        let mut editor = Editor::new();
        editor.handle_input(&InputEvent::Paste("a\r\nb\rc".to_string()));
        assert_eq!(editor.text(), "a\nb\nc");

        // Large CRLF paste: the marker counts normalized lines and the
        // expanded text carries no \r.
        let mut editor = Editor::new();
        let big = "line\r\n".repeat(20);
        editor.handle_input(&InputEvent::Paste(big));
        let shown = editor.lines.join("\n");
        assert!(shown.contains("[paste #1 +20 lines]"), "{shown}");
        assert_eq!(editor.text(), "line\n".repeat(20));
    }

    /// After a large paste, every editing command snapshots the whole
    /// content — the undo byte budget must bound total retention even
    /// below the 200-snapshot count cap.
    #[test]
    fn undo_byte_budget_bounds_large_paste() {
        let mut editor = Editor::new();
        editor.set_text(&"x".repeat(600 * 1024)); // ~600 KB in the editor
        for c in 'a'..='z' {
            editor.handle_input(&key(c));
        }
        assert!(
            editor.undo_bytes <= UNDO_MAX_BYTES + 600 * 1024 + 1024,
            "undo retained {} bytes",
            editor.undo_bytes
        );
        assert!(editor.undo.len() < 26, "byte budget evicted oldest first");
        // Undo still works (the newest snapshot survives).
        let before = editor.text();
        editor.undo_pop();
        assert_ne!(editor.text(), before);
    }

    #[test]
    fn kill_ring_capped_and_giant_kills_dropped() {
        let mut editor = Editor::new();
        // More kills than KILL_RING_MAX: ring stays bounded, newest kept.
        for i in 0..(KILL_RING_MAX + 10) {
            editor.set_text(&format!("kill me {i}"));
            editor.kill_to_line_start();
        }
        assert_eq!(editor.kill_ring.len(), KILL_RING_MAX);
        assert!(
            editor
                .kill_ring
                .last()
                .unwrap()
                .contains(&format!("{}", KILL_RING_MAX + 9))
        );
        // A giant kill is not retained at all.
        let before = editor.kill_ring.len();
        editor.set_text(&"y".repeat(KILL_MAX_BYTES + 1));
        editor.kill_to_line_start();
        assert_eq!(editor.kill_ring.len(), before);
    }

    #[test]
    fn history_byte_budget_drops_huge_oldest() {
        let mut editor = Editor::new();
        let huge = "h".repeat(HISTORY_MAX_BYTES + 1024);
        editor.add_to_history(&huge);
        // A lone entry stays even when it alone exceeds the budget.
        assert_eq!(editor.history().len(), 1);
        editor.add_to_history("small");
        // ...but a second push evicts it to get back under the budget.
        assert_eq!(editor.history(), &["small"]);
        editor.add_to_history(&huge);
        assert_eq!(editor.history().len(), 1);
        assert_eq!(editor.history()[0], huge);
        assert_eq!(editor.history_bytes, huge.len() + 24);
    }
}
