//! Local provider discovery tests: mock Ollama (`/api/tags`) and llama.cpp
//! (`/v1/models`) servers; asserts catalog discovery, silent timeouts, the
//! one-shot in-process cache, and catalog injection.
#![allow(clippy::unwrap_used)]
#![allow(clippy::await_holding_lock)]
#![allow(unsafe_code)]

use tack_ai::local_providers::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serve `body` for one request; returns the server host (scheme://addr).
async fn serve_once(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 65536];
        let _ = socket.read(&mut buf).await.unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    format!("http://{addr}")
}

/// Accept connections but never respond (drives the probe into its timeout).
async fn serve_hanging() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn ollama_tags_discovery() {
    let host = serve_once(
        r#"{"models":[
            {"name":"qwen3:32b","model":"qwen3:32b","details":{"parameter_size":"32.8B","family":"qwen3"}},
            {"name":"llama3.1:8b","model":"llama3.1:8b","details":{"parameter_size":"8.0B","family":"llama"}}
        ]}"#,
    )
    .await;
    let models = probe_ollama(&host).await.expect("server is up");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "llama3.1:8b");
    assert_eq!(models[0].name, "llama3.1:8b (8.0B)");
    assert_eq!(models[1].id, "qwen3:32b");
    for m in &models {
        assert_eq!(m.provider, "ollama");
        assert_eq!(m.api, "openai-completions");
        assert_eq!(m.context_window, 32_768);
    }
}

#[tokio::test]
async fn llama_cpp_models_discovery() {
    let host = serve_once(
        r#"{"object":"list","data":[
            {"id":"models/qwen3-32b.gguf","object":"model","created":1,"owned_by":"llamacpp"}
        ]}"#,
    )
    .await;
    let models = probe_llama_cpp(&host).await.expect("server is up");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "models/qwen3-32b.gguf");
    assert_eq!(models[0].provider, "llama.cpp");
}

/// A server that never answers must not hang the probe: it returns `None`
/// silently within roughly the 500ms timeout.
#[tokio::test]
async fn probe_timeout_is_silent_and_fast() {
    let host = serve_hanging().await;
    let start = std::time::Instant::now();
    assert!(probe_ollama(&host).await.is_none());
    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "probe took {:?}",
        start.elapsed()
    );
}

/// Nothing listening at all: connection refused → `None`, no panic, no log.
#[tokio::test]
async fn probe_connection_refused_is_silent() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let host = format!("http://{addr}");
    assert!(probe_ollama(&host).await.is_none());
    assert!(probe_llama_cpp(&host).await.is_none());
}

/// A 200 with a non-JSON body counts as "not an Ollama server".
#[tokio::test]
async fn probe_garbage_body_is_silent() {
    let host = serve_once("<html>404</html>").await;
    assert!(probe_ollama(&host).await.is_none());
}

/// End-to-end: refresh() probes the hosts from the env overrides, records
/// statuses, and injects the discovered models into the built-in catalog.
/// Runs once per process (refresh caches); keep it as a single test.
#[tokio::test]
async fn refresh_discovers_and_installs_catalogs() {
    let _guard = ENV_LOCK.lock().unwrap();
    let ollama_host =
        serve_once(r#"{"models":[{"name":"llama3.1:8b","details":{"parameter_size":"8.0B"}}]}"#)
            .await;
    let llama_host = serve_once(r#"{"data":[{"id":"b-model"}]}"#).await;
    unsafe {
        std::env::set_var("OLLAMA_HOST", &ollama_host);
        std::env::set_var("LLAMA_CPP_HOST", &llama_host);
        std::env::remove_var("TACK_OFFLINE");
    }
    refresh().await;
    unsafe {
        std::env::remove_var("OLLAMA_HOST");
        std::env::remove_var("LLAMA_CPP_HOST");
    }

    let ollama = status("ollama").expect("probed");
    assert!(ollama.running);
    assert_eq!(ollama.model_count, 1);
    assert_eq!(ollama.host, ollama_host);
    let llama = status("llama.cpp").expect("probed");
    assert!(llama.running);
    assert_eq!(llama.model_count, 1);

    // Catalog injection: builtin_models sees the discovered lists.
    let catalog = tack_ai::providers::builtin_models("ollama");
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].id, "llama3.1:8b");
    assert_eq!(catalog[0].base_url, format!("{ollama_host}/v1"));
    let catalog = tack_ai::providers::builtin_models("llama.cpp");
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].id, "b-model");

    // Second refresh is a cached no-op (servers are gone; statuses stay).
    refresh().await;
    assert!(status("ollama").unwrap().running);
}
