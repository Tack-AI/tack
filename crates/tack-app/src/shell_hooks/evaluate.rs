//! LLM-backed hook handlers (`{"type": "prompt"}` / `{"type": "agent"}`):
//! the hook input is evaluated by a model instead of a shell command, and
//! the model answers with the same verdict JSON a command would print
//! (Claude prompt-hook semantics).
//!
//! - `prompt`: one-shot completion, no tools.
//! - `agent`: a short agent loop with read-only tools (read/grep/find/ls)
//!   so the evaluation can inspect the workspace before answering.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

/// The engine-facing evaluation surface (implemented by [`LlmEvaluator`]).
#[async_trait::async_trait]
pub trait HookLlmEvaluator: Send + Sync + std::fmt::Debug {
    /// Run one evaluation; returns the verdict JSON object the model
    /// produced. Implementations must be tolerant: a non-JSON answer is an
    /// error the engine treats as "no verdict" (fail-open).
    async fn evaluate(
        &self,
        instruction: &str,
        model: Option<&str>,
        use_tools: bool,
        input: &Value,
        timeout: Duration,
    ) -> Result<Value, String>;
}

const EVALUATOR_SYSTEM_PROMPT: &str = "\
You are the evaluator for a tack lifecycle hook. You receive an \
instruction and a JSON object describing an event (a tool call, a user \
prompt, a session boundary, ...). Decide the outcome and answer with ONLY \
a JSON object — no prose, no code fences. Fields (all optional):\n\
\n\
- \"decision\": \"approve\" | \"block\" — \"block\" stops the operation.\n\
- \"reason\": string — why (required when decision is \"block\").\n\
- \"hookSpecificOutput\": {\n\
    \"permissionDecision\": \"allow\" | \"deny\" | \"ask\",\n\
    \"permissionDecisionReason\": string,\n\
    \"updatedInput\": object — replacement tool arguments (PreToolUse only),\n\
    \"additionalContext\": string — extra context injected for the model\n\
  }\n\
\n\
Answer \"approve\" (or an empty object {}) when the operation is fine. \
When unsure, prefer \"ask\" over \"deny\".";

const READ_ONLY_TOOLS: &[&str] = &["read", "grep", "find", "ls"];

/// Evaluator backed by the session's model/auth. LLM calls go through a
/// fresh provider adapter (not the session's event-wrapped provider) so
/// evaluations stay invisible to extensions.
pub struct LlmEvaluator {
    pub model: tack_ai::Model,
    pub auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    /// For resolving per-hook `model` overrides (`provider/id`).
    pub agent_dir: std::path::PathBuf,
    pub cwd: std::path::PathBuf,
}

impl std::fmt::Debug for LlmEvaluator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmEvaluator").finish_non_exhaustive()
    }
}

impl LlmEvaluator {
    fn resolve_model(&self, model: Option<&str>) -> tack_ai::Model {
        let Some(spec) = model else {
            return self.model.clone();
        };
        let Some((provider, id)) = spec.split_once('/') else {
            tracing::warn!("hook model {spec:?} is not provider/id; using session model");
            return self.model.clone();
        };
        match crate::model::resolve_model(provider, Some(id), &self.agent_dir) {
            Ok(model) => model,
            Err(e) => {
                tracing::warn!("hook model {spec:?} failed to resolve ({e}); using session model");
                self.model.clone()
            }
        }
    }

    fn build_user_message(instruction: &str, input: &Value) -> String {
        format!(
            "{instruction}\n\nHook input:\n```json\n{}\n```",
            serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
        )
    }

    /// One-shot completion path (prompt hooks).
    async fn evaluate_prompt(
        &self,
        instruction: &str,
        model: &tack_ai::Model,
        input: &Value,
    ) -> Result<Value, String> {
        let provider = tack_ai::provider_for(model)
            .ok_or_else(|| format!("no provider adapter for api {}", model.api))?;
        let context = tack_ai::Context {
            system_prompt: Some(EVALUATOR_SYSTEM_PROMPT.to_string()),
            messages: vec![tack_ai::Message::user(Self::build_user_message(
                instruction,
                input,
            ))],
            tools: Vec::new(),
        };
        let resolved = self
            .auth
            .resolve()
            .await
            .map_err(|e| format!("hook evaluator auth failed: {e}"))?;
        let mut model = model.clone();
        if let Some(base_url) = &resolved.base_url {
            model.base_url = base_url.clone();
        }
        let options = tack_ai::StreamOptions {
            api_key: resolved.api_key,
            headers: resolved.headers,
            ..Default::default()
        };
        let message = provider.complete(&model, &context, options).await;
        if message.stop_reason == tack_ai::StopReason::Error {
            return Err(message
                .error_message
                .unwrap_or_else(|| "hook evaluator LLM call failed".to_string()));
        }
        let text = assistant_text(&message);
        extract_json(&text)
    }

    /// Multi-turn loop with read-only tools (agent hooks).
    async fn evaluate_agent(
        &self,
        instruction: &str,
        model: &tack_ai::Model,
        input: &Value,
    ) -> Result<Value, String> {
        let provider = tack_ai::provider_for(model)
            .ok_or_else(|| format!("no provider adapter for api {}", model.api))?;
        let services = tack_tools::default_services(self.cwd.clone());
        let tools: Vec<Arc<dyn tack_agent_core::AgentTool>> =
            tack_tools::create_coding_tools(&services)
                .into_iter()
                .filter(|t| READ_ONLY_TOOLS.contains(&t.name()))
                .collect();
        let config = tack_agent_core::AgentLoopConfig {
            model: model.clone(),
            provider,
            hooks: Arc::new(tack_agent_core::NoopHooks),
            tool_execution: tack_agent_core::ToolExecutionMode::Parallel,
            reasoning: None,
            auth: self.auth.clone(),
            max_tokens: None,
            temperature: None,
            session_id: None,
            // Tiny hook-gating call: defer to the provider's own
            // resolution (TACK_CACHE_RETENTION env, then short).
            cache_retention: None,
            fallback_models: Vec::new(),
            tool_pool: Vec::new(),
            retry_cancel: None,
        };
        let context = tack_agent_core::AgentContext {
            system_prompt: Some(EVALUATOR_SYSTEM_PROMPT.to_string()),
            messages: Vec::new(),
            tools,
        };
        let mut stream = tack_agent_core::agent_loop(
            vec![tack_agent_core::AgentMessage::user(
                Self::build_user_message(instruction, input),
            )],
            context,
            config,
            tokio_util::sync::CancellationToken::new(),
        );
        let mut final_text = String::new();
        let mut error: Option<String> = None;
        while let Some(event) = stream.next().await {
            match event {
                tack_agent_core::AgentEvent::MessageEnd {
                    message: tack_agent_core::AgentMessage::Assistant(assistant),
                } => {
                    let text = assistant_text(&assistant);
                    if !text.is_empty() {
                        final_text = text;
                    }
                }
                tack_agent_core::AgentEvent::AgentEnd { messages } => {
                    for message in messages.iter().rev() {
                        if let tack_agent_core::AgentMessage::Assistant(assistant) = message
                            && assistant.stop_reason == tack_ai::StopReason::Error
                        {
                            error = assistant.error_message.clone();
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(error) = error {
            return Err(format!("agent hook evaluator failed: {error}"));
        }
        extract_json(&final_text)
    }
}

#[async_trait::async_trait]
impl HookLlmEvaluator for LlmEvaluator {
    async fn evaluate(
        &self,
        instruction: &str,
        model: Option<&str>,
        use_tools: bool,
        input: &Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let model = self.resolve_model(model);
        let run = async {
            if use_tools {
                self.evaluate_agent(instruction, &model, input).await
            } else {
                self.evaluate_prompt(instruction, &model, input).await
            }
        };
        match tokio::time::timeout(timeout, run).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "hook evaluation timed out ({}s)",
                timeout.as_secs()
            )),
        }
    }
}

/// Concatenate the text blocks of an assistant message.
fn assistant_text(message: &tack_ai::AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            tack_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tolerant JSON extraction: the model was told to answer with only JSON,
/// but accept the first `{...}` span inside prose/fences too.
fn extract_json(text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Ok(value);
    }
    let start = trimmed.find('{');
    let end = trimmed.rfind('}');
    match (start, end) {
        (Some(start), Some(end)) if end > start => {
            serde_json::from_str::<Value>(&trimmed[start..=end])
                .map_err(|e| format!("evaluator answer is not valid JSON: {e}"))
        }
        _ => Err("evaluator produced no JSON object".to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn extract_json_handles_fences_and_prose() {
        let fenced = "```json\n{\"decision\": \"approve\"}\n```";
        assert_eq!(
            extract_json(fenced).unwrap()["decision"],
            serde_json::json!("approve")
        );
        let prose = "Sure! {\"decision\": \"block\", \"reason\": \"no\"} hope that helps";
        assert_eq!(
            extract_json(prose).unwrap()["reason"],
            serde_json::json!("no")
        );
        assert!(extract_json("no json here").is_err());
    }

    /// Bare, well-formed verdict JSON parses as-is (the happy path the
    /// system prompt asks the model for).
    #[test]
    fn extract_json_parses_bare_verdict() {
        let value = extract_json(r#"{"decision": "approve"}"#).unwrap();
        assert_eq!(value, serde_json::json!({"decision": "approve"}));
    }

    /// An empty object is the documented "approve" verdict: every
    /// optional field is absent and consumers see their defaults.
    #[test]
    fn extract_json_empty_object_is_approve_default() {
        let value = extract_json("{}").unwrap();
        assert!(value.get("decision").is_none());
        assert!(value.get("reason").is_none());
        assert!(value.get("hookSpecificOutput").is_none());
    }

    /// Both `decision` values the protocol documents round-trip verbatim;
    /// `reason` rides along for "block".
    #[test]
    fn extract_json_decision_values() {
        for decision in ["approve", "block"] {
            let text = format!(r#"{{"decision": "{decision}", "reason": "because"}}"#);
            let value = extract_json(&text).unwrap();
            assert_eq!(value["decision"], serde_json::json!(decision));
            assert_eq!(value["reason"], serde_json::json!("because"));
        }
    }

    /// hookSpecificOutput.permissionDecision round-trips for all three
    /// values the engine maps (allow/deny/ask), plus its reason string.
    #[test]
    fn extract_json_permission_decision_values() {
        for permission in ["allow", "deny", "ask"] {
            let text = format!(
                r#"{{"hookSpecificOutput": {{"permissionDecision": "{permission}", "permissionDecisionReason": "r"}}}}"#
            );
            let value = extract_json(&text).unwrap();
            let specific = &value["hookSpecificOutput"];
            assert_eq!(
                specific["permissionDecision"],
                serde_json::json!(permission)
            );
            assert_eq!(specific["permissionDecisionReason"], serde_json::json!("r"));
        }
    }

    /// updatedInput is a replacement tool-argument object and must be
    /// preserved verbatim, nested values included.
    #[test]
    fn extract_json_preserves_updated_input() {
        let text = r#"{"hookSpecificOutput": {"updatedInput": {"command": "ls -la", "flags": ["--color"], "depth": 2}}}"#;
        let value = extract_json(text).unwrap();
        assert_eq!(
            value["hookSpecificOutput"]["updatedInput"],
            serde_json::json!({"command": "ls -la", "flags": ["--color"], "depth": 2})
        );
    }

    /// additionalContext is a free-form string injected for the model.
    #[test]
    fn extract_json_preserves_additional_context() {
        let text = r#"{"hookSpecificOutput": {"additionalContext": "be careful with rm"}}"#;
        let value = extract_json(text).unwrap();
        assert_eq!(
            value["hookSpecificOutput"]["additionalContext"],
            serde_json::json!("be careful with rm")
        );
    }

    /// Fields with the wrong JSON type do not fail extraction — the
    /// extraction layer is type-tolerant and consumers read via `as_str`.
    #[test]
    fn extract_json_tolerates_wrong_field_types() {
        let text = r#"{"decision": 42, "hookSpecificOutput": "nope", "reason": null}"#;
        let value = extract_json(text).unwrap();
        assert!(value["decision"].as_str().is_none());
        assert!(value["hookSpecificOutput"].as_object().is_none());
        assert!(value["reason"].is_null());
    }

    /// Garbage that merely contains braces is still an error, as are
    /// empty / unbalanced inputs.
    #[test]
    fn extract_json_rejects_braced_garbage() {
        assert!(extract_json("{not json}").is_err());
        assert!(extract_json("").is_err());
        assert!(extract_json("{").is_err());
        assert!(extract_json("}").is_err());
    }

    /// Two separate objects in prose cannot be merged: the first-`{` /
    /// last-`}` span is not valid JSON, so extraction fails (the engine
    /// treats this as "no verdict", fail-open).
    #[test]
    fn extract_json_rejects_multiple_objects_in_prose() {
        let text = r#"first {"decision": "approve"} then {"decision": "block"}"#;
        assert!(extract_json(text).is_err());
    }

    /// The tolerant path spans from the first `{` to the last `}`, so
    /// trailing prose after the object is accepted and braces inside
    /// string values keep working.
    #[test]
    fn extract_json_spans_first_brace_to_last() {
        let text = r#"Here you go: {"decision": "block", "reason": "matched {pattern}"} — done."#;
        let value = extract_json(text).unwrap();
        assert_eq!(value["reason"], serde_json::json!("matched {pattern}"));
    }

    /// A bare scalar is valid JSON and passes extraction as-is; consumers
    /// read object fields off it, so it simply yields "no verdict"
    /// downstream. A bare word, however, is neither JSON nor braced.
    #[test]
    fn extract_json_passes_through_non_object_json() {
        assert_eq!(extract_json("42").unwrap(), serde_json::json!(42));
        assert_eq!(
            extract_json(r#""approve""#).unwrap(),
            serde_json::json!("approve")
        );
        assert!(extract_json("approve").is_err());
    }

    /// assistant_text joins only the text blocks, in order, with
    /// newlines; thinking/tool blocks are skipped.
    #[test]
    fn assistant_text_concatenates_text_blocks_only() {
        let message = tack_ai::AssistantMessage {
            content: vec![
                tack_ai::ContentBlock::Text {
                    text: "line one".to_string(),
                    text_signature: None,
                },
                tack_ai::ContentBlock::Thinking {
                    thinking: "hmm".to_string(),
                    thinking_signature: None,
                    redacted: None,
                },
                tack_ai::ContentBlock::Text {
                    text: "line two".to_string(),
                    text_signature: None,
                },
            ],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "claude".to_string(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            diagnostics: None,
            usage: tack_ai::Usage::default(),
            stop_reason: tack_ai::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        };
        assert_eq!(assistant_text(&message), "line one\nline two");
    }
}
