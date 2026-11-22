//! Shared `streamSimple` option resolution.
//!
//! Output limits must fit the estimated context, not merely the model's
//! advertised completion cap. OpenRouter may tokenize prose less efficiently
//! than the estimate, so the unused margin grows with the input size.

use crate::transcript::get_system_message_text;
use crate::types::{
    ContentBlock, Context, InputContentBlock, Message, Model, StopReason, Usage, UserContent,
};

const CONTEXT_SAFETY_TOKENS: u64 = 4096;
const CONTEXT_SAFETY_RATIO_PERCENT: u64 = 15;
const MIN_OUTPUT_WITH_RATIO_MARGIN: u64 = 32768;
const MIN_MAX_TOKENS: u32 = 1;
const ESTIMATED_IMAGE_CHARS: u64 = 4800;

/// Fit `max_tokens` into the model context window while leaving room for the
/// provider to count more input tokens than our local estimate.
pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u32) -> u32 {
    if model.context_window == 0 {
        return max_tokens.max(MIN_MAX_TOKENS);
    }

    let input = estimate_context_tokens(&context.messages);
    let ratio_margin = input
        .saturating_mul(CONTEXT_SAFETY_RATIO_PERCENT)
        .div_ceil(100);
    let margin = CONTEXT_SAFETY_TOKENS.max(ratio_margin);
    let window = u64::from(model.context_window);
    let mut room = window.saturating_sub(input.saturating_add(margin));
    if room < MIN_OUTPUT_WITH_RATIO_MARGIN {
        room = MIN_OUTPUT_WITH_RATIO_MARGIN
            .min(window.saturating_sub(input.saturating_add(CONTEXT_SAFETY_TOKENS)));
    }
    max_tokens.min(room.max(u64::from(MIN_MAX_TOKENS)) as u32)
}

fn utf16_len(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

fn estimate_text_tokens(text: &str) -> u64 {
    chars_to_tokens(utf16_len(text))
}

fn chars_to_tokens(chars: u64) -> u64 {
    // ceil(chars / 3.5), expressed as ceil(2 * chars / 7).
    chars.saturating_mul(2).div_ceil(7)
}

fn content_chars(content: &UserContent) -> u64 {
    match content {
        UserContent::Text(text) => utf16_len(text),
        UserContent::Blocks(blocks) => blocks.iter().map(input_block_chars).sum(),
    }
}

fn input_block_chars(block: &InputContentBlock) -> u64 {
    match block {
        InputContentBlock::Text { text, .. } => utf16_len(text),
        InputContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
    }
}

fn json_chars<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_string(value)
        .map(|text| utf16_len(&text))
        .unwrap_or_else(|_| utf16_len("[unserializable]"))
}

fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::System(system) => {
            estimate_text_tokens(&get_system_message_text(system))
                + system
                    .tools_added
                    .as_ref()
                    .map_or(0, |tools| chars_to_tokens(json_chars(tools)))
                + system
                    .tools_removed
                    .as_ref()
                    .map_or(0, |tools| chars_to_tokens(json_chars(tools)))
        }
        Message::User(user) => chars_to_tokens(content_chars(&user.content)),
        Message::ToolResult(result) => result
            .content
            .iter()
            .map(input_block_chars)
            .sum::<u64>()
            .saturating_mul(2)
            .div_ceil(7),
        Message::Assistant(assistant) => {
            let chars = assistant
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text, .. } => utf16_len(text),
                    ContentBlock::Thinking { thinking, .. } => utf16_len(thinking),
                    ContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => utf16_len(name) + json_chars(arguments),
                })
                .sum();
            chars_to_tokens(chars)
        }
    }
}

fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
    }
}

fn message_timestamp(message: &Message) -> u64 {
    match message {
        Message::System(message) => message.timestamp,
        Message::User(message) => message.timestamp,
        Message::Assistant(message) => message.timestamp,
        Message::ToolResult(message) => message.timestamp,
    }
}

fn estimate_context_tokens(messages: &[Message]) -> u64 {
    let mut latest_prefix_timestamp = None;
    let mut usage_info = None;
    for (index, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            let applies_to_prefix =
                latest_prefix_timestamp.is_none_or(|timestamp| assistant.timestamp >= timestamp);
            if applies_to_prefix
                && !matches!(
                    assistant.stop_reason,
                    StopReason::Aborted | StopReason::Error
                )
                && calculate_context_tokens(&assistant.usage) > 0
            {
                usage_info = Some((index, calculate_context_tokens(&assistant.usage)));
            }
        }
        latest_prefix_timestamp = Some(latest_prefix_timestamp.map_or_else(
            || message_timestamp(message),
            |timestamp| timestamp.max(message_timestamp(message)),
        ));
    }

    if let Some((index, usage_tokens)) = usage_info {
        return usage_tokens.saturating_add(
            messages[index + 1..]
                .iter()
                .map(estimate_message_tokens)
                .sum::<u64>(),
        );
    }
    messages.iter().map(estimate_message_tokens).sum()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::types::{AssistantMessage, ContentBlock, InputKind, UserMessage};

    const WINDOW: u32 = 1_048_576;

    fn model_with(max_tokens: u32, context_window: u32) -> Model {
        Model {
            id: "gpt-4o-mini".to_string(),
            name: "GPT-4o mini".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputKind::Text],
            cost: Default::default(),
            context_window,
            max_tokens,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn context_with_input(input_tokens: u64) -> Context {
        let model = model_with(1, WINDOW);
        let mut assistant = AssistantMessage::pending(&model);
        assistant.content = vec![ContentBlock::Text {
            text: "ok".to_string(),
            text_signature: None,
        }];
        assistant.usage.input = input_tokens;
        assistant.usage.total_tokens = input_tokens;
        assistant.stop_reason = StopReason::Stop;
        assistant.timestamp = 2;
        Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("hi".to_string()),
                    timestamp: 1,
                }),
                Message::Assistant(assistant),
            ],
            tools: Vec::new(),
        }
    }

    #[test]
    fn leaves_fifteen_percent_for_openrouter_overcount() {
        let model = model_with(943_718, WINDOW);
        let context = context_with_input(220_815);
        let max_tokens = clamp_max_tokens_to_context(&model, &context, 943_718);
        assert_eq!(max_tokens, WINDOW - 220_815 - 33_123);
        assert!(229_747 + u64::from(max_tokens) <= u64::from(WINDOW));
    }

    #[test]
    fn leaves_fixed_margin_for_small_inputs() {
        let model = model_with(WINDOW, WINDOW);
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context_with_input(10_000), WINDOW),
            WINDOW - 10_000 - 4096
        );
    }

    #[test]
    fn keeps_model_output_limit_when_it_fits() {
        let model = model_with(64_000, WINDOW);
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context_with_input(100_000), 64_000),
            64_000
        );
    }

    #[test]
    fn ratio_fallback_caps_output_at_32768() {
        let model = model_with(943_718, WINDOW);
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context_with_input(912_000), 943_718),
            32_768
        );
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context_with_input(1_030_000), 943_718),
            WINDOW - 1_030_000 - 4096
        );
    }

    #[test]
    fn small_windows_keep_only_fixed_margin() {
        let model = model_with(32_768, 32_768);
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context_with_input(20_000), 32_768),
            32_768 - 20_000 - 4096
        );
    }

    #[test]
    fn overfull_input_still_requests_one_token() {
        let model = model_with(943_718, WINDOW);
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context_with_input(1_050_000), 943_718),
            1
        );
        let unknown = model_with(0, 0);
        assert_eq!(
            clamp_max_tokens_to_context(&unknown, &Context::default(), 0),
            1
        );
    }

    #[test]
    fn usage_anchor_adds_trailing_messages() {
        let mut context = context_with_input(100_000);
        context.messages.push(Message::User(UserMessage {
            content: UserContent::Text("1234567".to_string()),
            timestamp: 3,
        }));
        assert_eq!(estimate_context_tokens(&context.messages), 100_002);
    }
}
