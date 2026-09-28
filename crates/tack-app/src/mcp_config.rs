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
    /// OAuth 2.1 for remote servers: {"clientId": "…", "scopes": ["…"]}.
    /// `true` means "authorize with dynamic registration on 401".
    #[serde(default)]
    oauth: Option<serde_json::Value>,
}

fn parse_oauth(value: Option<serde_json::Value>) -> Option<tack_tools::mcp::McpOAuthConfig> {
    match value {
        Some(serde_json::Value::Bool(true)) => Some(tack_tools::mcp::McpOAuthConfig::default()),
        Some(serde_json::Value::Object(map)) => Some(tack_tools::mcp::McpOAuthConfig {
            client_id: map
                .get("clientId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            scopes: map
                .get("scopes")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        }),
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
    let oauth = parse_oauth(entry.oauth);
    if let Some(url) = entry.url {
        let headers: Vec<(String, String)> = entry.headers.into_iter().collect();
        let spec = if entry.transport_type.as_deref() == Some("sse") {
            McpServerSpec::sse(name, url, headers)
        } else {
            McpServerSpec::http(name, url, headers)
        };
        Some(match oauth {
            Some(oauth) => spec.with_oauth(oauth),
            None => spec,
        })
    } else if let Some(command) = entry.command {
        Some(McpServerSpec::stdio(
            name,
            command,
            entry.args,
            entry.env.into_iter().collect(),
            None,
        ))
    } else {
        tracing::warn!("mcp.json entry {name:?} has neither command nor url; skipped");
        None
    }
}

/// Client callbacks for Level-2 MCP server plugins (connected at
/// extension-load time): elicitation follows the mode's usual rule (TUI
/// prompts, headless auto-declines). Sampling is NOT wired: the session
/// model does not exist yet at load time, so a plugin server's sampling
/// request gets method-not-found (documented Level-2 limitation).
pub fn plugin_mcp_callbacks(
    settings: &crate::settings::Settings,
    mode: crate::mcp_elicitation::InteractionMode,
    tui_events: Option<crate::tui::AppEventTx>,
) -> tack_tools::mcp::McpClientCallbacks {
    let mut callbacks = tack_tools::mcp::McpClientCallbacks::default();
    if let Some(handler) =
        crate::mcp_elicitation::elicitation_callback(mode, settings.mcp_elicitation, tui_events)
    {
        callbacks = callbacks.with_elicitation(handler);
    }
    callbacks
}

/// LLM access for MCP sampling: the session's current provider/model/auth.
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
/// - sampling (`mcpSampling`, default off) runs against the session model in
///   an isolated untrusted context; usage flows to `usage_sink`;
/// - elicitation (`mcpElicitation`, default on) prompts in the TUI and
///   auto-declines in headless modes.
pub fn client_callbacks(
    settings: &crate::settings::Settings,
    llm: Option<&SamplingLlm>,
    usage_sink: crate::mcp_sampling::SamplingUsageSink,
    mode: crate::mcp_elicitation::InteractionMode,
    tui_events: Option<crate::tui::AppEventTx>,
) -> tack_tools::mcp::McpClientCallbacks {
    let mut callbacks = tack_tools::mcp::McpClientCallbacks::default();
    if settings.mcp_sampling
        && let Some(llm) = llm
    {
        callbacks = callbacks.with_sampling(Arc::new(crate::mcp_sampling::SamplingExecutor::new(
            llm.provider.clone(),
            llm.model.clone(),
            llm.auth.clone(),
            usage_sink,
        )));
    }
    if let Some(handler) =
        crate::mcp_elicitation::elicitation_callback(mode, settings.mcp_elicitation, tui_events)
    {
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
}
