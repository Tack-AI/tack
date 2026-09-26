#![allow(clippy::unwrap_used)]
//! Serde byte-compatibility tests: the JSON shapes must match what the
//! TypeScript pi writes into session files.

use serde_json::json;
use tack_ai::*;

#[test]
fn user_message_text_matches_ts_shape() {
    let msg = Message::user("hello");
    let value = serde_json::to_value(&msg).unwrap();
    assert_eq!(value["role"], json!("user"));
    assert_eq!(value["content"], json!("hello"));
    assert!(value["timestamp"].is_u64());

    let roundtrip: Message = serde_json::from_value(value).unwrap();
    assert_eq!(roundtrip, msg);
}

#[test]
fn assistant_message_matches_ts_shape() {
    let ts_json = json!({
        "role": "assistant",
        "content": [
            { "type": "thinking", "thinking": "hmm", "thinkingSignature": "sig123" },
            { "type": "text", "text": "answer" },
            { "type": "toolCall", "id": "toolu_1", "name": "read", "arguments": { "path": "a.rs" } }
        ],
        "api": "anthropic-messages",
        "provider": "anthropic",
        "model": "claude-sonnet-5",
        "usage": {
            "input": 10, "output": 5, "cacheRead": 2, "cacheWrite": 1,
            "totalTokens": 18,
            "cost": { "input": 0.1, "output": 0.2, "cacheRead": 0.01, "cacheWrite": 0.01, "total": 0.32 }
        },
        "stopReason": "toolUse",
        "timestamp": 1750000000000u64
    });

    let msg: Message = serde_json::from_value(ts_json.clone()).unwrap();
    let Message::Assistant(a) = &msg else {
        panic!("expected assistant")
    };
    assert_eq!(a.stop_reason, StopReason::ToolUse);
    assert_eq!(a.usage.total_tokens, 18);
    assert!(a.has_tool_calls());

    // Re-serialization preserves the exact shape (key sets, discriminants).
    let out = serde_json::to_value(&msg).unwrap();
    assert_eq!(out, ts_json);
}

#[test]
fn tool_result_message_matches_ts_shape() {
    let ts_json = json!({
        "role": "toolResult",
        "toolCallId": "toolu_1",
        "toolName": "read",
        "content": [{ "type": "text", "text": "file contents" }],
        "isError": false,
        "timestamp": 1750000000000u64
    });
    let msg: Message = serde_json::from_value(ts_json.clone()).unwrap();
    let out = serde_json::to_value(&msg).unwrap();
    assert_eq!(out, ts_json);
}

#[test]
fn model_matches_ts_shape() {
    let ts_json = json!({
        "id": "claude-sonnet-5",
        "name": "Claude Sonnet 5",
        "api": "anthropic-messages",
        "provider": "anthropic",
        "baseUrl": "https://api.anthropic.com",
        "reasoning": true,
        "input": ["text", "image"],
        "cost": { "input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75 },
        "contextWindow": 200000,
        "maxTokens": 64000
    });
    let model: Model = serde_json::from_value(ts_json.clone()).unwrap();
    assert!(model.reasoning);
    assert!(model.supports_images());
    let out = serde_json::to_value(&model).unwrap();
    assert_eq!(out, ts_json);
}

#[test]
fn unknown_assistant_fields_are_tolerated() {
    // TS writes extra optional fields (responseId, endTurn, ...); we must
    // parse them and round-trip the ones we model.
    let ts_json = json!({
        "role": "assistant",
        "content": [],
        "api": "openai-completions",
        "provider": "openai",
        "model": "gpt-5.2",
        "responseId": "chatcmpl-123",
        "usage": {
            "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
            "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 }
        },
        "stopReason": "stop",
        "rawStopReason": "stop",
        "timestamp": 1750000000000u64
    });
    let msg: Message = serde_json::from_value(ts_json.clone()).unwrap();
    let out = serde_json::to_value(&msg).unwrap();
    assert_eq!(out, ts_json);
}
