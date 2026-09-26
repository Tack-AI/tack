//! /model selector dialog (extracted from `commands.rs`; pure move,
//! no behavior change).

use tack_tui::{InputEvent, Key, Line, Span};

use crate::tui::theme::Theme;

/// One row of the model selector (TS ModelItem).
#[derive(Clone, Debug)]
pub struct ModelEntry {
    pub provider: String,
    pub id: String,
    pub name: String,
}

impl ModelEntry {
    pub(crate) fn qualified(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// `/model` selector (TS ModelSelectorComponent): available models only,
/// current-first sort, all/scoped scope toggle on Tab, fuzzy search with
/// "default" keyword, ✓ current / · default badges, Ctrl+S saves default.
#[derive(Debug)]
pub struct ModelDialog {
    search: String,
    all: Vec<ModelEntry>,
    scoped: Vec<ModelEntry>,
    scope_scoped: bool,
    default_ref: Option<String>,
    current_ref: String,
    filtered: Vec<usize>,
    selected: usize,
    pub(super) done: bool,
    pub(super) cancelled: bool,
    pub(super) on_confirm: Option<String>,
    pub(super) save_default: bool,
    theme: Theme,
}

impl ModelDialog {
    pub fn new(
        all: Vec<ModelEntry>,
        scoped: Vec<ModelEntry>,
        current_ref: String,
        default_ref: Option<String>,
        theme: Theme,
    ) -> Self {
        let sort = |items: &mut Vec<ModelEntry>| {
            items.sort_by(|a, b| {
                let a_cur = a.qualified() == current_ref;
                let b_cur = b.qualified() == current_ref;
                if a_cur != b_cur {
                    return if a_cur {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    };
                }
                let a_def = default_ref.as_deref() == Some(a.qualified().as_str());
                let b_def = default_ref.as_deref() == Some(b.qualified().as_str());
                if a_def != b_def {
                    return if a_def {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    };
                }
                a.provider.cmp(&b.provider)
            });
        };
        let mut all = all;
        let mut scoped = scoped;
        sort(&mut all);
        sort(&mut scoped);
        let scope_scoped = !scoped.is_empty();
        let mut dialog = ModelDialog {
            search: String::new(),
            all,
            scoped,
            scope_scoped,
            default_ref,
            current_ref,
            filtered: Vec::new(),
            selected: 0,
            done: false,
            cancelled: false,
            on_confirm: None,
            save_default: false,
            theme,
        };
        dialog.refilter();
        // Initial selection: the current model (TS).
        let active = dialog.active();
        if let Some(pos) = active
            .iter()
            .position(|e| e.qualified() == dialog.current_ref)
            .and_then(|i| dialog.filtered.iter().position(|&fi| fi == i))
        {
            dialog.selected = pos;
        }
        dialog
    }

    fn active(&self) -> &[ModelEntry] {
        if self.scope_scoped {
            &self.scoped
        } else {
            &self.all
        }
    }

    fn is_default(&self, entry: &ModelEntry) -> bool {
        self.default_ref.as_deref() == Some(entry.qualified().as_str())
    }

    fn refilter(&mut self) {
        let query = self.search.trim().to_lowercase();
        let active = self.active();
        if query.is_empty() {
            self.filtered = (0..active.len()).collect();
        } else {
            let mut matched: Vec<usize> = active
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    let default_text = if self.is_default(e) { " default" } else { "" };
                    let text =
                        format!("{}/{} {}{default_text}", e.provider, e.id, e.name).to_lowercase();
                    tack_tui::components::select_list::fuzzy_match(&query, &text)
                })
                .map(|(i, _)| i)
                .collect();
            // "default" keyword: the default model leads the results (TS).
            if "default".starts_with(&query) {
                matched.sort_by_key(|&i| usize::from(!self.is_default(&active[i])));
            }
            self.filtered = matched;
        }
        self.selected = if query.is_empty() {
            self.selected.min(self.filtered.len().saturating_sub(1))
        } else {
            0
        };
    }

    pub fn render(&mut self, _width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        lines.push(Line::styled(
            crate::i18n::tr("modeld.title"),
            self.theme.accent.bold(),
        ));
        if !self.scoped.is_empty() {
            let (all_style, scoped_style) = if self.scope_scoped {
                (self.theme.muted, self.theme.accent)
            } else {
                (self.theme.accent, self.theme.muted)
            };
            let mut scope = Line::new();
            scope.push(Span::styled(
                crate::i18n::tr("modeld.scope"),
                self.theme.muted,
            ));
            scope.push(Span::styled(crate::i18n::tr("modeld.scope_all"), all_style));
            scope.push(Span::styled(" | ", self.theme.muted));
            scope.push(Span::styled(
                crate::i18n::tr("modeld.scope_scoped"),
                scoped_style,
            ));
            scope.push(Span::styled(
                crate::i18n::tr("modeld.scope_hint"),
                self.theme.dim,
            ));
            lines.push(scope);
        } else {
            lines.push(Line::styled(
                crate::i18n::tr("modeld.only_configured"),
                self.theme.warning,
            ));
        }
        if !self.search.is_empty() {
            lines.push(Line::styled(
                format!(" {}{}", crate::i18n::tr("dialog.filter"), self.search),
                self.theme.dim,
            ));
        }

        let active = self.active();
        const MAX_VISIBLE: usize = 10;
        let len = self.filtered.len();
        let start = if len > MAX_VISIBLE {
            self.selected
                .saturating_sub(MAX_VISIBLE / 2)
                .min(len - MAX_VISIBLE)
        } else {
            0
        };
        let end = (start + MAX_VISIBLE).min(len);
        for pos in start..end {
            let entry = &active[self.filtered[pos]];
            let is_selected = pos == self.selected;
            let mut line = Line::new();
            let text_style = if is_selected {
                self.theme.accent
            } else {
                self.theme.text
            };
            line.push(Span::styled(
                if is_selected { "→ " } else { "  " },
                text_style,
            ));
            line.push(Span::styled(entry.id.clone(), text_style));
            line.push(Span::styled(
                format!(" [{}]", entry.provider),
                self.theme.muted,
            ));
            if self.is_default(entry) {
                line.push(Span::styled(
                    crate::i18n::tr("modeld.default_badge"),
                    self.theme.muted,
                ));
            }
            if entry.qualified() == self.current_ref {
                line.push(Span::styled(" ✓", self.theme.success));
            }
            lines.push(line);
        }
        if start > 0 || end < len {
            lines.push(Line::styled(
                format!("  ({}/{len})", self.selected + 1),
                self.theme.muted,
            ));
        }
        if len == 0 {
            lines.push(Line::styled(
                crate::i18n::tr("modeld.no_match"),
                self.theme.muted,
            ));
        } else if let Some(&fi) = self.filtered.get(self.selected) {
            lines.push(Line::new());
            lines.push(Line::styled(
                crate::i18n::trf("modeld.model_name", &[("name", &active[fi].name)]),
                self.theme.muted,
            ));
        }
        lines.push(Line::styled(crate::i18n::tr("modeld.hint"), self.theme.dim));
        lines
    }

    pub fn handle_input(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        if key.matches("tab") {
            if !self.scoped.is_empty() {
                self.scope_scoped = !self.scope_scoped;
                self.selected = 0;
                self.refilter();
            }
            return true;
        }
        if key.matches("up") {
            if !self.filtered.is_empty() {
                self.selected = if self.selected == 0 {
                    self.filtered.len() - 1
                } else {
                    self.selected - 1
                };
            }
            return true;
        }
        if key.matches("down") {
            if !self.filtered.is_empty() {
                self.selected = (self.selected + 1) % self.filtered.len();
            }
            return true;
        }
        if key.matches("enter") {
            if let Some(&fi) = self.filtered.get(self.selected) {
                self.on_confirm = Some(self.active()[fi].qualified());
                self.done = true;
            }
            return true;
        }
        if key.matches("ctrl+s") {
            if let Some(&fi) = self.filtered.get(self.selected) {
                self.on_confirm = Some(self.active()[fi].qualified());
                self.save_default = true;
                self.done = true;
            }
            return true;
        }
        if key.matches("escape") {
            self.cancelled = true;
            self.done = true;
            return true;
        }
        match key.key {
            Key::Char(c) if !key.modifiers.ctrl && !key.modifiers.alt => {
                self.search.push(c);
                self.refilter();
                true
            }
            Key::Backspace => {
                self.search.pop();
                self.refilter();
                true
            }
            _ => false,
        }
    }
}
