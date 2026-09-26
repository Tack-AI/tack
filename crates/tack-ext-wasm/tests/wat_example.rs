//! e2e: the examples/hello-wasm WAT plugin through the carrier.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use serde_json::Value;
use tack_ext::HostServices;
use tack_ext_wasm::{WasmCarrier, WasmLimits};

struct Noop;
#[async_trait::async_trait]
impl HostServices for Noop {
    async fn handle_request(&self, _m: &str, _p: Value) -> Result<Value, String> {
        Ok(Value::Null)
    }
    async fn handle_event(&self, _e: &str, _p: Value) {}
}

#[tokio::test(flavor = "multi_thread")]
async fn example_wat_handshakes() {
    let wat = include_str!("../../../examples/extensions/hello-wasm/plugin.wat");
    let carrier = WasmCarrier::new().unwrap();
    let plugin = carrier
        .spawn(wat.as_bytes(), &WasmLimits::default(), Arc::new(Noop))
        .await
        .expect("spawn");
    let register = plugin
        .peer
        .initialize(tack_ext::InitializePayload {
            protocol: 2,
            mode: "tui".into(),
            cwd: "/tmp".into(),
            trusted: true,
            host: "test".into(),
        })
        .await
        .expect("handshake");
    assert_eq!(register.name.as_deref(), Some("hello-wasm"));
    let result = plugin
        .peer
        .call(
            "tool.execute",
            serde_json::json!({"name": "ping", "toolCallId": "c1", "arguments": {}}),
        )
        .await
        .expect("tool call");
    assert_eq!(
        result["content"],
        serde_json::json!("pong from the WASM sandbox")
    );
    plugin.shutdown().await;
}
