//! Core message/model types. Serde shapes are byte-compatible with the
//! TypeScript pi JSON (see `packages/ai/src/types.ts`), so session files and
//! fixtures can be shared with the TS implementation.

use std::collections::BTreeMap;

use indexmap::IndexMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Unix timestamp in milliseconds (matches JS `Date.now()`).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(rename = "textSignature", skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(rename = "thinkingSignature", skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            text_signature: None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}

/// Content blocks allowed in user messages and tool results (text | image).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum InputContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(rename = "textSignature", skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

impl InputContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        InputContentBlock::Text {
            text: text.into(),
            text_signature: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<InputContentBlock>),
}

impl From<&str> for UserContent {
    fn from(s: &str) -> Self {
        UserContent::Text(s.to_string())
    }
}

impl From<String> for UserContent {
    fn from(s: String) -> Self {
        UserContent::Text(s)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UserMessage {
    pub content: UserContent,
    pub timestamp: u64,
}

/// A tool referenced by name (TS `ToolReference`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolReference {
    pub name: String,
}

/// System instructions and tool declarations at one point in the transcript
/// (TS `SystemMessage`).
///
/// The leading system message is the system prompt. Later system messages
/// change it: `content` adds instructions from that point on, `sections`
/// replace or remove named prompt sections, and `tools_added`/`tools_removed`
/// change the tool set. Replaying every system message in order yields the
/// current prompt and tools.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    /// Instruction text. On the leading message this is the base prompt;
    /// later, additional instructions. TS is `string | TextContent[]`;
    /// [`UserContent`] is a wire-compatible superset.
    pub content: UserContent,
    /// Named, ordered prompt sections rendered verbatim after `content`;
    /// `None` removes a section. Insertion order is preserved (JS `Map`
    /// semantics), so replayed prompts render exactly like upstream's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sections: Option<IndexMap<String, Option<String>>>,
    /// Complete definitions of tools that become available at this point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_added: Option<Vec<ToolDefinition>>,
    /// Tools that stop being available at this point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_removed: Option<Vec<ToolReference>>,
    pub timestamp: u64,
}

impl SystemMessage {
    /// An empty mid-conversation system message carrying only deltas
    /// (`{ role: "system", content: "", timestamp }` in TS).
    pub fn update(timestamp: u64) -> Self {
        SystemMessage {
            content: UserContent::Text(String::new()),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp,
        }
    }
}

/// Serde helper for standalone `Option<SystemMessage>` fields (e.g. the v3
/// compaction `systemMessage`): upstream includes the `role: "system"`
/// discriminator even when the message appears outside the tagged `Message`
/// union (the union's tag supplies it there instead).
pub mod serde_system_message {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::SystemMessage;

    #[derive(Serialize)]
    struct WithRole<'a> {
        role: &'static str,
        #[serde(flatten)]
        message: &'a SystemMessage,
    }

    #[derive(Deserialize)]
    struct RoleIgnored {
        #[serde(rename = "role")]
        _role: Option<String>,
        #[serde(flatten)]
        message: SystemMessage,
    }

    pub fn serialize<S: Serializer>(
        message: &Option<SystemMessage>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match message {
            Some(message) => WithRole {
                role: "system",
                message,
            }
            .serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SystemMessage>, D::Error> {
        Ok(Option::<RoleIgnored>::deserialize(deserializer)?.map(|r| r.message))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(rename = "cacheWrite1h", skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

impl Usage {
    pub fn zero() -> Self {
        Usage::default()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

/// Error info carried by an [`AssistantMessageDiagnostic`] (TS
/// `DiagnosticErrorInfo` in `utils/diagnostics.ts`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DiagnosticErrorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// Provider error code: string or number (kept as raw JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
}

/// Redacted provider/runtime diagnostic for failures and recoveries (TS
/// `AssistantMessageDiagnostic`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AssistantMessageDiagnostic {
    #[serde(rename = "type")]
    pub diagnostic_type: String,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DiagnosticErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<BTreeMap<String, Value>>,
}

/// Handle for a deferred (provider-side) assistant response (TS
/// `DeferredHandle`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: String,
    /// Provider token, such as a response id or batch id plus row id.
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    /// Provider conversion data required to reconstruct the final assistant
    /// message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(rename = "responseModel", skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(rename = "responseId", skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Exact provider-native effort level used for this response (TS
    /// `providerThinkingLevel`). Absent for legacy or unmanaged responses.
    #[serde(
        rename = "providerThinkingLevel",
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_thinking_level: Option<String>,
    /// Redacted provider/runtime diagnostics for failures and recoveries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<AssistantMessageDiagnostic>>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<Box<DeferredHandle>>,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(rename = "rawStopReason", skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    #[serde(rename = "endTurn", skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    pub timestamp: u64,
}

impl AssistantMessage {
    /// Fresh pending message for a model, used as the streaming accumulator.
    pub fn pending(model: &Model) -> Self {
        AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            diagnostics: None,
            usage: Usage::zero(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_millis(),
        }
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some((id.as_str(), name.as_str(), arguments)),
            _ => None,
        })
    }

    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
    }

    /// Concatenated text content.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<InputContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub is_error: bool,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "system")]
    System(SystemMessage),
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn user(content: impl Into<UserContent>) -> Self {
        Message::User(UserMessage {
            content: content.into(),
            timestamp: now_millis(),
        })
    }

    /// The message's role tag (TS `message.role`).
    pub fn role(&self) -> &'static str {
        match self {
            Message::System(_) => "system",
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }

    /// The system message payload, if this is a system message.
    pub fn as_system(&self) -> Option<&SystemMessage> {
        match self {
            Message::System(m) => Some(m),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tools / context
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON schema for the tool parameters.
    pub parameters: Value,
    /// Anthropic deferred tools: emit `defer_loading: true` so the model
    /// discovers the tool via tool search instead of an always-on schema
    /// (TS convertTools deferLoading). Only meaningful on capable models.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub defer_loading: bool,
    /// Constrained sampling declaration (TS Tool.constrainedSampling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<crate::constrained_sampling::ConstrainedSampling>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
}

// ---------------------------------------------------------------------------
// Thinking levels
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    /// `clampReasoning` from simple-options.ts: xhigh/max clamp to high.
    pub fn clamped(self) -> Self {
        match self {
            ThinkingLevel::Xhigh | ThinkingLevel::Max => ThinkingLevel::High,
            other => other,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }
}

/// Token budgets for each thinking level (token-based providers only).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThinkingBudgets {
    pub minimal: u32,
    pub low: u32,
    pub medium: u32,
    pub high: u32,
}

impl Default for ThinkingBudgets {
    /// `DEFAULT_THINKING_BUDGETS` from simple-options.ts.
    fn default() -> Self {
        ThinkingBudgets {
            minimal: 1024,
            low: 2048,
            medium: 8192,
            high: 16384,
        }
    }
}

impl ThinkingBudgets {
    pub fn for_level(&self, level: ThinkingLevel) -> u32 {
        match level.clamped() {
            ThinkingLevel::Minimal => self.minimal,
            ThinkingLevel::Low => self.low,
            ThinkingLevel::Medium => self.medium,
            ThinkingLevel::High => self.high,
            ThinkingLevel::Xhigh | ThinkingLevel::Max => unreachable!("clamped"),
        }
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InputKind {
    Text,
    Image,
}

/// One request-wide pricing tier (TS ModelCostTier).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    /// Use this tier when total input usage exceeds this token count.
    pub input_tokens_above: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// Request-wide pricing tiers. The highest matching input threshold
    /// applies to the full request (TS ModelCost.tiers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Model {
    pub id: String,
    pub name: String,
    /// Wire protocol, e.g. "anthropic-messages", "openai-completions".
    pub api: String,
    pub provider: String,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub reasoning: bool,
    /// Maps pi thinking levels to provider-specific values. Missing keys use
    /// provider defaults; null marks a level as unsupported.
    #[serde(rename = "thinkingLevelMap", skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    pub input: Vec<InputKind>,
    pub cost: ModelCost,
    #[serde(rename = "contextWindow")]
    pub context_window: u32,
    #[serde(rename = "maxTokens")]
    pub max_tokens: u32,
    #[serde(rename = "samplingParams", skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Per-API compatibility overrides. Kept as raw JSON so unknown fields
    /// round-trip; adapters deserialize the subset they understand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
}

impl Model {
    pub fn supports_images(&self) -> bool {
        self.input.contains(&InputKind::Image)
    }

    /// Mapped value for a thinking level, if the model overrides it.
    pub fn thinking_level_value(&self, level: ThinkingLevel) -> Option<&Option<String>> {
        self.thinking_level_map.as_ref()?.get(level.as_str())
    }
}

// ---------------------------------------------------------------------------
// Cost calculation (port of calculateCost from packages/ai/src/models.ts)
// ---------------------------------------------------------------------------

pub fn calculate_cost(model: &Model, usage: &mut Usage) {
    // Tiered pricing (TS calculateCost): the highest matching input
    // threshold applies to the full request; total input = input + cached.
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let mut rates = (
        model.cost.input,
        model.cost.output,
        model.cost.cache_read,
        model.cost.cache_write,
    );
    let mut matched_threshold = 0u64;
    for tier in model.cost.tiers.iter().flatten() {
        if input_tokens > tier.input_tokens_above && tier.input_tokens_above >= matched_threshold {
            rates = (tier.input, tier.output, tier.cache_read, tier.cache_write);
            matched_threshold = tier.input_tokens_above;
        }
    }
    let (input_rate, output_rate, cache_read_rate, cache_write_rate) = rates;
    let long_write = usage.cache_write_1h.unwrap_or(0) as f64;
    let short_write = usage.cache_write as f64 - long_write;
    usage.cost.input = (input_rate / 1_000_000.0) * usage.input as f64;
    usage.cost.output = (output_rate / 1_000_000.0) * usage.output as f64;
    usage.cost.cache_read = (cache_read_rate / 1_000_000.0) * usage.cache_read as f64;
    usage.cost.cache_write =
        (cache_write_rate * short_write + input_rate * 2.0 * long_write) / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}
