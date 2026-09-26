//! Context/editor/utility commands (`/context`, `/todo`, `/copy`, `!cmd`, ...).

use std::path::PathBuf;

use super::super::chat::TranscriptItem;
use super::super::export::context_segment_chars;
use super::super::theme::Theme;
use super::super::{ChatEntry, NoticeKind, TuiApp, fullscreen};
use super::{assistant_notice, export_html};
use tack_tui::Line;

impl TuiApp {
    /// externalEditorCommand setting > $EDITOR > $VISUAL > platform default.
    fn external_editor_command(&self) -> String {
        self.settings
            .external_editor_command
            .clone()
            .or_else(|| std::env::var("EDITOR").ok())
            .or_else(|| std::env::var("VISUAL").ok())
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "vi".to_string()
                }
            })
    }

    /// Open an existing file in the external editor, suspending raw mode
    /// while the editor owns the terminal. Returns true on clean exit.
    pub(crate) fn edit_file_in_external_editor(&mut self, path: &std::path::Path) -> bool {
        // Queued frame output must reach the terminal FIRST, or stale diff
        // bytes land in the middle of the editor's screen.
        tack_tui::terminal::drain_frames();
        let _ = crossterm::terminal::disable_raw_mode();
        let status = std::process::Command::new(self.external_editor_command())
            .arg(path)
            .status();
        let _ = crossterm::terminal::enable_raw_mode();
        match status {
            Ok(s) if s.success() => true,
            Ok(s) => {
                self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.editor_exited",
                        &[("status", &s.to_string())],
                    ),
                    NoticeKind::Warning,
                );
                false
            }
            Err(e) => {
                self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.editor_launch_failed",
                        &[("error", &e.to_string())],
                    ),
                    NoticeKind::Error,
                );
                false
            }
        }
    }

    /// Ctrl+G: edit the prompt in an external editor.
    pub async fn open_external_editor(&mut self) {
        let dir = std::env::temp_dir();
        let file = dir.join("tack-prompt.md");
        if let Err(e) = std::fs::write(&file, self.editor.text()) {
            self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.ext_editor_failed",
                    &[("error", &e.to_string())],
                ),
                NoticeKind::Error,
            );
            return;
        }
        if self.edit_file_in_external_editor(&file) {
            match std::fs::read_to_string(&file) {
                Ok(text) => self.editor.set_text(text.trim_end()),
                Err(e) => self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.ext_readback_failed",
                        &[("error", &e.to_string())],
                    ),
                    NoticeKind::Error,
                ),
            }
        }
        let _ = std::fs::remove_file(&file);
    }

    // ---- settings menu ----

    /// /todo — show the shared session todo list (the same state the panel
    /// renders); `/todo clear` empties it.
    pub(crate) async fn command_todo(&mut self, args: &str) {
        if args.trim() == "clear" {
            let cleared = {
                let mut todo = self.todo_state.try_lock();
                match todo.as_deref_mut() {
                    Ok(todo) => {
                        let n = todo.items.len();
                        todo.items.clear();
                        Some(n)
                    }
                    Err(_) => None,
                }
            };
            if let Some(n) = cleared {
                self.notice(
                    crate::i18n::t(self.lang, "todo.cleared", &[("count", &n.to_string())]),
                    NoticeKind::Info,
                );
            }
            return;
        }
        let rendered = {
            let todo = self.todo_state.lock().await;
            let mut text = crate::i18n::t(self.lang, "todo.header", &[]);
            if todo.items.is_empty() {
                text.push_str(&crate::i18n::t(self.lang, "todo.empty", &[]));
            }
            for item in &todo.items {
                let marker = match item.status.as_str() {
                    "done" => "☑",
                    "in_progress" => "◐",
                    _ => "☐",
                };
                text.push_str(&format!("- {marker} #{} {}\n", item.id, item.text));
            }
            let done = todo.items.iter().filter(|i| i.status == "done").count();
            if !todo.items.is_empty() {
                text.push_str(&crate::i18n::t(
                    self.lang,
                    "todo.done_count",
                    &[
                        ("done", &done.to_string()),
                        ("total", &todo.items.len().to_string()),
                    ],
                ));
            }
            text
        };
        self.items
            .push(TranscriptItem::Chat(ChatEntry::Markdown { text: rendered }));
        self.line_cache.clear();
    }

    /// /context — what occupies the context window right now: fixed parts
    /// (system prompt, tool schemas) and per-segment history estimates, plus
    /// how close the run is to the auto-compaction threshold.
    pub(crate) fn command_context(&mut self) {
        const CHARS_PER_TOKEN: f64 = 4.0;
        let est = |chars: usize| (chars as f64 / CHARS_PER_TOKEN) as u64;

        let context = self.state.session.build_session_context();
        let messages = &context.messages;
        let total_estimate = tack_session::estimate_context_tokens(messages);

        // Segment the history by message kind.
        let segments = context_segment_chars(messages);
        let user_chars = segments.user;
        let assistant_chars = segments.assistant;
        let thinking_chars = segments.thinking;
        let tool_result_chars = segments.tool_results;

        let tool_results_total: usize = tool_result_chars.values().sum();
        let window = self.state.model.context_window as u64;
        let reserve = self.settings.compaction.reserve_tokens;

        let mut rows: Vec<(String, u64)> = vec![
            (
                crate::i18n::t(self.lang, "ctx.seg_system", &[]),
                est(self.context_system_chars),
            ),
            (
                crate::i18n::t(self.lang, "ctx.seg_tools", &[]),
                est(self.context_tools_chars),
            ),
            (
                crate::i18n::t(self.lang, "ctx.seg_user", &[]),
                est(user_chars),
            ),
            (
                crate::i18n::t(self.lang, "ctx.seg_assistant", &[]),
                est(assistant_chars),
            ),
            (
                crate::i18n::t(self.lang, "ctx.seg_thinking", &[]),
                est(thinking_chars),
            ),
            (
                crate::i18n::t(self.lang, "ctx.seg_tool_results", &[]),
                est(tool_results_total),
            ),
        ];
        rows.retain(|(_, tokens)| *tokens > 0);
        let fixed_total: u64 = rows.iter().map(|(_, t)| *t).sum();

        let mut text = crate::i18n::t(self.lang, "ctx.header", &[]);
        text.push_str(&crate::i18n::t(self.lang, "ctx.table_header", &[]));
        for (name, tokens) in &rows {
            text.push_str(&format!("| {name} | {tokens} |\n"));
        }
        text.push_str(&crate::i18n::t(
            self.lang,
            "ctx.sum",
            &[("total", &fixed_total.to_string())],
        ));

        // The provider-reported total is authoritative once a response has
        // come back (it includes chat template overhead the estimate lacks).
        if total_estimate.usage_tokens > 0 {
            text.push_str(&crate::i18n::t(
                self.lang,
                "ctx.provider_count",
                &[
                    ("usage", &total_estimate.usage_tokens.to_string()),
                    ("trailing", &total_estimate.trailing_tokens.to_string()),
                    ("total", &total_estimate.tokens.to_string()),
                ],
            ));
        } else {
            text.push_str(&crate::i18n::t(self.lang, "ctx.no_usage", &[]));
        }
        if let Some(pct) = (total_estimate.tokens * 100).checked_div(window) {
            text.push_str(&crate::i18n::t(
                self.lang,
                "ctx.window",
                &[
                    ("used", &total_estimate.tokens.to_string()),
                    ("window", &window.to_string()),
                    ("pct", &pct.to_string()),
                ],
            ));
            if self.settings.compaction.enabled {
                let trigger = window.saturating_sub(reserve);
                text.push_str(&crate::i18n::t(
                    self.lang,
                    "ctx.autocompaction",
                    &[
                        ("trigger", &trigger.to_string()),
                        ("reserve", &reserve.to_string()),
                        (
                            "remaining",
                            &trigger.saturating_sub(total_estimate.tokens).to_string(),
                        ),
                    ],
                ));
            }
        }
        // Prompt-cache friendliness (provider-reported). History rewrites
        // invalidate the cache from the first rewritten message on — the
        // reason the optimization passes are savings-gated.
        let totals = self.state.session.session_totals();
        if totals.cache_read + totals.cache_write > 0 {
            let base = totals.cache_read + totals.input;
            let pct = totals
                .cache_read
                .saturating_mul(100)
                .checked_div(base)
                .unwrap_or(0);
            text.push_str(&crate::i18n::t(
                self.lang,
                "ctx.cache",
                &[
                    ("read", &totals.cache_read.to_string()),
                    ("write", &totals.cache_write.to_string()),
                    ("pct", &pct.to_string()),
                ],
            ));
        }
        // Active context budgets / history-optimization knobs.
        let flag = |on: bool| {
            crate::i18n::t(
                self.lang,
                if on { "ctx.flag_on" } else { "ctx.flag_off" },
                &[],
            )
        };
        let micro = if self.settings.microcompact_enabled {
            self.settings.microcompact_max_chars.to_string()
        } else {
            flag(false)
        };
        text.push_str(&crate::i18n::t(
            self.lang,
            "ctx.budgets",
            &[
                ("cap", &self.settings.tool_result_max_chars.to_string()),
                ("micro", &micro),
                (
                    "savings",
                    &self.settings.microcompact_min_savings_chars.to_string(),
                ),
                ("dedup", &flag(self.settings.mask_duplicate_reads)),
                ("rules", &self.settings.rules_max_chars.to_string()),
                ("recite", &flag(self.settings.compaction.goal_recitation)),
            ],
        ));
        // Largest tool results — the usual reason context grows fast.
        if tool_result_chars.len() > 1 {
            let mut by_size: Vec<(&String, &usize)> = tool_result_chars.iter().collect();
            by_size.sort_by_key(|(_, chars)| -(**chars as i64));
            text.push_str(&crate::i18n::t(self.lang, "ctx.by_size", &[]));
            for (name, chars) in by_size.iter().take(5) {
                text.push_str(&crate::i18n::t(
                    self.lang,
                    "ctx.by_size_row",
                    &[("name", name), ("tokens", &est(**chars).to_string())],
                ));
            }
        }

        self.items
            .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
        self.line_cache.clear();
    }

    /// Skills visible to this session: the standard discovery set plus
    /// skill directories contributed by installed extension bundles.
    pub(crate) fn load_session_skills(
        &self,
    ) -> (
        Vec<crate::skills::Skill>,
        Vec<crate::skills::SkillDiagnostic>,
    ) {
        let (mut skills, mut diagnostics) = crate::skills::load_skills(&self.cwd, &self.agent_dir);
        for dir in &self.extensions.bundle_skill_dirs {
            let (found, diag) = crate::skills::load_skills_from_dir(dir);
            for skill in found {
                if skills
                    .iter()
                    .any(|s: &crate::skills::Skill| s.name == skill.name)
                {
                    diagnostics.push(crate::skills::SkillDiagnostic {
                        message: format!("name \"{}\" collision", skill.name),
                        path: skill.file_path.clone(),
                    });
                } else {
                    skills.push(skill);
                }
            }
            diagnostics.extend(diag);
        }
        (skills, diagnostics)
    }

    pub(crate) fn command_rules(&mut self) {
        let files = crate::resources::load_project_context_files(&self.cwd, &self.agent_dir);
        let (skills, _) = self.load_session_skills();
        let mut text = crate::i18n::t(self.lang, "rules.context_files", &[]);
        if files.is_empty() {
            text.push_str(&crate::i18n::t(self.lang, "rules.none", &[]));
        }
        for f in &files {
            text.push_str(&crate::i18n::t(
                self.lang,
                "rules.file_row",
                &[("path", &f.path), ("chars", &f.content.len().to_string())],
            ));
        }
        text.push_str(&crate::i18n::t(self.lang, "rules.skills", &[]));
        if skills.is_empty() {
            text.push_str(&crate::i18n::t(self.lang, "rules.none", &[]));
        }
        for s in &skills {
            text.push_str(&format!("- `{}` — {}\n", s.name, s.description));
        }
        self.items.push(TranscriptItem::Chat(ChatEntry::Assistant {
            message: assistant_notice(&text),
            streaming: false,
        }));
        self.line_cache.clear();
    }

    pub fn command_copy(&mut self) {
        // TS handleCopyCommand: in fullscreen with fullscreenCopyOnSelect
        // disabled, an active text selection is the copy target; otherwise
        // fall back to the last assistant message.
        if self.fullscreen
            && let Some(text) = fullscreen::ctrl_x_selection_text(
                self.settings.fullscreen_copy_on_select,
                self.selection,
                &self.last_frame,
            )
        {
            // Upstream 60e7e76bd: a failed copy is surfaced, not swallowed.
            // The write itself can block for seconds (native backends with
            // timeouts) — never on the event loop (upstream #9163).
            let tx = self.event_tx.clone();
            let lang = self.lang;
            tokio::task::spawn_blocking(move || {
                let (msg, kind) = match tack_tui::terminal::copy_to_clipboard(&text) {
                    Ok(()) => (
                        crate::i18n::t(lang, "msg.copied_selection", &[]),
                        NoticeKind::Info,
                    ),
                    Err(err) => (
                        crate::i18n::t(lang, "msg.copy_failed", &[("error", &err.to_string())]),
                        NoticeKind::Error,
                    ),
                };
                let _ = tx.send(crate::tui::AppEvent::Notice(msg, kind));
            });
            return;
        }
        let last = self.items.iter().rev().find_map(|e| match e {
            TranscriptItem::Chat(ChatEntry::Assistant { message, .. }) => Some(message.text()),
            _ => None,
        });
        match last {
            Some(text) if !text.is_empty() => {
                let tx = self.event_tx.clone();
                let lang = self.lang;
                tokio::task::spawn_blocking(move || {
                    let (msg, kind) = match tack_tui::terminal::copy_to_clipboard(&text) {
                        Ok(()) => (
                            crate::i18n::t(lang, "msg.copied_last", &[]),
                            NoticeKind::Info,
                        ),
                        Err(err) => (
                            crate::i18n::t(lang, "msg.copy_failed", &[("error", &err.to_string())]),
                            NoticeKind::Error,
                        ),
                    };
                    let _ = tx.send(crate::tui::AppEvent::Notice(msg, kind));
                });
            }
            _ => self.notice(
                crate::i18n::t(self.lang, "msg.nothing_to_copy", &[]),
                NoticeKind::Info,
            ),
        }
    }

    /// `/debug` (TS handleDebugCommand): dump the rendered frame (JSON-escaped
    /// ANSI with visible widths) plus the session messages as JSONL to
    /// `<agent>/tack-debug.log`. Invaluable for rendering bug reports.
    ///
    /// Regular mode no longer keeps a joined frame around (that clone was
    /// per-keystroke overhead), so this only ARMS the capture: the next
    /// render — same event-loop turn — joins the frame parts and calls
    /// write_debug_dump, which also posts the result notice.
    pub fn command_debug(&mut self) {
        self.debug_capture = true;
    }

    /// Write the /debug dump for a captured frame (see command_debug).
    pub(crate) fn write_debug_dump(&mut self, frame: &[Line]) {
        let (width, height) = self.tui.size();
        let path = self.agent_dir.join("tack-debug.log");
        let mut out = format!(
            "Debug output at {:?}\nTerminal: {width}x{height}\nTotal lines: {}\n\n=== All rendered lines with visible widths ===\n",
            std::time::SystemTime::now(),
            frame.len()
        );
        for (i, line) in frame.iter().enumerate() {
            let ansi = line.to_ansi();
            let escaped = serde_json::to_string(&ansi).unwrap_or_else(|_| ansi.clone());
            out.push_str(&format!("[{i}] (w={}) {escaped}\n", line.width()));
        }
        out.push_str("\n=== Agent messages (JSONL) ===\n");
        for msg in &self.state.session.build_session_context().messages {
            if let Ok(json) = serde_json::to_string(msg) {
                out.push_str(&json);
                out.push('\n');
            }
        }
        match std::fs::write(&path, out) {
            Ok(()) => self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.debug_written",
                    &[("path", &path.display().to_string())],
                ),
                NoticeKind::Info,
            ),
            Err(e) => self.notice(
                crate::i18n::t(self.lang, "msg.debug_failed", &[("error", &e.to_string())]),
                NoticeKind::Error,
            ),
        }
    }

    pub(crate) fn command_export(&mut self, args: &str) {
        let Some(file) = self.state.session.session_file() else {
            self.notice(
                crate::i18n::t(self.lang, "msg.session_not_persisted", &[]),
                NoticeKind::Warning,
            );
            return;
        };
        // .jsonl → raw copy; anything else (default) → self-contained HTML
        // (TS pi's default export format).
        let target = if args.is_empty() {
            PathBuf::from(format!("{}.html", self.state.session.session_id()))
        } else {
            PathBuf::from(args)
        };
        if target.extension().is_some_and(|e| e == "jsonl") {
            match std::fs::copy(file, &target) {
                Ok(_) => self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.exported",
                        &[("path", &target.display().to_string())],
                    ),
                    NoticeKind::Info,
                ),
                Err(e) => self.notice(
                    crate::i18n::t(self.lang, "msg.export_failed", &[("error", &e.to_string())]),
                    NoticeKind::Error,
                ),
            }
            return;
        }
        match export_html(&self.state.session, &target) {
            Ok(()) => self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.exported",
                    &[("path", &target.display().to_string())],
                ),
                NoticeKind::Info,
            ),
            Err(e) => self.notice(
                crate::i18n::t(self.lang, "msg.export_failed", &[("error", &e.to_string())]),
                NoticeKind::Error,
            ),
        }
    }

    pub(crate) fn command_reload(&mut self) {
        self.settings = crate::settings::Settings::load(&self.cwd, &self.agent_dir);
        self.theme = Theme::resolve(self.settings.theme.as_deref(), &self.agent_dir, &self.cwd);
        self.line_cache.clear();
        // Settings may change prompt/skill dirs and extra paths.
        self.ac_sources = None;
        self.notice(
            crate::i18n::t(self.lang, "msg.reloaded", &[]),
            NoticeKind::Info,
        );
    }

    /// `!cmd` / `!!cmd`: run bash directly, output as a chat entry.
    pub async fn run_bash(&mut self, command: String, exclude_from_context: bool) {
        if command.is_empty() {
            return;
        }
        let id = format!("bash-{}", tack_ai::now_millis());
        let args = serde_json::json!({ "command": command });
        self.register_tool(super::super::tool_render::ToolEntry {
            tool_call_id: id.clone(),
            tool_name: "bash".into(),
            args_fp: super::super::tool_render::args_fingerprint(&args),
            args,
            state: super::super::tool_render::ToolState::Running {
                partial_output: String::new(),
            },
            expanded: false,
        });
        // Spawned like the agent run: waiting on the subprocess inline
        // froze the UI loop for the command's whole lifetime. Partial
        // output streams through the same ToolExecutionUpdate path agent
        // tools use; the final result lands as AppEvent::BangDone. Esc
        // cancels through the per-command token in self.bang_cancel.
        let cancel = tokio_util::sync::CancellationToken::new();
        self.bang_cancel.insert(id.clone(), cancel.clone());
        let tx = self.event_tx.clone();
        let cwd = self.cwd.clone();
        // Captured for the landing guard: by the time the task finishes
        // the session may have been replaced (/new, /resume, a run's
        // placeholder) — the result must not land there.
        let session_id = self.state.session.session_id().to_string();
        tokio::spawn(async move {
            let services = tack_tools::default_services(cwd);
            let bash = tack_tools::BashTool::new(services);
            use tack_agent_core::AgentTool;
            let update_tx = tx.clone();
            let update_id = id.clone();
            let exec_cancel = cancel.clone();
            let result = bash
                .execute(
                    &id,
                    serde_json::json!({ "command": command }),
                    cancel,
                    &move |partial| {
                        let _ = update_tx.send(crate::tui::AppEvent::Agent(Box::new(
                            tack_agent_core::AgentEvent::ToolExecutionUpdate {
                                tool_call_id: update_id.clone(),
                                tool_name: "bash".to_string(),
                                args: serde_json::Value::Null,
                                partial_result: partial,
                            },
                        )));
                    },
                )
                .await;
            let cancelled = exec_cancel.is_cancelled();
            let (result, is_error) = match result {
                Ok(r) => (r, false),
                Err(e) => (tack_agent_core::AgentToolResult::error(e), true),
            };
            let _ = tx.send(crate::tui::AppEvent::BangDone {
                id,
                command,
                session_id,
                exclude_from_context,
                cancelled,
                result,
                is_error,
            });
        });
    }

    /// `!cmd` result landed (AppEvent::BangDone): finish the tool card and
    /// persist the BashExecution message (main loop — owns the session).
    pub(crate) fn handle_bang_done(
        &mut self,
        id: &str,
        command: &str,
        session_id: &str,
        outcome: BangOutcome,
    ) {
        let BangOutcome {
            exclude_from_context,
            cancelled,
            result,
            is_error,
        } = outcome;
        self.bang_cancel.remove(id);
        if let Some(tool) = self.tools.get_mut(id) {
            tool.state = super::super::tool_render::ToolState::Done {
                result: result.clone(),
                is_error,
            };
        }
        self.line_cache.clear();
        let output = result
            .content
            .iter()
            .filter_map(|b| match b {
                tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let message =
            tack_agent_core::AgentMessage::BashExecution(tack_agent_core::BashExecutionMessage {
                command: command.to_string(),
                output,
                exit_code: Some(if is_error { 1 } else { 0 }),
                cancelled,
                truncated: false,
                full_output_path: None,
                exclude_from_context: Some(exclude_from_context),
                timestamp: tack_ai::now_millis(),
            });
        // Landing guard: the session may have been replaced (/new,
        // /resume, or a run's in-memory placeholder) while the command
        // ran — appending there pollutes the wrong session or silently
        // loses the entry. The tool card above still shows the result.
        if self.state.session.session_id() != session_id {
            self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.bash_persist_failed",
                    &[("error", "session changed while the command ran")],
                ),
                NoticeKind::Warning,
            );
            return;
        }
        if let Err(e) = self.state.session.append_message(message) {
            self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.bash_persist_failed",
                    &[("error", &e.to_string())],
                ),
                NoticeKind::Warning,
            );
        }
    }

    // ---- dialog result routing ----
}

/// Outcome payload of a finished `!cmd` task (see AppEvent::BangDone).
pub(crate) struct BangOutcome {
    pub exclude_from_context: bool,
    pub cancelled: bool,
    pub result: tack_agent_core::AgentToolResult,
    pub is_error: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Regression: assistant tool-call arguments (name + arguments) must
    /// count toward the /context estimate — a large `write` call is real
    /// context and previously vanished from the segment table entirely.
    #[test]
    fn context_segments_count_tool_call_arguments() {
        let big_args = serde_json::json!({ "path": "a.rs", "content": "x".repeat(4000) });
        let mut assistant = tack_ai::AssistantMessage::pending(&tack_ai::Model {
            id: "m".into(),
            name: "m".into(),
            api: "test".into(),
            provider: "test".into(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![tack_ai::InputKind::Text],
            cost: tack_ai::ModelCost::default(),
            context_window: 1000,
            max_tokens: 1000,
            sampling_params: None,
            headers: None,
            compat: None,
        });
        assistant.content = vec![
            tack_ai::ContentBlock::Text {
                text: "writing the file".into(),
                text_signature: None,
            },
            tack_ai::ContentBlock::ToolCall {
                id: "call-1".into(),
                name: "write".into(),
                arguments: big_args.clone(),
                thought_signature: None,
                namespace: None,
            },
        ];
        let segments =
            context_segment_chars(&[tack_agent_core::AgentMessage::Assistant(assistant)]);
        let expected = "writing the file".chars().count()
            + "write".chars().count()
            + big_args.to_string().chars().count();
        assert_eq!(segments.assistant, expected);
        assert!(
            segments.assistant > 4000,
            "the 4k of tool-call arguments must be counted: {}",
            segments.assistant
        );
    }
}
