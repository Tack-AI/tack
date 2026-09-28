# GENERATED from protocol/tack-rpc.openrpc.json by `cargo run -p xtask -- codegen`. Do not edit by hand.

from enum import Enum
from typing import Any, Optional, TypedDict

from typing import NotRequired

ERR_PARSE = -32700
ERR_INVALID_REQUEST = -32600
ERR_METHOD_NOT_FOUND = -32601
ERR_INVALID_PARAMS = -32602
ERR_INTERNAL = -32603
ERR_POLICY_DENIED = -32001
ERR_CAPABILITY_NOT_GRANTED = -32002
ERR_PLUGIN_UNAVAILABLE = -32003
ERR_REQUEST_TIMEOUT = -32004

INITIALIZE = "initialize"
SHUTDOWN = "shutdown"
TOOLS_EXECUTE = "tools/execute"
COMMANDS_INVOKE = "commands/invoke"
HOOKS_BEFORE_TOOL_CALL = "hooks/beforeToolCall"
HOOKS_TRANSFORM_CONTEXT = "hooks/transformContext"
HOOKS_AFTER_TOOL_CALL = "hooks/afterToolCall"
APPROVAL_REVIEW = "approval/review"
AUTOCOMPLETE_PROVIDE = "autocomplete/provide"
EVENTS_LIFECYCLE = "events/lifecycle"
WIDGETS_ACTION = "widgets/action"
WIDGETS_UPDATE = "widgets/update"
SESSION_GET = "session/get"
SESSION_SEND_USER_MESSAGE = "session/sendUserMessage"
SNAPSHOT_GET = "snapshot/get"
CONFIG_GET = "config/get"
UI_NOTIFY = "ui/notify"
UI_SELECT = "ui/select"
UI_CONFIRM = "ui/confirm"
UI_INPUT = "ui/input"
EXEC_RUN = "exec/run"
LOGS_EMIT = "logs/emit"
WARNINGS_EMIT = "warnings/emit"
HOST_REGISTER_PROVIDER = "host/registerProvider"

class AfterToolCallParams(TypedDict):
    """hooks/afterToolCall params."""
    isError: bool
    result: "ToolOutput"
    toolCall: "ToolCall"


class AfterToolCallPatch(TypedDict):
    """Per-field patch; absent fields are untouched. Later plugins in the chain win per field."""
    content: NotRequired[list["ContentBlock"]]
    details: NotRequired[Any]
    isError: NotRequired[bool]
    terminate: NotRequired[bool]
    usage: NotRequired[Any]


class ApprovalDecision(TypedDict):
    """A claimed approval decision (a null method result passes to the next reviewer)."""
    action: "ApprovalDecisionAction"
    reason: NotRequired[str]


class ApprovalDecisionAction(str, Enum):
    """Approval outcomes a reviewer can return."""
    ALLOW = "allow"
    REVIEWED = "reviewed"
    ASK_USER = "askUser"


class ApprovalReviewParams(TypedDict):
    """approval/review params."""
    approvalId: str
    approvalPolicy: str
    evidence: NotRequired[Any]
    toolCall: "ToolCall"


class AutocompleteProvideParams(TypedDict):
    """autocomplete/provide params."""
    cursorOffset: int
    providerId: str
    query: str


class AutocompleteProvideResult(TypedDict):
    """An empty suggestions list is a legal no-suggestions answer."""
    suggestions: list["AutocompleteSuggestion"]


class AutocompleteProviderSpec(TypedDict):
    """An autocomplete provider for the input line."""
    description: NotRequired[str]
    id: str
    trigger: str


class AutocompleteSuggestion(TypedDict):
    """One suggestion. insertText absent = insert value."""
    detail: NotRequired[str]
    insertText: NotRequired[str]
    label: str
    value: str


class BeforeToolCallParams(TypedDict):
    """hooks/beforeToolCall params."""
    assistantMessage: NotRequired[Any]
    toolCall: "ToolCall"


class CommandInvokeParams(TypedDict):
    """commands/invoke params."""
    args: NotRequired[str]
    name: str


class CommandSpec(TypedDict):
    """A contributed slash command."""
    description: NotRequired[str]
    name: str


class ConfigDeclaration(TypedDict):
    """Per-plugin configuration declaration: a JSON Schema the host validates settings plugins."<id>".config against at load time."""
    schema: Any


class ConfigResult(TypedDict):
    """config/get result."""
    config: Any


class ContentBlock(TypedDict):
    """One tool output content block. Fields not matching kind are absent (text blocks carry text; image blocks carry mimeType + data)."""
    data: NotRequired[str]
    mimeType: NotRequired[str]
    text: NotRequired[str]
    type: "ContentBlockKind"


class ContentBlockKind(str, Enum):
    """Tool output content block kinds."""
    TEXT = "text"
    IMAGE = "image"


class ExecRunParams(TypedDict):
    """exec/run params."""
    command: str
    timeoutMs: NotRequired[int]


class ExecRunResult(TypedDict):
    """exec/run result."""
    code: int
    stderr: str
    stdout: str


class HookCapabilities(TypedDict):
    """Opt-in hook surfaces. Absent boolean means not implemented (the host skips the call entirely)."""
    afterToolCall: NotRequired[bool]
    approvalReview: NotRequired[bool]
    beforeToolCall: NotRequired[bool]
    transformContext: NotRequired[bool]


class HostCapabilities(TypedDict):
    """What this host supports in the current mode. Absent boolean means unsupported; a plugin must check before depending on a surface."""
    autocomplete: NotRequired[bool]
    exec: NotRequired[bool]
    metrics: NotRequired["MetricsHostCapability"]
    providerRegistration: NotRequired[bool]
    sessionControl: NotRequired[bool]
    snapshot: NotRequired[bool]
    uiDialogs: NotRequired[bool]
    widgets: NotRequired[bool]


class HostInfo(TypedDict):
    """Host identification (for user agents and logs)."""
    name: str
    version: str


class InitializeParams(TypedDict):
    """Host -> plugin handshake."""
    capabilities: "HostCapabilities"
    config: NotRequired[Any]
    cwd: str
    host: "HostInfo"
    mode: "RunMode"
    protocolVersion: str
    trusted: bool


class InitializeResult(TypedDict):
    """Plugin -> host handshake answer."""
    capabilities: "PluginCapabilities"
    plugin: "PluginInfo"
    protocolVersion: str


class LifecycleEventParams(TypedDict):
    """events/lifecycle params. event is the lifecycle event name; payload is event-shaped JSON."""
    event: str
    payload: Any


class LogLevel(str, Enum):
    """Log/notify levels."""
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"
    DEBUG = "debug"


class LogParams(TypedDict):
    """logs/emit params."""
    level: NotRequired["LogLevel"]
    message: str


class MetricOperation(TypedDict):
    """One declared metric operation. Identifiers match [a-z][a-z0-9_.]{0,63}; at most 8 dimensions per operation."""
    description: NotRequired[str]
    dimensions: NotRequired[dict[str, list[str]]]


class MetricsDeclaration(TypedDict):
    """Declared telemetry schema for the metrics sidecar. Any violation voids the whole declaration with a load warning."""
    operations: dict[str, "MetricOperation"]


class MetricsHostCapability(TypedDict):
    """Metrics sidecar host support: the plugin appends NDJSON measurements to scratchFile; the host validates the drain against the declared operations schema."""
    scratchFile: str


class PluginCapabilities(TypedDict):
    """Everything a plugin contributes. Every field is optional and independent; an absent field contributes nothing and costs nothing."""
    autocompleteProviders: NotRequired[list["AutocompleteProviderSpec"]]
    commands: NotRequired[list["CommandSpec"]]
    config: NotRequired["ConfigDeclaration"]
    events: NotRequired[list[str]]
    hooks: NotRequired["HookCapabilities"]
    metrics: NotRequired["MetricsDeclaration"]
    tools: NotRequired[list["ToolSpec"]]
    widgets: NotRequired[list["WidgetSpec"]]


class PluginInfo(TypedDict):
    """Plugin identification."""
    description: NotRequired[str]
    name: str
    version: NotRequired[str]


class RegisterProviderParams(TypedDict):
    """host/registerProvider params: a provider registry entry (provider-shaped JSON)."""
    provider: Any


class RunMode(str, Enum):
    """Host run mode, reported at initialize so a plugin never depends on interactive requests for correctness."""
    TUI = "tui"
    PRINT = "print"
    RPC = "rpc"
    ACP = "acp"


class SendUserMessageParams(TypedDict):
    """session/sendUserMessage params."""
    text: str


class SessionInfo(TypedDict):
    """session/get result."""
    cwd: str
    label: NotRequired[str]
    messageCount: int
    mode: "RunMode"
    modelId: NotRequired[str]
    sessionId: str
    trusted: bool


class Snapshot(TypedDict):
    """snapshot/get result: a digest, not a transcript (full-history use cases are served by event subscriptions)."""
    compactionRevision: int
    historyVersion: int
    messageCount: int
    recentDigest: str
    tokenUsage: NotRequired["TokenUsage"]


class TokenUsage(TypedDict):
    """Token accounting snapshot (provider-shaped totals)."""
    cacheRead: NotRequired[int]
    cacheWrite: NotRequired[int]
    input: int
    output: int
    total: int


class ToolCall(TypedDict):
    """A tool call in flight."""
    arguments: Any
    toolCallId: str
    toolName: str


class ToolExecuteParams(TypedDict):
    """tools/execute params."""
    arguments: Any
    name: str
    toolCallId: str


class ToolOutput(TypedDict):
    """Tool execution result."""
    content: list["ContentBlock"]
    details: NotRequired[Any]
    isError: NotRequired[bool]


class ToolSpec(TypedDict):
    """A contributed tool. parameters must be a JSON object schema; anything else is rejected at registration."""
    description: str
    label: NotRequired[str]
    name: str
    parameters: Any


class TransformContextParams(TypedDict):
    """hooks/transformContext params. Messages are session-entry shaped JSON."""
    messages: list[Any]


class TransformContextResult(TypedDict):
    """null = unchanged; a list replaces the context every later hook (and the loop) sees."""
    messages: list[Any]


class UiConfirmParams(TypedDict):
    """ui/confirm params."""
    message: str
    title: str


class UiInputParams(TypedDict):
    """ui/input params."""
    placeholder: NotRequired[str]
    title: str


class UiNotifyParams(TypedDict):
    """ui/notify params."""
    level: NotRequired["LogLevel"]
    message: str


class UiSelectParams(TypedDict):
    """ui/select params."""
    options: list[str]
    title: str


class Verdict(TypedDict):
    """Interception verdict. deny carries reason (becomes the error tool result); rewrite carries arguments (the full replacement)."""
    action: "VerdictAction"
    arguments: NotRequired[Any]
    reason: NotRequired[str]


class VerdictAction(str, Enum):
    """Interception verdict actions."""
    ALLOW = "allow"
    DENY = "deny"
    REWRITE = "rewrite"


class WarningParams(TypedDict):
    """warnings/emit params."""
    context: NotRequired[Any]
    message: str


class WidgetActionParams(TypedDict):
    """widgets/action params (for example action select for list panels)."""
    action: str
    id: str
    itemId: NotRequired[str]


class WidgetKind(str, Enum):
    """Declarative widget kinds; the host owns rendering and layout."""
    STATUS_LINE_SEGMENT = "statusLineSegment"
    MARKDOWN_PANEL = "markdownPanel"
    LIST_PANEL = "listPanel"


class WidgetSpec(TypedDict):
    """A declared long-lived UI unit. The host keys it as <plugin>:<id>; the plugin pushes full-state snapshots via widgets/update."""
    id: str
    initial: NotRequired[Any]
    priority: NotRequired[int]
    title: NotRequired[str]
    type: "WidgetKind"
    visible: NotRequired[bool]


class WidgetUpdateParams(TypedDict):
    """widgets/update params: idempotent full-state replacement, shaped per the widget kind."""
    id: str
    state: Any
    visible: NotRequired[bool]

