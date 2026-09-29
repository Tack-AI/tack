//! Enterprise plugin policy: the managed settings layer's `pluginPolicy`
//! key (docs/plugin-roadmap.md §8.3). The policy is enforced **twice**:
//!
//! 1. At add/install time — the source allow-list is checked before any
//!    clone or network access, and the per-plugin rules again after the
//!    manifest is parsed but before activation.
//! 2. At load time — the discovered plugin set is filtered before the
//!    loader starts carriers (the backstop that keeps every downstream
//!    consumer compliant by construction), and per-plugin
//!    `tools`/`mcpServers` narrow (intersect-only) what a plugin may
//!    register. A managed `enabled` wins over the user/project layers.
//!
//! Every decision is audit-logged as a structured tracing event (target
//! `plugin_policy`) naming the rule and its origin layer; with a managed
//! `auditSink` configured those events are shipped to the org collector.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tack_ext::plugin_id::PluginId;

/// One `pluginPolicy.allowedSources` entry.
#[derive(Clone, Debug)]
pub enum AllowedSource {
    /// `{ "type": "git", "url": ..., "ref"? }` — exact URL match. A rule
    /// carrying `ref` only allows installs pinned to exactly that ref
    /// (an unpinned install clones the default branch, which the rule
    /// cannot vouch for).
    Git {
        url: String,
        git_ref: Option<String>,
    },
    /// `{ "type": "hostPattern", "pattern": ... }` — regex matched
    /// against the source URL's host (https and scp-style `git@host:`).
    HostPattern { regex: regex::Regex },
    /// `{ "type": "local", "path": ... }` — the source (or plugin)
    /// directory must be the rule path or live beneath it.
    Local { path: PathBuf },
}

impl AllowedSource {
    fn matches_git(&self, source: &str, rev: Option<&str>) -> bool {
        match self {
            AllowedSource::Git { url, git_ref } => {
                url == source
                    && git_ref
                        .as_deref()
                        .is_none_or(|pinned| pinned == rev.unwrap_or(""))
            }
            AllowedSource::HostPattern { regex } => {
                url_host(source).is_some_and(|host| regex.is_match(&host))
            }
            AllowedSource::Local { .. } => false,
        }
    }

    fn matches_path(&self, dir: &Path) -> bool {
        let AllowedSource::Local { path } = self else {
            return false;
        };
        // Canonicalize both sides when possible so `..` and symlinks
        // cannot escape the rule root; fall back to the lexical path
        // (a not-yet-existing source is still compared lexically).
        let rule = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        let target = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        target.starts_with(&rule)
    }

    fn describe(&self) -> String {
        match self {
            AllowedSource::Git { url, git_ref } => match git_ref {
                Some(git_ref) => format!("git {url}#{git_ref}"),
                None => format!("git {url}"),
            },
            AllowedSource::HostPattern { regex } => format!("hostPattern {}", regex.as_str()),
            AllowedSource::Local { path } => format!("local {}", path.display()),
        }
    }
}

/// Extract the host from an https/http/ssh or scp-style git source.
/// Userinfo (`https://oauth2:token@host/…`, `ssh://git@host/…`) and a
/// port suffix are stripped first; the host is the authority component
/// after the LAST `@` (WHATWG URL rule — `https://a.com@evil.com/x`
/// yields `evil.com`, never `a.com`).
fn url_host(source: &str) -> Option<String> {
    if let Some(rest) = source.strip_prefix("git@") {
        let (host, _) = rest.split_once(':')?;
        return (!host.is_empty()).then(|| host.to_string());
    }
    let after_scheme = source.split_once("://").map(|(_, rest)| rest)?;
    let authority = after_scheme.split('/').next().filter(|h| !h.is_empty())?;
    let after_userinfo = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let host = after_userinfo.split(':').next().filter(|h| !h.is_empty())?;
    Some(host.to_string())
}

/// The git-source heuristic, shared with the install path: http(s) URLs,
/// scp-style `git@`, and `*.git` paths clone through git.
fn looks_like_git_source(source: &str) -> bool {
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.ends_with(".git")
}

/// Per-plugin managed rules (`pluginPolicy.plugins."<id>"`). `tools` and
/// `mcpServers` are narrow-only: they intersect with what the plugin
/// registers and can never expand it.
#[derive(Clone, Debug, Default)]
pub struct PluginPolicyEntry {
    pub enabled: Option<bool>,
    pub tools: Option<Vec<String>>,
    pub mcp_servers: Option<Vec<String>>,
}

/// The parsed managed `pluginPolicy`. Absent managed file or absent key
/// ⇒ `None` (no policy, everything permitted).
#[derive(Clone, Debug, Default)]
pub struct PluginPolicy {
    /// Only plugins explicitly named in `plugins` may load.
    pub managed_plugins_only: bool,
    /// Source allow-list; empty means no source restriction.
    pub allowed_sources: Vec<AllowedSource>,
    /// Per-plugin rules keyed by `name@source`.
    pub plugins: HashMap<String, PluginPolicyEntry>,
    /// Origin label for denials/audit (the managed settings file path).
    pub origin: String,
}

/// A policy denial. The message names the rule and the layer it came
/// from (roadmap: "denials name the rule and its origin layer").
#[derive(Debug)]
pub struct PolicyDenial {
    pub rule: String,
    pub message: String,
}

impl std::fmt::Display for PolicyDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PolicyDenial {}

/// Audit-log one policy decision (structured fields; the managed audit
/// sink ships these when configured).
fn audit(decision: &str, rule: &str, layer: &str, subject: &str, detail: &str) {
    tracing::info!(
        target: "plugin_policy",
        decision,
        rule,
        layer,
        subject,
        detail,
        "plugin policy decision"
    );
}

/// Where a discovered plugin's bits came from, for the load-time source
/// check: the lockfile's recorded source (store installs) or the plugin
/// directory itself (legacy user/project/extensionPaths checkouts).
#[derive(Debug)]
pub enum LoadOrigin<'a> {
    Locked {
        source: &'a str,
        rev: Option<&'a str>,
    },
    Dir(&'a Path),
}

impl PluginPolicy {
    /// Read `pluginPolicy` from the managed settings file (highest
    /// authority layer; user/project layers cannot set policy).
    pub fn load() -> Option<PluginPolicy> {
        let path = crate::settings::managed_settings_path();
        let content = std::fs::read_to_string(&path).ok()?;
        let raw = match serde_json::from_str::<Value>(&content) {
            Ok(raw) => raw,
            Err(e) => {
                // A managed control plane that cannot be parsed must
                // never fail SILENTLY: with no warning, a truncated or
                // corrupt deploy looks identical to "no policy" while
                // every restriction is in fact inactive.
                tracing::warn!(
                    "managed settings {} is not valid JSON ({e}); managed plugin policy is \
                     INACTIVE (all plugins permitted) until the file is fixed",
                    path.display()
                );
                return None;
            }
        };
        PluginPolicy::from_raw(&raw, path.display().to_string())
    }

    /// Parse the `pluginPolicy` key of one settings document. Malformed
    /// rules are skipped with a warning (consistent with the settings
    /// loader's treatment of malformed files); a wholly non-object
    /// `pluginPolicy` is ignored.
    pub fn from_raw(raw: &Value, origin: String) -> Option<PluginPolicy> {
        let policy_raw = raw.get("pluginPolicy")?;
        let Some(object) = policy_raw.as_object() else {
            tracing::warn!("managed settings: `pluginPolicy` is not an object; ignoring it");
            return None;
        };
        let mut policy = PluginPolicy {
            managed_plugins_only: policy_raw
                .get("managedPluginsOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            origin,
            ..PluginPolicy::default()
        };
        if let Some(sources) = object.get("allowedSources").and_then(Value::as_array) {
            for source in sources {
                match parse_allowed_source(source) {
                    Ok(rule) => policy.allowed_sources.push(rule),
                    Err(e) => {
                        tracing::warn!("managed settings: skipping bad allowedSources entry: {e}")
                    }
                }
            }
        }
        if let Some(plugins) = object.get("plugins").and_then(Value::as_object) {
            for (id, entry) in plugins {
                if id.parse::<PluginId>().is_err() {
                    tracing::warn!(
                        "managed settings: skipping pluginPolicy entry with invalid id {id:?}"
                    );
                    continue;
                }
                policy.plugins.insert(id.clone(), parse_policy_entry(entry));
            }
        }
        Some(policy)
    }

    /// No source restriction when the allow-list is absent/empty.
    fn source_list_open(&self) -> bool {
        self.allowed_sources.is_empty()
    }

    fn deny(&self, rule: &str, subject: &str, detail: String) -> PolicyDenial {
        audit("deny", rule, &self.origin, subject, &detail);
        PolicyDenial {
            rule: rule.to_string(),
            message: format!(
                "blocked by managed plugin policy ({}): {detail}",
                self.origin
            ),
        }
    }

    fn source_allowed(&self, source: &str, rev: Option<&str>) -> bool {
        if looks_like_git_source(source) {
            self.allowed_sources
                .iter()
                .any(|rule| rule.matches_git(source, rev))
        } else {
            self.allowed_sources
                .iter()
                .any(|rule| rule.matches_path(Path::new(source)))
        }
    }

    /// Install-time source check, run BEFORE any clone or network
    /// access. `rev` is the requested `<url>#<ref>` pin, if any.
    pub fn check_install_source(
        &self,
        source: &str,
        rev: Option<&str>,
    ) -> Result<(), PolicyDenial> {
        if self.source_list_open() {
            return Ok(());
        }
        if self.source_allowed(source, rev) {
            audit(
                "allow",
                "allowedSources",
                &self.origin,
                source,
                "install source matches the allow-list",
            );
            return Ok(());
        }
        let rules = self
            .allowed_sources
            .iter()
            .map(AllowedSource::describe)
            .collect::<Vec<_>>()
            .join(", ");
        Err(self.deny(
            "allowedSources",
            source,
            format!("source {source:?} matches no allowedSources rule: {rules}"),
        ))
    }

    /// Install-time per-plugin check, run after the manifest is parsed
    /// (the id is known) but before activation: `managedPluginsOnly`
    /// requires an explicit entry, and a managed `enabled: false`
    /// forbids the install outright.
    pub fn check_install_allowed(&self, id: &PluginId) -> Result<(), PolicyDenial> {
        let id_string = id.to_string();
        if self.managed_plugins_only && !self.plugins.contains_key(&id_string) {
            return Err(self.deny(
                "managedPluginsOnly",
                &id_string,
                format!("plugin {id_string} is not in the managed plugin allow-list"),
            ));
        }
        if self.plugins.get(&id_string).and_then(|e| e.enabled) == Some(false) {
            return Err(self.deny(
                "plugins.enabled",
                &id_string,
                format!("plugin {id_string} is disabled by managed policy"),
            ));
        }
        Ok(())
    }

    /// Load-time filter (the backstop): returns the block reason when
    /// the plugin must not load. Checks `managedPluginsOnly` membership
    /// and re-verifies the origin against `allowedSources` so a
    /// hand-edited store/lockfile cannot smuggle in a non-allow-listed
    /// source after the fact.
    pub fn load_block(&self, id: &PluginId, origin: LoadOrigin<'_>) -> Option<String> {
        let id_string = id.to_string();
        if self.managed_plugins_only && !self.plugins.contains_key(&id_string) {
            let reason = format!(
                "not in the managed plugin allow-list (managedPluginsOnly, {})",
                self.origin
            );
            audit(
                "filter",
                "managedPluginsOnly",
                &self.origin,
                &id_string,
                &reason,
            );
            return Some(reason);
        }
        if !self.source_list_open() {
            let allowed = match origin {
                LoadOrigin::Locked { source, rev } => self.source_allowed(source, rev),
                LoadOrigin::Dir(dir) => self
                    .allowed_sources
                    .iter()
                    .any(|rule| rule.matches_path(dir)),
            };
            if !allowed {
                let reason = format!(
                    "origin matches no allowedSources rule (managed policy, {})",
                    self.origin
                );
                audit(
                    "filter",
                    "allowedSources",
                    &self.origin,
                    &id_string,
                    &reason,
                );
                return Some(reason);
            }
        }
        None
    }

    /// The managed `enabled` override for one plugin (wins over the
    /// user/project layers in both directions).
    pub fn managed_enabled(&self, id: &str) -> Option<bool> {
        self.plugins.get(id).and_then(|entry| entry.enabled)
    }

    /// The managed tool allow-list for one plugin (narrow-only
    /// intersection with the registered set).
    pub fn narrowed_tools(&self, id: &str) -> Option<&[String]> {
        self.plugins.get(id)?.tools.as_deref()
    }

    /// The managed bundle MCP-server allow-list for one plugin
    /// (narrow-only intersection with the declared servers).
    pub fn narrowed_mcp_servers(&self, id: &str) -> Option<&[String]> {
        self.plugins.get(id)?.mcp_servers.as_deref()
    }

    /// Audit a narrow decision (called by the loader after intersecting).
    pub fn audit_narrow(&self, id: &str, kind: &str, dropped: &[String]) {
        if dropped.is_empty() {
            return;
        }
        audit(
            "narrow",
            kind,
            &self.origin,
            id,
            &format!("dropped by managed policy: {}", dropped.join(", ")),
        );
    }

    /// Audit a managed `enabled` override of the user/project layers.
    pub fn audit_enabled_override(&self, id: &str, enabled: bool) {
        audit(
            "override",
            "plugins.enabled",
            &self.origin,
            id,
            &format!("managed policy forces enabled={enabled}"),
        );
    }
}

fn parse_allowed_source(raw: &Value) -> Result<AllowedSource, String> {
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `type`".to_string())?;
    match kind {
        "git" => {
            let url = raw
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| "git rule missing `url`".to_string())?;
            Ok(AllowedSource::Git {
                url: url.to_string(),
                git_ref: raw.get("ref").and_then(Value::as_str).map(str::to_string),
            })
        }
        "hostPattern" => {
            let pattern = raw
                .get("pattern")
                .and_then(Value::as_str)
                .ok_or_else(|| "hostPattern rule missing `pattern`".to_string())?;
            let regex = regex::Regex::new(pattern)
                .map_err(|e| format!("hostPattern {pattern:?} does not compile: {e}"))?;
            Ok(AllowedSource::HostPattern { regex })
        }
        "local" => {
            let path = raw
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "local rule missing `path`".to_string())?;
            Ok(AllowedSource::Local {
                path: PathBuf::from(path),
            })
        }
        other => Err(format!("unknown allowedSources type {other:?}")),
    }
}

fn parse_policy_entry(raw: &Value) -> PluginPolicyEntry {
    fn string_list(raw: &Value, key: &str) -> Option<Vec<String>> {
        raw.get(key).and_then(Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
    }
    PluginPolicyEntry {
        enabled: raw.get("enabled").and_then(Value::as_bool),
        tools: string_list(raw, "tools"),
        mcp_servers: string_list(raw, "mcpServers"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn policy(raw: &str) -> PluginPolicy {
        let raw: Value = serde_json::from_str(raw).unwrap();
        PluginPolicy::from_raw(&raw, "/etc/tack/managed-settings.json".to_string())
            .expect("policy present")
    }

    #[test]
    fn absent_key_means_no_policy() {
        let raw = serde_json::json!({"features": {}});
        assert!(PluginPolicy::from_raw(&raw, "origin".to_string()).is_none());
        let non_object = serde_json::json!({"pluginPolicy": 42});
        assert!(PluginPolicy::from_raw(&non_object, "origin".to_string()).is_none());
    }

    #[test]
    fn parses_all_rule_types() {
        let policy = policy(
            r#"{"pluginPolicy": {
                "managedPluginsOnly": true,
                "allowedSources": [
                    {"type": "git", "url": "https://git.acme.com/tack/plugins.git", "ref": "main"},
                    {"type": "hostPattern", "pattern": "^(.+\\.)?acme\\.com$"},
                    {"type": "local", "path": "/opt/acme/tack-ext"},
                    {"type": "unknown"},
                    {"type": "hostPattern", "pattern": "("}
                ],
                "plugins": {
                    "review@acme": {"enabled": true, "tools": ["create_ticket"], "mcpServers": ["jira"]},
                    "bad id": {"enabled": true},
                    "off@acme": {"enabled": false}
                }
            }}"#,
        );
        assert!(policy.managed_plugins_only);
        assert_eq!(policy.allowed_sources.len(), 3, "bad rules are skipped");
        assert_eq!(policy.plugins.len(), 2, "invalid ids are skipped");
        let entry = &policy.plugins["review@acme"];
        assert_eq!(entry.enabled, Some(true));
        assert_eq!(entry.tools.as_deref().unwrap(), &["create_ticket"]);
        assert_eq!(entry.mcp_servers.as_deref().unwrap(), &["jira"]);
        assert_eq!(policy.managed_enabled("off@acme"), Some(false));
    }

    #[test]
    fn git_rules_match_url_and_ref() {
        let policy = policy(
            r#"{"pluginPolicy": {"allowedSources": [
                {"type": "git", "url": "https://git.acme.com/plugins.git", "ref": "main"},
                {"type": "git", "url": "https://git.acme.com/any.git"}
            ]}}"#,
        );
        // Exact URL + matching pin.
        assert!(
            policy
                .check_install_source("https://git.acme.com/plugins.git", Some("main"))
                .is_ok()
        );
        // A pinned rule rejects other refs and unpinned installs.
        for rev in [Some("dev"), None, Some("maintenance")] {
            assert!(
                policy
                    .check_install_source("https://git.acme.com/plugins.git", rev)
                    .is_err(),
                "rev {rev:?} must be denied"
            );
        }
        // An unpinned rule allows any ref.
        assert!(
            policy
                .check_install_source("https://git.acme.com/any.git", Some("v2"))
                .is_ok()
        );
        // Unknown URL: denial names the rule and layer.
        let err = policy
            .check_install_source("https://evil.example.com/x.git", None)
            .unwrap_err();
        assert!(err.message.contains("/etc/tack/managed-settings.json"));
        assert!(
            err.message
                .contains("git https://git.acme.com/plugins.git#main")
        );
    }

    #[test]
    fn host_pattern_matches_https_and_scp_style() {
        let policy = policy(
            r#"{"pluginPolicy": {"allowedSources": [
                {"type": "hostPattern", "pattern": "^(.+\\.)?acme\\.com$"}
            ]}}"#,
        );
        for ok in [
            "https://git.acme.com/tack/plugins.git",
            "https://acme.com/x.git",
            "git@git.acme.com:tack/plugins.git",
            // Credential-embedded URL (common for private hosting tokens):
            // the userinfo must not hide the real host.
            "https://oauth2:token@git.acme.com/tack/plugins.git",
            "ssh://git@git.acme.com/tack/plugins.git",
            "https://git.acme.com:8443/tack/plugins.git",
        ] {
            assert!(policy.check_install_source(ok, None).is_ok(), "{ok}");
        }
        for denied in [
            "https://acme.com.evil.net/x.git",
            "https://github.com/acme/plugins.git",
            // Userinfo must never smuggle a fake host past the pattern:
            // the authority host is after the LAST '@'.
            "https://git.acme.com@evil.net/x.git",
            "https://oauth2:git.acme.com@evil.net/x.git",
        ] {
            assert!(
                policy.check_install_source(denied, None).is_err(),
                "{denied}"
            );
        }
    }

    #[test]
    fn url_host_strips_userinfo_and_port() {
        assert_eq!(
            url_host("https://oauth2:token@git.acme.com/repo.git").as_deref(),
            Some("git.acme.com")
        );
        assert_eq!(
            url_host("ssh://git@git.acme.com:2222/repo.git").as_deref(),
            Some("git.acme.com")
        );
        assert_eq!(
            url_host("https://git.acme.com@evil.net/x").as_deref(),
            Some("evil.net")
        );
    }

    #[test]
    fn local_rules_match_beneath_the_rule_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("approved");
        let nested = root.join("team").join("plugin");
        std::fs::create_dir_all(&nested).unwrap();
        let policy = policy(&format!(
            r#"{{"pluginPolicy": {{"allowedSources": [{{"type": "local", "path": "{}"}}]}}}}"#,
            root.display()
        ));
        assert!(
            policy
                .check_install_source(nested.to_str().unwrap(), None)
                .is_ok()
        );
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        assert!(
            policy
                .check_install_source(outside.to_str().unwrap(), None)
                .is_err()
        );
        // A git source never matches a local rule.
        assert!(
            policy
                .check_install_source("https://x.com/a.git", None)
                .is_err()
        );
    }

    #[test]
    fn managed_plugins_only_filters_unlisted_plugins() {
        let policy = policy(
            r#"{"pluginPolicy": {"managedPluginsOnly": true,
                "plugins": {"review@acme": {"enabled": true}}}}"#,
        );
        let listed: PluginId = "review@acme".parse().unwrap();
        let unlisted: PluginId = "sketchy@user".parse().unwrap();
        assert!(
            policy
                .load_block(&listed, LoadOrigin::Dir(Path::new("/x")))
                .is_none()
        );
        let reason = policy
            .load_block(&unlisted, LoadOrigin::Dir(Path::new("/x")))
            .expect("unlisted must be filtered");
        assert!(reason.contains("managedPluginsOnly"));
        // Install-time the same rule denies before activation.
        assert!(policy.check_install_allowed(&listed).is_ok());
        let err = policy.check_install_allowed(&unlisted).unwrap_err();
        assert_eq!(err.rule, "managedPluginsOnly");
    }

    #[test]
    fn managed_disabled_blocks_install_and_wins_at_load() {
        let policy = policy(r#"{"pluginPolicy": {"plugins": {"off@acme": {"enabled": false}}}}"#);
        let id: PluginId = "off@acme".parse().unwrap();
        let err = policy.check_install_allowed(&id).unwrap_err();
        assert!(err.message.contains("disabled by managed policy"));
        assert_eq!(policy.managed_enabled("off@acme"), Some(false));
        // Not a load-time block though: the plugin stays listed as
        // disabled (rows, not absences).
        assert!(
            policy
                .load_block(&id, LoadOrigin::Dir(Path::new("/x")))
                .is_none()
        );
    }

    #[test]
    fn load_time_source_backstop_uses_lock_origin() {
        let policy = policy(
            r#"{"pluginPolicy": {"allowedSources": [
                {"type": "hostPattern", "pattern": "^git\\.acme\\.com$"}
            ]}}"#,
        );
        let id: PluginId = "review@acme".parse().unwrap();
        assert!(
            policy
                .load_block(
                    &id,
                    LoadOrigin::Locked {
                        source: "https://git.acme.com/plugins.git",
                        rev: None,
                    },
                )
                .is_none()
        );
        // A lockfile edited to point at an unapproved host is caught.
        assert!(
            policy
                .load_block(
                    &id,
                    LoadOrigin::Locked {
                        source: "https://evil.example.com/plugins.git",
                        rev: None,
                    },
                )
                .is_some()
        );
        // No source list at all ⇒ no backstop.
        let open = PluginPolicy::default();
        assert!(
            open.load_block(
                &id,
                LoadOrigin::Locked {
                    source: "https://anything.example.com/x.git",
                    rev: None,
                },
            )
            .is_none()
        );
    }

    #[test]
    fn narrow_lists_are_exposed_for_intersection() {
        let policy = policy(
            r#"{"pluginPolicy": {"plugins": {
                "review@acme": {"tools": ["a", "b"], "mcpServers": ["jira"]}
            }}}"#,
        );
        assert_eq!(policy.narrowed_tools("review@acme").unwrap(), &["a", "b"]);
        assert_eq!(
            policy.narrowed_mcp_servers("review@acme").unwrap(),
            &["jira"]
        );
        assert!(policy.narrowed_tools("other@user").is_none());
    }
}
