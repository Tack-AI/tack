//! OAuth core tests: PKCE/URL building, device-code poller, credential math.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;
use tack_ai::oauth::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn query_of(url: &str) -> BTreeMap<String, String> {
    let url = reqwest::Url::parse(url).unwrap();
    url.query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn anthropic_browser_flow_url() {
    let client = reqwest::Client::new();
    let flow = oauth_flow("anthropic").unwrap();
    let browser = flow
        .start_browser(&client, &BTreeMap::new())
        .await
        .unwrap()
        .unwrap();
    let q = query_of(&browser.authorize_url);
    assert!(
        browser
            .authorize_url
            .starts_with("https://claude.ai/oauth/authorize")
    );
    assert_eq!(q["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(q["redirect_uri"], "http://localhost:53692/callback");
    assert_eq!(q["code_challenge_method"], "S256");
    // state == verifier (Anthropic quirk)
    assert_eq!(q["state"], browser.verifier);
    assert_eq!(challenge_s256(&browser.verifier), q["code_challenge"]);
    assert!(q["scope"].contains("user:inference"));
}

#[tokio::test]
async fn codex_browser_flow_url() {
    let client = reqwest::Client::new();
    let flow = oauth_flow("openai-codex").unwrap();
    let browser = flow
        .start_browser(&client, &BTreeMap::new())
        .await
        .unwrap()
        .unwrap();
    let q = query_of(&browser.authorize_url);
    assert!(
        browser
            .authorize_url
            .starts_with("https://auth.openai.com/oauth/authorize")
    );
    assert_eq!(q["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
    assert_eq!(q["redirect_uri"], "http://localhost:1455/auth/callback");
    assert_eq!(q["originator"], "pi");
    assert_eq!(q["codex_cli_simplified_flow"], "true");
    // state is 16 random bytes hex, independent from the verifier
    assert_eq!(q["state"].len(), 32);
    assert_ne!(q["state"], browser.verifier);
}

#[tokio::test]
async fn openrouter_flow_uses_bound_callback_port() {
    let client = reqwest::Client::new();
    let flow = oauth_flow("openrouter").unwrap();
    // No port → error.
    assert!(flow.start_browser(&client, &BTreeMap::new()).await.is_err());
    let options = BTreeMap::from([("callback_port".to_string(), "8123".to_string())]);
    let browser = flow
        .start_browser(&client, &options)
        .await
        .unwrap()
        .unwrap();
    let q = query_of(&browser.authorize_url);
    assert!(q["callback_url"].starts_with("http://127.0.0.1:8123/oauth/callback/"));
    assert_eq!(q["code_challenge_method"], "S256");
}

#[test]
fn codex_to_auth_carries_account_id_header() {
    let flow = oauth_flow("openai-codex").unwrap();
    let mut credential = OAuthCredential::new("tok".into(), "ref".into(), i64::MAX);
    credential
        .extras
        .insert("accountId".into(), json!("acct_123"));
    let auth = flow.to_auth(&credential);
    assert_eq!(auth.api_key.as_deref(), Some("tok"));
    assert_eq!(auth.headers["chatgpt-account-id"], "acct_123");
    assert_eq!(auth.headers["originator"], "pi");
}

#[test]
fn copilot_to_auth_derives_base_url_from_proxy_ep() {
    let flow = oauth_flow("github-copilot").unwrap();
    let credential = OAuthCredential::new(
        "tid=abc;proxy-ep=proxy.individual.githubcopilot.com;exp=1".into(),
        "gh".into(),
        i64::MAX,
    );
    let auth = flow.to_auth(&credential);
    assert_eq!(
        auth.base_url.as_deref(),
        Some("https://api.individual.githubcopilot.com")
    );

    // Enterprise fallback when the token has no proxy-ep.
    let mut enterprise = OAuthCredential::new("tok".into(), "gh".into(), i64::MAX);
    enterprise
        .extras
        .insert("enterpriseUrl".into(), json!("acme.ghe.com"));
    let auth = flow.to_auth(&enterprise);
    assert_eq!(
        auth.base_url.as_deref(),
        Some("https://copilot-api.acme.ghe.com")
    );
}

#[test]
fn kimi_to_auth_uses_bearer_header() {
    let flow = oauth_flow("kimi-coding").unwrap();
    let credential = OAuthCredential::new("tok".into(), "ref".into(), i64::MAX);
    let auth = flow.to_auth(&credential);
    assert_eq!(auth.api_key, None);
    assert_eq!(auth.headers["authorization"], "Bearer tok");
}

#[test]
fn jwt_claim_extraction() {
    // Synthetic unsigned JWT: header.payload.sig
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_x"}}).to_string(),
    );
    let token = format!("e30.{payload}.sig");
    let claim = tack_ai::oauth::jwt_claim(&token, "https://api.openai.com/auth").unwrap();
    assert_eq!(claim["chatgpt_account_id"], "acct_x");
}

#[test]
fn expiry_math() {
    let fresh = OAuthCredential::new(
        "a".into(),
        "r".into(),
        tack_ai::now_millis() as i64 + 60_000,
    );
    assert!(!fresh.expires_within(Duration::from_secs(30)));
    assert!(fresh.expires_within(Duration::from_secs(120)));
    let expired = OAuthCredential::new("a".into(), "r".into(), 1);
    assert!(expired.expires_within(Duration::ZERO));
}

/// Mock device-token endpoint: pending → slow_down → success. Asserts the
/// poller honors RFC 8628 transitions and returns the token body.
#[tokio::test]
async fn device_code_poller_pending_slowdown_success() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls2 = calls.clone();
    tokio::spawn(async move {
        let bodies = [
            "{\"error\":\"authorization_pending\"}",
            "{\"error\":\"slow_down\"}",
            "{\"access_token\":\"tok_dev\",\"refresh_token\":\"ref_dev\",\"expires_in\":3600}",
        ];
        for (i, body) in bodies.iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await.unwrap();
            calls2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let status = if i < 2 { "400 Bad Request" } else { "200 OK" };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let client = reqwest::Client::new();
    let params = BTreeMap::from([("client_id".to_string(), "test".to_string())]);
    let result = poll_device_code(
        &client,
        &format!("http://{addr}/token"),
        &params,
        Duration::from_millis(1).max(Duration::from_secs(1)), // min clamp is 1s
        std::time::Instant::now() + Duration::from_secs(30),
        device::PollStyle::default(),
    )
    .await
    .unwrap();
    assert_eq!(result["access_token"], "tok_dev");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn device_code_poller_access_denied() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = socket.read(&mut buf).await.unwrap();
        let body = "{\"error\":\"access_denied\"}";
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let client = reqwest::Client::new();
    let result = poll_device_code(
        &client,
        &format!("http://{addr}/token"),
        &BTreeMap::new(),
        Duration::from_secs(1),
        std::time::Instant::now() + Duration::from_secs(30),
        device::PollStyle::default(),
    )
    .await;
    assert!(matches!(result, Err(OAuthError::Denied)));
}

#[test]
fn registry_covers_all_seven_providers() {
    for id in [
        "anthropic",
        "openai-codex",
        "github-copilot",
        "openrouter",
        "kimi-coding",
        "xai",
        "radius",
    ] {
        assert!(oauth_flow(id).is_some(), "missing flow for {id}");
        assert_eq!(oauth_flow(id).unwrap().id(), id);
    }
    assert!(oauth_flow("openai").is_none());
}
