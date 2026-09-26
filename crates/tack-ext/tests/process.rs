//! Process-lifecycle integration tests: real plugin subprocesses (POSIX sh
//! scripts) through PluginProcess — full spawn/handshake/call/shutdown
//! cycle, immediate exit, and mid-request crash semantics.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use serde_json::{Value, json};
use std::sync::Arc;
use tack_ext::{HostServices, PluginProcess};

/// Register payload line every fake plugin script emits after initialize.
const REGISTER_LINE: &str = r#"{"type":"event","event":"register","payload":{"name":"sh-fake","tools":[{"name":"ping","description":"d","parameters":{}}]}}"#;

/// Fake plugin: handshake, then echo every request back with a fixed
/// result, extracting the request id with sed.
fn echo_script() -> String {
    format!(
        r#"
IFS= read -r init
printf '%s\n' '{REGISTER_LINE}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -n "$id" ]; then
    printf '{{"type":"response","id":%s,"result":{{"content":"pong"}}}}\n' "$id"
  fi
done
"#
    )
}

/// Fake plugin: handshake, read one request, then crash without answering.
fn crash_script() -> String {
    format!(
        r#"
IFS= read -r init
printf '%s\n' '{REGISTER_LINE}'
IFS= read -r line
exit 1
"#
    )
}

async fn spawn_script(script: &str, services: Arc<dyn HostServices>) -> PluginProcess {
    let cwd = std::env::temp_dir();
    PluginProcess::spawn(
        "sh",
        &["-c".to_string(), script.to_string()],
        &[],
        &cwd,
        services,
    )
    .await
    .expect("spawn sh plugin")
}

/// Full lifecycle over a real subprocess: spawn → initialize →
/// tool.execute round trip → graceful shutdown.
#[tokio::test]
async fn subprocess_full_lifecycle() {
    let services = Arc::new(RecordingServices::default());
    let mut process = spawn_script(&echo_script(), services).await;

    let register = process.peer.initialize(initialize_payload()).await.unwrap();
    assert_eq!(register.name.as_deref(), Some("sh-fake"));
    assert_eq!(register.tools.len(), 1);

    let result = process
        .peer
        .call(
            "tool.execute",
            json!({"name": "ping", "toolCallId": "c1", "arguments": {}}),
        )
        .await
        .unwrap();
    assert_eq!(result["content"], "pong");

    // A second call gets a fresh id and still matches its response.
    let result = process
        .peer
        .call(
            "tool.execute",
            json!({"name": "ping", "toolCallId": "c2", "arguments": {}}),
        )
        .await
        .unwrap();
    assert_eq!(result["content"], "pong");

    assert!(process.peer.is_alive());
    process.shutdown().await;
    assert!(!process.peer.is_alive(), "dead after the child exits");
}

/// A plugin process that exits before registering fails the handshake
/// immediately (no 10s wait).
#[tokio::test]
async fn subprocess_exiting_immediately_fails_handshake() {
    let services = Arc::new(RecordingServices::default());
    let mut process = spawn_script("exit 0", services).await;

    let start = std::time::Instant::now();
    let err = process
        .peer
        .initialize(initialize_payload())
        .await
        .unwrap_err();
    assert!(err.contains("exited"), "got: {err}");
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert!(!process.peer.is_alive());
    process.shutdown().await;
}

/// A plugin that crashes mid-request must fail the in-flight request fast
/// (EOF drains pending) — callers must never hang until the 30s request
/// timeout. Requests issued after the crash fail immediately as "dead".
#[tokio::test]
async fn subprocess_crash_fails_pending_and_future_requests() {
    let services = Arc::new(RecordingServices::default());
    let mut process = spawn_script(&crash_script(), services).await;
    process.peer.initialize(initialize_payload()).await.unwrap();

    let start = std::time::Instant::now();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        process.peer.call("tool.execute", json!({"name": "ping"})),
    )
    .await
    .expect("pending request hung after plugin crash")
    .unwrap_err();
    assert!(err.contains("exited"), "got: {err}");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "crash should fail pending requests fast, took {:?}",
        start.elapsed()
    );

    // Post-crash calls fail fast with a distinct "dead" error.
    let err = process
        .peer
        .call("tool.execute", json!({"name": "ping"}))
        .await
        .unwrap_err();
    assert!(err.contains("dead"), "got: {err}");
    process.shutdown().await;
}

/// Events from a dead plugin are dropped silently (send_event is
/// fire-and-forget and must not error on a dead peer).
#[tokio::test]
async fn events_to_dead_plugin_are_dropped() {
    let services = Arc::new(RecordingServices::default());
    let mut process = spawn_script("exit 0", services).await;
    let _ = process.peer.initialize(initialize_payload()).await;
    assert!(!process.peer.is_alive());
    process
        .peer
        .send_event("agent_start", Value::Null)
        .await
        .unwrap();
    process.shutdown().await;
}
