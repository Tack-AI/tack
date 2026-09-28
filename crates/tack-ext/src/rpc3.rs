//! tack-RPC v3 types — GENERATED from `protocol/tack-rpc.openrpc.json`
//! by `cargo run -p xtask -- codegen`. Do not edit by hand.
#![allow(clippy::doc_markdown)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only legal `jsonrpc` field value.
pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC 2.0 error codes.
pub const ERR_PARSE: i64 = -32700;
pub const ERR_INVALID_REQUEST: i64 = -32600;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_INVALID_PARAMS: i64 = -32602;
pub const ERR_INTERNAL: i64 = -32603;

// tack-RPC domain error codes (components.errors in the schema).
/// Denied by host policy (for example exec in an untrusted context).
pub const ERR_POLICY_DENIED: i64 = -32001;
/// The plugin did not declare / was not granted this capability.
pub const ERR_CAPABILITY_NOT_GRANTED: i64 = -32002;
/// The plugin carrier is dead or unreachable.
pub const ERR_PLUGIN_UNAVAILABLE: i64 = -32003;
/// The request exceeded the host's call timeout.
pub const ERR_REQUEST_TIMEOUT: i64 = -32004;

/// A JSON-RPC request/response id (per-sender numbering).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric id (the common case).
    Num(u64),
    /// String id (tolerated for JSON-RPC compliance).
    Str(String),
}

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Id::Num(value)
    }
}

/// A JSON-RPC 2.0 request (either direction; both peers may issue).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// Build a request with a typed params payload.
    pub fn new(id: impl Into<Id>, method: impl Into<String>, params: Value) -> Self {
        Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params: Some(params),
        }
    }
}

/// A JSON-RPC 2.0 notification (no id, no response).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    /// Build a notification with a typed params payload.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Notification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params: Some(params),
        }
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC 2.0 response. `id` is null when the request could not be
/// parsed; exactly one of `result`/`error` is present in a valid response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    /// A success response.
    pub fn result(id: Option<Id>, result: Value) -> Self {
        Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error response.
    pub fn error(id: Option<Id>, code: i64, message: impl Into<String>) -> Self {
        Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(ErrorObject {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Wire method names (see the OpenRPC document for contracts).
pub mod method {
    /// \[host-to-plugin\] Lifecycle handshake, host -\> plugin, sent immediately after spawn. The host advertises its capabilities and the validated per-plugin config; the plugin answers with its identity and its (all-optional) capability contributions. A protocolVersion outside the host's supported range is a clean handshake error, not a silent degradation.
    pub const INITIALIZE: &str = "initialize";

    /// \[host-to-plugin\] Graceful stop, host -\> plugin. The plugin should finish in-flight work and exit; the host kills the carrier after a grace period.
    pub const SHUTDOWN: &str = "shutdown";

    /// \[host-to-plugin\] Run a plugin-contributed tool. The call carries the provider-issued toolCallId so the plugin can correlate progress/cancellation.
    pub const TOOLS_EXECUTE: &str = "tools/execute";

    /// \[host-to-plugin\] Run a plugin-registered slash command.
    pub const COMMANDS_INVOKE: &str = "commands/invoke";

    /// \[host-to-plugin\] A tool call is about to run; the plugin may allow, deny (reason becomes the error tool result), or rewrite the arguments. Chained plugins observe the previous plugin's rewrite; the first deny short-circuits.
    pub const HOOKS_BEFORE_TOOL_CALL: &str = "hooks/beforeToolCall";

    /// \[host-to-plugin\] COW context pipeline (opt-in via capabilities.hooks.transformContext): a null result means unchanged; a returned list replaces the messages every later hook sees.
    pub const HOOKS_TRANSFORM_CONTEXT: &str = "hooks/transformContext";

    /// \[host-to-plugin\] Observe and patch a tool result (opt-in via capabilities.hooks.afterToolCall). Patches merge in chain order; later plugins win per field.
    pub const HOOKS_AFTER_TOOL_CALL: &str = "hooks/afterToolCall";

    /// \[host-to-plugin\] An approval decision is needed (opt-in via capabilities.hooks.approvalReview). A null result passes to the next reviewer in the chain; a returned decision claims the approval (first-claim-wins).
    pub const APPROVAL_REVIEW: &str = "approval/review";

    /// \[host-to-plugin\] Query an autocomplete provider for input-line suggestions. Timeouts and cancellations degrade to no suggestions.
    pub const AUTOCOMPLETE_PROVIDE: &str = "autocomplete/provide";

    /// \[host-to-plugin\] Lifecycle notification, subscription-gated via capabilities.events. The event name is one of the well-known lifecycle set (sessionStart, sessionShutdown, agentStart, agentEnd, turnStart, turnEnd, messageStart, messageEnd, toolExecutionStart, toolExecutionEnd, modelSelect, thinkingLevelSelect); the payload shape depends on the event.
    pub const EVENTS_LIFECYCLE: &str = "events/lifecycle";

    /// \[host-to-plugin\] User interaction with a declared widget (never produced in headless modes). Sent only to the owning plugin.
    pub const WIDGETS_ACTION: &str = "widgets/action";

    /// \[plugin-to-host\] Idempotent full-state replacement for a declared widget (not a diff; dropped frames are harmless).
    pub const WIDGETS_UPDATE: &str = "widgets/update";

    /// \[plugin-to-host\] Read current session state (trust/mode gated).
    pub const SESSION_GET: &str = "session/get";

    /// \[plugin-to-host\] Inject a user message into the session (trust/mode gated).
    pub const SESSION_SEND_USER_MESSAGE: &str = "session/sendUserMessage";

    /// \[plugin-to-host\] Versioned read-only session digest. historyVersion and compactionRevision let the plugin detect staleness between calls without re-fetching.
    pub const SNAPSHOT_GET: &str = "snapshot/get";

    /// \[plugin-to-host\] Read the plugin's effective configuration (host-validated against the declared config schema; invalid user config degrades to defaults with a load warning).
    pub const CONFIG_GET: &str = "config/get";

    /// \[plugin-to-host\] Show a notification. Headless modes degrade to a log line.
    pub const UI_NOTIFY: &str = "ui/notify";

    /// \[plugin-to-host\] Ask the user to pick one option. Headless modes answer with an error (see host capabilities). A null result means dismissed.
    pub const UI_SELECT: &str = "ui/select";

    /// \[plugin-to-host\] Ask the user for confirmation. Headless modes answer with an error.
    pub const UI_CONFIRM: &str = "ui/confirm";

    /// \[plugin-to-host\] Prompt the user for free text. A null result means cancelled; headless modes answer with an error.
    pub const UI_INPUT: &str = "ui/input";

    /// \[plugin-to-host\] Run a shell command on the host. Trust-gated: untrusted contexts answer with a policyDenied error.
    pub const EXEC_RUN: &str = "exec/run";

    /// \[plugin-to-host\] Diagnostic log line, routed to the host log with plugin attribution.
    pub const LOGS_EMIT: &str = "logs/emit";

    /// \[plugin-to-host\] Structured user-facing warning (dismissible in the TUI, logged with plugin attribution everywhere).
    pub const WARNINGS_EMIT: &str = "warnings/emit";

    /// \[plugin-to-host\] Dynamically register an LLM provider bridge (trust/mode gated). The registration payload mirrors the provider registry entry format.
    pub const HOST_REGISTER_PROVIDER: &str = "host/registerProvider";
}

/// hooks/afterToolCall params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AfterToolCallParams {
    #[serde(rename = "isError")]
    pub is_error: bool,
    #[serde(rename = "result")]
    pub result: ToolOutput,
    #[serde(rename = "toolCall")]
    pub tool_call: ToolCall,
}

/// Per-field patch; absent fields are untouched. Later plugins in the chain win per field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AfterToolCallPatch {
    #[serde(rename = "content")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
    #[serde(rename = "details")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(rename = "isError")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// End the agent loop after this result.
    #[serde(rename = "terminate")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    /// Usage accounting override (provider-shaped).
    #[serde(rename = "usage")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}

/// A claimed approval decision (a null method result passes to the next reviewer).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDecision {
    #[serde(rename = "action")]
    pub action: ApprovalDecisionAction,
    #[serde(rename = "reason")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Approval outcomes a reviewer can return.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecisionAction {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "reviewed")]
    Reviewed,
    #[serde(rename = "askUser")]
    AskUser,
}

/// approval/review params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalReviewParams {
    #[serde(rename = "approvalId")]
    pub approval_id: String,
    /// The session's active approval policy name.
    #[serde(rename = "approvalPolicy")]
    pub approval_policy: String,
    /// Permission evidence gathered by the host (rule matches, sandbox state).
    #[serde(rename = "evidence")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
    #[serde(rename = "toolCall")]
    pub tool_call: ToolCall,
}

/// autocomplete/provide params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutocompleteProvideParams {
    #[serde(rename = "cursorOffset")]
    pub cursor_offset: u64,
    #[serde(rename = "providerId")]
    pub provider_id: String,
    #[serde(rename = "query")]
    pub query: String,
}

/// An empty suggestions list is a legal no-suggestions answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutocompleteProvideResult {
    #[serde(rename = "suggestions")]
    pub suggestions: Vec<AutocompleteSuggestion>,
}

/// An autocomplete provider for the input line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutocompleteProviderSpec {
    #[serde(rename = "description")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "id")]
    pub id: String,
    /// Token prefix that triggers the provider (for example #).
    #[serde(rename = "trigger")]
    pub trigger: String,
}

/// One suggestion. insertText absent = insert value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutocompleteSuggestion {
    #[serde(rename = "detail")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(rename = "insertText")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_text: Option<String>,
    #[serde(rename = "label")]
    pub label: String,
    #[serde(rename = "value")]
    pub value: String,
}

/// hooks/beforeToolCall params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BeforeToolCallParams {
    /// The assistant message that issued the call (provider-shaped JSON).
    #[serde(rename = "assistantMessage")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_message: Option<Value>,
    #[serde(rename = "toolCall")]
    pub tool_call: ToolCall,
}

/// commands/invoke params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandInvokeParams {
    #[serde(rename = "args")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(rename = "name")]
    pub name: String,
}

/// A contributed slash command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandSpec {
    #[serde(rename = "description")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "name")]
    pub name: String,
}

/// Per-plugin configuration declaration: a JSON Schema the host validates settings plugins."\<id\>".config against at load time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigDeclaration {
    /// JSON Schema (object) of the plugin configuration.
    #[serde(rename = "schema")]
    pub schema: Value,
}

/// config/get result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigResult {
    /// Effective per-plugin config (host-validated; defaults when the user config was invalid).
    #[serde(rename = "config")]
    pub config: Value,
}

/// One tool output content block. Fields not matching kind are absent (text blocks carry text; image blocks carry mimeType + data).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContentBlock {
    /// Base64 payload for image blocks.
    #[serde(rename = "data")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(rename = "mimeType")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(rename = "text")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(rename = "type")]
    pub r#type: ContentBlockKind,
}

/// Tool output content block kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentBlockKind {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "image")]
    Image,
}

/// exec/run params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecRunParams {
    #[serde(rename = "command")]
    pub command: String,
    #[serde(rename = "timeoutMs")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// exec/run result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecRunResult {
    #[serde(rename = "code")]
    pub code: i32,
    #[serde(rename = "stderr")]
    pub stderr: String,
    #[serde(rename = "stdout")]
    pub stdout: String,
}

/// Opt-in hook surfaces. Absent boolean means not implemented (the host skips the call entirely).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookCapabilities {
    #[serde(rename = "afterToolCall")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_tool_call: Option<bool>,
    #[serde(rename = "approvalReview")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_review: Option<bool>,
    #[serde(rename = "beforeToolCall")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_tool_call: Option<bool>,
    #[serde(rename = "transformContext")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform_context: Option<bool>,
}

/// What this host supports in the current mode. Absent boolean means unsupported; a plugin must check before depending on a surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostCapabilities {
    #[serde(rename = "autocomplete")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocomplete: Option<bool>,
    #[serde(rename = "exec")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<bool>,
    #[serde(rename = "metrics")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsHostCapability>,
    #[serde(rename = "providerRegistration")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_registration: Option<bool>,
    #[serde(rename = "sessionControl")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_control: Option<bool>,
    #[serde(rename = "snapshot")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<bool>,
    #[serde(rename = "uiDialogs")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_dialogs: Option<bool>,
    #[serde(rename = "widgets")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widgets: Option<bool>,
}

/// Host identification (for user agents and logs).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "version")]
    pub version: String,
}

/// Host -\> plugin handshake.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "capabilities")]
    pub capabilities: HostCapabilities,
    /// Effective per-plugin configuration, validated against the plugin's declared config schema (empty object when the plugin declares none).
    #[serde(rename = "config")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    #[serde(rename = "cwd")]
    pub cwd: String,
    #[serde(rename = "host")]
    pub host: HostInfo,
    #[serde(rename = "mode")]
    pub mode: RunMode,
    /// Semver protocol version the host speaks.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Project trust state; gates exec and other privileged services.
    #[serde(rename = "trusted")]
    pub trusted: bool,
}

/// Plugin -\> host handshake answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "capabilities")]
    pub capabilities: PluginCapabilities,
    #[serde(rename = "plugin")]
    pub plugin: PluginInfo,
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
}

/// events/lifecycle params. event is the lifecycle event name; payload is event-shaped JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LifecycleEventParams {
    #[serde(rename = "event")]
    pub event: String,
    #[serde(rename = "payload")]
    pub payload: Value,
}

/// Log/notify levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "debug")]
    Debug,
}

/// logs/emit params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogParams {
    #[serde(rename = "level")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<LogLevel>,
    #[serde(rename = "message")]
    pub message: String,
}

/// One declared metric operation. Identifiers match \[a-z\]\[a-z0-9_.\]{0,63}; at most 8 dimensions per operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricOperation {
    #[serde(rename = "description")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Dimension name -\> allowed values.
    #[serde(rename = "dimensions")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<BTreeMap<String, Vec<String>>>,
}

/// Declared telemetry schema for the metrics sidecar. Any violation voids the whole declaration with a load warning.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricsDeclaration {
    #[serde(rename = "operations")]
    pub operations: BTreeMap<String, MetricOperation>,
}

/// Metrics sidecar host support: the plugin appends NDJSON measurements to scratchFile; the host validates the drain against the declared operations schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricsHostCapability {
    /// Absolute path of the per-session scratch file (WASM carrier: inside the dedicated preopen).
    #[serde(rename = "scratchFile")]
    pub scratch_file: String,
}

/// Everything a plugin contributes. Every field is optional and independent; an absent field contributes nothing and costs nothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginCapabilities {
    #[serde(rename = "autocompleteProviders")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocomplete_providers: Option<Vec<AutocompleteProviderSpec>>,
    #[serde(rename = "commands")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<CommandSpec>>,
    #[serde(rename = "config")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigDeclaration>,
    /// Lifecycle events the plugin subscribes to (empty/absent = the default set).
    #[serde(rename = "events")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<String>>,
    #[serde(rename = "hooks")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HookCapabilities>,
    #[serde(rename = "metrics")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsDeclaration>,
    #[serde(rename = "tools")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,
    #[serde(rename = "widgets")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widgets: Option<Vec<WidgetSpec>>,
}

/// Plugin identification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginInfo {
    #[serde(rename = "description")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "version")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// host/registerProvider params: a provider registry entry (provider-shaped JSON).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegisterProviderParams {
    #[serde(rename = "provider")]
    pub provider: Value,
}

/// Host run mode, reported at initialize so a plugin never depends on interactive requests for correctness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunMode {
    #[serde(rename = "tui")]
    Tui,
    #[serde(rename = "print")]
    Print,
    #[serde(rename = "rpc")]
    Rpc,
    #[serde(rename = "acp")]
    Acp,
}

/// session/sendUserMessage params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SendUserMessageParams {
    #[serde(rename = "text")]
    pub text: String,
}

/// session/get result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    #[serde(rename = "cwd")]
    pub cwd: String,
    #[serde(rename = "label")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(rename = "messageCount")]
    pub message_count: u64,
    #[serde(rename = "mode")]
    pub mode: RunMode,
    #[serde(rename = "modelId")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "trusted")]
    pub trusted: bool,
}

/// snapshot/get result: a digest, not a transcript (full-history use cases are served by event subscriptions).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Bumped on every compaction.
    #[serde(rename = "compactionRevision")]
    pub compaction_revision: u64,
    /// Bumped on every history mutation.
    #[serde(rename = "historyVersion")]
    pub history_version: u64,
    #[serde(rename = "messageCount")]
    pub message_count: u64,
    /// Truncated, prompt-hygiene-normalized digest of recent messages.
    #[serde(rename = "recentDigest")]
    pub recent_digest: String,
    #[serde(rename = "tokenUsage")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
}

/// Token accounting snapshot (provider-shaped totals).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(rename = "cacheRead")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(rename = "cacheWrite")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    #[serde(rename = "input")]
    pub input: u64,
    #[serde(rename = "output")]
    pub output: u64,
    #[serde(rename = "total")]
    pub total: u64,
}

/// A tool call in flight.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "arguments")]
    pub arguments: Value,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
}

/// tools/execute params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolExecuteParams {
    /// Validated against the tool's parameter schema by the host.
    #[serde(rename = "arguments")]
    pub arguments: Value,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
}

/// Tool execution result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    #[serde(rename = "content")]
    pub content: Vec<ContentBlock>,
    /// Structured details for the UI (not sent to the model).
    #[serde(rename = "details")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(rename = "isError")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// A contributed tool. parameters must be a JSON object schema; anything else is rejected at registration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    #[serde(rename = "description")]
    pub description: String,
    #[serde(rename = "label")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(rename = "name")]
    pub name: String,
    /// JSON Schema (object) of the tool arguments.
    #[serde(rename = "parameters")]
    pub parameters: Value,
}

/// hooks/transformContext params. Messages are session-entry shaped JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransformContextParams {
    #[serde(rename = "messages")]
    pub messages: Vec<Value>,
}

/// null = unchanged; a list replaces the context every later hook (and the loop) sees.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransformContextResult {
    #[serde(rename = "messages")]
    pub messages: Vec<Value>,
}

/// ui/confirm params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiConfirmParams {
    #[serde(rename = "message")]
    pub message: String,
    #[serde(rename = "title")]
    pub title: String,
}

/// ui/input params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiInputParams {
    #[serde(rename = "placeholder")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(rename = "title")]
    pub title: String,
}

/// ui/notify params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiNotifyParams {
    #[serde(rename = "level")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<LogLevel>,
    #[serde(rename = "message")]
    pub message: String,
}

/// ui/select params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiSelectParams {
    #[serde(rename = "options")]
    pub options: Vec<String>,
    #[serde(rename = "title")]
    pub title: String,
}

/// Interception verdict. deny carries reason (becomes the error tool result); rewrite carries arguments (the full replacement).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    #[serde(rename = "action")]
    pub action: VerdictAction,
    #[serde(rename = "arguments")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    #[serde(rename = "reason")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Interception verdict actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerdictAction {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "deny")]
    Deny,
    #[serde(rename = "rewrite")]
    Rewrite,
}

/// warnings/emit params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WarningParams {
    /// Structured context shown in the warning details.
    #[serde(rename = "context")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    #[serde(rename = "message")]
    pub message: String,
}

/// widgets/action params (for example action select for list panels).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetActionParams {
    #[serde(rename = "action")]
    pub action: String,
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "itemId")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

/// Declarative widget kinds; the host owns rendering and layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WidgetKind {
    #[serde(rename = "statusLineSegment")]
    StatusLineSegment,
    #[serde(rename = "markdownPanel")]
    MarkdownPanel,
    #[serde(rename = "listPanel")]
    ListPanel,
}

/// A declared long-lived UI unit. The host keys it as \<plugin\>:\<id\>; the plugin pushes full-state snapshots via widgets/update.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetSpec {
    #[serde(rename = "id")]
    pub id: String,
    /// Initial state, shaped per the widget kind.
    #[serde(rename = "initial")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<Value>,
    /// Sort key for status line segments (smaller first).
    #[serde(rename = "priority")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// Panel title (required by contract for panel kinds).
    #[serde(rename = "title")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub r#type: WidgetKind,
    /// Initial visibility for panel kinds.
    #[serde(rename = "visible")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
}

/// widgets/update params: idempotent full-state replacement, shaped per the widget kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetUpdateParams {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "state")]
    pub state: Value,
    #[serde(rename = "visible")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
}
