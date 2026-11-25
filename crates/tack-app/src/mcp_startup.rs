//! Asynchronous MCP startup. Direct servers gate the first prompt (bounded
//! wait); deferred servers connect in the background and are awaited by
//! `tool_search` on demand. Connections stay owned by the manager so a
//! deferred snapshot and the initial tool list share the same live services.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::join_all;
use tack_agent_core::AgentTool;
use tack_tools::mcp::{McpConnection, McpExposure, McpServerSpec};
use tack_tools::tool_search::{DeferredPoolSnapshot, DeferredPoolSource, PoolEntry};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_STARTUP_WAIT: Duration = Duration::from_secs(10);

type ConnectResult = Result<Arc<McpConnection>, String>;
type ConnectFuture = Pin<Box<dyn Future<Output = ConnectResult> + Send>>;
type Connector = Arc<dyn Fn(McpServerSpec, CancellationToken) -> ConnectFuture + Send + Sync>;

#[derive(Clone)]
struct ServerState {
    spec: McpServerSpec,
    ready: watch::Receiver<Option<ConnectResult>>,
}

#[derive(Clone)]
pub struct McpStartupManager {
    servers: Arc<BTreeMap<String, ServerState>>,
    untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl std::fmt::Debug for McpStartupManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpStartupManager")
            .field("servers", &self.servers.len())
            .finish_non_exhaustive()
    }
}

fn has_direct_tools(spec: &McpServerSpec) -> bool {
    spec.exposure == McpExposure::Direct
        || spec
            .tool_exposure
            .iter()
            .any(|(_, exposure)| *exposure == McpExposure::Direct)
}

fn has_deferred_tools(spec: &McpServerSpec) -> bool {
    spec.exposure == McpExposure::Deferred
        || spec
            .tool_exposure
            .iter()
            .any(|(_, exposure)| *exposure == McpExposure::Deferred)
}

impl McpStartupManager {
    /// Start every enabled server immediately, without awaiting any of them.
    pub fn start(
        specs: Vec<McpServerSpec>,
        agent_dir: &std::path::Path,
        interactive: bool,
        callbacks: tack_tools::mcp::McpClientCallbacks,
        cancel: CancellationToken,
        untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        let connector: Connector = {
            let agent_dir = agent_dir.to_path_buf();
            Arc::new(move |spec, cancel| {
                let agent_dir = agent_dir.clone();
                let callbacks = callbacks.clone();
                Box::pin(async move {
                    crate::mcp_oauth::connect_one_oauth(
                        &spec,
                        &agent_dir,
                        interactive,
                        &callbacks,
                        &cancel,
                    )
                    .await
                })
            })
        };
        Self::start_with_connector(specs, connector, cancel, untrusted)
    }

    fn start_with_connector(
        specs: Vec<McpServerSpec>,
        connector: Connector,
        connect_cancel: CancellationToken,
        untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        let mut servers = BTreeMap::new();
        for spec in specs.into_iter().filter(|spec| spec.enabled) {
            let (tx, rx) = watch::channel(None);
            let task_spec = spec.clone();
            let connector = connector.clone();
            let cancel = connect_cancel.clone();
            tokio::spawn(async move {
                let result = connector(task_spec, cancel).await;
                let _ = tx.send(Some(result));
            });
            servers.insert(spec.name.clone(), ServerState { spec, ready: rx });
        }
        McpStartupManager {
            servers: Arc::new(servers),
            untrusted,
        }
    }

    pub fn with_untrusted(mut self, untrusted: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.untrusted = Some(untrusted);
        self
    }

    pub fn has_deferred_servers(&self) -> bool {
        self.servers
            .values()
            .any(|server| has_deferred_tools(&server.spec))
    }

    pub fn deferred_server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .servers
            .values()
            .filter(|server| has_deferred_tools(&server.spec))
            .map(|server| server.spec.name.clone())
            .collect();
        names.sort();
        names
    }

    fn direct_server_names(&self) -> Vec<String> {
        self.servers
            .values()
            .filter(|server| has_direct_tools(&server.spec))
            .map(|server| server.spec.name.clone())
            .collect()
    }

    async fn wait_server(&self, name: &str, cancel: CancellationToken) -> ConnectResult {
        let Some(server) = self.servers.get(name) else {
            return Err(format!("unknown MCP server {name}"));
        };
        let mut ready = server.ready.clone();
        loop {
            if let Some(result) = ready.borrow().clone() {
                return result;
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err("MCP connection wait cancelled".to_string()),
                changed = ready.changed() => {
                    if changed.is_err() {
                        return Err(format!("MCP server {name} stopped before becoming ready"));
                    }
                }
            }
        }
    }

    /// Wait only for servers with direct tools. Deferred connections never
    /// block this path. `false` means the bounded wait expired; failures are
    /// reported by the ordinary per-server logging/status paths.
    pub async fn wait_direct(&self, wait: Duration, cancel: CancellationToken) -> bool {
        let names = self.direct_server_names();
        if names.is_empty() {
            return true;
        }
        let waits = names
            .iter()
            .map(|name| self.wait_server(name, cancel.clone()));
        match tokio::time::timeout(wait, join_all(waits)).await {
            Ok(_) => !cancel.is_cancelled(),
            Err(_) => {
                tracing::warn!(
                    "MCP direct servers still connecting after {}s; continuing without their unavailable tools",
                    wait.as_secs_f64()
                );
                false
            }
        }
    }

    /// Connections ready at this instant. Pending/failed servers simply do
    /// not contribute tools; they can still appear in a later deferred snapshot.
    pub fn ready_connections(&self) -> Vec<Arc<McpConnection>> {
        self.servers
            .values()
            .filter_map(|server| server.ready.borrow().clone())
            .filter_map(|result| result.ok())
            .collect()
    }

    pub fn failures(&self) -> Vec<(String, String)> {
        self.servers
            .values()
            .filter_map(|server| {
                server
                    .ready
                    .borrow()
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .map(|error| (server.spec.name.clone(), error.clone()))
            })
            .collect()
    }

    pub fn has_failures(&self) -> bool {
        self.servers.values().any(|server| {
            server
                .ready
                .borrow()
                .as_ref()
                .is_some_and(|result| result.is_err())
        })
    }

    async fn deferred_tools(
        &self,
        cancel: CancellationToken,
    ) -> Result<Vec<Arc<dyn AgentTool>>, String> {
        let names = self.deferred_server_names();
        let waits = names
            .iter()
            .map(|name| self.wait_server(name, cancel.clone()));
        // Failed servers settle too; do not let one broken entry prevent
        // every other deferred server from becoming searchable.
        let _ = join_all(waits).await;
        if cancel.is_cancelled() {
            return Err("MCP connection wait cancelled".to_string());
        }
        let connections: Vec<Arc<McpConnection>> = self
            .servers
            .values()
            .filter(|server| has_deferred_tools(&server.spec))
            .filter_map(|server| server.ready.borrow().clone())
            .filter_map(|result| result.ok())
            .collect();
        Ok(
            tack_tools::mcp::mcp_tools_with(&connections, self.untrusted.clone())
                .into_iter()
                .filter(|tool| tool.starts_deferred())
                .collect(),
        )
    }
}

#[async_trait]
impl DeferredPoolSource for McpStartupManager {
    async fn snapshot(&self, cancel: CancellationToken) -> Result<DeferredPoolSnapshot, String> {
        let tools = self.deferred_tools(cancel).await?;
        let entries = tools
            .iter()
            .map(|tool| PoolEntry {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
            })
            .collect();
        Ok(DeferredPoolSnapshot { entries, tools })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn spec(name: &str, exposure: McpExposure) -> McpServerSpec {
        McpServerSpec::stdio(
            name.to_string(),
            "unused".to_string(),
            Vec::new(),
            Vec::new(),
            None,
        )
        .with_exposure(exposure)
    }

    #[tokio::test]
    async fn deferred_pending_server_does_not_block_first_prompt() {
        let connector: Connector = Arc::new(|_spec, _cancel| {
            Box::pin(async {
                std::future::pending::<()>().await;
                unreachable!()
            })
        });
        let manager = McpStartupManager::start_with_connector(
            vec![spec("slow-deferred", McpExposure::Deferred)],
            connector,
            CancellationToken::new(),
            None,
        );
        let started = std::time::Instant::now();
        assert!(
            manager
                .wait_direct(Duration::from_secs(10), CancellationToken::new())
                .await
        );
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(manager.ready_connections().is_empty());
        assert_eq!(manager.deferred_server_names(), ["slow-deferred"]);
    }

    #[tokio::test]
    async fn direct_wait_is_bounded_and_cancellable() {
        let connector: Connector = Arc::new(|_spec, _cancel| {
            Box::pin(async {
                std::future::pending::<()>().await;
                unreachable!()
            })
        });
        let manager = McpStartupManager::start_with_connector(
            vec![spec("slow-direct", McpExposure::Direct)],
            connector,
            CancellationToken::new(),
            None,
        );
        let started = std::time::Instant::now();
        assert!(
            !manager
                .wait_direct(Duration::from_millis(20), CancellationToken::new())
                .await
        );
        assert!(started.elapsed() < Duration::from_millis(200));
    }
}
