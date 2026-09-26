//! The ask_codebuddy tool. Port of the reference plugin's AskCodebuddy
//! (pi-codebuddy-sdk): delegate a focused sub-task to a fresh CodeBuddy
//! CLI call — second opinion, code review, architecture questions, or
//! autonomous full-mode execution. The delegated call runs in a CLEAN
//! session (it sees only the prompt, not the conversation), so prompts
//! must be self-contained. CodeBuddy runs its own built-in tools per
//! mode; tack's tools and permissions are not involved.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult, ToolExecutionMode};
use tokio_util::sync::CancellationToken;

const DESCRIPTION: &str = "Delegate a focused sub-task to CodeBuddy (second opinion, code review, architecture questions, debugging theories), or autonomously handle a task in full mode. The delegated call runs in a clean session — it sees ONLY this prompt, not the conversation — so make the prompt self-contained: include the question, relevant file paths, and what to look at. Do not research up front; let CodeBuddy explore. Prefer to handle straightforward tasks yourself.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AskCodebuddyParams {
    /// The question or task for CodeBuddy. Must be self-contained: the
    /// delegated call sees only this prompt, not the conversation.
    prompt: String,
    /// "read" (default): questions about the codebase — review, analysis,
    /// explain (no writes). "none": general knowledge only (no file
    /// access). "full": allows writing and bash execution (careful: runs
    /// without feedback to tack).
    mode: Option<String>,
    /// CodeBuddy model id. Omit to use CodeBuddy's default.
    model: Option<String>,
    /// Thinking effort level: "off" | "minimal" | "low" | "medium" |
    /// "high" | "xhigh". Omit to use CodeBuddy's default.
    thinking: Option<String>,
}

pub struct AskCodebuddyTool;

impl AskCodebuddyTool {
    pub fn new() -> Self {
        AskCodebuddyTool
    }
}

impl Default for AskCodebuddyTool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AskCodebuddyTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskCodebuddyTool").finish()
    }
}

/// Per-tool-use counts for the actions footer (reference:
/// buildActionSummary).
fn action_summary(tool_uses: &[String]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for name in tool_uses {
        let short = name
            .rsplit(['_', '/'])
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase();
        match counts.iter_mut().find(|(n, _)| *n == short) {
            Some(entry) => entry.1 += 1,
            None => counts.push((short, 1)),
        }
    }
    counts
        .iter()
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name}×{count}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait]
impl AgentTool for AskCodebuddyTool {
    fn name(&self) -> &'static str {
        "ask_codebuddy"
    }

    fn label(&self) -> &str {
        "Ask CodeBuddy"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        crate::schema_for::<AskCodebuddyParams>()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    /// Provider-bound: only advertised to codebuddy models (the tool is
    /// part of the codebuddy provider's feature set — other providers
    /// must not see it at all).
    fn available_for_provider(&self, provider: &str) -> bool {
        provider == tack_ai::codebuddy::PROVIDER_ID
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: AskCodebuddyParams =
            serde_json::from_value(params).map_err(|e| format!("invalid arguments: {e}"))?;
        if params.prompt.trim().is_empty() {
            return Err("prompt must not be empty".to_string());
        }
        let mode = match params.mode.as_deref().unwrap_or("read") {
            "read" => tack_ai::codebuddy::AskMode::Read,
            "full" => tack_ai::codebuddy::AskMode::Full,
            "none" => tack_ai::codebuddy::AskMode::None,
            other => {
                return Err(format!(
                    "invalid mode {other:?} — expected \"read\" | \"full\" | \"none\""
                ));
            }
        };
        let effort = match params.thinking.as_deref() {
            None | Some("off") => None,
            Some(level) => {
                let level: tack_ai::ThinkingLevel =
                    serde_json::from_str(&format!("\"{level}\""))
                        .map_err(|_| format!("invalid thinking level {level:?}"))?;
                tack_ai::codebuddy::effort_for_level(level)
            }
        };

        let start = std::time::Instant::now();
        let outcome = tack_ai::codebuddy::ask_codebuddy(
            &params.prompt,
            mode,
            params.model.as_deref(),
            effort.as_deref(),
            cancel,
            &|accumulated| {
                // Progress: tail of the streamed answer.
                let tail: String = accumulated.chars().rev().take(200).collect();
                let tail: String = tail.chars().rev().collect();
                on_update(AgentToolResult::text(format!(
                    "◉ CodeBuddy ({}s) …{tail}",
                    start.elapsed().as_secs()
                )));
            },
        )
        .await?;

        let elapsed = start.elapsed().as_secs();
        let actions = action_summary(&outcome.tool_uses);
        let text = if actions.is_empty() {
            outcome.text
        } else {
            format!("{}\n\n[CodeBuddy actions: {actions}]", outcome.text)
        };
        let mut result = AgentToolResult::text(text);
        result.details = serde_json::json!({
            "prompt": params.prompt,
            "executionTimeMs": start.elapsed().as_millis(),
            "actions": actions,
            "elapsedSecs": elapsed,
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn action_summary_counts_and_shortens() {
        let uses = vec![
            "Bash".to_string(),
            "Bash".to_string(),
            "Read".to_string(),
            "mcp__tack__echo".to_string(),
        ];
        assert_eq!(action_summary(&uses), "bash×2, read, echo");
        assert_eq!(action_summary(&[]), "");
    }

    #[test]
    fn schema_has_expected_params() {
        use tack_agent_core::AgentTool as _;
        let schema = AskCodebuddyTool::new().parameters_schema();
        let props = schema["properties"].as_object().unwrap();
        for key in ["prompt", "mode", "model", "thinking"] {
            assert!(props.contains_key(key), "missing {key}: {schema}");
        }
        assert_eq!(schema["required"][0], "prompt");
    }

    /// The tool is bound to the codebuddy provider: invisible to every
    /// other provider's models at the LLM-context boundary.
    #[test]
    fn tool_is_bound_to_codebuddy_provider() {
        use tack_agent_core::AgentTool as _;
        let tool = AskCodebuddyTool::new();
        assert!(tool.available_for_provider("codebuddy"));
        for provider in ["anthropic", "openai", "google", "ollama", ""] {
            assert!(!tool.available_for_provider(provider), "{provider}");
        }
    }
}
