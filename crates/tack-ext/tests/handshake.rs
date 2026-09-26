//! Handshake integration tests: initialize → register exchange over the
//! NDJSON transport, covering the success path and the defined failure
//! modes (plugin exits immediately, protocol version mismatch, malformed
//! JSON, malformed register payload, no register at all).
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use tack_ext::{Envelope, RegisterPayload};

/// Happy path: initialize carries the documented fields; the plugin's
/// register event is parsed into the host's RegisterPayload.
#[tokio::test]
async fn handshake_exchanges_capabilities() {
    let (peer, side, _services) = connect();
    let driver = tokio::spawn(run_versioned_handshake(side, tack_ext::PROTOCOL_VERSION));

    let register: RegisterPayload = peer.initialize(initialize_payload()).await.unwrap();

    let init_sent = driver.await.unwrap();
    // The plugin saw the full initialize payload (camelCase wire names).
    assert_eq!(init_sent["protocol"], tack_ext::PROTOCOL_VERSION);
    assert_eq!(init_sent["mode"], "tui");
    assert_eq!(init_sent["cwd"], "/tmp/tack-ext-test");
    assert_eq!(init_sent["trusted"], true);
    assert_eq!(init_sent["host"], "tack-test/0.0");

    // The host parsed the register payload field-for-field.
    assert_eq!(register.name.as_deref(), Some("fake"));
    assert_eq!(register.tools.len(), 1);
    assert_eq!(register.tools[0].name, "ping");
    assert_eq!(register.tools[0].label.as_deref(), Some("Ping"));
    assert_eq!(register.commands.len(), 1);
    assert_eq!(register.commands[0].name, "hello");
    assert_eq!(register.shortcuts.len(), 1);
    assert_eq!(register.shortcuts[0].action, "ext.fake.ping");
    assert_eq!(register.subscriptions, vec!["tool_call".to_string()]);
    // Note: no liveness assertion after join — the driver dropping the
    // plugin side correctly kills the peer via EOF.
}

/// A plugin that dies before registering must fail the handshake fast —
/// not after the 10s handshake timeout.
#[tokio::test]
async fn plugin_exiting_immediately_fails_handshake_fast() {
    let (peer, side, _services) = connect();
    let driver = tokio::spawn(async move {
        // Read (or ignore) initialize, then exit: drop both ends.
        drop(side);
    });

    let start = std::time::Instant::now();
    let err = peer.initialize(initialize_payload()).await.unwrap_err();
    assert!(
        err.contains("exited"),
        "expected 'plugin exited during handshake', got: {err}"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "handshake should fail fast on EOF, took {:?}",
        start.elapsed()
    );
    assert!(!peer.is_alive());
    driver.await.unwrap();
}

/// Protocol version mismatch: the plugin supports only v999, sees v1, and
/// exits (the handshake has no error channel — refusal = process exit).
/// The host must surface that as a handshake failure.
#[tokio::test]
async fn protocol_version_mismatch_ends_handshake() {
    let (peer, side, _services) = connect();
    let driver = tokio::spawn(run_versioned_handshake(side, 999));

    let err = peer.initialize(initialize_payload()).await.unwrap_err();
    assert!(err.contains("exited"), "got: {err}");

    // The plugin really did see (and reject) the host's version.
    let init_sent = driver.await.unwrap();
    assert_eq!(init_sent["protocol"], tack_ext::PROTOCOL_VERSION);
}

/// Malformed JSON lines before/during the handshake are skipped, not
/// fatal: a noisy plugin can still complete the handshake.
#[tokio::test]
async fn malformed_json_lines_are_skipped() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let _init = side.read_envelope().await;
        side.send_raw("this is not json at all").await;
        side.send_raw(r#"{"type":"request","id":"not-a-number"}"#)
            .await;
        side.send_raw("").await; // blank line
        side.send(&Envelope::event("register", register_payload("fake")))
            .await;
        // Keep the connection open so the host-side peer stays alive.
        std::future::pending::<()>().await;
    });

    let register = peer.initialize(initialize_payload()).await.unwrap();
    assert_eq!(register.name.as_deref(), Some("fake"));
    assert!(peer.is_alive());
    driver.abort();
}

/// A register event whose payload does not match RegisterPayload is
/// ignored (warned), and the handshake waits for a VALID register.
#[tokio::test]
async fn malformed_register_payload_is_ignored_until_valid_one() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let _init = side.read_envelope().await;
        // tools is not an array → fails RegisterPayload deserialization.
        side.send(&Envelope::event(
            "register",
            serde_json::json!({"name": 42, "tools": "nope"}),
        ))
        .await;
        // The valid register later still completes the handshake.
        side.send(&Envelope::event("register", register_payload("fake")))
            .await;
    });

    let register = peer.initialize(initialize_payload()).await.unwrap();
    assert_eq!(register.name.as_deref(), Some("fake"));
    driver.await.unwrap();
}

/// A plugin that stays connected but never registers must fail the
/// handshake after the (hardcoded 10s) handshake deadline.
#[tokio::test]
async fn handshake_times_out_when_plugin_never_registers() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let _init = side.read_envelope().await;
        // Stay alive and silent until the host gives up.
        std::future::pending::<()>().await;
    });

    let start = std::time::Instant::now();
    let err = peer.initialize(initialize_payload()).await.unwrap_err();
    assert!(err.contains("timed out"), "got: {err}");
    assert!(start.elapsed() >= std::time::Duration::from_secs(9));
    // A timed-out handshake does not kill the peer — the transport is
    // still fine; the host decides whether to keep the plugin.
    assert!(peer.is_alive());
    driver.abort();
}
