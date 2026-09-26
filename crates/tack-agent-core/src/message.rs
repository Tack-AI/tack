//! Agent message: the loop's message type. A superset of `tack_ai::Message`
//! covering pi-coding-agent's extended types (bashExecution, custom,
//! branchSummary, compactionSummary). Serde shapes are byte-compatible with
//! pi session files.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tack_ai::{
    AssistantMessage, InputContentBlock, Message, SystemMessage, ToolResultMessage, UserContent,
    UserMessage,
};

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CustomAgentMessage {
    pub custom_type: String,
    pub content: UserContent,
    pub display: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    pub summary: String,
    pub from_id: String,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role")]
pub enum AgentMessage {
    #[serde(rename = "system")]
    System(SystemMessage),
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
    #[serde(rename = "custom")]
    Custom(CustomAgentMessage),
    #[serde(rename = "bashExecution")]
    BashExecution(BashExecutionMessage),
    #[serde(rename = "branchSummary")]
    BranchSummary(BranchSummaryMessage),
    #[serde(rename = "compactionSummary")]
    CompactionSummary(CompactionSummaryMessage),
}

impl AgentMessage {
    pub fn user(content: impl Into<UserContent>) -> Self {
        AgentMessage::User(UserMessage {
            content: content.into(),
            timestamp: tack_ai::now_millis(),
        })
    }

    pub fn custom(
        custom_type: impl Into<String>,
        content: impl Into<UserContent>,
        display: bool,
        details: Option<Value>,
    ) -> Self {
        AgentMessage::Custom(CustomAgentMessage {
            custom_type: custom_type.into(),
            content: content.into(),
            display,
            details,
            timestamp: tack_ai::now_millis(),
        })
    }

    pub fn timestamp(&self) -> u64 {
        match self {
            AgentMessage::System(m) => m.timestamp,
            AgentMessage::User(m) => m.timestamp,
            AgentMessage::Assistant(m) => m.timestamp,
            AgentMessage::ToolResult(m) => m.timestamp,
            AgentMessage::Custom(m) => m.timestamp,
            AgentMessage::BashExecution(m) => m.timestamp,
            AgentMessage::BranchSummary(m) => m.timestamp,
            AgentMessage::CompactionSummary(m) => m.timestamp,
        }
    }

    /// The system message payload, if this is a system message (inherent
    /// convenience so callers need not import the
    /// [`tack_ai::transcript::TranscriptMessage`] trait).
    pub fn as_system(&self) -> Option<&SystemMessage> {
        match self {
            AgentMessage::System(s) => Some(s),
            _ => None,
        }
    }

    /// The message's role tag (TS `message.role`).
    pub fn role(&self) -> &'static str {
        match self {
            AgentMessage::System(_) => "system",
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolResult",
            AgentMessage::Custom(_) => "custom",
            AgentMessage::BashExecution(_) => "bashExecution",
            AgentMessage::BranchSummary(_) => "branchSummary",
            AgentMessage::CompactionSummary(_) => "compactionSummary",
        }
    }

    /// Convert a bash execution to user message text (pi's bashExecutionToText).
    fn bash_execution_text(m: &BashExecutionMessage) -> String {
        let mut text = format!("Ran `{}`\n", m.command);
        if m.output.is_empty() {
            text.push_str("(no output)");
        } else {
            text.push_str(&format!("```\n{}\n```", m.output));
        }
        if m.cancelled {
            text.push_str("\n\n(command cancelled)");
        } else if let Some(code) = m.exit_code
            && code != 0
        {
            text.push_str(&format!("\n\nCommand exited with code {code}"));
        }
        if m.truncated
            && let Some(path) = &m.full_output_path
        {
            text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        }
        text
    }

    /// Default `convertToLlm` mapping, mirroring pi-coding-agent's
    /// `convertToLlm` in `core/messages.ts`: extended types become user
    /// messages with the pi prefixes; excluded bash executions are dropped.
    pub fn default_convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
        messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::System(s) => Some(Message::System(s.clone())),
                AgentMessage::User(u) => Some(Message::User(u.clone())),
                AgentMessage::Assistant(a) => Some(Message::Assistant(a.clone())),
                AgentMessage::ToolResult(t) => Some(Message::ToolResult(t.clone())),
                AgentMessage::Custom(c) => Some(Message::User(UserMessage {
                    content: c.content.clone(),
                    timestamp: c.timestamp,
                })),
                AgentMessage::BashExecution(b) => {
                    if b.exclude_from_context == Some(true) {
                        None
                    } else {
                        Some(Message::User(UserMessage {
                            content: UserContent::Blocks(vec![InputContentBlock::text(
                                Self::bash_execution_text(b),
                            )]),
                            timestamp: b.timestamp,
                        }))
                    }
                }
                AgentMessage::BranchSummary(b) => Some(Message::User(UserMessage {
                    content: UserContent::Blocks(vec![InputContentBlock::text(format!(
                        "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                        b.summary
                    ))]),
                    timestamp: b.timestamp,
                })),
                AgentMessage::CompactionSummary(c) => Some(Message::User(UserMessage {
                    content: UserContent::Blocks(vec![InputContentBlock::text(format!(
                        "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                        c.summary
                    ))]),
                    timestamp: c.timestamp,
                })),
            })
            .collect()
    }
}

impl From<Message> for AgentMessage {
    fn from(m: Message) -> Self {
        match m {
            Message::System(s) => AgentMessage::System(s),
            Message::User(u) => AgentMessage::User(u),
            Message::Assistant(a) => AgentMessage::Assistant(a),
            Message::ToolResult(t) => AgentMessage::ToolResult(t),
        }
    }
}

impl tack_ai::transcript::TranscriptMessage for AgentMessage {
    fn as_system(&self) -> Option<&SystemMessage> {
        match self {
            AgentMessage::System(s) => Some(s),
            _ => None,
        }
    }
}
