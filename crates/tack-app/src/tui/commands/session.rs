//! Session lifecycle commands (`/new`, `/resume`, `/tree`, `/share`, ...).

use tack_session::SessionManager;
use tack_tui::components::select_list::SelectItem;

use super::super::chat::TranscriptItem;
use super::super::export::share_via_gist;
use super::super::{ChatEntry, NoticeKind, TuiApp};
use super::{
    Dialog, SelectDialog, SelectPurpose, SessionDialog, TreeDialog, TreeFilter, assistant_notice,
    build_tree_items_filtered,
};

impl TuiApp {
    pub(crate) fn command_new(&mut self) {
        // Close the old session's codebuddy CLI (registry is keyed by
        // session id; /new abandons the old key).
        let old_session_id = self.state.session.session_id().to_string();
        tokio::spawn(async move {
            tack_ai::codebuddy::close_session(&old_session_id).await;
        });
        match SessionManager::create(
            &self.cwd,
            Some(tack_session::default_session_dir(
                &self.cwd,
                &self.agent_dir,
            )),
        ) {
            Ok(session) => {
                self.state.session = session;
                self.items.clear();
                self.tools.clear();
                self.tool_order.clear();
                self.line_cache.clear();
                self.welcome_banner();
            }
            Err(e) => self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.session_create_failed",
                    &[("error", &e.to_string())],
                ),
                NoticeKind::Error,
            ),
        }
    }

    /// Open the session picker. Returns false (with a notice) when there is
    /// nothing to resume.
    pub fn command_resume(&mut self) -> bool {
        let dir = tack_session::default_session_dir(&self.cwd, &self.agent_dir);
        if tack_session::list_sessions(&dir).is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.no_previous_sessions", &[]),
                NoticeKind::Info,
            );
            return false;
        }
        let active = self.state.session.session_file().map(|p| p.to_path_buf());
        self.dialog = Some(Dialog::Sessions(SessionDialog::new(
            dir, active, self.theme,
        )));
        true
    }

    /// /rewind: checkpoint navigation — jump back to any earlier turn. Every
    /// user message is a checkpoint (the session tree branches there); the
    /// jump summarizes the abandoned branch (branch_summary) before moving.
    /// /checkpoints [restore <turn>] — file-level rollback (complements
    /// /rewind, which only forks the conversation tree).
    pub(crate) fn command_checkpoints(&mut self, args: &str) {
        if !self.settings.features.checkpoints {
            self.notice(
                crate::i18n::t(self.lang, "msg.checkpoints_disabled", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let mut parts = args.split_whitespace();
        let turn_arg = match parts.next() {
            Some("restore") => parts.next().map(str::to_string),
            Some(other) => Some(other.to_string()),
            None => None,
        };
        if let Some(turn_arg) = turn_arg {
            let turn: u64 = match turn_arg.parse() {
                Ok(t) => t,
                Err(_) => {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.invalid_turn", &[("arg", &turn_arg)]),
                        NoticeKind::Warning,
                    );
                    return;
                }
            };
            match self.checkpoints.restore(turn) {
                Ok(restored) => {
                    let mut lines = vec![crate::i18n::t(
                        self.lang,
                        "msg.restored_header",
                        &[
                            ("turn", &turn.to_string()),
                            ("count", &restored.len().to_string()),
                        ],
                    )];
                    for (path, written) in &restored {
                        let action = if *written {
                            crate::i18n::t(self.lang, "msg.restored_action", &[])
                        } else {
                            crate::i18n::t(self.lang, "msg.deleted_action", &[])
                        };
                        lines.push(format!("  {} — {action}", path.display()));
                    }
                    self.notice(lines.join("\n"), NoticeKind::Info);
                }
                Err(e) => self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.restore_failed",
                        &[("error", &e.to_string())],
                    ),
                    NoticeKind::Error,
                ),
            }
            return;
        }

        let turns = self.checkpoints.list();
        if turns.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.no_file_checkpoints", &[]),
                NoticeKind::Info,
            );
            return;
        }
        let mut text = crate::i18n::t(self.lang, "msg.checkpoints_header", &[]);
        text.push_str("\n\n");
        for turn in turns.iter().take(20) {
            let names: Vec<String> = turn
                .files
                .iter()
                .take(5)
                .map(|f| {
                    f.path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                })
                .collect();
            let more = if turn.files.len() > 5 {
                crate::i18n::t(
                    self.lang,
                    "msg.more_files",
                    &[("count", &(turn.files.len() - 5).to_string())],
                )
            } else {
                String::new()
            };
            text.push_str(&crate::i18n::t(
                self.lang,
                "msg.checkpoint_row",
                &[
                    ("turn", &turn.turn.to_string()),
                    ("count", &turn.files.len().to_string()),
                    ("names", &names.join(", ")),
                    ("more", &more),
                ],
            ));
            text.push('\n');
        }
        text.push_str(&crate::i18n::t(self.lang, "msg.checkpoints_footer", &[]));
        self.items
            .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
    }

    /// /memory [forget <name> [project|user]] [edit [<name>] [project|user]]
    /// — inspect the persistent memory scopes (project = this repository,
    /// shared across worktrees; user = cross-project).
    pub(crate) fn command_memory(&mut self, args: &str) {
        if !self.settings.features.memory {
            self.notice(
                crate::i18n::t(self.lang, "msg.memory_disabled", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let dirs = tack_tools::memory::resolve_dirs(
            &self.agent_dir,
            &self.cwd,
            self.settings.memory_directory.as_deref(),
        );
        let mut parts = args.split_whitespace();
        match parts.next() {
            Some("forget") => {
                let Some(name) = parts.next().map(|n| n.trim_end_matches(".md")) else {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.memory_forget_usage", &[]),
                        NoticeKind::Warning,
                    );
                    return;
                };
                let Some(dir) = self.memory_scope_dir(&dirs, parts.next(), Some(name)) else {
                    return;
                };
                let path = dir.join(format!("{name}.md"));
                if !name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    || !path.exists()
                {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.no_memory_named", &[("name", name)]),
                        NoticeKind::Warning,
                    );
                    return;
                }
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        if let Err(e) = tack_tools::memory::rebuild_index(dir) {
                            self.notice(
                                crate::i18n::t(
                                    self.lang,
                                    "msg.memory_index_failed",
                                    &[("error", &e.to_string())],
                                ),
                                NoticeKind::Warning,
                            );
                        } else {
                            self.notice(
                                crate::i18n::t(self.lang, "msg.memory_deleted", &[("name", name)]),
                                NoticeKind::Info,
                            );
                        }
                    }
                    Err(e) => self.notice(
                        crate::i18n::t(
                            self.lang,
                            "msg.memory_delete_failed",
                            &[("name", name), ("error", &e.to_string())],
                        ),
                        NoticeKind::Error,
                    ),
                }
            }
            Some("edit") => {
                // `/memory edit user` opens the user scope's MEMORY.md; a
                // scope keyword in first position is never a memory name.
                let (name, scope) = match parts.next() {
                    Some(scope @ ("project" | "user")) => (None, Some(scope)),
                    other => (other.map(|n| n.trim_end_matches(".md")), parts.next()),
                };
                let Some(dir) = self.memory_scope_dir(&dirs, scope, name) else {
                    return;
                };
                let path = match name {
                    Some(name) => dir.join(format!("{name}.md")),
                    None => dir.join("MEMORY.md"),
                };
                if !path.exists() {
                    self.notice(
                        crate::i18n::t(
                            self.lang,
                            "msg.no_memory_named",
                            &[("name", name.unwrap_or("MEMORY.md"))],
                        ),
                        NoticeKind::Warning,
                    );
                    return;
                }
                let edited = self.edit_file_in_external_editor(&path);
                // Editing a memory file may change its description → rebuild
                // the scope's index. (Bare `/memory edit` opens MEMORY.md
                // itself; rebuilding would regenerate away the user's edits.)
                if edited
                    && name.is_some()
                    && let Err(e) = tack_tools::memory::rebuild_index(dir)
                {
                    self.notice(
                        crate::i18n::t(
                            self.lang,
                            "msg.memory_index_failed",
                            &[("error", &e.to_string())],
                        ),
                        NoticeKind::Warning,
                    );
                }
            }
            _ => {
                let mut sections = Vec::new();
                for (label, dir) in [("Project", &dirs.project), ("User", &dirs.user)] {
                    if let Some(index) = tack_tools::memory::read_index(dir) {
                        sections.push(format!(
                            "{}\n\n{}",
                            crate::i18n::t(
                                self.lang,
                                "msg.memory_scope_header",
                                &[("scope", label), ("dir", &dir.display().to_string()),]
                            ),
                            index.text
                        ));
                    }
                }
                if sections.is_empty() {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.no_memories", &[]),
                        NoticeKind::Info,
                    );
                } else {
                    let text = format!(
                        "{}\n\n{}",
                        sections.join("\n\n"),
                        crate::i18n::t(self.lang, "msg.memory_footer", &[])
                    );
                    self.items
                        .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
                }
            }
        }
    }

    /// Scope argument → directory. Without an explicit scope: project when
    /// the named file exists there (or no name given), else user.
    fn memory_scope_dir<'a>(
        &mut self,
        dirs: &'a tack_tools::memory::MemoryDirs,
        scope: Option<&str>,
        name: Option<&str>,
    ) -> Option<&'a std::path::PathBuf> {
        match scope {
            None => Ok(match name {
                Some(n) if !dirs.project.join(format!("{n}.md")).exists() => &dirs.user,
                _ => &dirs.project,
            }),
            Some("project") => Ok(&dirs.project),
            Some("user") => Ok(&dirs.user),
            Some(other) => Err(other),
        }
        .map_err(|scope| {
            self.notice(
                crate::i18n::t(self.lang, "msg.memory_scope_unknown", &[("scope", scope)]),
                NoticeKind::Warning,
            );
        })
        .ok()
    }

    /// /search <query> — full-text search across all stored sessions.
    pub(crate) fn command_search(&mut self, args: &str) {
        let query = args.trim();
        if query.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.usage_search", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let hits = tack_session::search_sessions(&self.agent_dir, query, 10);
        if hits.is_empty() {
            self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.no_sessions_match",
                    &[("query", &format!("{query:?}"))],
                ),
                NoticeKind::Info,
            );
            return;
        }
        let mut text = crate::i18n::t(
            self.lang,
            "msg.sessions_matching",
            &[
                ("query", &format!("{query:?}")),
                ("count", &hits.len().to_string()),
            ],
        );
        text.push_str("\n\n");
        for hit in &hits {
            let title = hit
                .name
                .clone()
                .or(hit.first_prompt.clone())
                .unwrap_or_else(|| hit.session_id.chars().take(8).collect());
            let time = &hit.timestamp[..10.min(hit.timestamp.len())];
            text.push_str(&crate::i18n::t(
                self.lang,
                "msg.search_row",
                &[
                    ("title", &title),
                    ("time", time),
                    ("count", &hit.match_count.to_string()),
                ],
            ));
            text.push('\n');
            for (role, snippet) in &hit.snippets {
                text.push_str(&format!("- *{role}*: {snippet}\n"));
            }
            text.push('\n');
        }
        text.push_str(&crate::i18n::t(self.lang, "msg.search_footer", &[]));
        self.items
            .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
    }

    /// /cron — manage scheduled prompts (fired into this session).
    pub(crate) fn command_cron(&mut self, args: &str) {
        if !self.settings.features.cron {
            self.notice(
                crate::i18n::t(self.lang, "msg.cron_disabled", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let mut parts = args.trim().splitn(2, char::is_whitespace);
        match (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or("").trim(),
        ) {
            ("add", rest) if !rest.is_empty() => {
                // Schedule is "every <dur>" (2 tokens) or a 5-field cron expr.
                let tokens: Vec<&str> = rest.split_whitespace().collect();
                let schedule_len = if tokens.first() == Some(&"every") {
                    2
                } else {
                    5
                };
                if tokens.len() <= schedule_len {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.cron_usage", &[]),
                        NoticeKind::Warning,
                    );
                    return;
                }
                let schedule = tokens[..schedule_len].join(" ");
                let prompt = tokens[schedule_len..].join(" ");
                match self.cron.add(&schedule, &prompt) {
                    Ok(id) => self.notice(
                        crate::i18n::t(
                            self.lang,
                            "msg.cron_scheduled",
                            &[("id", &id), ("schedule", &schedule), ("prompt", &prompt)],
                        ),
                        NoticeKind::Info,
                    ),
                    Err(e) => self.notice(e, NoticeKind::Warning),
                }
            }
            ("remove", id) if !id.is_empty() => {
                if self.cron.remove(id) {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.cron_removed", &[("id", id)]),
                        NoticeKind::Info,
                    );
                } else {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.cron_no_job", &[("id", id)]),
                        NoticeKind::Warning,
                    );
                }
            }
            ("pause", id) if !id.is_empty() => {
                if self.cron.set_enabled(id, false) {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.cron_paused", &[("id", id)]),
                        NoticeKind::Info,
                    );
                } else {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.cron_no_job", &[("id", id)]),
                        NoticeKind::Warning,
                    );
                }
            }
            ("resume", id) if !id.is_empty() => {
                if self.cron.set_enabled(id, true) {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.cron_resumed", &[("id", id)]),
                        NoticeKind::Info,
                    );
                } else {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.cron_no_job", &[("id", id)]),
                        NoticeKind::Warning,
                    );
                }
            }
            ("", _) => {
                if self.cron.jobs.is_empty() {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.cron_none", &[]),
                        NoticeKind::Info,
                    );
                    return;
                }
                let mut text = crate::i18n::t(self.lang, "msg.cron_header", &[]);
                text.push_str("\n\n");
                for job in &self.cron.jobs {
                    let state = if job.enabled {
                        crate::cron::CronStore::describe_next_fire(job)
                    } else {
                        crate::i18n::t(self.lang, "msg.cron_paused_state", &[])
                    };
                    text.push_str(&format!(
                        "- `{}` — `{}` — {} — {}\n",
                        job.id, job.schedule, state, job.prompt
                    ));
                }
                self.items
                    .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
            }
            (other, _) => self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.cron_unknown",
                    &[("cmd", &format!("{other:?}"))],
                ),
                NoticeKind::Warning,
            ),
        }
    }

    /// /trace [level] [target-prefix] — recent observability events.
    pub(crate) fn command_trace(&mut self, args: &str) {
        let mut level = None;
        let mut target = None;
        for token in args.split_whitespace() {
            if matches!(token, "trace" | "debug" | "info" | "warn" | "error") {
                level = Some(token.to_string());
            } else {
                target = Some(token.to_string());
            }
        }
        let events = crate::logs::tail_events(
            &self.agent_dir,
            &crate::logs::LogOptions {
                tail: 30,
                level,
                target,
                follow: false,
            },
        );
        if events.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.trace_none", &[]),
                NoticeKind::Info,
            );
            return;
        }
        let text = format!(
            "{}\n\n```\n{}\n```",
            crate::i18n::t(self.lang, "msg.trace_header", &[]),
            events.join("\n")
        );
        self.items
            .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
        self.line_cache.clear();
    }

    pub(crate) fn command_rewind(&mut self) {
        // Rewind lands on the same fork-confirm path: same guard (TS #9178).
        if self.running {
            self.notice(
                crate::i18n::t(self.lang, "msg.nav_running", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let path = self.state.session.build_session_path();
        let mut items: Vec<SelectItem> = Vec::new();
        let mut turn = 0usize;
        let mut assistant_chars = 0usize;
        for entry in &path {
            if let tack_session::SessionEntry::Message {
                id,
                message,
                timestamp,
                ..
            } = entry
            {
                match message {
                    tack_agent_core::AgentMessage::User(u) => {
                        turn += 1;
                        let text = match &u.content {
                            tack_ai::UserContent::Text(t) => t.clone(),
                            tack_ai::UserContent::Blocks(b) => b
                                .iter()
                                .filter_map(|b| match b {
                                    tack_ai::InputContentBlock::Text { text, .. } => {
                                        Some(text.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join(" "),
                        };
                        let first_line = text
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(70)
                            .collect::<String>();
                        let time = &timestamp[..10.min(timestamp.len())];
                        let label = format!("#{turn} [{time}] {first_line}");
                        let detail = if assistant_chars > 0 {
                            crate::i18n::t(
                                self.lang,
                                "msg.rewind_detail",
                                &[("chars", &assistant_chars.to_string())],
                            )
                        } else {
                            String::new()
                        };
                        let mut item = SelectItem::new(label, id.clone());
                        if !detail.is_empty() {
                            item = item.with_description(detail);
                        }
                        items.push(item);
                        assistant_chars = 0;
                    }
                    tack_agent_core::AgentMessage::Assistant(a) => {
                        assistant_chars += a.text().chars().count();
                    }
                    _ => {}
                }
            }
        }
        if items.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.no_checkpoints", &[]),
                NoticeKind::Info,
            );
            return;
        }
        self.dialog = Some(Dialog::Select(SelectDialog::new(
            crate::i18n::t(self.lang, "msg.rewind_title", &[]),
            items,
            SelectPurpose::Fork,
            self.theme,
        )));
    }

    /// Fork picker: earlier user messages in this session.
    pub fn open_fork_picker(&mut self) {
        // TS pi #9178: reject tree navigation while a response is streaming.
        if self.running {
            self.notice(
                crate::i18n::t(self.lang, "msg.nav_running", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let path = self.state.session.build_session_path();
        let mut items = Vec::new();
        for entry in &path {
            if let tack_session::SessionEntry::Message { id, message, .. } = entry
                && let tack_agent_core::AgentMessage::User(u) = message
            {
                let text = match &u.content {
                    tack_ai::UserContent::Text(t) => t.clone(),
                    tack_ai::UserContent::Blocks(b) => b
                        .iter()
                        .filter_map(|b| match b {
                            tack_ai::InputContentBlock::Text { text, .. } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                let label: String = text.chars().take(80).collect();
                items.push(SelectItem::new(label, id.clone()));
            }
        }
        if items.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.nothing_to_fork", &[]),
                NoticeKind::Info,
            );
            return;
        }
        self.dialog = Some(Dialog::Select(SelectDialog::new(
            crate::i18n::t(self.lang, "msg.fork_title", &[]),
            items,
            SelectPurpose::Fork,
            self.theme,
        )));
    }

    /// Tree navigator: every branch of the session, indented by depth,
    /// selecting jumps (branches) to that entry.
    pub fn command_tree(&mut self) {
        // TS pi #9178: reject tree navigation while a response is streaming.
        if self.running {
            self.notice(
                crate::i18n::t(self.lang, "msg.nav_running", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let filter = TreeFilter::from_setting(self.settings.tree_filter_mode.as_deref());
        if build_tree_items_filtered(&self.state.session, filter).is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.tree_empty", &[]),
                NoticeKind::Info,
            );
            return;
        }
        self.dialog = Some(Dialog::Tree(TreeDialog::new(
            &self.state.session,
            filter,
            self.theme,
        )));
    }

    /// LLM-summarize the branch being abandoned by a tree/fork navigation
    /// (best-effort; TS generates a branch_summary entry on tree jumps).
    ///
    /// The summarization runs on a spawned task (LLM call, seconds) so the
    /// caller can branch immediately — the summary entry attaches to the
    /// OLD leaf via from_id, which stays a valid tree node after the jump.
    /// The result lands as AppEvent::BranchSummaryDone; Esc cancels through
    /// the shared run-cancel token.
    pub async fn summarize_abandoned_branch(&mut self, target_id: &str) {
        let old_leaf = self.state.session.leaf_id().map(str::to_string);
        if old_leaf.as_deref() == Some(target_id) || self.running {
            return;
        }
        let (entries, _) = tack_session::collect_entries_for_branch_summary(
            &self.state.session,
            old_leaf.as_deref(),
            target_id,
        );
        // Only worth an LLM call when real conversation content is abandoned.
        let has_content = entries
            .iter()
            .any(|e| matches!(e, tack_session::SessionEntry::Message { .. }));
        if !has_content {
            return;
        }
        self.notice(
            crate::i18n::t(self.lang, "msg.branch_summarizing", &[]),
            NoticeKind::Info,
        );
        self.status = Some(super::super::status::StatusIndicator::working());
        let model = self.state.model.clone();
        let provider = self.provider.clone();
        let auth = self.auth.clone();
        let thinking = self.state.thinking;
        let session_id = self.state.session.session_id().to_string();
        // Fresh token: self.cancel may already be cancelled by an earlier
        // idle Esc (the shared run-cancel plumbing), and a pre-cancelled
        // token would abort the summary before it starts. Esc reaches this
        // task through self.branch_summary_cancel.
        let cancel = self
            .branch_summary_cancel
            .insert(tokio_util::sync::CancellationToken::new())
            .clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result = match auth.resolve().await {
                Ok(auth) => {
                    tack_session::generate_branch_summary(
                        &entries,
                        &model,
                        &provider,
                        &auth,
                        thinking,
                        Some(&session_id),
                        &cancel,
                    )
                    .await
                }
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(crate::tui::AppEvent::BranchSummaryDone {
                session_id,
                from_id: old_leaf,
                result,
            });
        });
    }

    /// Branch-summary result landed (AppEvent::BranchSummaryDone): attach
    /// the summary entry to the abandoned branch's tip, on the main loop
    /// (owns the session).
    pub(crate) fn handle_branch_summary_done(
        &mut self,
        session_id: String,
        from_id: Option<String>,
        result: Result<tack_session::BranchSummaryResult, String>,
    ) {
        if !self.running && !self.compacting {
            self.status = None;
        }
        // The session may have been replaced or parked (a run started)
        // while the task ran — never append blindly.
        if self.running || self.state.session.session_id() != session_id {
            if result.is_ok() {
                self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.branch_failed",
                        &[("error", "session changed while summarizing")],
                    ),
                    NoticeKind::Warning,
                );
            }
            return;
        }
        match result {
            Ok(result) => {
                if let Some(from_id) = from_id {
                    let details = serde_json::json!({
                        "readFiles": result.read_files,
                        "modifiedFiles": result.modified_files,
                    });
                    if let Err(e) = self.state.session.append_branch_summary(
                        &from_id,
                        result.summary,
                        Some(details),
                        Some(result.usage),
                    ) {
                        self.notice(
                            crate::i18n::t(
                                self.lang,
                                "msg.branch_record_failed",
                                &[("error", &e.to_string())],
                            ),
                            NoticeKind::Warning,
                        );
                    }
                }
            }
            Err(e) => {
                self.notice(
                    crate::i18n::t(self.lang, "msg.branch_failed", &[("error", &e.to_string())]),
                    NoticeKind::Warning,
                );
            }
        }
    }

    pub(crate) fn command_share(&mut self) {
        let Some(file) = self.state.session.session_file() else {
            self.notice(
                crate::i18n::t(self.lang, "msg.session_not_persisted", &[]),
                NoticeKind::Warning,
            );
            return;
        };
        let Ok(token) = std::env::var("GITHUB_TOKEN") else {
            self.notice(
                crate::i18n::t(self.lang, "msg.share_needs_token", &[]),
                NoticeKind::Warning,
            );
            return;
        };
        let Ok(content) = std::fs::read_to_string(file) else {
            self.notice(
                crate::i18n::t(self.lang, "msg.share_read_failed", &[]),
                NoticeKind::Error,
            );
            return;
        };
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "session.jsonl".to_string());
        self.notice(
            crate::i18n::t(self.lang, "msg.sharing", &[]),
            NoticeKind::Info,
        );
        let tx = self.event_tx.clone();
        let lang = self.lang;
        tokio::spawn(async move {
            let result = share_via_gist(&token, &name, &content).await;
            match result {
                Ok(url) => {
                    let _ = tx.send(crate::tui::AppEvent::Notice(
                        crate::i18n::t(lang, "msg.shared", &[("url", &url)]),
                        NoticeKind::Info,
                    ));
                    // A failed clipboard write must not be swallowed: the
                    // share itself worked, so report the copy error
                    // separately (upstream 60e7e76bd). The write can block
                    // for seconds — keep it off the runtime workers.
                    let copy_url = url.clone();
                    let copy_result = tokio::task::spawn_blocking(move || {
                        tack_tui::terminal::copy_to_clipboard(&copy_url)
                    })
                    .await;
                    if let Ok(Err(err)) = copy_result {
                        let _ = tx.send(crate::tui::AppEvent::Notice(
                            crate::i18n::t(lang, "msg.copy_failed", &[("error", &err.to_string())]),
                            NoticeKind::Error,
                        ));
                    }
                }
                Err(e) => {
                    let _ = tx.send(crate::tui::AppEvent::Notice(
                        crate::i18n::t(lang, "msg.share_failed", &[("error", &e.to_string())]),
                        NoticeKind::Error,
                    ));
                }
            }
        });
    }

    pub(crate) fn command_import(&mut self, path: &str) {
        if path.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.usage_import", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        match SessionManager::fork_from(std::path::Path::new(path), &self.cwd) {
            Ok(session) => {
                self.state.session = session;
                self.replay_transcript();
                self.notice(
                    crate::i18n::t(self.lang, "msg.session_imported", &[]),
                    NoticeKind::Info,
                );
            }
            Err(e) => self.notice(
                crate::i18n::t(self.lang, "msg.import_failed", &[("error", &e.to_string())]),
                NoticeKind::Error,
            ),
        }
    }

    pub(crate) fn command_clone(&mut self) {
        if let Some(file) = self.state.session.session_file() {
            match SessionManager::fork_from(file, &self.cwd) {
                Ok(session) => {
                    self.state.session = session;
                    self.notice(
                        crate::i18n::t(self.lang, "msg.session_cloned", &[]),
                        NoticeKind::Info,
                    );
                }
                Err(e) => self.notice(
                    crate::i18n::t(self.lang, "msg.clone_failed", &[("error", &e.to_string())]),
                    NoticeKind::Error,
                ),
            }
        }
    }

    pub(crate) fn command_name(&mut self, name: &str) {
        if name.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.usage_name", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        match self
            .state
            .session
            .append_session_info(Some(name.to_string()))
        {
            Ok(_) => self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.session_named",
                    &[("name", &format!("{name:?}"))],
                ),
                NoticeKind::Info,
            ),
            Err(e) => self.notice(
                crate::i18n::t(self.lang, "msg.failed", &[("error", &e.to_string())]),
                NoticeKind::Error,
            ),
        }
    }

    pub(crate) fn command_session(&mut self) {
        let totals = self.state.session.session_totals();
        let context = self.state.session.build_session_context();
        let tokens = tack_session::estimate_context_tokens(&context.messages);
        let file = self
            .state
            .session
            .session_file()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(in-memory)".to_string());
        let text = crate::i18n::t(
            self.lang,
            "session.body",
            &[
                ("file", &file),
                ("id", self.state.session.session_id()),
                ("tokens", &tokens.tokens.to_string()),
                (
                    "pct",
                    &if self.state.model.context_window > 0 {
                        tokens.tokens * 100 / self.state.model.context_window as u64
                    } else {
                        0
                    }
                    .to_string(),
                ),
                (
                    "window",
                    &(self.state.model.context_window / 1000).to_string(),
                ),
                ("input", &totals.input.to_string()),
                ("output", &totals.output.to_string()),
                ("cr", &totals.cache_read.to_string()),
                ("cw", &totals.cache_write.to_string()),
                ("cost", &format!("{:.4}", totals.cost.total)),
            ],
        );
        self.items.push(TranscriptItem::Chat(ChatEntry::Assistant {
            message: assistant_notice(&text),
            streaming: false,
        }));
        self.line_cache.clear();
    }

    /// /cost — token usage and cost breakdown by model, plus budget progress.
    pub(crate) fn command_cost(&mut self) {
        // Per-model aggregation over the session's assistant messages.
        let mut per_model: std::collections::BTreeMap<String, (u64, u64, u64, u64, f64)> =
            std::collections::BTreeMap::new();
        for entry in &self.state.session.build_session_path() {
            if let tack_session::SessionEntry::Message {
                message: tack_agent_core::AgentMessage::Assistant(a),
                ..
            } = entry
            {
                if a.usage.total_tokens == 0 {
                    continue;
                }
                let key = a.model.clone();
                let entry = per_model.entry(key).or_default();
                entry.0 += a.usage.input;
                entry.1 += a.usage.output;
                entry.2 += a.usage.cache_read;
                entry.3 += a.usage.cache_write;
                entry.4 += a.usage.cost.total;
            }
        }
        let totals = self.state.session.session_totals();

        let mut text = crate::i18n::t(self.lang, "cost.header", &[]);
        if per_model.is_empty() {
            text.push_str(&crate::i18n::t(self.lang, "cost.no_usage", &[]));
        } else {
            text.push_str(&crate::i18n::t(self.lang, "cost.table_header", &[]));
            for (model, (input, output, cache_r, cache_w, cost)) in &per_model {
                text.push_str(&format!(
                    "| {model} | {input} | {output} | {cache_r} | {cache_w} | ${cost:.4} |\n"
                ));
            }
            text.push_str(&crate::i18n::t(
                self.lang,
                "cost.total",
                &[
                    ("tokens", &totals.total_tokens.to_string()),
                    ("input", &totals.input.to_string()),
                    ("output", &totals.output.to_string()),
                    ("cost", &format!("{:.4}", totals.cost.total)),
                ],
            ));
        }
        if let Some(budget) = self.settings.token_budget {
            let pct = totals.total_tokens * 100 / budget.max(1);
            let extra = if pct >= 100 {
                crate::i18n::t(self.lang, "cost.budget_exceeded", &[])
            } else {
                String::new()
            };
            text.push_str(&crate::i18n::t(
                self.lang,
                "cost.budget",
                &[
                    ("used", &totals.total_tokens.to_string()),
                    ("budget", &budget.to_string()),
                    ("pct", &pct.to_string()),
                    ("extra", &extra),
                ],
            ));
        }
        self.items
            .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
        self.line_cache.clear();
    }

    pub(crate) async fn command_compact(&mut self, custom: &str) {
        if self.running || self.compacting {
            self.notice(
                crate::i18n::t(self.lang, "msg.compact_running", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let path = self.state.session.build_session_path();
        let Some(preparation) = tack_session::prepare_compaction(&path, &self.settings.compaction)
        else {
            self.notice(
                crate::i18n::t(self.lang, "msg.nothing_to_compact", &[]),
                NoticeKind::Info,
            );
            return;
        };
        // Lineage marker for the landing guard: tree navigation while
        // compacting must not let the entry land on a different branch.
        let compact_leaf = path.last().map(|e| e.id().to_string());
        self.status = Some(super::super::status::StatusIndicator::compacting());
        let tokens_before = preparation.tokens_before;
        let model = self.state.model.clone();
        let provider = self.provider.clone();
        let auth = match self.auth.resolve().await {
            Ok(auth) => auth,
            Err(e) => {
                self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.compact_auth_failed",
                        &[("error", &e.to_string())],
                    ),
                    NoticeKind::Error,
                );
                self.status = None;
                return;
            }
        };
        let thinking = self.state.thinking;
        let session_id = self.state.session.session_id().to_string();
        // PreCompact hooks (manual trigger) — inline, before the task.
        {
            let groups = self
                .hook_config
                .take_groups(crate::shell_hooks::HookEvent::PreCompact);
            if !groups.is_empty() {
                let payload = serde_json::json!({
                    "session_id": session_id,
                    "transcript_path": serde_json::Value::Null,
                    "cwd": self.cwd,
                    "hook_event_name": "PreCompact",
                    "model": self.state.model.id,
                    "trigger": "manual",
                });
                self.hook_engine.run(&groups, None, &payload).await;
            }
        }
        // PostCompact hook groups are taken now (the config lives on the
        // main loop) but fired from the task on success.
        let post_groups = self
            .hook_config
            .take_groups(crate::shell_hooks::HookEvent::PostCompact);
        let custom = custom.to_string();
        // Spawned like the agent run: the summarization LLM call takes
        // seconds to minutes and must not freeze the UI loop. The result
        // lands as AppEvent::ManualCompactDone (session mutation back on
        // the main loop); Esc cancels through the shared cancel token.
        self.cancel = tokio_util::sync::CancellationToken::new();
        let cancel = self.cancel.clone();
        let tx = self.event_tx.clone();
        let hook_engine = self.hook_engine.clone();
        let cwd = self.cwd.clone();
        let model_id = self.state.model.id.clone();
        self.compacting = true;
        tokio::spawn(async move {
            let result = tack_session::compact(
                &preparation,
                &model,
                &provider,
                &auth,
                if custom.is_empty() {
                    None
                } else {
                    Some(custom.as_str())
                },
                thinking,
                Some(&session_id),
                &cancel,
            )
            .await
            .map(|result| {
                let kept = &path[path
                    .iter()
                    .position(|e| e.id() == result.first_kept_entry_id)
                    .unwrap_or(path.len())..];
                let retained_tail: Vec<tack_agent_core::AgentMessage> = kept
                    .iter()
                    .flat_map(tack_session::session_entry_to_context_messages)
                    .collect();
                (result, retained_tail)
            });
            // PostCompact hooks (fire-and-forget, before the result lands).
            if result.is_ok() && !post_groups.is_empty() {
                let payload = serde_json::json!({
                    "session_id": session_id,
                    "transcript_path": serde_json::Value::Null,
                    "cwd": cwd,
                    "hook_event_name": "PostCompact",
                    "model": model_id,
                    "trigger": "manual",
                    "tokens_before": tokens_before,
                });
                tokio::spawn(async move {
                    hook_engine.run(&post_groups, None, &payload).await;
                });
            }
            let _ = tx.send(crate::tui::AppEvent::ManualCompactDone {
                session_id,
                leaf: compact_leaf,
                result,
            });
        });
    }

    /// Manual /compact result landed (AppEvent::ManualCompactDone): append
    /// the compaction entry, refresh the transcript, then drain any prompts
    /// queued while the task ran.
    pub(crate) async fn handle_manual_compact_done(
        &mut self,
        session_id: String,
        leaf: Option<String>,
        result: Result<
            (
                tack_session::CompactionResult,
                Vec<tack_agent_core::AgentMessage>,
            ),
            String,
        >,
    ) {
        self.compacting = false;
        self.status = None;
        // Two stale-landing guards (the task ran while the UI stayed
        // interactive): the session must be the same one, and its lineage
        // must still contain the leaf the compaction was prepared from
        // (tree navigation mid-compact would otherwise land the entry on
        // the WRONG branch with the abandoned branch's tail inside).
        // Either way the queued-prompt drain below still runs — a guard
        // rejection must not strand prompts the user already queued.
        let landable = if self.state.session.session_id() != session_id {
            self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.compact_failed",
                    &[("error", "session changed while compacting")],
                ),
                NoticeKind::Warning,
            );
            false
        } else if leaf.as_ref().is_some_and(|leaf| {
            !self
                .state
                .session
                .build_session_path()
                .iter()
                .any(|e| e.id() == *leaf)
        }) {
            self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.compact_failed",
                    &[("error", "session lineage changed while compacting")],
                ),
                NoticeKind::Warning,
            );
            false
        } else {
            true
        };
        if landable {
            match result {
                Ok((result, retained_tail)) => {
                    let tokens_before = result.tokens_before;
                    match self.state.session.append_compaction(
                        &result.summary,
                        Some(result.first_kept_entry_id.clone()),
                        result.tokens_before,
                        Some(retained_tail),
                        Some(result.details.clone()),
                        Some(result.usage.clone()),
                    ) {
                        Ok(_) => {
                            self.items
                                .push(TranscriptItem::Chat(ChatEntry::CompactionSummary {
                                    summary: result.summary.clone(),
                                    tokens_before,
                                }));
                            self.replay_transcript();
                        }
                        Err(e) => self.notice(
                            crate::i18n::t(
                                self.lang,
                                "msg.compact_persist_failed",
                                &[("error", &e.to_string())],
                            ),
                            NoticeKind::Error,
                        ),
                    }
                }
                Err(e) => self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.compact_failed",
                        &[("error", &e.to_string())],
                    ),
                    NoticeKind::Error,
                ),
            }
        }
        // Prompts queued while compacting drive the next run.
        let queued = {
            let steering = self.steering.lock().await;
            let follow_up = self.follow_up.lock().await;
            !steering.is_empty() || !follow_up.is_empty()
        };
        if queued && !self.running {
            self.start_run(Vec::new()).await;
        }
    }

    // ---- bash ----
}
