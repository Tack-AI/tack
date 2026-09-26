//! Headless `HostServices` for non-TUI run modes (print / rpc / acp).
//!
//! Plugins keep working in headless modes — tools, intercepts, lifecycle
//! events, `exec` (trust-gated) — but anything that needs a terminal UI is
//! degraded deterministically:
//!
//! - `ui.notify` / `ui.set_status`: accepted, forwarded to the tracing log
//!   (fire-and-forget semantics preserved);
//! - `ui.select` / `ui.confirm` / `ui.input`: error — there is no user to
//!   ask. Plugins must treat these as optional capabilities (the
//!   `initialize` payload's `mode` field tells them which mode hosts them);
//! - `session.*` / `provider.register`: error in v1 headless support —
//!   session control needs a live session UI/owner;
//! - `exec`: honored when the context is trusted, run inline (no TUI main
//!   loop to route through) with the same shell + timeout semantics as the
//!   TUI path (`run_ext_exec` is shared).

use std::sync::Arc;

use serde_json::Value;
use tack_ext::HostServices;

/// Plugin `exec` request via the platform shell with a timeout. Shared by
/// the TUI bridge and the headless services.
pub(crate) async fn run_ext_exec(
    params: &tack_ext::protocol::ExecParams,
) -> tack_ext::protocol::ExecResult {
    use tokio::io::AsyncReadExt as _;
    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/c".to_string(), params.command.clone()])
    } else {
        ("sh", vec!["-c".to_string(), params.command.clone()])
    };
    let timeout = std::time::Duration::from_millis(params.timeout_ms.unwrap_or(120_000));
    let mut child = match tokio::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return tack_ext::protocol::ExecResult {
                stdout: String::new(),
                stderr: e.to_string(),
                code: -1,
            };
        }
    };
    let pid = child.id().unwrap_or(0);
    // Drain both pipes concurrently: a child that fills a pipe buffer
    // blocks on write and would otherwise hang until the timeout.
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => tack_ext::protocol::ExecResult {
            stdout: String::from_utf8_lossy(&out_task.await.unwrap_or_default()).to_string(),
            stderr: String::from_utf8_lossy(&err_task.await.unwrap_or_default()).to_string(),
            code: status.code().unwrap_or(-1),
        },
        Ok(Err(e)) => tack_ext::protocol::ExecResult {
            stdout: String::new(),
            stderr: e.to_string(),
            code: -1,
        },
        Err(_) => {
            // Timeout: a dropped wait-future leaves the child RUNNING. Kill
            // the whole tree (the shell may have spawned grandchildren),
            // then wait to reap it — no stray plugin processes, no zombie.
            tack_tools::shell::kill_process_tree(pid);
            let _ = child.wait().await;
            tack_ext::protocol::ExecResult {
                stdout: String::new(),
                stderr: format!("exec timed out after {}ms", timeout.as_millis()),
                code: -1,
            }
        }
    }
}

/// Headless host services: no terminal, no session owner. `mode` is the
/// run-mode label ("print" | "rpc" | "acp") used in error messages.
pub struct HeadlessExtServices {
    mode: &'static str,
    /// Project trust: `exec` is only honored for trusted contexts.
    trusted: bool,
}

impl std::fmt::Debug for HeadlessExtServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeadlessExtServices")
            .field("mode", &self.mode)
            .field("trusted", &self.trusted)
            .finish()
    }
}

impl HeadlessExtServices {
    pub fn new(mode: &'static str, trusted: bool) -> Arc<Self> {
        Arc::new(HeadlessExtServices { mode, trusted })
    }
}

#[async_trait::async_trait]
impl HostServices for HeadlessExtServices {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            // Fire-and-forget UI primitives degrade to log lines.
            "ui.notify" => {
                let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                tracing::info!(target: "tack_ext::plugin", "notify: {message}");
                Ok(Value::Null)
            }
            "ui.set_status" => Ok(Value::Null),
            "ui.select" | "ui.confirm" | "ui.input" => Err(format!(
                "{method} needs an interactive terminal; not available in {} mode",
                self.mode
            )),
            "exec" => {
                if !self.trusted {
                    return Err("exec requires project trust".to_string());
                }
                match serde_json::from_value::<tack_ext::protocol::ExecParams>(params) {
                    Ok(parsed) => {
                        let result = run_ext_exec(&parsed).await;
                        serde_json::to_value(result).map_err(|e| e.to_string())
                    }
                    Err(_) => Err("bad exec params".to_string()),
                }
            }
            other if other.starts_with("session.") => Err(format!(
                "{other} needs a live session UI; not available in {} mode",
                self.mode
            )),
            "provider.register" => Err(format!(
                "provider.register is not available in {} mode",
                self.mode
            )),
            other => Err(format!("unknown host method {other}")),
        }
    }

    async fn handle_event(&self, event: &str, payload: Value) {
        if event == "log" {
            let level = payload
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("info");
            let message = payload.get("message").and_then(Value::as_str).unwrap_or("");
            match level {
                "error" => tracing::error!(target: "tack_ext::plugin", "{message}"),
                "warn" => tracing::warn!(target: "tack_ext::plugin", "{message}"),
                "debug" => tracing::debug!(target: "tack_ext::plugin", "{message}"),
                _ => tracing::info!(target: "tack_ext::plugin", "{message}"),
            }
        }
        // widget.update: headless modes accept and ignore widget state (the
        // protocol contract — widgets are best-effort UI, never load-bearing).
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn exec_requires_trust() {
        let services = HeadlessExtServices::new("print", false);
        let err = services
            .handle_request("exec", serde_json::json!({"command": "echo hi"}))
            .await
            .unwrap_err();
        assert!(err.contains("trust"), "{err}");
    }

    #[tokio::test]
    async fn exec_runs_inline_when_trusted() {
        let services = HeadlessExtServices::new("print", true);
        let value = services
            .handle_request(
                "exec",
                serde_json::json!({"command": "echo hi", "timeout_ms": 5000}),
            )
            .await
            .unwrap();
        assert_eq!(value["code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("hi"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_timeout_kills_the_child() {
        // F19: a timed-out exec must not leave the shell (or its children)
        // running — the timeout branch kills the process tree and reaps it.
        let services = HeadlessExtServices::new("print", true);
        let started = std::time::Instant::now();
        let value = services
            .handle_request(
                "exec",
                serde_json::json!({"command": "sleep 60", "timeout_ms": 300}),
            )
            .await
            .unwrap();
        assert_eq!(value["code"], -1);
        assert!(
            value["stderr"].as_str().unwrap().contains("timed out"),
            "{value}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "timeout must return promptly (kill + reap), not after the command"
        );
    }

    #[tokio::test]
    async fn interactive_ui_is_an_explicit_error() {
        let services = HeadlessExtServices::new("rpc", true);
        for method in ["ui.select", "ui.confirm", "ui.input"] {
            let err = services
                .handle_request(method, serde_json::json!({}))
                .await
                .unwrap_err();
            assert!(err.contains("rpc mode"), "{method}: {err}");
        }
        // notify / set_status degrade silently.
        services
            .handle_request("ui.notify", serde_json::json!({"message": "hi"}))
            .await
            .unwrap();
        services
            .handle_request("ui.set_status", serde_json::json!({"text": "x"}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn session_control_and_provider_register_are_errors() {
        let services = HeadlessExtServices::new("acp", true);
        let err = services
            .handle_request(
                "session.send_user_message",
                serde_json::json!({"text": "x"}),
            )
            .await
            .unwrap_err();
        assert!(err.contains("acp mode"), "{err}");
        let err = services
            .handle_request("provider.register", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("acp mode"), "{err}");
    }
}
