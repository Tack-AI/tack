//! Vertex ADC tests: credentials JSON parsing, authorized_user token mint,
//! service-account JWT-bearer mint (request shape asserted at a mock token
//! endpoint), and token caching.
#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use tack_ai::api::vertex_adc::{GoogleTokenSource, parse_credentials_json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[test]
fn parse_service_account_json() {
    let adc = parse_credentials_json(&json!({
        "type": "service_account",
        "client_email": "svc@proj.iam.gserviceaccount.com",
        "private_key": "-----BEGIN RSA PRIVATE KEY-----\n...\n",
        "project_id": "proj-1",
    }))
    .unwrap();
    assert_eq!(adc.project.as_deref(), Some("proj-1"));
    let GoogleTokenSource::ServiceAccount {
        client_email,
        token_uri,
        ..
    } = adc.source
    else {
        panic!("expected service account");
    };
    assert_eq!(client_email, "svc@proj.iam.gserviceaccount.com");
    assert_eq!(token_uri, "https://oauth2.googleapis.com/token");
}

#[test]
fn parse_authorized_user_json() {
    let adc = parse_credentials_json(&json!({
        "type": "authorized_user",
        "client_id": "id",
        "client_secret": "secret",
        "refresh_token": "refresh",
    }))
    .unwrap();
    assert!(matches!(
        adc.source,
        GoogleTokenSource::AuthorizedUser { .. }
    ));
}

#[test]
fn parse_rejects_unknown_type() {
    assert!(parse_credentials_json(&json!({ "type": "impersonated" })).is_err());
}

/// Serve one form POST; capture the body, reply with a token.
async fn serve_token_once(body: &'static str) -> (String, tokio::sync::oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        let mut sent_continue = false;
        let request = loop {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break String::from_utf8_lossy(&buf).to_string();
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(head_end) = text.find("\r\n\r\n") {
                let headers = &text[..head_end];
                let content_length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                // A client sending `Expect: 100-continue` holds the body
                // until we answer; without this both sides wait forever
                // (the wedge that stalled CI on loaded runners).
                if !sent_continue
                    && headers
                        .lines()
                        .any(|l| l.eq_ignore_ascii_case("expect: 100-continue"))
                    && text.len() - (head_end + 4) < content_length
                {
                    socket
                        .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                        .await
                        .unwrap();
                    sent_continue = true;
                    continue;
                }
                if text.len() - (head_end + 4) >= content_length {
                    break text;
                }
            }
        };
        let head_end = request.find("\r\n\r\n").unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = tx.send(request[head_end + 4..].to_string());
    });
    (format!("http://{addr}"), rx)
}

/// Every await that crosses the mock/client boundary gets a bound: a
/// wedge (however unlikely on a loaded CI runner) must fail the test
/// with a message in seconds, not stall the whole job until its
/// timeout-minutes kills it with no diagnostics.
const IO_BOUND: std::time::Duration = std::time::Duration::from_secs(30);

async fn access_token_bounded(
    source: &GoogleTokenSource,
    client: &reqwest::Client,
) -> (String, std::time::Instant) {
    tokio::time::timeout(IO_BOUND, source.access_token(client))
        .await
        .expect("token mint wedged (no response from mock server)")
        .unwrap()
}

async fn recv_body_bounded(rx: tokio::sync::oneshot::Receiver<String>) -> String {
    tokio::time::timeout(IO_BOUND, rx)
        .await
        .expect("mock server never saw the request body")
        .unwrap()
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key { Some(v.to_string()) } else { None }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn authorized_user_mint_posts_refresh_grant() {
    let (url, rx) = serve_token_once("{\"access_token\":\"ya29.test\",\"expires_in\":3600}").await;
    let source = GoogleTokenSource::AuthorizedUser {
        client_id: "cid".into(),
        client_secret: "csecret".into(),
        refresh_token: "r_token".into(),
        token_uri: url,
    };
    let (token, _) = access_token_bounded(&source, &reqwest::Client::new()).await;
    assert_eq!(token, "ya29.test");
    let body = recv_body_bounded(rx).await;
    assert_eq!(
        form_value(&body, "grant_type").as_deref(),
        Some("refresh_token")
    );
    assert_eq!(
        form_value(&body, "refresh_token").as_deref(),
        Some("r_token")
    );

    // Second call is served from the cache — the (dropped) mock would fail.
    let (token, _) = access_token_bounded(&source, &reqwest::Client::new()).await;
    assert_eq!(token, "ya29.test");
}

/// Static throwaway RSA-2048 fixture (generated once with
/// `openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048`, used by
/// no real credential). Runtime keygen made the mint tests' duration
/// probabilistic — debug CI runners could stall on it for minutes.
const TEST_SA_PRIVATE_KEY_PEM: &str = include_str!("fixtures/test_sa_private_key.pem");

#[tokio::test(flavor = "multi_thread")]
async fn service_account_mint_posts_jwt_bearer_grant() {
    let pem = TEST_SA_PRIVATE_KEY_PEM.to_string();

    let (url, rx) = serve_token_once("{\"access_token\":\"ya29.sa\",\"expires_in\":3600}").await;
    let source = GoogleTokenSource::ServiceAccount {
        client_email: "svc@proj.iam.gserviceaccount.com".into(),
        private_key: pem,
        token_uri: url.clone(),
    };
    let (token, _) = access_token_bounded(&source, &reqwest::Client::new()).await;
    assert_eq!(token, "ya29.sa");

    let body = recv_body_bounded(rx).await;
    assert_eq!(
        form_value(&body, "grant_type").as_deref(),
        Some("urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
    );
    // The assertion is a 3-segment RS256 JWT with the SA claims.
    let assertion = form_value(&body, "assertion").unwrap();
    let segments: Vec<&str> = assertion.split('.').collect();
    assert_eq!(segments.len(), 3);
    use base64::Engine;
    let decode = |s: &str| {
        // Form-encoding leaves base64url's `-`/`_` untouched; pad and decode.
        let padded = format!("{}{}", s, "=".repeat((4 - s.len() % 4) % 4));
        base64::engine::general_purpose::URL_SAFE
            .decode(padded)
            .unwrap()
    };
    let header: Value = serde_json::from_slice(&decode(segments[0])).unwrap();
    assert_eq!(header["alg"], "RS256");
    let claims: Value = serde_json::from_slice(&decode(segments[1])).unwrap();
    assert_eq!(claims["iss"], "svc@proj.iam.gserviceaccount.com");
    assert_eq!(
        claims["scope"],
        "https://www.googleapis.com/auth/cloud-platform"
    );
    assert_eq!(claims["aud"], Value::String(url));
}
