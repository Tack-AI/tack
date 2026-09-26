//! tack-ext protocol: newline-delimited JSON (NDJSON) between the host and a
//! plugin subprocess. One object per line, three envelope kinds:
//!
//! ```json
//! {"type":"request","id":1,"method":"tool.execute","params":{...}}
//! {"type":"response","id":1,"result":{...}}            (or {"error":"..."})
//! {"type":"event","event":"agent_start","payload":{...}}
//! ```
//!
//! Both sides may issue requests concurrently (ids are per-sender). The
//! handshake: host spawns the plugin, sends `initialize` (protocol version,
//! mode, cwd, trust state); the plugin answers with a `register` event
//! carrying its tools/commands/subscriptions. Host requests use methods
//! `tool.execute`, `command.invoke`, `intercept.tool_call`; plugin requests
//! use `ui.*`, `session.*`, `exec`, `log`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire protocol version (bump on breaking changes).
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Envelope {
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    Response {
        id: u64,
        // Present-but-null is a REAL null result (e.g. a cancelled
        // ui.input) — only a MISSING key means "no result". Option<Value>
        // alone collapses the two, and the peer would report a legitimate
        // null result as a malformed response.
        #[serde(
            default,
            deserialize_with = "de_explicit_null",
            skip_serializing_if = "Option::is_none"
        )]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Event {
        event: String,
        payload: Value,
    },
}

/// Deserialize a present key (even `"result": null`) as `Some(value)`;
/// a missing key falls back to the `default` (None).
fn de_explicit_null<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Value::deserialize(deserializer)?))
}

impl Envelope {
    pub fn request(id: u64, method: impl Into<String>, params: Value) -> Self {
        Envelope::Request {
            id,
            method: method.into(),
            params,
        }
    }

    pub fn result(id: u64, result: Value) -> Self {
        Envelope::Response {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u64, error: impl Into<String>) -> Self {
        Envelope::Response {
            id,
            result: None,
            error: Some(error.into()),
        }
    }

    pub fn event(event: impl Into<String>, payload: Value) -> Self {
        Envelope::Event {
            event: event.into(),
            payload,
        }
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializePayload {
    pub protocol: u32,
    /// "tui" | "print" | "rpc" | "acp"
    pub mode: String,
    pub cwd: String,
    pub trusted: bool,
    /// Host name + version for user agents / logs.
    pub host: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterPayload {
    /// Protocol version the plugin was built against. Absent in
    /// pre-versioning plugins (serde default keeps them loadable); the
    /// host warns on absent/older versions and rejects newer ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub commands: Vec<CommandSpec>,
    #[serde(default)]
    pub shortcuts: Vec<ShortcutSpec>,
    /// Event names the plugin wants (empty = the default lifecycle set;
    /// "message_update" is opt-in because of its frequency).
    #[serde(default)]
    pub subscriptions: Vec<String>,
    /// Declarative widgets the plugin wants the host to render (v2.1).
    /// Optional so v1 hosts/plugins interoperate: serde ignores unknown
    /// fields on old hosts, and an absent field deserializes as empty.
    #[serde(default)]
    pub widgets: Vec<WidgetSpec>,
    /// Autocomplete providers for the input line (v2.2); wire name is
    /// `autocompleteProviders`.
    #[serde(default)]
    pub autocomplete_providers: Vec<AutocompleteProviderSpec>,
}

// ---------------------------------------------------------------------------
// Declarative widgets (v2.1) and autocomplete providers (v2.2)
// ---------------------------------------------------------------------------
// The plugin DECLARES long-lived UI units at register time; the host owns
// rendering and layout, and the plugin pushes full-state snapshots via the
// `widget.update` event. User interaction comes back as `widget.action`
// (host → plugin). See docs/extensions-v2.md §3.

/// A declared UI unit (`RegisterPayload::widgets` entry).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetSpec {
    /// Unique within the plugin; the host keys it as `<plugin>:<id>`.
    pub id: String,
    /// Wire name is `type` (see docs/extensions-v2.md §3.1).
    #[serde(rename = "type")]
    pub kind: WidgetKind,
    /// Sort key for status line segments (smaller first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// Panel title (required by contract for panel kinds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Initial visibility for panel kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    /// Initial state, shaped per `kind` (see the `*State` structs below).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<Value>,
}

/// The kind of a declared widget (wire values are snake_case).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    StatusLineSegment,
    MarkdownPanel,
    ListPanel,
}

/// State of a `status_line_segment` widget: the shape of
/// `WidgetSpec::initial` / `WidgetUpdatePayload::state` when
/// `kind == WidgetKind::StatusLineSegment`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusLineState {
    /// Empty text hides the segment; the host truncates to one line.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<StatusStyle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}

/// Status line segment style (wire values are snake_case).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusStyle {
    Default,
    Info,
    Warning,
    Error,
    Dim,
}

/// State of a `markdown_panel` widget (`kind == WidgetKind::MarkdownPanel`):
/// markdown source rendered with the host's pulldown-cmark pipeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownPanelState {
    pub markdown: String,
}

/// State of a `list_panel` widget (`kind == WidgetKind::ListPanel`). The
/// host renders the selection; a user pick is reported back as a
/// `widget.action` event with `action == "select"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPanelState {
    pub items: Vec<ListPanelItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPanelItem {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// Payload of the plugin → host `widget.update` event: an idempotent
/// full-state replacement (not a diff); dropped frames are harmless.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetUpdatePayload {
    pub id: String,
    /// Shaped per the widget's kind (see the `*State` structs above).
    pub state: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
}

/// Payload of the host → plugin `widget.action` event reporting user
/// interaction (never produced in headless modes).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetActionPayload {
    pub id: String,
    /// e.g. "select" for list panels.
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

/// An autocomplete provider for the input line
/// (`RegisterPayload::autocomplete_providers` entry, v2.2).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutocompleteProviderSpec {
    pub id: String,
    /// Token prefix that triggers the provider (e.g. "#").
    pub trigger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `autocomplete.provide` params (host → plugin request; timeouts and
/// UI-level cancellation silently degrade to no suggestions).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutocompleteProvideParams {
    pub provider_id: String,
    pub query: String,
    pub cursor_offset: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutocompleteSuggestion {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Text inserted on accept; absent = insert `value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_text: Option<String>,
}

/// `autocomplete.provide` result; an empty `suggestions` vec is a legal
/// "no suggestions" answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutocompleteProvideResult {
    #[serde(default)]
    pub suggestions: Vec<AutocompleteSuggestion>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Defaulted so a tool WITHOUT the field (upstream #9300's exact case)
    /// deserializes as `null` and is then rejected by
    /// [`ToolSpec::validate_parameters`] with the upstream wording —
    /// instead of failing the whole handshake register payload (which used
    /// to surface as a 10s handshake timeout).
    #[serde(default)]
    pub parameters: Value,
}

impl ToolSpec {
    /// Validate the parameter schema at registration time (upstream pi
    /// acaa253cc, #9300): `parameters` must be a JSON object — an array,
    /// string, or null schema would otherwise flow into the provider
    /// request and break its serialization. Returns the upstream-worded
    /// rejection message for a malformed schema.
    pub fn validate_parameters(&self, extension: &str) -> Result<(), String> {
        if self.parameters.is_object() {
            Ok(())
        } else {
            Err(format!(
                "Tool \"{}\" registered by extension \"{extension}\" must define an object parameter schema.",
                self.name
            ))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutSpec {
    /// Action id, e.g. "ext.my-ext.myAction".
    pub action: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub description: String,
}

// ---------------------------------------------------------------------------
// Host → plugin requests
// ---------------------------------------------------------------------------

/// `tool.execute` params: run a plugin-registered tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecuteParams {
    pub name: String,
    pub tool_call_id: String,
    pub arguments: Value,
}

/// `command.invoke` params: run a plugin-registered slash command.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandInvokeParams {
    pub name: String,
    #[serde(default)]
    pub args: String,
}

/// `intercept.tool_call` params: a built-in tool is about to run; the plugin
/// may allow, deny, or rewrite the arguments.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallInterceptParams {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

/// Intercept verdict returned by the plugin.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ToolCallVerdict {
    Allow,
    /// Deny execution; `reason` becomes the (error) tool result.
    Deny {
        reason: String,
    },
    /// Allow with rewritten arguments.
    Rewrite {
        arguments: Value,
    },
}

// ---------------------------------------------------------------------------
// Plugin → host requests
// ---------------------------------------------------------------------------

/// `ui.notify` params.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyParams {
    pub message: String,
    /// "info" | "warning" | "error"
    #[serde(default = "default_level")]
    pub level: String,
}

fn default_level() -> String {
    "info".to_string()
}

/// `ui.select` params/result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectParams {
    pub title: String,
    pub options: Vec<String>,
}

/// `ui.confirm` params/result (bool).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfirmParams {
    pub title: String,
    pub message: String,
}

/// `ui.input` params/result (string or null on cancel).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputParams {
    pub title: String,
    #[serde(default)]
    pub placeholder: Option<String>,
}

/// `ui.set_status` params (null clears).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetStatusParams {
    #[serde(default)]
    pub text: Option<String>,
}

/// `exec` params: run a shell command on the host (trust-gated).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecParams {
    pub command: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// `log` event payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogPayload {
    #[serde(default = "default_level")]
    pub level: String,
    pub message: String,
}

/// `session.set_label` params (TUI status area label).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetLabelParams {
    #[serde(default)]
    pub label: Option<String>,
}

/// Event names the host forwards without an explicit subscription.
pub const DEFAULT_EVENTS: &[&str] = &[
    "session_start",
    "session_shutdown",
    "agent_start",
    "agent_end",
    "turn_start",
    "turn_end",
    "message_start",
    "message_end",
    "tool_execution_start",
    "tool_execution_end",
    "model_select",
    "thinking_level_select",
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn envelope_roundtrip() {
        let request = Envelope::request(7, "tool.execute", serde_json::json!({"name": "t"}));
        let line = serde_json::to_string(&request).unwrap();
        let parsed: Envelope = serde_json::from_str(&line).unwrap();
        let Envelope::Request { id, method, .. } = parsed else {
            panic!("request")
        };
        assert_eq!(id, 7);
        assert_eq!(method, "tool.execute");

        let response = Envelope::error(7, "boom");
        let parsed: Envelope =
            serde_json::from_str(&serde_json::to_string(&response).unwrap()).unwrap();
        let Envelope::Response { id, error, result } = parsed else {
            panic!("response")
        };
        assert_eq!(id, 7);
        assert_eq!(error.as_deref(), Some("boom"));
        assert!(result.is_none());

        let event = Envelope::event("agent_start", serde_json::json!({}));
        let parsed: Envelope =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        let Envelope::Event { event, .. } = parsed else {
            panic!("event")
        };
        assert_eq!(event, "agent_start");
    }

    #[test]
    fn verdict_shapes() {
        let deny = ToolCallVerdict::Deny {
            reason: "nope".to_string(),
        };
        let json = serde_json::to_value(&deny).unwrap();
        assert_eq!(json["action"], "deny");
        let parsed: ToolCallVerdict = serde_json::from_value(json).unwrap();
        assert!(matches!(parsed, ToolCallVerdict::Deny { .. }));
    }

    /// A null result is a legitimate value (ui.input cancelled, ui.select
    /// dismissed) and must round-trip as Some(Null) — not collapse into
    /// "no result", which the peer reports as a malformed response.
    #[test]
    fn null_result_roundtrips_as_result_not_absence() {
        let response = Envelope::result(9, Value::Null);
        let line = serde_json::to_string(&response).unwrap();
        let parsed: Envelope = serde_json::from_str(&line).unwrap();
        let Envelope::Response { id, result, error } = parsed else {
            panic!("response")
        };
        assert_eq!(id, 9);
        assert_eq!(result, Some(Value::Null), "null result lost: {line}");
        assert!(error.is_none());

        // A response with neither key is still distinguishable (malformed).
        let parsed: Envelope = serde_json::from_str(r#"{"type":"response","id":9}"#).unwrap();
        let Envelope::Response { result, error, .. } = parsed else {
            panic!("response")
        };
        assert!(result.is_none());
        assert!(error.is_none());
    }

    // ------------------------------------------------------------------
    // Widgets (v2.1) and autocomplete providers (v2.2)
    // ------------------------------------------------------------------

    #[test]
    fn widget_spec_wire_shape() {
        let spec = WidgetSpec {
            id: "branch".to_string(),
            kind: WidgetKind::StatusLineSegment,
            priority: Some(50),
            title: None,
            visible: None,
            initial: Some(serde_json::json!({"text": "main", "style": "dim"})),
        };
        let json = serde_json::to_value(&spec).unwrap();
        // `kind` is named "type" on the wire; kind values are snake_case.
        assert_eq!(json["type"], "status_line_segment");
        assert!(json.get("kind").is_none(), "rust field name leaked: {json}");
        // Absent options are omitted, not null.
        assert!(json.get("title").is_none(), "{json}");
        assert!(json.get("visible").is_none(), "{json}");

        let parsed: WidgetSpec = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.kind, WidgetKind::StatusLineSegment);
        assert_eq!(parsed.priority, Some(50));

        // The other kinds rename to snake_case too.
        for (kind, wire) in [
            (WidgetKind::StatusLineSegment, "status_line_segment"),
            (WidgetKind::MarkdownPanel, "markdown_panel"),
            (WidgetKind::ListPanel, "list_panel"),
        ] {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, wire);
            let parsed: WidgetKind = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    /// The register payload from docs/extensions-v2.md §3.1 must parse.
    #[test]
    fn widget_spec_parses_doc_example() {
        let spec: WidgetSpec = serde_json::from_str(
            r#"{"id": "diff-panel", "type": "markdown_panel",
                "title": "Pending diff", "visible": false}"#,
        )
        .unwrap();
        assert_eq!(spec.kind, WidgetKind::MarkdownPanel);
        assert_eq!(spec.title.as_deref(), Some("Pending diff"));
        assert_eq!(spec.visible, Some(false));
        assert!(spec.priority.is_none());
        assert!(spec.initial.is_none());
    }

    #[test]
    fn status_style_renames_snake_case() {
        for (style, wire) in [
            (StatusStyle::Default, "default"),
            (StatusStyle::Info, "info"),
            (StatusStyle::Warning, "warning"),
            (StatusStyle::Error, "error"),
            (StatusStyle::Dim, "dim"),
        ] {
            let json = serde_json::to_value(style).unwrap();
            assert_eq!(json, wire);
            let parsed: StatusStyle = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, style);
        }
    }

    #[test]
    fn widget_states_roundtrip() {
        let status: StatusLineState =
            serde_json::from_str(r#"{"text": "main", "style": "dim", "tooltip": "branch"}"#)
                .unwrap();
        assert_eq!(status.style, Some(StatusStyle::Dim));
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["tooltip"], "branch");

        let panel: MarkdownPanelState = serde_json::from_str(r##"{"markdown": "# Hi"}"##).unwrap();
        assert_eq!(panel.markdown, "# Hi");

        // camelCase: selectedId; absent detail/icon tolerated and omitted.
        let list = ListPanelState {
            items: vec![ListPanelItem {
                id: "src/main.rs".to_string(),
                label: "main.rs".to_string(),
                detail: None,
                icon: None,
            }],
            selected_id: Some("src/main.rs".to_string()),
        };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["selectedId"], "src/main.rs");
        assert!(json["items"][0].get("detail").is_none(), "{json}");
        let parsed: ListPanelState = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.selected_id.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn widget_event_payloads_roundtrip() {
        let update = WidgetUpdatePayload {
            id: "branch".to_string(),
            state: serde_json::json!({"text": "feature/wasm", "style": "info"}),
            visible: None,
        };
        let envelope = Envelope::event("widget.update", serde_json::to_value(&update).unwrap());
        let line = serde_json::to_string(&envelope).unwrap();
        assert!(line.contains("widget.update"), "{line}");
        // `visible` omitted when absent.
        assert!(!line.contains("visible"), "{line}");
        let Envelope::Event { event, payload } = serde_json::from_str(&line).unwrap() else {
            panic!("event")
        };
        assert_eq!(event, "widget.update");
        let parsed: WidgetUpdatePayload = serde_json::from_value(payload).unwrap();
        assert_eq!(parsed.id, "branch");
        assert_eq!(parsed.state["style"], "info");

        // Doc §3.2 example: itemId is camelCase.
        let action: WidgetActionPayload =
            serde_json::from_str(r#"{"id": "files", "action": "select", "itemId": "src/main.rs"}"#)
                .unwrap();
        assert_eq!(action.item_id.as_deref(), Some("src/main.rs"));
        let json = serde_json::to_value(&action).unwrap();
        assert_eq!(json["itemId"], "src/main.rs");
    }

    #[test]
    fn autocomplete_wire_shape() {
        let spec = AutocompleteProviderSpec {
            id: "issues".to_string(),
            trigger: "#".to_string(),
            description: None,
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert!(json.get("description").is_none(), "{json}");

        let params = AutocompleteProvideParams {
            provider_id: "issues".to_string(),
            query: "wasm".to_string(),
            cursor_offset: 5,
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["providerId"], "issues");
        assert_eq!(json["cursorOffset"], 5);
        let parsed: AutocompleteProvideParams = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.cursor_offset, 5);

        let result = AutocompleteProvideResult {
            suggestions: vec![AutocompleteSuggestion {
                value: "#1234".to_string(),
                label: "#1234 WASM carrier".to_string(),
                detail: Some("open".to_string()),
                insert_text: None,
            }],
        };
        let json = serde_json::to_value(&result).unwrap();
        // insertText absent (host falls back to `value`); detail present.
        assert!(json["suggestions"][0].get("insertText").is_none(), "{json}");
        assert_eq!(json["suggestions"][0]["detail"], "open");
        let parsed: AutocompleteProvideResult = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.suggestions.len(), 1);

        // Empty suggestions is a legal "no suggestions" result.
        let parsed: AutocompleteProvideResult =
            serde_json::from_str(r#"{"suggestions": []}"#).unwrap();
        assert!(parsed.suggestions.is_empty());
    }

    /// Regression test for #9300: tool parameter schemas must be JSON
    /// objects; anything else is rejected at registration.
    #[test]
    fn tool_spec_parameters_must_be_object() {
        let spec = |parameters: Value| ToolSpec {
            name: "noop".to_string(),
            label: None,
            description: "Do nothing".to_string(),
            parameters,
        };
        assert!(
            spec(serde_json::json!({"type": "object"}))
                .validate_parameters("ext")
                .is_ok()
        );
        assert!(
            spec(serde_json::json!({}))
                .validate_parameters("ext")
                .is_ok()
        );
        for bad in [
            serde_json::json!([{"type": "object"}]),
            serde_json::json!("schema"),
            serde_json::json!(null),
            serde_json::json!(42),
        ] {
            let err = spec(bad)
                .validate_parameters("/exts/missing-parameters.js")
                .unwrap_err();
            assert_eq!(
                err,
                "Tool \"noop\" registered by extension \"/exts/missing-parameters.js\" must define an object parameter schema."
            );
        }
    }

    /// The exact upstream #9300 case: a tool with NO `parameters` field at
    /// all must still deserialize (as `null`) so the registration-time
    /// validation can reject it with the upstream wording — rather than
    /// failing the whole handshake payload.
    #[test]
    fn tool_spec_without_parameters_field_deserializes_then_rejects() {
        let spec: ToolSpec = serde_json::from_value(serde_json::json!({
            "name": "noop",
            "description": "Do nothing"
        }))
        .unwrap();
        assert!(spec.parameters.is_null());
        let err = spec
            .validate_parameters("/exts/missing-parameters.js")
            .unwrap_err();
        assert!(err.contains("must define an object parameter schema"));
    }

    /// v1 register payloads (no widgets/autocompleteProviders) still parse;
    /// unknown fields from a newer plugin are ignored (serde convention).
    #[test]
    fn register_payload_backward_and_forward_compatible() {
        let old: RegisterPayload = serde_json::from_str(
            r#"{"name": "git-status", "tools": [],
                "subscriptions": ["agent_start"]}"#,
        )
        .unwrap();
        assert!(old.widgets.is_empty());
        assert!(old.autocomplete_providers.is_empty());

        // A v2 payload with the new fields plus a hypothetical future field.
        let new: RegisterPayload = serde_json::from_str(
            r##"{"name": "git-status",
                "widgets": [
                    {"id": "branch", "type": "status_line_segment",
                     "priority": 50, "initial": {"text": "main"}}
                ],
                "autocompleteProviders": [
                    {"id": "issues", "trigger": "#", "description": "GitHub issues"}
                ],
                "futureField": {"anything": true}}"##,
        )
        .unwrap();
        assert_eq!(new.widgets.len(), 1);
        assert_eq!(new.widgets[0].kind, WidgetKind::StatusLineSegment);
        assert_eq!(new.autocomplete_providers.len(), 1);
        assert_eq!(new.autocomplete_providers[0].trigger, "#");

        // Roundtrip keeps the camelCase wire name.
        let json = serde_json::to_value(&new).unwrap();
        assert!(json.get("autocompleteProviders").is_some(), "{json}");
        assert!(json.get("autocomplete_providers").is_none(), "{json}");
        let parsed: RegisterPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.widgets.len(), 1);
    }
}
