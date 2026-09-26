//! Converse request body construction (port of the message conversion in
//! `bedrock-converse-stream.ts`).

use base64::Engine;
use serde_json::{Value, json};

use crate::provider::{StreamOptions, ToolChoice};
use crate::transform::transform_messages;
use crate::types::{ContentBlock, Context, InputContentBlock, Message, Model, UserContent};

/// Bedrock tool call ids: `[a-zA-Z0-9_-]`, max 64 chars.
pub fn sanitize_tool_call_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.chars().take(64).collect()
}

/// Build the ConverseStream command input.
pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let transformed = transform_messages(context.messages.as_slice(), model, None);
    let mut messages: Vec<Value> = Vec::new();

    for message in &transformed {
        match message {
            Message::User(u) => {
                let content: Vec<Value> = match &u.content {
                    UserContent::Text(text) => vec![json!({ "text": or_empty(text) })],
                    UserContent::Blocks(blocks) => blocks.iter().map(user_block).collect(),
                };
                messages.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(a) => {
                let mut content: Vec<Value> = Vec::new();
                for block in &a.content {
                    match block {
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            content.push(json!({ "text": text }));
                        }
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            // Encrypted reasoning from non-Anthropic models is
                            // opaque: replay the stored payload as the
                            // `redactedContent` member instead of lowering it
                            // to reasoning text. On the JSON wire the blob is
                            // base64, so the stored signature passes through —
                            // but a hand-edited session can hold a signature
                            // that is not base64; drop that block instead of
                            // failing the whole request.
                            if *redacted == Some(true) {
                                if let Some(sig) = thinking_signature
                                    && !sig.is_empty()
                                    && base64::engine::general_purpose::STANDARD
                                        .decode(sig)
                                        .is_ok_and(|bytes| !bytes.is_empty())
                                {
                                    content.push(json!({
                                        "reasoningContent": { "redactedContent": sig }
                                    }));
                                }
                                continue;
                            }
                            // Skip empty thinking blocks
                            if thinking.trim().is_empty() {
                                continue;
                            }
                            // Claude on Bedrock replays reasoning with its
                            // signature; without one, fall back to plain text.
                            match thinking_signature {
                                Some(signature) => content.push(json!({
                                    "reasoningContent": {
                                        "reasoningText": { "text": thinking, "signature": signature }
                                    }
                                })),
                                None => content.push(json!({ "text": thinking })),
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            content.push(json!({
                                "toolUse": {
                                    "toolUseId": sanitize_tool_call_id(id),
                                    "name": name,
                                    "input": arguments,
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": content }));
                }
            }
            Message::ToolResult(t) => {
                // Consecutive tool results merge into ONE user message with
                // multiple toolResult blocks.
                let block = tool_result_block(t);
                let merged = matches!(
                    messages.last(),
                    Some(m) if m.get("role").and_then(Value::as_str) == Some("user")
                        && m.pointer("/content/0/toolResult").is_some()
                );
                if merged {
                    let last = messages.last_mut().expect("checked above");
                    last["content"].as_array_mut().expect("array").push(block);
                } else {
                    messages.push(json!({ "role": "user", "content": [block] }));
                }
            }
            // The caller collapses transcripts before building the Context;
            // skip system messages defensively.
            Message::System(_) => {}
        }
    }

    let mut body = json!({
        "modelId": model.id,
        "messages": messages,
    });

    if let Some(system_prompt) = &context.system_prompt {
        body["system"] = json!([{ "text": system_prompt }]);
    }

    let mut inference = serde_json::Map::new();
    if let Some(max_tokens) = options.max_tokens {
        inference.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        inference.insert("temperature".into(), json!(temperature));
    }
    if !inference.is_empty() {
        body["inferenceConfig"] = Value::Object(inference);
    }

    if !context.tools.is_empty() && options.tool_choice != Some(ToolChoice::None) {
        let tools: Vec<Value> = context
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "toolSpec": {
                        "name": tool.name,
                        "description": tool.description,
                        "inputSchema": { "json": tool.parameters },
                    }
                })
            })
            .collect();
        body["toolConfig"] = json!({ "tools": tools, "toolChoice": { "auto": {} } });
    }

    // Claude thinking via additionalModelRequestFields (budget-based).
    if model.reasoning
        && let Some(level) = options.reasoning
    {
        let budgets = options.thinking_budgets.unwrap_or_default();
        body["additionalModelRequestFields"] = json!({
            "thinking": { "type": "enabled", "budget_tokens": budgets.for_level(level) },
            "anthropic_beta": ["interleaved-thinking-2025-05-14"],
        });
    }

    for (k, v) in &options.sampling_params {
        body[k.as_str()] = v.clone();
    }

    body
}

fn or_empty(text: &str) -> &str {
    if text.is_empty() { "<empty>" } else { text }
}

fn user_block(block: &InputContentBlock) -> Value {
    match block {
        InputContentBlock::Text { text, .. } => json!({ "text": or_empty(text) }),
        InputContentBlock::Image { data, mime_type } => json!({
            "image": {
                "format": mime_type.rsplit('/').next().unwrap_or("png"),
                // Converse blobs are base64 over the JSON wire; our data
                // already is.
                "source": { "bytes": data },
            }
        }),
    }
}

fn tool_result_block(t: &crate::types::ToolResultMessage) -> Value {
    let content: Vec<Value> = t
        .content
        .iter()
        .map(|block| match block {
            InputContentBlock::Text { text, .. } => json!({ "text": text }),
            InputContentBlock::Image { data, mime_type } => json!({
                "image": {
                    "format": mime_type.rsplit('/').next().unwrap_or("png"),
                    "source": { "bytes": data },
                }
            }),
        })
        .collect();
    let content = if content.is_empty() {
        vec![json!({ "text": "(no tool output)" })]
    } else {
        content
    };
    json!({
        "toolResult": {
            "toolUseId": sanitize_tool_call_id(&t.tool_call_id),
            "content": content,
            "status": if t.is_error { "error" } else { "success" },
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::types::{Message, ToolResultMessage};

    fn model() -> Model {
        Model {
            id: "us.anthropic.claude-sonnet-4-5".into(),
            name: "Claude".into(),
            api: "bedrock-converse-stream".into(),
            provider: "amazon-bedrock".into(),
            base_url: String::new(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![crate::types::InputKind::Text],
            cost: crate::types::ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn sanitizes_tool_call_ids() {
        assert_eq!(sanitize_tool_call_id("call_abc:123/x"), "call_abc_123_x");
        let long = "a".repeat(100);
        assert_eq!(sanitize_tool_call_id(&long).len(), 64);
    }

    #[test]
    fn consecutive_tool_results_merge() {
        let make_result = |id: &str| {
            Message::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: "read".into(),
                content: vec![InputContentBlock::text("ok")],
                details: None,
                usage: None,
                is_error: false,
                timestamp: 1,
            })
        };
        let context = Context {
            system_prompt: None,
            messages: vec![Message::user("hi"), make_result("r1"), make_result("r2")],
            tools: vec![],
        };
        let body = build_request_body(&model(), &context, &StreamOptions::default());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "user");
        let content = messages[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["toolResult"]["toolUseId"], "r1");
        assert_eq!(content[1]["toolResult"]["toolUseId"], "r2");
        assert_eq!(content[0]["toolResult"]["status"], "success");
    }

    #[test]
    fn thinking_replay_uses_reasoning_content_with_signature() {
        let mut assistant = crate::types::AssistantMessage::pending(&model());
        assistant.stop_reason = crate::types::StopReason::Stop;
        assistant.content = vec![
            ContentBlock::Thinking {
                thinking: "hmm".into(),
                thinking_signature: Some("sig".into()),
                redacted: None,
            },
            ContentBlock::text("answer"),
        ];
        let context = Context {
            system_prompt: None,
            messages: vec![Message::user("hi"), Message::Assistant(assistant)],
            tools: vec![],
        };
        let body = build_request_body(&model(), &context, &StreamOptions::default());
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(
            content[0]["reasoningContent"]["reasoningText"]["text"],
            "hmm"
        );
        assert_eq!(
            content[0]["reasoningContent"]["reasoningText"]["signature"],
            "sig"
        );
        assert_eq!(content[1]["text"], "answer");
    }

    /// Redacted (encrypted) reasoning from non-Anthropic models on Bedrock
    /// replays as the opaque `redactedContent` member (TS #8314), even with
    /// empty thinking text, and stays ahead of the toolUse it belongs to.
    #[test]
    fn redacted_thinking_replays_as_redacted_content() {
        let redacted_base64 = "cnNuXzVaVnJpZjRKMGJYSXFtV2RsZWRqN1FJRmVOaWtSUWJF";
        let mut assistant = crate::types::AssistantMessage::pending(&model());
        assistant.stop_reason = crate::types::StopReason::ToolUse;
        assistant.content = vec![
            ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: Some(redacted_base64.into()),
                redacted: Some(true),
            },
            ContentBlock::ToolCall {
                id: "tool-1".into(),
                name: "read".into(),
                arguments: json!({ "path": "/tmp/a.txt" }),
                thought_signature: None,
                namespace: None,
            },
        ];
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::user("read the file"),
                Message::Assistant(assistant),
            ],
            tools: vec![],
        };
        let body = build_request_body(&model(), &context, &StreamOptions::default());
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(
            content[0],
            json!({ "reasoningContent": { "redactedContent": redacted_base64 } })
        );
        assert_eq!(content[1]["toolUse"]["toolUseId"], "tool-1");
    }

    /// A hand-edited session can hold a signature that is not base64; drop
    /// that block instead of failing the whole request.
    #[test]
    fn redacted_thinking_with_invalid_base64_is_dropped() {
        let mut assistant = crate::types::AssistantMessage::pending(&model());
        assistant.stop_reason = crate::types::StopReason::Stop;
        assistant.content = vec![
            ContentBlock::Thinking {
                thinking: "[Reasoning redacted]".into(),
                thinking_signature: Some("!!!not-base64!!!".into()),
                redacted: Some(true),
            },
            ContentBlock::text("done"),
        ];
        let context = Context {
            system_prompt: None,
            messages: vec![Message::user("hi"), Message::Assistant(assistant)],
            tools: vec![],
        };
        let body = build_request_body(&model(), &context, &StreamOptions::default());
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "done");
    }

    #[test]
    fn thinking_budgets_in_additional_fields() {
        let context = Context {
            system_prompt: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
        };
        let options = StreamOptions {
            reasoning: Some(crate::types::ThinkingLevel::Medium),
            ..Default::default()
        };
        let body = build_request_body(&model(), &context, &options);
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            serde_json::json!(8192)
        );
    }
}
