//! Event conversion: pi `AgentEvent` → ACP `session/update` notifications.

use agent_client_protocol::{
    ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde_json::Value;
use tack_agent_core::AgentEvent;

pub fn tool_kind(tool_name: &str) -> ToolKind {
    match tool_name {
        "read" => ToolKind::Read,
        "edit" | "write" => ToolKind::Edit,
        "grep" | "find" | "ls" => ToolKind::Search,
        "bash" => ToolKind::Execute,
        _ => ToolKind::Other,
    }
}

fn tool_title(tool_name: &str, args: &Value) -> String {
    let key_arg = match tool_name {
        "bash" => args.get("command").and_then(Value::as_str).unwrap_or(""),
        "grep" => args.get("pattern").and_then(Value::as_str).unwrap_or(""),
        "find" => args.get("pattern").and_then(Value::as_str).unwrap_or(""),
        _ => args.get("path").and_then(Value::as_str).unwrap_or(""),
    };
    let one_line = key_arg.replace('\n', " ");
    let truncated = if one_line.chars().count() > 80 {
        format!("{}…", one_line.chars().take(80).collect::<String>())
    } else {
        one_line
    };
    if truncated.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name} {truncated}")
    }
}

fn text_chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text.into())))
}

fn result_text(result: &tack_agent_core::AgentToolResult) -> String {
    result
        .content
        .iter()
        .map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => text.as_str(),
            tack_ai::InputContentBlock::Image { .. } => "[image]",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Convert an agent event into zero or more session updates.
pub fn event_to_updates(event: &AgentEvent) -> Vec<SessionUpdate> {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => {
            use tack_ai::AssistantMessageEvent as E;
            match assistant_message_event {
                E::TextDelta { delta, .. } => {
                    vec![SessionUpdate::AgentMessageChunk(text_chunk(delta.clone()))]
                }
                E::ThinkingDelta { delta, .. } => {
                    vec![SessionUpdate::AgentThoughtChunk(text_chunk(delta.clone()))]
                }
                _ => Vec::new(),
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            vec![SessionUpdate::ToolCall(
                ToolCall::new(tool_call_id.clone(), tool_title(tool_name, args))
                    .kind(tool_kind(tool_name))
                    .status(ToolCallStatus::InProgress)
                    .raw_input(args.clone()),
            )]
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } => {
            let text = result_text(partial_result);
            if text.is_empty() {
                return Vec::new();
            }
            vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::InProgress)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(text),
                    ))]),
            ))]
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
            ..
        } => {
            vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .status(if *is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    })
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(result_text(result)),
                    ))])
                    .raw_output(result.details.clone()),
            ))]
        }
        _ => Vec::new(),
    }
}

/// Map the loop's terminal stop reason to ACP's.
pub fn stop_reason_for(
    reason: tack_ai::StopReason,
    cancelled: bool,
) -> agent_client_protocol::StopReason {
    use agent_client_protocol::StopReason as Acp;
    if cancelled {
        return Acp::Cancelled;
    }
    match reason {
        tack_ai::StopReason::Length => Acp::MaxTokens,
        _ => Acp::EndTurn,
    }
}

/// Build replay updates for `session/load` from persisted messages: user and
/// assistant text/thinking as single chunks; tool calls as completed cards
/// (outputs skipped to keep replay cheap).
pub fn replay_updates_for_messages(
    messages: &[tack_agent_core::AgentMessage],
) -> Vec<SessionUpdate> {
    use tack_agent_core::AgentMessage;
    let mut updates = Vec::new();
    for message in messages {
        match message {
            AgentMessage::User(u) => {
                let text = match &u.content {
                    tack_ai::UserContent::Text(t) => t.clone(),
                    tack_ai::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                if !text.is_empty() {
                    updates.push(SessionUpdate::UserMessageChunk(text_chunk(text)));
                }
            }
            AgentMessage::Assistant(a) => {
                for block in &a.content {
                    match block {
                        tack_ai::ContentBlock::Text { text, .. } if !text.is_empty() => {
                            updates
                                .push(SessionUpdate::AgentMessageChunk(text_chunk(text.clone())));
                        }
                        tack_ai::ContentBlock::Thinking { thinking, .. }
                            if !thinking.is_empty() =>
                        {
                            updates.push(SessionUpdate::AgentThoughtChunk(text_chunk(
                                thinking.clone(),
                            )));
                        }
                        tack_ai::ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            updates.push(SessionUpdate::ToolCall(
                                ToolCall::new(id.clone(), tool_title(name, arguments))
                                    .kind(tool_kind(name))
                                    .status(ToolCallStatus::Completed)
                                    .raw_input(arguments.clone()),
                            ));
                        }
                        _ => {}
                    }
                }
            }
            AgentMessage::ToolResult(t) => {
                // Mark the call completed; skip output (replay perf).
                updates.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    t.tool_call_id.clone(),
                    ToolCallUpdateFields::new().status(if t.is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    }),
                )));
            }
            _ => {}
        }
    }
    updates
}
