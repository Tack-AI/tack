//! MCP server configuration: `mcp.json` files (global agent dir + project
//! `.pi/`), plus conversion from ACP `session/new` `mcpServers`.
//!
//! Entry shapes (Claude Code compatible):
//! ```jsonc
//! { "mcpServers": {
//!     "local":  { "command": "npx", "args": ["-y", "server"], "env": {} },
//!     "remote": { "type": "http", "url": "http://localhost:3000/mcp", "headers": { "authorization": "Bearer …" } }
//! } }
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use tack_tools::mcp::McpServerSpec;

/// One server entry: stdio (command) or HTTP (url + type).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpServerEntry {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// "http" / "streamable-http" → Streamable HTTP; "sse" → legacy SSE
    /// transport (spec 2024-11-05). `url` presence still selects HTTP when
    /// no type is given.
    #[serde(default, rename = "type")]
    transport_type: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// OAuth 2.1 for remote servers: {"clientId": "…", "clientSecret": "…",
    /// "scopes": ["…"], "callbackPort": 8765, "callbackUrl": "…"}.
    /// `true` means "authorize with dynamic registration on 401".
    #[serde(default)]
    oauth: Option<serde_json::Value>,
    /// `false` keeps the entry without ever connecting to it.
    #[serde(default)]
    enabled: Option<bool>,
    /// Per-request timeout in seconds (default 60; `0` disables). Progress
    /// notifications reset the clock.
    #[serde(default)]
    timeout: Option<f64>,
    /// How the server's tools reach the model: "direct" (default),
    /// "deferred" (loaded on demand via tool_search), "hidden" (never).
    /// pi's "codemode"/"codemode-deferred" map to "deferred".
    #[serde(default)]
    exposure: Option<String>,
    /// Per-tool exposure overrides: exact server tool names or `*`
    /// patterns → exposure. Exact names win; among patterns the longest
    /// (most specific) wins.
    #[serde(default, rename = "toolExposure")]
    tool_exposure: std::collections::BTreeMap<String, String>,
}

/// Expand `${VAR}` references against the process environment (mcp.json
/// `env`/`headers`/`clientSecret` values — the interpolation syntax other
/// MCP clients share). Undefined variables expand to empty with a warning:
/// a literal `${VAR}` reaching the server would be a silent
/// misconfiguration. An unterminated `${` stays literal.
fn expand_env_vars(value: &str, name: &str, origin: &str) -> String {
    if !value.contains("${") {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) if !after[..end].is_empty() => {
                let var = &after[..end];
                match std::env::var(var) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        tracing::warn!(
                            "MCP server {name:?} in {origin}: environment variable {var:?} is not set; expanding to empty"
                        );
                    }
                }
                rest = &after[end + 1..];
            }
            // Unterminated or empty `${}`: keep the remainder literal.
            _ => {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

fn parse_oauth(
    value: Option<serde_json::Value>,
    name: &str,
    origin: &str,
) -> Option<tack_tools::mcp::McpOAuthConfig> {
    match value {
        Some(serde_json::Value::Bool(true)) => Some(tack_tools::mcp::McpOAuthConfig::default()),
        Some(serde_json::Value::Object(map)) => {
            let callback_port = match map.get("callbackPort").and_then(|v| v.as_u64()) {
                Some(port) if port <= u16::MAX as u64 => Some(port as u16),
                Some(port) => {
                    tracing::warn!(
                        "MCP server {name:?} in {origin}: callbackPort {port} out of range; ignoring"
                    );
                    None
                }
                None => None,
            };
            Some(tack_tools::mcp::McpOAuthConfig {
                client_id: map
                    .get("clientId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                client_secret: map
                    .get("clientSecret")
                    .and_then(|v| v.as_str())
                    .map(|s| expand_env_vars(s, name, origin)),
                scopes: map
                    .get("scopes")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                callback_port,
                callback_url: map
                    .get("callbackUrl")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            })
        }
        _ => None,
    }
}

fn load_mcp_json(path: &Path) -> Vec<McpServerSpec> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    // Parse loosely first: one malformed entry must not drop the whole file
    // (a single typo would otherwise silently disable every MCP server).
    let raw: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("ignoring malformed mcp.json {}: {e}", path.display());
            return Vec::new();
        }
    };
    specs_from_value(&raw, &path.display().to_string())
}

/// Parse an `{"mcpServers": {...}}` object (or a bare server map) into
/// specs. `origin` labels warnings (file path or extension name).
pub fn specs_from_value(raw: &serde_json::Value, origin: &str) -> Vec<McpServerSpec> {
    let servers = match raw.get("mcpServers").and_then(serde_json::Value::as_object) {
        Some(servers) => servers,
        None => match raw.as_object() {
            // Bare map form: {"server-name": {...}} (bundle manifests).
            Some(map) if raw.get("mcpServers").is_none() => map,
            _ => {
                tracing::warn!("mcpServers in {origin} is not an object; ignoring");
                return Vec::new();
            }
        },
    };
    servers
        .iter()
        .filter_map(|(name, value)| spec_from_entry(name, value, origin))
        .collect()
}

/// Parse ONE MCP server entry (`{"command": …} | {"url": …}`) into a
/// spec — the same shape as an `mcp.json` entry. Used for Level-2 MCP
/// server plugins (`extension.json` `mcpServer`), where the entry is not
/// part of a server map. Malformed entries warn and return None.
pub fn spec_from_entry(
    name: &str,
    value: &serde_json::Value,
    origin: &str,
) -> Option<McpServerSpec> {
    let entry: McpServerEntry = match serde_json::from_value(value.clone()) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("skipping malformed MCP server entry {name:?} in {origin}: {e}",);
            return None;
        }
    };
    let name = name.to_string();
    let oauth = parse_oauth(entry.oauth, &name, origin);
    let enabled = entry.enabled.unwrap_or(true);
    let exposure = entry
        .exposure
        .as_deref()
        .and_then(|e| parse_exposure(e, &name, origin));
    let tool_exposure: Vec<(String, tack_tools::mcp::McpExposure)> = entry
        .tool_exposure
        .iter()
        .filter_map(|(pattern, value)| {
            parse_exposure(value, &name, origin).map(|e| (pattern.clone(), e))
        })
        .collect();
    let timeout = match entry.timeout {
        Some(secs) if secs < 0.0 => {
            tracing::warn!("mcp.json entry {name:?} has a negative timeout; using the default");
            None
        }
        // 0 disables the limit (spec encodes that as Duration::ZERO).
        Some(secs) => Some(Some(std::time::Duration::from_secs_f64(secs))),
        None => None,
    };
    if let Some(url) = entry.url {
        let headers: Vec<(String, String)> = entry
            .headers
            .into_iter()
            .map(|(k, v)| (k, expand_env_vars(&v, &name, origin)))
            .collect();
        let spec = if entry.transport_type.as_deref() == Some("sse") {
            McpServerSpec::sse(name, url, headers)
        } else {
            McpServerSpec::http(name, url, headers)
        };
        let spec = match oauth {
            Some(oauth) => spec.with_oauth(oauth),
            None => spec,
        };
        Some(apply_entry_flags(
            spec,
            enabled,
            timeout,
            exposure,
            tool_exposure,
        ))
    } else if let Some(command) = entry.command {
        let env = entry
            .env
            .into_iter()
            .map(|(k, v)| (k, expand_env_vars(&v, &name, origin)))
            .collect();
        Some(apply_entry_flags(
            McpServerSpec::stdio(name, command, entry.args, env, None),
            enabled,
            timeout,
            exposure,
            tool_exposure,
        ))
    } else {
        tracing::warn!("mcp.json entry {name:?} has neither command nor url; skipped");
        None
    }
}

/// Apply the transport-independent entry flags (`enabled`, `timeout`,
/// `exposure`, `toolExposure`).
fn apply_entry_flags(
    spec: McpServerSpec,
    enabled: bool,
    timeout: Option<Option<std::time::Duration>>,
    exposure: Option<tack_tools::mcp::McpExposure>,
    tool_exposure: Vec<(String, tack_tools::mcp::McpExposure)>,
) -> McpServerSpec {
    let spec = spec.with_enabled(enabled);
    let spec = match timeout {
        Some(t) => spec.with_request_timeout(t),
        None => spec,
    };
    let spec = match exposure {
        Some(e) => spec.with_exposure(e),
        None => spec,
    };
    if !tool_exposure.is_empty() {
        return spec.with_tool_exposure(tool_exposure);
    }
    spec
}

/// Parse one exposure value (mcp.json `exposure` / `toolExposure` values).
/// Unknown values warn and fall back to None (entry default: `direct`).
fn parse_exposure(value: &str, name: &str, origin: &str) -> Option<tack_tools::mcp::McpExposure> {
    match tack_tools::mcp::McpExposure::from_config(value) {
        Some((exposure, codemode_alias)) => {
            if codemode_alias {
                tracing::warn!(
                    "MCP server {name:?} in {origin}: exposure {value:?} needs the codemode tool, which tack does not have; using \"deferred\" (tools reachable via tool_search)"
                );
            }
            Some(exposure)
        }
        None => {
            tracing::warn!(
                "MCP server {name:?} in {origin}: unknown exposure {value:?} (expected direct/deferred/hidden); using \"direct\""
            );
            None
        }
    }
}

/// Client callbacks for Level-2 MCP server plugins (connected at
/// extension-load time): elicitation follows the mode's usual rule (TUI
/// prompts, headless auto-declines). Sampling (`mcpSampling`, default off)
/// resolves the session model LATE through `llm` — plugin connections
/// outlive any session, so the executor reads the shared cell per request
/// (see `crate::mcp_sampling::SharedSamplingLlm`).
pub fn plugin_mcp_callbacks(
    settings: &crate::settings::Settings,
    mode: crate::mcp_elicitation::InteractionMode,
    elicitation_channel: Option<crate::mcp_elicitation::ElicitationChannel>,
    llm: &crate::mcp_sampling::SharedSamplingLlm,
    usage_sink: crate::mcp_sampling::SamplingUsageSink,
) -> tack_tools::mcp::McpClientCallbacks {
    let mut callbacks = tack_tools::mcp::McpClientCallbacks::default();
    if settings.mcp_sampling {
        callbacks = callbacks.with_sampling(Arc::new(
            crate::mcp_sampling::SamplingExecutor::shared(llm.clone(), usage_sink),
        ));
    }
    if let Some(handler) = crate::mcp_elicitation::elicitation_callback(
        mode,
        settings.mcp_elicitation,
        elicitation_channel,
    ) {
        callbacks = callbacks.with_elicitation(handler);
    }
    callbacks
}

/// LLM access for MCP sampling: the session's current provider/model/auth.
#[derive(Clone)]
pub struct SamplingLlm {
    pub provider: Arc<dyn tack_ai::Provider>,
    pub model: tack_ai::Model,
    pub auth: Arc<dyn tack_ai::oauth::AuthResolver>,
}

impl std::fmt::Debug for SamplingLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplingLlm")
            .field("model", &self.model.id)
            .finish()
    }
}

/// Assemble the client-side callbacks for MCP connections:
/// - sampling (`mcpSampling`, default off) runs against the session model
///   (resolved per request through the shared `llm` cell, so model changes
///   after connect are picked up) in an isolated untrusted context; usage
///   flows to `usage_sink`;
/// - elicitation (`mcpElicitation`, default on) prompts in the TUI,
///   crosses to dialog-capable clients in remote mode, and auto-declines
///   in headless modes.
pub fn client_callbacks(
    settings: &crate::settings::Settings,
    llm: &crate::mcp_sampling::SharedSamplingLlm,
    usage_sink: crate::mcp_sampling::SamplingUsageSink,
    mode: crate::mcp_elicitation::InteractionMode,
    elicitation_channel: Option<crate::mcp_elicitation::ElicitationChannel>,
) -> tack_tools::mcp::McpClientCallbacks {
    let mut callbacks = tack_tools::mcp::McpClientCallbacks::default();
    if settings.mcp_sampling {
        callbacks = callbacks.with_sampling(Arc::new(
            crate::mcp_sampling::SamplingExecutor::shared(llm.clone(), usage_sink),
        ));
    }
    if let Some(handler) = crate::mcp_elicitation::elicitation_callback(
        mode,
        settings.mcp_elicitation,
        elicitation_channel,
    ) {
        callbacks = callbacks.with_elicitation(handler);
    }
    callbacks
}

/// MCP servers from config files: global `~/.tack/agent/mcp.json` merged
/// with project `<cwd>/.pi/mcp.json` (project wins on name conflicts).
pub fn configured_servers(cwd: &Path, agent_dir: &Path) -> Vec<McpServerSpec> {
    let mut by_name: BTreeMap<String, McpServerSpec> = BTreeMap::new();
    for spec in load_mcp_json(&agent_dir.join("mcp.json")) {
        by_name.insert(spec.name.clone(), spec);
    }
    // Project MCP servers can execute arbitrary commands — trust-gated.
    if crate::project_trust::is_trusted(cwd, agent_dir) {
        for spec in load_mcp_json(&cwd.join(".pi").join("mcp.json")) {
            by_name.insert(spec.name.clone(), spec);
        }
    }
    by_name.into_values().collect()
}

/// Merge session-scoped specs (RPC `set_mcp_servers`, delivered by a trusted
/// host at session create) over file-configured ones: on a name conflict the
/// session entry wins — it is the host's explicit per-session override.
pub fn merge_session_servers(
    configured: Vec<McpServerSpec>,
    session: Vec<McpServerSpec>,
) -> Vec<McpServerSpec> {
    if session.is_empty() {
        return configured;
    }
    let mut by_name: BTreeMap<String, McpServerSpec> = configured
        .into_iter()
        .map(|spec| (spec.name.clone(), spec))
        .collect();
    for spec in session {
        by_name.insert(spec.name.clone(), spec);
    }
    by_name.into_values().collect()
}

/// Convert ACP `session/new` mcpServers to specs. Stdio and HTTP are
/// supported; legacy SSE entries use the 2024-11-05 SSE transport.
pub fn specs_from_acp(servers: &[agent_client_protocol::McpServer]) -> Vec<McpServerSpec> {
    let mut specs = Vec::new();
    for server in servers {
        match server {
            agent_client_protocol::McpServer::Stdio(stdio) => {
                specs.push(McpServerSpec::stdio(
                    stdio.name.clone(),
                    stdio.command.to_string_lossy().to_string(),
                    stdio.args.clone(),
                    stdio
                        .env
                        .iter()
                        .map(|e| (e.name.clone(), e.value.clone()))
                        .collect(),
                    None,
                ));
            }
            agent_client_protocol::McpServer::Http(http) => {
                specs.push(McpServerSpec::http(
                    http.name.clone(),
                    http.url.clone(),
                    http.headers
                        .iter()
                        .map(|e| (e.name.clone(), e.value.clone()))
                        .collect(),
                ));
            }
            agent_client_protocol::McpServer::Sse(sse) => {
                specs.push(McpServerSpec::sse(
                    sse.name.clone(),
                    sse.url.clone(),
                    sse.headers
                        .iter()
                        .map(|e| (e.name.clone(), e.value.clone()))
                        .collect(),
                ));
            }
            other => {
                tracing::warn!(
                    "skipping unknown MCP server entry: {:?}",
                    std::mem::discriminant(other)
                );
            }
        }
    }
    specs
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(unsafe_code)] // std::env::set_var in tests (unique var names)
    use super::*;

    #[test]
    fn parses_stdio_and_http_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{
  "mcpServers": {
    "local": { "command": "npx", "args": ["-y", "srv"], "env": { "A": "1" } },
    "remote": { "type": "http", "url": "http://localhost:9999/mcp", "headers": { "authorization": "Bearer x" } }
  }
}"#,
        )
        .unwrap();
        let specs = load_mcp_json(&dir.path().join("mcp.json"));
        assert_eq!(specs.len(), 2);
        let remote = specs.iter().find(|s| s.name == "remote").unwrap();
        let tack_tools::mcp::McpTransport::Http { url, headers } = &remote.transport else {
            panic!("expected http transport");
        };
        assert_eq!(url, "http://localhost:9999/mcp");
        assert_eq!(headers[0].0, "authorization");
    }

    /// One malformed entry must not drop the whole file: valid servers
    /// still load (previously serde rejected the entire mcp.json).
    #[test]
    fn malformed_entry_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{
  "mcpServers": {
    "good": { "command": "npx", "args": ["-y", "srv"] },
    "bad": { "command": "npx", "args": "not-an-array" }
  }
}"#,
        )
        .unwrap();
        let specs = load_mcp_json(&dir.path().join("mcp.json"));
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "good");
    }

    #[test]
    fn session_specs_override_configured_by_name() {
        let configured = vec![
            McpServerSpec::stdio("a".into(), "cmd-a".into(), vec![], vec![], None),
            McpServerSpec::stdio("b".into(), "cmd-b".into(), vec![], vec![], None),
        ];
        let session = vec![McpServerSpec::stdio(
            "b".into(),
            "cmd-session".into(),
            vec![],
            vec![],
            None,
        )];
        let merged = merge_session_servers(configured, session);
        assert_eq!(merged.len(), 2);
        let b = merged.iter().find(|s| s.name == "b").unwrap();
        let tack_tools::mcp::McpTransport::Stdio { command, .. } = &b.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(
            command, "cmd-session",
            "session spec must win on name conflict"
        );
        // No session specs: configured pass through unchanged.
        let configured = vec![McpServerSpec::stdio(
            "a".into(),
            "cmd-a".into(),
            vec![],
            vec![],
            None,
        )];
        let merged = merge_session_servers(configured, Vec::new());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "a");
    }

    #[test]
    fn enabled_timeout_and_expansion_are_parsed() {
        // Unique var name: env mutation is process-global, tests run in
        // parallel.
        unsafe { std::env::set_var("TACK_TEST_MCP_EXPANSION", "expanded!") };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{
  "mcpServers": {
    "off": { "command": "npx", "enabled": false },
    "slow": { "command": "npx", "timeout": 120 },
    "unlimited": { "command": "npx", "timeout": 0 },
    "expanded": { "type": "http", "url": "http://localhost:1/mcp",
      "headers": { "authorization": "Bearer ${TACK_TEST_MCP_EXPANSION}" } }
  }
}"#,
        )
        .unwrap();
        let specs = load_mcp_json(&dir.path().join("mcp.json"));
        assert_eq!(specs.len(), 4);

        let off = specs.iter().find(|s| s.name == "off").unwrap();
        assert!(!off.enabled, "enabled: false must parse");
        assert!(
            specs.iter().all(|s| s.name == "off" || s.enabled),
            "enabled defaults to true"
        );

        let slow = specs.iter().find(|s| s.name == "slow").unwrap();
        assert_eq!(
            slow.effective_request_timeout(),
            Some(std::time::Duration::from_secs(120))
        );
        let unlimited = specs.iter().find(|s| s.name == "unlimited").unwrap();
        assert_eq!(
            unlimited.effective_request_timeout(),
            None,
            "timeout 0 disables the limit"
        );
        let default_timeout = specs.iter().find(|s| s.name == "off").unwrap();
        assert_eq!(
            default_timeout.effective_request_timeout(),
            Some(tack_tools::mcp::DEFAULT_REQUEST_TIMEOUT),
            "absent timeout uses the 60s default"
        );

        let expanded = specs.iter().find(|s| s.name == "expanded").unwrap();
        let tack_tools::mcp::McpTransport::Http { headers, .. } = &expanded.transport else {
            panic!("expected http transport");
        };
        assert_eq!(
            headers[0].1, "Bearer expanded!",
            "${{VAR}} expands against the process env"
        );
    }

    #[test]
    fn oauth_extended_fields_are_parsed() {
        unsafe { std::env::set_var("TACK_TEST_MCP_SECRET", "s3cret") };
        let value = serde_json::json!({
            "mcpServers": {
                "sentry": {
                    "url": "https://mcp.sentry.dev/mcp",
                    "oauth": {
                        "clientId": "my-client",
                        "clientSecret": "${TACK_TEST_MCP_SECRET}",
                        "scopes": ["read", "write"],
                        "callbackPort": 8765,
                        "callbackUrl": "http://127.0.0.1:8765/callback"
                    }
                }
            }
        });
        let specs = specs_from_value(&value, "test");
        let oauth = specs[0].oauth.as_ref().expect("oauth config");
        assert_eq!(oauth.client_id.as_deref(), Some("my-client"));
        assert_eq!(
            oauth.client_secret.as_deref(),
            Some("s3cret"),
            "clientSecret gets env expansion too"
        );
        assert_eq!(oauth.scopes, ["read", "write"]);
        assert_eq!(oauth.callback_port, Some(8765));
        assert_eq!(
            oauth.callback_url.as_deref(),
            Some("http://127.0.0.1:8765/callback")
        );

        // Out-of-range callbackPort is dropped, not wrapped.
        let value = serde_json::json!({
            "mcpServers": {
                "bad": { "url": "https://x/mcp", "oauth": { "callbackPort": 70000 } }
            }
        });
        let specs = specs_from_value(&value, "test");
        assert_eq!(specs[0].oauth.as_ref().unwrap().callback_port, None);
    }

    #[test]
    fn exposure_and_tool_exposure_are_parsed() {
        let value = serde_json::json!({
            "mcpServers": {
                "github": {
                    "url": "https://api.githubcopilot.com/mcp/",
                    "exposure": "deferred",
                    "toolExposure": {
                        "search_code": "direct",
                        "get_*": "deferred",
                        "delete_*": "hidden"
                    }
                },
                "off": { "command": "npx", "exposure": "hidden" },
                "legacy": { "command": "npx", "exposure": "codemode" }
            }
        });
        let specs = specs_from_value(&value, "test");
        let github = specs.iter().find(|s| s.name == "github").unwrap();
        assert_eq!(github.exposure, tack_tools::mcp::McpExposure::Deferred);
        use tack_tools::mcp::McpExposure;
        assert_eq!(
            github.effective_exposure("search_code"),
            McpExposure::Direct,
            "exact toolExposure key wins over server exposure"
        );
        assert_eq!(github.effective_exposure("get_pr"), McpExposure::Deferred);
        assert_eq!(
            github.effective_exposure("delete_repo"),
            McpExposure::Hidden
        );
        assert_eq!(github.effective_exposure("anything"), McpExposure::Deferred);

        let off = specs.iter().find(|s| s.name == "off").unwrap();
        assert_eq!(off.exposure, McpExposure::Hidden);
        let legacy = specs.iter().find(|s| s.name == "legacy").unwrap();
        assert_eq!(
            legacy.exposure,
            McpExposure::Deferred,
            "pi's codemode maps to deferred"
        );
    }

    /// MCP sampling is opt-in (`mcpSampling`, default off) for BOTH
    /// config-file and Level-2 plugin connections: no capability is
    /// advertised unless the setting is on.
    #[test]
    #[allow(deprecated)]
    fn sampling_follows_setting_for_both_connection_kinds() {
        let cell = crate::mcp_sampling::SharedSamplingLlm::default();
        let sink = crate::mcp_sampling::log_usage_sink();
        let mode = crate::mcp_elicitation::InteractionMode::Headless;
        let mut settings = crate::settings::Settings::default();
        assert!(!settings.mcp_sampling, "default must be off");

        let client = client_callbacks(&settings, &cell, sink.clone(), mode, None);
        assert!(client.sampling.is_none());
        let plugin = plugin_mcp_callbacks(&settings, mode, None, &cell, sink.clone());
        assert!(plugin.sampling.is_none());

        settings.mcp_sampling = true;
        let client = client_callbacks(&settings, &cell, sink.clone(), mode, None);
        assert!(client.sampling.is_some());
        let plugin = plugin_mcp_callbacks(&settings, mode, None, &cell, sink);
        assert!(plugin.sampling.is_some());
    }
}
