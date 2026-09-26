//! Context-overflow detection. Port of `packages/ai/src/utils/overflow.ts`
//! (including TS #9816-adjacent fixes `661619e87` — Cerebras bodyless
//! overflow scoping — and `0e283203c` — z.ai "Prompt too long").
//!
//! Three cases are detected:
//! 1. Error-based overflow: stop reason `error` with a provider-specific
//!    error message pattern.
//! 2. Silent overflow (z.ai style): a successful stop whose usage exceeds
//!    the context window.
//! 3. Length-stop overflow (Xiaomi MiMo style): the server truncated the
//!    input to fill the window, leaving no room for output.

use std::sync::LazyLock;

use regex::Regex;

use crate::types::{AssistantMessage, StopReason};

/// Provider-specific overflow patterns (see upstream docs for the example
/// messages per provider). Kept in upstream order.
static OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // Anthropic and z.ai token overflow.
        r"prompt (?:is )?too long",
        // Anthropic request byte-size overflow (HTTP 413).
        r"request_too_large",
        // Amazon Bedrock.
        r"input is too long for requested model",
        // OpenAI (Completions & Responses API).
        r"exceeds the context window",
        // OpenAI-compatible proxies (LiteLLM).
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        // Google (Gemini).
        r"input token count.*exceeds the maximum",
        // xAI (Grok).
        r"maximum prompt length is \d+",
        // Groq.
        r"reduce the length of the messages",
        // OpenRouter (most backends).
        r"maximum context length is \d+ tokens",
        // OpenRouter/Poolside.
        r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        // Together AI.
        r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
        // GitHub Copilot.
        r"exceeds the limit of \d+",
        // llama.cpp server.
        r"exceeds the available context size",
        // LM Studio.
        r"greater than the context length",
        // MiniMax.
        r"context window exceeds limit",
        // Kimi For Coding.
        r"exceeded model token limit",
        // Mistral.
        r"too large for model with \d+ maximum context length",
        // DS4 server.
        r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        // z.ai non-standard finish_reason surfaced as error text.
        r"model_context_window_exceeded",
        // Ollama explicit overflow error.
        r"prompt too long; exceeded (?:max )?context length",
        // DashScope / Qwen Token Plan.
        r"range of input length should be",
        // Generic fallbacks.
        r"context[_ ]length[_ ]exceeded",
        r"too many tokens",
        r"token limit exceeded",
    ]
    .iter()
    .map(|p| Regex::new(&format!("(?i){p}")).expect("overflow pattern compiles"))
    .collect()
});

/// Cerebras reports overflow as a bare status with no body (TS `661619e87`
/// scoped this pattern to Cerebras — other providers emit 400/413 for
/// non-overflow reasons).
static CEREBRAS_BODYLESS_OVERFLOW_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)")
        .expect("cerebras overflow pattern compiles")
});

/// Patterns that indicate non-overflow errors (rate limiting, throttling)
/// even when they also match an overflow pattern — e.g. Bedrock formats
/// throttling as "ThrottlingException: Too many tokens, please wait…".
static NON_OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // AWS Bedrock non-overflow errors (human-readable prefixes).
        r"^(?:Throttling error|Service unavailable):",
        r"rate limit",
        r"too many requests",
    ]
    .iter()
    .map(|p| Regex::new(&format!("(?i){p}")).expect("non-overflow pattern compiles"))
    .collect()
});

/// TS `isContextOverflow`: does this assistant message represent a context
/// overflow? Pass the model's context window to also detect silent (z.ai)
/// and length-stop (Xiaomi MiMo) overflows.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    // Case 1: error message patterns.
    if message.stop_reason == StopReason::Error
        && let Some(error) = &message.error_message
    {
        let is_non_overflow = NON_OVERFLOW_PATTERNS.iter().any(|p| p.is_match(error));
        if !is_non_overflow {
            if OVERFLOW_PATTERNS.iter().any(|p| p.is_match(error)) {
                return true;
            }
            if message.provider == "cerebras" && CEREBRAS_BODYLESS_OVERFLOW_PATTERN.is_match(error)
            {
                return true;
            }
        }
    }

    // Case 2: silent overflow (z.ai style) — successful but usage exceeds
    // the context window.
    if let Some(window) = context_window
        && message.stop_reason == StopReason::Stop
    {
        let input_tokens = message.usage.input + message.usage.cache_read;
        if input_tokens > window {
            return true;
        }
    }

    // Case 3: length-stop overflow (Xiaomi MiMo style) — server truncates
    // oversized input to fit the window, leaving no room for output.
    if let Some(window) = context_window
        && message.stop_reason == StopReason::Length
        && message.usage.output == 0
    {
        let input_tokens = message.usage.input + message.usage.cache_read;
        if (input_tokens as f64) >= window as f64 * 0.99 {
            return true;
        }
    }

    false
}

/// TS `isRecoverableLength`: a length stop that ended below the caller or
/// model's intended output limit may be caused by context pressure or
/// provider-side truncation; callers can make one bounded compact-and-retry
/// attempt. `desired_max_output` must be the original limit before any
/// context-based clamping.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && message.usage.output < desired_max_output
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::types::{Model, StopReason};

    fn model(provider: &str) -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: "openai-completions".into(),
            provider: provider.into(),
            base_url: "http://x".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: Default::default(),
            context_window: 200_000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn error_msg(provider: &str, error: &str) -> AssistantMessage {
        let mut m = AssistantMessage::pending(&model(provider));
        m.stop_reason = StopReason::Error;
        m.error_message = Some(error.into());
        m
    }

    #[test]
    fn error_patterns_match_provider_overflows() {
        // Anthropic.
        assert!(is_context_overflow(
            &error_msg(
                "anthropic",
                "prompt is too long: 213462 tokens > 200000 maximum"
            ),
            None
        ));
        assert!(is_context_overflow(
            &error_msg(
                "anthropic",
                "413 {\"error\":{\"type\":\"request_too_large\",\"message\":\"Request exceeds the maximum size\"}}"
            ),
            None
        ));
        // OpenAI.
        assert!(is_context_overflow(
            &error_msg(
                "openai",
                "Your input exceeds the context window of this model"
            ),
            None
        ));
        // z.ai without "is" (TS `0e283203c`).
        assert!(is_context_overflow(
            &error_msg("zai", "{\"code\":\"1261\",\"message\":\"Prompt too long\"}"),
            None
        ));
        // OpenRouter.
        assert!(is_context_overflow(
            &error_msg(
                "openrouter",
                "This endpoint's maximum context length is 131072 tokens. However, you requested about 200000 tokens"
            ),
            None
        ));
        // llama.cpp.
        assert!(is_context_overflow(
            &error_msg(
                "llama-cpp",
                "the request exceeds the available context size, try increasing it"
            ),
            None
        ));
    }

    #[test]
    fn cerebras_bodyless_overflow_is_scoped_to_cerebras() {
        // TS `661619e87`: 400/413 (no body) only means overflow for Cerebras.
        assert!(is_context_overflow(
            &error_msg("cerebras", "400 (no body)"),
            None
        ));
        assert!(is_context_overflow(
            &error_msg("cerebras", "413 status code (no body)"),
            None
        ));
        assert!(!is_context_overflow(
            &error_msg("groq", "400 (no body)"),
            None
        ));
    }

    #[test]
    fn non_overflow_patterns_win() {
        // Bedrock throttling contains "too many tokens" but is rate limiting.
        assert!(!is_context_overflow(
            &error_msg(
                "amazon-bedrock",
                "Throttling error: Too many tokens, please wait before trying again."
            ),
            None
        ));
        assert!(!is_context_overflow(
            &error_msg("openai", "429 Too many requests: rate limit reached"),
            None
        ));
        // Ordinary errors are not overflow.
        assert!(!is_context_overflow(
            &error_msg("openai", "500 internal server error"),
            None
        ));
    }

    #[test]
    fn silent_overflow_via_usage() {
        let mut m = AssistantMessage::pending(&model("zai"));
        m.stop_reason = StopReason::Stop;
        m.usage.input = 150_000;
        m.usage.cache_read = 60_000;
        assert!(is_context_overflow(&m, Some(200_000)));
        assert!(!is_context_overflow(&m, Some(300_000)));
        // Without a context window, silent overflow is undetectable.
        assert!(!is_context_overflow(&m, None));
    }

    #[test]
    fn length_stop_overflow_via_filled_window() {
        let mut m = AssistantMessage::pending(&model("xiaomi"));
        m.stop_reason = StopReason::Length;
        m.usage.input = 199_000;
        m.usage.output = 0;
        assert!(is_context_overflow(&m, Some(200_000)));
        // Room was left for output — not an overflow.
        m.usage.output = 50;
        assert!(!is_context_overflow(&m, Some(200_000)));
    }

    #[test]
    fn recoverable_length_matches_upstream() {
        let mut m = AssistantMessage::pending(&model("openai"));
        m.stop_reason = StopReason::Length;
        m.usage.output = 100;
        assert!(is_recoverable_length(&m, 8192));
        assert!(!is_recoverable_length(&m, 100));
        assert!(!is_recoverable_length(&m, 0));
        m.stop_reason = StopReason::Stop;
        assert!(!is_recoverable_length(&m, 8192));
    }
}
