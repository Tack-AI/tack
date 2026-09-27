//! Run control: prompt submission, the agent loop, run stats and
//! steering/follow-up queue hooks. Inherent `impl TuiApp` split out of
//! `mod.rs` — pure code move, no behavior change.

use super::*;

/// Graceful-quit decision for the main loop (pure; unit-tested below).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuitGraceDecision {
    /// First quit request while running: cancel + bounded grace wait.
    StartGrace,
    /// Exit now (idle, forced, settled, or grace expired).
    Break,
    /// Keep waiting for the cancelled run to settle.
    Wait,
}

/// How long the graceful quit waits for the cancelled run to settle.
const QUIT_GRACE_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// Decide what a loop iteration should do about quit state.
pub(crate) fn quit_grace_decision(
    should_quit: bool,
    running: bool,
    grace_started: Option<Instant>,
) -> QuitGraceDecision {
    if should_quit {
        if running && grace_started.is_none() {
            return QuitGraceDecision::StartGrace;
        }
        return QuitGraceDecision::Break;
    }
    if let Some(started) = grace_started
        && (!running || started.elapsed() >= QUIT_GRACE_WINDOW)
    {
        return QuitGraceDecision::Break;
    }
    QuitGraceDecision::Wait
}

impl TuiApp {
    /// The main loop: terminal input + agent events + spinner ticks.
    pub(crate) async fn run(&mut self) -> Result<i32> {
        let _guard = tack_tui::terminal::TerminalGuard::enter(self.fullscreen, self.fullscreen)?;
        let mut events = tack_tui::terminal::TerminalEvents::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        // Frame output goes through the writer thread: a slow terminal
        // (Termux with a large transcript) then lags OUTPUT instead of
        // blocking the loop that reads INPUT (see FrameWriter docs).
        let mut out = tack_tui::terminal::FrameWriter;
        let mut permission_rx = self.permission_rx.take().expect("permission rx");
        let mut compaction_rx = self.compaction_rx.take().expect("compaction rx");
        let mut bg_notify_rx = self.bg_notify_rx.take().expect("bg notify rx");
        let mut rate_limit_rx = self.rate_limit_rx.take().expect("rate limit rx");
        let mut budget_rx = self.budget_rx.take().expect("budget rx");
        tack_tui::terminal::set_title(&format!(
            "tack — {}",
            self.cwd
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        ));

        self.render(&mut out)?;
        // Rendering is dirty-gated: an unconditional redraw would yank the
        // terminal viewport back to the bottom, breaking scrollback browsing.
        let mut dirty = false;
        // Graceful-quit state: the first quit request with a running agent
        // cancels the run and waits (bounded) for it to settle — otherwise
        // an in-flight tool call is orphaned in the session (no result is
        // ever persisted). A second quit request forces an immediate exit.
        let mut quit_grace_started: Option<Instant> = None;
        loop {
            tokio::select! {
                input = events.next() => {
                    match input {
                        Some(InputEvent::Resize { width, height }) => {
                            self.tui.resize(width, height);
                            dirty = true;
                        }
                        Some(event) => {
                            self.handle_input(event).await;
                            // Coalesce queued input into this frame: when
                            // rendering lags behind typing (slow terminal,
                            // large transcript), handling one event per
                            // frame builds an ever-growing backlog and the
                            // screen falls seconds behind app state — keys
                            // then land on state the user can't see yet
                            // (e.g. typed chars reaching the editor while a
                            // stale permission dialog is still displayed).
                            for _ in 0..32 {
                                match events.try_next().await {
                                    Some(InputEvent::Resize { width, height }) => {
                                        self.tui.resize(width, height);
                                    }
                                    Some(event) => self.handle_input(event).await,
                                    None => break,
                                }
                            }
                            self.pop_pending_permission();
                            dirty = true;
                        }
                        None => break,
                    }
                }
                Some(app_event) = self.event_rx.recv() => {
                    self.handle_app_event(app_event).await;
                    // Collapse event bursts. Providers emit one MessageUpdate
                    // per stream delta (each carrying a full partial-message
                    // clone); rendering once per event is slower than deltas
                    // arrive, so the unbounded queue backlogs and the UI
                    // keeps dribbling stale frames long after the run ends.
                    // Drain everything pending; consecutive streaming updates
                    // collapse to the latest (each carries the full state),
                    // everything else keeps order. One render per drain.
                    let mut drained = 1usize;
                    let mut latest_update: Option<Box<AgentEvent>> = None;
                    while !self.should_quit {
                        match self.event_rx.try_recv() {
                            Some(AppEvent::Agent(event)) => {
                                drained += 1;
                                let is_stream_update = matches!(
                                    event.as_ref(),
                                    AgentEvent::MessageUpdate { .. }
                                        | AgentEvent::ToolExecutionUpdate { .. }
                                );
                                if is_stream_update {
                                    if let Some(pending) = &latest_update
                                        && !stream_update_supersedes(pending, &event)
                                    {
                                        let pending =
                                            latest_update.take().expect("checked above");
                                        self.handle_agent_event(*pending).await;
                                    }
                                    latest_update = Some(event);
                                } else {
                                    if let Some(pending) = latest_update.take() {
                                        self.handle_agent_event(*pending).await;
                                    }
                                    self.handle_agent_event(*event).await;
                                }
                            }
                            Some(app_event) => {
                                drained += 1;
                                if let Some(pending) = latest_update.take() {
                                    self.handle_agent_event(*pending).await;
                                }
                                self.handle_app_event(app_event).await;
                            }
                            None => break,
                        }
                    }
                    if let Some(pending) = latest_update.take() {
                        self.handle_agent_event(*pending).await;
                    }
                    tracing::debug!(target: "tack_tui::perf", drained, "event batch");
                    dirty = true;
                }
                Some(query) = permission_rx.recv() => {
                    // A dialog is already open: queue the query instead of
                    // replacing the dialog (which would drop its oneshot
                    // sender and silently deny that tool call).
                    if self.dialog.is_some() {
                        self.pending_permissions.push_back(query);
                    } else {
                        self.present_permission_query(query);
                    }
                    dirty = true;
                }
                Some((summary, tokens_before)) = compaction_rx.recv() => {
                    self.handle_app_event(AppEvent::CompactionSummary(summary, tokens_before)).await;
                    dirty = true;
                }
                Some(message) = budget_rx.recv() => {
                    self.notice(message, chat::NoticeKind::Warning);
                    dirty = true;
                }
                Some(note) = bg_notify_rx.recv() => {
                    self.handle_bg_notification(note).await;
                    dirty = true;
                }
                Some(msg) = rate_limit_rx.recv() => {
                    // CodeBuddy rate limit: inline notice + (gated,
                    // throttled) desktop notification.
                    self.desktop_notify("codebuddy:rate-limit", "CodeBuddy", &msg);
                    self.notice(msg, chat::NoticeKind::Warning);
                    dirty = true;
                }
                _ = tick.tick() => {
                    // Only the spinner needs periodic redraws, and only while
                    // a run is active.
                    if let Some(status) = &mut self.status {
                        status.tick();
                        dirty = true;
                    }
                    // Cron: check every ~2s; fire due jobs into the session.
                    if self.settings.features.cron
                        && self.cron_last_check.elapsed() >= Duration::from_secs(2)
                    {
                        self.cron_last_check = Instant::now();
                        let due = self.cron.take_due();
                        if !due.is_empty() {
                            dirty = true;
                        }
                        for job in due {
                            let prompt = format!("[scheduled task {} — {}]\n{}", job.id, job.schedule, job.prompt);
                            self.items.push(chat::TranscriptItem::Chat(ChatEntry::notice(
                                crate::i18n::trf(
                                    "msg.cron_fired",
                                    &[("schedule", &job.schedule), ("prompt", &job.prompt)],
                                ),
                                chat::NoticeKind::Info,
                            )));
                            if self.running {
                                self.steering.lock().await.push_back(prompt);
                            } else {
                                self.start_run(vec![prompt]).await;
                            }
                        }
                    }
                }
            }
            match quit_grace_decision(self.should_quit, self.running, quit_grace_started) {
                QuitGraceDecision::StartGrace => {
                    quit_grace_started = Some(Instant::now());
                    self.should_quit = false;
                    self.cancel.cancel();
                    self.notice(
                        crate::i18n::t(self.lang, "notice.quit_grace", &[]),
                        chat::NoticeKind::Info,
                    );
                }
                QuitGraceDecision::Break => break,
                QuitGraceDecision::Wait => {}
            }
            if dirty {
                let render_start = Instant::now();
                self.render(&mut out)?;
                // Perf probe (observability.level=debug): slow frames here
                // point at terminal write blocking / render cost; fast
                // frames + big gaps between tack_agent_core::stream flushes
                // point at provider/proxy buffering.
                tracing::debug!(
                    target: "tack_tui::perf",
                    took_ms = render_start.elapsed().as_millis() as u64,
                    "frame"
                );
                dirty = false;
            }
        }
        self.tui.stop(&mut out)?;
        // The TerminalGuard's Drop also drains; do it here too so the
        // SessionEnd hooks below run after the final frame is out.
        tack_tui::terminal::drain_frames();
        // SessionEnd hooks (fire-and-forget), then tack-ext session_shutdown.
        {
            let groups = self
                .hook_config
                .take_groups(crate::shell_hooks::HookEvent::SessionEnd);
            if !groups.is_empty() {
                let payload = serde_json::json!({
                    "session_id": self.state.session.session_id(),
                    "transcript_path": serde_json::Value::Null,
                    "cwd": self.cwd,
                    "hook_event_name": "SessionEnd",
                    "reason": "exit",
                });
                self.hook_engine.run(&groups, None, &payload).await;
            }
        }
        // tack-ext: session_shutdown + graceful plugin shutdown.
        self.extensions
            .notify(
                "session_shutdown",
                serde_json::json!({ "sessionId": self.state.session.session_id() }),
            )
            .await;
        self.extensions.shutdown().await;
        // fullscreenExitOutput=transcript: print the transcript back into
        // scrollback when leaving fullscreen (TS stopInteractiveTui).
        if self.fullscreen && self.settings.fullscreen_exit_output.as_deref() == Some("transcript")
        {
            use std::io::Write as _;
            let (width, _) = self.tui.size();
            let media = self.media();
            let mut lines: Vec<Line> = Vec::new();
            for item in &self.items {
                match item {
                    chat::TranscriptItem::Chat(entry) => {
                        lines.extend(entry.render(width, &self.theme, media));
                    }
                    chat::TranscriptItem::Tool(id) => {
                        if let Some(tool) = self.tools.get(id) {
                            lines.extend(tool.render(
                                width,
                                &self.theme,
                                self.image_protocol,
                                self.settings.image_width_cells,
                            ));
                        }
                    }
                }
            }
            for line in lines {
                let _ = writeln!(out, "{}", line.to_ansi());
            }
        }
        // --export: copy the session file on exit.
        if let Some(target) = self.flags.export.clone()
            && let Err(e) = crate::cli_flags::export_session_file(&self.state.session, &target)
        {
            eprintln!("export failed: {e}");
        }
        Ok(0)
    }

    /// Background task finished: surface in the transcript; steer a running
    /// agent, or auto-wake an idle one (settings backgroundAutoWake).
    pub async fn handle_bg_notification(&mut self, note: tack_tools::background::TaskNotification) {
        let command_summary: String = note.command.chars().take(80).collect();
        self.items
            .push(chat::TranscriptItem::Chat(ChatEntry::notice(
                crate::i18n::t(
                    self.lang,
                    "notice.bg_done",
                    &[
                        ("id", &note.task_id),
                        ("status", &note.status),
                        ("command", &command_summary),
                    ],
                ),
                chat::NoticeKind::Info,
            )));
        let wake = format!(
            "Background task {} ({}) {}. Use bash_output with task_id=\"{}\" to read its output.",
            note.task_id, command_summary, note.status, note.task_id
        );
        if self.running {
            self.steering.lock().await.push_back(wake);
        } else if self.settings.background_auto_wake {
            // Idle: auto-wake the agent (same pattern as a fired cron job)
            // so it reads the output and continues on its own.
            self.start_run(vec![wake]).await;
        }
        // Desktop notification (throttled): a background task needs attention.
        let title = crate::i18n::t(
            self.lang,
            "notify.bg_task_title",
            &[("status", &note.status)],
        );
        self.desktop_notify("background", &title, &command_summary);
    }

    /// Open the permission dialog for one query (+ desktop notification
    /// and Notification hooks). Caller must ensure no dialog is open.
    fn present_permission_query(&mut self, query: permission::PermissionQuery) {
        // Desktop notification: the agent is waiting on the user.
        let title = crate::i18n::t(self.lang, "notify.permission_title", &[]);
        self.desktop_notify("permission", &title, &query.title);
        // Notification hooks: permission prompt needs attention.
        let groups = self
            .hook_config
            .take_groups(crate::shell_hooks::HookEvent::Notification);
        if !groups.is_empty() {
            let payload = serde_json::json!({
                "session_id": self.state.session.session_id(),
                "transcript_path": serde_json::Value::Null,
                "cwd": self.cwd,
                "hook_event_name": "Notification",
                "message": query.title,
                "kind": "permission",
                "tool": query.tool_name,
            });
            let engine = self.hook_engine.clone();
            crate::task::spawn_guarded("notification-hook", async move {
                engine.run(&groups, None, &payload).await;
            });
        }
        self.dialog = Some(commands::Dialog::Permission(
            permission::PermissionDialog::with_lang(query, self.theme, self.lang),
        ));
    }

    /// Open the next queued permission query once the dialog slot is free
    /// (queries queue while any dialog is open — see the permission arm).
    fn pop_pending_permission(&mut self) {
        if self.dialog.is_none()
            && let Some(query) = self.pending_permissions.pop_front()
        {
            self.present_permission_query(query);
        }
    }

    /// A queued user message was delivered to the model: replace its dimmed
    /// `Queued` transcript echo with a regular user entry. Matches by text
    /// (FIFO); unknown deliveries (initial prompt, background notifications)
    /// leave the transcript untouched.
    fn mark_queued_delivered(&mut self, text: &str) {
        let pos = self.items.iter().position(|item| {
            matches!(
                item,
                chat::TranscriptItem::Chat(ChatEntry::Queued { text: t, .. }) if t == text
            )
        });
        if let Some(pos) = pos {
            self.items[pos] = chat::TranscriptItem::Chat(chat::ChatEntry::User {
                text: text.to_string(),
            });
            // Invalidate only the replaced entry: line_cache is index-aligned
            // with items, and a blanket clear() would re-render the whole
            // transcript (new span Arcs defeat the renderer's pointer
            // fingerprints → full-screen rewrite per delivered message).
            if let Some(entry) = self.line_cache.get_mut(pos) {
                entry.invalidate();
            }
        }
    }

    /// Editor submission: slash command, bang command, or prompt.
    pub async fn on_submit(&mut self, text: String) {
        let trimmed = text.trim();
        // Skill invocation: /skill:<name> [args] — expanded into the wrapped
        // user message (TS _expandSkillCommand), regardless of
        // enableSkillCommands (that setting only gates autocomplete).
        if trimmed.starts_with("/skill:") {
            let (skills, _) = self.load_session_skills();
            if let Some(expanded) = crate::skills::expand_skill_command(trimmed, &skills) {
                let rest = trimmed.trim_start_matches("/skill:");
                let (name, args) = match rest.find(char::is_whitespace) {
                    Some(pos) => (rest[..pos].to_string(), rest[pos..].trim().to_string()),
                    None => (rest.to_string(), String::new()),
                };
                self.items
                    .push(chat::TranscriptItem::Chat(ChatEntry::SkillInvocation {
                        name,
                        args,
                    }));
                if self.running {
                    self.steering.lock().await.push_back(expanded.clone());
                    self.items
                        .push(chat::TranscriptItem::Chat(ChatEntry::Queued {
                            text: expanded,
                            follow_up: false,
                        }));
                } else if self.compacting {
                    // Manual /compact holds the session: queue as a
                    // follow-up; the completion handler starts the run.
                    self.follow_up.lock().await.push_back(expanded.clone());
                    self.items
                        .push(chat::TranscriptItem::Chat(ChatEntry::Queued {
                            text: expanded,
                            follow_up: true,
                        }));
                } else {
                    self.start_run_with_content(tack_ai::UserContent::Text(expanded))
                        .await;
                }
                return;
            }
            // Unknown skill: pass through unchanged (TS behavior).
        }
        if let Some(command) = trimmed.strip_prefix('/') {
            self.run_command(command).await;
            return;
        }
        if let Some(command) = trimmed.strip_prefix('!') {
            let exclude = command.starts_with('!');
            let command = command.trim_start_matches('!').trim().to_string();
            self.run_bash(command, exclude).await;
            return;
        }
        if self.running {
            // Queue as steering (consumed before the next assistant turn),
            // echoed so the user can see — and recall (alt+↑) — it.
            self.steering.lock().await.push_back(text.clone());
            self.items
                .push(chat::TranscriptItem::Chat(ChatEntry::Queued {
                    text,
                    follow_up: false,
                }));
            return;
        }
        if self.compacting {
            // Manual /compact holds the session: queue as a follow-up;
            // the completion handler starts the run.
            self.follow_up.lock().await.push_back(text.clone());
            self.items
                .push(chat::TranscriptItem::Chat(ChatEntry::Queued {
                    text,
                    follow_up: true,
                }));
            return;
        }
        // A fresh user prompt resets the Stop-hook continuation guard.
        self.stop_hook_active = false;
        // UserPromptSubmit hooks: a block verdict drops the prompt;
        // additionalContext is prepended as context for this turn.
        let mut text = text;
        let prompt_groups = self
            .hook_config
            .take_groups(crate::shell_hooks::HookEvent::UserPromptSubmit);
        if !prompt_groups.is_empty() {
            let verdict = self
                .hook_engine
                .run(
                    &prompt_groups,
                    None,
                    &serde_json::json!({
                        "session_id": self.state.session.session_id(),
                        "transcript_path": serde_json::Value::Null,
                        "cwd": self.cwd,
                        "hook_event_name": "UserPromptSubmit",
                        "model": self.state.model.id,
                        "permission_mode": self.mode.lock().map(|m| m.as_str().to_string()).unwrap_or_default(),
                        "prompt": text,
                    }),
                )
                .await;
            if let Some(reason) = verdict.blocked {
                self.notice(
                    crate::i18n::t(self.lang, "notice.prompt_blocked", &[("reason", &reason)]),
                    chat::NoticeKind::Warning,
                );
                return;
            }
            if !verdict.additional_context.is_empty() {
                text = format!(
                    "<hook_additional_context>\n{}\n</hook_additional_context>\n\n{text}",
                    verdict.additional_context.join("\n")
                );
            }
        }
        // Expand @file/@image references into content blocks.
        let content = images::expand_attachments_with(
            &text,
            &self.state.model,
            &self.cwd,
            self.settings.block_images,
        );
        self.items.push(chat::TranscriptItem::Chat(ChatEntry::User {
            text: text.clone(),
        }));
        self.start_run_with_content(content).await;
    }

    /// Start a run with a prepared user message (attachments expanded).
    pub(crate) async fn start_run_with_content(&mut self, content: tack_ai::UserContent) {
        let message = AgentMessage::User(tack_ai::UserMessage {
            content,
            timestamp: tack_ai::now_millis(),
        });
        self.start_run_messages(vec![message]).await;
    }

    // -----------------------------------------------------------------
    // Agent run
    // -----------------------------------------------------------------

    /// Start an agent run. `prompts` may be empty (follow-up queue drives).
    pub(crate) async fn start_run(&mut self, prompts: Vec<String>) {
        let messages: Vec<AgentMessage> = prompts.into_iter().map(AgentMessage::user).collect();
        self.start_run_messages(messages).await;
    }

    /// Start an agent run with prebuilt prompt messages.
    async fn start_run_messages(&mut self, prompt_messages: Vec<AgentMessage>) {
        if self.running {
            return;
        }
        if self.compacting {
            // A manual /compact holds the session (its completion handler
            // appends to self.state.session): park the prompts in the
            // follow-up queue; the handler starts the run once the
            // compaction has landed.
            let mut queue = self.follow_up.lock().await;
            let mut dropped_images = 0usize;
            for message in prompt_messages {
                if let AgentMessage::User(u) = message {
                    match u.content {
                        tack_ai::UserContent::Text(text) => queue.push_back(text),
                        tack_ai::UserContent::Blocks(blocks) => {
                            // The queue is text-only: keep the text parts
                            // and COUNT attachments instead of silently
                            // dropping the whole prompt.
                            let text = blocks
                                .iter()
                                .filter_map(|b| match b {
                                    tack_ai::InputContentBlock::Text { text, .. } => {
                                        Some(text.as_str())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            dropped_images += blocks
                                .iter()
                                .filter(|b| !matches!(b, tack_ai::InputContentBlock::Text { .. }))
                                .count();
                            if !text.is_empty() {
                                queue.push_back(text);
                            }
                        }
                    }
                }
            }
            drop(queue);
            if dropped_images > 0 {
                self.notice(
                    format!(
                        "{dropped_images} attachment(s) were dropped: they cannot be \
                         queued while a compact is in flight — re-attach and resend"
                    ),
                    NoticeKind::Warning,
                );
            }
            return;
        }
        self.running = true;
        self.cancel = tokio_util::sync::CancellationToken::new();
        self.status = Some(status::StatusIndicator::working());
        self.streaming = None;
        self.stream_rev += 1;

        // Tools + MCP (mcp.json only; ACP session servers don't exist here).
        let services = tack_tools::default_services(self.cwd.clone())
            .with_background(self.background_tasks.clone())
            .with_lsp(self.lsp.clone())
            .with_checkpoints(self.checkpoints.clone())
            .with_background_tasks_enabled(self.settings.features.background_tasks);
        // New user prompt ⇒ new trust boundary: untrusted-content elevation
        // from the previous run resets.
        services
            .untrusted_seen
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let services = match self.settings.sandbox_spec(&self.cwd) {
            Some(spec) => services.with_sandbox(spec),
            None => services,
        }
        .with_web_render(self.settings.web_render_mode())
        .with_web_search(self.settings.web_search_config())
        .with_memory_dir(self.settings.memory_directory.clone())
        // ask_user: the TUI answers questions with dialogs; headless modes
        // leave the handler unset and the tool degrades in-band.
        .with_ask_user(std::sync::Arc::new(
            crate::ask_user::TuiAskUserHandler::new(self.event_tx.clone()),
        ));
        let mut tools = tack_tools::create_coding_tools(&services);
        // tack-subagents: built-in parallel sub-agent tool, with custom agent
        // definitions from .pi/agents/*.md (project dir trust-gated). The
        // concurrency cap + token budget live in a session-owned limits
        // object (created once) so they survive the per-prompt tool rebuild.
        let subagent_limits = self
            .subagent_limits
            .get_or_insert_with(|| {
                crate::subagent_tool::SubagentLimits::shared(
                    (self.settings.subagents_max_concurrent > 0)
                        .then_some(self.settings.subagents_max_concurrent),
                    (self.settings.subagents_budget_tokens > 0)
                        .then_some(self.settings.subagents_budget_tokens),
                )
            })
            .clone();
        tools.push(Arc::new(
            crate::subagent_tool::SubagentTool::new(
                self.provider.clone(),
                self.state.model.clone(),
                self.auth.clone(),
            )
            .with_cwd(self.cwd.clone())
            .with_agents(crate::agents::load_agents(
                &self.cwd,
                &self.agent_dir,
                crate::project_trust::is_trusted(&self.cwd, &self.agent_dir),
            ))
            .with_features(self.settings.features.clone())
            .with_deny_rules(crate::permissions::PermissionRules::load(
                &self.settings,
                &self.agent_dir,
            ))
            .with_memory_dir(self.settings.memory_directory.clone())
            .with_model_locks(
                self.settings.locked_provider.clone(),
                self.settings.locked_model.clone(),
            )
            .with_start_hooks(
                self.hook_config
                    .take_groups(crate::shell_hooks::HookEvent::SubagentStart),
                self.hook_engine.clone(),
            )
            .with_stop_hooks(
                self.hook_config
                    .take_groups(crate::shell_hooks::HookEvent::SubagentStop),
                self.hook_engine.clone(),
            )
            .with_shared_limits(subagent_limits)
            .with_background(self.background_tasks.clone()),
        ));
        // rpiv-todo: persistent session todo list.
        tools.push(Arc::new(tack_tools::todo::TodoTool::new(
            services.clone(),
            self.todo_state.clone(),
        )));
        // Cross-session retrieval: "how did we solve this last time?".
        tools.push(Arc::new(
            crate::session_search_tool::SessionSearchTool::new(self.agent_dir.clone()),
        ));
        // tack-ext: plugin tools join the tool set (MCP-style proxies).
        tools.extend(self.extensions.tools());
        // MCP server specs are resolved here (cheap config read); the
        // connections themselves are established INSIDE the spawned run
        // task (see below) — subprocess spawn + handshake + OAuth on a
        // slow server would otherwise freeze every prompt submission.
        let mut specs = crate::mcp_config::configured_servers(&self.cwd, &self.agent_dir);
        specs.extend(self.extensions.bundle_mcp_servers.clone());

        // Plan mode: the exit_plan_mode tool + plan-mode prompt section
        // (both assembled in the run task, after the MCP tools join).
        let in_plan_mode = *lock_recover(&self.mode) == PermissionMode::Plan;

        let state_session = Arc::new(Mutex::new(std::mem::replace(
            &mut self.state.session,
            // Placeholder while moving into the Arc; restored on completion.
            SessionManager::in_memory(&self.cwd),
        )));
        let existing = state_session.lock().await.build_session_context().messages;
        let session_id = state_session.lock().await.session_id().to_string();

        // Footer stats base for the run: totals so far + the context estimate
        // of what is being sent. The streaming message's usage is added on
        // top per update (the session itself is parked until the run ends).
        {
            let totals = state_session.lock().await.session_totals();
            self.run_stats_base = Some(self.stats_with_sampling(FooterStats {
                input: totals.input,
                output: totals.output,
                cache_read: totals.cache_read,
                cache_write: totals.cache_write,
                cost: totals.cost.total,
                context_tokens: tack_session::estimate_context_tokens(&existing).tokens,
            }));
        }

        // File checkpoints: scope to the session, start a new turn.
        if !self.settings.features.checkpoints {
            self.checkpoints.disable();
        } else {
            self.checkpoints
                .enable(self.agent_dir.join("checkpoints").join(&session_id));
            self.checkpoints.set_workdir(self.cwd.clone());
            self.checkpoints.begin_turn();
        }

        let model = self.state.model.clone();
        let thinking = self.state.thinking;
        let mode = self.mode.clone();

        // Lifecycle hooks: PreToolUse/PostToolUse via the hook engine. Runs
        // BEFORE the permission hooks so permissionDecision allow/ask and
        // updatedInput rewrites are honored there (shared hook_decisions).
        let mut hook_list: Vec<Arc<dyn tack_agent_core::AgentHooks>> = vec![
            Arc::new(crate::hooks::SessionHooks {
                session: state_session.clone(),
                model: model.clone(),
                provider: self.provider.clone(),
                auth: self.auth.clone(),
                reasoning: thinking,
                settings: self.settings.compaction,
                cancel: self.cancel.clone(),
                on_compaction: Some(self.compaction_tx.clone()),
                history: self.settings.history(&self.agent_dir),
                hook_engine: self.hook_engine.clone(),
                pre_compact: self
                    .hook_config
                    .take_groups(crate::shell_hooks::HookEvent::PreCompact),
                post_compact: self
                    .hook_config
                    .take_groups(crate::shell_hooks::HookEvent::PostCompact),
                hook_session_id: session_id.clone(),
            }),
            Arc::new(
                crate::shell_hooks::ShellHooks::new(
                    self.hook_engine.clone(),
                    self.hook_config
                        .take_groups(crate::shell_hooks::HookEvent::PreToolUse),
                    self.hook_config
                        .take_groups(crate::shell_hooks::HookEvent::PostToolUse),
                    crate::shell_hooks::HookSessionInfo {
                        session_id: session_id.clone(),
                        model: model.id.clone(),
                        permission_mode: self
                            .mode
                            .lock()
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default(),
                    },
                    self.hook_decisions.clone(),
                )
                .with_post_failure(
                    self.hook_config
                        .take_groups(crate::shell_hooks::HookEvent::PostToolUseFailure),
                ),
            ),
            Arc::new(TuiPermissionHooks {
                mode,
                allow_always: self.allow_always.clone(),
                queries: self.permission_tx.clone(),
                rules: crate::permissions::PermissionRules::load(&self.settings, &self.agent_dir),
                agent_dir: self.agent_dir.clone(),
                disable_bypass: self.settings.disable_bypass,
                untrusted_seen: services.untrusted_seen.clone(),
                hook_decisions: self.hook_decisions.clone(),
                permission_request: if self.settings.features.shell_hooks {
                    Some((
                        self.hook_engine.clone(),
                        self.hook_config
                            .take_groups(crate::shell_hooks::HookEvent::PermissionRequest),
                        session_id.clone(),
                    ))
                } else {
                    None
                },
            }),
            Arc::new(QueueHooks {
                steering: self.steering.clone(),
                follow_up: self.follow_up.clone(),
                steering_mode: self.settings.steering_mode.clone(),
                follow_up_mode: self.settings.follow_up_mode.clone(),
            }),
        ];
        // Token budget enforcement: pause stops the run at the next turn
        // boundary, downgrade switches to the budget model.
        if let Some(budget) = self.settings.token_budget {
            let action = self
                .settings
                .token_budget_action
                .clone()
                .unwrap_or_else(|| "warn".to_string());
            if action != "warn" {
                let downgrade_model = match &self.settings.budget_downgrade_model {
                    Some(entry) => entry.split_once('/').and_then(|(p, id)| {
                        crate::model::resolve_model(p, Some(id), &self.agent_dir).ok()
                    }),
                    None => crate::model::resolve_fallback_models(
                        &self.settings.fallback_models,
                        &model,
                        &self.agent_dir,
                    )
                    .into_iter()
                    .last(),
                };
                hook_list.push(Arc::new(crate::hooks::BudgetHooks {
                    session: state_session.clone(),
                    budget,
                    action,
                    downgrade_model,
                    fired: std::sync::Arc::new(std::sync::Mutex::new(false)),
                    on_trigger: Some(self.budget_tx.clone()),
                }));
            }
        }
        // tack-ext: per-plugin hook bridges (tool_call interception).
        hook_list.extend(self.extensions.hooks());
        let hooks: Arc<dyn tack_agent_core::AgentHooks> = Arc::new(HooksChain::new(hook_list));

        // Values the run task owns from here on. MCP connection setup
        // (subprocess spawn + handshake + OAuth) and everything derived
        // from it (tool filtering/pool, system prompt, agent loop) moved
        // INTO the task: on the UI loop a slow MCP server froze every
        // prompt submission for seconds.
        let settings = self.settings.clone();
        let flags = self.flags.clone();
        let custom_system_prompt = self.custom_system_prompt.clone();
        // SessionStart hook output joins the first run's system prompt.
        let session_context = self.session_context.take();
        let fallback_models = crate::model::resolve_fallback_models(
            &self.settings.fallback_models,
            &model,
            &self.agent_dir,
        );
        let plan_mode_handle = self.mode.clone();
        let plan_permission_tx = self.permission_tx.clone();
        let provider = self.provider.clone();
        let auth = self.auth.clone();
        let agent_dir = self.agent_dir.clone();
        let cwd = self.cwd.clone();
        let cancel = self.cancel.clone();
        let tx = self.event_tx.clone();
        let session_handle = state_session.clone();
        tokio::spawn(async move {
            use futures_util::FutureExt as _;

            let tx_work = tx.clone();
            let work = async {
                // MCP servers connect here, off the UI loop; the status
                // spinner (working) stays visible meanwhile.
                let connections = if specs.is_empty() {
                    Vec::new()
                } else {
                    let event_tx = tx_work.clone();
                    let usage_sink: crate::mcp_sampling::SamplingUsageSink = Arc::new(move |m| {
                        let _ = event_tx.send(AppEvent::McpSamplingDone {
                            usage: m.usage.clone(),
                            model: m.model.clone(),
                        });
                    });
                    let callbacks = crate::mcp_config::client_callbacks(
                        &settings,
                        Some(&crate::mcp_config::SamplingLlm {
                            provider: provider.clone(),
                            model: model.clone(),
                            auth: auth.clone(),
                        }),
                        usage_sink,
                        crate::mcp_elicitation::InteractionMode::Tui,
                        Some(tx_work.clone()),
                    );
                    crate::mcp_oauth::connect_all_oauth(specs, &agent_dir, true, callbacks).await
                };
                tools.extend(tack_tools::mcp::mcp_tools_with(
                    &connections,
                    Some(services.untrusted_seen.clone()),
                ));
                let tools = crate::cli_flags::filter_tools(tools, &flags);
                // features.*: disabled features' tools are never registered.
                let tools = crate::cli_flags::filter_feature_tools(tools, &settings.features);
                // settings.defaultTools: built-in tool allowlist (MCP tools
                // unaffected) + optional powershell tool opt-in.
                let tools = crate::cli_flags::apply_default_tools(
                    tools,
                    &settings.default_tools,
                    &services,
                );

                // Client-side tool search: MCP tools beyond mcpDeferThreshold
                // defer to a pool; tool_search activates them on demand.
                let (tools, mut tool_pool) =
                    crate::cli_flags::split_for_tool_search(tools, settings.mcp_defer_threshold);
                // Plan mode: the exit_plan_mode tool + plan-mode prompt section.
                let mut tools = if in_plan_mode {
                    let mut tools = tools;
                    tools.push(Arc::new(plan_mode::ExitPlanModeTool::new(
                        plan_mode_handle,
                        plan_permission_tx,
                        agent_dir.join("plans"),
                    )));
                    tools
                } else {
                    tools
                };

                let selected: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
                // /context: fixed-cost segments of the prompt (chars/4 ≈ tokens).
                let tools_chars: usize = tools
                    .iter()
                    .map(|t| t.parameters_schema().to_string().chars().count())
                    .sum();
                let mut system_prompt = crate::print_mode::assemble_system_prompt(
                    &cwd,
                    &agent_dir,
                    &settings,
                    custom_system_prompt.as_deref(),
                    &selected,
                    &flags,
                );
                if in_plan_mode {
                    system_prompt.push_str(plan_mode::PLAN_MODE_PROMPT);
                }
                if let Some(context) = session_context {
                    system_prompt = format!(
                        "{system_prompt}\n\n# Session context (SessionStart hook)\n{context}"
                    );
                }
                let system_chars = system_prompt.chars().count();
                // Transcript-declared tool state (upstream #9548): re-activate
                // pool tools the session had activated when it was last written.
                tack_agent_core::agent_loop::restore_tools_from_transcript(
                    &existing,
                    &mut tools,
                    &mut tool_pool,
                );
                // The app owns the connections (dropping them closes the
                // servers); the /mcp picker and /context read these.
                let _ = tx_work.send(AppEvent::RunContextReady {
                    connections,
                    tools_chars,
                    system_chars,
                });

                let config = AgentLoopConfig {
                    model,
                    provider,
                    hooks,
                    tool_execution: ToolExecutionMode::Parallel,
                    reasoning: thinking,
                    auth,
                    max_tokens: None,
                    temperature: None,
                    session_id: Some(session_id),
                    cache_retention: settings.cache_retention_mode(),
                    fallback_models,
                    tool_pool,
                    retry_cancel: None,
                };
                let context = AgentContext {
                    system_prompt: Some(system_prompt),
                    messages: existing,
                    tools,
                };

                let mut stream = agent_loop(prompt_messages, context, config, cancel);
                while let Some(event) = stream.next().await {
                    // Persist completed messages.
                    if let AgentEvent::MessageEnd { message } = &event
                        && !matches!(message, AgentMessage::Custom(_))
                        && let Err(e) = session_handle.lock().await.append_message(message.clone())
                    {
                        tracing::warn!("persist failed: {e}");
                    }
                    if tx_work.send(AppEvent::Agent(Box::new(event))).is_err() {
                        return;
                    }
                }
                let _ = stream.result().await;
            };
            if std::panic::AssertUnwindSafe(work)
                .catch_unwind()
                .await
                .is_err()
            {
                // The panic hook already wrote crash.log; surface the failure
                // and keep the UI alive instead of freezing mid-run.
                tracing::error!("agent pump panicked; details in crash.log");
                let _ = tx.send(AppEvent::Notice(
                    "agent run crashed unexpectedly (details in crash.log)".to_string(),
                    NoticeKind::Error,
                ));
            }
            let _ = tx.send(AppEvent::RunFinished);
            // Hand the session back to the app — even after a pump panic, or
            // the app keeps the in-memory placeholder and loses the run.
            let session = std::mem::replace(
                &mut *session_handle.lock().await,
                SessionManager::in_memory(std::path::Path::new("")),
            );
            let _ = tx.send(AppEvent::SessionBack(Box::new(session)));
        });
    }

    /// Footer stats from session totals plus accrued MCP sampling usage
    /// (sampling runs outside the agent loop, so session_totals can't see
    /// it; it still counts against the session's spend).
    pub(crate) fn stats_with_sampling(&self, mut s: footer::FooterStats) -> footer::FooterStats {
        s.input += self.mcp_sampling_stats.input;
        s.output += self.mcp_sampling_stats.output;
        s.cache_read += self.mcp_sampling_stats.cache_read;
        s.cache_write += self.mcp_sampling_stats.cache_write;
        s.cost += self.mcp_sampling_stats.cost;
        s
    }

    /// Fold one finished MCP sampling call into the live stats + audit log.
    pub(crate) fn handle_mcp_sampling_done(&mut self, usage: &tack_ai::Usage, model: &str) {
        let fold = |s: &mut footer::FooterStats| {
            s.input += usage.input;
            s.output += usage.output;
            s.cache_read += usage.cache_read;
            s.cache_write += usage.cache_write;
            s.cost += usage.cost.total;
        };
        fold(&mut self.mcp_sampling_stats);
        fold(&mut self.last_stats);
        if let Some(base) = &mut self.run_stats_base {
            fold(base);
        }
        // Idle recompute derives from session totals (+ the accumulator), so
        // the key must not suppress it.
        self.stats_key = None;
        tracing::info!(
            target: "tack_app::mcp_sampling",
            model,
            input = usage.input,
            output = usage.output,
            cache_read = usage.cache_read,
            cost = usage.cost.total,
            "MCP sampling usage"
        );
        self.notice(
            format!(
                "MCP sampling: {model} used {} tokens (in {}, out {})",
                usage.total_tokens, usage.input, usage.output
            ),
            NoticeKind::Info,
        );
    }

    /// Open the InputDialog for the elicitation's current field.
    pub(crate) fn open_elicitation_dialog(
        &mut self,
        pending: &crate::mcp_elicitation::PendingElicitation,
    ) {
        let Some(field) = pending.current() else {
            return;
        };
        let total = pending.remaining.len() + pending.collected.len();
        let step = pending.collected.len() + 1;
        let required = if field.required {
            "required"
        } else {
            "optional"
        };
        let title = format!(
            "MCP {}: {} — field {step}/{total} {} ({}, {})",
            pending.server, pending.message, field.name, field.kind, required
        );
        let placeholder = match (&field.description, field.choices.is_empty()) {
            (Some(d), true) => d.clone(),
            (Some(d), false) => format!("{d} — one of: {}", field.choices.join(", ")),
            (None, false) => format!("one of: {}", field.choices.join(", ")),
            (None, true) => String::new(),
        };
        self.dialog = Some(commands::Dialog::Input(commands::InputDialog::new(
            title,
            placeholder,
            self.theme,
        )));
    }

    /// Continue the per-field walk after one accepted answer.
    pub(crate) fn continue_elicitation(
        &mut self,
        pending: crate::mcp_elicitation::PendingElicitation,
    ) {
        if pending.current().is_some() {
            self.open_elicitation_dialog(&pending);
            self.pending_elicitation = Some(pending);
        } else {
            pending.finish();
        }
    }

    /// Open the dialog for the ask_user walk's current question:
    /// multiple-choice questions get a SelectDialog with a trailing
    /// "Other…" escape, free-text questions (and the Other escape itself)
    /// an InputDialog.
    pub(crate) fn open_ask_user_dialog(&mut self, pending: &crate::ask_user::PendingAskUser) {
        use tack_tui::components::select_list::SelectItem;
        let Some(question) = pending.current() else {
            return;
        };
        let (step, total) = pending.progress();
        let step_label = crate::i18n::trf(
            "ask_user.step",
            &[("step", &step.to_string()), ("total", &total.to_string())],
        );
        let title = match &question.header {
            Some(header) if !header.trim().is_empty() => {
                format!("{step_label} [{header}] {}", question.question)
            }
            _ => format!("{step_label} {}", question.question),
        };
        if pending.awaiting_custom || question.options.is_none() {
            self.dialog = Some(commands::Dialog::Input(commands::InputDialog::new(
                title,
                crate::i18n::tr("ask_user.placeholder"),
                self.theme,
            )));
            return;
        }
        let options = question.options.as_deref().unwrap_or_default();
        let mut items: Vec<SelectItem> = options
            .iter()
            .map(|option| {
                let mut item = SelectItem::new(option.label.clone(), option.label.clone());
                item.description = option.description.clone();
                item
            })
            .collect();
        items.push(SelectItem::new(
            crate::i18n::tr("ask_user.other"),
            crate::ask_user::CUSTOM_ANSWER_VALUE,
        ));
        self.dialog = Some(commands::Dialog::Select(commands::SelectDialog::new(
            title,
            items,
            commands::SelectPurpose::AskUser,
            self.theme,
        )));
    }

    /// Continue the per-question walk after one accepted answer.
    pub(crate) fn continue_ask_user(&mut self, pending: crate::ask_user::PendingAskUser) {
        if pending.current().is_some() {
            self.open_ask_user_dialog(&pending);
            self.pending_ask_user = Some(pending);
        } else {
            pending.finish();
        }
    }

    /// Live footer stats during a run: base snapshot + the streaming
    /// message's usage. `finish` additionally folds the message into the
    /// base so later turns in the same run accumulate (matching
    /// session_totals semantics).
    fn update_run_stats(&mut self, a: &tack_ai::AssistantMessage, finish: bool) {
        let Some(base) = &self.run_stats_base else {
            return;
        };
        let s = fold_run_stats(base, a);
        self.last_stats = s.clone();
        if finish && let Some(base) = &mut self.run_stats_base {
            *base = s;
        }
    }

    pub async fn handle_agent_event(&mut self, event: AgentEvent) {
        // tack-ext: capture the lifecycle name + payload before consumption.
        // With no plugins loaded, skip the JSON serialization of every
        // event entirely (event_to_json deep-clones the payload).
        let ext_forward: Option<(&'static str, serde_json::Value)> = if self.extensions.is_empty() {
            None
        } else {
            ext_event_name(&event).map(|name| (name, crate::rpc::event_to_json(&event)))
        };
        match event {
            AgentEvent::AgentStart => {
                tack_tui::terminal::set_progress(true);
                self.status = Some(status::StatusIndicator::working());
            }
            AgentEvent::AgentEnd { .. } => {
                tack_tui::terminal::set_progress(false);
            }
            AgentEvent::ModelFallback { from, to, reason } => {
                self.state.model = to.clone();
                self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.model_fallback",
                        &[
                            ("from", &format!("{}/{}", from.provider, from.id)),
                            ("to", &format!("{}/{}", to.provider, to.id)),
                            ("reason", &reason.chars().take(120).collect::<String>()),
                        ],
                    ),
                    NoticeKind::Warning,
                );
            }
            AgentEvent::TurnStart | AgentEvent::TurnEnd { .. } => {}
            AgentEvent::MessageStart { message } => {
                match message {
                    AgentMessage::User(u) => {
                        // A queued steering/follow-up message reached the model.
                        let text = match &u.content {
                            tack_ai::UserContent::Text(t) => t.clone(),
                            tack_ai::UserContent::Blocks(blocks) => blocks
                                .iter()
                                .filter_map(|b| match b {
                                    tack_ai::InputContentBlock::Text { text, .. } => {
                                        Some(text.as_str())
                                    }
                                    _ => None,
                                })
                                .collect(),
                        };
                        self.mark_queued_delivered(&text);
                    }
                    AgentMessage::Assistant(a) => {
                        // A fresh attempt is producing — clear any retry countdown.
                        if self
                            .status
                            .as_ref()
                            .is_some_and(status::StatusIndicator::is_retrying)
                        {
                            self.status = Some(status::StatusIndicator::working());
                        }
                        self.update_run_stats(&a, false);
                        // Move (not clone): the event was consumed by value.
                        self.streaming = Some(a);
                        self.stream_rev += 1;
                    }
                    _ => {}
                }
            }
            AgentEvent::MessageUpdate { message, .. } => {
                if let AgentMessage::Assistant(a) = message {
                    // Update tool args as they stream in.
                    for block in &a.content {
                        if let tack_ai::ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } = block
                            && !self.tools.contains_key(id)
                        {
                            self.register_tool(tool_render::ToolEntry {
                                tool_call_id: id.clone(),
                                tool_name: name.clone(),
                                args_fp: tool_render::args_fingerprint(arguments),
                                args: arguments.clone(),
                                state: tool_render::ToolState::Running {
                                    partial_output: String::new(),
                                },
                                expanded: false,
                            });
                        } else if let tack_ai::ContentBlock::ToolCall { id, arguments, .. } = block
                            && let Some(tool) = self.tools.get_mut(id)
                        {
                            // Change detection without re-walking BOTH
                            // Values per stream delta: raw-string args
                            // (the streaming common case) compare by
                            // length first — args stream in append-only,
                            // so an equal length means unchanged — with
                            // an exact memcmp guarding a non-streaming
                            // same-length replacement. Parsed (object)
                            // args compare against the fingerprint cached
                            // at the last update, so only the NEW side
                            // pays the O(args) walk. Clone only on real
                            // change (the render cache keys running cards
                            // on partial-output length only, so a changed
                            // args header must drop the stale entry).
                            let changed = match (tool.args.as_str(), arguments.as_str()) {
                                (Some(old), Some(new)) => old.len() != new.len() || old != new,
                                _ => tool_render::args_fingerprint(arguments) != tool.args_fp,
                            };
                            if changed {
                                tool.args_fp = tool_render::args_fingerprint(arguments);
                                tool.args = arguments.clone();
                                self.tool_render_cache.remove(id);
                                self.tool_heights.remove(id);
                            }
                        }
                    }
                    self.update_run_stats(&a, false);
                    // Move (not clone): the event was consumed by value —
                    // one AssistantMessage moves into `streaming` per
                    // update instead of a full deep-clone per update.
                    self.streaming = Some(a);
                    self.stream_rev += 1;
                }
            }
            AgentEvent::MessageEnd { message } => {
                if let AgentMessage::Assistant(a) = message {
                    self.streaming = None;
                    self.stream_rev += 1;
                    self.update_run_stats(&a, true);
                    // showCacheMissNotices: surface prompt-cache misses on
                    // non-trivial contexts (TS cache-miss notices).
                    if self.settings.show_cache_miss_notices
                        && a.stop_reason == tack_ai::StopReason::Stop
                        && a.usage.cache_read == 0
                        && a.usage.input >= 1000
                    {
                        self.notice(
                            crate::i18n::t(
                                self.lang,
                                "msg.cache_miss",
                                &[("count", &a.usage.input.to_string())],
                            ),
                            NoticeKind::Warning,
                        );
                    }
                    self.items
                        .push(chat::TranscriptItem::Chat(ChatEntry::Assistant {
                            message: a,
                            streaming: false,
                        }));
                    // No line_cache.clear(): pushing keeps the index
                    // alignment intact and earlier entries are immutable, so
                    // their cached renders (and the renderer's pointer
                    // fingerprints) stay valid — only the new item renders.
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                if let Some(tool) = self.tools.get_mut(&tool_call_id) {
                    if tool_render::args_fingerprint(&args) != tool.args_fp {
                        tool.args_fp = tool_render::args_fingerprint(&args);
                        tool.args = args;
                        self.tool_render_cache.remove(&tool_call_id);
                        self.tool_heights.remove(&tool_call_id);
                    }
                } else {
                    self.register_tool(tool_render::ToolEntry {
                        args_fp: tool_render::args_fingerprint(&args),
                        tool_call_id,
                        tool_name,
                        args,
                        state: tool_render::ToolState::Running {
                            partial_output: String::new(),
                        },
                        expanded: false,
                    });
                }
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                let text = partial_result
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        tack_ai::InputContentBlock::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Some(tool) = self.tools.get_mut(&tool_call_id)
                    && let tool_render::ToolState::Running { partial_output } = &mut tool.state
                {
                    *partial_output = text;
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                // rpiv-todo: refresh the shared panel state from the result.
                if let Some(tool) = self.tools.get(&tool_call_id)
                    && tool.tool_name == "todo"
                    && let Some(todos) = result.details.get("todos")
                {
                    *self.todo_state.lock().await = tack_tools::todo::TodoState::from_json(todos);
                }
                if let Some(tool) = self.tools.get_mut(&tool_call_id) {
                    tool.state = tool_render::ToolState::Done { result, is_error };
                    // Only this tool's card re-renders (its cache key moves
                    // from partial-output length to the Done sentinel); chat
                    // entries don't depend on tool state — no line_cache
                    // clear, or every tool call would rewrite the screen.
                }
            }
        }
        // tack-ext: fan the event out to subscribed plugins.
        if let Some((name, payload)) = ext_forward {
            self.extensions.notify(name, payload).await;
        }
    }
}

/// tack-ext lifecycle name for an agent event (None = not forwarded;
/// message_update is subscription-gated downstream anyway).
fn ext_event_name(event: &AgentEvent) -> Option<&'static str> {
    match event {
        AgentEvent::AgentStart => Some("agent_start"),
        AgentEvent::AgentEnd { .. } => Some("agent_end"),
        AgentEvent::TurnStart => Some("turn_start"),
        AgentEvent::TurnEnd { .. } => Some("turn_end"),
        AgentEvent::MessageStart { .. } => Some("message_start"),
        AgentEvent::MessageUpdate { .. } => Some("message_update"),
        AgentEvent::MessageEnd { .. } => Some("message_end"),
        AgentEvent::ToolExecutionStart { .. } => Some("tool_execution_start"),
        AgentEvent::ToolExecutionEnd { .. } => Some("tool_execution_end"),
        _ => None,
    }
}

/// Steering/follow-up queue hooks for the TUI run.
struct QueueHooks {
    steering: Arc<Mutex<VecDeque<String>>>,
    follow_up: Arc<Mutex<VecDeque<String>>>,
    /// "all" drains the queue; "one-at-a-time" injects only the oldest
    /// message per drain point (TS PendingMessageQueue semantics).
    steering_mode: Option<String>,
    follow_up_mode: Option<String>,
}

impl std::fmt::Debug for QueueHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueHooks").finish()
    }
}

#[async_trait::async_trait]
impl tack_agent_core::AgentHooks for QueueHooks {
    async fn steering_messages(&self) -> Vec<AgentMessage> {
        let mut queue = self.steering.lock().await;
        if self.steering_mode.as_deref() == Some("one-at-a-time") {
            queue
                .pop_front()
                .into_iter()
                .map(AgentMessage::user)
                .collect()
        } else {
            queue.drain(..).map(AgentMessage::user).collect()
        }
    }
    async fn follow_up_messages(&self) -> Vec<AgentMessage> {
        let mut queue = self.follow_up.lock().await;
        if self.follow_up_mode.as_deref() == Some("one-at-a-time") {
            queue
                .pop_front()
                .into_iter()
                .map(AgentMessage::user)
                .collect()
        } else {
            queue.drain(..).map(AgentMessage::user).collect()
        }
    }
}

#[cfg(test)]
mod quit_grace_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn first_quit_request_while_running_starts_grace() {
        assert_eq!(
            quit_grace_decision(true, true, None),
            QuitGraceDecision::StartGrace
        );
    }

    #[test]
    fn quit_request_while_idle_exits_immediately() {
        assert_eq!(
            quit_grace_decision(true, false, None),
            QuitGraceDecision::Break
        );
    }

    #[test]
    fn second_quit_request_during_grace_forces_exit() {
        assert_eq!(
            quit_grace_decision(true, true, Some(Instant::now())),
            QuitGraceDecision::Break
        );
    }

    #[test]
    fn grace_waits_until_run_settles_or_window_expires() {
        let started = Instant::now();
        assert_eq!(
            quit_grace_decision(false, true, Some(started)),
            QuitGraceDecision::Wait
        );
        // Run settled.
        assert_eq!(
            quit_grace_decision(false, false, Some(started)),
            QuitGraceDecision::Break
        );
        // Window expired with the run still going.
        let expired = Instant::now() - QUIT_GRACE_WINDOW - std::time::Duration::from_secs(1);
        assert_eq!(
            quit_grace_decision(false, true, Some(expired)),
            QuitGraceDecision::Break
        );
    }
}
