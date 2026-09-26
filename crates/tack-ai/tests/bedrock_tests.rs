//! End-to-end Bedrock adapter test: mock server replies with AWS
//! event-stream binary frames; asserts the emitted pi event sequence, and
//! both auth modes (SigV4 env creds, Bearer api key).
#![allow(clippy::unwrap_used)]
#![allow(clippy::await_holding_lock)]
#![allow(unsafe_code)]

use serde_json::json;
use tack_ai::api::bedrock::eventstream::build_frame;
use tack_ai::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn event_frame(event_type: &str, payload: &str) -> Vec<u8> {
    build_frame(
        &[
            (":message-type", "event"),
            (":event-type", event_type),
            (":content-type", "application/json"),
        ],
        payload.as_bytes(),
    )
}

fn exception_frame(code: &str, message: &str) -> Vec<u8> {
    build_frame(
        &[
            (":message-type", "exception"),
            (":error-code", code),
            (":error-message", message),
        ],
        format!("{{\"message\":\"{message}\"}}").as_bytes(),
    )
}

fn converse_stream_body() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(event_frame("messageStart", r#"{"role":"assistant"}"#));
    body.extend(event_frame(
        "contentBlockDelta",
        r#"{"delta":{"text":"Hel"},"contentBlockIndex":0}"#,
    ));
    body.extend(event_frame(
        "contentBlockDelta",
        r#"{"delta":{"text":"lo"},"contentBlockIndex":0}"#,
    ));
    body.extend(event_frame(
        "contentBlockStop",
        r#"{"contentBlockIndex":0}"#,
    ));
    body.extend(event_frame(
        "contentBlockStart",
        r#"{"start":{"toolUse":{"toolUseId":"tu_1","name":"read"}},"contentBlockIndex":1}"#,
    ));
    body.extend(event_frame(
        "contentBlockDelta",
        r#"{"delta":{"toolUse":{"input":"{\"path\":"}},"contentBlockIndex":1}"#,
    ));
    body.extend(event_frame(
        "contentBlockDelta",
        r#"{"delta":{"toolUse":{"input":"\"a.rs\"}"}},"contentBlockIndex":1}"#,
    ));
    body.extend(event_frame(
        "contentBlockStop",
        r#"{"contentBlockIndex":1}"#,
    ));
    body.extend(event_frame("messageStop", r#"{"stopReason":"tool_use"}"#));
    body.extend(event_frame(
        "metadata",
        r#"{"usage":{"inputTokens":30,"outputTokens":12,"totalTokens":42,"cacheReadInputTokens":8,"cacheWriteInputTokens":0},"metrics":{"latencyMs":100}}"#,
    ));
    body
}

/// Serve one request; reply with the given binary body.
async fn serve_frames(body: Vec<u8>) -> (String, tokio::sync::oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        let request = loop {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break String::from_utf8_lossy(&buf).to_string();
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(head_end) = text.find("\r\n\r\n") {
                let content_length = text[..head_end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if text.len() - (head_end + 4) >= content_length {
                    break text;
                }
            }
        };
        let mut response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.amazon.eventstream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        socket.write_all(&response).await.unwrap();
        let _ = tx.send(request);
    });
    (format!("http://{addr}"), rx)
}

fn make_model(base_url: &str) -> Model {
    Model {
        id: "us.anthropic.claude-sonnet-4-5".to_string(),
        name: "Claude".to_string(),
        api: "bedrock-converse-stream".to_string(),
        provider: "amazon-bedrock".to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            tiers: None,
        },
        context_window: 200_000,
        max_tokens: 8192,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn test_context() -> Context {
    Context {
        system_prompt: Some("You are a test bot.".to_string()),
        messages: vec![Message::user("hi")],
        tools: vec![ToolDefinition {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            defer_loading: false,
            constrained_sampling: None,
        }],
    }
}

#[tokio::test]
async fn bedrock_sigv4_event_stream_replay() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKTEST");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "SKTEST");
        std::env::remove_var("AWS_BEARER_TOKEN_BEDROCK");
    }
    let (base_url, rx) = serve_frames(converse_stream_body()).await;
    let model = make_model(&base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), StreamOptions::default());
    let mut text = String::new();
    let mut tool_call_end = None;
    let message = loop {
        let Some(event) = stream.next().await else {
            panic!("no terminal event")
        };
        match event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => tool_call_end = Some(tool_call),
            AssistantMessageEvent::Done { message, .. } => break message,
            AssistantMessageEvent::Error { error, .. } => {
                panic!("stream error: {:?}", error.error_message)
            }
            _ => {}
        }
    };

    assert_eq!(text, "Hello");
    let Some(ContentBlock::ToolCall {
        id,
        name,
        arguments,
        ..
    }) = tool_call_end
    else {
        panic!("expected tool call");
    };
    assert_eq!(id, "tu_1");
    assert_eq!(name, "read");
    assert_eq!(arguments, json!({ "path": "a.rs" }));
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage.input, 30);
    assert_eq!(message.usage.cache_read, 8);
    assert!(message.usage.cost.total > 0.0);

    // Request is SigV4-signed with the env credentials.
    let request = rx.await.unwrap();
    assert!(
        request.starts_with("POST /model/us.anthropic.claude-sonnet-4-5/converse-stream"),
        "request: {request}"
    );
    let auth_line = request
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
        .expect("authorization header");
    assert!(
        auth_line.contains("AWS4-HMAC-SHA256 Credential=AKTEST/"),
        "{auth_line}"
    );
    assert!(
        auth_line.contains("/us-east-1/bedrock/aws4_request"),
        "{auth_line}"
    ); // us. prefix → us-east-1
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-amz-content-sha256:")
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("accept: application/vnd.amazon.eventstream")
    );

    unsafe {
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    }
}

#[tokio::test]
async fn bedrock_bearer_auth_skips_sigv4() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        std::env::remove_var("AWS_PROFILE");
    }
    let (base_url, rx) = serve_frames(converse_stream_body()).await;
    let model = make_model(&base_url);
    let provider = provider_for(&model).unwrap();

    let message = provider
        .complete(
            &model,
            &test_context(),
            StreamOptions {
                api_key: Some("bearer-token-1".into()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(
        message.stop_reason,
        StopReason::ToolUse,
        "{:?}",
        message.error_message
    );

    let request = rx.await.unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer bearer-token-1"),
        "{request}"
    );
    assert!(
        !request.to_ascii_lowercase().contains("x-amz-date"),
        "{request}"
    );
}

#[tokio::test]
async fn bedrock_exception_frame_maps_to_stable_prefix() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", "bearer-1") };
    let mut body = Vec::new();
    body.extend(event_frame("messageStart", r#"{"role":"assistant"}"#));
    body.extend(exception_frame("throttlingException", "Too many requests"));
    let (base_url, _) = serve_frames(body).await;
    let model = make_model(&base_url);
    let provider = provider_for(&model).unwrap();

    let message = provider
        .complete(&model, &test_context(), StreamOptions::default())
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.unwrap();
    assert!(error.contains("Throttling error"), "{error}");
    assert!(error.contains("Too many requests"), "{error}");

    unsafe { std::env::remove_var("AWS_BEARER_TOKEN_BEDROCK") };
}

/// TS #9457: one-hour cache writes are parsed from `cacheDetails` and priced
/// at 2x the input rate (short writes keep the cache_write rate).
#[tokio::test]
async fn bedrock_one_hour_cache_writes_are_priced() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKTEST");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "SKTEST");
        std::env::remove_var("AWS_BEARER_TOKEN_BEDROCK");
    }
    let mut body = Vec::new();
    body.extend(event_frame("messageStart", r#"{"role":"assistant"}"#));
    body.extend(event_frame(
        "contentBlockDelta",
        r#"{"delta":{"text":"hi"},"contentBlockIndex":0}"#,
    ));
    body.extend(event_frame(
        "contentBlockStop",
        r#"{"contentBlockIndex":0}"#,
    ));
    body.extend(event_frame("messageStop", r#"{"stopReason":"end_turn"}"#));
    body.extend(event_frame(
        "metadata",
        r#"{"usage":{"inputTokens":10,"outputTokens":2,"totalTokens":1512,"cacheReadInputTokens":0,"cacheWriteInputTokens":1500,"cacheDetails":[{"ttl":"1h","inputTokens":1000},{"ttl":"5m","inputTokens":500}]},"metrics":{"latencyMs":10}}"#,
    ));
    let (base_url, _rx) = serve_frames(body).await;
    let model = make_model(&base_url);
    let provider = provider_for(&model).unwrap();
    let message = provider
        .stream(&model, &test_context(), StreamOptions::default())
        .result()
        .await;

    assert_eq!(message.usage.cache_write, 1500);
    assert_eq!(message.usage.cache_write_1h, Some(1000));
    // cache_write cost = (3.75 * 500 short + 3.0 * 2 * 1000 long) / 1e6.
    let expected = (3.75 * 500.0 + 6.0 * 1000.0) / 1_000_000.0;
    assert!(
        (message.usage.cost.cache_write - expected).abs() < 1e-12,
        "cache_write cost {} != {expected}",
        message.usage.cost.cache_write
    );
}
