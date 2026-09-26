//! Agent hooks: the extension/gating surface of the loop. One trait object
//! with default methods (Rust equivalent of pi's `AgentLoopConfig` closures).

use async_trait::async_trait;
use serde_json::Value;
use tack_ai::{AssistantMessage, Message, Model, ThinkingLevel, ToolResultMessage};

use crate::message::AgentMessage;
use crate::tool::AgentToolResult;

#[derive(Debug)]
pub struct BeforeToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub args: &'a Value,
    pub context: &'a [AgentMessage],
}

#[derive(Clone, Debug, Default)]
pub enum BeforeToolCallOutcome {
    #[default]
    Allow,
    Block {
        reason: Option<String>,
        terminate: bool,
    },
    /// Allow with rewritten arguments (Claude `updatedInput` semantics).
    /// The loop re-validates the new arguments before execution.
    Rewrite { args: Value },
}

#[derive(Debug)]
pub struct AfterToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub args: &'a Value,
    pub context: &'a [AgentMessage],
}

/// Patch applied to a tool result by `after_tool_call`.
#[derive(Clone, Debug, Default)]
pub struct AfterToolCallPatch {
    pub content: Option<Vec<tack_ai::InputContentBlock>>,
    pub details: Option<Value>,
    pub usage: Option<tack_ai::Usage>,
    pub terminate: Option<bool>,
    pub is_error: Option<bool>,
}

#[derive(Debug)]
pub struct TurnContext<'a> {
    pub message: &'a AssistantMessage,
    pub tool_results: &'a [ToolResultMessage],
    pub new_messages: &'a [AgentMessage],
}

/// Update for the next turn (pi's `prepareNextTurn` snapshot), e.g. a model
/// or thinking-level switch requested by the app.
#[derive(Clone, Debug, Default)]
pub struct NextTurnUpdate {
    pub model: Option<Model>,
    /// `Some(None)` maps to pi's `"off"`.
    pub thinking_level: Option<Option<ThinkingLevel>>,
}

/// Hooks invoked by the agent loop. All methods have defaults; the no-op
/// implementation is `NoopHooks`.
#[async_trait]
pub trait AgentHooks: Send + Sync {
    /// Transform the context before each LLM call (e.g. compaction).
    ///
    /// Copy-on-write contract: the context is only BORROWED. Return `None`
    /// to pass it through unchanged (the zero-cost default); return
    /// `Some(messages)` only when the transform actually rewrote the list.
    /// The loop feeds the borrow (or the rewrite) straight into
    /// `convert_to_llm`, so the common no-op path no longer deep-clones
    /// the full message history — images included — before every LLM call.
    async fn transform_context(&self, _messages: &[AgentMessage]) -> Option<Vec<AgentMessage>> {
        None
    }

    /// Convert agent messages to LLM messages at the call boundary.
    fn convert_to_llm(&self, messages: &[AgentMessage]) -> Vec<Message> {
        AgentMessage::default_convert_to_llm(messages)
    }

    /// Gate tool execution (permission prompts, policy).
    async fn before_tool_call(&self, _ctx: &BeforeToolCallContext<'_>) -> BeforeToolCallOutcome {
        BeforeToolCallOutcome::Allow
    }

    /// Post-process tool results.
    async fn after_tool_call(
        &self,
        _ctx: &AfterToolCallContext<'_>,
        _result: &AgentToolResult,
        _is_error: bool,
    ) -> Option<AfterToolCallPatch> {
        None
    }

    /// Prepare the next turn (model/thinking switches).
    async fn prepare_next_turn(&self, _ctx: &TurnContext<'_>) -> Option<NextTurnUpdate> {
        None
    }

    /// Stop the agent after this turn even if the model produced tool calls.
    async fn should_stop_after_turn(&self, _ctx: &TurnContext<'_>) -> bool {
        false
    }

    /// Steering messages injected before the next assistant response.
    async fn steering_messages(&self) -> Vec<AgentMessage> {
        Vec::new()
    }

    /// Follow-up messages that keep the agent running after it would stop.
    async fn follow_up_messages(&self) -> Vec<AgentMessage> {
        Vec::new()
    }

    /// Overflow recovery (upstream agent-session `_checkCompaction` cases
    /// 1/2): a turn failed with a context-overflow error. Compact the
    /// session NOW (bypassing the token-threshold gate — the provider's
    /// overflow error is the trigger) and return the rebuilt
    /// post-compaction context; the loop retries the turn once against it.
    /// `None` = recovery unavailable (compaction disabled/failed,
    /// unsupported) and the turn ends with the original error.
    async fn compact_for_overflow(&self) -> Option<Vec<AgentMessage>> {
        None
    }
}

/// No-op hooks.
#[derive(Clone, Debug, Default)]
pub struct NoopHooks;

#[async_trait]
impl AgentHooks for NoopHooks {}
