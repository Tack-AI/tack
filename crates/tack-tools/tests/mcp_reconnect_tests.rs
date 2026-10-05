//! MCP resilience integration tests: reconnect-after-drop,
//! `tools/list_changed` refreshes, and per-request timeouts (with the
//! progress-reset rule). Fixture servers are in-process rmcp servers over
//! duplex streams (no network, no spawned processes).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rmcp::RoleServer;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorData as McpError, ListToolsResult,
    PaginatedRequestParams, ProgressNotificationParam, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::{Peer, RequestContext};
use rmcp::{ServerHandler, ServiceExt};
use serde_json::json;
use tack_tools::mcp::mcp_tools;
use tokio_util::sync::CancellationToken;

/// A fixture server with a DYNAMIC tool list: `alpha` always, `beta` once
/// the flag flips. `call_tool` echoes the tool name.
#[derive(Clone, Debug)]
struct DynamicServer {
    beta: Arc<AtomicBool>,
}

fn plain_tool(name: &str) -> Tool {
    Tool::new(
        name.to_string(),
        format!("tool {name}"),
        Arc::new(serde_json::Map::new()),
    )
}

impl ServerHandler for DynamicServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("dynamic fixture")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = vec![plain_tool("alpha")];
        if self.beta.load(Ordering::Relaxed) {
            tools.push(plain_tool("beta"));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        Ok(
            CallToolResult::success(vec![ContentBlock::text(format!("called {}", params.name))])
                .into(),
        )
    }
}

/// Server handle: lets a test kill the server or push list_changed
/// notifications.
struct ServerHandle {
    cancel: rmcp::service::RunningServiceCancellationToken,
    peer: Peer<RoleServer>,
}

/// Spawn the fixture server on one end of a fresh duplex. The control
/// handle arrives on the returned receiver once the CLIENT completes the
/// initialize handshake (the server's `serve()` only returns then) — so
/// connect first, then `rx.await`, never the other way around.
fn spawn_dynamic(
    beta: &Arc<AtomicBool>,
) -> (
    tokio::io::DuplexStream,
    tokio::sync::oneshot::Receiver<ServerHandle>,
) {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let server = DynamicServer { beta: beta.clone() };
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = tx.send(ServerHandle {
            cancel: running.cancellation_token(),
            peer: running.peer().clone(),
        });
        let _ = running.waiting().await;
    });
    (client_io, rx)
}

/// Wait until `cond` holds (poll; 5 s cap) — notification refreshes and
/// close detection are async.
async fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}

#[tokio::test]
async fn reconnect_respawns_a_dead_server_and_refreshes_tools() {
    let beta = Arc::new(AtomicBool::new(false));
    // The reconnect connector rebuilds BOTH ends (fresh duplex + fixture).
    let connector_beta = beta.clone();
    let connector = Arc::new(move || {
        let beta = connector_beta.clone();
        Box::pin(async move {
            let (client_io, _handle_rx) = spawn_dynamic(&beta);
            tack_tools::mcp::connect_transport("dyn", client_io, Default::default()).await
        })
            as std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<tack_tools::mcp::McpConnection, String>>
                        + Send,
                >,
            >
    });
    let (client_io, server_rx) = spawn_dynamic(&beta);
    let conn = Arc::new(
        tack_tools::mcp::connect_transport_with(
            "dyn",
            client_io,
            Default::default(),
            Some(connector),
        )
        .await
        .unwrap(),
    );
    let server = server_rx.await.unwrap();
    assert_eq!(conn.tools().len(), 1, "starts with alpha only");

    // Kill the server; the connection must notice.
    server.cancel.cancel();
    assert!(wait_until(|| conn.is_closed()).await, "close detected");

    // The next tool call heals the connection (fresh server, now with beta).
    beta.store(true, Ordering::Relaxed);
    let tools = mcp_tools(std::slice::from_ref(&conn));
    let alpha = tools
        .iter()
        .find(|t| t.name() == "mcp__dyn__alpha")
        .unwrap();
    let result = alpha
        .execute("t1", json!({}), CancellationToken::new(), &|_| {})
        .await
        .expect("call after reconnect must succeed");
    let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
        panic!("expected text result")
    };
    assert_eq!(text, "called alpha");
    // Reconnect re-probed capabilities: beta is now listed.
    assert_eq!(conn.tools().len(), 2, "reconnect re-probes the tool list");
}

#[tokio::test]
async fn tool_list_changed_notification_refreshes_cached_tools() {
    let beta = Arc::new(AtomicBool::new(false));
    let (client_io, server_rx) = spawn_dynamic(&beta);
    let conn = tack_tools::mcp::connect_transport("dyn", client_io, Default::default())
        .await
        .unwrap();
    let server = server_rx.await.unwrap();
    assert_eq!(conn.tools().len(), 1);
    let generation = conn.generation();

    // Server gains a tool and announces it.
    beta.store(true, Ordering::Relaxed);
    server.peer.notify_tool_list_changed().await.unwrap();

    assert!(
        wait_until(|| conn.tools().len() == 2).await,
        "tools/list_changed refreshes the cached list"
    );
    assert!(
        wait_until(|| conn.generation() > generation).await,
        "generation bump signals the change to status surfaces"
    );
    assert!(conn.tools().iter().any(|t| t.name == "beta"));
}

/// A fixture server whose tool sleeps (longer than any test timeout).
#[derive(Clone, Debug)]
struct SlowServer;

impl ServerHandler for SlowServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("slow fixture")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![plain_tool("slow")]))
    }

    async fn call_tool(
        &self,
        _params: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(CallToolResult::success(vec![ContentBlock::text("done")]).into())
    }
}

#[tokio::test]
async fn request_timeout_fails_a_stuck_call() {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let running = SlowServer.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let conn = Arc::new(
        tack_tools::mcp::connect_transport("slow", client_io, Default::default())
            .await
            .unwrap()
            .with_request_timeout_override(Some(Duration::from_millis(150))),
    );
    let tools = mcp_tools(&[conn]);
    let slow = &tools[0];
    let err = slow
        .execute("t1", json!({}), CancellationToken::new(), &|_| {})
        .await
        .unwrap_err();
    assert!(err.contains("timed out"), "timeout error: {err}");
}

/// A fixture server that reports progress while working (the timeout must
/// reset on each progress notification).
#[derive(Clone, Debug)]
struct ProgressServer;

impl ServerHandler for ProgressServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("progress fixture")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![plain_tool("working")]))
    }

    async fn call_tool(
        &self,
        _params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        // Work for ~450 ms in progress-reporting steps; the client's 150 ms
        // timeout would fire without the reset rule.
        if let Some(token) = context.meta.get_progress_token() {
            for step in 1..=9u32 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if context
                    .peer
                    .notify_progress(
                        ProgressNotificationParam::new(token.clone(), step as f64).with_total(9.0),
                    )
                    .await
                    .is_err()
                {
                    break;
                }
            }
        } else {
            panic!("client must attach a progress token for this test");
        }
        Ok(CallToolResult::success(vec![ContentBlock::text("done")]).into())
    }
}

#[tokio::test]
async fn progress_notifications_reset_the_request_timeout() {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let running = ProgressServer.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let conn = Arc::new(
        tack_tools::mcp::connect_transport("progress", client_io, Default::default())
            .await
            .unwrap()
            .with_request_timeout_override(Some(Duration::from_millis(150))),
    );
    let tools = mcp_tools(&[conn]);
    let working = &tools[0];
    let result = working
        .execute("t1", json!({}), CancellationToken::new(), &|_| {})
        .await;
    match &result {
        Ok(_) => {}
        Err(e) => panic!("progress must reset the timeout, got: {e}"),
    }
}

/// Annotations E2E: hints declared by the server land in the registry the
/// permission layer consults.
#[tokio::test]
async fn server_tool_annotations_reach_the_permission_registry() {
    #[derive(Clone, Debug)]
    struct AnnotatedServer;

    impl ServerHandler for AnnotatedServer {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            let read_only = plain_tool("search")
                .with_annotations(ToolAnnotations::new().read_only(true).destructive(false));
            let unannotated = plain_tool("mutate");
            Ok(ListToolsResult::with_all_items(vec![
                read_only,
                unannotated,
            ]))
        }
    }

    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let running = AnnotatedServer.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let conn = Arc::new(
        tack_tools::mcp::connect_transport("ann", client_io, Default::default())
            .await
            .unwrap(),
    );
    let _tools = mcp_tools(&[conn]); // registration side effect
    let hit = tack_tools::mcp::mcp_tool_annotations("mcp__ann__search")
        .expect("annotated tool must be registered");
    assert!(hit.read_only);
    assert!(!hit.destructive);
    // A tool without annotations stays unregistered (permission layer
    // treats it as mutating).
    assert!(tack_tools::mcp::mcp_tool_annotations("mcp__ann__mutate").is_none());
}
