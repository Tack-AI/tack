//! Model/provider/thinking commands (`/model`, `/models`, `/providers`,
//! `/scoped-models`, `/thinking`, `/login`, `/logout`).

use tack_tui::components::select_list::SelectItem;

use super::super::chat::TranscriptItem;
use super::super::export::run_browser_login;
use super::super::{ChatEntry, NoticeKind, TuiApp};
use super::{Dialog, ModelDialog, ModelEntry, ScopedModelsDialog, SelectDialog, SelectPurpose};

impl TuiApp {
    /// Model picker entries: only providers with resolvable credentials (TS
    /// getAvailableSnapshot — the picker never lists unusable models).
    fn model_entries(&self) -> Vec<ModelEntry> {
        // GitHub Copilot: when the credential carries availableModelIds (set
        // at login/refresh), the picker only shows those (TS behavior).
        let copilot_available: Option<Vec<String>> =
            crate::auth::get_oauth(&self.agent_dir, "github-copilot").and_then(|c| {
                c.extras.get("availableModelIds").and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                })
            });
        let mut entries = Vec::new();
        for def in tack_ai::providers::BUILTIN_PROVIDERS {
            if !crate::model::provider_has_auth(&self.agent_dir, def.id) {
                continue;
            }
            for model in tack_ai::providers::builtin_models(def.id).iter() {
                if def.id == "github-copilot"
                    && let Some(available) = &copilot_available
                    && !available.iter().any(|id| id == &model.id)
                {
                    continue;
                }
                entries.push(ModelEntry {
                    provider: def.id.to_string(),
                    id: model.id.clone(),
                    name: model.name.clone(),
                });
            }
        }
        // Runtime (extension-registered) providers bring their own auth and
        // are always listed; models.json providers need credentials somewhere
        // (inline apiKey or a stored credential).
        let runtime_ids: std::collections::HashSet<String> =
            tack_ai::providers::runtime_providers()
                .iter()
                .map(|p| p.id.clone())
                .collect();
        for custom in tack_ai::providers::load_custom_providers(&self.agent_dir)
            .into_iter()
            .chain(tack_ai::providers::runtime_providers())
        {
            // The built-in CodeBuddy CLI provider owns this id (see
            // resolve_model); a same-id custom entry is dead config.
            if custom.id == tack_ai::codebuddy::PROVIDER_ID {
                continue;
            }
            if !runtime_ids.contains(&custom.id)
                && custom.api_key.is_none()
                && crate::auth::get_credential(&self.agent_dir, &custom.id).is_none()
            {
                continue;
            }
            for model in custom.models {
                entries.push(ModelEntry {
                    provider: custom.id.clone(),
                    id: model.id.clone(),
                    name: model.name,
                });
            }
        }
        entries
    }

    /// All available models as (value="provider/id") selector items (used by
    /// the /scoped-models multi-toggle).
    fn all_model_items(&self) -> Vec<SelectItem> {
        self.model_entries()
            .into_iter()
            .map(|e| {
                let q = e.qualified();
                SelectItem::new(q.clone(), q).with_description(e.name)
            })
            .collect()
    }

    /// The configured scoped-models list resolved to entries (name looked up
    /// in the catalog; TS resolves live Model refs).
    fn scoped_model_entries(&self) -> Vec<ModelEntry> {
        let available = self.model_entries();
        self.settings
            .scoped_models
            .iter()
            .filter_map(|pattern| {
                let (provider, id) = pattern.split_once('/')?;
                let name = available
                    .iter()
                    .find(|e| e.provider == provider && e.id == id)
                    .map(|e| e.name.clone())
                    .or_else(|| {
                        tack_ai::providers::builtin_models(provider)
                            .iter()
                            .find(|m| m.id == id)
                            .map(|m| m.name.clone())
                    })
                    .unwrap_or_else(|| id.to_string());
                Some(ModelEntry {
                    provider: provider.to_string(),
                    id: id.to_string(),
                    name,
                })
            })
            .collect()
    }

    pub async fn command_model(&mut self, query: &str) {
        let entries = self.model_entries();
        if !query.is_empty() {
            // Direct fuzzy pick without a dialog.
            let query = query.to_lowercase();
            if let Some(entry) = entries
                .iter()
                .find(|e| e.qualified().to_lowercase() == query)
                .or_else(|| {
                    entries
                        .iter()
                        .find(|e| e.qualified().to_lowercase().contains(&query))
                })
            {
                let value = entry.qualified();
                self.apply_select(SelectPurpose::Model, &value).await;
                return;
            }
            self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.no_model_match",
                    &[("query", &format!("{query:?}"))],
                ),
                NoticeKind::Warning,
            );
            return;
        }
        if entries.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.no_providers", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let scoped = self.scoped_model_entries();
        let current = format!("{}/{}", self.state.model.provider, self.state.model.id);
        let default = self
            .settings
            .default_provider
            .as_deref()
            .zip(self.settings.default_model.as_deref())
            .map(|(p, m)| format!("{p}/{m}"));
        self.dialog = Some(Dialog::Model(ModelDialog::new(
            entries, scoped, current, default, self.theme,
        )));
    }

    /// `/models [refresh|reset]` — model catalog status and refresh. The
    /// embedded catalog snapshots models at build time; `refresh` fetches a
    /// fresh one from the published `@earendil-works/pi-ai` npm package and
    /// caches it in `<agentDir>/catalog.json` (loaded at every startup).
    /// `reset` drops the cache and returns to the embedded catalog
    /// (effective after restart).
    /// `/providers` — every provider with auth status and model count
    /// (TUI twin of the `tack providers` CLI command).
    pub(crate) fn command_providers(&mut self) {
        let custom = tack_ai::providers::load_custom_providers(&self.agent_dir);
        let mut text = crate::i18n::t(self.lang, "providers.header", &[]);
        let mut ready = 0;
        for def in tack_ai::providers::BUILTIN_PROVIDERS {
            let has_auth = crate::model::provider_has_auth(&self.agent_dir, def.id);
            ready += usize::from(has_auth);
            let models = tack_ai::providers::builtin_models(def.id);
            let mark = if has_auth { "✓" } else { " " };
            let hint = if !has_auth && !def.env_keys.is_empty() {
                crate::i18n::t(
                    self.lang,
                    "providers.login_hint",
                    &[("id", def.id), ("env", def.env_keys[0])],
                )
            } else {
                String::new()
            };
            text.push_str(&format!(
                "{mark} `{}` — {}{hint}\n",
                def.id,
                crate::i18n::t(
                    self.lang,
                    "providers.models_count",
                    &[("count", &models.len().to_string())]
                )
            ));
        }
        for cp in &custom {
            ready += usize::from(cp.api_key.is_some());
            let mark = if cp.api_key.is_some() { "✓" } else { " " };
            text.push_str(&format!(
                "{mark} `{}` — {}{}\n",
                cp.id,
                crate::i18n::t(
                    self.lang,
                    "providers.models_count",
                    &[("count", &cp.models.len().to_string())]
                ),
                crate::i18n::t(self.lang, "providers.custom_suffix", &[])
            ));
        }
        text.push_str(&crate::i18n::t(
            self.lang,
            "providers.footer",
            &[("ready", &ready.to_string())],
        ));
        self.items
            .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
    }

    pub(crate) async fn command_models(&mut self, args: &str) {
        match args {
            "refresh" => {
                if self.flags.offline || std::env::var_os("TACK_OFFLINE").is_some() {
                    self.notice(
                        crate::i18n::t(self.lang, "msg.offline_catalog", &[]),
                        NoticeKind::Warning,
                    );
                    return;
                }
                self.notice(
                    crate::i18n::t(self.lang, "msg.catalog_refreshing", &[]),
                    NoticeKind::Info,
                );
                match crate::catalog_refresh::refresh_from_npm(
                    &self.agent_dir,
                    std::time::Duration::from_secs(60),
                )
                .await
                {
                    Ok(s) => self.notice(
                        crate::i18n::t(
                            self.lang,
                            "msg.catalog_refreshed",
                            &[
                                ("version", &s.version),
                                ("providers", &s.providers.to_string()),
                                ("models", &s.models.to_string()),
                            ],
                        ),
                        NoticeKind::Info,
                    ),
                    Err(e) => self.notice(
                        crate::i18n::t(
                            self.lang,
                            "msg.catalog_refresh_failed",
                            &[("error", &format!("{e:#}"))],
                        ),
                        NoticeKind::Error,
                    ),
                }
            }
            "reset" => {
                crate::catalog_refresh::clear_override(&self.agent_dir);
                self.notice(
                    crate::i18n::t(self.lang, "msg.catalog_reset", &[]),
                    NoticeKind::Info,
                );
            }
            _ => {
                let meta = std::fs::read_to_string(crate::catalog_refresh::catalog_meta_path(
                    &self.agent_dir,
                ))
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
                let text = match &meta {
                    Some(m) => crate::i18n::t(
                        self.lang,
                        "catalog.status_cached",
                        &[
                            (
                                "version",
                                m.get("version").and_then(|v| v.as_str()).unwrap_or("?"),
                            ),
                            (
                                "providers",
                                &m.get("providers")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    .to_string(),
                            ),
                            (
                                "models",
                                &m.get("models")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    .to_string(),
                            ),
                            (
                                "path",
                                &crate::catalog_refresh::catalog_cache_path(&self.agent_dir)
                                    .display()
                                    .to_string(),
                            ),
                        ],
                    ),
                    None => crate::i18n::t(
                        self.lang,
                        "catalog.status_embedded",
                        &[(
                            "count",
                            &tack_ai::providers::catalog_override_count().to_string(),
                        )],
                    ),
                };
                self.items
                    .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
            }
        }
    }

    /// Multi-toggle Dialog for the scoped-models cycling list.
    pub(crate) fn command_scoped_models(&mut self) {
        let scoped: std::collections::HashSet<&str> = self
            .settings
            .scoped_models
            .iter()
            .map(String::as_str)
            .collect();
        let items: Vec<(SelectItem, bool)> = self
            .all_model_items()
            .into_iter()
            .map(|item| {
                let enabled = scoped.contains(item.value.as_str());
                (item, enabled)
            })
            .collect();
        self.dialog = Some(Dialog::ScopedModels(ScopedModelsDialog::new(
            items, self.theme,
        )));
    }

    /// Ctrl+P / Shift+Ctrl+P: cycle through scoped models (or the current
    /// provider's catalog when no scoped list is configured).
    pub async fn cycle_model(&mut self, direction: i32) {
        let candidates: Vec<String> = if !self.settings.scoped_models.is_empty() {
            self.settings.scoped_models.clone()
        } else {
            tack_ai::providers::builtin_models(&self.state.model.provider)
                .iter()
                .map(|m| format!("{}/{}", self.state.model.provider, m.id))
                .collect()
        };
        if candidates.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.no_models_cycle", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        let current = format!("{}/{}", self.state.model.provider, self.state.model.id);
        let index = candidates
            .iter()
            .position(|c| *c == current)
            .unwrap_or(if direction > 0 {
                candidates.len() - 1
            } else {
                0
            });
        let next = ((index as i32 + direction).rem_euclid(candidates.len() as i32)) as usize;
        let value = candidates[next].clone();
        self.apply_select(SelectPurpose::Model, &value).await;
    }

    pub(crate) async fn command_thinking(&mut self, level: &str) {
        let levels = self.available_thinking_levels();
        if !level.is_empty() {
            if levels.contains(&level) {
                self.apply_thinking(level).await;
            } else {
                self.notice(
                    crate::i18n::t(
                        self.lang,
                        "msg.invalid_thinking",
                        &[
                            ("level", &format!("{level:?}")),
                            ("levels", &levels.join(", ")),
                        ],
                    ),
                    NoticeKind::Warning,
                );
            }
            return;
        }
        let items = levels.iter().map(|l| SelectItem::new(*l, *l)).collect();
        self.dialog = Some(Dialog::Select(SelectDialog::new(
            crate::i18n::t(self.lang, "msg.dialog_thinking_title", &[]),
            items,
            SelectPurpose::Thinking,
            self.theme,
        )));
    }

    /// Levels the current model supports (mirrors ACP/rpc logic).
    pub fn available_thinking_levels(&self) -> Vec<&'static str> {
        if !self.state.model.reasoning {
            return vec!["off"];
        }
        let mut levels = vec!["off", "minimal", "low", "medium", "high"];
        for extra in ["xhigh", "max"] {
            let supported = self
                .state
                .model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(extra))
                .is_some_and(|v| v.is_some());
            if supported {
                levels.push(extra);
            }
        }
        levels
    }

    pub async fn apply_thinking(&mut self, level: &str) {
        let parsed = if level == "off" {
            None
        } else {
            crate::print_mode::parse_thinking_level(level)
                .ok()
                .flatten()
        };
        self.state.thinking = parsed;
        if let Err(e) = self.state.session.append_thinking_level_change(level) {
            self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.thinking_record_failed",
                    &[("error", &e.to_string())],
                ),
                NoticeKind::Warning,
            );
        }
        // tack-ext: thinking_level_select lifecycle event.
        self.extensions
            .notify(
                "thinking_level_select",
                serde_json::json!({ "level": level }),
            )
            .await;
        self.notice(
            crate::i18n::t(self.lang, "msg.thinking_set", &[("level", level)]),
            NoticeKind::Info,
        );
    }

    // ---- session commands ----

    pub(crate) async fn command_login(&mut self, provider: &str) {
        if provider.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "msg.usage_login", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        if tack_ai::oauth::oauth_flow(provider).is_none() {
            self.notice(
                crate::i18n::t(self.lang, "msg.no_oauth_flow", &[("provider", provider)]),
                NoticeKind::Warning,
            );
            return;
        }
        // Run the browser flow in the background; the TUI stays responsive.
        // Manual paste isn't available while the TUI owns stdin — device-code
        // providers should use that flow from the CLI.
        let agent_dir = self.agent_dir.clone();
        let provider = provider.to_string();
        self.notice(
            crate::i18n::t(self.lang, "msg.oauth_starting", &[("provider", &provider)]),
            NoticeKind::Info,
        );
        tokio::spawn(async move {
            if let Err(e) = run_browser_login(&agent_dir, &provider).await {
                tracing::warn!("OAuth login failed: {e}");
            }
        });
    }

    // ---- compact ----

    pub(crate) fn command_logout(&mut self, provider: &str) {
        if provider.is_empty() {
            let configured = crate::auth::list(&self.agent_dir);
            if configured.is_empty() {
                self.notice(
                    crate::i18n::t(self.lang, "msg.no_credentials", &[]),
                    NoticeKind::Info,
                );
                return;
            }
            let items = configured
                .iter()
                .map(|p| SelectItem::new(p.clone(), p.clone()))
                .collect();
            self.dialog = Some(Dialog::Select(SelectDialog::new(
                crate::i18n::t(self.lang, "msg.logout_title", &[]),
                items,
                SelectPurpose::Logout,
                self.theme,
            )));
            return;
        }
        match crate::auth::logout(&self.agent_dir, provider) {
            Ok(true) => self.notice(
                crate::i18n::t(self.lang, "msg.logged_out", &[("provider", provider)]),
                NoticeKind::Info,
            ),
            Ok(false) => self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.no_credential_for",
                    &[("provider", provider)],
                ),
                NoticeKind::Info,
            ),
            Err(e) => self.notice(
                crate::i18n::t(self.lang, "msg.logout_failed", &[("error", &e.to_string())]),
                NoticeKind::Error,
            ),
        }
    }
}
