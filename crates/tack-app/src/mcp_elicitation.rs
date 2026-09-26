//! MCP elicitation (`elicitation/create`): the server asks the USER for
//! structured input (a JSON-schema-described form).
//!
//! Mode-dependent policy ([`elicitation_decision`] — unit-tested):
//! - TUI: a per-field text dialog collects input (see tui/mod.rs);
//! - print/rpc/acp/serve (headless): decline automatically — there is no
//!   user to ask, and blocking a server request forever is worse;
//! - `mcpElicitation: false`: capability is not advertised at all (rmcp's
//!   default handler still declines misbehaving servers gracefully).
//!
//! URL-mode elicitation is always declined (we never open browser flows
//! triggered by a server).

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::model::{ElicitRequestParams, ElicitResult, ElicitationAction};
use serde_json::{Map, Value};
use tokio::sync::oneshot;

/// One form field, flattened from the server's requested schema.
#[derive(Clone, Debug, PartialEq)]
pub struct ElicitField {
    pub name: String,
    /// "string" | "number" | "integer" | "boolean" | "enum"
    pub kind: String,
    pub description: Option<String>,
    pub required: bool,
    /// Enum choices when `kind == "enum"`.
    pub choices: Vec<String>,
}

/// An elicitation request forwarded to the TUI main loop.
#[derive(Debug)]
pub struct ElicitationQuery {
    pub server: String,
    pub message: String,
    pub fields: Vec<ElicitField>,
    /// (outcome, collected content) — content is only meaningful on Accept.
    pub respond: oneshot::Sender<(ElicitationOutcome, Value)>,
}

/// What the user decided in the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElicitationOutcome {
    /// All fields answered; `content` is ready to send back.
    Accept,
    /// User pressed Esc: cancel the operation.
    Cancel,
    /// We could not show the dialog (another one is open): decline but let
    /// the server continue.
    Decline,
}

/// Whether the session can show interactive dialogs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractionMode {
    Tui,
    /// print / rpc / acp / serve.
    Headless,
}

/// The handling decision for one elicitation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElicitationDecision {
    /// Forward to the user via the dialog channel.
    Prompt,
    /// Answer Decline immediately.
    Decline,
}

/// Pure policy: mode × setting → decision (unit-tested; the TUI dialog and
/// the headless decline are both driven by this).
pub fn elicitation_decision(mode: InteractionMode, enabled: bool) -> ElicitationDecision {
    match (mode, enabled) {
        (InteractionMode::Tui, true) => ElicitationDecision::Prompt,
        _ => ElicitationDecision::Decline,
    }
}

/// Flatten an elicitation JSON schema into per-field descriptors. The MCP
/// spec restricts elicitation schemas to objects with primitive properties,
/// so a shallow walk is sufficient.
pub fn fields_from_schema(schema: &Value) -> Vec<ElicitField> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    properties
        .iter()
        .map(|(name, def)| {
            let choices: Vec<String> = def
                .get("enum")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let kind = if !choices.is_empty() {
                "enum".to_string()
            } else {
                def.get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("string")
                    .to_string()
            };
            ElicitField {
                name: name.clone(),
                kind,
                description: def
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| def.get("title").and_then(Value::as_str).map(str::to_string)),
                required: required.contains(&name.as_str()),
                choices,
            }
        })
        .collect()
}

/// Coerce dialog text into the field's JSON type. Returns Err with a
/// user-facing message when the input doesn't fit (dialog re-asks).
pub fn coerce_field_value(field: &ElicitField, input: &str) -> Result<Value, String> {
    let input = input.trim();
    if input.is_empty() {
        if field.required {
            return Err(format!("{} is required", field.name));
        }
        // Optional and left empty: omit the field (caller skips Null).
        return Ok(Value::Null);
    }
    match field.kind.as_str() {
        "number" => input
            .parse::<f64>()
            .map(Value::from)
            .map_err(|_| format!("{} must be a number", field.name)),
        "integer" => input
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("{} must be an integer", field.name)),
        "boolean" => match input.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" => Ok(Value::Bool(true)),
            "false" | "no" | "n" | "0" => Ok(Value::Bool(false)),
            _ => Err(format!("{} must be true or false", field.name)),
        },
        "enum" if !field.choices.is_empty() => {
            if field.choices.iter().any(|c| c == input) {
                Ok(Value::String(input.to_string()))
            } else {
                Err(format!(
                    "{} must be one of: {}",
                    field.name,
                    field.choices.join(", ")
                ))
            }
        }
        _ => Ok(Value::String(input.to_string())),
    }
}

/// Serialize an `ElicitationSchema` (rmcp type) to plain JSON for the
/// UI-side field walk.
pub fn schema_to_json(schema: &rmcp::model::ElicitationSchema) -> Value {
    serde_json::to_value(schema).unwrap_or(Value::Null)
}

/// Build the content object from collected field values (skips Nulls =
/// unanswered optional fields).
pub fn collect_content(values: Vec<(String, Value)>) -> Value {
    let mut map = Map::new();
    for (name, value) in values {
        if !value.is_null() {
            map.insert(name, value);
        }
    }
    Value::Object(map)
}

/// Elicitation handler for the TUI: forwards the request over the app-event
/// channel and awaits the dialog outcome.
pub struct TuiElicitationHandler {
    tx: crate::tui::AppEventTx,
}

impl std::fmt::Debug for TuiElicitationHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiElicitationHandler").finish()
    }
}

impl TuiElicitationHandler {
    pub fn new(tx: crate::tui::AppEventTx) -> Self {
        TuiElicitationHandler { tx }
    }
}

#[async_trait]
impl tack_tools::mcp::ElicitationHandler for TuiElicitationHandler {
    async fn elicit(
        &self,
        server: &str,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, String> {
        match params {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => {
                let fields = fields_from_schema(&schema_to_json(&requested_schema));
                let (respond, rx) = oneshot::channel();
                self.tx
                    .send(crate::tui::AppEvent::McpElicitation(ElicitationQuery {
                        server: server.to_string(),
                        message,
                        fields,
                        respond,
                    }))
                    .map_err(|_| "TUI is shutting down".to_string())?;
                // Err(oneshot::Canceled) = dialog never answered (shutdown).
                let (outcome, content) = rx
                    .await
                    .map_err(|_| "elicitation dialog closed without an answer".to_string())?;
                match outcome {
                    ElicitationOutcome::Accept => {
                        Ok(ElicitResult::new(ElicitationAction::Accept).with_content(content))
                    }
                    ElicitationOutcome::Cancel => Ok(ElicitResult::new(ElicitationAction::Cancel)),
                    ElicitationOutcome::Decline => {
                        Ok(ElicitResult::new(ElicitationAction::Decline))
                    }
                }
            }
            // URL-mode would send the user to a server-chosen browser flow;
            // we don't follow those.
            ElicitRequestParams::UrlElicitationParams { .. } => {
                Ok(ElicitResult::new(ElicitationAction::Decline))
            }
            other => {
                let _ = other;
                Ok(ElicitResult::new(ElicitationAction::Decline))
            }
        }
    }
}

/// Dialog-side state for one in-flight elicitation (owned by the TUI while
/// it walks the fields one InputDialog at a time).
#[derive(Debug)]
pub struct PendingElicitation {
    pub server: String,
    pub message: String,
    /// Fields still to ask; index 0 is the field in the open dialog.
    pub remaining: std::collections::VecDeque<ElicitField>,
    pub collected: Vec<(String, Value)>,
    respond: oneshot::Sender<(ElicitationOutcome, Value)>,
}

impl PendingElicitation {
    pub fn new(query: ElicitationQuery) -> Self {
        PendingElicitation {
            server: query.server,
            message: query.message,
            remaining: query.fields.into(),
            collected: Vec::new(),
            respond: query.respond,
        }
    }

    /// The field the current dialog is asking about.
    pub fn current(&self) -> Option<&ElicitField> {
        self.remaining.front()
    }

    /// Record the current field's answer and advance.
    pub fn push_answer(&mut self, value: Value) {
        if let Some(field) = self.remaining.pop_front() {
            self.collected.push((field.name, value));
        }
    }

    fn respond(self, outcome: ElicitationOutcome) {
        let content = collect_content(self.collected);
        let _ = self.respond.send((outcome, content));
    }

    /// All fields answered (or nothing to ask): accept.
    pub fn finish(self) {
        self.respond(ElicitationOutcome::Accept);
    }

    pub fn cancel(self) {
        self.respond(ElicitationOutcome::Cancel);
    }

    pub fn decline(self) {
        self.respond(ElicitationOutcome::Decline);
    }
}

/// Build the elicitation callback for a session, honoring mode + setting.
/// Returns None when the capability should not be advertised at all
/// (headless modes rely on rmcp's default decline).
pub fn elicitation_callback(
    mode: InteractionMode,
    enabled: bool,
    tx: Option<crate::tui::AppEventTx>,
) -> Option<Arc<dyn tack_tools::mcp::ElicitationHandler>> {
    match (elicitation_decision(mode, enabled), tx) {
        (ElicitationDecision::Prompt, Some(tx)) => Some(Arc::new(TuiElicitationHandler::new(tx))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tack_tools::mcp::ElicitationHandler as _;

    #[test]
    fn decision_matrix() {
        assert_eq!(
            elicitation_decision(InteractionMode::Tui, true),
            ElicitationDecision::Prompt
        );
        assert_eq!(
            elicitation_decision(InteractionMode::Tui, false),
            ElicitationDecision::Decline
        );
        assert_eq!(
            elicitation_decision(InteractionMode::Headless, true),
            ElicitationDecision::Decline
        );
        assert_eq!(
            elicitation_decision(InteractionMode::Headless, false),
            ElicitationDecision::Decline
        );
    }

    #[test]
    fn callback_only_in_tui_with_channel() {
        let (tx, _rx) = crate::tui::app_event_bus();
        assert!(elicitation_callback(InteractionMode::Tui, true, Some(tx)).is_some());
        assert!(elicitation_callback(InteractionMode::Tui, false, None).is_none());
        assert!(elicitation_callback(InteractionMode::Headless, true, None).is_none());
    }

    #[test]
    fn schema_flattening() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Your name" },
                "age": { "type": "integer" },
                "subscribe": { "type": "boolean" },
                "color": { "type": "string", "enum": ["red", "blue"] }
            },
            "required": ["name", "color"]
        });
        let fields = fields_from_schema(&schema);
        assert_eq!(fields.len(), 4);
        let name = fields.iter().find(|f| f.name == "name").unwrap();
        assert!(name.required);
        assert_eq!(name.kind, "string");
        assert_eq!(name.description.as_deref(), Some("Your name"));
        let color = fields.iter().find(|f| f.name == "color").unwrap();
        assert_eq!(color.kind, "enum");
        assert_eq!(color.choices, vec!["red", "blue"]);
        let age = fields.iter().find(|f| f.name == "age").unwrap();
        assert!(!age.required);
        assert_eq!(age.kind, "integer");
    }

    #[test]
    fn coercion_rules() {
        let string = ElicitField {
            name: "s".into(),
            kind: "string".into(),
            description: None,
            required: true,
            choices: vec![],
        };
        assert_eq!(
            coerce_field_value(&string, " hi ").unwrap(),
            Value::String("hi".into())
        );
        assert!(coerce_field_value(&string, "").is_err());

        let integer = ElicitField {
            kind: "integer".into(),
            required: false,
            ..string.clone()
        };
        assert_eq!(coerce_field_value(&integer, "42").unwrap(), Value::from(42));
        assert!(coerce_field_value(&integer, "4.2").is_err());
        // Optional + empty → Null (omitted from content).
        assert_eq!(coerce_field_value(&integer, "").unwrap(), Value::Null);

        let boolean = ElicitField {
            kind: "boolean".into(),
            ..string.clone()
        };
        assert_eq!(
            coerce_field_value(&boolean, "yes").unwrap(),
            Value::Bool(true)
        );
        assert!(coerce_field_value(&boolean, "maybe").is_err());

        let enum_field = ElicitField {
            kind: "enum".into(),
            choices: vec!["red".into(), "blue".into()],
            ..string.clone()
        };
        assert_eq!(
            coerce_field_value(&enum_field, "red").unwrap(),
            Value::String("red".into())
        );
        assert!(coerce_field_value(&enum_field, "green").is_err());
    }

    #[test]
    fn collect_content_omits_unanswered_optionals() {
        let content = collect_content(vec![
            ("a".to_string(), Value::String("x".into())),
            ("b".to_string(), Value::Null),
        ]);
        assert_eq!(content, serde_json::json!({ "a": "x" }));
    }

    #[tokio::test]
    async fn url_elicitation_is_declined() {
        let (tx, _rx) = crate::tui::app_event_bus();
        let handler = TuiElicitationHandler::new(tx);
        let result = handler
            .elicit(
                "srv",
                ElicitRequestParams::UrlElicitationParams {
                    meta: None,
                    message: "open this".into(),
                    url: "https://example.com".into(),
                    elicitation_id: "e1".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(result.action, ElicitationAction::Decline);
    }

    #[tokio::test]
    async fn form_cancel_maps_to_cancel_action() {
        let (tx, rx) = crate::tui::app_event_bus();
        let handler = TuiElicitationHandler::new(tx);
        let call = tokio::spawn(async move {
            handler
                .elicit(
                    "srv",
                    ElicitRequestParams::FormElicitationParams {
                        meta: None,
                        message: "who?".into(),
                        requested_schema: rmcp::model::ElicitationSchema::builder()
                            .required_string("name")
                            .build()
                            .unwrap(),
                    },
                )
                .await
        });
        // Simulate the user pressing Esc in the dialog.
        let Some(crate::tui::AppEvent::McpElicitation(query)) = rx.recv().await else {
            panic!("expected elicitation query");
        };
        assert_eq!(query.server, "srv");
        assert_eq!(query.message, "who?");
        assert_eq!(query.fields.len(), 1);
        query
            .respond
            .send((ElicitationOutcome::Cancel, Value::Null))
            .unwrap();
        let result = call.await.unwrap().unwrap();
        assert_eq!(result.action, ElicitationAction::Cancel);
        assert!(result.content.is_none());
    }
}
