//! Modal dialog components (extracted from `commands.rs`; pure move,
//! no behavior change). Re-exports keep the old `tui::commands::*` paths
//! working via `commands`' re-export.

use tack_tui::Component as _;
use tack_tui::components::select_list::{SelectItem, SelectList};
use tack_tui::{InputEvent, Key, KeyEvent, Line, Span};

use crate::tui::permission::PermissionDialog;
use crate::tui::theme::Theme;

mod model;
mod scoped_models;
mod session;
mod tree;

pub use model::{ModelDialog, ModelEntry};
pub use scoped_models::ScopedModelsDialog;
pub use session::SessionDialog;
pub use tree::{TreeDialog, TreeFilter, build_tree_items, build_tree_items_filtered};

/// Active modal dialog.
#[derive(Debug)]
pub enum Dialog {
    Permission(PermissionDialog),
    Select(SelectDialog),
    Model(ModelDialog),
    ScopedModels(ScopedModelsDialog),
    Sessions(SessionDialog),
    Tree(TreeDialog),
    Input(InputDialog),
}

impl Dialog {
    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<Line> {
        match self {
            Dialog::Permission(d) => {
                let out = d.render(width);
                let _ = theme;
                out
            }
            Dialog::Select(d) => d.render(width),
            Dialog::Model(d) => d.render(width),
            Dialog::ScopedModels(d) => d.render(width),
            Dialog::Sessions(d) => d.render(width),
            Dialog::Tree(d) => d.render(width),
            Dialog::Input(d) => d.render(width),
        }
    }

    /// Returns true when the event was consumed.
    pub fn handle_input(&mut self, event: &InputEvent) -> bool {
        match self {
            Dialog::Permission(d) => d.handle_input(event),
            Dialog::Select(d) => d.handle_input(event),
            Dialog::Model(d) => d.handle_input(event),
            Dialog::ScopedModels(d) => d.handle_input(event),
            Dialog::Sessions(d) => d.handle_input(event),
            Dialog::Tree(d) => d.handle_input(event),
            Dialog::Input(d) => d.handle_input(event),
        }
    }

    pub fn done(&self) -> bool {
        match self {
            Dialog::Permission(d) => d.resolved(),
            Dialog::Select(d) => d.done,
            Dialog::Model(d) => d.done,
            Dialog::ScopedModels(d) => d.done,
            Dialog::Sessions(d) => d.done,
            Dialog::Tree(d) => d.done,
            Dialog::Input(d) => d.done,
        }
    }

    /// The confirmed value (select dialogs), taken once.
    pub fn take_result(&mut self) -> Option<(SelectPurpose, String, bool)> {
        match self {
            Dialog::Select(d) => d.on_confirm.take().map(|v| (d.purpose, v, d.save_default)),
            Dialog::Model(d) => d
                .on_confirm
                .take()
                .map(|v| (SelectPurpose::Model, v, d.save_default)),
            Dialog::Sessions(d) => d
                .on_confirm
                .take()
                .map(|v| (SelectPurpose::Resume, v, false)),
            Dialog::Tree(d) => d.on_confirm.take().map(|v| (SelectPurpose::Fork, v, false)),
            Dialog::Input(d) => d
                .on_confirm
                .take()
                .map(|v| (SelectPurpose::ExtUi, v, false)),
            _ => None,
        }
    }

    pub fn cancelled(&self) -> bool {
        match self {
            Dialog::Select(d) => d.cancelled,
            Dialog::Model(d) => d.cancelled,
            Dialog::Sessions(d) => d.cancelled,
            Dialog::Tree(d) => d.cancelled,
            Dialog::Input(d) => d.cancelled,
            _ => false,
        }
    }
}

/// What a select dialog's result means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectPurpose {
    Model,
    Thinking,
    Resume,
    Fork,
    Logout,
    SettingsCategory,
    /// Value payload carries `<setting>=<value>` — apply directly.
    SettingsValue,
    /// Value payload is the trust option id (trust/parent/trust-session/distrust/distrust-session).
    ProjectTrust,
    /// Value payload: `r\t<conn>\t<uri>` (resource) or `p\t<conn>\t<name>` (prompt).
    McpPick,
    /// First-run wizard: theme choice (value = theme name).
    FirstRunTheme,
    /// tack-ext plugin dialog (answer goes to pending_ext_ui, not apply_select).
    ExtUi,
}

/// Minimal text-input dialog (plugin `ui.input`; also usable generally).
#[derive(Debug)]
pub struct InputDialog {
    title: String,
    input: String,
    placeholder: String,
    done: bool,
    cancelled: bool,
    on_confirm: Option<String>,
    theme: Theme,
}

impl InputDialog {
    pub fn new(title: impl Into<String>, placeholder: impl Into<String>, theme: Theme) -> Self {
        InputDialog {
            title: title.into(),
            input: String::new(),
            placeholder: placeholder.into(),
            done: false,
            cancelled: false,
            on_confirm: None,
            theme,
        }
    }

    pub fn render(&mut self, width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut title = Line::new();
        title.push(Span::styled(
            format!(" {}", self.title),
            self.theme.accent.bold(),
        ));
        title.truncate(width as usize, true);
        lines.push(title);
        let body = if self.input.is_empty() && !self.placeholder.is_empty() {
            Line::styled(format!(" {}", self.placeholder), self.theme.dim)
        } else {
            Line::plain(format!(" {}", self.input))
        };
        lines.push(body);
        lines.push(Line::styled(
            crate::i18n::tr("dialog.hint.input"),
            self.theme.dim,
        ));
        lines
    }

    pub fn handle_input(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        if key.is_release {
            return true;
        }
        if key.matches("enter") {
            self.on_confirm = Some(self.input.clone());
            self.done = true;
        } else if key.matches("escape") {
            self.cancelled = true;
            self.done = true;
        } else if key.matches("backspace") {
            self.input.pop();
        } else if let Key::Char(c) = key.key
            && !key.modifiers.ctrl
            && !key.modifiers.alt
        {
            self.input.push(c);
        }
        true
    }
}

/// Generic selector dialog: title + filterable list. Typed characters filter.
#[derive(Debug)]
pub struct SelectDialog {
    title: String,
    pub(crate) list: SelectList,
    purpose: SelectPurpose,
    done: bool,
    cancelled: bool,
    on_confirm: Option<String>,
    /// Ctrl+S pressed (model selector: save as default).
    pub save_default: bool,
    pub hint: &'static str,
    theme: Theme,
}

impl SelectDialog {
    pub fn new(
        title: impl Into<String>,
        items: Vec<SelectItem>,
        purpose: SelectPurpose,
        theme: Theme,
    ) -> Self {
        SelectDialog {
            title: title.into(),
            list: SelectList::new(items),
            purpose,
            done: false,
            cancelled: false,
            on_confirm: None,
            save_default: false,
            hint: "dialog.hint.select",
            theme,
        }
    }

    pub fn render(&mut self, width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut title = Line::new();
        title.push(Span::styled(
            format!(" {}", self.title),
            self.theme.accent.bold(),
        ));
        if !self.list.filter.is_empty() {
            title.push(Span::styled(
                format!("  {}{}", crate::i18n::tr("dialog.filter"), self.list.filter),
                self.theme.dim,
            ));
        }
        title.truncate(width as usize, true);
        lines.push(title);
        lines.extend(self.list.render(width));
        lines.push(Line::styled(crate::i18n::tr(self.hint), self.theme.dim));
        lines
    }

    pub fn handle_input(&mut self, event: &InputEvent) -> bool {
        // Ctrl+S: model selector "save as default" (TS models.save).
        if self.purpose == SelectPurpose::Model
            && let InputEvent::Key(key) = event
            && key.matches("ctrl+s")
        {
            if let Some(item) = self.list.selected_item() {
                self.on_confirm = Some(item.value.clone());
            }
            self.save_default = true;
            self.done = true;
            return true;
        }
        // Typed characters edit the filter.
        if let InputEvent::Key(KeyEvent {
            key: Key::Char(c),
            modifiers,
            is_release: false,
        }) = event
            && !modifiers.ctrl
            && !modifiers.alt
        {
            let mut filter = self.list.filter.clone();
            filter.push(*c);
            self.list.set_filter(filter);
            return true;
        }
        if let InputEvent::Key(key) = event
            && key.matches("backspace")
            && !self.list.filter.is_empty()
        {
            let mut filter = self.list.filter.clone();
            filter.pop();
            self.list.set_filter(filter);
            return true;
        }
        let handled = self.list.handle_input(event);
        if let Some(value) = self.list.on_confirm.take() {
            self.on_confirm = Some(value);
            self.done = true;
        }
        if self.list.cancelled {
            self.cancelled = true;
            self.done = true;
        }
        handled
    }
}
