//! Multi-select checkbox dialog for ask_user questions with
//! `multi_select: true` (single-pick questions use the filterable
//! `SelectDialog` in `mod.rs`).

use tack_tui::components::select_list::SelectItem;
use tack_tui::{InputEvent, Line, Span, Style};

use crate::tui::theme::Theme;

/// Checkbox dialog: space toggles the highlighted option, enter confirms
/// the checked labels (requires at least one), esc cancels the batch.
/// No filter — multiple-choice questions offer at most four options, and
/// no "Other…" escape: a pick-several answer cannot mix in free text.
#[derive(Debug)]
pub struct MultiSelectDialog {
    title: String,
    items: Vec<SelectItem>,
    checked: Vec<bool>,
    cursor: usize,
    pub(super) done: bool,
    pub(super) cancelled: bool,
    /// Checked labels joined with ", " (item order, not toggle order), set
    /// once on confirm and taken by `Dialog::take_result`.
    pub(super) on_confirm: Option<String>,
    theme: Theme,
}

impl MultiSelectDialog {
    pub fn new(title: impl Into<String>, items: Vec<SelectItem>, theme: Theme) -> Self {
        let checked = vec![false; items.len()];
        MultiSelectDialog {
            title: title.into(),
            items,
            checked,
            cursor: 0,
            done: false,
            cancelled: false,
            on_confirm: None,
            theme,
        }
    }

    /// Checked labels in item order — the answer reported to the tool.
    fn checked_labels(&self) -> Vec<&str> {
        self.items
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(item, _)| item.label.as_str())
            .collect()
    }

    pub(super) fn render(&mut self, width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut title = Line::new();
        title.push(Span::styled(
            format!(" {}", self.title),
            self.theme.accent.bold(),
        ));
        title.truncate(width as usize, true);
        lines.push(title);
        for (i, item) in self.items.iter().enumerate() {
            let highlighted = i == self.cursor;
            let checked = self.checked[i];
            let mut line = Line::new();
            line.push(Span::plain(if highlighted { "→ " } else { "  " }));
            line.push(Span::styled(
                if checked { "[x] " } else { "[ ] " },
                if checked {
                    self.theme.success
                } else {
                    self.theme.dim
                },
            ));
            line.push(Span::styled(
                item.label.clone(),
                if highlighted {
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
            crate::i18n::tr("dialog.hint.multiselect"),
            self.theme.dim,
        ));
        lines
    }

    pub(super) fn handle_input(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        if key.is_release {
            return true;
        }
        if key.matches("escape") {
            self.cancelled = true;
            self.done = true;
        } else if key.matches("up") {
            self.cursor = self.cursor.saturating_sub(1);
        } else if key.matches("down") {
            if self.cursor + 1 < self.items.len() {
                self.cursor += 1;
            }
        } else if key.matches(" ") {
            if let Some(checked) = self.checked.get_mut(self.cursor) {
                *checked = !*checked;
            }
        } else if key.matches("enter") {
            // Stay open until at least one option is checked: an empty
            // pick-several answer would read like a dismissal.
            let labels = self.checked_labels();
            if !labels.is_empty() {
                self.on_confirm = Some(labels.join(", "));
                self.done = true;
            }
        }
        // No filter: any other key (including typed characters) is ignored.
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tack_tui::{Key, KeyEvent};

    fn dialog() -> MultiSelectDialog {
        let items = vec![
            SelectItem::new("Alpha", "Alpha"),
            SelectItem::new("Beta", "Beta"),
            SelectItem::new("Gamma", "Gamma"),
        ];
        MultiSelectDialog::new("pick", items, Theme::default())
    }

    fn key(key: Key) -> InputEvent {
        InputEvent::Key(KeyEvent::plain(key))
    }

    #[test]
    fn toggle_and_confirm_reports_checked_in_item_order() {
        let mut d = dialog();
        // Confirm with nothing checked stays open.
        d.handle_input(&key(Key::Enter));
        assert!(!d.done);
        // Check Gamma first, then Alpha: the answer follows item order.
        d.handle_input(&key(Key::Down));
        d.handle_input(&key(Key::Down));
        d.handle_input(&key(Key::Char(' ')));
        d.handle_input(&key(Key::Up));
        d.handle_input(&key(Key::Up));
        d.handle_input(&key(Key::Char(' ')));
        d.handle_input(&key(Key::Enter));
        assert!(d.done);
        assert!(!d.cancelled);
        assert_eq!(d.on_confirm.as_deref(), Some("Alpha, Gamma"));
    }

    #[test]
    fn space_toggles_off_again() {
        let mut d = dialog();
        d.handle_input(&key(Key::Char(' ')));
        d.handle_input(&key(Key::Char(' ')));
        d.handle_input(&key(Key::Enter));
        assert!(!d.done);
    }

    #[test]
    fn escape_cancels_without_a_result() {
        let mut d = dialog();
        d.handle_input(&key(Key::Char(' ')));
        d.handle_input(&key(Key::Escape));
        assert!(d.done);
        assert!(d.cancelled);
        assert!(d.on_confirm.is_none());
    }
}
