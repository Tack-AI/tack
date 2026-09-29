//! Headless host services for non-TUI run modes (print / rpc / acp),
//! implementing the v3 [`PeerHandler`] surface.
//!
//! Plugins keep working in headless modes — tools, interception,
//! lifecycle events, `exec/run` (trust-gated) — but anything that needs a
//! terminal UI is degraded deterministically:
//!
//! - `ui/notify`: accepted, forwarded to the tracing log;
//! - `ui/select` / `ui/confirm` / `ui/input`: `ERR_CAPABILITY_NOT_GRANTED`
//!   — there is no user to ask (the initialize payload's `mode` and
//!   `capabilities` tell the plugin);
//! - `host/registerProvider`: honored — provider registration is
//!   mode-independent (it writes the process-global runtime registry that
//!   every mode's model resolution reads); `bridge: true` additionally
//!   wires the plugin connection as the serving endpoint;
//! - `session/*`, `snapshot/get`: not available in headless modes —
//!   session control needs a live session UI/owner;
//! - `exec/run`: honored when the context is trusted, run inline with the
//!   same shell + timeout semantics as the TUI path (`run_ext_exec` is
//!   shared).

use std::sync::Arc;

use serde_json::Value;
use tack_ext::rpc3::{
    ERR_CAPABILITY_NOT_GRANTED, ERR_METHOD_NOT_FOUND, ERR_POLICY_DENIED, ErrorObject,
};
use tack_ext::v3::PeerHandler;

fn service_error(code: i64, message: impl Into<String>) -> ErrorObject {
    ErrorObject {
        code,
        message: message.into(),
        data: None,
    }
}

/// Plugin `exec/run` request via the platform shell with a timeout.
/// Shared by the TUI bridge and the headless services.
pub(crate) async fn run_ext_exec(
    params: &tack_ext::rpc3::ExecRunParams,
) -> tack_ext::rpc3::ExecRunResult {
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
            return tack_ext::rpc3::ExecRunResult {
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
        Ok(Ok(status)) => tack_ext::rpc3::ExecRunResult {
            stdout: String::from_utf8_lossy(&out_task.await.unwrap_or_default()).to_string(),
            stderr: String::from_utf8_lossy(&err_task.await.unwrap_or_default()).to_string(),
            code: status.code().unwrap_or(-1),
        },
        Ok(Err(e)) => tack_ext::rpc3::ExecRunResult {
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
            tack_ext::rpc3::ExecRunResult {
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
    /// Project trust: `exec/run` is only honored for trusted contexts.
    trusted: bool,
    /// Provider bridge state (connections, stream sinks, registrations).
    bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
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
    pub fn new(
        mode: &'static str,
        trusted: bool,
        bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
    ) -> Arc<Self> {
        Arc::new(HeadlessExtServices {
            mode,
            trusted,
            bridge_state,
        })
    }
}

#[async_trait::async_trait]
impl PeerHandler for HeadlessExtServices {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, ErrorObject> {
        match method {
            // Fire-and-forget UI primitives degrade to log lines.
            "ui/notify" => {
                let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                tracing::info!(target: "tack_ext::plugin", "notify: {message}");
                Ok(Value::Null)
            }
            "ui/select" | "ui/confirm" | "ui/input" => Err(service_error(
                ERR_CAPABILITY_NOT_GRANTED,
                format!(
                    "{method} needs an interactive terminal; not available in {} mode",
                    self.mode
                ),
            )),
            "exec/run" => {
                if !self.trusted {
                    return Err(service_error(
                        ERR_POLICY_DENIED,
                        "exec requires project trust",
                    ));
                }
                match serde_json::from_value::<tack_ext::rpc3::ExecRunParams>(params) {
                    Ok(parsed) => {
                        let result = run_ext_exec(&parsed).await;
                        serde_json::to_value(result)
                            .map_err(|e| service_error(ERR_CAPABILITY_NOT_GRANTED, e.to_string()))
                    }
                    Err(e) => Err(service_error(
                        tack_ext::rpc3::ERR_INVALID_PARAMS,
                        format!("bad exec params: {e}"),
                    )),
                }
            }
            "host/registerProvider" => {
                crate::ext_provider_bridge::handle_register_provider(&self.bridge_state, params)
                    .await
            }
            other => Err(service_error(
                ERR_METHOD_NOT_FOUND,
                format!("{other} is not available in {} mode", self.mode),
            )),
        }
    }

    async fn handle_notification(&self, method: &str, payload: Value) {
        match method {
            "logs/emit" => {
                let level = payload
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info");
                let message = payload.get("message").and_then(Value::as_str).unwrap_or("");
                match level {
                    "error" => tracing::error!(target: "tack_ext::plugin", "{message}"),
                    "warn" | "warning" => tracing::warn!(target: "tack_ext::plugin", "{message}"),
                    "debug" => tracing::debug!(target: "tack_ext::plugin", "{message}"),
                    _ => tracing::info!(target: "tack_ext::plugin", "{message}"),
                }
            }
            "warnings/emit" => {
                let message = payload.get("message").and_then(Value::as_str).unwrap_or("");
                tracing::warn!(target: "tack_ext::plugin", "plugin warning: {message}");
            }
            // widgets/update: headless modes accept and ignore widget
            // state (widgets are best-effort UI, never load-bearing).
            "provider/streamEvent" => {
                let plugin = payload
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.bridge_state.route_stream_event(&plugin, payload);
            }
            "provider/event" => {
                let plugin = payload
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.bridge_state.route_provider_event(&plugin, payload);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn exec_requires_trust() {
        let services = HeadlessExtServices::new(
            "print",
            false,
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        );
        let err = services
            .handle_request("exec/run", serde_json::json!({"command": "echo hi"}))
            .await
            .unwrap_err();
        assert_eq!(err.code, ERR_POLICY_DENIED);
    }

    #[tokio::test]
    async fn exec_runs_inline_when_trusted() {
        let services = HeadlessExtServices::new(
            "print",
            true,
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        );
        let value = services
            .handle_request(
                "exec/run",
                serde_json::json!({"command": "echo hi", "timeoutMs": 5000}),
            )
            .await
            .unwrap();
        assert_eq!(value["code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("hi"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_timeout_kills_the_child() {
        // A timed-out exec must not leave the shell (or its children)
        // running — the timeout branch kills the process tree and reaps it.
        let services = HeadlessExtServices::new(
            "print",
            true,
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        );
        let started = std::time::Instant::now();
        let value = services
            .handle_request(
                "exec/run",
                serde_json::json!({"command": "sleep 60", "timeoutMs": 300}),
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
    async fn interactive_ui_is_capability_not_granted() {
        let services = HeadlessExtServices::new(
            "rpc",
            true,
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        );
        for method in ["ui/select", "ui/confirm", "ui/input"] {
            let err = services
                .handle_request(method, serde_json::json!({}))
                .await
                .unwrap_err();
            assert_eq!(err.code, ERR_CAPABILITY_NOT_GRANTED, "{method}");
            assert!(err.message.contains("rpc mode"), "{method}: {err:?}");
        }
        // notify degrades silently.
        services
            .handle_request("ui/notify", serde_json::json!({"message": "hi"}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn session_control_is_method_not_found() {
        let services = HeadlessExtServices::new(
            "acp",
            true,
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        );
        let err = services
            .handle_request("session/sendUserMessage", serde_json::json!({"text": "x"}))
            .await
            .unwrap_err();
        assert_eq!(err.code, ERR_METHOD_NOT_FOUND);
        assert!(err.message.contains("acp mode"), "{err:?}");
        let err = services
            .handle_request("snapshot/get", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code, ERR_METHOD_NOT_FOUND);
    }

    /// P7a: provider registration is mode-independent — headless modes
    /// honor `host/registerProvider` and the provider resolves in the
    /// process-global registry like a native one.
    #[tokio::test]
    async fn register_provider_is_honored_headless() {
        let services = HeadlessExtServices::new(
            "print",
            true,
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        );
        let spec = serde_json::json!({
            "provider": {
                "id": "headless-shim",
                "baseUrl": "http://localhost:9/v1",
                "api": "openai-completions",
                "models": [{"id": "shim-model"}]
            }
        });
        services
            .handle_request("host/registerProvider", spec)
            .await
            .unwrap();
        let registered = tack_ai::providers::runtime_providers();
        assert!(
            registered.iter().any(|p| p.id == "headless-shim"),
            "registered providers: {:?}",
            registered.iter().map(|p| &p.id).collect::<Vec<_>>()
        );
        // Model resolution reads the same registry (P7a's whole point).
        let model = crate::model::resolve_model(
            "headless-shim",
            Some("shim-model"),
            std::path::Path::new("/nonexistent-agent-dir"),
        )
        .unwrap();
        assert_eq!(model.api, "openai-completions");
        tack_ai::providers::unregister_runtime_provider("headless-shim");
    }

    /// A bridge registration without a live plugin connection and without
    /// the declared capability is rejected (capability gating).
    #[tokio::test]
    async fn register_bridge_provider_requires_a_serving_plugin() {
        let services = HeadlessExtServices::new(
            "rpc",
            true,
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        );
        let spec = serde_json::json!({
            "plugin": "ghost@user",
            "provider": {"id": "ghost-bridge", "bridge": true, "models": [{"id": "m"}]}
        });
        let err = services
            .handle_request("host/registerProvider", spec)
            .await
            .unwrap_err();
        assert_eq!(err.code, tack_ext::rpc3::ERR_PLUGIN_UNAVAILABLE);
    }
}
