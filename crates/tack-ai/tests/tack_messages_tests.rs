//! SSE replay tests for the tack-messages adapter (the upstream pi-messages
//! / Radius gateway protocol).
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use serde_json::json;
use tack_ai::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Read one full HTTP request from a socket; returns (request_line, body).
async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, String) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let request = String::from_utf8_lossy(&buf).to_string();
        if let Some(head_end) = request.find("\r\n\r\n") {
            let headers = &request[..head_end];
            let content_length = headers
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() - (head_end + 4) >= content_length {
                let head_end = request.find("\r\n\r\n").unwrap();
                let path = request.lines().next().unwrap_or("").to_string();
                return (path, request[head_end + 4..].to_string());
            }
        }
    }
    (String::new(), String::new())
}

async fn respond(socket: &mut tokio::net::TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
}

/// Serve one HTTP response; returns (base_url, request receiver).
async fn serve_once(
    status: &'static str,
    content_type: &'static str,
    body: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        respond(&mut socket, status, content_type, body).await;
        let _ = tx.send(request);
    });
    (format!("http://{addr}"), rx)
}

fn make_model(base_url: &str) -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test".to_string(),
        api: "tack-messages".to_string(),
        provider: "radius".to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: ModelCost {
            input: 2.0,
            output: 8.0,
            cache_read: 0.5,
            cache_write: 2.0,
            tiers: None,
        },
        context_window: 128_000,
        max_tokens: 4096,
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
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            defer_loading: false,
            constrained_sampling: None,
        }],
    }
}

const TACK_MESSAGES_SSE: &str = concat!(
    "data: {\"type\":\"start\"}\n\n",
    "data: {\"type\":\"text_start\",\"contentIndex\":0}\n\n",
    "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"Hello\"}\n\n",
    "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\" world\"}\n\n",
    "data: {\"type\":\"text_end\",\"contentIndex\":0,\"content\":\"Hello world\"}\n\n",
    "data: {\"type\":\"toolcall_start\",\"contentIndex\":1,\"id\":\"call_1\",\"toolName\":\"read\"}\n\n",
    "data: {\"type\":\"toolcall_delta\",\"contentIndex\":1,\"delta\":\"{\\\"path\\\":\"}\n\n",
    "data: {\"type\":\"toolcall_delta\",\"contentIndex\":1,\"delta\":\"\\\"a.rs\\\"}\"}\n\n",
    "data: {\"type\":\"toolcall_end\",\"contentIndex\":1,\"toolCall\":{\"id\":\"call_1\",\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}}\n\n",
    "data: {\"type\":\"done\",\"reason\":\"toolUse\",\"usage\":{\"input\":30,\"output\":12,\"cacheRead\":8,\"cacheWrite\":0,\"totalTokens\":42,\"cost\":{\"input\":0.1,\"output\":0.2,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":0.3}},\"responseId\":\"resp_1\"}\n\n",
);

#[tokio::test]
async fn tack_messages_sse_replay() {
    let (base_url, request_rx) = serve_once("200 OK", "text/event-stream", TACK_MESSAGES_SSE).await;
    let model = make_model(&base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(
        &model,
        &test_context(),
        StreamOptions {
            api_key: Some("test-key".to_string()),
            ..Default::default()
        },
    );
    let mut text = String::new();
    let mut saw_tool_call_end = false;
    let message = loop {
        let Some(event) = stream.next().await else {
            panic!("stream ended without done")
        };
        match &event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(delta),
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                saw_tool_call_end = true;
                let ContentBlock::ToolCall {
                    name, arguments, ..
                } = tool_call
                else {
                    panic!("expected tool call block");
                };
                assert_eq!(name, "read");
                assert_eq!(arguments, &json!({ "path": "a.rs" }));
            }
            AssistantMessageEvent::Done { message, .. } => break message.clone(),
            _ => {}
        }
    };

    assert_eq!(text, "Hello world");
    assert!(saw_tool_call_end);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage.input, 30);
    assert_eq!(message.usage.cache_read, 8);
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));

    // Request shape: pi Context verbatim + Bearer auth.
    let (path, body) = request_rx.await.unwrap();
    assert!(path.starts_with("POST /messages"));
    assert!(!path.contains("test-key"));
    let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(payload["model"], "test-model");
    assert_eq!(payload["context"]["systemPrompt"], "You are a test bot.");
    assert_eq!(payload["context"]["messages"][0]["role"], "user");
    assert_eq!(payload["context"]["tools"][0]["name"], "read");
}

#[tokio::test]
async fn tack_messages_error_body_formatting() {
    let (base_url, _) = serve_once(
        "400 Bad Request",
        "application/json",
        "{\"error\":{\"message\":\"slow down\",\"code\":\"rate_limited\"}}",
    )
    .await;
    let model = make_model(&base_url);
    let provider = provider_for(&model).unwrap();

    let message = provider
        .complete(
            &model,
            &test_context(),
            StreamOptions {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.unwrap();
    assert!(error.contains("slow down"), "unexpected error: {error}");
    assert!(
        error.contains("(rate_limited)"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn tack_messages_radius_gateway_discovery() {
    // Empty base_url on the model → discover via {gateway}/v1/config.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gateway_url = format!("http://{addr}");
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // 1) GET /v1/config → { baseUrl: <self> }
        let (mut socket, _) = listener.accept().await.unwrap();
        let (path, _) = read_request(&mut socket).await;
        assert!(
            path.starts_with("GET /v1/config"),
            "unexpected request: {path}"
        );
        let config = format!("{{\"baseUrl\":\"http://{addr}\",\"models\":[]}}");
        respond(&mut socket, "200 OK", "application/json", &config).await;
        // 2) POST /messages → SSE
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        respond(
            &mut socket,
            "200 OK",
            "text/event-stream",
            TACK_MESSAGES_SSE,
        )
        .await;
        let _ = tx.send(request);
    });

    unsafe { std::env::set_var("RADIUS_GATEWAY_URL", &gateway_url) };
    let model = make_model("");
    let provider = provider_for(&model).unwrap();
    let message = provider
        .complete(
            &model,
            &test_context(),
            StreamOptions {
                api_key: Some("test-key".to_string()),
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
    let (path, _) = rx.await.unwrap();
    assert!(
        path.starts_with("POST /messages"),
        "unexpected request: {path}"
    );
}

/// SSE that starts a text block but never reaches a terminal event.
const TACK_MESSAGES_PARTIAL_SSE: &str = concat!(
    "data: {\"type\":\"start\"}\n\n",
    "data: {\"type\":\"text_start\",\"contentIndex\":0}\n\n",
    "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"Hel\"}\n\n",
);

/// Regression: a mid-stream transport failure must fail with the
/// ACCUMULATED partial message (text streamed so far), not the pristine
/// pending message created at the top of `run()`.
#[tokio::test]
async fn tack_messages_midstream_transport_error_keeps_partial_content() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket).await;
        // Lie about the body length so reqwest's body read fails mid-stream
        // ("error decoding response body") after the delta was delivered.
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{TACK_MESSAGES_PARTIAL_SSE}",
            TACK_MESSAGES_PARTIAL_SSE.len() + 1024
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let model = make_model(&format!("http://{addr}"));
    let provider = provider_for(&model).unwrap();

    let message = provider
        .complete(
            &model,
            &test_context(),
            StreamOptions {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    let Some(ContentBlock::Text { text, .. }) = message.content.first() else {
        panic!("partial text block must survive the failure: {message:?}");
    };
    assert_eq!(text, "Hel");
}

/// Same for a cleanly-ended SSE stream that never sends a terminal event.
#[tokio::test]
async fn tack_messages_missing_terminal_event_keeps_partial_content() {
    let (base_url, _) = serve_once("200 OK", "text/event-stream", TACK_MESSAGES_PARTIAL_SSE).await;
    let model = make_model(&base_url);
    let provider = provider_for(&model).unwrap();

    let message = provider
        .complete(
            &model,
            &test_context(),
            StreamOptions {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("terminal event"),
        "unexpected error: {:?}",
        message.error_message
    );
    let Some(ContentBlock::Text { text, .. }) = message.content.first() else {
        panic!("partial text block must survive the failure: {message:?}");
    };
    assert_eq!(text, "Hel");
}
