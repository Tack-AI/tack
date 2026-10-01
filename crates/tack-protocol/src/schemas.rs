//! Protocol schemas. Serde types matching `packages/protocol/src/schemas.ts`
//! (protocol version 1) field-for-field.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// Hello capability (client → server): opt into extension widget events
/// (`ext_widget_update` / `ext_widgets_removed`). Without it the server
/// never sends those events to the connection — pre-extension clients
/// keep their exact v1 event stream.
pub const CAP_EXT_WIDGETS: &str = "ext_widgets";
/// Hello capability (client → server): opt into extension dialog events
/// (`ext_dialog_request` / `ext_dialog_closed`) — plugin
/// `ui/select`/`ui/confirm`/`ui/input` and MCP elicitation. Only
/// connections with this capability are counted as dialog answerers: a
/// plugin dialog with no capable client connected fails fast instead of
/// parking unanswered.
pub const CAP_EXT_DIALOGS: &str = "ext_dialogs";
/// The extension-surface capabilities this protocol version defines
/// (echoed in the server hello so clients can feature-detect).
pub const SERVER_CAPABILITIES: &[&str] = &[CAP_EXT_WIDGETS, CAP_EXT_DIALOGS];

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    Turn,
    Compaction,
    BranchSummary,
    Retry,
}

/// Session permission mode (TS pi / ACP session modes). Added as a v1
/// additive extension: it only appears in OPTIONAL fields and NEW command/
/// event variants, which pre-extension peers tolerate (unknown fields are
/// skipped; unknown variants decode as the `Unknown` catch-all).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionMode {
    /// Prompt for edits/commands; read-only tools free.
    #[serde(rename = "ask")]
    Ask,
    /// File edits free, commands prompt.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// Read-only; edits/commands blocked.
    #[serde(rename = "plan")]
    Plan,
    /// No prompts at all (default for remote sessions: pre-extension
    /// servers ran every non-denied tool, so this preserves behavior for
    /// clients that cannot answer permission prompts).
    #[default]
    #[serde(rename = "bypass")]
    Bypass,
}

/// Answer to a `ServerEvent::PermissionRequest`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    AllowOnce,
    AllowAlways,
    Deny,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelMetadata {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub context_window: u32,
    pub max_tokens: u32,
    pub cost: ModelCost,
    pub supported_thinking_levels: Vec<ThinkingLevel>,
    pub authenticated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum UserContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AssistantContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    #[serde(rename = "toolCall")]
    ToolCall {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        input: Value,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    #[serde(rename = "cacheRead")]
    pub cache_read: u64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: u64,
    pub cost: UsageCost,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role")]
pub enum TranscriptItem {
    #[serde(rename = "user")]
    User {
        id: String,
        content: Vec<UserContent>,
        timestamp: u64,
    },
    #[serde(rename = "assistant")]
    Assistant {
        id: String,
        content: Vec<AssistantContent>,
        model: ModelRef,
        #[serde(rename = "responseModel", skip_serializing_if = "Option::is_none")]
        response_model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        timestamp: u64,
        status: String, // "streaming" | "complete" | "error" | "aborted"
        #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    #[serde(rename = "tool")]
    Tool {
        id: String,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        input: Value,
        content: Vec<UserContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        timestamp: u64,
        status: String, // "running" | "complete" | "error"
        #[serde(rename = "isError")]
        is_error: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum TranscriptProgress {
    #[serde(rename = "item_started")]
    ItemStarted { item: TranscriptItem },
    #[serde(rename = "assistant_delta")]
    AssistantDelta {
        #[serde(rename = "messageId")]
        message_id: String,
        #[serde(rename = "contentIndex")]
        content_index: u32,
        kind: String, // "text" | "thinking" | "toolCall"
        delta: String,
    },
    #[serde(rename = "item_updated")]
    ItemUpdated { item: TranscriptItem },
    #[serde(rename = "item_finished")]
    ItemFinished { item: TranscriptItem },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    pub id: String,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub phase: SessionPhase,
    pub model: ModelRef,
    pub thinking_level: ThinkingLevel,
    pub attached: bool,
    pub locked: bool,
    pub revision: u64,
    /// Current permission mode. Optional so pre-extension (TS v1) peers
    /// parse the snapshot unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<SessionMode>,
    pub transcript: Vec<TranscriptItem>,
    pub queued_steer: Vec<TranscriptItem>,
    pub queued_steer_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerSnapshot {
    pub server_id: String,
    pub protocol_version: u32,
    pub revision: u64,
    pub sessions: Vec<SessionMetadata>,
    pub models: Vec<ModelMetadata>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    Version,
    /// Missing or invalid auth token.
    Auth,
    Busy,
    SessionLocked,
    NotFound,
    InvalidRequest,
    NotImplemented,
    InternalError,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// A plugin-contributed slash command (tack-RPC `commands/invoke`).
/// Extension surfaces are an additive v1 extension: they appear only in
/// NEW command/event variants and optional hello fields, which
/// pre-extension peers tolerate (unknown variants decode as the `Unknown`
/// catch-all; unknown fields are skipped).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExtCommandSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Full state of one declarative plugin widget, keyed `<plugin>:<id>`.
/// Updates are idempotent full-state replacements (`rev` bumps per
/// applied update).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExtWidgetState {
    pub key: String,
    pub plugin: String,
    /// "statusLineSegment" | "markdownPanel" | "listPanel".
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Latest state snapshot, shaped per the widget kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    pub visible: bool,
    pub rev: u64,
}

/// A plugin-contributed autocomplete provider.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExtAutocompleteProviderInfo {
    /// Host key `<plugin>:<provider-id>`, used in `ext_autocomplete`.
    pub key: String,
    pub plugin: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Token prefix that triggers the provider (e.g. `#`).
    pub trigger: String,
}

/// One autocomplete suggestion. `insert_text` absent = insert `value`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExtAutocompleteSuggestion {
    pub label: String,
    pub value: String,
    #[serde(rename = "insertText", skip_serializing_if = "Option::is_none")]
    pub insert_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One form field of an MCP elicitation dialog, flattened from the
/// server's requested schema.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExtElicitField {
    pub name: String,
    /// "string" | "number" | "integer" | "boolean" | "enum".
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
    /// Enum choices when `kind == "enum"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// Which plugin dialog an `ext_dialog_request` carries.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExtDialogKind {
    /// Pick one of `options`; answer value = the chosen option string.
    #[serde(rename = "select")]
    Select,
    /// Yes/no; answer value = boolean.
    #[serde(rename = "confirm")]
    Confirm,
    /// Free text; answer value = string.
    #[serde(rename = "input")]
    Input,
    /// MCP elicitation form; answer value = the content object.
    #[serde(rename = "elicitation")]
    Elicitation,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    #[serde(rename = "list")]
    List,
    #[serde(rename = "create")]
    Create {
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(rename = "thinkingLevel", skip_serializing_if = "Option::is_none")]
        thinking_level: Option<ThinkingLevel>,
    },
    #[serde(rename = "attach")]
    Attach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "detach")]
    Detach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "prompt")]
    Prompt {
        #[serde(rename = "sessionId")]
        session_id: String,
        text: String,
    },
    #[serde(rename = "steer")]
    Steer {
        #[serde(rename = "sessionId")]
        session_id: String,
        text: String,
    },
    #[serde(rename = "abort")]
    Abort {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "set_model")]
    SetModel {
        #[serde(rename = "sessionId")]
        session_id: String,
        model: ModelRef,
    },
    #[serde(rename = "set_thinking")]
    SetThinking {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "thinkingLevel")]
        thinking_level: ThinkingLevel,
    },
    /// Set the session's permission mode (ask/acceptEdits/plan/bypass).
    #[serde(rename = "set_mode")]
    SetMode {
        #[serde(rename = "sessionId")]
        session_id: String,
        mode: SessionMode,
    },
    /// Answer a `ServerEvent::PermissionRequest`. Fire-and-forget
    /// semantics; the server still replies (empty result) so clients can
    /// detect an unknown/expired request id.
    #[serde(rename = "permission_response")]
    PermissionResponse {
        #[serde(rename = "requestId")]
        request_id: String,
        decision: PermissionDecision,
    },
    /// List the server's available models (built-in catalog + models.json).
    #[serde(rename = "list_models")]
    ListModels,
    /// List plugin-contributed slash commands (host-global; empty when no
    /// plugins are loaded or the build has no extension support).
    #[serde(rename = "list_ext_commands")]
    ListExtCommands,
    /// Invoke a plugin slash command. The reply may be delayed past other
    /// responses: the plugin can itself show a dialog
    /// (`ext_dialog_request`) that this same connection answers while the
    /// invocation is in flight.
    #[serde(rename = "invoke_ext_command")]
    InvokeExtCommand {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    /// Snapshot of every declarative plugin widget (host-global).
    #[serde(rename = "list_ext_widgets")]
    ListExtWidgets,
    /// Report a widget interaction to the OWNING plugin (fire-and-forget;
    /// the empty reply just confirms the widget key resolved).
    #[serde(rename = "ext_widget_action")]
    ExtWidgetAction {
        key: String,
        action: String,
        #[serde(rename = "itemId", skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
    },
    /// List plugin autocomplete providers (host-global).
    #[serde(rename = "list_ext_autocomplete")]
    ListExtAutocomplete,
    /// Query one plugin autocomplete provider (degrades to an empty list
    /// on unknown key / dead plugin, like the TUI contract).
    #[serde(rename = "ext_autocomplete")]
    ExtAutocomplete {
        #[serde(rename = "providerKey")]
        provider_key: String,
        query: String,
        #[serde(rename = "cursorOffset")]
        cursor_offset: u32,
    },
    /// Answer a `ServerEvent::ExtDialogRequest`. Fire-and-forget
    /// semantics; the server still replies (empty result) so clients can
    /// detect an unknown/expired request id (another client answered
    /// first, or the dialog timed out). `cancelled` maps per kind:
    /// select/input → null answer, confirm → false, elicitation → the
    /// MCP "cancel" action.
    #[serde(rename = "ext_dialog_response")]
    ExtDialogResponse {
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(default)]
        cancelled: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
    },
    /// Catch-all: commands added by NEWER peers decode as `Unknown`
    /// instead of failing the whole frame (additive-extension tolerance).
    /// Never serialized.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum CommandResult {
    #[serde(rename = "list")]
    List { sessions: Vec<SessionMetadata> },
    #[serde(rename = "create")]
    Create { session: SessionSnapshot },
    #[serde(rename = "attach")]
    Attach { session: SessionSnapshot },
    #[serde(rename = "detach")]
    Detach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "prompt")]
    Prompt { session: SessionSnapshot },
    #[serde(rename = "steer")]
    Steer { session: SessionSnapshot },
    #[serde(rename = "abort")]
    Abort { session: SessionSnapshot },
    #[serde(rename = "set_model")]
    SetModel { session: SessionSnapshot },
    #[serde(rename = "set_thinking")]
    SetThinking { session: SessionSnapshot },
    #[serde(rename = "set_mode")]
    SetMode { session: SessionSnapshot },
    #[serde(rename = "permission_response")]
    PermissionResponse,
    #[serde(rename = "list_models")]
    ListModels { models: Vec<ModelMetadata> },
    #[serde(rename = "list_ext_commands")]
    ListExtCommands { commands: Vec<ExtCommandSpec> },
    #[serde(rename = "invoke_ext_command")]
    InvokeExtCommand { result: Value },
    #[serde(rename = "list_ext_widgets")]
    ListExtWidgets { widgets: Vec<ExtWidgetState> },
    #[serde(rename = "ext_widget_action")]
    ExtWidgetAction,
    #[serde(rename = "list_ext_autocomplete")]
    ListExtAutocomplete {
        providers: Vec<ExtAutocompleteProviderInfo>,
    },
    #[serde(rename = "ext_autocomplete")]
    ExtAutocomplete {
        suggestions: Vec<ExtAutocompleteSuggestion>,
    },
    #[serde(rename = "ext_dialog_response")]
    ExtDialogResponse,
    /// Catch-all for results added by newer peers (never serialized).
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "hello")]
    Hello {
        version: u32,
        /// Shared-token auth (server requires it when started with
        /// --auth-token/--auth-token-file).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        token: Option<String>,
        /// Extension surfaces the client opts into (`CAP_EXT_*`).
        /// Absent = pre-extension client: the server sends it none of the
        /// gated events. Unknown values are ignored (forward tolerance).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    #[serde(rename = "request")]
    Request { id: String, request: Command },
    /// Catch-all for message kinds added by newer peers (never serialized).
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ServerEvent {
    #[serde(rename = "server_snapshot")]
    ServerSnapshot { snapshot: ServerSnapshot },
    #[serde(rename = "session_snapshot")]
    SessionSnapshot { snapshot: SessionSnapshot },
    #[serde(rename = "session_progress")]
    SessionProgress {
        #[serde(rename = "sessionId")]
        session_id: String,
        progress: TranscriptProgress,
    },
    #[serde(rename = "session_removed")]
    SessionRemoved {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// A running tool call needs user approval (session mode `ask` or
    /// `acceptEdits`). Answer with `Command::PermissionResponse` carrying
    /// the same `request_id`. Only emitted when the session's mode prompts
    /// — pre-extension clients never see it (their sessions stay in the
    /// default `bypass` mode).
    #[serde(rename = "permission_request")]
    PermissionRequest {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Short human-readable summary (e.g. `bash: cargo test`).
        title: String,
        /// Raw tool input.
        input: Value,
    },
    /// A plugin pushed a widget state update (`widgets/update`): full
    /// replacement of the addressed widget's state. Only sent to
    /// connections that opted into `CAP_EXT_WIDGETS`.
    #[serde(rename = "ext_widget_update")]
    ExtWidgetUpdate { widget: ExtWidgetState },
    /// A plugin died: its widgets vanished with it (no UI residue).
    /// Only sent to `CAP_EXT_WIDGETS` connections.
    #[serde(rename = "ext_widgets_removed")]
    ExtWidgetsRemoved { plugin: String, keys: Vec<String> },
    /// A plugin (`ui/select`/`ui/confirm`/`ui/input`) or an MCP server
    /// (elicitation) asks the user a question. Plugin connections are
    /// host-global, so — like `PermissionRequest` with several attached
    /// clients — the request is broadcast to every `CAP_EXT_DIALOGS`
    /// connection and the FIRST `ext_dialog_response` wins; the others
    /// get `ext_dialog_closed`. Answer semantics per kind are documented
    /// on [`ExtDialogKind`].
    #[serde(rename = "ext_dialog_request")]
    ExtDialogRequest {
        #[serde(rename = "requestId")]
        request_id: String,
        /// Originating plugin id or MCP server name.
        source: String,
        kind: ExtDialogKind,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// `select` only.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
        /// `input` only.
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        /// `elicitation` only.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<ExtElicitField>,
    },
    /// A dialog was resolved without this connection's answer (another
    /// client answered first, it timed out, or the last dialog-capable
    /// client disconnected): dismiss any prompt showing `request_id`.
    #[serde(rename = "ext_dialog_closed")]
    ExtDialogClosed {
        #[serde(rename = "requestId")]
        request_id: String,
    },
    /// Catch-all for events added by newer peers: consumers must ignore
    /// them instead of failing the frame (never serialized).
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "hello")]
    Hello {
        version: u32,
        #[serde(rename = "connectionId")]
        connection_id: String,
        snapshot: ServerSnapshot,
        /// Extension-surface capabilities the server supports
        /// (`SERVER_CAPABILITIES`). Absent on pre-extension servers —
        /// clients must treat that as "no extension surfaces".
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    #[serde(rename = "hello_error")]
    HelloError { error: ProtocolError },
    #[serde(rename = "response")]
    Response {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<CommandResult>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ProtocolError>,
    },
    #[serde(rename = "event")]
    Event { event: ServerEvent },
    /// Catch-all for message kinds added by newer peers (never serialized).
    #[serde(other)]
    Unknown,
}

impl ServerMessage {
    pub fn ok(id: impl Into<String>, result: CommandResult) -> Self {
        ServerMessage::Response {
            id: id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: impl Into<String>, code: ProtocolErrorCode, message: impl Into<String>) -> Self {
        ServerMessage::Response {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(ProtocolError {
                code,
                message: message.into(),
                details: None,
            }),
        }
    }
}
