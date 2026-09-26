//! Todo list tool (rpiv-todo as a built-in): a shared, session-persisted
//! task list. The tool mutates a shared `TodoState` and returns the full
//! list in `result.details.todos`; the TUI renders a persistent panel and
//! rebuilds the state from the latest todo tool result in the session
//! history (so it survives compaction and resume).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::services::ToolServices;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TodoItem {
    pub id: u64,
    pub text: String,
    /// "pending" | "in_progress" | "done"
    pub status: String,
}

#[derive(Clone, Debug, Default)]
pub struct TodoState {
    pub items: Vec<TodoItem>,
    pub next_id: u64,
}

impl TodoState {
    /// Rebuild from a serialized todos array (session replay).
    pub fn from_json(value: &Value) -> Self {
        let items: Vec<TodoItem> = value
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        // Empty list starts ids at 0, matching `TodoState::default()`.
        let next_id = items.iter().map(|i| i.id).max().map_or(0, |max| max + 1);
        TodoState { items, next_id }
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(&self.items).unwrap_or(Value::Null)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

pub struct TodoTool {
    state: Arc<tokio::sync::Mutex<TodoState>>,
    #[allow(dead_code)]
    services: ToolServices,
}

impl TodoTool {
    pub fn new(services: ToolServices, state: Arc<tokio::sync::Mutex<TodoState>>) -> Self {
        TodoTool { state, services }
    }
}

impl std::fmt::Debug for TodoTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TodoTool").finish()
    }
}

#[async_trait::async_trait]
impl AgentTool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn label(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Manage a persistent task list for this session: add items, mark them in_progress/done, \
         list, or clear. Use it to track multi-step work; the list survives compaction. \
         Discipline (the list is shown to the user live): mark an item in_progress WHEN you \
         start it and complete it THE MOMENT it is done — never batch-close everything at the \
         end. Multiple items may be in_progress at once (e.g. parallel subagent batches). \
         add accepts one text or an array of texts; update/complete accept id or ids."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "update", "complete", "clear", "list"],
                    "description": "add: append item(s); update: change text/status; complete: mark done; clear: remove all; list: show current list"
                },
                "id": { "type": "number", "description": "Item id (single-item update/complete)" },
                "ids": {
                    "type": "array",
                    "items": { "type": "number" },
                    "description": "Item ids (batch update/complete)"
                },
                "text": {
                    "oneOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ],
                    "description": "Item text (add: one string or an array of strings; update: single string)"
                },
                "status": { "type": "string", "enum": ["pending", "in_progress", "done"], "description": "New status (for update)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required parameter: action".to_string())?;
        let mut state = self.state.lock().await;
        // Track whether the mutation should end with a "next up" hint
        // (status transitions only — not add/clear/list).
        let mut hint_next = false;
        let note = match action {
            "add" => {
                // text: one string or an array of strings.
                let texts: Vec<String> = match params.get("text") {
                    Some(Value::String(s)) => vec![s.clone()],
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .map(str::to_string)
                                .ok_or_else(|| "text array items must be strings".to_string())
                        })
                        .collect::<Result<_, _>>()?,
                    Some(_) => {
                        return Err("text must be a string or an array of strings".to_string());
                    }
                    None => return Err("add requires text".to_string()),
                };
                if texts.is_empty() {
                    return Err("text must not be an empty array".to_string());
                }
                if texts.iter().any(|t| t.trim().is_empty()) {
                    return Err("text must not be empty".to_string());
                }
                let first = state.next_id;
                for text in &texts {
                    let id = state.next_id;
                    state.next_id += 1;
                    state.items.push(TodoItem {
                        id,
                        text: text.clone(),
                        status: "pending".to_string(),
                    });
                }
                if texts.len() == 1 {
                    format!("added #{first}")
                } else {
                    format!(
                        "added {} items (#{}-#{})",
                        texts.len(),
                        first,
                        state.next_id - 1
                    )
                }
            }
            "update" => {
                let ids = parse_ids(&params)?.ok_or("update requires id or ids")?;
                let new_text = params.get("text").and_then(Value::as_str);
                let new_status = params.get("status").and_then(Value::as_str);
                if let Some(status) = new_status
                    && !matches!(status, "pending" | "in_progress" | "done")
                {
                    return Err(format!("invalid status {status:?}"));
                }
                if new_text.is_some() && ids.len() > 1 {
                    return Err(
                        "text applies to a single id; use status for batch updates".to_string()
                    );
                }
                // All-or-nothing: validate every id before mutating.
                let missing: Vec<u64> = ids
                    .iter()
                    .filter(|id| !state.items.iter().any(|i| i.id == **id))
                    .copied()
                    .collect();
                if !missing.is_empty() {
                    return Err(format!("no todo item(s): {}", fmt_ids(&missing)));
                }
                for item in state.items.iter_mut().filter(|i| ids.contains(&i.id)) {
                    if let Some(text) = new_text {
                        item.text = text.to_string();
                    }
                    if let Some(status) = new_status {
                        item.status = status.to_string();
                    }
                }
                // Multiple items may be in_progress at once — the agent
                // often runs parallel subagent batches, and the panel
                // should reflect reality.
                hint_next = new_status.is_some();
                format!("updated {}", fmt_ids(&ids))
            }
            "complete" => {
                let ids = parse_ids(&params)?.ok_or("complete requires id or ids")?;
                let missing: Vec<u64> = ids
                    .iter()
                    .filter(|id| !state.items.iter().any(|i| i.id == **id))
                    .copied()
                    .collect();
                if !missing.is_empty() {
                    return Err(format!("no todo item(s): {}", fmt_ids(&missing)));
                }
                for item in state.items.iter_mut().filter(|i| ids.contains(&i.id)) {
                    item.status = "done".to_string();
                }
                hint_next = true;
                format!("completed {}", fmt_ids(&ids))
            }
            "clear" => {
                let n = state.items.len();
                state.items.clear();
                format!("cleared {n} item(s)")
            }
            "list" => {
                if state.items.is_empty() {
                    "(empty todo list)".to_string()
                } else {
                    String::new()
                }
            }
            other => return Err(format!("unknown action {other:?}")),
        };

        let mut text = String::new();
        if !note.is_empty() {
            text.push_str(&format!("{note}\n"));
        }
        for item in &state.items {
            let marker = match item.status.as_str() {
                "done" => "☑",
                "in_progress" => "◐",
                _ => "☐",
            };
            text.push_str(&format!("{marker} #{} {}\n", item.id, item.text));
        }
        // "Close one, start one": after a status transition with nothing in
        // flight, point at the oldest pending item.
        if hint_next
            && !state.items.iter().any(|i| i.status == "in_progress")
            && let Some(next) = state.items.iter().find(|i| i.status == "pending")
        {
            text.push_str(&format!("→ next: #{} {}\n", next.id, next.text));
        }
        Ok(AgentToolResult {
            content: vec![tack_ai::InputContentBlock::text(
                text.trim_end().to_string(),
            )],
            details: json!({ "todos": state.to_json() }),
            usage: None,
            terminate: false,
            added_tool_names: None,
        })
    }
}

/// `id` (single) or `ids` (batch), never both.
fn parse_ids(params: &Value) -> Result<Option<Vec<u64>>, String> {
    let id = params.get("id").and_then(Value::as_u64);
    let ids = params.get("ids").and_then(Value::as_array);
    match (id, ids) {
        (Some(_), Some(_)) => Err("pass id or ids, not both".to_string()),
        (Some(i), None) => Ok(Some(vec![i])),
        (None, Some(arr)) => {
            if arr.is_empty() {
                return Err("ids must not be empty".to_string());
            }
            let ids: Result<Vec<u64>, _> = arr
                .iter()
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| "ids items must be numbers".to_string())
                })
                .collect();
            Ok(Some(ids?))
        }
        (None, None) => Ok(None),
    }
}

/// "#1, #2, #3" for notes.
fn fmt_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn todo_lifecycle_and_serialization() {
        let state = Arc::new(tokio::sync::Mutex::new(TodoState::default()));
        let tool = TodoTool::new(
            crate::services::ToolServices::new(std::env::temp_dir()),
            state.clone(),
        );
        let cancel = CancellationToken::new();

        tool.execute(
            "c1",
            json!({ "action": "add", "text": "write tests" }),
            cancel.clone(),
            &|_| {},
        )
        .await
        .unwrap();
        let result = tool
            .execute(
                "c2",
                json!({ "action": "add", "text": "ship it" }),
                cancel.clone(),
                &|_| {},
            )
            .await
            .unwrap();
        let todos = &result.details["todos"];
        assert_eq!(todos.as_array().unwrap().len(), 2);

        tool.execute(
            "c3",
            json!({ "action": "complete", "id": 0 }),
            cancel.clone(),
            &|_| {},
        )
        .await
        .unwrap();
        let result = tool
            .execute("c4", json!({ "action": "list" }), cancel.clone(), &|_| {})
            .await
            .unwrap();
        let text = match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text block"),
        };
        assert!(text.contains("☑ #0 write tests"), "{text}");
        assert!(text.contains("☐ #1 ship it"), "{text}");

        // Round-trip through the serialized form (session replay path).
        let rebuilt = TodoState::from_json(&result.details["todos"]);
        assert_eq!(rebuilt.items.len(), 2);
        assert_eq!(rebuilt.items[0].status, "done");
        assert_eq!(rebuilt.next_id, 2);
    }

    #[test]
    fn from_json_empty_matches_default() {
        // Regression: replaying an empty todo list must start ids where a
        // fresh state would (0), not skip to 1.
        assert_eq!(
            TodoState::from_json(&json!([])).next_id,
            TodoState::default().next_id
        );
    }

    fn test_tool() -> (
        TodoTool,
        Arc<tokio::sync::Mutex<TodoState>>,
        CancellationToken,
    ) {
        let state = Arc::new(tokio::sync::Mutex::new(TodoState::default()));
        let tool = TodoTool::new(
            crate::services::ToolServices::new(std::env::temp_dir()),
            state.clone(),
        );
        (tool, state, CancellationToken::new())
    }

    fn result_text(result: &AgentToolResult) -> String {
        match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn batch_add_and_complete() {
        let (tool, state, cancel) = test_tool();
        let result = tool
            .execute(
                "c1",
                json!({ "action": "add", "text": ["one", "two", "three"] }),
                cancel.clone(),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(result_text(&result).contains("added 3 items (#0-#2)"));

        let result = tool
            .execute(
                "c2",
                json!({ "action": "complete", "ids": [0, 2] }),
                cancel.clone(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(text.contains("completed #0, #2"), "{text}");
        assert!(text.contains("☑ #0"), "{text}");
        assert!(text.contains("☐ #1"), "{text}");
        // Next-up hint points at the oldest pending item.
        assert!(text.contains("→ next: #1 two"), "{text}");
        // NB: bind statuses by value — `&state.lock().await.items[..]`
        // lifetime-extends the guard to end of fn and deadlocks the next
        // tool.execute.
        let statuses = state
            .lock()
            .await
            .items
            .iter()
            .map(|i| i.status.clone())
            .collect::<Vec<_>>();
        assert_eq!(statuses, ["done", "pending", "done"]);

        // id + ids together is rejected.
        assert!(
            tool.execute(
                "c3",
                json!({ "action": "complete", "id": 1, "ids": [1] }),
                cancel.clone(),
                &|_| {},
            )
            .await
            .is_err()
        );
        // Batch with a missing id fails without partial mutation.
        assert!(
            tool.execute(
                "c4",
                json!({ "action": "complete", "ids": [1, 99] }),
                cancel.clone(),
                &|_| {},
            )
            .await
            .is_err()
        );
        assert_eq!(state.lock().await.items[1].status, "pending");
    }

    #[tokio::test]
    async fn multiple_in_progress_allowed() {
        let (tool, state, cancel) = test_tool();
        tool.execute(
            "c1",
            json!({ "action": "add", "text": ["first", "second"] }),
            cancel.clone(),
            &|_| {},
        )
        .await
        .unwrap();
        tool.execute(
            "c2",
            json!({ "action": "update", "id": 0, "status": "in_progress" }),
            cancel.clone(),
            &|_| {},
        )
        .await
        .unwrap();
        // Starting #1 while #0 is in flight keeps both in_progress —
        // parallel batches are legitimate.
        let result = tool
            .execute(
                "c3",
                json!({ "action": "update", "id": 1, "status": "in_progress" }),
                cancel.clone(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(!text.contains("auto-paused"), "{text}");
        let items = state
            .lock()
            .await
            .items
            .iter()
            .map(|i| i.status.clone())
            .collect::<Vec<_>>();
        assert_eq!(items, ["in_progress", "in_progress"]);
        // Panel shows both ◐.
        assert_eq!(text.matches('◐').count(), 2, "{text}");
        // Completing both in-flight items suggests the next pending one.
        tool.execute(
            "c4",
            json!({ "action": "add", "text": "third" }),
            cancel.clone(),
            &|_| {},
        )
        .await
        .unwrap();
        let result = tool
            .execute(
                "c5",
                json!({ "action": "complete", "ids": [0, 1] }),
                cancel.clone(),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(result_text(&result).contains("→ next: #2 third"));
    }

    #[tokio::test]
    async fn add_rejects_empty_and_bad_shapes() {
        let (tool, _state, cancel) = test_tool();
        assert!(
            tool.execute(
                "c1",
                json!({ "action": "add", "text": [] }),
                cancel.clone(),
                &|_| {}
            )
            .await
            .is_err()
        );
        assert!(
            tool.execute(
                "c2",
                json!({ "action": "add", "text": ["ok", "  "] }),
                cancel.clone(),
                &|_| {}
            )
            .await
            .is_err()
        );
        assert!(
            tool.execute(
                "c3",
                json!({ "action": "add", "text": [1, 2] }),
                cancel.clone(),
                &|_| {}
            )
            .await
            .is_err()
        );
        // Batch text update is rejected (text is single-item only).
        tool.execute(
            "c4",
            json!({ "action": "add", "text": ["a", "b"] }),
            cancel.clone(),
            &|_| {},
        )
        .await
        .unwrap();
        assert!(
            tool.execute(
                "c5",
                json!({ "action": "update", "ids": [0, 1], "text": "x" }),
                cancel.clone(),
                &|_| {},
            )
            .await
            .is_err()
        );
    }
}
