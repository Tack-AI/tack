//! Slash commands (port of `core/slash-commands.ts`). Dialog components
//! live in `tui::dialogs`, export/share helpers in `tui::export`; the
//! re-exports below keep the old `tui::commands::*` paths working.
//! Inherent `impl TuiApp` here keeps mod.rs lean.

use super::chat::TranscriptItem;
use super::permission::PermissionMode;
use super::{AppEvent, ChatEntry, NoticeKind, TuiApp, lock_recover};

pub use super::dialogs::{
    Dialog, InputDialog, ModelDialog, ModelEntry, MultiSelectDialog, ScopedModelsDialog,
    SelectDialog, SelectPurpose, SessionDialog, TreeDialog, TreeFilter, build_tree_items,
    build_tree_items_filtered,
};
pub use super::export::export_html;

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

/// All built-in slash commands (name, i18n description key) for /help +
/// autocomplete. Descriptions resolve via `crate::i18n`.
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "cmd.desc.help"),
    ("/model", "cmd.desc.model"),
    ("/scoped-models", "cmd.desc.scoped-models"),
    ("/thinking", "cmd.desc.thinking"),
    ("/mode", "cmd.desc.mode"),
    ("/fullscreen", "cmd.desc.fullscreen"),
    ("/compact", "cmd.desc.compact"),
    ("/new", "cmd.desc.new"),
    ("/resume", "cmd.desc.resume"),
    ("/tree", "cmd.desc.tree"),
    ("/fork", "cmd.desc.fork"),
    ("/rewind", "cmd.desc.rewind"),
    ("/checkpoints", "cmd.desc.checkpoints"),
    ("/memory", "cmd.desc.memory"),
    ("/search", "cmd.desc.search"),
    ("/cron", "cmd.desc.cron"),
    ("/trace", "cmd.desc.trace"),
    ("/clone", "cmd.desc.clone"),
    ("/name", "cmd.desc.name"),
    ("/session", "cmd.desc.session"),
    ("/cost", "cmd.desc.cost"),
    ("/context", "cmd.desc.context"),
    ("/todo", "cmd.desc.todo"),
    ("/rules", "cmd.desc.rules"),
    ("/changelog", "cmd.desc.changelog"),
    ("/debug", "cmd.desc.debug"),
    ("/copy", "cmd.desc.copy"),
    ("/export", "cmd.desc.export"),
    ("/login", "cmd.desc.login"),
    ("/logout", "cmd.desc.logout"),
    ("/reload", "cmd.desc.reload"),
    ("/trust", "cmd.desc.trust"),
    ("/mcp", "cmd.desc.mcp"),
    ("/ext", "cmd.desc.ext"),
    ("/settings", "cmd.desc.settings"),
    ("/theme", "cmd.desc.theme"),
    ("/share", "cmd.desc.share"),
    ("/import", "cmd.desc.import"),
    ("/models", "cmd.desc.models"),
    ("/providers", "cmd.desc.providers"),
    ("/quit", "cmd.desc.quit"),
];

/// Setting categories for the /settings menu (key, i18n description key).
const SETTINGS_ITEMS: &[(&str, &str)] = &[
    ("theme", "settings.desc.theme"),
    ("tuiMode", "settings.desc.tuiMode"),
    ("autoCompact", "settings.desc.autoCompact"),
    ("autoRetry", "settings.desc.autoRetry"),
    ("steeringMode", "settings.desc.steeringMode"),
    ("followUpMode", "settings.desc.followUpMode"),
    ("doubleEscapeAction", "settings.desc.doubleEscapeAction"),
    ("treeFilterMode", "settings.desc.treeFilterMode"),
    ("hideThinkingBlock", "settings.desc.hideThinkingBlock"),
    ("blockImages", "settings.desc.blockImages"),
    ("showImages", "settings.desc.showImages"),
    ("showCacheMissNotices", "settings.desc.showCacheMissNotices"),
    ("cacheRetention", "settings.desc.cacheRetention"),
    ("clearOnShrink", "settings.desc.clearOnShrink"),
    ("mermaid", "settings.desc.mermaid"),
    ("quietStartup", "settings.desc.quietStartup"),
];

/// Documented keybinding actions (rendered from the registry in /hotkeys).
const KEYBINDING_DOCS: &[(&str, &str)] = &[
    ("app.interrupt", "keys.desc.app.interrupt"),
    ("app.clear", "keys.desc.app.clear"),
    ("app.exit", "keys.desc.app.exit"),
    ("app.tools.expand", "keys.desc.app.tools.expand"),
    ("app.editor.external", "keys.desc.app.editor.external"),
    (
        "app.clipboard.pasteImage",
        "keys.desc.app.clipboard.pasteImage",
    ),
    ("app.model.cycleForward", "keys.desc.app.model.cycleForward"),
    (
        "app.model.cycleBackward",
        "keys.desc.app.model.cycleBackward",
    ),
    ("app.mode.cycle", "keys.desc.app.mode.cycle"),
    ("app.thinking.toggle", "keys.desc.app.thinking.toggle"),
    ("app.message.dequeue", "keys.desc.app.message.dequeue"),
    ("app.message.sendNow", "keys.desc.app.message.sendNow"),
    ("app.message.followUp", "keys.desc.app.message.followUp"),
    ("app.search.open", "keys.desc.app.search.open"),
    ("app.search.next", "keys.desc.app.search.next"),
    ("app.search.previous", "keys.desc.app.search.previous"),
    (
        "app.scroll.promptPrevious",
        "keys.desc.app.scroll.promptPrevious",
    ),
    ("app.scroll.promptNext", "keys.desc.app.scroll.promptNext"),
];

pub(crate) mod context;
#[cfg(feature = "ext")]
mod ext;
mod mcp;
mod models;
mod select;
mod session;
mod settings;

impl TuiApp {
    /// Dispatch a slash command (text after the leading `/`).
    pub async fn run_command(&mut self, input: &str) {
        let mut parts = input.trim().splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("").trim();
        match name {
            "help" | "hotkeys" => self.show_help(),
            "settings" => self.command_settings(),
            "theme" => self.open_setting_values("theme"),
            "model" => self.command_model(args).await,
            "scoped-models" | "scoped_models" => self.command_scoped_models(),
            "models" => self.command_models(args).await,
            "providers" => self.command_providers(),
            "thinking" => self.command_thinking(args).await,
            "fullscreen" | "tui" => {
                self.toggle_fullscreen();
                let mode = if self.fullscreen {
                    "fullscreen"
                } else {
                    "regular"
                };
                self.notice(
                    crate::i18n::t(self.lang, "msg.tui_mode", &[("mode", mode)]),
                    NoticeKind::Info,
                );
            }
            "mode" => {
                if args.is_empty() {
                    let next = {
                        let current = *lock_recover(&self.mode);
                        let mut next = current.cycle();
                        // Managed policy: skip bypass entirely.
                        if self.settings.disable_bypass && next == PermissionMode::Bypass {
                            next = PermissionMode::Ask;
                        }
                        next
                    };
                    *lock_recover(&self.mode) = next;
                    self.notice(
                        crate::i18n::t(self.lang, "notice.mode", &[("mode", next.as_str())]),
                        NoticeKind::Info,
                    );
                } else {
                    match args {
                        "ask" | "acceptEdits" | "plan" | "bypass" => {
                            if args == "bypass" && self.settings.disable_bypass {
                                self.notice(
                                    crate::i18n::t(self.lang, "notice.mode_bypass_disabled", &[]),
                                    NoticeKind::Warning,
                                );
                                return;
                            }
                            let mode = match args {
                                "ask" => PermissionMode::Ask,
                                "acceptEdits" => PermissionMode::AcceptEdits,
                                "plan" => PermissionMode::Plan,
                                _ => PermissionMode::Bypass,
                            };
                            *lock_recover(&self.mode) = mode;
                            self.notice(
                                crate::i18n::t(
                                    self.lang,
                                    "notice.mode",
                                    &[("mode", mode.as_str())],
                                ),
                                NoticeKind::Info,
                            );
                        }
                        other => self.notice(
                            crate::i18n::t(
                                self.lang,
                                "msg.invalid_mode",
                                &[("mode", &format!("{other:?}"))],
                            ),
                            NoticeKind::Warning,
                        ),
                    }
                }
            }
            "compact" => self.command_compact(args).await,
            "new" => self.command_new(),
            "resume" => {
                let _ = self.command_resume();
            }
            "tree" => self.command_tree(),
            "fork" => self.open_fork_picker(),
            "rewind" => self.command_rewind(),
            "checkpoints" => self.command_checkpoints(args),
            "memory" => self.command_memory(args),
            "search" => self.command_search(args),
            "cron" => self.command_cron(args),
            "trace" => self.command_trace(args),
            "clone" => self.command_clone(),
            "name" => self.command_name(args),
            "session" => self.command_session(),
            "cost" => self.command_cost(),
            "context" => self.command_context(),
            "todo" => self.command_todo(args).await,
            "rules" => self.command_rules(),
            "changelog" => {
                self.items.push(TranscriptItem::Chat(ChatEntry::Markdown {
                    text: crate::changelog::CHANGELOG.trim().to_string(),
                }));
            }
            "debug" => self.command_debug(),
            "copy" => self.command_copy(),
            "share" => {
                if self.flags.offline {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.offline_share", &[]),
                        NoticeKind::Warning,
                    );
                } else {
                    self.command_share();
                }
            }
            "import" => self.command_import(args),
            "export" => self.command_export(args),
            "login" => {
                if self.flags.offline {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.offline_login", &[]),
                        NoticeKind::Warning,
                    );
                } else {
                    self.command_login(args).await;
                }
            }
            "logout" => self.command_logout(args),
            "reload" => self.command_reload(),
            "trust" => self.open_trust_dialog(),
            "mcp" => self.command_mcp().await,
            #[cfg(feature = "ext")]
            "ext" => self.command_ext(args),
            "quit" | "exit" => self.should_quit = true,
            "" => {}
            other => {
                // Extension commands take precedence over templates (TS order:
                // builtin → extension → template). The invoke runs on a
                // spawned task, never inline on the UI loop: a plugin whose
                // handler calls back into the host (ui/notify, ui/select, …)
                // is answered by this very loop, so an inline await
                // deadlocks until the 30s request timeout.
                if let Some(invoker) = self.extensions.command_invoker(other) {
                    let tx = self.event_tx.clone();
                    let name = other.to_string();
                    let args = args.to_string();
                    crate::task::spawn_guarded("ext-command", async move {
                        let result = invoker.invoke(args).await;
                        let _ = tx.send(AppEvent::ExtCommandResult { name, result });
                    });
                    return;
                }
                // Prompt templates as commands (/<name> args).
                let templates =
                    crate::prompt_templates::load_prompt_templates(&self.cwd, &self.agent_dir);
                if let Some(template) = templates.iter().find(|t| t.name == other) {
                    let expanded = crate::prompt_templates::substitute_args(
                        &template.content,
                        &crate::prompt_templates::parse_command_args(args),
                    );
                    self.items.push(TranscriptItem::Chat(ChatEntry::User {
                        text: expanded.clone(),
                    }));
                    self.start_run(vec![expanded]).await;
                } else {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.unknown_command", &[("name", other)]),
                        NoticeKind::Warning,
                    );
                }
            }
        }
    }

    pub fn notice(&mut self, text: impl Into<String>, kind: NoticeKind) {
        self.items
            .push(TranscriptItem::Chat(ChatEntry::notice(text, kind)));
        // No line_cache.clear(): pushing keeps the index alignment intact
        // (same rationale as MessageEnd in run.rs) — earlier entries are
        // immutable, so a blanket clear would re-render the whole
        // transcript per notice; the new entry renders on demand.
    }

    /// Persist the default provider/model to global settings and report
    /// the outcome — previously save errors were silently dropped with
    /// `let _ =`, so a failed write still showed "default model saved".
    pub(crate) fn save_default_model(&mut self, provider: &str, model_id: &str) {
        let result = crate::settings::Settings::save_global(
            &self.agent_dir,
            "defaultProvider",
            serde_json::json!(provider),
        )
        .and_then(|_| {
            crate::settings::Settings::save_global(
                &self.agent_dir,
                "defaultModel",
                serde_json::json!(model_id),
            )
        });
        match result {
            Ok(()) => self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.default_model_saved",
                    &[("ref", &format!("{provider}/{model_id}"))],
                ),
                NoticeKind::Info,
            ),
            Err(e) => {
                tracing::warn!("failed to save default model: {e}");
                self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.default_model_save_failed",
                        &[("error", &e.to_string())],
                    ),
                    NoticeKind::Error,
                );
            }
        }
    }

    /// Append text to the editor buffer (used by /mcp picks and similar).
    fn insert_into_editor(&mut self, text: &str) {
        let current = self.editor.text();
        let merged = if current.trim().is_empty() {
            text.to_string()
        } else {
            format!("{}\n\n{}", current.trim_end(), text)
        };
        self.editor.set_text(&merged);
    }

    fn show_help(&mut self) {
        let mut text = crate::i18n::t(self.lang, "help.commands", &[]);
        text.push('\n');
        for (name, key) in SLASH_COMMANDS {
            text.push_str(&format!(
                "- `{name}` — {}\n",
                crate::i18n::t(self.lang, key, &[])
            ));
        }
        text.push_str(&format!(
            "\n{}\n",
            crate::i18n::t(self.lang, "help.keys", &[])
        ));
        for (action, key) in KEYBINDING_DOCS {
            let description = crate::i18n::t(self.lang, key, &[]);
            let specs = self.kb.specs(action);
            if specs.is_empty() {
                text.push_str(&format!(
                    "- {} {description}\n",
                    crate::i18n::t(self.lang, "help.unbound", &[])
                ));
            } else {
                text.push_str(&format!("- `{}` {description}\n", specs.join(", ")));
            }
        }
        text.push_str(&crate::i18n::t(self.lang, "help.editor", &[]));
        self.items.push(TranscriptItem::Chat(ChatEntry::Assistant {
            message: assistant_notice(&text),
            streaming: false,
        }));
        self.line_cache.clear();
    }

    // ---- model / thinking ----
}

/// Build an AssistantMessage that just displays a notice (markdown).
fn assistant_notice(text: &str) -> tack_ai::AssistantMessage {
    let model = tack_ai::Model {
        id: String::new(),
        name: String::new(),
        api: String::new(),
        provider: String::new(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![tack_ai::InputKind::Text],
        cost: tack_ai::ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    let mut message = tack_ai::AssistantMessage::pending(&model);
    message.stop_reason = tack_ai::StopReason::Stop;
    message.content = vec![tack_ai::ContentBlock::text(text)];
    message
}
