// ---------------------------------------------------------------------------
// Session manager dialog (/resume): rename / delete / sort
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};

use tack_session::SessionManager;
use tack_tui::Component as _;
use tack_tui::components::select_list::{SelectItem, SelectList};
use tack_tui::{InputEvent, Key, Line, Span};

use crate::tui::theme::Theme;

fn session_label(s: &tack_session::SessionSummary) -> String {
    s.name
        .clone()
        .or_else(|| {
            s.first_prompt
                .as_ref()
                .map(|p| p.chars().take(60).collect::<String>())
        })
        .unwrap_or_else(|| s.session_id.clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionMode {
    Browse,
    ConfirmDelete,
    Rename,
}

/// `/resume` session manager (TS session-selector): filterable list with
/// rename (ctrl+r), delete (ctrl+d, two-stage), and sort toggle (ctrl+s).
/// Delete/rename happen in-dialog; the active session is protected.
#[derive(Debug)]
pub struct SessionDialog {
    dir: PathBuf,
    active: Option<PathBuf>,
    sort_by_name: bool,
    mode: SessionMode,
    rename_input: String,
    error: Option<String>,
    list: SelectList,
    pub(super) done: bool,
    pub(super) cancelled: bool,
    pub(super) on_confirm: Option<String>,
    theme: Theme,
}

impl SessionDialog {
    pub fn new(dir: PathBuf, active: Option<PathBuf>, theme: Theme) -> Self {
        let mut dialog = SessionDialog {
            dir,
            active,
            sort_by_name: false,
            mode: SessionMode::Browse,
            rename_input: String::new(),
            error: None,
            list: SelectList::new(Vec::new()),
            done: false,
            cancelled: false,
            on_confirm: None,
            theme,
        };
        dialog.refresh();
        dialog
    }

    fn refresh(&mut self) {
        let mut sessions = tack_session::list_sessions(&self.dir);
        if self.sort_by_name {
            sessions.sort_by_key(|s| session_label(s).to_lowercase());
        } else {
            sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
        }
        let filter = self.list.filter.clone();
        let items = sessions
            .iter()
            .map(|s| {
                SelectItem::new(session_label(s), s.path.display().to_string()).with_description(
                    crate::i18n::trf(
                        "sd.meta",
                        &[
                            ("count", &s.message_count.to_string()),
                            ("date", &s.timestamp[..10.min(s.timestamp.len())]),
                        ],
                    ),
                )
            })
            .collect();
        self.list = SelectList::new(items);
        self.list.set_filter(filter);
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.list.selected_item().map(|i| PathBuf::from(&i.value))
    }

    fn is_active(&self, path: &Path) -> bool {
        self.active.as_deref().is_some_and(|active| {
            dunce::canonicalize(active).unwrap_or_else(|_| active.to_path_buf())
                == dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        })
    }

    pub fn render(&mut self, width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut title = Line::new();
        let sort = if self.sort_by_name {
            crate::i18n::tr("sd.sort_name")
        } else {
            crate::i18n::tr("sd.sort_time")
        };
        title.push(Span::styled(
            crate::i18n::trf("sd.title", &[("sort", &sort)]),
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
        let footer = match self.mode {
            SessionMode::Browse => crate::i18n::tr("sd.hint_browse"),
            SessionMode::ConfirmDelete => {
                let label = self
                    .list
                    .selected_item()
                    .map(|i| i.label.clone())
                    .unwrap_or_default();
                crate::i18n::trf("sd.confirm_delete", &[("label", &format!("{label:?}"))])
            }
            SessionMode::Rename => crate::i18n::trf("sd.rename", &[("input", &self.rename_input)]),
        };
        lines.push(Line::styled(footer, self.theme.dim));
        if let Some(error) = &self.error {
            lines.push(Line::styled(format!(" {error}"), self.theme.error));
        }
        lines
    }

    pub fn handle_input(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        if key.is_release {
            return true;
        }
        match self.mode {
            SessionMode::ConfirmDelete => {
                if key.matches("enter") || key.matches("ctrl+d") {
                    if let Some(path) = self.selected_path()
                        && let Err(e) = std::fs::remove_file(&path)
                    {
                        self.error = Some(crate::i18n::trf(
                            "sd.delete_failed",
                            &[("error", &e.to_string())],
                        ));
                    }
                    self.mode = SessionMode::Browse;
                    self.refresh();
                    if self.list.items.is_empty() {
                        self.done = true;
                        self.cancelled = true;
                    }
                } else if key.matches("escape") {
                    self.mode = SessionMode::Browse;
                }
                true
            }
            SessionMode::Rename => {
                if key.matches("enter") {
                    if let Some(path) = self.selected_path() {
                        let name = self.rename_input.trim().to_string();
                        let result = SessionManager::open(&path, None)
                            .and_then(|mut s| s.append_session_info(Some(name)));
                        if let Err(e) = result {
                            self.error = Some(crate::i18n::trf(
                                "sd.rename_failed",
                                &[("error", &e.to_string())],
                            ));
                        }
                    }
                    self.mode = SessionMode::Browse;
                    self.refresh();
                } else if key.matches("escape") {
                    self.mode = SessionMode::Browse;
                } else if key.matches("backspace") {
                    self.rename_input.pop();
                } else if let Key::Char(c) = key.key
                    && !key.modifiers.ctrl
                    && !key.modifiers.alt
                {
                    self.rename_input.push(c);
                }
                true
            }
            SessionMode::Browse => {
                if key.matches("ctrl+d") {
                    match self.selected_path() {
                        Some(path) if self.is_active(&path) => {
                            self.error = Some(crate::i18n::tr("sd.no_delete_active"));
                        }
                        Some(_) => {
                            self.error = None;
                            self.mode = SessionMode::ConfirmDelete;
                        }
                        None => {}
                    }
                    return true;
                }
                if key.matches("ctrl+r") {
                    match self.selected_path() {
                        Some(path) if self.is_active(&path) => {
                            self.error = Some(crate::i18n::tr("sd.rename_active"));
                        }
                        Some(_) => {
                            self.error = None;
                            self.rename_input.clear();
                            self.mode = SessionMode::Rename;
                        }
                        None => {}
                    }
                    return true;
                }
                if key.matches("ctrl+s") {
                    self.sort_by_name = !self.sort_by_name;
                    self.refresh();
                    return true;
                }
                // Same filter typing as SelectDialog.
                if let Key::Char(c) = key.key
                    && !key.modifiers.ctrl
                    && !key.modifiers.alt
                {
                    let mut filter = self.list.filter.clone();
                    filter.push(c);
                    self.list.set_filter(filter);
                    return true;
                }
                if key.matches("backspace") && !self.list.filter.is_empty() {
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
    }
}
