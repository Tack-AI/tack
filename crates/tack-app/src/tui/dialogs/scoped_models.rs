//! Multi-toggle checkbox dialog for /scoped-models (extracted from
//! `commands.rs`; pure move, no behavior change).

use tack_tui::components::select_list::SelectItem;
use tack_tui::{InputEvent, Key, KeyEvent, Line, Span, Style};

use crate::tui::theme::Theme;

/// Multi-toggle checkbox dialog for /scoped-models (TS scoped-models-selector:
/// enter toggles, esc finishes).
#[derive(Debug)]
pub struct ScopedModelsDialog {
    items: Vec<(SelectItem, bool)>,
    selected: usize,
    filter: String,
    filtered: Vec<usize>,
    pub(super) done: bool,
    theme: Theme,
}

impl ScopedModelsDialog {
    pub fn new(items: Vec<(SelectItem, bool)>, theme: Theme) -> Self {
        let filtered: Vec<usize> = (0..items.len()).collect();
        ScopedModelsDialog {
            items,
            selected: 0,
            filter: String::new(),
            filtered,
            done: false,
            theme,
        }
    }

    /// Enabled model values (applied on close).
    pub fn enabled_values(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(item, _)| item.value.clone())
            .collect()
    }

    fn apply_filter(&mut self) {
        let query = self.filter.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, (item, _))| {
                query.is_empty()
                    || tack_tui::components::select_list::fuzzy_match(
                        &query,
                        &item.label.to_lowercase(),
                    )
            })
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
    }

    pub(super) fn render(&mut self, width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut title = Line::new();
        title.push(Span::styled(
            crate::i18n::tr("dialog.scoped_models.title"),
            self.theme.accent.bold(),
        ));
        if !self.filter.is_empty() {
            title.push(Span::styled(
                format!("  {}{}", crate::i18n::tr("dialog.filter"), self.filter),
                self.theme.dim,
            ));
        }
        title.truncate(width as usize, true);
        lines.push(title);
        let max = 12usize;
        let start = if self.selected >= max {
            self.selected + 1 - max
        } else {
            0
        };
        for (pos, item_index) in self.filtered.iter().enumerate().skip(start).take(max) {
            let (item, enabled) = &self.items[*item_index];
            let is_selected = pos == self.selected;
            let mut line = Line::new();
            line.push(Span::plain(if is_selected { "→ " } else { "  " }));
            line.push(Span::styled(
                if *enabled { "[x] " } else { "[ ] " },
                if *enabled {
                    self.theme.success
                } else {
                    self.theme.dim
                },
            ));
            line.push(Span::styled(
                item.label.clone(),
                if is_selected {
                    Style::new().bold()
                } else {
                    Style::default()
                },
            ));
            if let Some(desc) = &item.description {
                line.push(Span::styled(format!("  {desc}"), self.theme.dim));
            }
            line.truncate(width as usize, false);
            lines.push(line);
        }
        lines.push(Line::styled(
            crate::i18n::tr("dialog.hint.scoped"),
            self.theme.dim,
        ));
        lines
    }

    pub(super) fn handle_input(&mut self, event: &InputEvent) -> bool {
        if let InputEvent::Key(KeyEvent {
            key: Key::Char(c),
            modifiers,
            is_release: false,
        }) = event
            && !modifiers.ctrl
            && !modifiers.alt
        {
            self.filter.push(*c);
            self.apply_filter();
            return true;
        }
        let InputEvent::Key(key) = event else {
            return false;
        };
        if key.matches("backspace") && !self.filter.is_empty() {
            self.filter.pop();
            self.apply_filter();
            return true;
        }
        if key.matches("escape") {
            self.done = true;
            return true;
        }
        if key.matches("up") {
            if self.selected > 0 {
                self.selected -= 1;
            }
            return true;
        }
        if key.matches("down") {
            if self.selected + 1 < self.filtered.len() {
                self.selected += 1;
            }
            return true;
        }
        if key.matches("enter") || key.matches(" ") {
            if let Some(index) = self.filtered.get(self.selected) {
                self.items[*index].1 = !self.items[*index].1;
            }
            return true;
        }
        false
    }
}
