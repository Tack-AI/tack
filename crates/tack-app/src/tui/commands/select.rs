//! Select-dialog result dispatch.

use std::path::PathBuf;

use tack_session::SessionManager;

use super::super::theme::Theme;
use super::super::{NoticeKind, TuiApp};
use super::SelectPurpose;

impl TuiApp {
    /// Apply a finished select dialog's result.
    pub async fn apply_select(&mut self, purpose: SelectPurpose, value: &str) {
        match purpose {
            SelectPurpose::Model => {
                if let Some((provider, model_id)) = value.split_once('/') {
                    // Managed policy: provider/model may be locked.
                    if let Some(locked) = &self.settings.locked_provider
                        && provider != locked
                    {
                        self.notice(
                            crate::i18n::t(self.lang, "msg.provider_locked", &[("locked", locked)]),
                            NoticeKind::Warning,
                        );
                        return;
                    }
                    if let Some(locked) = &self.settings.locked_model
                        && model_id != locked
                    {
                        self.notice(
                            crate::i18n::t(self.lang, "msg.model_locked", &[("locked", locked)]),
                            NoticeKind::Warning,
                        );
                        return;
                    }
                    match crate::model::resolve_model(provider, Some(model_id), &self.agent_dir) {
                        Ok(model) => {
                            // The adapter + auth were resolved for the
                            // previous model at startup; rebind so a
                            // cross-api switch doesn't stream the new model
                            // through the old adapter.
                            let api_changed = model.api != self.state.model.api;
                            let provider_changed = model.provider != self.state.model.provider;
                            self.state.model = model;
                            self.rebind_provider(api_changed, provider_changed);
                            if let Err(e) =
                                self.state.session.append_model_change(provider, model_id)
                            {
                                self.notice(
                                    crate::i18n::t(
                                        self.lang,
                                        "msg.model_record_failed",
                                        &[("error", &e.to_string())],
                                    ),
                                    NoticeKind::Warning,
                                );
                            }
                            // tack-ext: model_select lifecycle event.
                            self.extensions
                                .notify(
                                    "model_select",
                                    serde_json::json!({ "provider": provider, "modelId": model_id, "source": "set" }),
                                )
                                .await;
                            self.notice(
                                crate::i18n::t(
                                    self.lang,
                                    "msg.model_set",
                                    &[("ref", &format!("{provider}/{model_id}"))],
                                ),
                                NoticeKind::Info,
                            );
                        }
                        Err(e) => self.notice(e, NoticeKind::Error),
                    }
                }
            }
            SelectPurpose::Thinking => self.apply_thinking(value).await,
            SelectPurpose::ExtUi => {} // routed to pending_ext_ui by the caller
            SelectPurpose::Resume => {
                let path = PathBuf::from(value);
                match SessionManager::open(
                    &path,
                    Some(tack_session::default_session_dir(
                        &self.cwd,
                        &self.agent_dir,
                    )),
                ) {
                    Ok(session) => {
                        self.state.session = session;
                        self.replay_transcript();
                        self.notice(
                            crate::i18n::t(self.lang, "msg.session_resumed", &[]),
                            NoticeKind::Info,
                        );
                    }
                    Err(e) => self.notice(
                        crate::i18n::t(
                            self.lang,
                            "msg.resume_failed",
                            &[("error", &e.to_string())],
                        ),
                        NoticeKind::Error,
                    ),
                }
            }
            SelectPurpose::Fork => {
                // TS pi 47acd8e6c: recheck after the dialog closes — a run
                // may have started (queued steering submitted) while the
                // picker was open.
                if self.running {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.nav_running", &[]),
                        NoticeKind::Warning,
                    );
                    return;
                }
                self.summarize_abandoned_branch(value).await;
                match self.state.session.branch(value) {
                    Ok(()) => {
                        self.replay_transcript();
                        self.notice(
                            crate::i18n::t(self.lang, "msg.forked", &[]),
                            NoticeKind::Info,
                        );
                    }
                    Err(e) => self.notice(
                        crate::i18n::t(self.lang, "msg.fork_failed", &[("error", &e.to_string())]),
                        NoticeKind::Error,
                    ),
                }
            }
            SelectPurpose::Logout => self.command_logout(value),
            SelectPurpose::ProjectTrust => self.apply_trust(value),
            SelectPurpose::McpPick => self.apply_mcp_pick(value).await,
            SelectPurpose::FirstRunTheme => {
                self.theme = Theme::resolve(Some(value), &self.agent_dir, &self.cwd);
                self.line_cache.clear();
                match crate::settings::Settings::save_global(
                    &self.agent_dir,
                    "theme",
                    serde_json::Value::String(value.to_string()),
                ) {
                    Ok(()) => self.notice(
                        crate::i18n::t(self.lang, "msg.theme_saved", &[("value", value)]),
                        NoticeKind::Info,
                    ),
                    Err(e) => {
                        tracing::warn!("failed to save theme setting: {e}");
                        self.notice(
                            crate::i18n::t(
                                self.lang,
                                "msg.theme_save_failed",
                                &[("error", &e.to_string())],
                            ),
                            NoticeKind::Error,
                        );
                    }
                }
            }
            SelectPurpose::SettingsCategory => self.open_setting_values(value),
            SelectPurpose::SettingsValue => {
                if let Some((key, val)) = value.split_once('=') {
                    self.apply_setting(key, val);
                }
            }
        }
    }
}
