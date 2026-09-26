//! Cross-API message transformation. Port of
//! `packages/ai/src/api/transform-messages.ts`: downgrades images for
//! non-vision models, normalizes tool-call IDs across providers, converts
//! thinking blocks for foreign models, drops errored/aborted assistant
//! messages, and synthesizes results for orphaned tool calls.

use std::collections::{HashMap, HashSet};

use crate::types::{
    AssistantMessage, ContentBlock, InputContentBlock, Message, Model, StopReason,
    ToolResultMessage, now_millis,
};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

fn replace_images_with_placeholder(
    content: &[InputContentBlock],
    placeholder: &str,
) -> Vec<InputContentBlock> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;

    for block in content {
        match block {
            InputContentBlock::Image { .. } => {
                if !previous_was_placeholder {
                    result.push(InputContentBlock::text(placeholder));
                }
                previous_was_placeholder = true;
            }
            InputContentBlock::Text { text, .. } => {
                result.push(block.clone());
                previous_was_placeholder = text == placeholder;
            }
        }
    }

    result
}

fn downgrade_unsupported_images(messages: Vec<Message>, model: &Model) -> Vec<Message> {
    if model.supports_images() {
        return messages;
    }

    messages
        .into_iter()
        .map(|msg| match msg {
            Message::User(mut u) => {
                if let crate::types::UserContent::Blocks(blocks) = &u.content {
                    u.content = crate::types::UserContent::Blocks(replace_images_with_placeholder(
                        blocks,
                        NON_VISION_USER_IMAGE_PLACEHOLDER,
                    ));
                }
                Message::User(u)
            }
            Message::ToolResult(mut t) => {
                t.content =
                    replace_images_with_placeholder(&t.content, NON_VISION_TOOL_IMAGE_PLACEHOLDER);
                Message::ToolResult(t)
            }
            other => other,
        })
        .collect()
}

/// Transform messages for replay against `model`. `normalize_tool_call_id` is
/// applied to tool-call IDs when the assistant message came from a different
/// provider/api/model (see TS `normalizeToolCallId` usage).
pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<&dyn Fn(&str) -> String>,
) -> Vec<Message> {
    match normalize_tool_call_id {
        Some(normalize) => {
            transform_messages_with_source(messages, model, Some(&|id, _source| normalize(id)))
        }
        None => transform_messages_with_source(messages, model, None),
    }
}

/// Source-aware tool-call id normalizer (id, source assistant message).
pub type ToolCallIdNormalizer<'a> = &'a dyn Fn(&str, &AssistantMessage) -> String;

/// Source-aware variant: the normalizer also receives the assistant message
/// the tool call came from (TS passes `source` so adapters can distinguish
/// foreign from same-provider ids, e.g. OpenAI Responses item ids).
pub fn transform_messages_with_source(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<ToolCallIdNormalizer<'_>>,
) -> Vec<Message> {
    let mut tool_call_id_map: HashMap<String, String> = HashMap::new();
    let messages = downgrade_unsupported_images(messages.to_vec(), model);

    // First pass: thinking blocks, tool call ID normalization.
    let transformed: Vec<Message> = messages
        .into_iter()
        .map(|msg| match msg {
            Message::ToolResult(mut t) => {
                if let Some(normalized) = tool_call_id_map.get(&t.tool_call_id)
                    && *normalized != t.tool_call_id
                {
                    t.tool_call_id = normalized.clone();
                }
                Message::ToolResult(t)
            }
            Message::Assistant(mut a) => {
                let is_same_model =
                    a.provider == model.provider && a.api == model.api && a.model == model.id;

                let mut content: Vec<ContentBlock> = Vec::new();
                // Take the blocks out so `a` can be borrowed as the normalizer's
                // source message while we iterate.
                let blocks: Vec<ContentBlock> = std::mem::take(&mut a.content);
                for block in blocks {
                    match block {
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if redacted == Some(true) {
                                // Redacted thinking is opaque encrypted content, only
                                // valid for the same model.
                                if is_same_model {
                                    content.push(ContentBlock::Thinking {
                                        thinking,
                                        thinking_signature,
                                        redacted,
                                    });
                                }
                                continue;
                            }
                            if is_same_model && thinking_signature.is_some() {
                                content.push(ContentBlock::Thinking {
                                    thinking,
                                    thinking_signature,
                                    redacted,
                                });
                                continue;
                            }
                            if thinking.trim().is_empty() {
                                continue;
                            }
                            if is_same_model {
                                content.push(ContentBlock::Thinking {
                                    thinking,
                                    thinking_signature,
                                    redacted,
                                });
                            } else {
                                content.push(ContentBlock::text(thinking));
                            }
                        }
                        ContentBlock::Text {
                            text,
                            text_signature,
                        } => {
                            // Same-model replay keeps the signature (OpenAI
                            // Responses message ids, Google thought signatures);
                            // cross-model drops it (TS transformMessages).
                            if is_same_model {
                                content.push(ContentBlock::Text {
                                    text,
                                    text_signature,
                                });
                            } else {
                                content.push(ContentBlock::text(text));
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            thought_signature,
                            namespace,
                        } => {
                            let mut id = id;
                            let thought_signature = if is_same_model {
                                thought_signature
                            } else {
                                None
                            };
                            if !is_same_model && let Some(normalize) = normalize_tool_call_id {
                                let normalized = normalize(&id, &a);
                                if normalized != id {
                                    tool_call_id_map.insert(id.clone(), normalized.clone());
                                    id = normalized;
                                }
                            }
                            content.push(ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                                thought_signature,
                                namespace,
                            });
                        }
                        other => content.push(other),
                    }
                }

                a.content = content;
                Message::Assistant(a)
            }
            other => other,
        })
        .collect();

    // Second pass: drop errored/aborted assistant messages and insert
    // synthetic results for orphaned tool calls.
    let mut result: Vec<Message> = Vec::new();
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new(); // (id, name)
    let mut existing_tool_result_ids: HashSet<String> = HashSet::new();

    fn insert_synthetic_tool_results(
        result: &mut Vec<Message>,
        pending: &mut Vec<(String, String)>,
        existing: &mut HashSet<String>,
    ) {
        if pending.is_empty() {
            return;
        }
        for (id, name) in pending.drain(..) {
            if !existing.contains(&id) {
                result.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: id,
                    tool_name: name,
                    content: vec![InputContentBlock::text("No result provided")],
                    details: None,
                    usage: None,
                    is_error: true,
                    timestamp: now_millis(),
                }));
            }
        }
        existing.clear();
    }

    for msg in transformed {
        match &msg {
            Message::Assistant(a) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                if a.stop_reason == StopReason::Error || a.stop_reason == StopReason::Aborted {
                    continue;
                }
                let tool_calls: Vec<(String, String)> = a
                    .tool_calls()
                    .map(|(id, name, _)| (id.to_string(), name.to_string()))
                    .collect();
                if !tool_calls.is_empty() {
                    pending_tool_calls = tool_calls;
                    existing_tool_result_ids.clear();
                }
                result.push(msg);
            }
            Message::ToolResult(t) => {
                existing_tool_result_ids.insert(t.tool_call_id.clone());
                result.push(msg);
            }
            Message::User(_) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                result.push(msg);
            }
            // System messages pass through untouched; providers decide how to
            // place them (TS transcript.ts resolveTranscript semantics).
            Message::System(_) => {
                result.push(msg);
            }
        }
    }

    insert_synthetic_tool_results(
        &mut result,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
    );

    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::types::InputKind;

    fn model(api: &str, provider: &str, id: &str) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: api.to_string(),
            provider: provider.to_string(),
            base_url: String::new(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputKind::Text],
            cost: Default::default(),
            context_window: 200_000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn assistant(model: &Model, content: Vec<ContentBlock>) -> Message {
        let mut a = AssistantMessage::pending(model);
        a.stop_reason = StopReason::Stop;
        a.content = content;
        Message::Assistant(a)
    }

    /// Regression: transform used to drop text signatures unconditionally.
    /// Same-model replay must keep them (OpenAI Responses message ids,
    /// Google thought signatures); cross-model must drop them (TS).
    #[test]
    fn same_model_text_signature_is_preserved() {
        let m = model("openai-responses", "openai", "gpt-5");
        let block = ContentBlock::Text {
            text: "hello".to_string(),
            text_signature: Some(r#"{"v":1,"id":"msg_123"}"#.to_string()),
        };
        let out = transform_messages(&[assistant(&m, vec![block.clone()])], &m, None);
        let Message::Assistant(a) = &out[0] else {
            panic!("assistant")
        };
        assert_eq!(a.content[0], block);

        let other = model("anthropic-messages", "anthropic", "claude-sonnet-4-5");
        let out = transform_messages(&[assistant(&m, vec![block])], &other, None);
        let Message::Assistant(a) = &out[0] else {
            panic!("assistant")
        };
        assert_eq!(
            a.content[0],
            ContentBlock::Text {
                text: "hello".to_string(),
                text_signature: None
            }
        );
    }

    #[test]
    fn orphaned_tool_calls_get_synthetic_results() {
        let m = model("anthropic-messages", "anthropic", "claude-sonnet-4-5");
        let call = ContentBlock::ToolCall {
            id: "toolu_1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({}),
            thought_signature: None,
            namespace: None,
        };
        let out = transform_messages(
            &[
                Message::user("hi"),
                assistant(&m, vec![call]),
                Message::user("next"),
            ],
            &m,
            None,
        );
        assert_eq!(out.len(), 4);
        let Message::ToolResult(t) = &out[2] else {
            panic!("synthetic tool result: {out:?}")
        };
        assert_eq!(t.tool_call_id, "toolu_1");
        assert_eq!(t.tool_name, "read");
        assert!(t.is_error);
    }

    #[test]
    fn errored_assistant_messages_are_dropped_with_their_calls() {
        let m = model("anthropic-messages", "anthropic", "claude-sonnet-4-5");
        let mut a = AssistantMessage::pending(&m);
        a.stop_reason = StopReason::Error;
        a.content = vec![ContentBlock::ToolCall {
            id: "toolu_1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({}),
            thought_signature: None,
            namespace: None,
        }];
        let out = transform_messages(
            &[
                Message::user("hi"),
                Message::Assistant(a),
                Message::user("next"),
            ],
            &m,
            None,
        );
        // The errored assistant turn disappears entirely and no synthetic
        // result is synthesized for its tool calls.
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn source_aware_normalizer_sees_the_source_message() {
        let m = model("anthropic-messages", "anthropic", "claude-sonnet-4-5");
        let foreign = model("openai-responses", "openai", "gpt-5");
        let call = ContentBlock::ToolCall {
            id: "call_1|fc_x".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({}),
            thought_signature: None,
            namespace: None,
        };
        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let normalize = |id: &str, source: &AssistantMessage| {
            seen.borrow_mut()
                .push(format!("{}:{}", source.provider, source.api));
            format!("N_{id}")
        };
        let out = transform_messages_with_source(
            &[assistant(&foreign, vec![call])],
            &m,
            Some(&normalize),
        );
        assert_eq!(
            seen.borrow().as_slice(),
            &["openai:openai-responses".to_string()]
        );
        let Message::Assistant(a) = &out[0] else {
            panic!("assistant")
        };
        match &a.content[0] {
            ContentBlock::ToolCall { id, .. } => assert_eq!(id, "N_call_1|fc_x"),
            other => panic!("tool call: {other:?}"),
        }
    }
}
