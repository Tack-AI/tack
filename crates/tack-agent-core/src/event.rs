//! Agent events. Mirror of `AgentEvent` in `packages/agent/src/types.ts`.

use serde_json::Value;
use tack_ai::{AssistantMessage, AssistantMessageEvent, ToolResultMessage};

use crate::message::AgentMessage;
use crate::tool::AgentToolResult;

// Events carry whole messages by value (pi semantics); boxing would churn
// every event for a rare size win.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        assistant_message_event: AssistantMessageEvent,
        message: AgentMessage,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
    /// The active model exhausted retries with a retryable failure (429 /
    /// overloaded / 5xx / timeout) and the loop switched to the next entry
    /// of the fallback chain.
    ModelFallback {
        from: tack_ai::Model,
        to: tack_ai::Model,
        reason: String,
    },
}

impl AgentEvent {
    /// Short tag for tests and logs.
    pub fn tag(&self) -> &'static str {
        match self {
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::AgentEnd { .. } => "agent_end",
            AgentEvent::TurnStart => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
            AgentEvent::ModelFallback { .. } => "model_fallback",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, AgentEvent::AgentEnd { .. })
    }
}
