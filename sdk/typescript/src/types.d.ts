// GENERATED from protocol/tack-rpc.openrpc.json by `cargo run -p xtask -- codegen`. Do not edit by hand.

export const ERR_PARSE = -32700;
export const ERR_INVALID_REQUEST = -32600;
export const ERR_METHOD_NOT_FOUND = -32601;
export const ERR_INVALID_PARAMS = -32602;
export const ERR_INTERNAL = -32603;
export const ERR_POLICY_DENIED = -32001;
export const ERR_CAPABILITY_NOT_GRANTED = -32002;
export const ERR_PLUGIN_UNAVAILABLE = -32003;
export const ERR_REQUEST_TIMEOUT = -32004;

/** [host-to-plugin] Lifecycle handshake, host -> plugin, sent immediately after spawn. The host advertises its capabilities and the validated per-plugin config; the plugin answers with its identity and its (all-optional) capability contributions. A protocolVersion outside the host's supported range is a clean handshake error, not a silent degradation. */
export const INITIALIZE = "initialize";
/** [host-to-plugin] Graceful stop, host -> plugin. The plugin should finish in-flight work and exit; the host kills the carrier after a grace period. */
export const SHUTDOWN = "shutdown";
/** [host-to-plugin] Run a plugin-contributed tool. The call carries the provider-issued toolCallId so the plugin can correlate progress/cancellation. */
export const TOOLS_EXECUTE = "tools/execute";
/** [host-to-plugin] Run a plugin-registered slash command. */
export const COMMANDS_INVOKE = "commands/invoke";
/** [host-to-plugin] A tool call is about to run; the plugin may allow, deny (reason becomes the error tool result), or rewrite the arguments. Chained plugins observe the previous plugin's rewrite; the first deny short-circuits. */
export const HOOKS_BEFORE_TOOL_CALL = "hooks/beforeToolCall";
/** [host-to-plugin] COW context pipeline (opt-in via capabilities.hooks.transformContext): a null result means unchanged; a returned list replaces the messages every later hook sees. */
export const HOOKS_TRANSFORM_CONTEXT = "hooks/transformContext";
/** [host-to-plugin] Observe and patch a tool result (opt-in via capabilities.hooks.afterToolCall). Patches merge in chain order; later plugins win per field. */
export const HOOKS_AFTER_TOOL_CALL = "hooks/afterToolCall";
/** [host-to-plugin] An approval decision is needed (opt-in via capabilities.hooks.approvalReview). A null result passes to the next reviewer in the chain; a returned decision claims the approval (first-claim-wins). */
export const APPROVAL_REVIEW = "approval/review";
/** [host-to-plugin] Query an autocomplete provider for input-line suggestions. Timeouts and cancellations degrade to no suggestions. */
export const AUTOCOMPLETE_PROVIDE = "autocomplete/provide";
/** [host-to-plugin] Lifecycle notification, subscription-gated via capabilities.events. The event name is one of the well-known lifecycle set (sessionStart, sessionShutdown, agentStart, agentEnd, turnStart, turnEnd, messageStart, messageEnd, toolExecutionStart, toolExecutionEnd, modelSelect, thinkingLevelSelect); the payload shape depends on the event. */
export const EVENTS_LIFECYCLE = "events/lifecycle";
/** [host-to-plugin] User interaction with a declared widget (never produced in headless modes). Sent only to the owning plugin. */
export const WIDGETS_ACTION = "widgets/action";
/** [plugin-to-host] Idempotent full-state replacement for a declared widget (not a diff; dropped frames are harmless). */
export const WIDGETS_UPDATE = "widgets/update";
/** [plugin-to-host] Read current session state (trust/mode gated). */
export const SESSION_GET = "session/get";
/** [plugin-to-host] Inject a user message into the session (trust/mode gated). */
export const SESSION_SEND_USER_MESSAGE = "session/sendUserMessage";
/** [plugin-to-host] Versioned read-only session digest. historyVersion and compactionRevision let the plugin detect staleness between calls without re-fetching. */
export const SNAPSHOT_GET = "snapshot/get";
/** [plugin-to-host] Read the plugin's effective configuration (host-validated against the declared config schema; invalid user config degrades to defaults with a load warning). */
export const CONFIG_GET = "config/get";
/** [plugin-to-host] Show a notification. Headless modes degrade to a log line. */
export const UI_NOTIFY = "ui/notify";
/** [plugin-to-host] Ask the user to pick one option. Headless modes answer with an error (see host capabilities). A null result means dismissed. */
export const UI_SELECT = "ui/select";
/** [plugin-to-host] Ask the user for confirmation. Headless modes answer with an error. */
export const UI_CONFIRM = "ui/confirm";
/** [plugin-to-host] Prompt the user for free text. A null result means cancelled; headless modes answer with an error. */
export const UI_INPUT = "ui/input";
/** [plugin-to-host] Run a shell command on the host. Trust-gated: untrusted contexts answer with a policyDenied error. */
export const EXEC_RUN = "exec/run";
/** [plugin-to-host] Diagnostic log line, routed to the host log with plugin attribution. */
export const LOGS_EMIT = "logs/emit";
/** [plugin-to-host] Structured user-facing warning (dismissible in the TUI, logged with plugin attribution everywhere). */
export const WARNINGS_EMIT = "warnings/emit";
/** [plugin-to-host] Dynamically register an LLM provider bridge (trust/mode gated). The registration payload mirrors the provider registry entry format. */
export const HOST_REGISTER_PROVIDER = "host/registerProvider";

/** hooks/afterToolCall params. */
export interface AfterToolCallParams {
  "isError": boolean;
  "result": ToolOutput;
  "toolCall": ToolCall;
}

/** Per-field patch; absent fields are untouched. Later plugins in the chain win per field. */
export interface AfterToolCallPatch {
  "content"?: Array<ContentBlock>;
  "details"?: any;
  "isError"?: boolean;
  /** End the agent loop after this result. */
  "terminate"?: boolean;
  /** Usage accounting override (provider-shaped). */
  "usage"?: any;
}

/** A claimed approval decision (a null method result passes to the next reviewer). */
export interface ApprovalDecision {
  "action": ApprovalDecisionAction;
  "reason"?: string;
}

/** Approval outcomes a reviewer can return. */
export type ApprovalDecisionAction = "allow" | "reviewed" | "askUser";


/** approval/review params. */
export interface ApprovalReviewParams {
  "approvalId": string;
  /** The session's active approval policy name. */
  "approvalPolicy": string;
  /** Permission evidence gathered by the host (rule matches, sandbox state). */
  "evidence"?: any;
  "toolCall": ToolCall;
}

/** autocomplete/provide params. */
export interface AutocompleteProvideParams {
  "cursorOffset": number;
  "providerId": string;
  "query": string;
}

/** An empty suggestions list is a legal no-suggestions answer. */
export interface AutocompleteProvideResult {
  "suggestions": Array<AutocompleteSuggestion>;
}

/** An autocomplete provider for the input line. */
export interface AutocompleteProviderSpec {
  "description"?: string;
  "id": string;
  /** Token prefix that triggers the provider (for example #). */
  "trigger": string;
}

/** One suggestion. insertText absent = insert value. */
export interface AutocompleteSuggestion {
  "detail"?: string;
  "insertText"?: string;
  "label": string;
  "value": string;
}

/** hooks/beforeToolCall params. */
export interface BeforeToolCallParams {
  /** The assistant message that issued the call (provider-shaped JSON). */
  "assistantMessage"?: any;
  "toolCall": ToolCall;
}

/** commands/invoke params. */
export interface CommandInvokeParams {
  "args"?: string;
  "name": string;
}

/** A contributed slash command. */
export interface CommandSpec {
  "description"?: string;
  "name": string;
}

/** Per-plugin configuration declaration: a JSON Schema the host validates settings plugins."<id>".config against at load time. */
export interface ConfigDeclaration {
  /** JSON Schema (object) of the plugin configuration. */
  "schema": any;
}

/** config/get result. */
export interface ConfigResult {
  /** Effective per-plugin config (host-validated; defaults when the user config was invalid). */
  "config": any;
}

/** One tool output content block. Fields not matching kind are absent (text blocks carry text; image blocks carry mimeType + data). */
export interface ContentBlock {
  /** Base64 payload for image blocks. */
  "data"?: string;
  "mimeType"?: string;
  "text"?: string;
  "type": ContentBlockKind;
}

/** Tool output content block kinds. */
export type ContentBlockKind = "text" | "image";


/** exec/run params. */
export interface ExecRunParams {
  "command": string;
  "timeoutMs"?: number;
}

/** exec/run result. */
export interface ExecRunResult {
  "code": number;
  "stderr": string;
  "stdout": string;
}

/** Opt-in hook surfaces. Absent boolean means not implemented (the host skips the call entirely). */
export interface HookCapabilities {
  "afterToolCall"?: boolean;
  "approvalReview"?: boolean;
  "beforeToolCall"?: boolean;
  "transformContext"?: boolean;
}

/** What this host supports in the current mode. Absent boolean means unsupported; a plugin must check before depending on a surface. */
export interface HostCapabilities {
  "autocomplete"?: boolean;
  "exec"?: boolean;
  "metrics"?: MetricsHostCapability;
  "providerRegistration"?: boolean;
  "sessionControl"?: boolean;
  "snapshot"?: boolean;
  "uiDialogs"?: boolean;
  "widgets"?: boolean;
}

/** Host identification (for user agents and logs). */
export interface HostInfo {
  "name": string;
  "version": string;
}

/** Host -> plugin handshake. */
export interface InitializeParams {
  "capabilities": HostCapabilities;
  /** Effective per-plugin configuration, validated against the plugin's declared config schema (empty object when the plugin declares none). */
  "config"?: any;
  "cwd": string;
  "host": HostInfo;
  "mode": RunMode;
  /** Semver protocol version the host speaks. */
  "protocolVersion": string;
  /** Project trust state; gates exec and other privileged services. */
  "trusted": boolean;
}

/** Plugin -> host handshake answer. */
export interface InitializeResult {
  "capabilities": PluginCapabilities;
  "plugin": PluginInfo;
  "protocolVersion": string;
}

/** events/lifecycle params. event is the lifecycle event name; payload is event-shaped JSON. */
export interface LifecycleEventParams {
  "event": string;
  "payload": any;
}

/** Log/notify levels. */
export type LogLevel = "info" | "warning" | "error" | "debug";


/** logs/emit params. */
export interface LogParams {
  "level"?: LogLevel;
  "message": string;
}

/** One declared metric operation. Identifiers match [a-z][a-z0-9_.]{0,63}; at most 8 dimensions per operation. */
export interface MetricOperation {
  "description"?: string;
  /** Dimension name -> allowed values. */
  "dimensions"?: Record<string, Array<string>>;
}

/** Declared telemetry schema for the metrics sidecar. Any violation voids the whole declaration with a load warning. */
export interface MetricsDeclaration {
  "operations": Record<string, MetricOperation>;
}

/** Metrics sidecar host support: the plugin appends NDJSON measurements to scratchFile; the host validates the drain against the declared operations schema. */
export interface MetricsHostCapability {
  /** Absolute path of the per-session scratch file (WASM carrier: inside the dedicated preopen). */
  "scratchFile": string;
}

/** Everything a plugin contributes. Every field is optional and independent; an absent field contributes nothing and costs nothing. */
export interface PluginCapabilities {
  "autocompleteProviders"?: Array<AutocompleteProviderSpec>;
  "commands"?: Array<CommandSpec>;
  "config"?: ConfigDeclaration;
  /** Lifecycle events the plugin subscribes to (empty/absent = the default set). */
  "events"?: Array<string>;
  "hooks"?: HookCapabilities;
  "metrics"?: MetricsDeclaration;
  "tools"?: Array<ToolSpec>;
  "widgets"?: Array<WidgetSpec>;
}

/** Plugin identification. */
export interface PluginInfo {
  "description"?: string;
  "name": string;
  "version"?: string;
}

/** host/registerProvider params: a provider registry entry (provider-shaped JSON). */
export interface RegisterProviderParams {
  "provider": any;
}

/** Host run mode, reported at initialize so a plugin never depends on interactive requests for correctness. */
export type RunMode = "tui" | "print" | "rpc" | "acp";


/** session/sendUserMessage params. */
export interface SendUserMessageParams {
  "text": string;
}

/** session/get result. */
export interface SessionInfo {
  "cwd": string;
  "label"?: string;
  "messageCount": number;
  "mode": RunMode;
  "modelId"?: string;
  "sessionId": string;
  "trusted": boolean;
}

/** snapshot/get result: a digest, not a transcript (full-history use cases are served by event subscriptions). */
export interface Snapshot {
  /** Bumped on every compaction. */
  "compactionRevision": number;
  /** Bumped on every history mutation. */
  "historyVersion": number;
  "messageCount": number;
  /** Truncated, prompt-hygiene-normalized digest of recent messages. */
  "recentDigest": string;
  "tokenUsage"?: TokenUsage;
}

/** Token accounting snapshot (provider-shaped totals). */
export interface TokenUsage {
  "cacheRead"?: number;
  "cacheWrite"?: number;
  "input": number;
  "output": number;
  "total": number;
}

/** A tool call in flight. */
export interface ToolCall {
  "arguments": any;
  "toolCallId": string;
  "toolName": string;
}

/** tools/execute params. */
export interface ToolExecuteParams {
  /** Validated against the tool's parameter schema by the host. */
  "arguments": any;
  "name": string;
  "toolCallId": string;
}

/** Tool execution result. */
export interface ToolOutput {
  "content": Array<ContentBlock>;
  /** Structured details for the UI (not sent to the model). */
  "details"?: any;
  "isError"?: boolean;
}

/** A contributed tool. parameters must be a JSON object schema; anything else is rejected at registration. */
export interface ToolSpec {
  "description": string;
  "label"?: string;
  "name": string;
  /** JSON Schema (object) of the tool arguments. */
  "parameters": any;
}

/** hooks/transformContext params. Messages are session-entry shaped JSON. */
export interface TransformContextParams {
  "messages": Array<any>;
}

/** null = unchanged; a list replaces the context every later hook (and the loop) sees. */
export interface TransformContextResult {
  "messages": Array<any>;
}

/** ui/confirm params. */
export interface UiConfirmParams {
  "message": string;
  "title": string;
}

/** ui/input params. */
export interface UiInputParams {
  "placeholder"?: string;
  "title": string;
}

/** ui/notify params. */
export interface UiNotifyParams {
  "level"?: LogLevel;
  "message": string;
}

/** ui/select params. */
export interface UiSelectParams {
  "options": Array<string>;
  "title": string;
}

/** Interception verdict. deny carries reason (becomes the error tool result); rewrite carries arguments (the full replacement). */
export interface Verdict {
  "action": VerdictAction;
  "arguments"?: any;
  "reason"?: string;
}

/** Interception verdict actions. */
export type VerdictAction = "allow" | "deny" | "rewrite";


/** warnings/emit params. */
export interface WarningParams {
  /** Structured context shown in the warning details. */
  "context"?: any;
  "message": string;
}

/** widgets/action params (for example action select for list panels). */
export interface WidgetActionParams {
  "action": string;
  "id": string;
  "itemId"?: string;
}

/** Declarative widget kinds; the host owns rendering and layout. */
export type WidgetKind = "statusLineSegment" | "markdownPanel" | "listPanel";


/** A declared long-lived UI unit. The host keys it as <plugin>:<id>; the plugin pushes full-state snapshots via widgets/update. */
export interface WidgetSpec {
  "id": string;
  /** Initial state, shaped per the widget kind. */
  "initial"?: any;
  /** Sort key for status line segments (smaller first). */
  "priority"?: number;
  /** Panel title (required by contract for panel kinds). */
  "title"?: string;
  "type": WidgetKind;
  /** Initial visibility for panel kinds. */
  "visible"?: boolean;
}

/** widgets/update params: idempotent full-state replacement, shaped per the widget kind. */
export interface WidgetUpdateParams {
  "id": string;
  "state": any;
  "visible"?: boolean;
}
