//! Process carrier for tack-RPC v3 plugins: spawn a child, wire a
//! [`JsonRpcPeer`] over its stdio, forward stderr to tracing, and tear
//! down with a grace period. Mirrors the v1 `PluginProcess` hardening
//! (sensitive env stripping, kill on drop, bounded stderr draining).

use std::sync::Arc;
use std::time::Duration;

use tokio::io::BufReader;
use tokio::process::Child;

use super::host::HostClient;
use super::peer::{JsonRpcPeer, PeerHandler};
use crate::process::{OverCap, env_vars_to_strip, read_line_bounded};

/// A v3 plugin child process plus its typed client. Dropping kills the
/// child (`kill_on_drop`).
pub struct V3Process {
    /// Typed host → plugin calls (handshake, tools, hooks, …).
    pub client: HostClient,
    /// The raw peer (liveness, cancellation, `wait_dead`).
    pub peer: Arc<JsonRpcPeer>,
    child: Child,
}

impl std::fmt::Debug for V3Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V3Process")
            .field("alive", &self.peer.is_alive())
            .finish()
    }
}

impl V3Process {
    /// Spawn `program` with piped stdio. Requests arriving from the
    /// plugin (ui/exec/session/… services) are dispatched to `handler`;
    /// stderr is forwarded to tracing with the plugin-stderr target.
    pub async fn spawn(
        program: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &std::path::Path,
        handler: Arc<dyn PeerHandler>,
    ) -> Result<Self, String> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        // Strip credentials from the inherited environment AFTER applying
        // the manifest's explicit env (see the v1 carrier for rationale).
        let parent_env: Vec<(String, String)> = std::env::vars_os()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        for key in env_vars_to_strip(&parent_env, env) {
            command.env_remove(&key);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn plugin {program}: {e}"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                match read_line_bounded(&mut reader, &mut buf, OverCap::Discard).await {
                    Ok(Some(line)) => {
                        // Line content is debug-only: a chatty plugin
                        // must not flood the host's info-level logs.
                        tracing::debug!(target: "tack_ext::plugin_stderr", "{line}")
                    }
                    Ok(None) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                        tracing::warn!(target: "tack_ext::plugin_stderr", "oversized stderr line dropped");
                    }
                    Err(e) => {
                        tracing::warn!(target: "tack_ext::plugin_stderr", "stderr read failed: {e}");
                        break;
                    }
                }
            }
        });
        let peer = JsonRpcPeer::new(stdout, stdin, handler);
        Ok(V3Process {
            client: HostClient::new(peer.clone()),
            peer,
            child,
        })
    }

    /// Graceful shutdown: `shutdown` request (short-bounded — see
    /// `HostClient::shutdown`, so a hung plugin cannot stall teardown
    /// for the default 30s call timeout), 2s grace, then force kill.
    pub async fn shutdown(&mut self) {
        let _ = self.client.shutdown().await;
        let wait = tokio::time::timeout(Duration::from_secs(2), self.child.wait());
        if wait.await.is_err() {
            let _ = self.child.kill().await;
        }
    }
}
