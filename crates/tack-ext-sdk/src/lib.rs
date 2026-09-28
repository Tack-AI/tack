//! Rust SDK for tack-RPC v3 plugins (Level 3 of the plugin model in
//! `docs/plugin-roadmap.md`).
//!
//! A plugin is a [`Plugin`] built from handlers; [`Plugin::run`] serves
//! it over stdio. The SDK owns framing, request/response correlation,
//! cancellation, timeouts, and version negotiation — plugin code only
//! sees typed params and results:
//!
//! ```no_run
//! use tack_ext_sdk::{Plugin, ToolSpec, text_output};
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     Plugin::builder("hello")
//!         .tool(
//!             ToolSpec {
//!                 name: "hello.echo".to_string(),
//!                 label: None,
//!                 description: "Echo the arguments".to_string(),
//!                 parameters: serde_json::json!({"type": "object"}),
//!             },
//!             |params, _cx| async move {
//!                 Ok(text_output(format!("echo: {}", params.arguments)))
//!             },
//!         )
//!         .run()
//!         .await
//! }
//! ```
//!
//! Protocol note: stdout is the RPC bus — never `println!` from a
//! plugin; use [`Host::log`] / [`Host::warn`] instead.

mod host;
mod plugin;

pub use host::{Cx, Host};
pub use plugin::{Plugin, PluginBuilder};

pub use tack_ext::rpc3; // generated protocol types
pub use tack_ext::rpc3::{
    AfterToolCallParams, AfterToolCallPatch, ApprovalDecision, ApprovalDecisionAction,
    ApprovalReviewParams, AutocompleteProvideParams, AutocompleteProvideResult,
    AutocompleteProviderSpec, AutocompleteSuggestion, BeforeToolCallParams, CommandInvokeParams,
    CommandSpec, ConfigDeclaration, ContentBlock, ContentBlockKind, ExecRunResult,
    HookCapabilities, HostCapabilities, InitializeParams, LifecycleEventParams, LogLevel,
    MetricOperation, MetricsDeclaration, PluginCapabilities, PluginInfo, RunMode, SessionInfo,
    Snapshot, ToolCall, ToolExecuteParams, ToolOutput, ToolSpec, TransformContextParams,
    TransformContextResult, UiInputParams, UiSelectParams, Verdict, VerdictAction,
    WidgetActionParams, WidgetKind, WidgetSpec, WidgetUpdateParams,
};
pub use tack_ext::v3::PeerError;

use serde_json::Value;
use tack_ext::rpc3::{ERR_INTERNAL, ERR_INVALID_PARAMS, ErrorObject};

/// The tack-RPC protocol version this SDK speaks.
pub const PROTOCOL_VERSION: &str = tack_ext::v3::PROTOCOL_VERSION;

/// A handler failure, serialized as a JSON-RPC error object. Domain
/// codes (`rpc3::ERR_*`) survive the trip to the host.
#[derive(Debug, Clone)]
pub struct Error {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl Error {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// `ERR_INVALID_PARAMS` (-32602): the host sent malformed arguments.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Error::new(ERR_INVALID_PARAMS, message)
    }

    /// `ERR_INTERNAL` (-32603): the handler itself failed.
    pub fn internal(message: impl Into<String>) -> Self {
        Error::new(ERR_INTERNAL, message)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

impl From<Error> for ErrorObject {
    fn from(error: Error) -> Self {
        ErrorObject {
            code: error.code,
            message: error.message,
            data: error.data,
        }
    }
}

impl From<ErrorObject> for Error {
    fn from(error: ErrorObject) -> Self {
        Error {
            code: error.code,
            message: error.message,
            data: error.data,
        }
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Error::internal(message)
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Error::internal(message)
    }
}

// ---------------------------------------------------------------------------
// Convenience constructors for the generated (helper-free) types
// ---------------------------------------------------------------------------

/// A plain-text content block.
pub fn text_block(text: impl Into<String>) -> ContentBlock {
    ContentBlock {
        r#type: ContentBlockKind::Text,
        text: Some(text.into()),
        mime_type: None,
        data: None,
    }
}

/// A plain-text tool output.
pub fn text_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![text_block(text)],
        details: None,
        is_error: None,
    }
}

/// An error tool output (fed back to the model as a failure).
pub fn error_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![text_block(text)],
        details: None,
        is_error: Some(true),
    }
}

/// `allow` verdict (before_tool_call).
pub fn allow() -> Verdict {
    Verdict {
        action: VerdictAction::Allow,
        reason: None,
        arguments: None,
    }
}

/// `deny` verdict; the reason becomes the error tool result.
pub fn deny(reason: impl Into<String>) -> Verdict {
    Verdict {
        action: VerdictAction::Deny,
        reason: Some(reason.into()),
        arguments: None,
    }
}

/// `rewrite` verdict with the full replacement arguments.
pub fn rewrite(arguments: Value) -> Verdict {
    Verdict {
        action: VerdictAction::Rewrite,
        reason: None,
        arguments: Some(arguments),
    }
}
