//! ACP (Agent Client Protocol) server mode: `tack acp`.
//!
//! Uses the official `agent-client-protocol` 0.x crate (JSON-RPC 2.0 over
//! stdio). The crate's futures are `?Send`, so everything ACP-side runs on a
//! current-thread runtime inside a `LocalSet`; the agent loop and tools are
//! `Send` and run via `tokio::spawn` on the same runtime.
//!
//! Known v1 SDK wart: handlers don't receive the connection, so a clone-able
//! handle is stashed in shared state (`Rc<RefCell<Option<AgentSideConnection>>>`)
//! right after construction.

pub mod agent;
pub mod convert;
pub mod session;
pub mod terminal;

use std::cell::RefCell;
use std::rc::Rc;

use agent_client_protocol::AgentSideConnection;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use self::agent::TackAcpAgent;

/// Shared handle through which handlers reach the client connection.
/// `AgentSideConnection` is not `Clone`, so it lives behind an `Rc`.
pub type SharedConn = Rc<RefCell<Option<Rc<AgentSideConnection>>>>;

/// Run the ACP server on stdin/stdout until the client disconnects.
pub async fn serve(overrides: &agent::AcpOverrides) -> anyhow::Result<()> {
    let shared_conn: SharedConn = Rc::new(RefCell::new(None));
    let agent = TackAcpAgent::new(shared_conn.clone(), overrides);
    // Kept alive past the connection so session plugins can be shut down
    // once the client disconnects (graceful stop + final metrics drain).
    let sessions = agent.sessions();

    let local = tokio::task::LocalSet::new();
    let result = local
        .run_until(async move {
            let stdout = tokio::io::stdout().compat_write();
            let stdin = tokio::io::stdin().compat();
            let (conn, io_task) = AgentSideConnection::new(agent, stdout, stdin, |fut| {
                tokio::task::spawn_local(fut);
            });
            shared_conn.borrow_mut().replace(Rc::new(conn));

            io_task
                .await
                .map_err(|e| anyhow::anyhow!("ACP connection error: {e}"))
        })
        .await;

    // The connection ended (client disconnect or io error). ACP has no
    // session/close handshake, so this is the only teardown point for the
    // per-session ExtensionManagers; without it every session's plugins
    // (processes/connections) and their final metrics flush leaked until
    // process exit. Runs inside the LocalSet: plugin shutdown may
    // spawn_local.
    local
        .run_until(session::shutdown_all_sessions(&sessions))
        .await;
    result
}
