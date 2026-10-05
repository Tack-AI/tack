//! Declarative permission rules (Claude-Code-style `permissions.allow/deny`)
//! plus durable allow-always storage. Third layer of the defense model:
//!   features.*  — is the capability present at all?
//!   permissions — may THIS call (tool + pattern) proceed without asking?
//!   sandbox     — what can it touch when it runs?
//!
//! Rule syntax: `"Tool(pattern)"` — e.g. `Bash(npm run *)`, `Edit(src/**)`,
//! `Write(*.env)`, or a bare `Bash` (all calls of that tool). `deny` always
//! wins over `allow`. Matching: shell commands (bash/powershell/git) use
//! `*` wildcards; file paths use glob (`**` crosses directories).
//!
//! Persistence: "allow always" answers are appended to
//! `<agent dir>/permissions.json` ({"allowAlways": [...]}), merged with the
//! settings rules at load.

use std::path::Path;

use serde_json::{Value, json};

/// Tools that never need a prompt (read-only builtins). The `git` tool is
/// classified by subcommand: only pure-read invocations are read-only.
/// MCP tools (`mcp__*`) are classified by the SERVER-DECLARED
/// `readOnlyHint` annotation (published to `tack_tools::mcp`'s registry
/// when the tool set is built): a hint is honored only when the server
/// did not also mark the tool destructive — contradictory hints resolve
/// to the safe side.
pub fn is_read_only_tool(name: &str, args: &Value) -> bool {
    match name {
        "read" | "grep" | "find" | "ls" => true,
        "git" => {
            if let Some(command) = args.get("command").and_then(Value::as_str) {
                return tack_tools::git_tool::is_read_only_command(command);
            }
            // Batch form: read-only only if EVERY entry is.
            if let Some(entries) = args.get("commands").and_then(Value::as_array) {
                let commands: Vec<String> = entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                return tack_tools::git_tool::is_read_only_commands(&commands);
            }
            false
        }
        _ if name.starts_with("mcp__") => tack_tools::mcp::mcp_tool_annotations(name)
            .map(|a| a.read_only && !a.destructive)
            .unwrap_or(false),
        _ => false,
    }
}

/// One parsed rule.
///
/// `matcher` is the glob pattern compiled ONCE at parse time (file-tool
/// rules only): glob compilation is far too expensive to redo per tool call
/// × per rule. Shell-command rules (bash/git/powershell) use `*`-wildcard
/// matching instead and keep `matcher: None`.
#[derive(Clone)]
pub struct Rule {
    /// Tool name as written (case-insensitive match for built-ins).
    pub tool: String,
    /// None = all calls of this tool.
    pub pattern: Option<String>,
    /// Precompiled glob for `pattern` (None for shell-command rules, bare
    /// tool rules, and invalid globs — those fall back to literal compare,
    /// same as before precompilation).
    matcher: Option<globset::GlobMatcher>,
}

/// Equality/identity of a rule is its written form (the compiled matcher
/// is derived state).
impl PartialEq for Rule {
    fn eq(&self, other: &Self) -> bool {
        self.tool == other.tool && self.pattern == other.pattern
    }
}
impl Eq for Rule {}

impl std::fmt::Debug for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rule")
            .field("tool", &self.tool)
            .field("pattern", &self.pattern)
            .finish()
    }
}

/// Shell-command tools match with `*` wildcards, not path globs.
fn is_shell_rule_tool(tool: &str) -> bool {
    tool.eq_ignore_ascii_case("bash")
        || tool.eq_ignore_ascii_case("git")
        || tool.eq_ignore_ascii_case("powershell")
}

/// Compile a path-glob matcher with the exact semantics glob_match has
/// always used (`*`/`**` both cross directories).
fn compile_glob(pattern: &str) -> Option<globset::GlobMatcher> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

impl Rule {
    /// Parse `"Bash(npm run *)"` / `"Edit(src/**)"` / `"Bash"`.
    pub fn parse(input: &str) -> Option<Rule> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        if let Some(open) = input.find('(') {
            let tool = input[..open].trim().to_string();
            let pattern = input[open + 1..].strip_suffix(')')?.to_string();
            if tool.is_empty() {
                return None;
            }
            let matcher = if is_shell_rule_tool(&tool) {
                None
            } else {
                compile_glob(&pattern)
            };
            Some(Rule {
                tool,
                pattern: Some(pattern),
                matcher,
            })
        } else {
            Some(Rule {
                tool: input.to_string(),
                pattern: None,
                matcher: None,
            })
        }
    }

    pub fn render(&self) -> String {
        match &self.pattern {
            Some(p) => format!("{}({})", self.tool, p),
            None => self.tool.clone(),
        }
    }

    /// The match target for a tool call: bash → command, file tools → path,
    /// everything else → first present of path/command/pattern/url.
    fn target<'a>(&self, tool_name: &str, args: &'a Value) -> &'a str {
        let _ = tool_name;
        args.get("path")
            .or_else(|| args.get("command"))
            .or_else(|| args.get("pattern"))
            .or_else(|| args.get("url"))
            .or_else(|| args.get("query"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// One target per git `commands` batch entry; otherwise a single target.
    fn targets<'a>(&self, tool_name: &str, args: &'a Value) -> Vec<&'a str> {
        if let Some(entries) = args.get("commands").and_then(Value::as_array) {
            return entries.iter().filter_map(Value::as_str).collect();
        }
        vec![self.target(tool_name, args)]
    }

    fn matches_target(&self, tool_name: &str, target: &str) -> bool {
        let pattern = self.pattern.as_ref().expect("checked by caller");
        if tool_name == "bash" || tool_name == "git" || tool_name == "powershell" {
            wildcard_match(pattern, target)
        } else if let Some(matcher) = &self.matcher {
            // Precompiled glob (identical semantics to glob_match: exact
            // fast path, then the normalized-path matcher).
            pattern == target || matcher.is_match(target.replace('\\', "/"))
        } else {
            glob_match(pattern, target)
        }
    }

    /// deny semantics: a batch matches when ANY entry matches — a denied
    /// subcommand anywhere in the batch denies the whole call.
    fn matches_deny(&self, tool_name: &str, args: &Value) -> bool {
        if !self.tool.eq_ignore_ascii_case(tool_name) {
            return false;
        }
        if self.pattern.is_none() {
            return true;
        }
        let targets = self.targets(tool_name, args);
        !targets.is_empty() && targets.iter().any(|t| self.matches_target(tool_name, t))
    }

    /// allow semantics: a batch matches only when EVERY entry matches —
    /// `Git(status*)` must not approve ["status", "push origin main"].
    fn matches_allow(&self, tool_name: &str, args: &Value) -> bool {
        if !self.tool.eq_ignore_ascii_case(tool_name) {
            return false;
        }
        if self.pattern.is_none() {
            return true;
        }
        let targets = self.targets(tool_name, args);
        !targets.is_empty() && targets.iter().all(|t| self.matches_target(tool_name, t))
    }
}

/// `*` matches any byte sequence (bash commands). Whitespace runs are
/// collapsed first so deny rules cannot be sidestepped by padding a command
/// with extra spaces (e.g. deny `Git(push --force*)` vs `push  --force`).
fn wildcard_match(pattern: &str, target: &str) -> bool {
    if pattern == target {
        return true;
    }
    let pattern = pattern.split_whitespace().collect::<Vec<_>>().join(" ");
    let target = target.split_whitespace().collect::<Vec<_>>().join(" ");
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return false;
    }
    let mut rest = target.as_str();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(idx) => {
                // The first segment must anchor at the start.
                if i == 0 && idx != 0 {
                    return false;
                }
                rest = &rest[idx + part.len()..];
            }
            None => return false,
        }
    }
    // The last segment must anchor at the end (unless pattern ends with *).
    if let Some(last) = parts.last()
        && !last.is_empty()
        && !target.ends_with(last)
    {
        return false;
    }
    true
}

/// Glob for file paths (`**` crosses directories) via globset.
fn glob_match(pattern: &str, target: &str) -> bool {
    if pattern == target {
        return true;
    }
    let normalized = target.replace('\\', "/");
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build();
    match glob {
        Ok(g) => g.compile_matcher().is_match(&normalized),
        Err(_) => pattern == normalized,
    }
}

/// The full rule set: settings rules + persisted allow-always.
#[derive(Clone, Debug, Default)]
pub struct PermissionRules {
    pub allow: Vec<Rule>,
    pub deny: Vec<Rule>,
}

impl PermissionRules {
    /// Build the rule set from Settings (rules are already the union of all
    /// config layers) plus the persisted allow-always file.
    pub fn load(settings: &crate::settings::Settings, agent_dir: &Path) -> Self {
        let mut rules = PermissionRules {
            allow: settings
                .permission_allow
                .iter()
                .filter_map(|s| Rule::parse(s))
                .collect(),
            deny: settings
                .permission_deny
                .iter()
                .filter_map(|s| Rule::parse(s))
                .collect(),
        };
        rules.allow.extend(load_persisted(agent_dir));
        rules
    }

    /// deny wins over everything.
    pub fn deny_match(&self, tool_name: &str, args: &Value) -> Option<&Rule> {
        self.deny.iter().find(|r| r.matches_deny(tool_name, args))
    }

    pub fn allow_match(&self, tool_name: &str, args: &Value) -> Option<&Rule> {
        self.allow.iter().find(|r| r.matches_allow(tool_name, args))
    }
}

fn persisted_path(agent_dir: &Path) -> std::path::PathBuf {
    agent_dir.join("permissions.json")
}

// ---------------------------------------------------------------------------
// Plugin-version binding for ext__* approvals
// ---------------------------------------------------------------------------
//
// `tack ext upgrade` swaps the code behind an `ext__<plugin>__<tool>` tool
// name, so an "always allow" granted against one version must not apply to
// the next (unreviewed) version. Allow-always cache keys and persisted
// permissions.json entries for ext__* tools therefore carry the LOADED
// plugin version: the in-memory key embeds it, and the persisted file
// records it in an additive `extToolVersions` map. Legacy persisted
// entries (no recorded version) and entries whose recorded version no
// longer matches the loaded one are ignored — fail-closed, never a crash.

/// The sanitized plugin-id part of an `ext__<plugin>__<tool>` name.
fn ext_tool_plugin_part(tool_name: &str) -> Option<&str> {
    tool_name
        .strip_prefix("ext__")?
        .split_once("__")
        .map(|(p, _)| p)
}

/// The charset sanitizer ExtTool (tack-ext) applies to plugin ids and
/// tool names ([A-Za-z0-9_-]; everything else becomes `_`). Only the
/// load-report lookup (ext feature) needs it.
#[cfg(feature = "ext")]
fn sanitize_ext_part(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The version of the plugin behind an ext__* tool as loaded this
/// session, from the extension load report (the extension host owns the
/// live version map; the report is the observable snapshot reachable
/// from here). None when the report is missing/unreadable or the plugin
/// is not in it — callers treat None as fail-closed.
#[cfg(feature = "ext")]
fn loaded_plugin_version(agent_dir: &Path, tool_name: &str) -> Option<String> {
    let part = ext_tool_plugin_part(tool_name)?;
    let report = crate::extension_host::read_load_report(agent_dir)?;
    report
        .plugins
        .iter()
        .filter(|row| row.outcome == "active")
        .find(|row| sanitize_ext_part(&row.id) == part)
        .map(|row| row.version.clone())
}

/// Without the ext feature no plugin can ever be loaded.
#[cfg(not(feature = "ext"))]
fn loaded_plugin_version(_agent_dir: &Path, _tool_name: &str) -> Option<String> {
    None
}

/// Key for allow-always caching: `tool:first_arg` — with the loaded
/// plugin version embedded for ext__* tools (`tool@version:first_arg`) so
/// a plugin upgrade invalidates stale approvals. When the version cannot
/// be determined the key binds to a version nothing persisted can match
/// (fail-closed: the user re-approves rather than a stale entry silently
/// applying to unknown code).
pub fn allow_always_key(agent_dir: &Path, tool_name: &str, args: &Value) -> String {
    let first = args
        .get("path")
        .or_else(|| args.get("command"))
        .or_else(|| args.get("pattern"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if ext_tool_plugin_part(tool_name).is_some() {
        let version =
            loaded_plugin_version(agent_dir, tool_name).unwrap_or_else(|| "unknown".to_string());
        format!("{tool_name}@{version}:{first}")
    } else {
        format!("{tool_name}:{first}")
    }
}

/// Fail-closed gate for one persisted allow-always entry (raw rule
/// string). ext__* entries survive only when a version was recorded at
/// approval time AND it matches the version loaded now; every other tool
/// passes through unchanged.
fn persisted_entry_current(
    entry: &str,
    agent_dir: &Path,
    recorded: Option<&serde_json::Map<String, Value>>,
) -> bool {
    let tool = entry.split('(').next().unwrap_or(entry).trim();
    if ext_tool_plugin_part(tool).is_none() {
        return true;
    }
    let Some(recorded) = recorded.and_then(|m| m.get(tool)).and_then(Value::as_str) else {
        // Legacy entry (pre-version-binding) or missing record: ignore.
        return false;
    };
    loaded_plugin_version(agent_dir, tool).as_deref() == Some(recorded)
}

fn load_persisted(agent_dir: &Path) -> Vec<Rule> {
    let content = std::fs::read_to_string(persisted_path(agent_dir))
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok());
    content
        .map(|v| {
            let recorded = v["extToolVersions"].as_object();
            v["allowAlways"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .filter(|s| persisted_entry_current(s, agent_dir, recorded))
                        .filter_map(Rule::parse)
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// Persist an allow-always answer as an exact-match rule. For ext__*
/// tools the loaded plugin version is recorded alongside (additive
/// `extToolVersions` map) so a later `tack ext upgrade` invalidates the
/// entry at load time — see the module section on version binding above.
pub fn persist_allow_always(agent_dir: &Path, tool_name: &str, args: &Value) {
    let target = args
        .get("path")
        .or_else(|| args.get("command"))
        .or_else(|| args.get("pattern"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if target.is_empty() {
        return;
    }
    let rule = format!("{tool_name}({target})");
    // Bound the version check to the load report BEFORE writing: when the
    // version is unknown the entry would be dropped at the next load
    // anyway (fail-closed gate), so persist nothing in that case.
    let ext_version = if ext_tool_plugin_part(tool_name).is_some() {
        match loaded_plugin_version(agent_dir, tool_name) {
            Some(version) => Some(version),
            None => {
                tracing::warn!(
                    "not persisting allow-always for {tool_name}: loaded plugin version unknown"
                );
                return;
            }
        }
    } else {
        None
    };
    let path = persisted_path(agent_dir);
    let mut content: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| json!({ "allowAlways": [] }));
    // A malformed file (non-object JSON, or allowAlways of the wrong type)
    // must not panic the permission prompt — start that section fresh.
    if !content.is_object() {
        content = json!({ "allowAlways": [] });
    }
    if !content["allowAlways"].is_array() {
        content["allowAlways"] = json!([]);
    }
    if let Some(version) = &ext_version {
        if !content["extToolVersions"].is_object() {
            content["extToolVersions"] = json!({});
        }
        content["extToolVersions"][tool_name] = Value::String(version.clone());
    }
    let list = content["allowAlways"]
        .as_array_mut()
        .expect("allowAlways array");
    let rule_new = !list.iter().any(|v| v.as_str() == Some(rule.as_str()));
    if rule_new {
        list.push(Value::String(rule));
    }
    // Skip the disk write only when nothing changed (existing rule, no
    // version record to add/update).
    if !rule_new && ext_version.is_none() {
        return;
    }
    if let Err(e) = crate::atomic_write::atomic_write(
        &path,
        &serde_json::to_string_pretty(&content).unwrap_or_default(),
    ) {
        tracing::warn!("cannot persist allow-always to {}: {e}", path.display());
    }
}

/// Headless deny enforcement: print/rpc/serve/mcp-serve have no permission
/// prompts (everything runs), so only `deny` rules apply — this is the CI /
/// unattended safety net.
pub struct DenyRulesHooks {
    pub rules: PermissionRules,
}

impl std::fmt::Debug for DenyRulesHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DenyRulesHooks").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl tack_agent_core::AgentHooks for DenyRulesHooks {
    async fn before_tool_call(
        &self,
        ctx: &tack_agent_core::BeforeToolCallContext<'_>,
    ) -> tack_agent_core::BeforeToolCallOutcome {
        if let Some(rule) = self.rules.deny_match(ctx.tool_name, ctx.args) {
            return tack_agent_core::BeforeToolCallOutcome::Block {
                reason: Some(format!(
                    "denied by permissions.deny rule \"{}\"",
                    rule.render()
                )),
                terminate: false,
            };
        }
        tack_agent_core::BeforeToolCallOutcome::Allow
    }
}

/// Wrap session hooks with deny-rule enforcement when the settings carry
/// any `permissions.deny` rules. The deny hooks run FIRST — before the
/// inner hooks, which may prompt the user — so a denied tool never
/// surfaces a prompt (deny wins over everything, including bypass modes).
/// No-op passthrough when there are no deny rules.
pub fn chain_with_deny_rules(
    settings: &crate::settings::Settings,
    agent_dir: &Path,
    inner: std::sync::Arc<dyn tack_agent_core::AgentHooks>,
) -> std::sync::Arc<dyn tack_agent_core::AgentHooks> {
    let deny_rules = PermissionRules::load(settings, agent_dir);
    if deny_rules.deny.is_empty() {
        return inner;
    }
    std::sync::Arc::new(tack_agent_core::HooksChain::new(vec![
        std::sync::Arc::new(DenyRulesHooks { rules: deny_rules }),
        inner,
    ]))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// ACP/regression: `chain_with_deny_rules` must put the deny hooks
    /// FIRST so a denied tool is blocked before the inner hooks (which may
    /// auto-allow in bypass mode) ever see the call — and pass the inner
    /// hooks through untouched when no deny rules are configured.
    #[tokio::test]
    async fn deny_chain_blocks_before_inner_hooks() {
        use tack_agent_core::{AgentHooks, BeforeToolCallContext, BeforeToolCallOutcome};

        #[derive(Debug)]
        struct AllowAll;
        #[async_trait::async_trait]
        impl AgentHooks for AllowAll {}

        let model = tack_ai::Model {
            id: "mock".into(),
            name: "Mock".into(),
            api: "mock".into(),
            provider: "mock".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![tack_ai::InputKind::Text],
            cost: tack_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            sampling_params: None,
            headers: None,
            compat: None,
        };
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = serde_json::json!({"command": "rm -rf /x"});
        let ctx = BeforeToolCallContext {
            assistant_message: &message,
            tool_call_id: "call-1",
            tool_name: "bash",
            args: &args,
            context: &[],
        };

        let tmp = tempfile::tempdir().unwrap();
        let mut settings = crate::settings::Settings::default();
        settings.permission_deny = vec!["Bash(rm *)".to_string()];
        let inner: std::sync::Arc<dyn AgentHooks> = std::sync::Arc::new(AllowAll);
        let chained = chain_with_deny_rules(&settings, tmp.path(), inner.clone());
        let outcome = chained.before_tool_call(&ctx).await;
        assert!(
            matches!(outcome, BeforeToolCallOutcome::Block { .. }),
            "deny rule must block even though the inner hooks allow everything"
        );

        // No deny rules: the inner hooks are returned as-is.
        let plain = chain_with_deny_rules(
            &crate::settings::Settings::default(),
            tmp.path(),
            inner.clone(),
        );
        assert!(std::sync::Arc::ptr_eq(&plain, &inner));
        assert!(matches!(
            plain.before_tool_call(&ctx).await,
            BeforeToolCallOutcome::Allow
        ));
    }

    #[test]
    fn parses_rule_forms() {
        let rule = Rule::parse("Bash(npm run *)").unwrap();
        assert_eq!(rule.tool, "Bash");
        assert_eq!(rule.pattern.as_deref(), Some("npm run *"));
        let bare = Rule::parse("Bash").unwrap();
        assert_eq!(bare.tool, "Bash");
        assert_eq!(bare.pattern, None);
        assert!(Rule::parse("(oops)").is_none());
    }

    #[test]
    fn precompiled_globs_match_like_glob_match() {
        // F37 regression: the matcher compiled at parse time must be
        // semantically identical to the old per-call glob_match.
        let rules = PermissionRules {
            allow: vec![Rule::parse("Edit(src/**)").unwrap()],
            deny: vec![Rule::parse("Write(*.env)").unwrap()],
        };
        assert!(
            rules
                .allow_match(
                    "edit",
                    &serde_json::json!({ "path": "src/deep/nested/a.rs" })
                )
                .is_some()
        );
        assert!(
            rules
                .allow_match("edit", &serde_json::json!({ "path": "tests/a.rs" }))
                .is_none()
        );
        // literal_separator(false): `*` crosses directories.
        assert!(
            rules
                .deny_match("write", &serde_json::json!({ "path": "config/prod.env" }))
                .is_some()
        );
        // Backslash normalization (Windows paths) still applies.
        assert!(
            rules
                .allow_match("edit", &serde_json::json!({ "path": "src\\deep\\a.rs" }))
                .is_some()
        );
        // An invalid glob falls back to literal comparison (parse never fails).
        let rules = PermissionRules {
            allow: vec![Rule::parse("Edit([unclosed)").unwrap()],
            deny: vec![],
        };
        assert!(
            rules
                .allow_match("edit", &serde_json::json!({ "path": "[unclosed" }))
                .is_some()
        );
        assert!(
            rules
                .allow_match("edit", &serde_json::json!({ "path": "other" }))
                .is_none()
        );
    }

    #[test]
    fn bash_wildcards() {
        assert!(wildcard_match("npm run *", "npm run test -- --watch"));
        assert!(wildcard_match("cargo test", "cargo test"));
        assert!(!wildcard_match("cargo test", "cargo test --workspace"));
        assert!(wildcard_match("git * push", "git -C repo push"));
        assert!(!wildcard_match("npm run *", "yarn run test"));
        assert!(wildcard_match("rm *", "rm -rf /"));
        assert!(!wildcard_match("* safe", "unsafe safe string"));
    }

    #[test]
    fn wildcards_ignore_whitespace_padding() {
        // Deny rules must not be sidestepped by extra spaces/tabs.
        assert!(wildcard_match("push --force*", "push  --force"));
        assert!(wildcard_match("push --force*", "push\t--force origin main"));
        assert!(wildcard_match("git push *", "git  push   origin main"));
        assert!(!wildcard_match("push --force*", "push origin main"));
    }

    /// MCP read-only gating honors the server-declared `readOnlyHint`
    /// (registry published when the tool set is built), but a
    /// contradictory destructive hint resolves to the safe side.
    #[test]
    fn mcp_read_only_hint_gating() {
        use tack_tools::mcp::{McpToolAnnotations, register_tool_annotations_for_test};
        register_tool_annotations_for_test(
            "mcp__t__search",
            McpToolAnnotations {
                read_only: true,
                destructive: false,
                idempotent: true,
                open_world: true,
            },
        );
        register_tool_annotations_for_test(
            "mcp__t__confused",
            McpToolAnnotations {
                read_only: true,
                destructive: true,
                idempotent: false,
                open_world: true,
            },
        );
        register_tool_annotations_for_test(
            "mcp__t__write",
            McpToolAnnotations {
                read_only: false,
                destructive: true,
                idempotent: false,
                open_world: true,
            },
        );
        let args = serde_json::json!({});
        assert!(is_read_only_tool("mcp__t__search", &args));
        assert!(
            !is_read_only_tool("mcp__t__confused", &args),
            "readOnly+destructive is contradictory: not read-only"
        );
        assert!(!is_read_only_tool("mcp__t__write", &args));
        assert!(
            !is_read_only_tool("mcp__t__unknown", &args),
            "unregistered MCP tools are not read-only"
        );
    }

    #[test]
    fn git_rules_match_command_and_read_only_gating() {
        // Git(...) rules match against the tool's `command` argument.
        let rules = PermissionRules {
            allow: vec![],
            deny: vec![Rule::parse("Git(push --force*)").unwrap()],
        };
        let forced = serde_json::json!({ "command": "push  --force origin main" });
        assert!(rules.deny_match("git", &forced).is_some());
        let normal = serde_json::json!({ "command": "push origin main" });
        assert!(rules.deny_match("git", &normal).is_none());

        // Plan/ask modes auto-approve only structurally read-only git calls;
        // write/exec options on read subcommands must not slip through.
        assert!(is_read_only_tool(
            "git",
            &serde_json::json!({ "command": "log --oneline" })
        ));
        assert!(!is_read_only_tool(
            "git",
            &serde_json::json!({ "command": "log --output=/tmp/x" })
        ));
        assert!(!is_read_only_tool(
            "git",
            &serde_json::json!({ "command": "grep -Oevil x" })
        ));
        assert!(!is_read_only_tool(
            "git",
            &serde_json::json!({ "command": "stash" })
        ));
        assert!(is_read_only_tool(
            "git",
            &serde_json::json!({ "command": "stash list" })
        ));
    }

    #[test]
    fn git_batch_rules_and_read_only_gating() {
        // Read-only gating: every batch entry must be read-only.
        assert!(is_read_only_tool(
            "git",
            &serde_json::json!({ "commands": ["status", "log --oneline -5"] })
        ));
        assert!(!is_read_only_tool(
            "git",
            &serde_json::json!({ "commands": ["status", "push origin main"] })
        ));
        assert!(!is_read_only_tool(
            "git",
            &serde_json::json!({ "commands": [] })
        ));

        // deny = ANY entry matches: a denied subcommand anywhere in the
        // batch denies the whole call.
        let rules = PermissionRules {
            allow: vec![Rule::parse("Git(status*)").unwrap()],
            deny: vec![Rule::parse("Git(push*)").unwrap()],
        };
        let batch = serde_json::json!({ "commands": ["status", "push origin main"] });
        assert!(rules.deny_match("git", &batch).is_some());

        // allow = ALL entries must match: Git(status*) must not approve a
        // batch smuggling a second command.
        assert!(rules.allow_match("git", &batch).is_none());
        let clean = serde_json::json!({ "commands": ["status", "status --short"] });
        assert!(rules.allow_match("git", &clean).is_some());
    }

    #[test]
    fn path_globs() {
        assert!(glob_match("src/**", "src/deep/nested/file.rs"));
        assert!(glob_match("*.env", ".env"));
        assert!(glob_match("*.env", "prod.env"));
        assert!(!glob_match("src/**", "tests/foo.rs"));
        assert!(glob_match("D:/work/**", "D:\\work\\src\\a.rs"));
    }

    #[test]
    fn glob_star_crosses_directories_today() {
        // Documents current behavior (literal_separator(false)): `*` and `**`
        // both cross `/`. Tightening `*` to a single segment would also
        // weaken existing deny rules like `Write(*.env)` for nested paths,
        // so the permissive behavior is kept deliberately.
        assert!(glob_match("*.env", "config/prod.env"));
        assert!(glob_match("src/*", "src/deep/file.rs"));
    }

    #[test]
    fn deny_wins_and_allow_matches() {
        let rules = PermissionRules {
            allow: vec![Rule::parse("Bash(npm run *)").unwrap()],
            deny: vec![Rule::parse("Bash(npm run publish*)").unwrap()],
        };
        let args = serde_json::json!({ "command": "npm run test" });
        assert!(rules.allow_match("bash", &args).is_some());
        assert!(rules.deny_match("bash", &args).is_none());
        let args = serde_json::json!({ "command": "npm run publish --dry-run" });
        assert!(rules.deny_match("bash", &args).is_some());
    }

    /// The optional PowerShell tool matches like bash: `*` wildcards against
    /// the `command` arg, case-insensitive tool name (TS rule syntax
    /// `PowerShell(...)`), deny wins over allow.
    #[test]
    fn powershell_rules_match_like_bash() {
        let rules = PermissionRules {
            allow: vec![Rule::parse("PowerShell(Get-*)").unwrap()],
            deny: vec![Rule::parse("PowerShell(Remove-Item *)").unwrap()],
        };
        let args = serde_json::json!({ "command": "Get-ChildItem -Recurse" });
        assert!(rules.allow_match("powershell", &args).is_some());
        assert!(rules.deny_match("powershell", &args).is_none());
        let args = serde_json::json!({ "command": "Remove-Item -Recurse C:\\temp" });
        assert!(rules.deny_match("powershell", &args).is_some());
        assert!(rules.allow_match("powershell", &args).is_none());
        // Bare tool name matches every call.
        let rules = PermissionRules {
            allow: vec![Rule::parse("PowerShell").unwrap()],
            deny: vec![],
        };
        assert!(
            rules
                .allow_match("powershell", &serde_json::json!({ "command": "anything" }))
                .is_some()
        );
        // Wildcard (not path glob) semantics: `*` crosses any characters.
        let rules = PermissionRules {
            allow: vec![Rule::parse("PowerShell(git * push*)").unwrap()],
            deny: vec![],
        };
        assert!(
            rules
                .allow_match(
                    "powershell",
                    &serde_json::json!({ "command": "git -C repo push origin main" })
                )
                .is_some()
        );
    }

    #[test]
    fn persist_tolerates_malformed_file() {
        // Regression: a permissions.json holding non-object JSON (or an
        // allowAlways of the wrong type) panicked the permission flow.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("permissions.json"), "[1, 2]").unwrap();
        persist_allow_always(
            tmp.path(),
            "bash",
            &serde_json::json!({ "command": "cargo test" }),
        );
        assert!(
            PermissionRules::load(&crate::settings::Settings::default(), tmp.path())
                .allow_match("bash", &serde_json::json!({ "command": "cargo test" }))
                .is_some()
        );

        std::fs::write(
            tmp.path().join("permissions.json"),
            "{\"allowAlways\": \"oops\"}",
        )
        .unwrap();
        persist_allow_always(tmp.path(), "edit", &serde_json::json!({ "path": "a.rs" }));
        assert!(
            PermissionRules::load(&crate::settings::Settings::default(), tmp.path())
                .allow_match("edit", &serde_json::json!({ "path": "a.rs" }))
                .is_some()
        );
    }

    #[test]
    fn persist_and_reload_allow_always() {
        let tmp = tempfile::tempdir().unwrap();
        persist_allow_always(
            tmp.path(),
            "bash",
            &serde_json::json!({ "command": "cargo test" }),
        );
        persist_allow_always(
            tmp.path(),
            "bash",
            &serde_json::json!({ "command": "cargo test" }),
        ); // dedupe
        persist_allow_always(
            tmp.path(),
            "edit",
            &serde_json::json!({ "path": "src/main.rs" }),
        );

        let rules = PermissionRules::load(&crate::settings::Settings::default(), tmp.path());
        assert_eq!(rules.allow.len(), 2);
        assert!(
            rules
                .allow_match("bash", &serde_json::json!({ "command": "cargo test" }))
                .is_some()
        );
        assert!(
            rules
                .allow_match("edit", &serde_json::json!({ "path": "src/main.rs" }))
                .is_some()
        );
    }

    /// Write a fake extension load report with one active plugin.
    #[cfg(feature = "ext")]
    fn write_load_report(agent_dir: &Path, plugin_id: &str, version: &str) {
        let dir = agent_dir.join("extensions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("last-load.json"),
            serde_json::json!({
                "version": 1,
                "ts": 0,
                "mode": "tui",
                "plugins": [{
                    "id": plugin_id,
                    "outcome": "active",
                    "version": version,
                    "dir": "/x",
                }],
                "warnings": [],
            })
            .to_string(),
        )
        .unwrap();
    }

    /// ext__* allow-always keys embed the loaded plugin version; built-in
    /// tools keep the legacy `tool:first_arg` format.
    #[cfg(feature = "ext")]
    #[test]
    fn allow_always_key_binds_plugin_version() {
        let tmp = tempfile::tempdir().unwrap();
        write_load_report(tmp.path(), "guard@acme", "1.2.3");
        let args = serde_json::json!({ "command": "scan ." });
        assert_eq!(
            allow_always_key(tmp.path(), "ext__guard_acme__scan", &args),
            "ext__guard_acme__scan@1.2.3:scan ."
        );
        assert_eq!(allow_always_key(tmp.path(), "bash", &args), "bash:scan .");
        // Unknown version (plugin not in the report): binds to a key no
        // persisted entry can satisfy (fail-closed).
        assert_eq!(
            allow_always_key(tmp.path(), "ext__other__tool", &args),
            "ext__other__tool@unknown:scan ."
        );
    }

    /// Version-keyed persistence: an ext__* approval is recorded with the
    /// loaded version and survives reload ONLY while the version matches;
    /// an upgrade (or a legacy entry with no recorded version) drops it.
    #[cfg(feature = "ext")]
    #[test]
    fn ext_allow_always_entries_are_version_bound() {
        let tmp = tempfile::tempdir().unwrap();
        write_load_report(tmp.path(), "guard@acme", "1.2.3");
        let args = serde_json::json!({ "command": "scan ." });

        persist_allow_always(tmp.path(), "ext__guard_acme__scan", &args);
        persist_allow_always(tmp.path(), "bash", &args);
        let loaded: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("permissions.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            loaded["extToolVersions"]["ext__guard_acme__scan"],
            serde_json::json!("1.2.3")
        );

        // Same version: the entry applies.
        let rules = PermissionRules::load(&crate::settings::Settings::default(), tmp.path());
        assert!(
            rules.allow_match("ext__guard_acme__scan", &args).is_some(),
            "matching version keeps the approval"
        );
        assert!(rules.allow_match("bash", &args).is_some());

        // Upgrade: version changes, the stale approval must NOT apply to
        // the unreviewed new code (the bash entry is unaffected).
        write_load_report(tmp.path(), "guard@acme", "2.0.0");
        let rules = PermissionRules::load(&crate::settings::Settings::default(), tmp.path());
        assert!(
            rules.allow_match("ext__guard_acme__scan", &args).is_none(),
            "upgraded plugin invalidates the approval"
        );
        assert!(rules.allow_match("bash", &args).is_some());
    }

    /// Legacy ext__* entries (persisted before version binding, no
    /// extToolVersions record) are ignored safely — fail-closed, no crash.
    #[cfg(feature = "ext")]
    #[test]
    fn legacy_ext_entries_are_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        write_load_report(tmp.path(), "guard@acme", "1.2.3");
        std::fs::write(
            tmp.path().join("permissions.json"),
            r#"{ "allowAlways": ["ext__guard_acme__scan(scan .)", "Bash(cargo test)"] }"#,
        )
        .unwrap();
        let rules = PermissionRules::load(&crate::settings::Settings::default(), tmp.path());
        let args = serde_json::json!({ "command": "scan ." });
        assert!(rules.allow_match("ext__guard_acme__scan", &args).is_none());
        assert!(
            rules
                .allow_match("bash", &serde_json::json!({ "command": "cargo test" }))
                .is_some()
        );
    }

    /// With no load report at all, an ext__* approval is not persisted
    /// (it would be dropped at the next load anyway).
    #[test]
    fn ext_persist_without_load_report_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        persist_allow_always(
            tmp.path(),
            "ext__guard_acme__scan",
            &serde_json::json!({ "command": "scan ." }),
        );
        assert!(
            !tmp.path().join("permissions.json").exists(),
            "unknown-version ext approvals are not persisted"
        );
    }
}
