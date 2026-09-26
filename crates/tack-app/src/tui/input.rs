//! Keyboard/mouse input handling, autocomplete and fullscreen input.
//! Inherent `impl TuiApp` split out of `mod.rs` — pure code move, no
//! behavior change.

use super::*;
#[cfg(feature = "ext")]
use tack_tui::components::select_list::{SelectItem, SelectList};

/// UI-level timeout for ext autocomplete queries (v2.2): the protocol's
/// 30s request timeout is far too slow for typing; on expiry the provider
/// silently contributes no suggestions.
#[cfg(feature = "ext")]
const EXT_AUTOCOMPLETE_UI_TIMEOUT: Duration = Duration::from_millis(300);

impl TuiApp {
    /// Runtime fullscreen toggle (`/fullscreen`).
    pub fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        self.tui.set_mode(if self.fullscreen {
            TuiMode::Fullscreen
        } else {
            TuiMode::Regular
        });
        // Drain queued main-screen frames before switching screen buffers.
        tack_tui::terminal::drain_frames();
        let _ = if self.fullscreen {
            tack_tui::terminal::enter_alt_screen(true)
        } else {
            tack_tui::terminal::leave_alt_screen(true)
        };
    }

    /// Top-level input dispatch: dialogs and mode-specific handlers get
    /// first claim on an event before it reaches the global keys/editor.
    pub async fn handle_input(&mut self, event: InputEvent) {
        // Key release events (Kitty protocol): no app component wants them,
        // and they must not retrigger autocomplete recomputation (which
        // would reset the selection after every arrow key).
        if let InputEvent::Key(k) = &event
            && k.is_release
        {
            return;
        }
        // Active dialog consumes input first.
        if self.dialog.is_some() {
            self.handle_dialog_input(&event).await;
            return;
        }
        if self.fullscreen && self.handle_fullscreen_input(&event).await {
            return;
        }
        // Focused ext panel (v2.1): navigation keys are captured for list
        // selection / panel scrolling; everything else flows through.
        #[cfg(feature = "ext")]
        if self.handle_ext_panel_key(&event).await {
            return;
        }
        // Autocomplete popup: completion keys are captured; everything else
        // flows to the editor and recomputes suggestions.
        if self.handle_autocomplete_key(&event) {
            return;
        }
        // Ctrl+R history reverse search: while active it captures keys
        // (query editing, match cycling, accept/cancel).
        if self.handle_history_search_key(&event) {
            return;
        }

        let InputEvent::Key(key) = &event else {
            // Paste goes to the editor.
            self.editor.handle_input(&event);
            self.refresh_autocomplete();
            return;
        };

        // Global keys (plugin shortcuts, then app actions).
        if self.handle_global_key(key).await {
            return;
        }

        // Editor gets everything else.
        self.editor.handle_input(&event);
        if let Some(text) = self.editor.submitted.take() {
            self.autocomplete = None;
            self.on_submit(text).await;
        }
        self.refresh_autocomplete();
    }

    /// Active dialog: feed the event to it and apply the outcome when it
    /// closes (tree label edits, scoped-models save, MCP elicitation field
    /// walk, tack-ext answers, select purposes, resume-picker cancel).
    async fn handle_dialog_input(&mut self, event: &InputEvent) {
        let mut label_error: Option<String> = None;
        if let Some(dialog) = &mut self.dialog {
            dialog.handle_input(event);
            // Tree label edits + filter cycles apply immediately (dialog stays open).
            if let commands::Dialog::Tree(d) = dialog {
                if let Some((id, label)) = d.pending_label.take() {
                    match self.state.session.append_label_change(&id, label) {
                        Ok(_) => d.refresh(&self.state.session),
                        Err(e) => label_error = Some(e.to_string()),
                    }
                }
                if d.pending_filter_cycle {
                    d.pending_filter_cycle = false;
                    d.cycle_filter(&self.state.session);
                }
            }
            if dialog.done() {
                // We just borrowed the dialog above so this is always Some;
                // stay defensive anyway — a keypress must never panic.
                let Some(mut dialog) = self.dialog.take() else {
                    return;
                };
                let resume_startup_cancel =
                    self.resume_startup && matches!(dialog, commands::Dialog::Sessions(_));
                self.resume_startup = false;
                if let Some(mut pending) = self.pending_elicitation.take() {
                    // MCP elicitation: the InputDialog belongs to the field walk.
                    match dialog.take_result() {
                        // Esc: cancel the whole elicitation.
                        None => pending.cancel(),
                        Some((_, text, _)) => {
                            // The field walk can drift out of sync if the
                            // server amends its schema mid-dialog: cancel
                            // gracefully instead of panicking.
                            let Some(field) = pending.current().cloned() else {
                                pending.cancel();
                                return;
                            };
                            match crate::mcp_elicitation::coerce_field_value(&field, &text) {
                                Ok(value) => {
                                    pending.push_answer(value);
                                    self.continue_elicitation(pending);
                                }
                                Err(e) => {
                                    // Invalid input: re-ask the same field.
                                    self.notice(e, NoticeKind::Warning);
                                    self.open_elicitation_dialog(&pending);
                                    self.pending_elicitation = Some(pending);
                                }
                            }
                        }
                    }
                } else if let commands::Dialog::ScopedModels(d) = &dialog {
                    let values = d.enabled_values();
                    self.settings.scoped_models = values.clone();
                    if let Err(e) = crate::settings::Settings::save_global(
                        &self.agent_dir,
                        "scopedModels",
                        serde_json::json!(values),
                    ) {
                        self.notice(
                            crate::i18n::t(
                                self.lang,
                                "msg.scoped_save_failed",
                                &[("error", &e.to_string())],
                            ),
                            NoticeKind::Error,
                        );
                    } else {
                        self.notice(
                            crate::i18n::t(self.lang, "msg.scoped_saved", &[]),
                            NoticeKind::Info,
                        );
                    }
                } else if let Some((purpose, value, save_default)) = dialog.take_result() {
                    if purpose == commands::SelectPurpose::ExtUi {
                        // tack-ext: route the dialog answer back to the plugin.
                        if let Some((method, respond)) = self.pending_ext_ui.take() {
                            let value = match method.as_str() {
                                "confirm" => serde_json::Value::Bool(value == "yes"),
                                _ => serde_json::Value::String(value),
                            };
                            let _ = respond.send(Ok(value));
                        }
                        return;
                    }
                    self.apply_select(purpose, &value).await;
                    if save_default
                        && purpose == commands::SelectPurpose::Model
                        && let Some((provider, model_id)) = value.split_once('/')
                    {
                        self.save_default_model(provider, model_id);
                    }
                } else if self.pending_ext_ui.is_some() {
                    // tack-ext dialog cancelled (Esc): null the answer.
                    if let Some((method, respond)) = self.pending_ext_ui.take() {
                        let value = match method.as_str() {
                            "confirm" => serde_json::Value::Bool(false),
                            _ => serde_json::Value::Null,
                        };
                        let _ = respond.send(Ok(value));
                    }
                } else if resume_startup_cancel {
                    // --resume picker cancelled without a selection: exit
                    // like TS ("No session selected"), leaving no file.
                    self.should_quit = true;
                }
            }
            if let Some(e) = label_error {
                self.notice(
                    crate::i18n::t(self.lang, "msg.label_failed", &[("error", &e)]),
                    NoticeKind::Error,
                );
            }
        }
    }

    /// Autocomplete popup keys: tab/enter accept, interrupt closes, up/down
    /// navigate. Returns true when the key was captured by the popup.
    fn handle_autocomplete_key(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        if self.autocomplete.is_none() {
            return false;
        }
        if key.matches("tab") || key.matches("enter") {
            let Some(auto) = self.autocomplete.take() else {
                return true;
            };
            if let Some(item) = auto.list.selected_item() {
                let text = autocomplete::apply(&self.editor.text(), &auto, &item.value);
                self.editor.set_text(&text);
            }
            return true;
        }
        if self.kb.matches("app.interrupt", key) {
            self.autocomplete = None;
            return true;
        }
        if key.matches("up") || key.matches("down") {
            if let Some(auto) = &mut self.autocomplete {
                auto.list.handle_input(event);
            }
            return true;
        }
        false
    }

    /// Ctrl+R incremental history reverse search (bash reverse-i-search).
    /// Inactive: only the historySearch binding opens it. Active: query
    /// editing + match cycling are captured; Enter accepts the preview
    /// into the editor (no submit), Esc restores the pre-search draft,
    /// any other key ends the search (keeping the preview) and falls
    /// through to normal handling. Returns true when consumed.
    fn handle_history_search_key(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            // Paste while searching extends the query (newlines flattened).
            if let InputEvent::Paste(text) = event
                && let Some(active) = &self.history_search
            {
                let mut query = active.query.clone();
                query.push_str(&text.replace(['\r', '\n'], " "));
                if let Some(search) = &mut self.history_search {
                    search.set_query(query, self.editor.history());
                }
                self.history_search_sync();
                return true;
            }
            return false;
        };
        if self.kb.matches("app.editor.historySearch", key) {
            if self.history_search.is_some() {
                // Repeated ctrl+r: cycle to an older hit (bash behavior).
                if let Some(search) = &mut self.history_search {
                    search.older();
                }
                self.history_search_sync();
            } else {
                self.autocomplete = None;
                let saved = self.editor.text();
                self.history_search = Some(history_search::HistorySearch::new(saved));
            }
            return true;
        }
        if self.history_search.is_none() {
            return false;
        }
        if key.matches("escape") {
            // Cancel: restore the pre-search draft.
            let saved = match self.history_search.take() {
                Some(search) => search.saved().to_string(),
                None => return true,
            };
            self.editor.set_text(&saved);
            return true;
        }
        if key.matches("enter") {
            // Accept the preview into the editor (no submit).
            self.history_search = None;
            return true;
        }
        if key.matches("up") || key.matches("down") {
            if let Some(search) = &mut self.history_search {
                if key.matches("up") {
                    search.older();
                } else {
                    search.newer();
                }
            }
            self.history_search_sync();
            return true;
        }
        if key.matches("backspace") {
            let Some(active) = &self.history_search else {
                return true;
            };
            let mut query = active.query.clone();
            query.pop();
            if let Some(search) = &mut self.history_search {
                search.set_query(query, self.editor.history());
            }
            self.history_search_sync();
            return true;
        }
        if let tack_tui::Key::Char(c) = key.key
            && !key.modifiers.ctrl
            && !key.modifiers.alt
        {
            let Some(active) = &self.history_search else {
                return true;
            };
            let mut query = active.query.clone();
            query.push(c);
            if let Some(search) = &mut self.history_search {
                search.set_query(query, self.editor.history());
            }
            self.history_search_sync();
            return true;
        }
        // Any other key: end the search, keep the preview, and let the key
        // fall through to normal handling (bash executes it).
        self.history_search = None;
        false
    }

    /// Push the search's current preview into the editor.
    fn history_search_sync(&mut self) {
        let Some(search) = &self.history_search else {
            return;
        };
        let preview = search.preview(self.editor.history()).to_string();
        self.editor.set_text(&preview);
    }

    /// Global keybindings: plugin-registered shortcuts first (a plugin may
    /// override), then the app actions. Returns true when consumed.
    async fn handle_global_key(&mut self, key: &tack_tui::KeyEvent) -> bool {
        // tack-ext: plugin-registered shortcuts fire first (plugin may override).
        let ext_actions = self.extensions.shortcut_actions();
        for (action, plugin_index) in ext_actions {
            if self.kb.matches(&action, key) {
                self.extensions.notify_shortcut(plugin_index, &action).await;
                return true;
            }
        }
        if self.kb.matches("app.interrupt", key) {
            self.handle_escape().await;
            return true;
        }
        if self.kb.matches("app.clear", key) {
            if !self.editor.text().is_empty() {
                self.editor.clear();
                self.last_ctrl_c = Some(Instant::now());
            } else {
                let now = Instant::now();
                if self
                    .last_ctrl_c
                    .is_some_and(|t| now.duration_since(t) < Duration::from_millis(500))
                {
                    self.should_quit = true;
                } else {
                    self.last_ctrl_c = Some(now);
                }
            }
            return true;
        }
        if self.kb.matches("app.exit", key) && self.editor.text().is_empty() && !self.running {
            self.should_quit = true;
            return true;
        }
        if self.kb.matches("app.tools.expand", key) {
            // Expand/collapse all tool outputs.
            let any_collapsed = self.tools.values().any(|t| !t.expanded);
            for tool in self.tools.values_mut() {
                tool.expanded = any_collapsed;
            }
            self.line_cache.clear();
            return true;
        }
        if self.kb.matches("app.editor.external", key) {
            self.open_external_editor().await;
            return true;
        }
        // Paste an image from the clipboard (Ctrl+V; Alt+V on Windows).
        if self.kb.matches("app.clipboard.pasteImage", key) {
            // Ctrl+V never triggers the terminal's bracketed paste, so the
            // clipboard is read natively: image first (unless disabled),
            // otherwise the text (which may be multi-line).
            // Clipboard reads can block for hundreds of ms (PowerShell cold
            // start on Windows, owner-less X11 selections) — never run them
            // on the UI thread (TS pi #9163 moved these to worker threads).
            let block_images = self.settings.block_images;
            let (image, text) = tokio::task::spawn_blocking(move || {
                let image = if block_images {
                    None
                } else {
                    images::paste_clipboard_image()
                };
                let text = if image.is_none() {
                    tack_tui::terminal::read_clipboard()
                } else {
                    None
                };
                (image, text)
            })
            .await
            .unwrap_or((None, None));
            match image {
                Some(path) => {
                    self.editor
                        .handle_input(&InputEvent::Paste(format!("@{} ", path.display())));
                }
                None => {
                    if let Some(text) = text
                        && !text.is_empty()
                    {
                        self.editor.handle_input(&InputEvent::Paste(text));
                    }
                }
            }
            return true;
        }
        if self.kb.matches("app.ext.panels.toggle", key) {
            // Host-side master hide for all ext panels (v2.1).
            #[cfg(feature = "ext")]
            if self
                .extensions
                .widgets()
                .iter()
                .any(|w| w.spec.kind != tack_ext::WidgetKind::StatusLineSegment)
            {
                self.ext_panels_hidden = !self.ext_panels_hidden;
                if self.ext_panels_hidden {
                    self.ext_panel_focus = None;
                }
            }
            return true;
        }
        if self.kb.matches("app.ext.panel.focusNext", key) {
            #[cfg(feature = "ext")]
            if !self.visible_panel_keys().is_empty() {
                self.focus_next_ext_panel();
            }
            return true;
        }
        if self.kb.matches("app.model.cycleForward", key) {
            self.cycle_model(1).await;
            return true;
        }
        if self.kb.matches("app.model.cycleBackward", key) {
            self.cycle_model(-1).await;
            return true;
        }
        if self.kb.matches("app.mode.cycle", key) {
            let next = lock_recover(&self.mode).cycle();
            *lock_recover(&self.mode) = next;
            self.items
                .push(chat::TranscriptItem::Chat(ChatEntry::notice(
                    crate::i18n::t(self.lang, "notice.mode", &[("mode", next.as_str())]),
                    NoticeKind::Info,
                )));
            return true;
        }
        if self.kb.matches("app.model.select", key) {
            self.command_model("").await;
            return true;
        }
        if self.kb.matches("app.models.save", key) {
            // Save the current model as the default (TS models.save).
            let (provider, model_id) = (
                self.state.model.provider.clone(),
                self.state.model.id.clone(),
            );
            self.save_default_model(&provider, &model_id);
            return true;
        }
        if self.kb.matches("app.thinking.cycle", key) {
            let levels = self.available_thinking_levels();
            if !levels.is_empty() {
                let current = self.state.thinking.map(|t| t.as_str()).unwrap_or("off");
                let next_index = levels
                    .iter()
                    .position(|l| *l == current)
                    .map(|i| i + 1)
                    .unwrap_or(0)
                    % levels.len();
                let next = levels[next_index];
                self.apply_thinking(next).await;
            }
            return true;
        }
        if self.kb.matches("app.thinking.toggle", key) {
            // Cycle thinking display: collapsed label → expanded full text
            // → hidden entirely → collapsed (TS app.thinking.toggle only
            // had hidden ↔ shown).
            let (label, expand, hide) =
                if !self.settings.hide_thinking_block && !self.thinking_expanded {
                    ("msg.thinking_expanded", true, false)
                } else if self.thinking_expanded {
                    ("msg.thinking_hidden", false, true)
                } else {
                    ("msg.thinking_collapsed", false, false)
                };
            self.thinking_expanded = expand;
            self.settings.hide_thinking_block = hide;
            self.line_cache.clear();
            self.notice(crate::i18n::t(self.lang, label, &[]), NoticeKind::Info);
            return true;
        }
        if self.kb.matches("app.message.copy", key) {
            self.command_copy();
            return true;
        }
        if self.kb.matches("app.message.dequeue", key) {
            // Recall the most recent queued message back into the editor
            // before it reaches the model. Queued transcript entries are in
            // enqueue order, so the last one identifies which queue/message
            // to withdraw; repeated presses drain the queue one by one.
            let pos = self.items.iter().rposition(|item| {
                matches!(item, chat::TranscriptItem::Chat(ChatEntry::Queued { .. }))
            });
            let Some(pos) = pos else {
                self.notice(
                    crate::i18n::t(self.lang, "msg.no_queued", &[]),
                    NoticeKind::Info,
                );
                return true;
            };
            let (text, follow_up) = match self.items.remove(pos) {
                chat::TranscriptItem::Chat(ChatEntry::Queued { text, follow_up }) => {
                    (text, follow_up)
                }
                _ => unreachable!(),
            };
            // A "send now" echo lives in no queue — just cancel the pending
            // resubmission. Otherwise withdraw the message from its queue.
            if self.pending_send_now.as_deref() == Some(text.as_str()) {
                self.pending_send_now = None;
            } else {
                let queue = if follow_up {
                    &self.follow_up
                } else {
                    &self.steering
                };
                let mut q = queue.lock().await;
                match q.iter().rposition(|m| *m == text) {
                    Some(i) => {
                        q.remove(i);
                    }
                    // Out of sync (shouldn't happen): withdraw the newest.
                    None => {
                        q.pop_back();
                    }
                }
            }
            self.line_cache.clear();
            let existing = self.editor.text();
            self.editor.set_text(&if existing.is_empty() {
                text
            } else {
                format!("{existing}\n{text}")
            });
            return true;
        }
        if self.kb.matches("app.session.new", key) {
            self.run_command("new").await;
            return true;
        }
        if self.kb.matches("app.session.tree", key) {
            self.command_tree();
            return true;
        }
        if self.kb.matches("app.session.fork", key) {
            self.open_fork_picker();
            return true;
        }
        if self.kb.matches("app.session.resume", key) {
            let _ = self.command_resume();
            return true;
        }
        if self.kb.matches("app.session.rename", key) {
            self.editor.set_text("/name ");
            return true;
        }
        if self.kb.matches("app.session.delete", key) {
            let _ = self.command_resume(); // delete lives inside the session manager dialog
            return true;
        }
        #[cfg(unix)]
        if self.kb.matches("app.suspend", key) {
            // Ctrl+Z: suspend to background (TS app.suspend; unix only).
            // Drain queued frames before the shell takes the terminal back.
            tack_tui::terminal::drain_frames();
            self.tui.stop(&mut std::io::stdout()).ok();
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = std::process::Command::new("kill")
                .arg("-TSTP")
                .arg(std::process::id().to_string())
                .status();
            let _ = crossterm::terminal::enable_raw_mode();
            self.line_cache.clear();
            self.render(&mut tack_tui::terminal::FrameWriter).ok();
            return true;
        }
        // Ctrl+Enter / Ctrl+S: send NOW — abort the current run and submit
        // the text as soon as it finishes (idle: same as Enter).
        if self.kb.matches("app.message.sendNow", key) {
            let mut text = self.editor.text();
            if !text.trim().is_empty() {
                self.editor.clear();
                if self.running {
                    // A previous "send now" is still waiting for the abort
                    // to land: merge it instead of orphaning its echo.
                    if let Some(prev) = self.pending_send_now.take() {
                        self.items.retain(|item| {
                            !matches!(
                                item,
                                chat::TranscriptItem::Chat(ChatEntry::Queued { text: t, .. })
                                    if *t == prev
                            )
                        });
                        text = format!("{prev}\n{text}");
                    }
                    self.items
                        .push(chat::TranscriptItem::Chat(ChatEntry::Queued {
                            text: text.clone(),
                            follow_up: false,
                        }));
                    self.pending_send_now = Some(text);
                    self.cancel.cancel();
                } else {
                    self.on_submit(text).await;
                }
            }
            return true;
        }
        // Follow-up: queue to run after the agent stops (unbound by default).
        if self.kb.matches("app.message.followUp", key) {
            let text = self.editor.text();
            if !text.trim().is_empty() {
                self.editor.clear();
                self.items
                    .push(chat::TranscriptItem::Chat(ChatEntry::Queued {
                        text: text.clone(),
                        follow_up: true,
                    }));
                self.follow_up.lock().await.push_back(text);
                if !self.running {
                    self.start_run(Vec::new()).await;
                }
            }
            return true;
        }
        false
    }

    /// Recompute the autocomplete popup from the editor text.
    pub(crate) fn refresh_autocomplete(&mut self) {
        // Gate on what compute() can actually complete before paying for it:
        // templates/skills load from disk and '@' walks the file tree, so
        // doing this per keystroke makes typing (and char-wise pastes) lag.
        let text = self.editor.text();
        let can_trigger = if let Some(rest) = text.strip_prefix('/') {
            !rest.contains(char::is_whitespace)
        } else {
            // Same tokenization as compute(): CJK punctuation separates the
            // prose from the @path token (upstream bfa686240).
            text.rsplit(autocomplete::is_token_separator)
                .next()
                .and_then(|t| t.strip_prefix('@'))
                .is_some_and(|q| !q.is_empty())
        };
        // tack-ext autocomplete providers (v2.2): a trigger-prefixed last
        // token queries plugins asynchronously. Built-in '/' and '@'
        // completion takes precedence over extension triggers.
        if !can_trigger {
            #[cfg(feature = "ext")]
            {
                let triggers: Vec<&str> = self
                    .ext_ac_providers
                    .iter()
                    .map(|p| p.spec.trigger.as_str())
                    .collect();
                if autocomplete::ext_trigger_token(&text, &triggers).is_some() {
                    self.request_ext_autocomplete(&text);
                    return;
                }
            }
            // Invalidate any in-flight ext query and close the popup.
            self.ext_ac_generation += 1;
            self.autocomplete = None;
            return;
        }
        // A built-in completion owns the popup; drop in-flight ext queries.
        self.ext_ac_generation += 1;
        // Templates/skills load from disk (several directories, frontmatter
        // parsing per file) — cached briefly so typing a slash command
        // doesn't pay that per keystroke (worst on slow flash, e.g. Termux).
        const AC_SOURCES_TTL: Duration = Duration::from_secs(2);
        let (templates, skills) = match &self.ac_sources {
            Some((templates, skills, at)) if at.elapsed() < AC_SOURCES_TTL => {
                (templates.clone(), skills.clone())
            }
            _ => {
                let mut templates: Vec<String> = if self.flags.no_prompt_templates {
                    Vec::new()
                } else {
                    crate::prompt_templates::load_prompt_templates(&self.cwd, &self.agent_dir)
                        .iter()
                        .map(|t| t.name.clone())
                        .collect()
                };
                // tack-ext: extension slash commands complete like built-ins.
                templates.extend(self.extensions.command_names());
                let skills = if self.settings.enable_skill_commands && !self.flags.no_skills {
                    self.load_session_skills().0
                } else {
                    Vec::new()
                };
                self.ac_sources = Some((templates.clone(), skills.clone(), Instant::now()));
                (templates, skills)
            }
        };
        self.autocomplete =
            autocomplete::compute(&self.editor.text(), &templates, &skills, &self.cwd);
        // autocompleteMaxVisible (TS: 3-20).
        if let (Some(max), Some(auto)) = (
            self.settings.autocomplete_max_visible,
            &mut self.autocomplete,
        ) {
            auto.list.max_visible = max;
        }
    }

    /// v2.2: fire `autocomplete.provide` at every provider whose trigger
    /// prefixes the current token. Merged results land as
    /// AppEvent::ExtAutocompleteReady; the UI-level timeout (300ms) and any
    /// plugin error silently degrade to no suggestions (contract).
    #[cfg(feature = "ext")]
    fn request_ext_autocomplete(&mut self, text: &str) {
        let token = text
            .split_whitespace()
            .last()
            .unwrap_or_default()
            .to_string();
        let providers: Vec<crate::extension_host::ExtAutocompleteProvider> = self
            .ext_ac_providers
            .iter()
            .filter(|p| !p.spec.trigger.is_empty() && token.starts_with(p.spec.trigger.as_str()))
            .cloned()
            .collect();
        if providers.is_empty() {
            return;
        }
        self.ext_ac_generation += 1;
        let generation = self.ext_ac_generation;
        let cursor_offset = text.len();
        let tx = self.event_tx.clone();
        crate::task::spawn_guarded("ext-autocomplete", async move {
            let mut items: Vec<SelectItem> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            // Register order; duplicates (same value) collapse to the first.
            for provider in providers {
                let Some(query) = token.strip_prefix(provider.spec.trigger.as_str()) else {
                    continue;
                };
                // UI timeout or plugin error: silently no suggestions.
                let suggestions = tokio::time::timeout(
                    EXT_AUTOCOMPLETE_UI_TIMEOUT,
                    provider.provide(query, cursor_offset),
                )
                .await
                .unwrap_or_default();
                for suggestion in suggestions {
                    if !seen.insert(suggestion.value.clone()) {
                        continue;
                    }
                    let insert = suggestion
                        .insert_text
                        .unwrap_or_else(|| suggestion.value.clone());
                    let mut item = SelectItem::new(suggestion.label, insert);
                    if let Some(detail) = suggestion.detail {
                        item = item.with_description(detail);
                    }
                    items.push(item);
                }
            }
            let auto = autocomplete::Autocomplete {
                list: SelectList::new(items),
                token,
                kind: autocomplete::Kind::ExtProvider,
            };
            let _ = tx.send(AppEvent::ExtAutocompleteReady { generation, auto });
        });
    }

    /// v2.2: merged ext suggestions arrived — apply them only if they
    /// answer the LATEST query and the editor still sits on the same token
    /// (the user may have kept typing while plugins responded).
    pub(crate) fn apply_ext_autocomplete(
        &mut self,
        generation: u64,
        auto: autocomplete::Autocomplete,
    ) {
        if generation != self.ext_ac_generation || auto.list.items.is_empty() {
            return;
        }
        let text = self.editor.text();
        if text.split_whitespace().last() != Some(auto.token.as_str()) {
            return;
        }
        let mut auto = auto;
        if let Some(max) = self.settings.autocomplete_max_visible {
            auto.list.max_visible = max;
        }
        self.autocomplete = Some(auto);
    }

    pub(crate) async fn handle_escape(&mut self) {
        if self.running {
            self.cancel.cancel();
            // Interrupt hooks (fire-and-forget).
            let groups = self
                .hook_config
                .take_groups(crate::shell_hooks::HookEvent::Interrupt);
            if !groups.is_empty() {
                let payload = serde_json::json!({
                    "session_id": self.state.session.session_id(),
                    "transcript_path": serde_json::Value::Null,
                    "cwd": self.cwd,
                    "hook_event_name": "Interrupt",
                    "model": self.state.model.id,
                });
                let engine = self.hook_engine.clone();
                crate::task::spawn_guarded("interrupt-hook", async move {
                    engine.run(&groups, None, &payload).await;
                });
            }
            return;
        }
        // Idle: spawned background ops (manual /compact, branch summary,
        // `!cmd`) share the run-cancel plumbing — Esc interrupts them just
        // like mid-run. Harmless when nothing is in flight: the next run
        // or op replaces the token before anyone can observe it.
        self.cancel.cancel();
        for (_, token) in self.bang_cancel.drain() {
            token.cancel();
        }
        if let Some(token) = self.branch_summary_cancel.take() {
            token.cancel();
        }
        // Double-Esc on empty editor: configurable via doubleEscapeAction
        // (TS: tree default / fork / none).
        if self.editor.text().is_empty() {
            let action = self
                .settings
                .double_escape_action
                .as_deref()
                .unwrap_or("tree");
            if action == "none" {
                return;
            }
            let now = Instant::now();
            if self
                .last_esc
                .is_some_and(|t| now.duration_since(t) < Duration::from_millis(500))
            {
                if action == "fork" {
                    self.open_fork_picker();
                } else {
                    self.command_tree();
                }
                self.last_esc = None;
                return;
            }
            self.last_esc = Some(now);
        }
    }

    /// Fullscreen-mode input: search bar, scroll keys, mouse
    /// scroll/selection, prompt jumping. Returns true when consumed.
    pub(crate) async fn handle_fullscreen_input(&mut self, event: &InputEvent) -> bool {
        use tack_tui::{Key, MouseEventKind};
        // Search bar captures input while open.
        if let Some(search) = &mut self.search {
            if let InputEvent::Key(key) = event {
                if self.kb.matches("app.interrupt", key) {
                    self.search = None;
                    return true;
                }
                if self.kb.matches("app.search.previous", key) {
                    search.previous();
                    self.scroll_to_match();
                    return true;
                }
                if self.kb.matches("app.search.next", key) {
                    search.next();
                    self.scroll_to_match();
                    return true;
                }
                if key.matches("backspace") {
                    search.query.pop();
                    search.dirty = true;
                    return true;
                }
                if let Key::Char(c) = key.key
                    && !key.modifiers.ctrl
                    && !key.modifiers.alt
                {
                    search.query.push(c);
                    search.dirty = true;
                    return true;
                }
            }
            return true; // swallow everything while searching
        }
        match event {
            InputEvent::Key(key) if self.kb.matches("app.search.open", key) => {
                self.search = Some(fullscreen::SearchState {
                    dirty: true,
                    ..Default::default()
                });
                true
            }
            InputEvent::Key(key) if self.kb.matches("app.scroll.promptPrevious", key) => {
                self.jump_prompt(-1);
                true
            }
            InputEvent::Key(key) if self.kb.matches("app.scroll.promptNext", key) => {
                self.jump_prompt(1);
                true
            }
            InputEvent::Key(key)
                if key.matches("pageup")
                    || key.matches("pagedown")
                    || key.matches("home")
                    || key.matches("end") =>
            {
                self.scroll.handle_input(event);
                true
            }
            InputEvent::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.scroll.scroll_by(-3);
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll.scroll_by(3);
                    }
                    MouseEventKind::Down(tack_tui::MouseButton::Left) => {
                        // Tap on the jump-to-bottom pill: resume following.
                        if let Some((row, c0, c1)) = self.jump_pill
                            && mouse.row == row
                            && mouse.column >= c0
                            && mouse.column < c1
                        {
                            self.scroll.scroll_to_end();
                            return true;
                        }
                        self.selecting = Some((mouse.row, mouse.column));
                        self.selection = None;
                    }
                    MouseEventKind::Drag(tack_tui::MouseButton::Left) => {
                        if let Some(anchor) = self.selecting {
                            self.selection = Some(fullscreen::Selection {
                                start: anchor,
                                end: (mouse.row, mouse.column),
                            });
                        }
                    }
                    MouseEventKind::Up(tack_tui::MouseButton::Left) => {
                        // TS fullscreenCopyOnSelect (default true): drag-select
                        // auto-copies. When disabled the selection stays
                        // highlighted and Ctrl+X copies it.
                        if self.settings.fullscreen_copy_on_select
                            && let Some(selection) = self.selection.take()
                        {
                            let text = selection.extract(&self.last_frame);
                            if !text.trim().is_empty() {
                                // Clipboard writes can block for seconds
                                // (native backends with timeouts) — never on
                                // the event loop (upstream #9163 precedent).
                                // Success stays silent (a transcript notice
                                // per drag-select would spam); failures are
                                // surfaced like upstream 60e7e76bd.
                                let tx = self.event_tx.clone();
                                let lang = self.lang;
                                tokio::task::spawn_blocking(move || {
                                    if let Err(err) = tack_tui::terminal::copy_to_clipboard(&text) {
                                        let _ = tx.send(crate::tui::AppEvent::Notice(
                                            crate::i18n::t(
                                                lang,
                                                "msg.copy_failed",
                                                &[("error", &err.to_string())],
                                            ),
                                            NoticeKind::Error,
                                        ));
                                    }
                                });
                            }
                        }
                        self.selecting = None;
                    }
                    MouseEventKind::Up(tack_tui::MouseButton::Right) => {
                        // Right-click paste (Windows terminals). Clipboard
                        // reads can block for hundreds of ms (owner-less X11
                        // selections, PowerShell cold start) — offload like
                        // the Ctrl+V path above (TS pi #9163).
                        let text = tokio::task::spawn_blocking(tack_tui::terminal::read_clipboard)
                            .await
                            .unwrap_or(None);
                        self.editor
                            .handle_input(&InputEvent::Paste(text.unwrap_or_default()));
                    }
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }

    pub(crate) fn scroll_to_match(&mut self) {
        let Some(search) = &self.search else { return };
        if let Some(row) = search.current_row() {
            self.scroll.scroll_top = row;
            self.scroll.follow_end = false;
        }
    }

    /// Ctrl+Shift+Up/Down: jump to the previous/next user prompt row.
    pub(crate) fn jump_prompt(&mut self, direction: i32) {
        if self.prompt_rows.is_empty() {
            return;
        }
        let current = self.scroll.scroll_top;
        let target = if direction < 0 {
            self.prompt_rows
                .iter()
                .rev()
                .find(|&&r| r < current)
                .copied()
        } else {
            self.prompt_rows.iter().find(|&&r| r > current).copied()
        };
        if let Some(row) = target {
            self.scroll.scroll_top = row;
            self.scroll.follow_end = false;
        }
    }

    // -----------------------------------------------------------------
    // Agent events
    // -----------------------------------------------------------------
}
