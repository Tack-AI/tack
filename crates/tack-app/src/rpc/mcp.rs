//! MCP connection pool shared by prompt runs and the `get_mcp_status` RPC
//! command. Extracted from prompt.rs so both paths build/reuse the same
//! cached pool (one server child set per RPC session) instead of the status
//! probe spawning parallel instances.

use std::sync::Arc;

use tack_tools::mcp::{McpConnection, McpServerSpec, McpTransport};
use tokio::sync::Mutex;

use crate::settings::Settings;

use super::RpcState;

/// Outcome of an MCP pool build: the live connections plus per-server
/// connect failures (empty when the pool came from cache or every server
/// connected), and the effective spec set they were built from.
pub(crate) struct McpPoolOutcome {
    pub(crate) connections: Vec<Arc<McpConnection>>,
    pub(crate) failures: Vec<(String, String)>,
    pub(crate) specs: Vec<McpServerSpec>,
}

/// Effective MCP server set for this session: config files overridden by
/// session-scoped specs (`set_mcp_servers`, the host's per-session
/// injection), plus extension bundle servers.
pub(crate) fn effective_specs(
    session_specs: &[McpServerSpec],
    ext_specs: Vec<McpServerSpec>,
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
) -> Vec<McpServerSpec> {
    let configured = crate::mcp_config::configured_servers(cwd, agent_dir);
    let mut merged = crate::mcp_config::merge_session_servers(configured, session_specs.to_vec());
    merged.extend(ext_specs);
    merged
}

/// Fold resolved OAuth token state into the pool fingerprint: bearer
/// tokens are baked into HTTP connections at connect time, so a refreshed
/// token must rebuild the pool — otherwise cached connections keep 401-ing.
/// Resolution is cheap (token-store read; a network refresh only when
/// expired). Hashed, never embedded, so tokens cannot leak via logs/debug
/// output.
async fn oauth_fingerprint_suffix(specs: &[McpServerSpec], agent_dir: &std::path::Path) -> String {
    let mut suffix = String::new();
    for spec in specs {
        if spec.oauth.is_some()
            && let Some(url) = spec.url()
        {
            let token = crate::mcp_oauth::access_token(
                agent_dir,
                &spec.name,
                url,
                &crate::mcp_oauth::spec_oauth(spec),
                false,
            )
            .await
            .ok()
            .flatten();
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            token.hash(&mut h);
            use std::fmt::Write as _;
            let _ = write!(suffix, "|{:x}", h.finish());
        }
    }
    suffix
}

/// Build or reuse the session's MCP connection pool. Replaces the inline
/// prompt.rs logic (F08): the pool is keyed by a fingerprint of the
/// effective spec set (+cwd/agent_dir/model); a mismatch or a dead cached
/// connection rebuilds, and the replaced connections kill their server
/// processes on drop. Only COMPLETE connection sets are cached: caching a
/// partial set under the full fingerprint would never retry a temporarily
/// failed server.
pub(crate) async fn ensure_mcp_pool(
    state: &Arc<Mutex<RpcState>>,
    settings: &Settings,
    sampling: &crate::mcp_sampling::SharedSamplingLlm,
    model: &tack_ai::Model,
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    ext_specs: Vec<McpServerSpec>,
) -> McpPoolOutcome {
    let session_specs = state.lock().await.session_mcp_servers.clone();
    let specs = effective_specs(&session_specs, ext_specs, cwd, agent_dir);
    if specs.is_empty() {
        // No servers configured (anymore): drop the pool — replacing
        // it kills the cached server processes.
        state.lock().await.mcp_connections = None;
        return McpPoolOutcome {
            connections: Vec::new(),
            failures: Vec::new(),
            specs,
        };
    }
    let mut fingerprint = super::mcp_cache_fingerprint(cwd, agent_dir, &specs, model);
    fingerprint.push_str(&oauth_fingerprint_suffix(&specs, agent_dir).await);
    let spec_count = specs.len();
    let cached = {
        let state_guard = state.lock().await;
        state_guard
            .mcp_connections
            .as_ref()
            .filter(|cache| {
                cache.fingerprint == fingerprint
                    // Liveness: a crashed server child must not
                    // be reused forever — dead connections fall
                    // through to a rebuild.
                    && cache.connections.iter().all(|c| !c.is_closed())
            })
            .map(|cache| cache.connections.clone())
    };
    match cached {
        Some(connections) => McpPoolOutcome {
            connections,
            failures: Vec::new(),
            specs,
        },
        None => {
            let callbacks = crate::mcp_config::client_callbacks(
                settings,
                sampling,
                crate::mcp_sampling::log_usage_sink(),
                crate::mcp_elicitation::InteractionMode::Headless,
                None,
            );
            let outcomes = crate::mcp_oauth::connect_all_oauth_reporting(
                specs.clone(),
                agent_dir,
                false,
                callbacks,
            )
            .await;
            let mut connections = Vec::new();
            let mut failures = Vec::new();
            for outcome in outcomes {
                match outcome.result {
                    Ok(conn) => connections.push(conn),
                    Err(e) => failures.push((outcome.name, e)),
                }
            }
            state.lock().await.mcp_connections =
                (connections.len() == spec_count).then(|| super::McpConnectionCache {
                    fingerprint,
                    connections: connections.clone(),
                });
            McpPoolOutcome {
                connections,
                failures,
                specs,
            }
        }
    }
}

/// One server's entry in the `get_mcp_status` response.
pub(crate) struct McpServerStatus {
    pub(crate) name: String,
    pub(crate) transport: &'static str,
    pub(crate) status: &'static str,
    pub(crate) tool_count: usize,
    pub(crate) error: Option<String>,
}

fn transport_name(spec: &McpServerSpec) -> &'static str {
    match &spec.transport {
        McpTransport::Stdio { .. } => "stdio",
        McpTransport::Http { .. } => "http",
        McpTransport::Sse { .. } => "sse",
    }
}

/// Status snapshot for `get_mcp_status`. `connect=true` builds/reuses the
/// pool (settings-page refresh); `connect=false` only reads the cached
/// pool — OAuth polling must not mutate the connection set or block on
/// slow servers.
pub(crate) async fn mcp_status(
    state: &Arc<Mutex<RpcState>>,
    settings: &Settings,
    sampling: &crate::mcp_sampling::SharedSamplingLlm,
    model: &tack_ai::Model,
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    connect: bool,
) -> Vec<McpServerStatus> {
    let (session_specs, ext_specs) = {
        let state_guard = state.lock().await;
        let ext = state_guard.extensions.lock().await;
        (
            state_guard.session_mcp_servers.clone(),
            ext.bundle_mcp_servers.clone(),
        )
    };
    if !connect {
        // Status-only: report from the cached pool without connecting.
        let specs = effective_specs(&session_specs, ext_specs, cwd, agent_dir);
        let cached = {
            let state_guard = state.lock().await;
            state_guard
                .mcp_connections
                .as_ref()
                .map(|cache| cache.connections.clone())
        };
        return specs
            .iter()
            .map(|spec| {
                let live = cached.as_ref().and_then(|connections| {
                    connections
                        .iter()
                        .find(|c| c.name == spec.name && !c.is_closed())
                });
                match live {
                    Some(conn) => McpServerStatus {
                        name: spec.name.clone(),
                        transport: transport_name(spec),
                        status: "connected",
                        tool_count: conn.tools.len(),
                        error: None,
                    },
                    None => McpServerStatus {
                        name: spec.name.clone(),
                        transport: transport_name(spec),
                        status: "disconnected",
                        tool_count: 0,
                        error: None,
                    },
                }
            })
            .collect();
    }
    let outcome =
        ensure_mcp_pool(state, settings, sampling, model, cwd, agent_dir, ext_specs).await;
    outcome
        .specs
        .iter()
        .map(|spec| {
            if let Some(conn) = outcome.connections.iter().find(|c| c.name == spec.name) {
                return McpServerStatus {
                    name: spec.name.clone(),
                    transport: transport_name(spec),
                    status: "connected",
                    tool_count: conn.tools.len(),
                    error: None,
                };
            }
            let error = outcome
                .failures
                .iter()
                .find(|(name, _)| name == &spec.name)
                .map(|(_, e)| e.clone());
            McpServerStatus {
                name: spec.name.clone(),
                transport: transport_name(spec),
                status: "failed",
                tool_count: 0,
                error: Some(error.unwrap_or_else(|| "connection failed".to_string())),
            }
        })
        .collect()
}
