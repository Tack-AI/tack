//! Vertex ADC end-to-end: SA JSON in GOOGLE_APPLICATION_CREDENTIALS → mock
//! token endpoint (JWT-bearer) → mock aiplatform endpoint, asserting the
//! `authorization: Bearer` header and streamed events.
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use serde_json::json;
use tack_ai::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const GEMINI_SSE: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hello from vertex\"}]}}],\"modelVersion\":\"gemini-test\"}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":4,\"totalTokenCount\":14}}\n\n",
);

#[tokio::test(flavor = "multi_thread")]
async fn vertex_adc_bearer_flow() {
    // Throwaway SA key — static fixture, see vertex_adc_tests.rs.
    let pem = include_str!("fixtures/test_sa_private_key.pem").to_string();

    // One listener, two requests: POST /token then the generateContent POST.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let credentials = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        credentials.path(),
        json!({
            "type": "service_account",
            "client_email": "e2e@proj.iam.gserviceaccount.com",
            "private_key": pem,
            "project_id": "e2e-project",
            // Point the JWT-bearer grant at the mock server.
            "token_uri": format!("http://{addr}/token"),
        })
        .to_string(),
    )
    .unwrap();

    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // 1) token endpoint
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_all(&mut socket).await;
        assert!(
            request.starts_with("POST /token"),
            "unexpected: {}",
            &request[..80.min(request.len())]
        );
        let body = "{\"access_token\":\"ya29.e2e\",\"expires_in\":3600}";
        respond(&mut socket, "application/json", body).await;
        // 2) generateContent
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_all(&mut socket).await;
        let _ = tx.send(request);
        respond(&mut socket, "text/event-stream", GEMINI_SSE).await;
    });

    unsafe {
        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", credentials.path());
        std::env::remove_var("GOOGLE_CLOUD_API_KEY");
        std::env::remove_var("GOOGLE_CLOUD_PROJECT"); // comes from the SA JSON
        // ADC mode requires a location (TS `resolveLocation`), even though a
        // collection-scope custom base doesn't put it in the URL.
        std::env::set_var("GOOGLE_CLOUD_LOCATION", "us-central1");
    }

    let mut model = Model {
        id: "gemini-test".to_string(),
        name: "Gemini".to_string(),
        api: "google-vertex".to_string(),
        provider: "google-vertex".to_string(),
        // Custom base (not aiplatform.googleapis.com) → publisher path appended.
        base_url: format!("http://{addr}/v1"),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: ModelCost::default(),
        context_window: 1_000_000,
        max_tokens: 8192,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    model.base_url = format!("http://{addr}"); // no version segment → /v1 appended
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("hi")],
        tools: vec![],
    };

    let provider = provider_for(&model).unwrap();
    let message = provider
        .complete(&model, &context, StreamOptions::default())
        .await;
    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );
    assert_eq!(message.text(), "hello from vertex");

    let request = rx.await.unwrap();
    assert!(
        request.contains("authorization: Bearer ya29.e2e"),
        "request: {request}"
    );
    assert!(
        request.starts_with("POST /v1/publishers/google/models/gemini-test"),
        "request: {request}"
    );

    unsafe {
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        std::env::remove_var("GOOGLE_CLOUD_LOCATION");
    }
}

async fn read_all(socket: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
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
                return text;
            }
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

async fn respond(socket: &mut tokio::net::TcpStream, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
}
