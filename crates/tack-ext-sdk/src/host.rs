//! The plugin-side host client (`ui/*`, `exec/run`, `session/*`,
//! `snapshot/get`, `config/get`, …) and the handler context [`Cx`].

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{Error, PeerError};
use tack_ext::rpc3::{
    ExecRunParams, ExecRunResult, HostCapabilities, InitializeParams, LogLevel, RunMode,
    SendUserMessageParams, SessionInfo, Snapshot, UiConfirmParams, UiInputParams, UiNotifyParams,
    UiSelectParams, WarningParams, WidgetUpdateParams, method,
};
use tack_ext::v3::JsonRpcPeer;

/// State created when the plugin starts serving; `init`/`peer` are
/// filled during the handshake / transport setup.
#[derive(Debug, Default)]
pub(crate) struct State {
    pub init: OnceLock<InitializeParams>,
    pub peer: OnceLock<Arc<JsonRpcPeer>>,
    pub shutdown: tokio::sync::Notify,
}

/// Handler context: the negotiated host environment plus the host
/// client. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Cx {
    pub(crate) state: Arc<State>,
}

impl Cx {
    fn init(&self) -> &InitializeParams {
        self.state.init.get().expect("initialize completed")
    }

    /// The host run mode (`tui` | `print` | `rpc` | `acp`). A plugin must
    /// not depend on interactive requests for correctness in headless
    /// modes — check [`Cx::capabilities`] before calling dialogs.
    pub fn mode(&self) -> RunMode {
        self.init().mode
    }

    /// Project trust state (gates `exec` and other privileged services).
    pub fn trusted(&self) -> bool {
        self.init().trusted
    }

    pub fn cwd(&self) -> &str {
        &self.init().cwd
    }

    /// What the host supports in this mode (widgets, dialogs, exec, …).
    pub fn capabilities(&self) -> &HostCapabilities {
        &self.init().capabilities
    }

    /// The effective per-plugin config, host-validated against the
    /// declared config schema (`None` when the plugin declared none).
    pub fn config(&self) -> Option<&Value> {
        self.init().config.as_ref()
    }

    /// The host client.
    pub fn host(&self) -> Host {
        Host {
            state: self.state.clone(),
        }
    }
}

/// Plugin → host typed client. All calls honor the peer's dead/timeout
/// semantics; mode-gated surfaces fail deterministically in headless
/// modes (see the protocol's degradation contract).
#[derive(Clone, Debug)]
pub struct Host {
    pub(crate) state: Arc<State>,
}

impl Host {
    fn peer(&self) -> &Arc<JsonRpcPeer> {
        self.state.peer.get().expect("plugin is serving")
    }

    async fn call<P, R>(&self, rpc_method: &str, params: &P) -> Result<R, PeerError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let params =
            serde_json::to_value(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        let result = self.peer().call(rpc_method, params).await?;
        serde_json::from_value(result).map_err(|e| PeerError::Transport(e.to_string()))
    }

    async fn call_unit<P: Serialize>(&self, rpc_method: &str, params: &P) -> Result<(), PeerError> {
        let params =
            serde_json::to_value(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        self.peer().call(rpc_method, params).await?;
        Ok(())
    }

    /// `ui/notify` (headless modes degrade to a log line).
    pub async fn notify(
        &self,
        message: impl Into<String>,
        level: Option<LogLevel>,
    ) -> Result<(), PeerError> {
        self.call_unit(
            method::UI_NOTIFY,
            &UiNotifyParams {
                message: message.into(),
                level,
            },
        )
        .await
    }

    /// `ui/select`; `None` = dismissed. Errors in headless modes.
    pub async fn select(
        &self,
        title: impl Into<String>,
        options: Vec<String>,
    ) -> Result<Option<String>, PeerError> {
        self.call(
            method::UI_SELECT,
            &UiSelectParams {
                title: title.into(),
                options,
            },
        )
        .await
    }

    /// `ui/confirm`. Errors in headless modes.
    pub async fn confirm(
        &self,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<bool, PeerError> {
        self.call(
            method::UI_CONFIRM,
            &UiConfirmParams {
                title: title.into(),
                message: message.into(),
            },
        )
        .await
    }

    /// `ui/input`; `None` = cancelled. Errors in headless modes.
    pub async fn input(
        &self,
        title: impl Into<String>,
        placeholder: Option<String>,
    ) -> Result<Option<String>, PeerError> {
        self.call(
            method::UI_INPUT,
            &UiInputParams {
                title: title.into(),
                placeholder,
            },
        )
        .await
    }

    /// `exec/run` — trust-gated; untrusted contexts fail with
    /// `ERR_POLICY_DENIED`.
    pub async fn exec(
        &self,
        command: impl Into<String>,
        timeout: Option<Duration>,
    ) -> Result<ExecRunResult, PeerError> {
        self.call(
            method::EXEC_RUN,
            &ExecRunParams {
                command: command.into(),
                timeout_ms: timeout.map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
            },
        )
        .await
    }

    /// `logs/emit` notification (diagnostic channel).
    pub async fn log(&self, level: LogLevel, message: impl Into<String>) -> Result<(), PeerError> {
        let params = serde_json::to_value(tack_ext::rpc3::LogParams {
            level: Some(level),
            message: message.into(),
        })
        .map_err(|e| PeerError::Transport(e.to_string()))?;
        self.peer().notify(method::LOGS_EMIT, params).await
    }

    /// `warnings/emit` notification (structured user-facing warning).
    pub async fn warn(&self, message: impl Into<String>) -> Result<(), PeerError> {
        let params = serde_json::to_value(WarningParams {
            message: message.into(),
            context: None,
        })
        .map_err(|e| PeerError::Transport(e.to_string()))?;
        self.peer().notify(method::WARNINGS_EMIT, params).await
    }

    /// `session/get`.
    pub async fn session(&self) -> Result<SessionInfo, PeerError> {
        self.call(method::SESSION_GET, &Value::Null).await
    }

    /// `session/sendUserMessage` (trust/mode gated).
    pub async fn send_user_message(&self, text: impl Into<String>) -> Result<(), PeerError> {
        self.call_unit(
            method::SESSION_SEND_USER_MESSAGE,
            &SendUserMessageParams { text: text.into() },
        )
        .await
    }

    /// `snapshot/get` (versioned read-only digest).
    pub async fn snapshot(&self) -> Result<Snapshot, PeerError> {
        self.call(method::SNAPSHOT_GET, &Value::Null).await
    }

    /// `config/get` — the effective per-plugin config. Prefer
    /// [`Cx::config`] (delivered at initialize, no round trip).
    pub async fn config(&self) -> Result<Value, PeerError> {
        let result: tack_ext::rpc3::ConfigResult =
            self.call(method::CONFIG_GET, &Value::Null).await?;
        Ok(result.config)
    }

    /// `host/registerProvider` (trust/mode gated).
    pub async fn register_provider(&self, provider: Value) -> Result<(), PeerError> {
        self.call_unit(
            method::HOST_REGISTER_PROVIDER,
            &tack_ext::rpc3::RegisterProviderParams { provider },
        )
        .await
    }

    /// `widgets/update` notification: idempotent full-state replacement.
    pub async fn widget_update(&self, update: WidgetUpdateParams) -> Result<(), PeerError> {
        let params =
            serde_json::to_value(update).map_err(|e| PeerError::Transport(e.to_string()))?;
        self.peer().notify(method::WIDGETS_UPDATE, params).await
    }
}

/// Error helper for handlers that need to fail a host call with context.
impl From<PeerError> for Error {
    fn from(error: PeerError) -> Self {
        Error {
            code: error.code(),
            message: error.to_string(),
            data: None,
        }
    }
}
