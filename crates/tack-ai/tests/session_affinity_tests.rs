//! Request-header regressions for provider session affinity and OpenCode routing.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use tack_ai::{CacheRetention, Context, Model, StreamOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn capture_headers(mut model: Model, mut options: StreamOptions) -> BTreeMap<String, String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 65536];
        let read = socket.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..read]);
        let headers = request
            .lines()
            .skip(1)
            .take_while(|line| !line.is_empty() && *line != "\r")
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect::<BTreeMap<_, _>>();
        socket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        headers
    });

    model.base_url = format!("http://{addr}");
    options.api_key = Some("test-key".into());
    let provider = tack_ai::provider_for(&model).unwrap();
    let _ = provider
        .complete(&model, &Context::default(), options)
        .await;
    server.await.unwrap()
}

fn options(cache_retention: CacheRetention) -> StreamOptions {
    StreamOptions {
        session_id: Some("session-1".into()),
        cache_retention: Some(cache_retention),
        ..Default::default()
    }
}

#[tokio::test]
async fn openrouter_defaults_to_x_session_id_and_cache_none_opts_out() {
    let model = tack_ai::providers::builtin_models("openrouter")[0].clone();
    let headers = capture_headers(model.clone(), options(CacheRetention::Short)).await;
    assert_eq!(
        headers.get("x-session-id").map(String::as_str),
        Some("session-1")
    );
    assert!(!headers.contains_key("x-session-affinity"));
    assert!(!headers.contains_key("session_id"));

    let headers = capture_headers(model, options(CacheRetention::None)).await;
    assert!(!headers.contains_key("x-session-id"));
    assert!(!headers.contains_key("x-client-request-id"));
    assert!(!headers.contains_key("x-session-affinity"));
}

#[tokio::test]
async fn baseten_catalog_models_send_openai_affinity_headers() {
    let model = tack_ai::providers::builtin_models("baseten")[0].clone();
    assert_eq!(
        model.compat.as_ref().unwrap()["sendSessionAffinityHeaders"],
        true
    );
    let headers = capture_headers(model, options(CacheRetention::Short)).await;
    for name in ["session_id", "x-client-request-id", "x-session-affinity"] {
        assert_eq!(
            headers.get(name).map(String::as_str),
            Some("session-1"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn opencode_routes_session_even_when_cache_is_disabled() {
    let model = tack_ai::providers::builtin_models("opencode")[0].clone();
    let headers = capture_headers(model, options(CacheRetention::None)).await;
    assert_eq!(
        headers.get("x-opencode-session").map(String::as_str),
        Some("session-1")
    );
}
