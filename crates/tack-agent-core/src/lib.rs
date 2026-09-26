//! Agent runtime: tool calling, event stream, and the agent loop.
//!
//! Rust port of `@earendil-works/pi-agent-core`.

pub mod agent_loop;
pub mod event;
pub mod extension;
pub mod hooks;
pub mod message;
pub mod tool;

pub use agent_loop::{AgentContext, AgentLoopConfig, agent_loop, agent_loop_continue};
pub use event::AgentEvent;
pub use extension::{Extension, HooksChain};
pub use hooks::{
    AfterToolCallContext, AfterToolCallPatch, AgentHooks, BeforeToolCallContext,
    BeforeToolCallOutcome, NextTurnUpdate, NoopHooks, TurnContext,
};
pub use message::{
    AgentMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    CustomAgentMessage,
};
pub use tool::{AgentTool, AgentToolResult, ToolExecutionMode, tool_definition};
