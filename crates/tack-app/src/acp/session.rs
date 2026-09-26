//! Per-session state and the client bridge.
//!
//! The bridge exists because the loop's hooks and tools run in `Send`
//! contexts (spawned tasks) while ACP client methods are `?Send` and only
//! callable from local tasks. Senders submit a `BridgeRequest`; a
//! `spawn_local` dispatcher performs the actual client call and answers via
//! oneshot.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol::{
    CreateTerminalRequest, KillTerminalRequest, PermissionOption, PermissionOptionId,
    PermissionOptionKind, ReleaseTerminalRequest, RequestPermissionOutcome,
    RequestPermissionRequest, SelectedPermissionOutcome, SessionId, TerminalId,
    TerminalOutputRequest, ToolCallUpdate, ToolCallUpdateFields, WaitForTerminalExitRequest,
};
use tack_session::SessionManager;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use super::SharedConn;
use super::convert::tool_kind;

/// The user's permission decision for a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionChoice {
    AllowOnce,
    AllowAlways,
    Denied,
}

pub struct PermissionQuery {
    pub tool_call_id: String,
    pub tool_name: String,
    pub title: String,
    pub raw_input: serde_json::Value,
    pub respond: oneshot::Sender<PermissionChoice>,
}

impl std::fmt::Debug for PermissionQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionQuery")
            .field("tool_name", &self.tool_name)
            .finish()
    }
}

/// Exit info from `terminal/wait_for_exit`.
#[derive(Clone, Copy, Debug)]
pub struct TerminalExitInfo {
    pub exit_code: Option<i32>,
}

/// Requests the local dispatcher can perform against the client.
#[derive(Debug)]
pub enum BridgeRequest {
    Permission(PermissionQuery),
    TerminalCreate {
        command: String,
        cwd: PathBuf,
        respond: oneshot::Sender<Result<String, String>>,
    },
    TerminalOutput {
        terminal_id: String,
        respond: oneshot::Sender<Result<String, String>>,
    },
    TerminalWaitExit {
        terminal_id: String,
        respond: oneshot::Sender<Result<TerminalExitInfo, String>>,
    },
    TerminalKill {
        terminal_id: String,
        respond: oneshot::Sender<()>,
    },
    TerminalRelease {
        terminal_id: String,
        respond: oneshot::Sender<()>,
    },
}

pub struct AcpSessionState {
    pub session: Arc<Mutex<SessionManager>>,
    /// Cancellation scope for the in-flight prompt turn; replaced each turn.
    pub cancel: Arc<std::sync::Mutex<CancellationToken>>,
    pub bridge: tokio::sync::mpsc::UnboundedSender<BridgeRequest>,
    /// Cached allow-always decisions: "tool" or "tool:first-arg-token".
    pub allow_always: Arc<std::sync::Mutex<HashSet<String>>>,
    /// MCP tools available in this session (from mcp.json + ACP mcpServers).
    pub mcp_tools: Vec<Arc<dyn tack_agent_core::AgentTool>>,
    /// Keep-alive handles: dropping a connection kills the server process.
    pub mcp_connections: Vec<Arc<tack_tools::mcp::McpConnection>>,
    /// Permission mode: "ask" (default), "auto" (no prompts), "plan" (read-only).
    pub mode: Arc<std::sync::Mutex<String>>,
    /// Model used for the next prompt turn (session/set_model).
    pub model: Arc<std::sync::Mutex<tack_ai::Model>>,
    /// Thinking level for the next prompt turn (config option).
    pub thinking: Arc<std::sync::Mutex<Option<tack_ai::ThinkingLevel>>>,
    /// tack-ext plugins loaded for this session's cwd (headless services:
    /// UI dialogs degrade, exec is trust-gated). Plugins stop with the
    /// process — ACP has no explicit session-close handshake.
    pub extensions: Arc<Mutex<crate::extension_host::ExtensionManager>>,
    /// In-flight prompt turn (F35): ACP clients (Zed et al.) send prompts
    /// serially, but the protocol does not enforce it — a concurrent
    /// session/prompt must be REJECTED, not queued: two agent loops on
    /// one SessionManager interleave file writes, and the second
    /// prompt's cancel token would replace the first's, leaving the
    /// first turn uncancellable.
    pub in_flight: Arc<std::sync::atomic::AtomicBool>,
}

/// RAII guard for the in-flight prompt turn (F35): `try_acquire` fails
/// while another prompt holds the session; drop releases it, so every
/// prompt() exit path frees the session for the next prompt.
pub struct InFlightGuard(Arc<std::sync::atomic::AtomicBool>);

impl InFlightGuard {
    pub fn try_acquire(flag: &Arc<std::sync::atomic::AtomicBool>) -> Option<Self> {
        if flag.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        Some(Self(flag.clone()))
    }
}

impl std::fmt::Debug for InFlightGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightGuard").finish()
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl std::fmt::Debug for AcpSessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpSessionState").finish_non_exhaustive()
    }
}

pub type Sessions = Rc<RefCell<HashMap<String, Rc<AcpSessionState>>>>;

/// Retained-output cap for client terminals (client truncates from the
/// beginning past this).
const TERMINAL_OUTPUT_BYTE_LIMIT: u64 = 8 * 1024 * 1024;

/// Create the session state and spawn the bridge dispatcher (must be called
/// from within the LocalSet).
pub fn create_session_state(
    session: SessionManager,
    shared_conn: SharedConn,
    session_id: SessionId,
    mcp_connections: Vec<Arc<tack_tools::mcp::McpConnection>>,
    model: tack_ai::Model,
    thinking: Option<tack_ai::ThinkingLevel>,
    extensions: crate::extension_host::ExtensionManager,
) -> Rc<AcpSessionState> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BridgeRequest>();

    tokio::task::spawn_local(async move {
        while let Some(request) = rx.recv().await {
            // Each request gets its own local task: a permission prompt can
            // wait minutes on a human, and a serial dispatcher would block
            // every terminal request queued behind it (the two channels
            // must stay independent).
            let shared_conn = shared_conn.clone();
            let session_id = session_id.clone();
            tokio::task::spawn_local(async move {
                dispatch(&shared_conn, &session_id, request).await;
            });
        }
    });

    let mcp_tools = tack_tools::mcp::mcp_tools(&mcp_connections);
    Rc::new(AcpSessionState {
        session: Arc::new(Mutex::new(session)),
        cancel: Arc::new(std::sync::Mutex::new(CancellationToken::new())),
        bridge: tx,
        allow_always: Arc::new(std::sync::Mutex::new(HashSet::new())),
        mcp_tools,
        mcp_connections,
        mode: Arc::new(std::sync::Mutex::new(DEFAULT_MODE.to_string())),
        model: Arc::new(std::sync::Mutex::new(model)),
        thinking: Arc::new(std::sync::Mutex::new(thinking)),
        extensions: Arc::new(Mutex::new(extensions)),
        in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    })
}

/// Default permission mode.
pub const DEFAULT_MODE: &str = "ask";

async fn dispatch(shared_conn: &SharedConn, session_id: &SessionId, request: BridgeRequest) {
    use agent_client_protocol::Client;
    let conn = shared_conn.borrow().clone();
    let Some(conn) = conn else { return };

    match request {
        BridgeRequest::Permission(query) => {
            let choice = dispatch_permission(&conn, session_id, &query).await;
            let _ = query.respond.send(choice);
        }
        BridgeRequest::TerminalCreate {
            command,
            cwd,
            respond,
        } => {
            let req = CreateTerminalRequest::new(session_id.clone(), command)
                .cwd(cwd)
                .output_byte_limit(TERMINAL_OUTPUT_BYTE_LIMIT);
            let result = conn
                .create_terminal(req)
                .await
                .map(|r| r.terminal_id.0.to_string())
                .map_err(|e| format!("terminal/create failed: {e}"));
            let _ = respond.send(result);
        }
        BridgeRequest::TerminalOutput {
            terminal_id,
            respond,
        } => {
            let req = TerminalOutputRequest::new(session_id.clone(), TerminalId::new(terminal_id));
            let result = conn
                .terminal_output(req)
                .await
                .map(|r| r.output)
                .map_err(|e| format!("terminal/output failed: {e}"));
            let _ = respond.send(result);
        }
        BridgeRequest::TerminalWaitExit {
            terminal_id,
            respond,
        } => {
            let req =
                WaitForTerminalExitRequest::new(session_id.clone(), TerminalId::new(terminal_id));
            let result = conn
                .wait_for_terminal_exit(req)
                .await
                .map(|r| TerminalExitInfo {
                    exit_code: r.exit_status.exit_code.map(|c| c as i32),
                })
                .map_err(|e| format!("terminal/wait_for_exit failed: {e}"));
            let _ = respond.send(result);
        }
        BridgeRequest::TerminalKill {
            terminal_id,
            respond,
        } => {
            let req = KillTerminalRequest::new(session_id.clone(), TerminalId::new(terminal_id));
            let _ = conn.kill_terminal(req).await;
            let _ = respond.send(());
        }
        BridgeRequest::TerminalRelease {
            terminal_id,
            respond,
        } => {
            let req = ReleaseTerminalRequest::new(session_id.clone(), TerminalId::new(terminal_id));
            let _ = conn.release_terminal(req).await;
            let _ = respond.send(());
        }
    }
}

async fn dispatch_permission(
    conn: &agent_client_protocol::AgentSideConnection,
    session_id: &SessionId,
    query: &PermissionQuery,
) -> PermissionChoice {
    use agent_client_protocol::Client;
    let options = vec![
        PermissionOption::new(
            PermissionOptionId::new("allow_once"),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::new("allow_always"),
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            PermissionOptionId::new("reject_once"),
            "Reject",
            PermissionOptionKind::RejectOnce,
        ),
    ];
    let request = RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(
            query.tool_call_id.clone(),
            ToolCallUpdateFields::new()
                .title(query.title.clone())
                .kind(tool_kind(&query.tool_name))
                .raw_input(query.raw_input.clone()),
        ),
        options,
    );

    match conn.request_permission(request).await {
        Ok(resp) => match resp.outcome {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
                if option_id.0.as_ref() == "allow_always" {
                    PermissionChoice::AllowAlways
                } else if option_id.0.starts_with("allow") {
                    PermissionChoice::AllowOnce
                } else {
                    PermissionChoice::Denied
                }
            }
            _ => PermissionChoice::Denied,
        },
        Err(e) => {
            tracing::warn!("permission request failed: {e}");
            PermissionChoice::Denied
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// F35: the in-flight guard admits exactly one prompt turn per
    /// session; dropping it frees the session for the next turn.
    #[test]
    fn in_flight_guard_excludes_concurrent_turns() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first = InFlightGuard::try_acquire(&flag).expect("first turn acquires");
        assert!(
            InFlightGuard::try_acquire(&flag).is_none(),
            "concurrent turn must be rejected"
        );
        drop(first);
        assert!(
            InFlightGuard::try_acquire(&flag).is_some(),
            "drop releases the session"
        );
    }
}
