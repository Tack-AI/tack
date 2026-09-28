//! e2e: the examples/hello-wasm WAT plugin through the carrier (v3).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use serde_json::Value;
use tack_ext::rpc3::{
    ErrorObject, HostCapabilities, HostInfo, InitializeParams, RunMode, ToolExecuteParams,
};
use tack_ext::v3::PeerHandler;
use tack_ext_wasm::{WasmCarrier, WasmLimits};

struct Noop;

#[async_trait::async_trait]
impl PeerHandler for Noop {
    async fn handle_request(&self, _m: &str, _p: Value) -> Result<Value, ErrorObject> {
        Ok(Value::Null)
    }
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
        .client
        .initialize(&InitializeParams {
            protocol_version: tack_ext::v3::PROTOCOL_VERSION.to_string(),
            host: HostInfo {
                name: "test".to_string(),
                version: "test".to_string(),
            },
            mode: RunMode::Tui,
            cwd: "/tmp".to_string(),
            trusted: true,
            capabilities: HostCapabilities::default(),
            config: None,
        })
        .await
        .expect("handshake");
    assert_eq!(register.plugin.name, "hello-wasm");
    let output = plugin
        .client
        .tool_execute(&ToolExecuteParams {
            name: "ping".to_string(),
            tool_call_id: "c1".to_string(),
            arguments: serde_json::json!({}),
        })
        .await
        .expect("tool call");
    assert_eq!(
        output.content[0].text.as_deref(),
        Some("pong from the WASM sandbox")
    );
    // A second request on the same plugin (third protocol line overall):
    // the WAT main loop must keep answering.
    let output = plugin
        .client
        .command_invoke(&tack_ext::rpc3::CommandInvokeParams {
            name: "hello-wasm".to_string(),
            args: Some(String::new()),
        })
        .await
        .expect("command invoke");
    assert_eq!(output["ok"], serde_json::json!(true));
    plugin.shutdown().await;
}
