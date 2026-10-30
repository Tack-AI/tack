//! `/tasks` — the background task & agent control panel.
//!
//! One live surface for everything that keeps running while the agent is
//! idle: background bash tasks, background sub-agents, and (through their
//! progress log) parallel worktree runs. Rows refresh in place, Enter opens
//! a tail-following output viewer, `x` cancels a running task, `c` clears
//! finished ones. The dialog owns a `BackgroundTaskManager` clone (Arc
//! inside), so it can act without routing one-shot actions through the app.

use std::hash::{Hash as _, Hasher as _};
use std::time::{Duration, Instant};

use tack_tools::background::{BackgroundTaskManager, KillOutcome, TaskStatus, TaskSummary};
use tack_tui::Component as _;
use tack_tui::components::select_list::{SelectItem, SelectList};
use tack_tui::{InputEvent, Key, KeyEvent, Line, Span};

use crate::i18n;
use crate::tui::theme::Theme;

/// Rows of output shown by the viewer (the dialog renders above the editor).
const VIEWER_ROWS: usize = 12;
/// Chatty tasks append continuously; re-snapshot at most this often.
const OUTPUT_REFRESH: Duration = Duration::from_millis(500);
const MAX_VISIBLE: u16 = 10;

#[derive(Debug)]
struct Viewer {
    task_id: String,
    lines: Vec<String>,
    scroll: usize,
    /// Stick to the tail while output grows (the default when opened).
    follow: bool,
    truncated: bool,
    full_output_path: Option<String>,
    output_len: usize,
    loaded_at: Instant,
}

#[derive(Debug)]
pub struct TaskDialog {
    manager: BackgroundTaskManager,
    list: SelectList,
    summaries: Vec<TaskSummary>,
    viewer: Option<Viewer>,
    /// Inline steering-message input (`m` on a running subagent).
    composing: Option<String>,
    /// One-line result of the last action (`x`/`c`), shown above the hint.
    note: Option<String>,
    theme: Theme,
    /// Fingerprint of the last rendered state; tick() only repaints on change.
    last_fp: u64,
    done: bool,
}

impl TaskDialog {
    pub fn new(manager: BackgroundTaskManager, theme: Theme) -> Self {
        let mut dialog = TaskDialog {
            manager,
            list: SelectList::new(Vec::new()),
            summaries: Vec::new(),
            viewer: None,
            composing: None,
            note: None,
            theme,
            last_fp: 0,
            done: false,
        };
        dialog.last_fp = u64::MAX; // force the first refresh to build rows
        dialog.refresh();
        dialog
    }

    pub fn done(&self) -> bool {
        self.done
    }

    /// Called once per event-loop tick while the dialog is open. Returns
    /// true when something changed (status, elapsed second, new output).
    pub fn tick(&mut self) -> bool {
        if self.done {
            return false;
        }
        self.refresh()
    }

    fn fingerprint(&self, tasks: &[TaskSummary], viewer_len: Option<usize>) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for task in tasks {
            task.id.hash(&mut hasher);
            task.status_text().hash(&mut hasher);
            task.elapsed_seconds.hash(&mut hasher);
        }
        viewer_len.hash(&mut hasher);
        hasher.finish()
    }

    fn refresh(&mut self) -> bool {
        let tasks = self.manager.tasks();
        let viewer_len = self
            .viewer
            .as_ref()
            .and_then(|v| self.manager.output_len(&v.task_id));
        let fp = self.fingerprint(&tasks, viewer_len);
        if fp == self.last_fp {
            return false;
        }
        self.last_fp = fp;

        let selected = self.list.selected_item().map(|item| item.value.clone());
        self.summaries = tasks;
        self.rebuild_rows(selected.as_deref());
        self.refresh_viewer(viewer_len);
        true
    }

    fn rebuild_rows(&mut self, selected_id: Option<&str>) {
        let items = self
            .summaries
            .iter()
            .map(|task| {
                let mark = if task.running() {
                    "●"
                } else if matches!(task.status, TaskStatus::Exited(0)) {
                    "✓"
                } else {
                    "✗"
                };
                let label = format!(
                    "{mark} {}  {}",
                    task.id,
                    // Background bash labels are full commands and may be
                    // multi-line; a newline would split the select row.
                    truncate(&task.label.replace(['\n', '\r'], " "), 64)
                );
                let description = i18n::trf(
                    "tasks.row",
                    &[
                        ("status", &task.status_text()),
                        ("elapsed", &task.elapsed_seconds.to_string()),
                    ],
                );
                SelectItem::new(label, task.id.clone()).with_description(description)
            })
            .collect();
        self.list = SelectList::new(items);
        self.list.max_visible = MAX_VISIBLE;
        // Keep the cursor on the same task while rows update underneath.
        if let Some(id) = selected_id
            && let Some(pos) = self.list.items.iter().position(|item| item.value == id)
        {
            self.list.selected = pos;
        }
    }

    fn selected(&self) -> Option<&TaskSummary> {
        let id = &self.list.selected_item()?.value;
        self.summaries.iter().find(|task| &task.id == id)
    }

    fn open_viewer(&mut self) {
        let Some(task) = self.selected() else {
            return;
        };
        let id = task.id.clone();
        match self.manager.output(&id) {
            Some(output) => {
                let lines = output_lines(&output.content);
                self.note = None;
                let output_len = self.manager.output_len(&id).unwrap_or(0);
                self.viewer = Some(Viewer {
                    task_id: id,
                    lines,
                    scroll: 0,
                    follow: true,
                    truncated: output.truncated,
                    full_output_path: output.full_output_path,
                    output_len,
                    loaded_at: Instant::now(),
                });
                self.scroll_to_tail();
            }
            None => {
                self.note = Some(i18n::trf("tasks.unknown", &[("id", &id)]));
            }
        }
    }

    fn refresh_viewer(&mut self, viewer_len: Option<usize>) {
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        // Snapshotting a large output persists a temp file; only do it when
        // the length actually moved, and not more than twice per second.
        let Some(len) = viewer_len else {
            return;
        };
        if len == viewer.output_len || viewer.loaded_at.elapsed() < OUTPUT_REFRESH {
            return;
        }
        let Some(output) = self.manager.output(&viewer.task_id) else {
            return;
        };
        let lines = output_lines(&output.content);
        viewer.lines = lines;
        viewer.truncated = output.truncated;
        viewer.full_output_path = output.full_output_path;
        viewer.output_len = len;
        viewer.loaded_at = Instant::now();
        if viewer.follow {
            viewer.scroll = viewer.lines.len().saturating_sub(VIEWER_ROWS);
        } else {
            viewer.scroll = viewer
                .scroll
                .min(viewer.lines.len().saturating_sub(VIEWER_ROWS));
        }
    }

    fn scroll_to_tail(&mut self) {
        if let Some(viewer) = &mut self.viewer {
            viewer.scroll = viewer.lines.len().saturating_sub(VIEWER_ROWS);
        }
    }

    fn kill_selected(&mut self) {
        let Some(task) = self.selected() else {
            return;
        };
        let id = task.id.clone();
        let running = task.running();
        if !running {
            self.note = Some(i18n::trf("tasks.finished_already", &[("id", &id)]));
            return;
        }
        self.note = Some(match self.manager.kill(&id) {
            KillOutcome::Signalled => i18n::trf("tasks.killed", &[("id", &id)]),
            KillOutcome::AlreadyFinished(_) => i18n::trf("tasks.finished_already", &[("id", &id)]),
            KillOutcome::Unknown => i18n::trf("tasks.unknown", &[("id", &id)]),
        });
        self.force_refresh();
    }

    fn clear_finished(&mut self) {
        let finished = self.summaries.iter().filter(|task| !task.running()).count();
        self.manager.prune_finished();
        self.note = Some(i18n::trf(
            "tasks.cleared",
            &[("count", &finished.to_string())],
        ));
        self.force_refresh();
    }

    fn force_refresh(&mut self) {
        self.last_fp = u64::MAX;
        self.refresh();
    }

    fn handle_viewer_key(&mut self, key: &KeyEvent) -> bool {
        let Some(viewer) = &mut self.viewer else {
            return false;
        };
        let page = VIEWER_ROWS;
        if key.matches("escape") || key.matches("enter") || key.matches("q") {
            self.viewer = None;
            return true;
        }
        if key.matches("down") || key.matches("j") {
            viewer.follow = false;
            viewer.scroll = viewer
                .scroll
                .saturating_add(1)
                .min(viewer.lines.len().saturating_sub(VIEWER_ROWS));
        } else if key.matches("up") || key.matches("k") {
            viewer.follow = false;
            viewer.scroll = viewer.scroll.saturating_sub(1);
        } else if key.matches("pagedown") {
            viewer.follow = false;
            viewer.scroll = viewer
                .scroll
                .saturating_add(page)
                .min(viewer.lines.len().saturating_sub(VIEWER_ROWS));
        } else if key.matches("pageup") {
            viewer.follow = false;
            viewer.scroll = viewer.scroll.saturating_sub(page);
        } else if key.matches("home") {
            viewer.follow = false;
            viewer.scroll = 0;
        } else if key.matches("end") {
            viewer.follow = true;
            viewer.scroll = viewer.lines.len().saturating_sub(VIEWER_ROWS);
        } else if key.matches("f") {
            viewer.follow = !viewer.follow;
            if viewer.follow {
                viewer.scroll = viewer.lines.len().saturating_sub(VIEWER_ROWS);
            }
        }
        true
    }

    fn start_compose(&mut self) {
        let Some(task) = self.selected() else {
            return;
        };
        let id = task.id.clone();
        if !task.running() {
            self.note = Some(i18n::trf("tasks.finished_already", &[("id", &id)]));
            return;
        }
        if !task.accepts_messages {
            self.note = Some(i18n::trf("tasks.no_message_channel", &[("id", &id)]));
            return;
        }
        self.note = None;
        self.composing = Some(String::new());
    }

    /// Inline input for a steering message: printable characters append,
    /// Enter queues it through the task registry, Esc cancels.
    fn handle_compose_key(&mut self, key: &KeyEvent) -> bool {
        let Some(mut text) = self.composing.take() else {
            return false;
        };
        if key.matches("escape") {
            self.note = Some(i18n::tr("tasks.message_cancelled"));
            return true;
        }
        if key.matches("enter") {
            let message = text.trim().to_string();
            if message.is_empty() {
                self.composing = Some(text);
                return true;
            }
            let Some(task) = self.selected() else {
                return true;
            };
            let id = task.id.clone();
            self.note = Some(match self.manager.send_message(&id, &message) {
                Ok(()) => i18n::trf("tasks.message_sent", &[("id", &id)]),
                Err(error) => i18n::trf("tasks.message_failed", &[("error", &error)]),
            });
            return true;
        }
        if key.matches("backspace") {
            text.pop();
        } else if let Key::Char(c) = key.key
            && !key.modifiers.ctrl
            && !key.modifiers.alt
        {
            text.push(c);
        }
        self.composing = Some(text);
        true
    }

    pub fn handle_input(&mut self, event: &InputEvent) -> bool {
        // A paste while composing a steering message appends to the input.
        if let InputEvent::Paste(text) = event {
            if let Some(composing) = &mut self.composing {
                composing.push_str(text);
            }
            return true;
        }
        let InputEvent::Key(key) = event else {
            return true;
        };
        if key.is_release {
            return true;
        }
        if self.viewer.is_some() {
            return self.handle_viewer_key(key);
        }
        if self.composing.is_some() {
            return self.handle_compose_key(key);
        }
        if key.matches("escape") {
            self.done = true;
            return true;
        }
        if key.matches("enter") {
            self.open_viewer();
            return true;
        }
        if key.matches("m") {
            self.start_compose();
            return true;
        }
        if key.matches("x") {
            self.kill_selected();
            return true;
        }
        if key.matches("c") {
            self.clear_finished();
            return true;
        }
        self.list.handle_input(event);
        true
    }

    pub fn render(&mut self, width: u16) -> Vec<Line> {
        if let Some(viewer) = &self.viewer {
            return Self::render_viewer(&self.summaries, &self.theme, viewer, width);
        }
        let mut lines = Vec::new();
        let mut title = Line::new();
        title.push(Span::styled(
            format!(" {}", i18n::tr("tasks.title")),
            self.theme.accent.bold(),
        ));
        let running = self.summaries.iter().filter(|t| t.running()).count();
        title.push(Span::styled(
            format!(
                "  {}",
                i18n::trf(
                    "tasks.counts",
                    &[
                        ("running", &running.to_string()),
                        ("total", &self.summaries.len().to_string()),
                    ],
                )
            ),
            self.theme.dim,
        ));
        title.truncate(width as usize, true);
        lines.push(title);
        if self.summaries.is_empty() {
            lines.push(Line::styled(
                format!("  {}", i18n::tr("tasks.empty")),
                self.theme.dim,
            ));
            lines.push(Line::styled(
                format!("  {}", i18n::tr("tasks.empty_hint")),
                self.theme.dim,
            ));
        } else {
            lines.extend(self.list.render(width));
        }
        if let Some(note) = &self.note {
            lines.push(Line::styled(format!(" {note}"), self.theme.warning));
        }
        if let Some(text) = &self.composing {
            let id = self
                .selected()
                .map(|task| task.id.clone())
                .unwrap_or_default();
            let mut input = Line::new();
            input.push(Span::styled(
                format!(" {} {id}> {text}▏", i18n::tr("tasks.message_prompt")),
                self.theme.accent,
            ));
            input.truncate(width as usize, true);
            lines.push(input);
            lines.push(Line::styled(
                format!(" {}", i18n::tr("tasks.message_hint")),
                self.theme.dim,
            ));
        } else {
            lines.push(Line::styled(
                format!(" {}", i18n::tr("tasks.hint")),
                self.theme.dim,
            ));
        }
        lines
    }

    fn render_viewer(
        summaries: &[TaskSummary],
        theme: &Theme,
        viewer: &Viewer,
        width: u16,
    ) -> Vec<Line> {
        let mut lines = Vec::new();
        let summary = summaries.iter().find(|task| task.id == viewer.task_id);
        let status = summary
            .map(|task| {
                i18n::trf(
                    "tasks.viewer_title",
                    &[
                        ("id", &viewer.task_id),
                        ("status", &task.status_text()),
                        ("elapsed", &task.elapsed_seconds.to_string()),
                    ],
                )
            })
            .unwrap_or_else(|| viewer.task_id.clone());
        let mut title = Line::new();
        title.push(Span::styled(format!(" {status}"), theme.accent.bold()));
        if viewer.follow {
            title.push(Span::styled(
                format!("  {}", i18n::tr("tasks.following")),
                theme.success,
            ));
        }
        title.truncate(width as usize, true);
        lines.push(title);

        if viewer.truncated {
            let message = match &viewer.full_output_path {
                Some(path) => i18n::trf("tasks.truncated", &[("path", path)]),
                None => i18n::tr("tasks.truncated_no_path"),
            };
            let mut warning = Line::styled(format!(" {message}"), theme.warning);
            warning.truncate(width as usize, true);
            lines.push(warning);
        }

        let end = (viewer.scroll + VIEWER_ROWS).min(viewer.lines.len());
        for line in viewer.lines.iter().take(end).skip(viewer.scroll) {
            let mut rendered = Line::styled(format!(" {line}"), theme.tool_output);
            rendered.truncate(width as usize, false);
            lines.push(rendered);
        }
        // Keep the frame height stable so scrolling does not jump.
        for _ in end.saturating_sub(viewer.scroll)..VIEWER_ROWS {
            lines.push(Line::new());
        }
        lines.push(Line::styled(
            format!(" {}", i18n::tr("tasks.viewer_hint")),
            theme.dim,
        ));
        lines
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Split sanitized task output into display lines. Carriage returns survive
/// `sanitize_binary_output` (progress bars use them); rendered raw they would
/// move the terminal cursor and overwrite the panel, so treat `\r` as a line
/// break (CRLF first, so Windows output does not gain blank lines).
fn output_lines(content: &str) -> Vec<String> {
    let normalized = content
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        // Tabs have no fixed width in the frame math; expand them so
        // truncation and padding stay aligned.
        .replace('\t', "    ");
    let mut lines: Vec<String> = normalized.lines().map(str::to_string).collect();
    if lines.is_empty() {
        lines.push(i18n::tr("tasks.no_output"));
    }
    lines
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;
    use tack_tui::{InputEvent, Key, KeyEvent};

    fn key(k: Key) -> InputEvent {
        InputEvent::Key(KeyEvent::plain(k))
    }

    fn dialog() -> TaskDialog {
        TaskDialog::new(BackgroundTaskManager::new(), Theme::dark())
    }

    #[test]
    fn output_lines_normalize_carriage_returns() {
        // CRLF and bare CR both become line breaks (progress bars must not
        // move the terminal cursor over the panel).
        assert_eq!(output_lines("a\r\nb\rc\nd"), vec!["a", "b", "c", "d"]);
        assert_eq!(output_lines("a\tb"), vec!["a    b"]);
        assert_eq!(output_lines(""), vec![i18n::tr("tasks.no_output")]);
    }

    #[test]
    fn empty_panel_renders_without_rows() {
        let mut dialog = dialog();
        let lines = dialog.render(80);
        let text: String = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.text.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&i18n::tr("tasks.title")), "{text}");
        assert!(text.contains(&i18n::tr("tasks.empty")), "{text}");
    }

    #[test]
    fn rows_stay_in_creation_order_and_selection_survives_refresh() {
        let manager = BackgroundTaskManager::new();
        let first = manager.register("first task".to_string());
        let second = manager.register("second task".to_string());
        // Background bash commands are often multi-line; rows must stay one line.
        let _multiline = manager.register("echo a\necho b".to_string());
        let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
        assert_eq!(dialog.summaries.len(), 3);
        assert!(
            dialog
                .list
                .items
                .iter()
                .all(|item| !item.label.contains('\n')),
            "rows must not contain newlines: {:?}",
            dialog.list.items
        );
        // Select the second row, then finish the first: order is stable.
        dialog.list.selected = 1;
        first.finish(TaskStatus::Exited(0), None);
        assert!(dialog.refresh());
        assert_eq!(
            dialog
                .list
                .selected_item()
                .map(|item| item.value.clone())
                .as_deref(),
            Some(second.id.as_str())
        );
        assert_eq!(dialog.summaries[0].id, "bg1");
        assert!(!dialog.summaries[0].running());
    }

    #[test]
    fn x_stops_the_selected_running_task() {
        let manager = BackgroundTaskManager::new();
        let task = manager.register("long".to_string());
        let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
        dialog.handle_input(&key(Key::Char('x')));
        assert!(task.cancel.is_cancelled(), "cancel token must be signalled");
        assert!(dialog.note.is_some());
    }

    #[test]
    fn viewer_opens_follows_and_scrolls() {
        let manager = BackgroundTaskManager::new();
        let task = manager.register("echo".to_string());
        for i in 0..40 {
            task.log(&format!("line {i}"));
        }
        let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
        dialog.handle_input(&key(Key::Enter));
        let viewer = dialog.viewer.as_ref().expect("viewer opens");
        assert!(viewer.follow);
        assert_eq!(viewer.lines.len(), 40);
        assert_eq!(viewer.scroll, 40 - VIEWER_ROWS);

        dialog.handle_input(&key(Key::Char('k')));
        assert!(!dialog.viewer.as_ref().unwrap().follow);
        dialog.handle_input(&key(Key::Home));
        assert_eq!(dialog.viewer.as_ref().unwrap().scroll, 0);
        dialog.handle_input(&key(Key::End));
        assert!(dialog.viewer.as_ref().unwrap().follow);

        // Esc backs out to the list, a second Esc closes the panel.
        dialog.handle_input(&key(Key::Escape));
        assert!(dialog.viewer.is_none());
        assert!(!dialog.done);
        dialog.handle_input(&key(Key::Escape));
        assert!(dialog.done);
    }

    #[test]
    fn c_clears_finished_rows_only() {
        let manager = BackgroundTaskManager::new();
        let done = manager.register("done".to_string());
        let running = manager.register("running".to_string());
        done.finish(TaskStatus::Exited(0), None);
        let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
        dialog.handle_input(&key(Key::Char('c')));
        let ids: Vec<&str> = dialog.summaries.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec![running.id.as_str()]);
        assert!(dialog.note.is_some());
    }

    #[test]
    fn m_queues_a_steering_message_for_a_subagent() {
        let manager = BackgroundTaskManager::new();
        let task = manager.register("subagent: refactor".to_string());
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        task.set_message_sink(Arc::new(move |text| {
            sink.lock().unwrap().push(text.to_string());
        }));
        let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
        dialog.handle_input(&key(Key::Char('m')));
        for c in "use the index".chars() {
            dialog.handle_input(&key(Key::Char(c)));
        }
        dialog.handle_input(&key(Key::Enter));
        assert_eq!(*seen.lock().unwrap(), vec!["use the index".to_string()]);
        assert!(dialog.composing.is_none());
        assert!(dialog.note.as_deref().unwrap().contains("bg1"));
    }

    #[test]
    fn m_is_refused_without_a_message_channel() {
        let manager = BackgroundTaskManager::new();
        // Keep the handle alive: dropping it marks the task failed.
        let _task = manager.register("bash".to_string());
        let mut dialog = TaskDialog::new(manager, Theme::dark());
        dialog.handle_input(&key(Key::Char('m')));
        assert!(dialog.composing.is_none());
        assert!(dialog.note.as_deref().unwrap().contains("does not accept"));
    }
}
