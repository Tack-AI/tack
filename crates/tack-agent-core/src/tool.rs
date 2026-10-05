//! The tool contract. Port of `AgentTool` from `packages/agent/src/types.ts`
//! with a `serde_json::Value` parameter boundary (validation errors become
//! in-band error tool results, matching pi's behavior).

use async_trait::async_trait;
use serde_json::Value;
use tack_ai::{InputContentBlock, Usage};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
    #[default]
    Parallel,
    Sequential,
}

/// Result of a tool execution. `content` is limited to text/image blocks.
#[derive(Clone, Debug)]
pub struct AgentToolResult {
    pub content: Vec<InputContentBlock>,
    pub details: Value,
    pub usage: Option<Usage>,
    /// When every result in a batch sets this, the agent stops after the turn.
    pub terminate: bool,
    /// Tools activated by this result: the loop moves them from the
    /// deferred pool into the context for subsequent LLM calls, and
    /// records the loadout change as a transcript system message.
    ///
    /// tack-internal channel set by the tool_search tool — upstream
    /// removed `addedToolNames` from the wire format (#9548), so this is
    /// never serialized into session messages.
    pub added_tool_names: Option<Vec<String>>,
}

impl AgentToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        AgentToolResult {
            content: vec![InputContentBlock::text(text)],
            details: Value::Object(serde_json::Map::new()),
            usage: None,
            terminate: false,
            added_tool_names: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        AgentToolResult::text(message)
    }
}

/// A tool the agent can call. Object-safe; schemas are produced with
/// `schemars` by the concrete tool.
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &'static str;
    /// Human-readable label for UI display.
    fn label(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON schema for the parameters (e.g. `schemars::schema_for!(Params)`).
    fn parameters_schema(&self) -> Value;

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    /// Constrained sampling declaration handed to the LLM provider
    /// (TS `Tool.constrainedSampling`). Built-in tools default to strict
    /// JSON-schema "prefer" (TS pi fcff255b0); extensions default to None.
    fn constrained_sampling(&self) -> Option<tack_ai::constrained_sampling::ConstrainedSampling> {
        None
    }

    /// Optional argument normalization before validation/execution.
    fn prepare_arguments(&self, args: Value) -> Value {
        args
    }

    /// Whether the tool may be advertised to the given provider's models
    /// (provider-bound tools override; default: every provider). Filtered
    /// at the LLM-context boundary in the agent loop.
    fn available_for_provider(&self, _provider: &str) -> bool {
        true
    }

    /// Whether this tool starts in the deferred `tool_search` pool instead
    /// of the model's declarations (MCP `exposure: "deferred"`). The loop
    /// activates pool tools via `AgentToolResult.added_tool_names`.
    fn starts_deferred(&self) -> bool {
        false
    }

    /// Validate raw arguments. Default: validate against the JSON schema.
    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        let schema = self.parameters_schema();
        let validator = jsonschema::validator_for(&schema)
            .map_err(|e| format!("invalid tool schema for {}: {e}", self.name()))?;
        validator
            .validate(args)
            .map_err(|e| format!("invalid arguments for {}: {e}", self.name()))
    }

    /// Execute the tool. `Err` is converted by the loop into an in-band error
    /// tool result (matching pi's catch in `executePreparedToolCall`).
    /// `on_update` delivers partial results (e.g. streaming bash output).
    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String>;
}

/// Definition handed to the LLM (name/description/schema), derived from a tool.
pub fn tool_definition(tool: &dyn AgentTool) -> tack_ai::ToolDefinition {
    tack_ai::ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters: tool.parameters_schema(),
        defer_loading: false,
        constrained_sampling: tool.constrained_sampling(),
    }
}
