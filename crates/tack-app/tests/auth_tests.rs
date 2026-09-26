//! OAuth resolve/refresh path tests (tack-app side): expired credential →
//! locked refresh → persisted + returned; concurrent resolves share one
//! refresh HTTP call.
#![allow(clippy::unwrap_used)]
#![allow(clippy::await_holding_lock)]
#![allow(unsafe_code)]

use std::sync::Mutex;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Env vars are process-global; serialize these tests.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn write_expired_kimi_credential(agent_dir: &std::path::Path) {
    std::fs::create_dir_all(agent_dir).unwrap();
    // Keep these tests hermetic: pin file storage so the refresh lands in
    // auth.json, not the real OS keyring.
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"credentialStore": "file"}"#,
    )
    .unwrap();
    std::fs::write(
        agent_dir.join("auth.json"),
        json!({ "kimi-coding": { "type": "oauth", "access": "old-token", "refresh": "old-refresh", "expires": 1 } })
            .to_string(),
    )
    .unwrap();
}

/// Serve N token-endpoint responses, counting requests.
async fn serve_token_endpoint(
    bodies: Vec<&'static str>,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count2 = count.clone();
    tokio::spawn(async move {
        for body in bodies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await.unwrap();
            count2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{addr}"), count)
}

#[tokio::test]
async fn expired_oauth_credential_is_refreshed_and_persisted() {
    let _guard = ENV_LOCK.lock().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();
    write_expired_kimi_credential(agent_dir.path());
    let (host, count) = serve_token_endpoint(vec![
        "{\"access_token\":\"new-token\",\"refresh_token\":\"new-refresh\",\"expires_in\":3600}",
    ])
    .await;
    unsafe { std::env::set_var("KIMI_CODE_OAUTH_HOST", &host) };

    let auth = tack_app::model::resolve_auth("kimi-coding", None, agent_dir.path());
    let resolved = auth.resolve().await.unwrap();
    // kimi maps to a Bearer header, not an api_key.
    assert_eq!(resolved.headers["authorization"], "Bearer new-token");
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Persisted: the stored credential now holds the fresh token.
    let stored = tack_app::auth::get_oauth(agent_dir.path(), "kimi-coding").unwrap();
    assert_eq!(stored.access, "new-token");
    assert_eq!(stored.refresh, "new-refresh");
    assert!(stored.expires > 1);

    // A second resolve uses the fresh credential — no more HTTP.
    let resolved = auth.resolve().await.unwrap();
    assert_eq!(resolved.headers["authorization"], "Bearer new-token");
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

    unsafe { std::env::remove_var("KIMI_CODE_OAUTH_HOST") };
}

#[tokio::test]
async fn concurrent_resolves_share_one_refresh() {
    let _guard = ENV_LOCK.lock().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();
    write_expired_kimi_credential(agent_dir.path());
    let (host, count) = serve_token_endpoint(vec![
        "{\"access_token\":\"shared-token\",\"expires_in\":3600}",
    ])
    .await;
    unsafe { std::env::set_var("KIMI_CODE_OAUTH_HOST", &host) };

    let auth = tack_app::model::resolve_auth("kimi-coding", None, agent_dir.path());
    let mut joins = Vec::new();
    for _ in 0..5 {
        let auth = auth.clone();
        joins.push(tokio::spawn(async move { auth.resolve().await.unwrap() }));
    }
    for join in joins {
        let resolved = join.await.unwrap();
        assert_eq!(resolved.headers["authorization"], "Bearer shared-token");
    }
    // Double-checked lock: exactly one refresh happened.
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

    unsafe { std::env::remove_var("KIMI_CODE_OAUTH_HOST") };
}

#[tokio::test]
async fn static_auth_passthrough_and_api_key_login() {
    let agent_dir = tempfile::tempdir().unwrap();
    // Pin file storage: otherwise CredentialStore::Auto writes the login to
    // the REAL OS keyring (service "tack"), which both pollutes the user's
    // keychain and makes the test flaky (macOS may prompt/deny access for a
    // test binary, changing behavior between runs).
    std::fs::write(
        agent_dir.path().join("settings.json"),
        r#"{"credentialStore": "file"}"#,
    )
    .unwrap();
    // Explicit key wins over everything.
    let auth =
        tack_app::model::resolve_auth("anthropic", Some("sk-explicit".into()), agent_dir.path());
    assert_eq!(
        auth.resolve().await.unwrap().api_key.as_deref(),
        Some("sk-explicit")
    );

    // api_key entry in auth.json.
    tack_app::auth::login(agent_dir.path(), "mistral", "ms-key").unwrap();
    let auth = tack_app::model::resolve_auth("mistral", None, agent_dir.path());
    assert_eq!(
        auth.resolve().await.unwrap().api_key.as_deref(),
        Some("ms-key")
    );
}
