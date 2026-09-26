use anyhow::Result;
use serde_json::{Value, json};
use tack_agent_core::{AgentEvent, AgentMessage};

/// Set one field inside a nested settings object (compaction/retry) without
/// clobbering sibling fields.
pub(crate) fn save_nested_bool(
    agent_dir: &std::path::Path,
    key: &str,
    field: &str,
    value: bool,
) -> Result<(), String> {
    let path = agent_dir.join("settings.json");
    let mut content: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| json!({}));
    let section = content
        .as_object_mut()
        .ok_or("settings root is not an object")?
        .entry(key.to_string())
        .or_insert_with(|| json!({}));
    let section = section
        .as_object_mut()
        .ok_or(format!("settings {key} is not an object"))?;
    section.insert(field.to_string(), Value::Bool(value));
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&content).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// Serialize an agent event to pi's JSON event wire shape (toJsonEvent).
pub fn event_to_json(event: &AgentEvent) -> Value {
    match event {
        AgentEvent::AgentStart => json!({ "type": "agent_start" }),
        AgentEvent::AgentEnd { .. } => json!({ "type": "agent_end" }),
        AgentEvent::ModelFallback { from, to, reason } => json!({
            "type": "model_fallback",
            "from": { "provider": from.provider, "id": from.id },
            "to": { "provider": to.provider, "id": to.id },
            "reason": reason,
        }),
        AgentEvent::TurnStart => json!({ "type": "turn_start" }),
        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => json!({
            "type": "turn_end",
            "message": message,
            "toolResults": tool_results,
        }),
        AgentEvent::MessageStart { message } => {
            json!({ "type": "message_start", "message": message })
        }
        AgentEvent::MessageEnd { message } => json!({ "type": "message_end", "message": message }),
        AgentEvent::MessageUpdate {
            assistant_message_event,
            message,
        } => {
            let usage = match message {
                AgentMessage::Assistant(a) => serde_json::to_value(&a.usage).unwrap_or_default(),
                _ => Value::Null,
            };
            let slim = match assistant_message_event {
                tack_ai::AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial,
                } => {
                    let tool_call = partial.content.get(*content_index);
                    match tool_call {
                        Some(tack_ai::ContentBlock::ToolCall { id, name, .. }) => json!({
                            "type": "toolcall_start", "contentIndex": content_index,
                            "id": id, "toolName": name,
                        }),
                        _ => json!({ "type": "toolcall_start", "contentIndex": content_index }),
                    }
                }
                other => slim_assistant_event(other),
            };
            json!({ "type": "message_update", "usage": usage, "assistantMessageEvent": slim })
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => json!({
            "type": "tool_execution_start",
            "toolCallId": tool_call_id, "toolName": tool_name, "args": args,
        }),
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => json!({
            "type": "tool_execution_update",
            "toolCallId": tool_call_id, "toolName": tool_name, "args": args,
            "partialResult": result_to_json(partial_result),
        }),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => json!({
            "type": "tool_execution_end",
            "toolCallId": tool_call_id, "toolName": tool_name,
            "result": result_to_json(result), "isError": is_error,
        }),
    }
}

fn slim_assistant_event(event: &tack_ai::AssistantMessageEvent) -> Value {
    use tack_ai::AssistantMessageEvent as E;
    match event {
        E::Start { .. } => json!({ "type": "start" }),
        E::TextStart { content_index, .. } => {
            json!({ "type": "text_start", "contentIndex": content_index })
        }
        E::TextDelta {
            content_index,
            delta,
            ..
        } => {
            json!({ "type": "text_delta", "contentIndex": content_index, "delta": delta })
        }
        E::TextEnd {
            content_index,
            content,
            ..
        } => {
            json!({ "type": "text_end", "contentIndex": content_index, "content": content })
        }
        E::ThinkingStart { content_index, .. } => {
            json!({ "type": "thinking_start", "contentIndex": content_index })
        }
        E::ThinkingDelta {
            content_index,
            delta,
            ..
        } => {
            json!({ "type": "thinking_delta", "contentIndex": content_index, "delta": delta })
        }
        E::ThinkingEnd {
            content_index,
            content,
            ..
        } => {
            json!({ "type": "thinking_end", "contentIndex": content_index, "content": content })
        }
        E::ToolCallStart { content_index, .. } => {
            json!({ "type": "toolcall_start", "contentIndex": content_index })
        }
        E::ToolCallDelta {
            content_index,
            delta,
            ..
        } => {
            json!({ "type": "toolcall_delta", "contentIndex": content_index, "delta": delta })
        }
        E::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => {
            json!({ "type": "toolcall_end", "contentIndex": content_index, "toolCall": tool_call })
        }
        E::Done { reason, message } => {
            json!({ "type": "done", "reason": reason, "message": message })
        }
        E::Error { reason, error } => {
            json!({ "type": "error", "reason": reason, "error": error })
        }
    }
}

fn result_to_json(result: &tack_agent_core::AgentToolResult) -> Value {
    json!({
        "content": result.content,
        "details": result.details,
        "usage": result.usage,
        "terminate": result.terminate,
    })
}
