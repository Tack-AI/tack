//! The ask_user tool: let the agent pause mid-run and ask the user
//! structured questions — multiple-choice (with an "Other" free-text
//! escape in the TUI) or plain free-text when no options are given.
//!
//! The tool itself is UI-agnostic: the host injects an [`AskUserHandler`]
//! via `ToolServices::with_ask_user`. The interactive TUI installs a
//! dialog-backed handler (tack-app `ask_user` module); headless modes
//! (print/rpc/acp/serve) and subagents install none, and the tool then
//! returns an in-band "no interactive user" message so the model proceeds
//! with its best judgment instead of blocking forever (same policy as MCP
//! elicitation, which headless modes decline).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tack_agent_core::{AgentTool, AgentToolResult, ToolExecutionMode};
use tokio_util::sync::CancellationToken;

use crate::services::ToolServices;

/// At most this many questions per call — each one is a separate dialog,
/// so large batches are a poor user experience.
pub const MAX_QUESTIONS: usize = 4;
/// A multiple-choice question offers 2–4 options (plus the TUI's "Other"
/// escape, which the tool schema does not count).
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 4;

const DESCRIPTION: &str = "Ask the user structured questions when you need a decision, clarification, \
or preference only they can provide (ambiguous requirements, multiple viable approaches, destructive \
choices). Present 1-4 questions; each is either multiple-choice (2-4 options, the user can also enter \
a custom answer) or free-text (omit options). Do NOT use for things you can determine yourself from \
the codebase, and never as a substitute for just doing the obvious thing. The answers are returned \
as the tool result.";

/// In-band message when no interactive user exists (headless modes,
/// subagents): the model must not retry — it should decide on its own.
const UNAVAILABLE_MESSAGE: &str = "ask_user is unavailable in this mode: there is no interactive \
user to answer. Do not retry this tool — make a reasonable decision on your own, state the \
assumption you made, and continue.";

/// In-band message when the user dismissed the dialog (Esc).
const CANCELLED_MESSAGE: &str = "The user declined to answer. Do not ask the same question(s) again \
immediately — make a reasonable decision on your own, state the assumption you made, and continue.";

/// One option the user can pick for a multiple-choice question.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct AskUserOption {
    /// Short option label; this exact text is reported back as the answer
    /// when the option is picked.
    pub label: String,
    /// Optional longer explanation shown alongside the label.
    pub description: Option<String>,
}

/// One question for the user.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct AskUserQuestion {
    /// The question text, phrased so the user can answer directly.
    pub question: String,
    /// Optional very short label (a few words, e.g. "Scope" or "Approach")
    /// shown as a header chip above the question.
    pub header: Option<String>,
    /// 2-4 choices for a multiple-choice question; omit for free-text.
    pub options: Option<Vec<AskUserOption>>,
}

/// One answered question (the question text echoes back for context).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AskUserAnswer {
    pub question: String,
    /// The chosen option label, or the user's free-text answer.
    pub answer: String,
}

/// The user's response to one batch of questions.
#[derive(Clone, Debug, PartialEq)]
pub enum AskUserResponse {
    Answered(Vec<AskUserAnswer>),
    /// The user dismissed the dialog (Esc) or the UI went away.
    Cancelled,
}

/// Host-provided channel for actually asking the user. The interactive TUI
/// installs a dialog-backed implementation; everything else installs none.
#[async_trait::async_trait]
pub trait AskUserHandler: Send + Sync + std::fmt::Debug {
    async fn ask(&self, questions: Vec<AskUserQuestion>) -> AskUserResponse;
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AskUserParams {
    /// 1-4 questions, presented to the user one at a time.
    questions: Vec<AskUserQuestion>,
}

/// Semantic validation beyond the JSON schema (counts, non-empty texts,
/// unique option labels). Pure, so both the tool and tests exercise it.
pub fn validate_questions(questions: &[AskUserQuestion]) -> Result<(), String> {
    if questions.is_empty() {
        return Err("questions must contain at least one question".to_string());
    }
    if questions.len() > MAX_QUESTIONS {
        return Err(format!("at most {MAX_QUESTIONS} questions per call"));
    }
    for (i, q) in questions.iter().enumerate() {
        if q.question.trim().is_empty() {
            return Err(format!("questions[{i}]: question text must not be empty"));
        }
        if let Some(options) = &q.options {
            if options.len() < MIN_OPTIONS || options.len() > MAX_OPTIONS {
                return Err(format!(
                    "questions[{i}]: options must have {MIN_OPTIONS}-{MAX_OPTIONS} entries \
                     (or omit options for a free-text answer)"
                ));
            }
            for option in options {
                if option.label.trim().is_empty() {
                    return Err(format!("questions[{i}]: option labels must not be empty"));
                }
            }
            let mut seen = std::collections::HashSet::new();
            if options.iter().any(|o| !seen.insert(o.label.trim())) {
                return Err(format!("questions[{i}]: option labels must be unique"));
            }
        }
    }
    Ok(())
}

pub struct AskUserTool {
    services: ToolServices,
}

impl AskUserTool {
    pub fn new(services: ToolServices) -> Self {
        AskUserTool { services }
    }
}

impl std::fmt::Debug for AskUserTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskUserTool").finish()
    }
}

#[async_trait::async_trait]
impl AgentTool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    fn label(&self) -> &str {
        "ask user"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        crate::schema_for::<AskUserParams>()
    }

    /// Interactive: never run two question dialogs (or a dialog plus other
    /// work) concurrently.
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn constrained_sampling(&self) -> Option<tack_ai::constrained_sampling::ConstrainedSampling> {
        crate::prefer_strict_sampling()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: AskUserParams = serde_json::from_value(params)
            .map_err(|e| format!("invalid arguments for ask_user: {e}"))?;
        validate_questions(&params.questions)?;

        let Some(handler) = self.services.ask_user.clone() else {
            return Ok(AgentToolResult::text(UNAVAILABLE_MESSAGE));
        };

        // Race the dialog against run cancellation so an aborted run does
        // not leave a parked tool call awaiting an answer forever.
        let response = tokio::select! {
            response = handler.ask(params.questions.clone()) => response,
            () = cancel.cancelled() => AskUserResponse::Cancelled,
        };

        match response {
            AskUserResponse::Cancelled => Ok(AgentToolResult::text(CANCELLED_MESSAGE)),
            AskUserResponse::Answered(answers) => {
                let mut text = String::new();
                for (i, answer) in answers.iter().enumerate() {
                    if answers.len() > 1 {
                        text.push_str(&format!("Q{}: {}\n", i + 1, answer.question));
                    } else {
                        text.push_str(&format!("Q: {}\n", answer.question));
                    }
                    text.push_str(&format!("A: {}\n", answer.answer));
                }
                Ok(AgentToolResult {
                    content: vec![tack_ai::InputContentBlock::text(
                        text.trim_end().to_string(),
                    )],
                    details: json!({ "answers": answers }),
                    usage: None,
                    terminate: false,
                    added_tool_names: None,
                })
            }
        }
    }
}

/// Test helper / convenience for hosts: build the answer list from raw
/// per-question answer strings in question order.
pub fn answers_from(questions: &[AskUserQuestion], answers: Vec<String>) -> Vec<AskUserAnswer> {
    questions
        .iter()
        .zip(answers)
        .map(|(q, answer)| AskUserAnswer {
            question: q.question.clone(),
            answer,
        })
        .collect()
}

/// Wrap a handler into the shared pointer stored in `ToolServices`.
pub fn shared_handler(handler: impl AskUserHandler + 'static) -> Arc<dyn AskUserHandler> {
    Arc::new(handler)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn choice(question: &str, labels: &[&str]) -> AskUserQuestion {
        AskUserQuestion {
            question: question.to_string(),
            header: None,
            options: Some(
                labels
                    .iter()
                    .map(|label| AskUserOption {
                        label: label.to_string(),
                        description: None,
                    })
                    .collect(),
            ),
        }
    }

    fn free_text(question: &str) -> AskUserQuestion {
        AskUserQuestion {
            question: question.to_string(),
            header: None,
            options: None,
        }
    }

    #[derive(Debug)]
    struct ScriptedHandler(AskUserResponse);

    #[async_trait::async_trait]
    impl AskUserHandler for ScriptedHandler {
        async fn ask(&self, _questions: Vec<AskUserQuestion>) -> AskUserResponse {
            self.0.clone()
        }
    }

    fn tool_with(handler: Option<AskUserResponse>) -> AskUserTool {
        let services = crate::services::ToolServices::new(std::env::temp_dir());
        match handler {
            Some(response) => {
                AskUserTool::new(services.with_ask_user(shared_handler(ScriptedHandler(response))))
            }
            None => AskUserTool::new(services),
        }
    }

    fn result_text(result: &AgentToolResult) -> String {
        match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn validation_matrix() {
        assert!(validate_questions(&[]).is_err());
        assert!(validate_questions(&[free_text("ok")]).is_ok());
        assert!(validate_questions(&[free_text("  ")]).is_err());
        // Too many questions.
        assert!(
            validate_questions(&[
                free_text("q"),
                free_text("q"),
                free_text("q"),
                free_text("q"),
                free_text("q")
            ])
            .is_err()
        );
        // Option count bounds.
        assert!(validate_questions(&[choice("q", &["only"])]).is_err());
        assert!(validate_questions(&[choice("q", &["a", "b"])]).is_ok());
        assert!(validate_questions(&[choice("q", &["a", "b", "c", "d", "e"])]).is_err());
        // Empty / duplicate labels.
        assert!(validate_questions(&[choice("q", &["a", " "])]).is_err());
        assert!(validate_questions(&[choice("q", &["a", "a"])]).is_err());
    }

    #[tokio::test]
    async fn no_handler_returns_unavailable_message() {
        let tool = tool_with(None);
        let result = tool
            .execute(
                "c1",
                json!({ "questions": [{ "question": "which?" }] }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(text.contains("no interactive user"), "{text}");
        assert!(text.contains("Do not retry"), "{text}");
    }

    #[tokio::test]
    async fn answered_batch_formats_q_and_a() {
        let tool = tool_with(Some(AskUserResponse::Answered(vec![
            AskUserAnswer {
                question: "which approach?".into(),
                answer: "A".into(),
            },
            AskUserAnswer {
                question: "scope?".into(),
                answer: "just the parser".into(),
            },
        ])));
        let result = tool
            .execute(
                "c1",
                json!({ "questions": [
                    { "question": "which approach?", "options": [
                        { "label": "A" }, { "label": "B" }
                    ]},
                    { "question": "scope?" }
                ]}),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(text.contains("Q1: which approach?\nA: A"), "{text}");
        assert!(text.contains("Q2: scope?\nA: just the parser"), "{text}");
        let answers = result.details["answers"].as_array().unwrap();
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0]["answer"], "A");
    }

    #[tokio::test]
    async fn cancelled_and_run_cancellation_are_in_band() {
        let tool = tool_with(Some(AskUserResponse::Cancelled));
        let result = tool
            .execute(
                "c1",
                json!({ "questions": [{ "question": "which?" }] }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(result_text(&result).contains("declined to answer"));

        // A pre-cancelled run token short-circuits even a parked handler.
        #[derive(Debug)]
        struct Parked;
        #[async_trait::async_trait]
        impl AskUserHandler for Parked {
            async fn ask(&self, _questions: Vec<AskUserQuestion>) -> AskUserResponse {
                std::future::pending().await
            }
        }
        let services = crate::services::ToolServices::new(std::env::temp_dir())
            .with_ask_user(shared_handler(Parked));
        let tool = AskUserTool::new(services);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tool
            .execute(
                "c2",
                json!({ "questions": [{ "question": "which?" }] }),
                cancel,
                &|_| {},
            )
            .await
            .unwrap();
        assert!(result_text(&result).contains("declined to answer"));
    }

    #[tokio::test]
    async fn invalid_arguments_are_rejected() {
        let tool = tool_with(None);
        assert!(
            tool.execute(
                "c1",
                json!({ "questions": [] }),
                CancellationToken::new(),
                &|_| {}
            )
            .await
            .is_err()
        );
        assert!(
            tool.execute(
                "c2",
                json!({ "questions": [{ "question": "q", "options": [{ "label": "a" }] }] }),
                CancellationToken::new(),
                &|_| {}
            )
            .await
            .is_err()
        );
    }
}
