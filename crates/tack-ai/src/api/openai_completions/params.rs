use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::compat::{
    REASONING_FIELDS, ResolvedCompat, parse_legacy_encrypted_reasoning_detail,
    parse_openai_reasoning_details,
};
use crate::provider::{CacheRetention, StreamOptions};
use crate::transform::transform_messages;
use crate::types::{
    ContentBlock, Context, InputContentBlock, Message, Model, ToolDefinition, UserContent,
};

pub(crate) fn has_auth_header(headers: &BTreeMap<String, String>) -> bool {
    headers.keys().any(|k| {
        let k = k.to_ascii_lowercase();
        k == "authorization" || k == "cf-aig-authorization"
    })
}

pub(crate) fn resolve_cache_retention(options: &StreamOptions) -> CacheRetention {
    if let Some(r) = options.cache_retention {
        return r;
    }
    if std::env::var("TACK_CACHE_RETENTION").is_ok_and(|v| v == "long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m {
        Message::ToolResult(_) => true,
        Message::Assistant(a) => a.has_tool_calls(),
        _ => false,
    })
}

/// `normalizeToolCallId` from the TS adapter.
fn normalize_tool_call_id(id: &str, provider: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    if let Some(sep) = id.find('|') {
        // OpenAI Responses-style "{call_id}|{item_id}" IDs.
        let call_id = sanitize(&id[..sep]);
        let item_id = sanitize(&id[sep + 1..]);
        let combined = if item_id.is_empty() {
            call_id.clone()
        } else {
            format!("{call_id}_{item_id}")
        };
        if combined.len() <= 40 {
            return combined;
        }
        // Cheap deterministic hash stand-in for TS shortHash (display-only).
        let hash = format!("{:08x}", fnv1a(id));
        let prefix_len = 40usize.saturating_sub(hash.len() + 1).max(1);
        return format!("{}_{}", &call_id[..call_id.len().min(prefix_len)], hash);
    }
    if provider == "openai" && id.len() > 40 {
        // Char-boundary truncation: `&id[..40]` is a byte slice and panics
        // when byte 40 falls inside a multi-byte UTF-8 char.
        id.chars().take(40).collect()
    } else {
        id.to_string()
    }
}

fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for b in s.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// tool name → resolved grammar constraint for the request (TS
/// createGrammarToolInputProperties; failures degrade to plain tools).
pub(crate) fn grammar_constraint_map(
    context: &Context,
    compat: &ResolvedCompat,
) -> std::collections::HashMap<String, crate::constrained_sampling::GrammarConstraint> {
    context
        .tools
        .iter()
        .filter_map(|tool| {
            crate::constrained_sampling::resolve_grammar(tool, compat.supports_openai_grammar_tools)
                .ok()
                .flatten()
                .map(|g| (tool.name.clone(), g))
        })
        .collect()
}

fn convert_tools(
    tools: &[ToolDefinition],
    compat: &ResolvedCompat,
    grammar_tools: &std::collections::HashMap<
        String,
        crate::constrained_sampling::GrammarConstraint,
    >,
) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            // Grammar-constrained tools become OpenAI custom tools.
            if let Some(grammar) = grammar_tools.get(&tool.name) {
                return json!({
                    "type": "custom",
                    "custom": {
                        "name": tool.name,
                        "description": tool.description,
                        "format": {
                            "type": "grammar",
                            "grammar": {
                                "syntax": grammar.format,
                                "definition": grammar.definition,
                            },
                        },
                    },
                });
            }
            // Strict JSON-schema sampling: strict:true only when the schema
            // converts cleanly (TS resolveJsonSchemaStrictSampling).
            let (parameters, strict) =
                match crate::constrained_sampling::resolve_json_schema_strict(tool, compat.supports_strict_mode) {
                    Ok(Some(true)) => (
                        crate::constrained_sampling::make_strict_json_schema(&tool.parameters)
                            .unwrap_or_else(|_| tool.parameters.clone()),
                        true,
                    ),
                    _ => (tool.parameters.clone(), false),
                };
            let mut function = json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": parameters,
            });
            if compat.supports_strict_mode {
                function["strict"] = json!(strict);
            }
            json!({ "type": "function", "function": function })
        })
        .collect()
}

/// Port of `convertMessages` (grammar tools and Kimi deferred tools omitted).
fn convert_messages(
    model: &Model,
    context: &Context,
    compat: &ResolvedCompat,
    grammar_tools: &std::collections::HashMap<String, String>,
) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();
    let provider = model.provider.clone();
    let normalize = move |id: &str| normalize_tool_call_id(id, &provider);
    let transformed = transform_messages(context.messages.as_slice(), model, Some(&normalize));

    if let Some(system_prompt) = &context.system_prompt {
        let role = if model.reasoning && compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        params.push(json!({ "role": role, "content": system_prompt }));
    }

    let mut last_role: Option<&'static str> = None;
    let mut i = 0;
    while i < transformed.len() {
        let msg = &transformed[i];

        if compat.requires_assistant_after_tool_result
            && last_role == Some("toolResult")
            && matches!(msg, Message::User(_))
        {
            params.push(
                json!({ "role": "assistant", "content": "I have processed the tool results." }),
            );
        }

        match msg {
            // The caller collapses transcripts before building the Context;
            // skip system messages defensively.
            Message::System(_) => {}
            Message::User(u) => match &u.content {
                UserContent::Text(text) => {
                    params.push(json!({ "role": "user", "content": text }));
                }
                UserContent::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|b| match b {
                            InputContentBlock::Text { text, .. } => {
                                json!({ "type": "text", "text": text })
                            }
                            InputContentBlock::Image { data, mime_type } => json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{mime_type};base64,{data}") },
                            }),
                        })
                        .collect();
                    if content.is_empty() {
                        i += 1;
                        continue;
                    }
                    params.push(json!({ "role": "user", "content": content }));
                }
            },
            Message::Assistant(a) => {
                let mut assistant_msg = json!({
                    "role": "assistant",
                    "content": if compat.requires_assistant_after_tool_result { json!("") } else { Value::Null },
                });

                let text: String = a
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .filter(|t| !t.trim().is_empty())
                    .collect();

                let thinking_blocks: Vec<&ContentBlock> = a
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Thinking { .. }))
                    .collect();
                let non_empty_thinking: Vec<&&ContentBlock> = thinking_blocks
                    .iter()
                    .filter(|b| match b {
                        ContentBlock::Thinking { thinking, .. } => !thinking.trim().is_empty(),
                        _ => false,
                    })
                    .collect();

                // reasoning_details preserved for replay (TS): a thinking
                // signature holding a JSON detail array wins; otherwise fall
                // back to legacy per-tool-call encrypted entries.
                let signed_details = thinking_blocks.iter().find_map(|b| match b {
                    ContentBlock::Thinking {
                        thinking_signature, ..
                    } => parse_openai_reasoning_details(thinking_signature.as_deref()),
                    _ => None,
                });
                let legacy_details: Vec<Value> = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall {
                            thought_signature, ..
                        } => parse_legacy_encrypted_reasoning_detail(thought_signature.as_deref()),
                        _ => None,
                    })
                    .collect();
                let preserved_details = signed_details.or(if legacy_details.is_empty() {
                    None
                } else {
                    Some(legacy_details)
                });

                if !non_empty_thinking.is_empty() {
                    if compat.requires_thinking_as_text {
                        let thinking_text = non_empty_thinking
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let mut parts = vec![json!({ "type": "text", "text": thinking_text })];
                        if !text.is_empty() {
                            parts.push(json!({ "type": "text", "text": text }));
                        }
                        assistant_msg["content"] = Value::Array(parts);
                    } else {
                        if !text.is_empty() {
                            assistant_msg["content"] = json!(text);
                        }
                        // Replay reasoning via the field the provider used, if known
                        // (only when structured reasoning_details didn't survive).
                        let signature = thinking_blocks.iter().find_map(|b| match b {
                            ContentBlock::Thinking {
                                thinking_signature, ..
                            } => thinking_signature.as_deref(),
                            _ => None,
                        });
                        if preserved_details.is_none()
                            && let Some(field) = signature
                            && REASONING_FIELDS.contains(&field)
                        {
                            let joined = non_empty_thinking
                                .iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Thinking { thinking, .. } => {
                                        Some(thinking.as_str())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            assistant_msg[field] = json!(joined);
                        }
                    }
                } else if !text.is_empty() {
                    assistant_msg["content"] = json!(text);
                }

                let tool_calls: Vec<Value> = a
                    .tool_calls()
                    .map(|(id, name, arguments)| {
                        // Grammar tools replay as custom calls with plain
                        // grammar text (TS convertMessages).
                        if let Some(property) = grammar_tools.get(name)
                            && let Ok(input) = crate::constrained_sampling::get_grammar_tool_input(
                                name, arguments, property,
                            )
                        {
                            return json!({
                                "id": id,
                                "type": "custom",
                                "custom": { "name": name, "input": input },
                            });
                        }
                        json!({
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": arguments.to_string() },
                        })
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    assistant_msg["tool_calls"] = Value::Array(tool_calls);
                }
                if let Some(details) = preserved_details {
                    assistant_msg["reasoning_details"] = Value::Array(details);
                }

                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && assistant_msg.get("reasoning_content").is_none()
                {
                    assistant_msg["reasoning_content"] = json!("");
                }

                // Skip assistant messages with neither content nor tool calls.
                let has_content = match &assistant_msg["content"] {
                    Value::Null => false,
                    Value::String(s) => !s.is_empty(),
                    Value::Array(a) => !a.is_empty(),
                    _ => true,
                };
                if !has_content && assistant_msg.get("tool_calls").is_none() {
                    i += 1;
                    continue;
                }
                params.push(assistant_msg);
            }
            Message::ToolResult(_) => {
                let mut image_blocks: Vec<Value> = Vec::new();
                while i < transformed.len() {
                    let Message::ToolResult(t) = &transformed[i] else {
                        break;
                    };
                    let text_result: String = t
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            InputContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let has_images = t
                        .content
                        .iter()
                        .any(|c| matches!(c, InputContentBlock::Image { .. }));
                    let tool_result_text = if !text_result.is_empty() {
                        text_result
                    } else if has_images {
                        "(see attached image)".to_string()
                    } else {
                        "(no tool output)".to_string()
                    };
                    let mut tool_msg = json!({
                        "role": "tool",
                        "content": tool_result_text,
                        "tool_call_id": t.tool_call_id,
                    });
                    if compat.requires_tool_result_name && !t.tool_name.is_empty() {
                        tool_msg["name"] = json!(t.tool_name);
                    }
                    params.push(tool_msg);

                    if has_images && model.supports_images() {
                        for block in &t.content {
                            if let InputContentBlock::Image { data, mime_type } = block {
                                image_blocks.push(json!({
                                    "type": "image_url",
                                    "image_url": { "url": format!("data:{mime_type};base64,{data}") },
                                }));
                            }
                        }
                    }
                    i += 1;
                }
                i = i.saturating_sub(1);

                if !image_blocks.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        params.push(json!({
                            "role": "assistant",
                            "content": "I have processed the tool results.",
                        }));
                    }
                    let mut content = vec![
                        json!({ "type": "text", "text": "Attached image(s) from tool result:" }),
                    ];
                    content.extend(image_blocks);
                    params.push(json!({ "role": "user", "content": content }));
                    last_role = Some("user");
                } else {
                    last_role = Some("toolResult");
                }
                i += 1;
                continue;
            }
        }

        last_role = Some(match msg {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
            // The caller collapses transcripts before building the Context;
            // system messages never reach here in practice.
            Message::System(_) => "system",
        });
        i += 1;
    }

    params
}

fn resolve_thinking_budget(model: &Model, options: &StreamOptions, params: &Value) -> Option<u64> {
    let level = options.reasoning?;
    if !model.reasoning {
        return None;
    }
    let ceiling = params
        .get("max_tokens")
        .or_else(|| params.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(model.max_tokens as u64);
    let budgets = options.thinking_budgets.unwrap_or_default();
    let budget = budgets.for_level(level) as u64;
    let clamped = budget.min(ceiling.saturating_sub(1024));
    (clamped > 0).then_some(clamped)
}

/// Resolve a `chat_template_kwargs`-style value: `{ "$var": "thinking.enabled" | ... }`.
fn resolve_chat_template_values(
    model: &Model,
    options: &StreamOptions,
    template: &Value,
    thinking_budget: Option<u64>,
) -> Option<Value> {
    let obj = template.as_object()?;
    let mut resolved = serde_json::Map::new();
    for (key, value) in obj {
        let out = if let Some(var) = value.get("$var").and_then(Value::as_str) {
            let reasoning_on = options.reasoning.is_some();
            if !reasoning_on && value.get("omitWhenOff").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            match var {
                "thinking.enabled" => json!(reasoning_on),
                "thinking.budget" => match thinking_budget {
                    Some(b) => json!(b),
                    None => continue,
                },
                "thinking.effort" => {
                    let mapped = options
                        .reasoning
                        .and_then(|l| model.thinking_level_value(l).cloned().flatten());
                    match mapped.or_else(|| options.reasoning.map(|l| l.as_str().to_string())) {
                        Some(v) => json!(v),
                        None => continue,
                    }
                }
                _ => continue,
            }
        } else {
            value.clone()
        };
        resolved.insert(key.clone(), out);
    }
    if resolved.is_empty() {
        None
    } else {
        Some(Value::Object(resolved))
    }
}

pub(crate) fn build_params(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &ResolvedCompat,
    retention: CacheRetention,
) -> Value {
    let grammar_constraints = grammar_constraint_map(context, compat);
    let grammar_properties: std::collections::HashMap<String, String> = grammar_constraints
        .iter()
        .map(|(name, g)| (name.clone(), g.input_property.clone()))
        .collect();
    let messages = convert_messages(model, context, compat, &grammar_properties);

    let mut params = json!({
        "model": model.id,
        "messages": messages,
        "stream": true,
    });

    // Prompt caching.
    if compat.kimi_prompt_cache_options {
        // Moonshot/Kimi Chat Completions: cache writes are controlled by the
        // top-level prompt_cache_options field; when omitted the server
        // writes with a 5m TTL by default, so only Long needs an explicit
        // marker. OpenAI's prompt_cache_key / prompt_cache_retention are not
        // part of Kimi's contract and must not be sent.
        if retention == CacheRetention::Long {
            params["prompt_cache_options"] = json!({ "mode": "implicit", "ttl": "1h" });
        }
    } else {
        if ((model.base_url.contains("api.openai.com") && retention != CacheRetention::None)
            || (retention == CacheRetention::Long && compat.supports_long_cache_retention))
            && let Some(session_id) = &options.session_id
        {
            params["prompt_cache_key"] = json!(session_id.chars().take(64).collect::<String>());
        }
        if retention == CacheRetention::Long && compat.supports_long_cache_retention {
            params["prompt_cache_retention"] = json!("24h");
        }
    }

    if compat.supports_usage_in_streaming {
        params["stream_options"] = json!({ "include_usage": true });
    }
    if compat.supports_store {
        params["store"] = json!(false);
    }
    if let Some(max_tokens) = options.max_tokens {
        params[compat.max_tokens_field.as_str()] = json!(max_tokens);
    }
    if let Some(temperature) = options.temperature {
        params["temperature"] = json!(temperature);
    }

    if !context.tools.is_empty() {
        params["tools"] = Value::Array(convert_tools(&context.tools, compat, &grammar_constraints));
        if compat.zai_tool_stream {
            params["tool_stream"] = json!(true);
        }
    } else if has_tool_history(&context.messages) {
        // Anthropic (via LiteLLM/proxy) requires tools when the conversation
        // has tool_calls/tool results.
        params["tools"] = json!([]);
    }

    // Anthropic-style cache_control for OpenRouter Anthropic models.
    if compat.cache_control_format.as_deref() == Some("anthropic")
        && retention != CacheRetention::None
    {
        apply_anthropic_cache_control(&mut params);
    }

    if let Some(choice) = options.tool_choice {
        params["tool_choice"] = json!(match choice {
            crate::provider::ToolChoice::Auto => "auto",
            crate::provider::ToolChoice::None => "none",
        });
    }

    let thinking_budget = resolve_thinking_budget(model, options, &params);
    let effort_string = |model: &Model, options: &StreamOptions| -> Option<String> {
        options.reasoning.map(|l| {
            model
                .thinking_level_value(l)
                .cloned()
                .flatten()
                .unwrap_or_else(|| l.as_str().to_string())
        })
    };
    let off_value = || -> Option<String> {
        model
            .thinking_level_map
            .as_ref()?
            .get("off")
            .cloned()
            .flatten()
    };

    if model.reasoning {
        match compat.thinking_format.as_str() {
            "zai" => {
                params["thinking"] = if options.reasoning.is_some() {
                    json!({ "type": "enabled", "clear_thinking": false })
                } else {
                    json!({ "type": "disabled" })
                };
                if options.reasoning.is_some()
                    && compat.supports_reasoning_effort
                    && let Some(e) = effort_string(model, options)
                {
                    params["reasoning_effort"] = json!(e);
                }
            }
            "qwen" => {
                params["enable_thinking"] = json!(options.reasoning.is_some());
                if options.reasoning.is_some()
                    && compat.supports_reasoning_effort
                    && let Some(e) = effort_string(model, options)
                {
                    params["reasoning_effort"] = json!(e);
                }
            }
            "qwen-chat-template" => {
                params["chat_template_kwargs"] = json!({ "enable_thinking": options.reasoning.is_some(), "preserve_thinking": true });
            }
            "chat-template" => {
                if let Some(template) = &compat.chat_template_kwargs
                    && let Some(values) =
                        resolve_chat_template_values(model, options, template, thinking_budget)
                {
                    params["chat_template_kwargs"] = values;
                }
            }
            "baseten" => {
                if let Some(template) = &compat.chat_template_args
                    && let Some(values) =
                        resolve_chat_template_values(model, options, template, thinking_budget)
                {
                    params["chat_template_args"] = values;
                }
                if compat.supports_reasoning_effort {
                    let mapped = match options.reasoning {
                        Some(l) => model
                            .thinking_level_value(l)
                            .cloned()
                            .flatten()
                            .or_else(|| Some(l.as_str().to_string())),
                        None => off_value(),
                    };
                    if let Some(e) = mapped {
                        params["reasoning_effort"] = json!(e);
                    }
                }
            }
            "deepseek" => {
                if options.reasoning.is_some() {
                    params["thinking"] = json!({ "type": "enabled" });
                } else if model.thinking_level_map.as_ref().and_then(|m| m.get("off"))
                    != Some(&None)
                {
                    // Port of TS `model.thinkingLevelMap?.off !== null`:
                    // send disabled unless the map explicitly sets off to
                    // null (a missing map or missing off key also counts).
                    params["thinking"] = json!({ "type": "disabled" });
                }
                if options.reasoning.is_some()
                    && compat.supports_reasoning_effort
                    && let Some(e) = effort_string(model, options)
                {
                    params["reasoning_effort"] = json!(e);
                }
            }
            "openrouter" => {
                if let Some(e) = effort_string(model, options) {
                    params["reasoning"] = json!({ "effort": e });
                } else if model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|m| m.get("off"))
                    .is_none_or(|v| v.is_some())
                {
                    let off = off_value().unwrap_or_else(|| "none".to_string());
                    params["reasoning"] = json!({ "effort": off });
                }
            }
            "ant-ling" => {
                if let Some(level) = options.reasoning
                    && let Some(Some(mapped)) = model.thinking_level_value(level)
                {
                    params["reasoning"] = json!({ "effort": mapped });
                }
            }
            "together" => {
                params["reasoning"] = json!({ "enabled": options.reasoning.is_some() });
                if options.reasoning.is_some()
                    && compat.supports_reasoning_effort
                    && let Some(e) = effort_string(model, options)
                {
                    params["reasoning_effort"] = json!(e);
                }
            }
            "string-thinking" => {
                if let Some(e) = effort_string(model, options) {
                    params["thinking"] = json!(e);
                } else if let Some(off) = off_value() {
                    params["thinking"] = json!(off);
                } else {
                    params["thinking"] = json!("none");
                }
            }
            _ => {
                // "openai" default
                if options.reasoning.is_some() && compat.supports_reasoning_effort {
                    if let Some(e) = effort_string(model, options) {
                        params["reasoning_effort"] = json!(e);
                    }
                } else if options.reasoning.is_none()
                    && compat.supports_reasoning_effort
                    && let Some(off) = off_value()
                {
                    params["reasoning_effort"] = json!(off);
                }
            }
        }
    }

    // Top-level reasoning token budget (vLLM/Qwen/llama.cpp style).
    if let (Some(field), Some(budget)) = (&compat.thinking_token_budget_field, thinking_budget) {
        params[field.as_str()] = json!(budget);
    }

    // OpenRouter routing / Vercel gateway routing.
    if let Some(routing) = &compat.open_router_routing {
        params["provider"] = routing.clone();
    }
    if let Some(routing) = &compat.vercel_gateway_routing {
        let only = routing.get("only").cloned();
        let order = routing.get("order").cloned();
        if only.is_some() || order.is_some() {
            let mut gateway = serde_json::Map::new();
            if let Some(only) = only {
                gateway.insert("only".to_string(), only);
            }
            if let Some(order) = order {
                gateway.insert("order".to_string(), order);
            }
            params["providerOptions"] = json!({ "gateway": Value::Object(gateway) });
        }
    }

    // Model defaults, then per-request sampling params override everything.
    if let Some(sampling) = &model.sampling_params {
        for (k, v) in sampling {
            params[k.as_str()] = v.clone();
        }
    }
    for (k, v) in &options.sampling_params {
        params[k.as_str()] = v.clone();
    }

    params
}

fn apply_anthropic_cache_control(params: &mut Value) {
    let cc = json!({ "type": "ephemeral" });
    let Some(messages) = params.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };

    // System/developer message.
    for message in messages.iter_mut() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "system" || role == "developer" {
            add_cache_control_to_content(message, &cc);
            break;
        }
    }
    // Last conversation message.
    for message in messages.iter_mut().rev() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if (role == "user" || role == "assistant" || role == "tool")
            && add_cache_control_to_content(message, &cc)
        {
            break;
        }
    }
    // Last tool.
    if let Some(tools) = params.get_mut("tools").and_then(Value::as_array_mut)
        && let Some(last_tool) = tools.last_mut()
    {
        last_tool["cache_control"] = cc;
    }
}

fn add_cache_control_to_content(message: &mut Value, cc: &Value) -> bool {
    match message.get_mut("content") {
        Some(Value::String(text)) => {
            if text.is_empty() {
                return false;
            }
            let text = text.clone();
            message["content"] = json!([{ "type": "text", "text": text, "cache_control": cc }]);
            true
        }
        Some(Value::Array(parts)) => {
            for part in parts.iter_mut().rev() {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part["cache_control"] = cc.clone();
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::super::compat::{REASONING_FIELDS, get_compat};
    use super::super::generic_model;
    use super::*;
    use crate::types::ThinkingLevel;

    fn deepseek_model(thinking_level_map: Option<BTreeMap<String, Option<String>>>) -> Model {
        Model {
            id: "deepseek-reasoner".into(),
            name: "deepseek-reasoner".into(),
            api: "openai-completions".into(),
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com".into(),
            reasoning: true,
            thinking_level_map,
            input: vec![],
            cost: Default::default(),
            context_window: 64000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn thinking_param(
        map: Option<BTreeMap<String, Option<String>>>,
        reasoning: Option<ThinkingLevel>,
    ) -> Option<Value> {
        let model = deepseek_model(map);
        let compat = get_compat(&model);
        let options = StreamOptions {
            reasoning,
            ..Default::default()
        };
        let params = build_params(
            &model,
            &Context::default(),
            &options,
            &compat,
            CacheRetention::None,
        );
        params.get("thinking").cloned()
    }

    /// Port of TS `model.thinkingLevelMap?.off !== null`: with reasoning off,
    /// `thinking: disabled` must be sent unless the map explicitly sets
    /// `off` to null. A missing map (or missing `off` key) counts as
    /// "not null". Previously the condition was `key "off" exists`, so the
    /// common no-map case never disabled thinking.
    #[test]
    fn normalize_tool_call_id_truncates_on_char_boundary() {
        // 41 multi-byte chars: `&id[..40]` would panic mid-char.
        let id: String = "é".repeat(41);
        let normalized = normalize_tool_call_id(&id, "openai");
        assert_eq!(normalized, "é".repeat(40));
        let ascii = "a".repeat(50);
        assert_eq!(normalize_tool_call_id(&ascii, "openai"), "a".repeat(40));
        // Non-openai providers keep the id as-is.
        assert_eq!(normalize_tool_call_id(&id, "anthropic"), id);
    }

    /// Moonshot/Kimi Chat Completions: Long retention writes the cache with
    /// `prompt_cache_options: {"mode":"implicit","ttl":"1h"}` — never OpenAI's
    /// `prompt_cache_key` / `prompt_cache_retention`, which Kimi does not
    /// implement.
    #[test]
    fn moonshot_long_retention_sends_prompt_cache_options() {
        let model = generic_model("moonshotai", "https://api.moonshot.ai/v1");
        let compat = get_compat(&model);
        assert!(compat.kimi_prompt_cache_options);
        assert!(!compat.supports_long_cache_retention);
        let options = StreamOptions {
            session_id: Some("session-1".to_string()),
            ..Default::default()
        };
        let params = build_params(
            &model,
            &Context::default(),
            &options,
            &compat,
            CacheRetention::Long,
        );
        assert_eq!(
            params["prompt_cache_options"],
            json!({ "mode": "implicit", "ttl": "1h" })
        );
        assert!(params.get("prompt_cache_key").is_none());
        assert!(params.get("prompt_cache_retention").is_none());
    }

    /// Short/None retention on Kimi: no marker at all — the server writes
    /// with a 5m TTL by default when prompt_cache_options is omitted.
    #[test]
    fn moonshot_short_retention_omits_prompt_cache_options() {
        let model = generic_model("moonshotai-cn", "https://api.moonshot.cn/v1");
        let compat = get_compat(&model);
        assert!(compat.kimi_prompt_cache_options);
        let options = StreamOptions {
            session_id: Some("session-1".to_string()),
            ..Default::default()
        };
        for retention in [CacheRetention::Short, CacheRetention::None] {
            let params = build_params(&model, &Context::default(), &options, &compat, retention);
            assert!(params.get("prompt_cache_options").is_none());
            assert!(params.get("prompt_cache_key").is_none());
            assert!(params.get("prompt_cache_retention").is_none());
        }
    }

    /// A catalog/user override can switch the Kimi contract off (e.g. a
    /// Moonshot-shaped proxy that only speaks plain OpenAI).
    #[test]
    fn kimi_prompt_cache_options_can_be_overridden_off() {
        let mut model = generic_model("moonshotai", "https://api.moonshot.ai/v1");
        model.compat = Some(json!({ "kimiPromptCacheOptions": false }));
        let compat = get_compat(&model);
        assert!(!compat.kimi_prompt_cache_options);
        let params = build_params(
            &model,
            &Context::default(),
            &StreamOptions::default(),
            &compat,
            CacheRetention::Long,
        );
        assert!(params.get("prompt_cache_options").is_none());
    }

    /// OpenAI first-party keeps its own contract (prompt_cache_key +
    /// 24h retention) — the Kimi branch must not leak into it.
    #[test]
    fn openai_long_retention_still_sends_prompt_cache_retention() {
        let model = generic_model("openai", "https://api.openai.com/v1");
        let compat = get_compat(&model);
        assert!(!compat.kimi_prompt_cache_options);
        let options = StreamOptions {
            session_id: Some("session-1".to_string()),
            ..Default::default()
        };
        let params = build_params(
            &model,
            &Context::default(),
            &options,
            &compat,
            CacheRetention::Long,
        );
        assert_eq!(params["prompt_cache_key"], json!("session-1"));
        assert_eq!(params["prompt_cache_retention"], json!("24h"));
        assert!(params.get("prompt_cache_options").is_none());
    }

    #[test]
    fn deepseek_sends_disabled_when_reasoning_off_and_no_map() {
        assert_eq!(
            thinking_param(None, None),
            Some(json!({ "type": "disabled" }))
        );
    }

    #[test]
    fn deepseek_sends_disabled_when_off_mapped_to_value() {
        let map = BTreeMap::from([("off".to_string(), Some("0".to_string()))]);
        assert_eq!(
            thinking_param(Some(map), None),
            Some(json!({ "type": "disabled" }))
        );
    }

    #[test]
    fn deepseek_omits_thinking_when_off_explicitly_null() {
        let map = BTreeMap::from([("off".to_string(), None)]);
        assert_eq!(thinking_param(Some(map), None), None);
    }

    #[test]
    fn deepseek_sends_enabled_when_reasoning_on() {
        assert_eq!(
            thinking_param(None, Some(ThinkingLevel::Low)),
            Some(json!({ "type": "enabled" }))
        );
    }

    /// OpenRouter reasoning-mandatory models carry `off: null` in their
    /// thinkingLevelMap (TS #8614): with no reasoning requested, the payload
    /// must NOT send `reasoning: { effort: "none" }`.
    #[test]
    fn openrouter_mandatory_reasoning_omits_effort_none() {
        let mut model = generic_model("openrouter", "https://openrouter.ai/api/v1");
        model.thinking_level_map = Some(BTreeMap::from([
            ("off".to_string(), None),
            ("low".to_string(), Some("low".to_string())),
            ("high".to_string(), Some("high".to_string())),
        ]));
        let compat = get_compat(&model);
        let params = build_params(
            &model,
            &Context::default(),
            &StreamOptions::default(),
            &compat,
            CacheRetention::None,
        );
        assert!(params.get("reasoning").is_none());

        // An explicitly requested supported effort is still sent.
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::Low),
            ..Default::default()
        };
        let params = build_params(
            &model,
            &Context::default(),
            &options,
            &compat,
            CacheRetention::None,
        );
        assert_eq!(params["reasoning"], json!({ "effort": "low" }));
    }

    /// Optional OpenRouter models keep the explicit disable.
    #[test]
    fn openrouter_optional_reasoning_sends_effort_none() {
        let model = generic_model("openrouter", "https://openrouter.ai/api/v1");
        let compat = get_compat(&model);
        let params = build_params(
            &model,
            &Context::default(),
            &StreamOptions::default(),
            &compat,
            CacheRetention::None,
        );
        assert_eq!(params["reasoning"], json!({ "effort": "none" }));
    }

    fn assistant_with_thinking_signature(signature: &str) -> crate::types::AssistantMessage {
        let model = generic_model("openrouter", "https://openrouter.ai/api/v1");
        let mut assistant = crate::types::AssistantMessage::pending(&model);
        assistant.stop_reason = crate::types::StopReason::Stop;
        assistant.content = vec![
            ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: Some(signature.to_string()),
                redacted: None,
            },
            ContentBlock::text("answer"),
        ];
        assistant
    }

    /// Assistant-level reasoning_details replay verbatim and in order (TS
    /// #8246/#8671): the thinking signature holding the JSON detail array is
    /// decoded back onto the outgoing assistant message.
    #[test]
    fn reasoning_details_replay_verbatim_and_in_order() {
        let details = json!([
            { "type": "reasoning.summary", "summary": "Looked up time.", "format": "openai-responses-v1", "index": 0 },
            { "type": "reasoning.encrypted", "id": "rs_1", "data": "encrypted", "format": "openai-responses-v1" },
        ]);
        let model = generic_model("openrouter", "https://openrouter.ai/api/v1");
        let compat = get_compat(&model);
        let context = Context {
            system_prompt: None,
            messages: vec![
                crate::types::Message::user("hi"),
                crate::types::Message::Assistant(assistant_with_thinking_signature(
                    &serde_json::to_string(&details).unwrap(),
                )),
            ],
            tools: vec![],
        };
        let params = convert_messages(&model, &context, &compat, &Default::default());
        let assistant = params
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message");
        assert_eq!(assistant["reasoning_details"], details);
        // No raw reasoning field may be set alongside reasoning_details.
        for field in REASONING_FIELDS {
            assert!(assistant.get(field).is_none(), "unexpected {field}");
        }
    }

    /// Legacy sessions stored one encrypted entry on the tool call's
    /// thoughtSignature; it must replay as reasoning_details too.
    #[test]
    fn legacy_tool_call_encrypted_detail_replays() {
        let detail = json!({ "type": "reasoning.encrypted", "id": "rs_1", "data": "encrypted" });
        let model = generic_model("openrouter", "https://openrouter.ai/api/v1");
        let compat = get_compat(&model);
        let mut assistant = crate::types::AssistantMessage::pending(&model);
        assistant.stop_reason = crate::types::StopReason::ToolUse;
        assistant.content = vec![
            ContentBlock::text("calling a tool"),
            ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: json!({ "path": "a.txt" }),
                thought_signature: Some(serde_json::to_string(&detail).unwrap()),
                namespace: None,
            },
        ];
        let context = Context {
            system_prompt: None,
            messages: vec![
                crate::types::Message::user("hi"),
                crate::types::Message::Assistant(assistant),
            ],
            tools: vec![],
        };
        let params = convert_messages(&model, &context, &compat, &Default::default());
        let assistant = params
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message");
        assert_eq!(assistant["reasoning_details"], json!([detail]));
    }

    /// Garbage signatures never become reasoning_details.
    #[test]
    fn invalid_reasoning_details_are_not_replayed() {
        let model = generic_model("openrouter", "https://openrouter.ai/api/v1");
        let compat = get_compat(&model);
        let context = Context {
            system_prompt: None,
            messages: vec![
                crate::types::Message::user("hi"),
                crate::types::Message::Assistant(assistant_with_thinking_signature("not json")),
            ],
            tools: vec![],
        };
        let params = convert_messages(&model, &context, &compat, &Default::default());
        let assistant = params
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message");
        assert!(assistant.get("reasoning_details").is_none());
    }
}
