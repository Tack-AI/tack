//! `/ext` — inspect installed plugins from inside the TUI.
//!
//! Read-only view of the ExtensionManager snapshot taken at session
//! start. Management stays on the `tack ext` CLI: hot reload in
//! production sessions is a documented non-goal (plugin capabilities are
//! woven into the agent loop at session start), so this command only
//! *reports* — newly installed plugins appear after a restart.

use super::super::{ChatEntry, NoticeKind, TuiApp};
use super::{TranscriptItem, assistant_notice};

impl TuiApp {
    /// `/ext [list]`: render the discovered plugins with their load state
    /// (active/disabled/failed/policy-filtered), version, directory, and
    /// contributed capabilities (or the failure/policy reason).
    pub(crate) fn command_ext(&mut self, args: &str) {
        let sub = args.split_whitespace().next().unwrap_or("list");
        if sub != "list" {
            self.notice(
                crate::i18n::t(self.lang, "ext.unknown", &[("sub", sub)]),
                NoticeKind::Warning,
            );
            return;
        }
        let mut text = crate::i18n::t(self.lang, "ext.header", &[]);
        if self.extensions.plugins.is_empty() {
            text.push_str(&crate::i18n::t(self.lang, "ext.none", &[]));
        }
        for plugin in &self.extensions.plugins {
            text.push_str(&crate::i18n::t(
                self.lang,
                "ext.row",
                &[
                    ("id", &plugin.id.to_string()),
                    ("state", plugin.outcome()),
                    ("version", &plugin.version),
                    ("dir", &plugin.dir.display().to_string()),
                ],
            ));
            if let Some(detail) = plugin_detail(plugin) {
                text.push_str(&crate::i18n::t(
                    self.lang,
                    "ext.row_detail",
                    &[("detail", &detail)],
                ));
            }
        }
        if !self.extensions.load_warnings.is_empty() {
            text.push_str(&crate::i18n::t(self.lang, "ext.warnings_header", &[]));
            for warning in &self.extensions.load_warnings {
                text.push_str(&format!("- {warning}\n"));
            }
        }
        self.items.push(TranscriptItem::Chat(ChatEntry::Assistant {
            message: assistant_notice(&text),
            streaming: false,
        }));
    }
}

/// The indented second line of a plugin row: the failure or policy
/// reason, else a capability summary for running plugins. The vocabulary
/// matches `tack ext list` / the handshake (technical terms, English in
/// both locales).
fn plugin_detail(plugin: &crate::extension_host::LoadedPlugin) -> Option<String> {
    if let Some(error) = &plugin.error {
        return Some(format!("error: {error}"));
    }
    if let Some(reason) = &plugin.policy_block {
        return Some(format!("policy: {reason}"));
    }
    let caps = &plugin.register.as_ref()?.capabilities;
    let mut parts: Vec<String> = Vec::new();
    if let Some(tools) = &caps.tools {
        parts.push(format!("tools: {}", tools.len()));
    }
    if let Some(commands) = &caps.commands {
        parts.push(format!("commands: {}", commands.len()));
    }
    if let Some(hooks) = &caps.hooks {
        let mut names = Vec::new();
        if hooks.before_tool_call == Some(true) {
            names.push("beforeToolCall");
        }
        if hooks.after_tool_call == Some(true) {
            names.push("afterToolCall");
        }
        if hooks.transform_context == Some(true) {
            names.push("transformContext");
        }
        if hooks.approval_review == Some(true) {
            names.push("approvalReview");
        }
        if !names.is_empty() {
            parts.push(format!("hooks: {}", names.join(", ")));
        }
    }
    if let Some(widgets) = &caps.widgets {
        parts.push(format!("widgets: {}", widgets.len()));
    }
    if let Some(provider) = &caps.provider {
        let mut kinds = Vec::new();
        if provider.stream == Some(true) {
            kinds.push("stream");
        }
        if provider.register == Some(true) {
            kinds.push("register");
        }
        parts.push(format!("provider: {}", kinds.join("+")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}
