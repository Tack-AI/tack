use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::Model;

pub(crate) const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];
/// Port of TS `isOpenAIReasoningDetail`.
pub(crate) fn is_openai_reasoning_detail(detail: &Value) -> bool {
    let Some(obj) = detail.as_object() else {
        return false;
    };
    let common = obj.get("id").is_none_or(|v| v.is_null() || v.is_string())
        && obj.get("format").is_none_or(Value::is_string)
        && obj.get("index").is_none_or(Value::is_number);
    if !common {
        return false;
    }
    match obj.get("type").and_then(Value::as_str) {
        Some("reasoning.summary") => obj.get("summary").is_some_and(Value::is_string),
        Some("reasoning.encrypted") => obj.get("data").is_some_and(Value::is_string),
        Some("reasoning.text") => {
            obj.get("text").is_some_and(Value::is_string)
                && obj
                    .get("signature")
                    .is_none_or(|v| v.is_null() || v.is_string())
        }
        _ => false,
    }
}

/// Port of TS `parseOpenAIReasoningDetails`: a thinking signature holding a
/// JSON array of `reasoning_details` entries replays verbatim.
pub(crate) fn parse_openai_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let parsed: Value = serde_json::from_str(signature?).ok()?;
    let arr = parsed.as_array()?;
    if arr.is_empty() || !arr.iter().all(is_openai_reasoning_detail) {
        return None;
    }
    Some(arr.clone())
}

/// Port of TS `parseLegacyEncryptedReasoningDetail`: older sessions stored a
/// single encrypted entry in the tool call's `thoughtSignature`.
pub(crate) fn parse_legacy_encrypted_reasoning_detail(signature: Option<&str>) -> Option<Value> {
    let parsed: Value = serde_json::from_str(signature?).ok()?;
    if is_openai_reasoning_detail(&parsed)
        && parsed.get("type").and_then(Value::as_str) == Some("reasoning.encrypted")
        && parsed
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        && parsed
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    {
        Some(parsed)
    } else {
        None
    }
}

/// Port of TS `fillMissingCommonReasoningDetailFields`.
fn fill_missing_common_reasoning_detail_fields(target: &mut Value, source: &Value) {
    if target.get("id").is_none_or(Value::is_null)
        && let Some(id) = source.get("id")
    {
        target["id"] = id.clone();
    }
    if target
        .get("format")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        && let Some(format) = source.get("format").and_then(Value::as_str)
    {
        target["format"] = json!(format);
    }
    if target.get("index").is_none_or(Value::is_null)
        && let Some(index) = source.get("index")
    {
        target["index"] = index.clone();
    }
}

/// Port of TS `appendOpenAIReasoningDetail`: OpenRouter streams
/// `reasoning_details` as deltas — consecutive text/summary entries merge
/// into logical entries, while encrypted entries stay opaque and discrete.
pub(crate) fn append_openai_reasoning_detail(details: &mut Vec<Value>, detail: Value) {
    let merge_field = match detail.get("type").and_then(Value::as_str) {
        Some("reasoning.text") => Some("text"),
        Some("reasoning.summary") => Some("summary"),
        _ => None,
    };
    if let Some(field) = merge_field
        && let Some(last) = details.last_mut()
        && last.get("type").and_then(Value::as_str) == detail.get("type").and_then(Value::as_str)
        && let (Some(a), Some(b)) = (
            last.get(field).and_then(Value::as_str).map(str::to_owned),
            detail.get(field).and_then(Value::as_str),
        )
    {
        last[field] = json!(format!("{a}{b}"));
        // TS `lastDetail.signature ||= detail.signature` (text entries only).
        if field == "text"
            && !last
                .get("signature")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            && let Some(sig) = detail.get("signature").and_then(Value::as_str)
        {
            last["signature"] = json!(sig);
        }
        fill_missing_common_reasoning_detail_fields(last, &detail);
        return;
    }
    details.push(detail);
}

/// Compatibility overrides for OpenAI-compatible APIs
/// (`OpenAICompletionsCompat` in TS). Unknown fields are ignored.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OpenAiCompat {
    pub supports_store: Option<bool>,
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub supports_usage_in_streaming: Option<bool>,
    pub supports_finish_reason: Option<bool>,
    pub max_tokens_field: Option<String>,
    pub requires_tool_result_name: Option<bool>,
    pub requires_assistant_after_tool_result: Option<bool>,
    pub requires_thinking_as_text: Option<bool>,
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    pub thinking_format: Option<String>,
    pub zai_tool_stream: Option<bool>,
    pub supports_thinking_token_budget: Option<bool>,
    pub thinking_token_budget_field: Option<String>,
    pub supports_strict_mode: Option<bool>,
    pub cache_control_format: Option<String>,
    pub supports_long_cache_retention: Option<bool>,
    /// Moonshot/Kimi-style caching: cache writes are controlled by the
    /// request-level `prompt_cache_options` field (`{"mode":"implicit","ttl":...}`)
    /// instead of OpenAI's `prompt_cache_key` / `prompt_cache_retention`.
    pub kimi_prompt_cache_options: Option<bool>,
    #[serde(rename = "openRouterRouting")]
    pub open_router_routing: Option<Value>,
    pub vercel_gateway_routing: Option<Value>,
    pub chat_template_kwargs: Option<Value>,
    pub chat_template_args: Option<Value>,
    /// Whether the provider supports OpenAI custom tools with Lark/regex
    /// grammar formats (TS compat.supportsOpenAIGrammarTools).
    pub supports_openai_grammar_tools: Option<bool>,
}

/// Fully-resolved compat settings (no `Option`s on the hot path).
#[derive(Clone, Debug)]
pub struct ResolvedCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_finish_reason: bool,
    pub max_tokens_field: String, // "max_completion_tokens" | "max_tokens"
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub thinking_format: String,
    pub zai_tool_stream: bool,
    pub thinking_token_budget_field: Option<String>,
    pub supports_strict_mode: bool,
    pub cache_control_format: Option<String>,
    pub supports_long_cache_retention: bool,
    pub kimi_prompt_cache_options: bool,
    pub open_router_routing: Option<Value>,
    pub vercel_gateway_routing: Option<Value>,
    pub chat_template_kwargs: Option<Value>,
    pub chat_template_args: Option<Value>,
    pub supports_openai_grammar_tools: bool,
}

/// Port of `detectCompat`: auto-detect from provider name and baseUrl.
fn detect_compat(model: &Model) -> ResolvedCompat {
    let provider = model.provider.as_str();
    let base_url = model.base_url.as_str();

    let is_zai = provider == "zai"
        || provider == "zai-coding-cn"
        || base_url.contains("api.z.ai")
        || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai")
        || base_url.contains("api.together.xyz");
    let is_moonshot = provider == "moonshotai"
        || provider == "moonshotai-cn"
        || base_url.contains("api.moonshot.");
    let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cloudflare_workers_ai =
        provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cloudflare_ai_gateway =
        provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_deepseek = provider == "deepseek" || base_url.to_lowercase().contains("deepseek.com");

    let is_non_standard = is_nvidia
        || provider == "cerebras"
        || base_url.contains("cerebras.ai")
        || provider == "xai"
        || base_url.contains("api.x.ai")
        || is_together
        || base_url.contains("chutes.ai")
        || is_deepseek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers_ai
        || is_cloudflare_ai_gateway
        || is_ant_ling;

    let use_max_tokens = base_url.contains("chutes.ai")
        || is_deepseek
        || is_moonshot
        || is_cloudflare_ai_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;

    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_openrouter_developer_role_model =
        is_openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
    let cache_control_format = if provider == "openrouter" && model.id.starts_with("anthropic/") {
        Some("anthropic".to_string())
    } else {
        None
    };

    ResolvedCompat {
        supports_store: !is_non_standard,
        supports_developer_role: is_openrouter_developer_role_model
            || (!is_non_standard && !is_openrouter),
        supports_reasoning_effort: !is_grok
            && !is_zai
            && !is_moonshot
            && !is_together
            && !is_cloudflare_ai_gateway
            && !is_nvidia
            && !is_ant_ling,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: if use_max_tokens {
            "max_tokens".to_string()
        } else {
            "max_completion_tokens".to_string()
        },
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: is_deepseek,
        thinking_format: if is_deepseek {
            "deepseek"
        } else if is_zai {
            "zai"
        } else if is_together {
            "together"
        } else if is_ant_ling {
            "ant-ling"
        } else if is_openrouter {
            "openrouter"
        } else {
            "openai"
        }
        .to_string(),
        zai_tool_stream: false,
        thinking_token_budget_field: None,
        // OpenAI compatibility alone does not imply strict JSON-schema tool
        // support (TS #9816). Generated capable models opt in explicitly via
        // catalog `compat.supportsStrictMode`; Cerebras is excluded (mixed
        // strict/unstrict tools 400, TS #9804).
        supports_strict_mode: false,
        cache_control_format,
        supports_long_cache_retention: !(is_together
            || is_cloudflare_workers_ai
            || is_cloudflare_ai_gateway
            || is_nvidia
            || is_ant_ling
            // Moonshot has its own prompt_cache_options contract (see
            // kimi_prompt_cache_options); OpenAI's prompt_cache_retention
            // is not part of it.
            || is_moonshot),
        kimi_prompt_cache_options: is_moonshot,
        open_router_routing: None,
        vercel_gateway_routing: None,
        chat_template_kwargs: None,
        chat_template_args: None,
        // First-party OpenAI supports custom (grammar) tools; everyone else
        // needs an explicit compat override (TS catalog opt-in).
        supports_openai_grammar_tools: provider == "openai",
    }
}

/// `getCompat`: detection base, overridden by explicit `model.compat`.
pub(crate) fn get_compat(model: &Model) -> ResolvedCompat {
    let detected = detect_compat(model);
    let Some(overrides) = model
        .compat
        .as_ref()
        .and_then(|v| serde_json::from_value::<OpenAiCompat>(v.clone()).ok())
    else {
        return detected;
    };

    ResolvedCompat {
        supports_store: overrides.supports_store.unwrap_or(detected.supports_store),
        supports_developer_role: overrides
            .supports_developer_role
            .unwrap_or(detected.supports_developer_role),
        supports_reasoning_effort: overrides
            .supports_reasoning_effort
            .unwrap_or(detected.supports_reasoning_effort),
        supports_usage_in_streaming: overrides
            .supports_usage_in_streaming
            .unwrap_or(detected.supports_usage_in_streaming),
        supports_finish_reason: overrides
            .supports_finish_reason
            .unwrap_or(detected.supports_finish_reason),
        max_tokens_field: overrides
            .max_tokens_field
            .unwrap_or(detected.max_tokens_field),
        requires_tool_result_name: overrides
            .requires_tool_result_name
            .unwrap_or(detected.requires_tool_result_name),
        requires_assistant_after_tool_result: overrides
            .requires_assistant_after_tool_result
            .unwrap_or(detected.requires_assistant_after_tool_result),
        requires_thinking_as_text: overrides
            .requires_thinking_as_text
            .unwrap_or(detected.requires_thinking_as_text),
        requires_reasoning_content_on_assistant_messages: overrides
            .requires_reasoning_content_on_assistant_messages
            .unwrap_or(detected.requires_reasoning_content_on_assistant_messages),
        thinking_format: overrides
            .thinking_format
            .unwrap_or(detected.thinking_format),
        zai_tool_stream: overrides
            .zai_tool_stream
            .unwrap_or(detected.zai_tool_stream),
        thinking_token_budget_field: if overrides.supports_thinking_token_budget == Some(true) {
            Some("thinking_token_budget".to_string())
        } else {
            overrides
                .thinking_token_budget_field
                .or(detected.thinking_token_budget_field)
        },
        supports_strict_mode: overrides
            .supports_strict_mode
            .unwrap_or(detected.supports_strict_mode),
        cache_control_format: overrides
            .cache_control_format
            .or(detected.cache_control_format),
        supports_long_cache_retention: overrides
            .supports_long_cache_retention
            .unwrap_or(detected.supports_long_cache_retention),
        kimi_prompt_cache_options: overrides
            .kimi_prompt_cache_options
            .unwrap_or(detected.kimi_prompt_cache_options),
        open_router_routing: overrides
            .open_router_routing
            .or(detected.open_router_routing),
        vercel_gateway_routing: overrides
            .vercel_gateway_routing
            .or(detected.vercel_gateway_routing),
        chat_template_kwargs: overrides
            .chat_template_kwargs
            .or(detected.chat_template_kwargs),
        chat_template_args: overrides.chat_template_args.or(detected.chat_template_args),
        supports_openai_grammar_tools: overrides
            .supports_openai_grammar_tools
            .unwrap_or(detected.supports_openai_grammar_tools),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn test_model(provider: &str, base_url: &str, compat: Option<Value>) -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: "openai-completions".into(),
            provider: provider.into(),
            base_url: base_url.into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: Default::default(),
            context_window: 0,
            max_tokens: 0,
            sampling_params: None,
            headers: None,
            compat,
        }
    }

    /// TS #9816: OpenAI compatibility alone does not imply strict JSON-schema
    /// tool support — unknown endpoints default to non-strict tools; capable
    /// catalog models opt in via explicit `compat.supportsStrictMode`.
    #[test]
    fn unknown_providers_default_to_non_strict_tools() {
        let local = test_model("my-local", "http://localhost:8080/v1", None);
        assert!(!get_compat(&local).supports_strict_mode);

        // Cerebras is excluded even among known providers (TS #9804): mixed
        // strict/unstrict tool usage 400s.
        let cerebras = test_model("cerebras", "https://api.cerebras.ai/v1", None);
        assert!(!get_compat(&cerebras).supports_strict_mode);

        // Explicit catalog opt-in still wins.
        let capable = test_model(
            "groq",
            "https://api.groq.com/openai/v1",
            Some(json!({ "supportsStrictMode": true })),
        );
        assert!(get_compat(&capable).supports_strict_mode);
    }

    /// Port of TS #8605: consecutive text/summary reasoning_details deltas
    /// merge into logical entries; encrypted entries stay discrete.
    #[test]
    fn reasoning_detail_deltas_merge_consecutive_text_and_summary() {
        let mut details: Vec<Value> = Vec::new();
        for delta in [
            json!({ "type": "reasoning.text", "text": "The", "index": 0 }),
            json!({
                "type": "reasoning.text",
                "text": " user wants the time.",
                "signature": "sha256:text-signature",
                "format": "openai-responses-v1",
                "index": 0,
            }),
            json!({ "type": "reasoning.summary", "summary": "Looked", "index": 0 }),
            json!({
                "type": "reasoning.summary",
                "summary": " up time.",
                "format": "openai-responses-v1",
                "index": 0,
            }),
            json!({ "type": "reasoning.encrypted", "id": "rs_1", "data": "encrypted" }),
            json!({
                "type": "reasoning.summary",
                "summary": "After encrypted block.",
                "format": "openai-responses-v1",
                "index": 0,
            }),
        ] {
            append_openai_reasoning_detail(&mut details, delta);
        }
        assert_eq!(
            details,
            vec![
                json!({
                    "type": "reasoning.text",
                    "text": "The user wants the time.",
                    "index": 0,
                    "signature": "sha256:text-signature",
                    "format": "openai-responses-v1",
                }),
                json!({
                    "type": "reasoning.summary",
                    "summary": "Looked up time.",
                    "index": 0,
                    "format": "openai-responses-v1",
                }),
                json!({ "type": "reasoning.encrypted", "id": "rs_1", "data": "encrypted" }),
                json!({
                    "type": "reasoning.summary",
                    "summary": "After encrypted block.",
                    "format": "openai-responses-v1",
                    "index": 0,
                }),
            ]
        );
    }
}
