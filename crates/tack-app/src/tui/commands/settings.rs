//! Settings and trust commands (`/settings`, first-run/trust dialogs).

use tack_tui::components::select_list::SelectItem;

use super::super::theme::Theme;
use super::super::{NoticeKind, TuiApp};
use super::{Dialog, SETTINGS_ITEMS, SelectDialog, SelectPurpose};

impl TuiApp {
    pub(crate) fn command_settings(&mut self) {
        let items = SETTINGS_ITEMS
            .iter()
            .map(|(key, desc_key)| {
                let s = &self.settings;
                let on = |b: bool| if b { "on" } else { "off" }.to_string();
                let current = match *key {
                    "theme" => s.theme.clone().unwrap_or_else(|| "dark".into()),
                    "tuiMode" => if self.fullscreen {
                        "fullscreen"
                    } else {
                        "regular"
                    }
                    .to_string(),
                    "autoCompact" => on(s.compaction.enabled),
                    "autoRetry" => on(s.retry.enabled),
                    "steeringMode" => s.steering_mode.clone().unwrap_or_else(|| "all".into()),
                    "followUpMode" => s.follow_up_mode.clone().unwrap_or_else(|| "all".into()),
                    "doubleEscapeAction" => s
                        .double_escape_action
                        .clone()
                        .unwrap_or_else(|| "tree".into()),
                    "treeFilterMode" => s
                        .tree_filter_mode
                        .clone()
                        .unwrap_or_else(|| "default".into()),
                    "hideThinkingBlock" => on(s.hide_thinking_block),
                    "blockImages" => on(s.block_images),
                    "showImages" => on(s.show_images),
                    "showCacheMissNotices" => on(s.show_cache_miss_notices),
                    "cacheRetention" => s.cache_retention.clone().unwrap_or_else(|| "short".into()),
                    "clearOnShrink" => on(s.clear_on_shrink),
                    "mermaid" => s.mermaid.clone().unwrap_or_else(|| "image".into()),
                    "quietStartup" => on(s.quiet_startup),
                    _ => String::new(),
                };
                SelectItem::new(*key, *key).with_description(format!(
                    "{} — {current}",
                    crate::i18n::t(self.lang, desc_key, &[])
                ))
            })
            .collect();
        self.dialog = Some(Dialog::Select(SelectDialog::new(
            crate::i18n::t(self.lang, "settings.title", &[]),
            items,
            SelectPurpose::SettingsCategory,
            self.theme,
        )));
    }

    pub(crate) fn open_setting_values(&mut self, key: &str) {
        let mut values: Vec<(String, &str)> = Vec::new();
        let statics: Vec<(&'static str, &str)> = match key {
            // Theme values are dynamic (built-ins + themes dirs) — see below.
            "theme" => vec![],
            "tuiMode" => vec![
                ("regular", "settings.val.regular"),
                ("fullscreen", "settings.val.fullscreen"),
            ],
            "autoCompact" => vec![
                ("on", "settings.val.autoCompact_on"),
                ("off", "settings.val.autoCompact_off"),
            ],
            "autoRetry" => vec![
                ("on", "settings.val.autoRetry_on"),
                ("off", "settings.val.autoRetry_off"),
            ],
            "steeringMode" | "followUpMode" => {
                vec![
                    ("all", "settings.val.deliver_all"),
                    ("one-at-a-time", "settings.val.deliver_one"),
                ]
            }
            "doubleEscapeAction" => {
                vec![
                    ("tree", "settings.val.esc_tree"),
                    ("fork", "settings.val.esc_fork"),
                    ("none", "settings.val.esc_none"),
                ]
            }
            "treeFilterMode" => vec![
                ("default", "settings.val.tf_default"),
                ("no-tools", "settings.val.tf_no_tools"),
                ("user-only", "settings.val.tf_user_only"),
                ("labeled-only", "settings.val.tf_labeled_only"),
                ("all", "settings.val.tf_all"),
            ],
            "mermaid" => vec![
                ("image", "settings.val.mermaid_image"),
                ("off", "settings.val.mermaid_off"),
            ],
            "cacheRetention" => vec![
                ("short", "settings.val.cache_short"),
                ("long", "settings.val.cache_long"),
                ("off", "settings.val.cache_off"),
            ],
            _ if matches!(
                key,
                "hideThinkingBlock"
                    | "blockImages"
                    | "showImages"
                    | "showCacheMissNotices"
                    | "clearOnShrink"
                    | "quietStartup"
            ) =>
            {
                vec![("on", "settings.val.on"), ("off", "settings.val.off")]
            }
            _ => vec![],
        };
        values.extend(statics.into_iter().map(|(v, d)| (v.to_string(), d)));
        if key == "theme" {
            // Built-ins + user themes from the themes dirs (owned Strings —
            // SelectItem holds owned data, so no per-open Box::leak of
            // theme names).
            for name in Theme::available(&self.agent_dir, &self.cwd) {
                let desc = match name.as_str() {
                    "dark" => "settings.val.dark",
                    "light" => "settings.val.light",
                    _ if Theme::is_builtin(&name) => "settings.val.builtin_theme",
                    _ => "settings.val.user_theme",
                };
                values.push((name, desc));
            }
        }
        let items = values
            .iter()
            .map(|(value, desc_key)| {
                SelectItem::new(value.clone(), format!("{key}={value}"))
                    .with_description(crate::i18n::t(self.lang, desc_key, &[]))
            })
            .collect();
        self.dialog = Some(Dialog::Select(SelectDialog::new(
            key.to_string(),
            items,
            SelectPurpose::SettingsValue,
            self.theme,
        )));
    }

    pub(crate) fn apply_setting(&mut self, key: &str, value: &str) {
        let json_value = match key {
            "autoCompact"
            | "autoRetry"
            | "hideThinkingBlock"
            | "blockImages"
            | "showImages"
            | "showCacheMissNotices"
            | "clearOnShrink"
            | "quietStartup" => serde_json::Value::Bool(value == "on"),
            _ => serde_json::Value::String(value.to_string()),
        };
        if let Err(e) = crate::settings::Settings::save_global(&self.agent_dir, key, json_value) {
            self.notice(
                crate::i18n::t(
                    self.lang,
                    "msg.setting_save_failed",
                    &[("error", &e.to_string())],
                ),
                NoticeKind::Error,
            );
            return;
        }
        match key {
            "theme" => {
                self.theme = Theme::resolve(Some(value), &self.agent_dir, &self.cwd);
                self.line_cache.clear();
            }
            "tuiMode" => {
                let want = value == "fullscreen";
                if want != self.fullscreen {
                    self.toggle_fullscreen();
                }
            }
            "autoCompact" => self.settings.compaction.enabled = value == "on",
            "autoRetry" => self.settings.retry.enabled = value == "on",
            "steeringMode" => self.settings.steering_mode = Some(value.to_string()),
            "followUpMode" => self.settings.follow_up_mode = Some(value.to_string()),
            "doubleEscapeAction" => self.settings.double_escape_action = Some(value.to_string()),
            "treeFilterMode" => self.settings.tree_filter_mode = Some(value.to_string()),
            "hideThinkingBlock" => self.settings.hide_thinking_block = value == "on",
            "blockImages" => self.settings.block_images = value == "on",
            "showImages" => self.settings.show_images = value == "on",
            "showCacheMissNotices" => self.settings.show_cache_miss_notices = value == "on",
            "clearOnShrink" => self.settings.clear_on_shrink = value == "on",
            "mermaid" => self.settings.mermaid = Some(value.to_string()),
            "cacheRetention" => self.settings.cache_retention = Some(value.to_string()),
            "quietStartup" => self.settings.quiet_startup = value == "on",
            _ => {}
        }
        // Media-affecting settings take effect immediately.
        if matches!(
            key,
            "showImages" | "mermaid" | "hideThinkingBlock" | "clearOnShrink"
        ) {
            if key == "showImages" {
                self.mermaid_enabled =
                    self.settings.mermaid.as_deref() != Some("off") && self.settings.show_images;
                self.image_protocol = if !self.settings.show_images {
                    None
                } else {
                    let caps = tack_tui::terminal::Capabilities::detect();
                    if caps.kitty_images {
                        Some(tack_tui::image::ImageProtocol::Kitty)
                    } else if caps.iterm2_images {
                        Some(tack_tui::image::ImageProtocol::ITerm2)
                    } else {
                        None
                    }
                };
            }
            if key == "mermaid" {
                self.mermaid_enabled =
                    self.settings.mermaid.as_deref() != Some("off") && self.settings.show_images;
            }
            if key == "clearOnShrink" {
                self.tui.set_clear_on_shrink(self.settings.clear_on_shrink);
            }
            self.line_cache.clear();
        }
        self.settings.theme = if key == "theme" {
            Some(value.to_string())
        } else {
            self.settings.theme.clone()
        };
        if key == "tuiMode" {
            self.settings.tui_mode = Some(value.to_string());
        }
        self.notice(
            crate::i18n::t(
                self.lang,
                "msg.setting_saved",
                &[("key", key), ("value", value)],
            ),
            NoticeKind::Info,
        );
    }

    // ---- share / import ----

    /// First-run wizard (TS FirstTimeSetupComponent): theme choice. The TS
    /// analytics opt-in step is N/A — tack has no telemetry. The detected
    /// terminal background is preselected, like TS detectedTheme.
    pub fn open_first_run_dialog(&mut self) {
        let detected_dark = crate::tui::theme::terminal_is_dark();
        let detected = if detected_dark { "dark" } else { "light" };
        let items = vec![
            SelectItem::new(crate::i18n::t(self.lang, "firstrun.dark", &[]), "dark")
                .with_description(crate::i18n::t(self.lang, "firstrun.dark_desc", &[])),
            SelectItem::new(crate::i18n::t(self.lang, "firstrun.light", &[]), "light")
                .with_description(crate::i18n::t(self.lang, "firstrun.light_desc", &[])),
            SelectItem::new(
                crate::i18n::t(self.lang, "firstrun.auto", &[]),
                "light/dark",
            )
            .with_description(crate::i18n::t(self.lang, "firstrun.auto_desc", &[])),
        ];
        let mut dialog = SelectDialog::new(
            crate::i18n::t(self.lang, "firstrun.title", &[("detected", detected)]),
            items,
            SelectPurpose::FirstRunTheme,
            self.theme,
        );
        dialog.list.selected = usize::from(!detected_dark); // 0 = Dark, 1 = Light
        self.dialog = Some(Dialog::Select(dialog));
    }

    /// `/trust` (and the startup prompt): project trust decision dialog.
    pub fn open_trust_dialog(&mut self) {
        let mut items = vec![
            SelectItem::new(crate::i18n::t(self.lang, "trust.trust", &[]), "trust")
                .with_description(crate::i18n::t(self.lang, "trust.trust_desc", &[])),
        ];
        if let Some(parent) = self.cwd.parent() {
            items.push(
                SelectItem::new(
                    crate::i18n::t(
                        self.lang,
                        "trust.parent",
                        &[("path", &parent.display().to_string())],
                    ),
                    "parent",
                )
                .with_description(crate::i18n::t(
                    self.lang,
                    "trust.parent_desc",
                    &[],
                )),
            );
        }
        items.push(SelectItem::new(
            crate::i18n::t(self.lang, "trust.trust_session", &[]),
            "trust-session",
        ));
        items.push(
            SelectItem::new(crate::i18n::t(self.lang, "trust.distrust", &[]), "distrust")
                .with_description(crate::i18n::t(self.lang, "trust.distrust_desc", &[])),
        );
        items.push(SelectItem::new(
            crate::i18n::t(self.lang, "trust.distrust_session", &[]),
            "distrust-session",
        ));
        self.dialog = Some(Dialog::Select(SelectDialog::new(
            crate::i18n::t(
                self.lang,
                "trust.title",
                &[("path", &self.cwd.display().to_string())],
            ),
            items,
            SelectPurpose::ProjectTrust,
            self.theme,
        )));
    }

    pub(crate) fn apply_trust(&mut self, value: &str) {
        use crate::project_trust as trust;
        match value {
            "trust" => trust::set_decision(&self.agent_dir, &self.cwd, true, false),
            "parent" => trust::set_parent_decision(&self.agent_dir, &self.cwd),
            "trust-session" => trust::set_decision(&self.agent_dir, &self.cwd, true, true),
            "distrust" => trust::set_decision(&self.agent_dir, &self.cwd, false, false),
            "distrust-session" => trust::set_decision(&self.agent_dir, &self.cwd, false, true),
            _ => return,
        }
        let trusted = trust::is_trusted(&self.cwd, &self.agent_dir);
        self.command_reload();
        self.notice(
            if trusted {
                crate::i18n::t(self.lang, "trust.now_trusted", &[])
            } else {
                crate::i18n::t(self.lang, "trust.now_untrusted", &[])
            },
            NoticeKind::Info,
        );
    }
}
