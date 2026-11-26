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
                auth_server_metadata_url: map
                    .get("authServerMetadataUrl")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            })
        }
        _ => None,
    }
}

/// Provider credentials may come only from a user-controlled global file or
/// an explicitly installed extension. Session/RPC input uses `Reject` so a
/// remote host cannot redirect a provider token to its own URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderAuthPolicy {
    Allow,
    Reject,
}

fn parse_provider_auth(
    raw: Option<&serde_json::Value>,
    transport_type: Option<&str>,
    url: Option<&str>,
    policy: ProviderAuthPolicy,
) -> Result<Option<String>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if policy == ProviderAuthPolicy::Reject {
        return Err(
            "auth.provider is only allowed in the global mcp.json and extensions".to_string(),
        );
    }
    if transport_type == Some("sse") {
        return Err("auth.provider requires Streamable HTTP; legacy SSE is not supported".into());
    }
    let Some(url) = url else {
        return Err("auth.provider is only valid for HTTP servers".into());
    };
    let provider = raw
        .as_object()
        .and_then(|auth| auth.get("provider"))
        .and_then(serde_json::Value::as_str)
        .filter(|provider| !provider.trim().is_empty())
        .ok_or_else(|| "auth.provider must be a non-empty provider name".to_string())?;
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("invalid auth URL: {error}"))?;
    let loopback = matches!(
        parsed.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    );
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(
            "auth.provider requires HTTPS, or HTTP on localhost, 127.0.0.1, or [::1]".into(),
        );
    }
    Ok(Some(provider.to_string()))
}

fn parse_entry(
    name: &str,
    value: &serde_json::Value,
    origin: &str,
    provider_policy: ProviderAuthPolicy,
) -> Result<McpServerSpec, String> {
    let entry: McpServerEntry = serde_json::from_value(value.clone())
        .map_err(|error| format!("malformed MCP server entry: {error}"))?;
    let provider_auth = parse_provider_auth(
        value.as_object().and_then(|entry| entry.get("auth")),
        entry.transport_type.as_deref(),
        entry.url.as_deref(),
        provider_policy,
    )?;
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
        let spec = match provider_auth {
            Some(provider) => spec.with_auth_provider(provider),
            None => spec,
        };
        Ok(apply_entry_flags(
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
        Ok(apply_entry_flags(
            McpServerSpec::stdio(name, command, entry.args, env, None),
            enabled,
            timeout,
            exposure,
            tool_exposure,
        ))
    } else {
        Err("needs \"command\" or \"url\"".into())
    }
}

fn entry_or_warn(
    name: &str,
    value: &serde_json::Value,
    origin: &str,
    provider_policy: ProviderAuthPolicy,
) -> Option<McpServerSpec> {
    match parse_entry(name, value, origin, provider_policy) {
        Ok(spec) => Some(spec),
        Err(error) => {
            tracing::warn!("skipping MCP server entry {name:?} in {origin}: {error}");
            None
        }
    }
}

fn specs_from_value_with_policy(
    raw: &serde_json::Value,
    origin: &str,
    provider_policy: ProviderAuthPolicy,
) -> Vec<McpServerSpec> {
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
        .filter_map(|(name, value)| entry_or_warn(name, value, origin, provider_policy))
        .collect()
}

/// Parse an `{"mcpServers": {...}}` object (or a bare server map) into
/// specs. This conservative entry point is used by RPC/session input and
/// rejects provider authentication; global files and extensions have
/// dedicated policy-aware paths below.
pub fn specs_from_value(raw: &serde_json::Value, origin: &str) -> Vec<McpServerSpec> {
    specs_from_value_with_policy(raw, origin, ProviderAuthPolicy::Reject)
}

/// Extension manifests are installed code, so they have the same provider
/// auth privilege as the global file (unlike repository project files).
pub(crate) fn extension_specs_from_value(
    raw: &serde_json::Value,
    origin: &str,
) -> Vec<McpServerSpec> {
    specs_from_value_with_policy(raw, origin, ProviderAuthPolicy::Allow)
}

/// Parse ONE MCP server entry from an untrusted/session source. Provider
/// authentication is deliberately rejected here.
pub fn spec_from_entry(
    name: &str,
    value: &serde_json::Value,
    origin: &str,
) -> Option<McpServerSpec> {
    entry_or_warn(name, value, origin, ProviderAuthPolicy::Reject)
}

/// Extension counterpart of [`spec_from_entry`].
pub(crate) fn extension_spec_from_entry(
    name: &str,
    value: &serde_json::Value,
    origin: &str,
) -> Option<McpServerSpec> {
    entry_or_warn(name, value, origin, ProviderAuthPolicy::Allow)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpConfigScope {
    Global,
    Project,
    ProjectOverride,
}

/// One effective raw entry. Keeping the raw JSON here is important: session
/// loading and `mcp list` must make the same security and merge decisions,
/// rather than maintaining subtly different typed-spec overlays.
pub(crate) struct McpConfigEntry {
    pub(crate) name: String,
    raw: serde_json::Value,
    pub(crate) origin: String,
    pub(crate) scope: McpConfigScope,
    pub(crate) spec: Option<McpServerSpec>,
    pub(crate) invalid: Option<String>,
}

pub(crate) struct LoadedMcpConfig {
    pub(crate) entries: Vec<McpConfigEntry>,
    /// File-level errors and rejected project changes that left a valid
    /// global entry in place. Entry-specific fatal errors live on the entry.
    pub(crate) errors: Vec<String>,
}

const OVERRIDE_KEYS: [&str; 3] = ["enabled", "exposure", "toolExposure"];

fn is_override(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|entry| {
        !(entry.contains_key("command") || entry.contains_key("url") || entry.contains_key("type"))
    })
}

fn validate_override(value: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    if let Some(enabled) = value.get("enabled")
        && !enabled.is_boolean()
    {
        return Err("override enabled must be a boolean".into());
    }
    if let Some(exposure) = value.get("exposure") {
        let valid = exposure
            .as_str()
            .is_some_and(|value| tack_tools::mcp::McpExposure::from_config(value).is_some());
        if !valid {
            return Err("override exposure is not a supported exposure".into());
        }
    }
    if let Some(tool_exposure) = value.get("toolExposure") {
        let Some(tool_exposure) = tool_exposure.as_object() else {
            return Err("override toolExposure must be an object".into());
        };
        for (tool, exposure) in tool_exposure {
            let valid = exposure
                .as_str()
                .is_some_and(|value| tack_tools::mcp::McpExposure::from_config(value).is_some());
            if !valid {
                return Err(format!(
                    "override toolExposure for {tool:?} is not a supported exposure"
                ));
            }
        }
    }
    Ok(())
}

fn raw_servers<'a>(
    raw: &'a serde_json::Value,
    origin: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>, String> {
    if let Some(servers) = raw.get("mcpServers") {
        return servers
            .as_object()
            .ok_or_else(|| format!("{origin}: mcpServers must be an object"));
    }
    raw.as_object()
        .ok_or_else(|| format!("{origin}: expected an mcp.json object"))
}

fn invalid_entry(
    name: &str,
    raw: &serde_json::Value,
    origin: &str,
    scope: McpConfigScope,
    error: String,
) -> McpConfigEntry {
    let error = format!("{origin}: server {name:?}: {error}");
    McpConfigEntry {
        name: name.to_string(),
        raw: raw.clone(),
        origin: origin.to_string(),
        scope,
        spec: None,
        invalid: Some(error),
    }
}

/// Merge already parsed file values. `project_trusted=false` deliberately
/// does not even inspect the project value: malformed or hostile project
/// configuration must have no effect before trust is granted.
fn merge_raw_values(
    global: Option<(serde_json::Value, String)>,
    project: Option<(serde_json::Value, String)>,
    project_trusted: bool,
) -> LoadedMcpConfig {
    let mut entries = BTreeMap::<String, McpConfigEntry>::new();
    let mut errors = Vec::new();
    if let Some((raw, origin)) = global {
        match raw_servers(&raw, &origin) {
            Ok(servers) => {
                for (name, value) in servers {
                    let entry = match parse_entry(name, value, &origin, ProviderAuthPolicy::Allow) {
                        Ok(spec) => McpConfigEntry {
                            name: name.clone(),
                            raw: value.clone(),
                            origin: origin.clone(),
                            scope: McpConfigScope::Global,
                            spec: Some(spec),
                            invalid: None,
                        },
                        Err(error) => {
                            invalid_entry(name, value, &origin, McpConfigScope::Global, error)
                        }
                    };
                    entries.insert(name.clone(), entry);
                }
            }
            Err(error) => errors.push(error),
        }
    }
    if !project_trusted {
        return LoadedMcpConfig {
            entries: entries.into_values().collect(),
            errors,
        };
    }
    let Some((raw, origin)) = project else {
        return LoadedMcpConfig {
            entries: entries.into_values().collect(),
            errors,
        };
    };
    let servers = match raw_servers(&raw, &origin) {
        Ok(servers) => servers,
        Err(error) => {
            errors.push(error);
            return LoadedMcpConfig {
                entries: entries.into_values().collect(),
                errors,
            };
        }
    };
    for (name, value) in servers {
        if is_override(value) {
            let object = value.as_object().expect("is_override implies object");
            let has_base = entries.get(name).is_some_and(|entry| entry.spec.is_some());
            if !has_base {
                if entries.contains_key(name) {
                    errors.push(format!(
                        "{origin}: server {name:?}: no valid global server to override"
                    ));
                } else {
                    entries.insert(
                        name.clone(),
                        invalid_entry(
                            name,
                            value,
                            &origin,
                            McpConfigScope::Project,
                            "needs \"command\" or \"url\", or a global server to override".into(),
                        ),
                    );
                }
                continue;
            }
            let extra: Vec<&str> = object
                .keys()
                .map(String::as_str)
                .filter(|key| !OVERRIDE_KEYS.contains(key))
                .collect();
            let validation = if extra.is_empty() {
                validate_override(object)
            } else {
                Err(format!(
                    "an override can only set {}",
                    OVERRIDE_KEYS.join(", ")
                ))
            };
            if let Err(error) = validation {
                errors.push(format!(
                    "{origin}: server {name:?}: {error}; keeping global"
                ));
                continue;
            }
            let base = entries.get(name).expect("checked base");
            let mut merged = base.raw.clone();
            let merged_object = merged
                .as_object_mut()
                .expect("a valid global server entry is an object");
            // This is intentionally a SHALLOW merge. In particular,
            // toolExposure is one replacement subtree, not a per-pattern
            // merge, so removing a global pattern really hides/exposes it as
            // the project requested.
            for (key, value) in object {
                merged_object.insert(key.clone(), value.clone());
            }
            match parse_entry(name, &merged, &origin, ProviderAuthPolicy::Allow) {
                Ok(spec) => {
                    entries.insert(
                        name.clone(),
                        McpConfigEntry {
                            name: name.clone(),
                            raw: merged,
                            origin: base.origin.clone(),
                            scope: McpConfigScope::ProjectOverride,
                            spec: Some(spec),
                            invalid: None,
                        },
                    );
                }
                Err(error) => errors.push(format!(
                    "{origin}: server {name:?}: invalid merged override: {error}; keeping global"
                )),
            }
            continue;
        }

        // A complete project definition replaces the WHOLE global entry. It
        // never inherits auth, Authorization headers, OAuth, or other secrets.
        match parse_entry(name, value, &origin, ProviderAuthPolicy::Reject) {
            Ok(spec) => {
                entries.insert(
                    name.clone(),
                    McpConfigEntry {
                        name: name.clone(),
                        raw: value.clone(),
                        origin: origin.clone(),
                        scope: McpConfigScope::Project,
                        spec: Some(spec),
                        invalid: None,
                    },
                );
            }
            Err(error) => {
                if entries.get(name).is_some_and(|entry| entry.spec.is_some()) {
                    errors.push(format!(
                        "{origin}: server {name:?}: {error}; keeping global"
                    ));
                } else {
                    entries.insert(
                        name.clone(),
                        invalid_entry(name, value, &origin, McpConfigScope::Project, error),
                    );
                }
            }
        }
    }
    LoadedMcpConfig {
        entries: entries.into_values().collect(),
        errors,
    }
}

fn read_raw_file(path: &Path) -> Result<Option<serde_json::Value>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|error| format!("{}: invalid JSON: {error}", path.display()))
}

/// The single file-loading/raw-merge path used by both sessions and the CLI.
pub(crate) fn load_config(cwd: &Path, agent_dir: &Path) -> LoadedMcpConfig {
    let trusted = crate::project_trust::is_trusted(cwd, agent_dir);
    let mut file_errors = Vec::new();
    let global = match read_raw_file(&agent_dir.join("mcp.json")) {
        Ok(raw) => raw.map(|raw| (raw, agent_dir.join("mcp.json").display().to_string())),
        Err(error) => {
            file_errors.push(error);
            None
        }
    };
    let project_path = cwd.join(".pi").join("mcp.json");
    let project = if trusted {
        match read_raw_file(&project_path) {
            Ok(raw) => raw.map(|raw| (raw, project_path.display().to_string())),
            Err(error) => {
                file_errors.push(error);
                None
            }
        }
    } else {
        None
    };
    let mut loaded = merge_raw_values(global, project, trusted);
    file_errors.append(&mut loaded.errors);
    loaded.errors = file_errors;
    loaded
}

/// MCP servers from config files: global `~/.tack/agent/mcp.json` merged
/// with a trusted project's `.pi/mcp.json` by the shared raw merge above.
pub fn configured_servers(cwd: &Path, agent_dir: &Path) -> Vec<McpServerSpec> {
    let loaded = load_config(cwd, agent_dir);
    for error in &loaded.errors {
        tracing::warn!("MCP configuration: {error}");
    }
    loaded
        .entries
        .into_iter()
        .filter_map(|entry| {
            if let Some(error) = &entry.invalid {
                tracing::warn!("MCP configuration: {error}");
            }
            entry.spec
        })
        .collect()
}

#[cfg(test)]
fn load_mcp_json(path: &Path) -> Vec<McpServerSpec> {
    let raw = read_raw_file(path).ok().flatten();
    merge_raw_values(
        raw.map(|raw| (raw, path.display().to_string())),
        None,
        false,
    )
    .entries
    .into_iter()
    .filter_map(|entry| entry.spec)
    .collect()
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
        if spec.auth_provider.is_some() {
            // Defense in depth behind specs_from_value: even an internally
            // constructed session spec must not smuggle provider auth past
            // the RPC/session provenance boundary.
            tracing::warn!(
                "MCP session server {:?}: auth.provider is not allowed; ignoring entry",
                spec.name
            );
            continue;
        }
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

    fn merged(global: serde_json::Value, project: serde_json::Value) -> LoadedMcpConfig {
        merge_raw_values(
            Some((global, "global".into())),
            Some((project, "project".into())),
            true,
        )
    }

    fn entry<'a>(loaded: &'a LoadedMcpConfig, name: &str) -> &'a McpConfigEntry {
        loaded
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap()
    }

    #[test]
    fn project_override_shallow_merges_only_supported_keys() {
        let loaded = merged(
            serde_json::json!({"mcpServers": {
                "tools": {
                    "command": "global", "args": ["--safe"], "timeout": 12,
                    "enabled": true, "exposure": "hidden",
                    "toolExposure": {"keep": "direct", "remove": "hidden"}
                },
                "extra": {"command": "global", "enabled": true},
                "bad-enabled": {"command": "global", "enabled": true},
                "bad-exposure": {"command": "global", "exposure": "hidden"},
                "bad-tools": {"command": "global", "toolExposure": {"x": "hidden"}},
                "command-key": {"command": "global", "enabled": true},
                "url-key": {"command": "global", "enabled": true},
                "type-key": {"command": "global", "enabled": true}
            }}),
            serde_json::json!({"mcpServers": {
                "tools": {
                    "enabled": false, "exposure": "deferred",
                    "toolExposure": {"new": "direct"}
                },
                "extra": {"enabled": false, "args": ["--attacker"]},
                "bad-enabled": {"enabled": "false"},
                "bad-exposure": {"exposure": "bogus"},
                "bad-tools": {"toolExposure": {"x": "bogus"}},
                "command-key": {"command": null, "enabled": false},
                "url-key": {"url": null, "enabled": false},
                "type-key": {"type": null, "enabled": false},
                "missing": {"enabled": false}
            }}),
        );
        let tools = entry(&loaded, "tools");
        assert_eq!(tools.scope, McpConfigScope::ProjectOverride);
        let spec = tools.spec.as_ref().unwrap();
        assert!(!spec.enabled);
        assert_eq!(spec.exposure, tack_tools::mcp::McpExposure::Deferred);
        assert_eq!(
            spec.tool_exposure,
            vec![("new".into(), tack_tools::mcp::McpExposure::Direct)],
            "toolExposure is replaced as one subtree, not merged per pattern"
        );
        let tack_tools::mcp::McpTransport::Stdio { command, args, .. } = &spec.transport else {
            panic!("expected stdio")
        };
        assert_eq!(command, "global");
        assert_eq!(args, &["--safe"]);
        assert_eq!(
            spec.effective_request_timeout(),
            Some(std::time::Duration::from_secs(12))
        );

        for name in [
            "extra",
            "bad-enabled",
            "bad-exposure",
            "bad-tools",
            "command-key",
            "url-key",
            "type-key",
        ] {
            let spec = entry(&loaded, name).spec.as_ref().unwrap();
            assert!(spec.enabled, "{name}: global entry must survive");
            assert_eq!(entry(&loaded, name).scope, McpConfigScope::Global);
        }
        assert!(entry(&loaded, "missing").spec.is_none());
        assert!(entry(&loaded, "missing").invalid.is_some());
        assert!(
            loaded.errors.len() >= 5,
            "invalid overrides need diagnostics: {:?}",
            loaded.errors
        );
    }

    #[test]
    fn untrusted_project_is_completely_ignored() {
        let global = serde_json::json!({"mcpServers": {
            "shared": {"command": "global", "exposure": "hidden"}
        }});
        let project = serde_json::json!({"mcpServers": {
            "shared": {"command": "project", "exposure": "direct"},
            "project-only": {"enabled": false}
        }});
        let loaded = merge_raw_values(
            Some((global, "global".into())),
            Some((project, "project".into())),
            false,
        );
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(entry(&loaded, "shared").scope, McpConfigScope::Global);
        let spec = entry(&loaded, "shared").spec.as_ref().unwrap();
        assert_eq!(spec.exposure, tack_tools::mcp::McpExposure::Hidden);
        let tack_tools::mcp::McpTransport::Stdio { command, .. } = &spec.transport else {
            panic!("expected stdio")
        };
        assert_eq!(command, "global");
        assert!(loaded.errors.is_empty());
    }

    #[test]
    fn project_auth_security_boundaries_preserve_or_replace_atomically() {
        let loaded = merged(
            serde_json::json!({"mcpServers": {
                "inherited": {
                    "url": "https://global.example/mcp", "auth": {"provider": "radius"},
                    "headers": {"Authorization": "Bearer global-static"}, "oauth": true
                },
                "blocked": {
                    "url": "https://global.example/mcp", "auth": {"provider": "radius"}
                },
                "replacement": {
                    "url": "https://global.example/mcp", "auth": {"provider": "radius"},
                    "headers": {"Authorization": "Bearer global-static"}, "oauth": true
                }
            }}),
            serde_json::json!({"mcpServers": {
                "inherited": {"exposure": "hidden"},
                "blocked": {
                    "url": "https://attacker.example/mcp", "auth": {"provider": "radius"}
                },
                "replacement": {"url": "http://localhost:8080/mcp", "enabled": false}
            }}),
        );
        let inherited = entry(&loaded, "inherited").spec.as_ref().unwrap();
        assert_eq!(inherited.auth_provider.as_deref(), Some("radius"));
        assert!(inherited.oauth.is_some(), "overrides inherit global OAuth");
        let tack_tools::mcp::McpTransport::Http { headers, .. } = &inherited.transport else {
            panic!("expected HTTP")
        };
        assert_eq!(headers[0].1, "Bearer global-static");

        let blocked = entry(&loaded, "blocked");
        assert_eq!(blocked.scope, McpConfigScope::Global);
        let blocked = blocked.spec.as_ref().unwrap();
        assert_eq!(blocked.url(), Some("https://global.example/mcp"));
        assert_eq!(blocked.auth_provider.as_deref(), Some("radius"));
        assert!(loaded.errors.iter().any(|error| error.contains("blocked")));

        let replacement = entry(&loaded, "replacement");
        assert_eq!(replacement.scope, McpConfigScope::Project);
        let replacement = replacement.spec.as_ref().unwrap();
        assert_eq!(replacement.url(), Some("http://localhost:8080/mcp"));
        assert_eq!(replacement.auth_provider, None);
        assert!(replacement.oauth.is_none());
        let tack_tools::mcp::McpTransport::Http { headers, .. } = &replacement.transport else {
            panic!("expected HTTP")
        };
        assert!(headers.is_empty(), "a full replacement inherits no token");
    }

    #[test]
    fn provider_auth_validation_and_provenance_are_strict() {
        let loaded = merge_raw_values(
            Some((
                serde_json::json!({"mcpServers": {
                    "https": {"url": "https://example.com/mcp", "auth": {"provider": "p"}},
                    "localhost": {"url": "http://localhost:1/mcp", "auth": {"provider": "p"}},
                    "ipv4": {"url": "http://127.0.0.1:1/mcp", "auth": {"provider": "p"}},
                    "ipv6": {"url": "http://[::1]:1/mcp", "auth": {"provider": "p"}},
                    "plain": {"url": "http://example.com/mcp", "auth": {"provider": "p"}},
                    "empty": {"url": "https://example.com/mcp", "auth": {"provider": " "}},
                    "null": {"url": "https://example.com/mcp", "auth": null},
                    "sse": {"type": "sse", "url": "https://example.com/sse", "auth": {"provider": "p"}},
                    "stdio": {"command": "server", "auth": {"provider": "p"}}
                }}),
                "global".into(),
            )),
            None,
            false,
        );
        for valid in ["https", "localhost", "ipv4", "ipv6"] {
            assert_eq!(
                entry(&loaded, valid)
                    .spec
                    .as_ref()
                    .unwrap()
                    .auth_provider
                    .as_deref(),
                Some("p")
            );
        }
        for invalid in ["plain", "empty", "null", "sse", "stdio"] {
            let entry = entry(&loaded, invalid);
            assert!(entry.spec.is_none(), "{invalid} must be rejected");
            assert!(entry.invalid.is_some(), "{invalid} needs a diagnostic");
        }
        let raw = serde_json::json!({"mcpServers": {
            "rpc": {"url": "https://example.com/mcp", "auth": {"provider": "p"}}
        }});
        assert!(specs_from_value(&raw, "set_mcp_servers").is_empty());
        assert_eq!(extension_specs_from_value(&raw, "extension test").len(), 1);
    }

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
    fn session_provider_auth_is_rejected_even_for_constructed_specs() {
        let configured = vec![McpServerSpec::http(
            "a".into(),
            "https://global.example/mcp".into(),
            vec![],
        )];
        let session = vec![
            McpServerSpec::http("a".into(), "https://attacker.example/mcp".into(), vec![])
                .with_auth_provider("radius".into()),
        ];
        let merged = merge_session_servers(configured, session);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].url(), Some("https://global.example/mcp"));
        assert_eq!(merged[0].auth_provider, None);
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
