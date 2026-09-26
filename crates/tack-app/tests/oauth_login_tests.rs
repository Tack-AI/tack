//! OAuth login glue tests: paste parsing, callback request parsing, and the
//! one-shot loopback server end to end.
#![allow(clippy::unwrap_used)]

use tack_app::oauth_login::{await_callback, parse_callback_request, parse_manual_input};

#[test]
fn manual_input_variants() {
    // Bare code.
    assert_eq!(
        parse_manual_input("abc123"),
        Some(("abc123".to_string(), None))
    );
    // code#state.
    assert_eq!(
        parse_manual_input("abc#xyz"),
        Some(("abc".to_string(), Some("xyz".to_string())))
    );
    // Full URL.
    assert_eq!(
        parse_manual_input("http://localhost:53692/callback?code=abc&state=st1"),
        Some(("abc".to_string(), Some("st1".to_string())))
    );
    // Query string only.
    assert_eq!(
        parse_manual_input("code=abc&state=st1"),
        Some(("abc".to_string(), Some("st1".to_string())))
    );
    // Empty.
    assert_eq!(parse_manual_input("  "), None);
}

#[test]
fn callback_request_parsing() {
    let code = parse_callback_request(
        "GET /callback?code=abc&state=st1 HTTP/1.1",
        "/callback",
        "st1",
    )
    .unwrap();
    assert_eq!(code, "abc");
    // State mismatch rejected.
    assert!(
        parse_callback_request(
            "GET /callback?code=abc&state=wrong HTTP/1.1",
            "/callback",
            "st1"
        )
        .is_err()
    );
    // Percent-decoding.
    let code = parse_callback_request("GET /cb?code=a%20b%2Bc HTTP/1.1", "/cb", "").unwrap();
    assert_eq!(code, "a b+c");
    // Wrong path.
    assert!(parse_callback_request("GET /nope?code=abc HTTP/1.1", "/callback", "").is_err());
}

#[tokio::test]
async fn loopback_callback_round_trip() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(await_callback(listener, "/callback", "state-1"));
    let response = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/callback?code=code-42&state=state-1"
        ))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body = response.text().await.unwrap();
    assert!(body.contains("Login successful"));
    assert_eq!(server.await.unwrap().unwrap(), "code-42");
}
