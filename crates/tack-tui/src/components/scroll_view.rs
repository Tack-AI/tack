//! ScrollView: a scrollable region over child content (port of
//! `scroll-view.ts` essentials: follow-end, page/line scrolling, scrollbar).

use crate::component::Component;
use crate::input::{InputEvent, MouseEventKind};
use crate::line::{Line, Span};
use crate::style::Style;

/// A scrollable viewport over a list of lines (or child components).
#[derive(Debug, Default)]
pub struct ScrollView {
    /// Content source: lines produced by children (concatenated).
    pub children: Vec<Box<dyn Component>>,
    /// Follow the end when content grows (chat behavior).
    pub follow_end: bool,
    /// First visible line index into the content.
    pub scroll_top: usize,
    /// Viewport height (set from render; full content height when 0 = auto).
    pub viewport_height: u16,
    pub scrollbar: bool,
    pub scrollbar_style: Option<Style>,
    content: Vec<Line>,
    /// Virtual content: absolute line index of `content[0]` (0 for full
    /// content). Lets the app materialize only a visible window of a huge
    /// transcript while scroll positions/scrollbar stay absolute.
    content_top: usize,
    /// Virtual total line count (None = the window IS the full content).
    total_override: Option<usize>,
}

impl ScrollView {
    pub fn new() -> Self {
        ScrollView {
            follow_end: true,
            ..Default::default()
        }
    }

    /// Replace content directly (no children).
    pub fn set_content(&mut self, lines: Vec<Line>) {
        self.content = lines;
        self.content_top = 0;
        self.total_override = None;
        self.children.clear();
        self.invalidate();
    }

    /// Replace content with a materialized window of a larger virtual
    /// document: `lines` covers absolute lines `[top, top + lines.len())`
    /// out of `total`. Rendering/scrollbar use the virtual total; ranges
    /// outside the window render as blank rows.
    pub fn set_virtual_content(&mut self, total: usize, top: usize, lines: Vec<Line>) {
        self.content = lines;
        self.content_top = top;
        self.total_override = Some(total);
        self.children.clear();
        self.invalidate();
    }

    /// Total lines of the (possibly virtual) document.
    pub fn total_lines(&self) -> usize {
        self.total_override.unwrap_or(self.content.len())
    }

    pub fn content_height(&self) -> usize {
        self.total_lines()
    }

    /// The current content lines (for search/selection mapping).
    pub fn content_lines(&self) -> &[Line] {
        &self.content
    }

    pub fn scroll_to_end(&mut self) {
        self.scroll_top = usize::MAX; // clamped at render
        self.follow_end = true;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let max = self.max_scroll();
        let new = self.scroll_top as i64 + delta as i64;
        self.scroll_top = new.clamp(0, max as i64) as usize;
        if delta != 0 {
            self.follow_end = false;
        }
        if self.scroll_top >= max {
            self.follow_end = true;
        }
    }

    fn max_scroll(&self) -> usize {
        self.total_lines()
            .saturating_sub(self.viewport_height as usize)
    }

    fn rebuild(&mut self, width: u16) {
        if !self.children.is_empty() {
            self.content.clear();
            for child in &mut self.children {
                self.content.extend(child.render(width));
            }
        }
    }
}

impl Component for ScrollView {
    fn render(&mut self, width: u16) -> Vec<Line> {
        self.rebuild(width);
        let height = self.viewport_height as usize;
        if height == 0 {
            return self.content.clone();
        }
        let max_scroll = self.max_scroll();
        if self.follow_end || self.scroll_top > max_scroll {
            self.scroll_top = max_scroll;
        }
        // Slice the visible range out of the (possibly windowed) content.
        let start = self.scroll_top.saturating_sub(self.content_top);
        let mut visible: Vec<Line> = if start < self.content.len() {
            self.content[start..].iter().take(height).cloned().collect()
        } else {
            Vec::new()
        };
        // Pad short frames so old content below is overwritten.
        while visible.len() < height {
            visible.push(Line::new());
        }
        if self.scrollbar && self.total_lines() > height {
            let thumb_style = self.scrollbar_style.unwrap_or_else(|| Style::new().dim());
            let track = height;
            let thumb_size = if self.total_lines() == 0 {
                1
            } else {
                ((track * track) / self.total_lines()).max(1).min(track)
            };
            let thumb_top = if max_scroll == 0 {
                0
            } else {
                (self.scroll_top * (track - thumb_size))
                    .checked_div(max_scroll)
                    .unwrap_or(0)
            };
            for (i, line) in visible.iter_mut().enumerate() {
                let in_thumb = i >= thumb_top && i < thumb_top + thumb_size;
                let cell = if in_thumb { "█" } else { "│" };
                let style = if in_thumb {
                    thumb_style
                } else {
                    Style::new().dim()
                };
                let width_cells = line.width();
                let pad = (width as usize).saturating_sub(width_cells + 1);
                line.push(Span::plain(" ".repeat(pad)));
                line.push(Span::styled(cell, style));
            }
        }
        visible
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        match event {
            InputEvent::Key(key) if key.matches("pageup") => {
                self.scroll_by(-(self.viewport_height as i32).max(1));
                true
            }
            InputEvent::Key(key) if key.matches("pagedown") => {
                self.scroll_by(self.viewport_height as i32);
                true
            }
            InputEvent::Key(key) if key.matches("home") => {
                self.scroll_top = 0;
                self.follow_end = false;
                true
            }
            InputEvent::Key(key) if key.matches("end") => {
                self.scroll_to_end();
                true
            }
            InputEvent::Mouse(mouse) => match mouse.kind {
                // TS pi #9166: Alt-modified wheel scrolls 5× faster.
                MouseEventKind::ScrollUp => {
                    let lines = if mouse.modifiers.alt { 15 } else { 3 };
                    self.scroll_by(-lines);
                    true
                }
                MouseEventKind::ScrollDown => {
                    let lines = if mouse.modifiers.alt { 15 } else { 3 };
                    self.scroll_by(lines);
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}
