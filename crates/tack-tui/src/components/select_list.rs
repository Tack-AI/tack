//! SelectList: a scrollable list of selectable options (port of
//! `select-list.ts`), used by every selector dialog.

use crate::component::Component;
use crate::input::InputEvent;
use crate::line::{Line, Span};
use crate::style::{Color, Style};

#[derive(Clone, Debug)]
pub struct SelectItem {
    pub label: String,
    pub description: Option<String>,
    /// Opaque value returned on selection.
    pub value: String,
}

impl SelectItem {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        SelectItem {
            label: label.into(),
            description: None,
            value: value.into(),
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

#[derive(Debug)]
pub struct SelectList {
    pub items: Vec<SelectItem>,
    /// Indices into `items`, after filtering.
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub max_visible: u16,
    pub selected_style: Style,
    pub description_style: Style,
    /// Optional fuzzy filter query.
    pub filter: String,
    /// Set by handle_input on Enter; cleared by the caller.
    pub on_confirm: Option<String>,
    /// Set on Escape.
    pub cancelled: bool,
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>) -> Self {
        let filtered: Vec<usize> = (0..items.len()).collect();
        SelectList {
            items,
            filtered,
            selected: 0,
            max_visible: 10,
            selected_style: Style::new().bg(Color::Rgb(60, 60, 90)),
            description_style: Style::new().dim(),
            filter: String::new(),
            on_confirm: None,
            cancelled: false,
        }
    }

    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items;
        self.apply_filter();
        self.selected = 0;
    }

    pub fn set_filter(&mut self, filter: impl Into<String>) {
        self.filter = filter.into();
        self.apply_filter();
        self.selected = 0;
    }

    fn apply_filter(&mut self) {
        let query = self.filter.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                query.is_empty()
                    || crate::components::select_list::fuzzy_match(
                        &query,
                        &item.label.to_lowercase(),
                    )
            })
            .map(|(i, _)| i)
            .collect();
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered.get(self.selected).map(|i| &self.items[*i])
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as i32;
        self.selected = ((self.selected as i32 + delta).rem_euclid(len)) as usize;
    }
}

/// Subsequence fuzzy match (case-folded by the caller).
pub fn fuzzy_match(query: &str, candidate: &str) -> bool {
    let mut chars = candidate.chars();
    query.chars().all(|q| chars.by_ref().any(|c| c == q))
}

impl Component for SelectList {
    fn render(&mut self, width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        if self.filtered.is_empty() {
            lines.push(Line::styled("  (no matches)", self.description_style));
            return lines;
        }
        // Scroll window around the selection.
        let max = self.max_visible.max(1) as usize;
        let start = if self.selected >= max {
            self.selected + 1 - max
        } else {
            0
        };
        for (pos, item_index) in self.filtered.iter().enumerate().skip(start).take(max) {
            let item = &self.items[*item_index];
            let is_selected = pos == self.selected;
            let mut line = Line::new();
            let marker = if is_selected { "→ " } else { "  " };
            line.push(Span::plain(marker));
            line.push(Span::styled(
                item.label.clone(),
                if is_selected {
                    Style::new().bold()
                } else {
                    Style::default()
                },
            ));
            if let Some(description) = &item.description {
                line.push(Span::styled(
                    format!("  {description}"),
                    self.description_style,
                ));
            }
            line.truncate(width as usize, false);
            line.pad_right(
                width as usize,
                if is_selected {
                    self.selected_style
                } else {
                    Style::default()
                },
            );
            if is_selected {
                for span in &mut line.spans {
                    span.style = span.style.merged_with(&self.selected_style);
                }
            }
            lines.push(line);
        }
        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        if key.matches("up") {
            self.move_selection(-1);
            return true;
        }
        if key.matches("down") {
            self.move_selection(1);
            return true;
        }
        if key.matches("pageup") {
            self.move_selection(-(self.max_visible as i32));
            return true;
        }
        if key.matches("pagedown") {
            self.move_selection(self.max_visible as i32);
            return true;
        }
        if key.matches("enter") {
            self.on_confirm = self.selected_item().map(|i| i.value.clone());
            return true;
        }
        if key.matches("escape") {
            self.cancelled = true;
            return true;
        }
        false
    }
}

impl Default for SelectList {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}
