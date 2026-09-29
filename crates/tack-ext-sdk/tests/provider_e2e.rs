//! Provider bridge e2e tests (P7): a plugin serving inference over an
//! in-memory duplex, driven by the host-side `HostClient` — the same code
//! paths the process carrier uses.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tack_ext::rpc3::{
    ErrorObject, HostCapabilities, HostInfo, InitializeParams, InitializeResult,
    ProviderStreamParams, RunMode, method,
};
use tack_ext::v3::{HostClient, JsonRpcPeer, PeerHandler, PluginConnection};
use tack_ext_sdk::{Plugin, ProviderEvents, ProviderStreamCx};

// ---------------------------------------------------------------------------
// Host stub: captures provider registrations and stream events
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HostStub {
    registrations: Mutex<Vec<Value>>,
    stream_events: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl PeerHandler for HostStub {
    async fn handle_request(&self, rpc_method: &str, params: Value) -> Result<Value, ErrorObject> {
        match rpc_method {
            method::HOST_REGISTER_PROVIDER => {
                self.registrations
                    .lock()
                    .unwrap()
                    .push(params.get("provider").cloned().unwrap_or(Value::Null));
                Ok(Value::Null)
            }
            _ => Err(ErrorObject {
                code: tack_ext::rpc3::ERR_METHOD_NOT_FOUND,
                message: format!("stub: unknown {rpc_method}"),
                data: None,
            }),
        }
    }

    async fn handle_notification(&self, rpc_method: &str, params: Value) {
        if rpc_method == method::PROVIDER_STREAM_EVENT {
            let stream_id = params
                .get("streamId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let event = params.get("event").cloned().unwrap_or(Value::Null);
            self.stream_events.lock().unwrap().push((stream_id, event));
        }
    }
}

struct Fixture {
    client: HostClient,
    stub: Arc<HostStub>,
    plugin_task: tokio::task::JoinHandle<std::io::Result<()>>,
    init: InitializeResult,
}

fn init_params() -> InitializeParams {
    InitializeParams {
        protocol_version: tack_ext_sdk::PROTOCOL_VERSION.to_string(),
        host: HostInfo {
            name: "tack".to_string(),
            version: "test".to_string(),
        },
        mode: RunMode::Tui,
        cwd: "/tmp".to_string(),
        trusted: true,
        capabilities: HostCapabilities {
            provider_registration: Some(true),
            ..Default::default()
        },
        config: None,
    }
}

async fn spawn(plugin: Plugin) -> Fixture {
    let (s1, s2) = tokio::io::duplex(64 * 1024);
    let (r1, w1) = tokio::io::split(s1);
    let (r2, w2) = tokio::io::split(s2);
    let stub = Arc::new(HostStub::default());
    let host_peer = JsonRpcPeer::new(r1, w1, stub.clone());
    let plugin_task = tokio::spawn(plugin.run_on(r2, w2));
    let client = HostClient::new(host_peer);
    let init = client.initialize(&init_params()).await.expect("handshake");
    Fixture {
        client,
        stub,
        plugin_task,
        init,
    }
}

fn stream_params(text: &str) -> ProviderStreamParams {
    ProviderStreamParams {
        stream_id: "ps-test-1".to_string(),
        model: json!({"id": "fake-1", "provider": "demo-provider", "api": "ext-provider-bridge"}),
        context: json!({"messages": [{"role": "user", "content": text, "timestamp": 0}]}),
        options: json!({"maxTokens": 1024}),
    }
}

/// A pending assistant message skeleton for scripted events.
fn partial(model: &Value) -> Value {
    json!({
        "content": [],
        "api": model["api"].as_str().unwrap_or(""),
        "provider": model["provider"].as_str().unwrap_or(""),
        "model": model["id"].as_str().unwrap_or(""),
        "usage": {
            "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
        },
        "stopReason": "pending",
        "timestamp": 0,
    })
}

async fn wait_for_events(stub: &HostStub, count: usize) -> Vec<(String, Value)> {
    for _ in 0..100 {
        let captured = stub.stream_events.lock().unwrap().clone();
        if captured.len() >= count {
            return captured;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {count} stream events");
}

fn echo_plugin() -> Plugin {
    Plugin::builder("provider-plugin")
        .provider_stream(|params, events, _cx| async move {
            events
                .send(json!({"type": "start", "partial": partial(&params.model)}))
                .await?;
            events
                .text_delta(0, "hello", partial(&params.model))
                .await?;
            let mut message = partial(&params.model);
            message["content"] = json!([{"type": "text", "text": "hello"}]);
            message["stopReason"] = json!("stop");
            events.done(message).await?;
            Ok(())
        })
        .build()
}

#[tokio::test]
async fn capability_advertised_and_stream_events_flow() {
    let fixture = spawn(echo_plugin()).await;
    // The handshake advertised provider.stream (and only that).
    let provider = fixture
        .init
        .capabilities
        .provider
        .as_ref()
        .expect("provider capability");
    assert_eq!(provider.stream, Some(true));
    assert_eq!(provider.register, None);
    PluginConnection::provider_stream(&fixture.client, &stream_params("hi"))
        .await
        .expect("fast ack");
    let captured = wait_for_events(&fixture.stub, 3).await;
    assert!(captured.iter().all(|(id, _)| id == "ps-test-1"));
    assert_eq!(captured[0].1["type"], "start");
    assert_eq!(captured[1].1["type"], "textDelta");
    assert_eq!(captured[1].1["delta"], "hello");
    assert_eq!(captured[2].1["type"], "done");
    assert_eq!(captured[2].1["message"]["stopReason"], "stop");
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn provider_register_capability_is_advertised() {
    let plugin = Plugin::builder("register-plugin")
        .provider_register(true)
        .build();
    let fixture = spawn(plugin).await;
    let provider = fixture
        .init
        .capabilities
        .provider
        .as_ref()
        .expect("provider capability");
    assert_eq!(provider.register, Some(true));
    assert_eq!(provider.stream, None);
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn provider_register_combines_with_stream() {
    let plugin = Plugin::builder("register-plugin")
        .provider_register(true)
        .provider_stream(|_params, _events, _cx| async move { Ok(()) })
        .build();
    let fixture = spawn(plugin).await;
    let provider = fixture
        .init
        .capabilities
        .provider
        .as_ref()
        .expect("provider capability");
    assert_eq!(provider.register, Some(true));
    assert_eq!(provider.stream, Some(true));
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn provider_capability_is_absent_without_knobs() {
    let plugin = Plugin::builder("plain-plugin").build();
    let fixture = spawn(plugin).await;
    assert!(fixture.init.capabilities.provider.is_none());
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn provider_register_false_declares_nothing() {
    let plugin = Plugin::builder("plain-plugin")
        .provider_register(false)
        .build();
    let fixture = spawn(plugin).await;
    assert!(fixture.init.capabilities.provider.is_none());
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn on_ready_registers_the_provider() {
    let plugin = Plugin::builder("provider-plugin")
        .provider_stream(|_params, _events, _cx| async move { Ok(()) })
        .on_ready(|cx| async move {
            cx.host()
                .register_provider(json!({
                    "id": "demo-provider", "bridge": true, "models": [{"id": "fake-1"}]
                }))
                .await
                .expect("registration ack");
        })
        .build();
    let fixture = spawn(plugin).await;
    for _ in 0..100 {
        if !fixture.stub.registrations.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let registrations = fixture.stub.registrations.lock().unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0]["id"], "demo-provider");
    assert_eq!(registrations[0]["bridge"], true);
    drop(registrations);
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn missing_terminal_fires_the_automatic_error() {
    let plugin = Plugin::builder("provider-plugin")
        .provider_stream(|params, events, _cx| async move {
            events
                .send(json!({"type": "start", "partial": partial(&params.model)}))
                .await?;
            Ok(()) // no terminal: SDK enforcement fires
        })
        .build();
    let fixture = spawn(plugin).await;
    PluginConnection::provider_stream(&fixture.client, &stream_params("hi"))
        .await
        .unwrap();
    let captured = wait_for_events(&fixture.stub, 2).await;
    assert_eq!(captured[0].1["type"], "start");
    assert_eq!(captured[1].1["type"], "error");
    assert!(
        captured[1].1["error"]["errorMessage"]
            .as_str()
            .unwrap()
            .contains("without a terminal event"),
        "{:?}",
        captured[1].1
    );
    // The synthesized error message is a valid assistant message shape.
    assert_eq!(captured[1].1["error"]["provider"], "demo-provider");
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn handler_error_becomes_the_terminal_error_event() {
    let plugin = Plugin::builder("provider-plugin")
        .provider_stream(|_params, _events, _cx| async move {
            Err(tack_ext_sdk::Error::internal("backend exploded"))
        })
        .build();
    let fixture = spawn(plugin).await;
    PluginConnection::provider_stream(&fixture.client, &stream_params("hi"))
        .await
        .unwrap();
    let captured = wait_for_events(&fixture.stub, 1).await;
    assert_eq!(captured[0].1["type"], "error");
    assert_eq!(
        captured[0].1["error"]["errorMessage"].as_str().unwrap(),
        "backend exploded"
    );
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn second_terminal_event_is_rejected() {
    let plugin = Plugin::builder("provider-plugin")
        .provider_stream(|params, events, _cx| async move {
            let message = partial(&params.model);
            events.done(message.clone()).await?;
            let second = events.done(message).await;
            assert!(second.is_err(), "a second terminal must be rejected");
            Ok(())
        })
        .build();
    let fixture = spawn(plugin).await;
    PluginConnection::provider_stream(&fixture.client, &stream_params("hi"))
        .await
        .unwrap();
    let captured = wait_for_events(&fixture.stub, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(captured.len(), 1, "exactly one terminal crossed the wire");
    assert_eq!(captured[0].1["type"], "done");
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn stream_cancel_reaches_the_handler() {
    let plugin = Plugin::builder("provider-plugin")
        .provider_stream(|params, events, cx: ProviderStreamCx| async move {
            events
                .send(json!({"type": "start", "partial": partial(&params.model)}))
                .await?;
            cx.cancelled().await;
            let mut error = partial(&params.model);
            error["stopReason"] = json!("aborted");
            error["errorMessage"] = json!("demo aborted");
            events
                .send(json!({"type": "error", "reason": "aborted", "error": error}))
                .await?;
            Ok(())
        })
        .build();
    let fixture = spawn(plugin).await;
    PluginConnection::provider_stream(&fixture.client, &stream_params("hi"))
        .await
        .unwrap();
    wait_for_events(&fixture.stub, 1).await; // start landed
    PluginConnection::provider_stream_cancel(&fixture.client, "ps-test-1")
        .await
        .unwrap();
    let captured = wait_for_events(&fixture.stub, 2).await;
    assert_eq!(captured[1].1["type"], "error");
    assert_eq!(captured[1].1["reason"], "aborted");
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn undeclared_provider_stream_is_capability_not_granted() {
    let plugin = Plugin::builder("plain-plugin").build();
    let fixture = spawn(plugin).await;
    let err = PluginConnection::provider_stream(&fixture.client, &stream_params("hi"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tack_ext::rpc3::ERR_CAPABILITY_NOT_GRANTED);
    fixture.plugin_task.abort();
}

#[tokio::test]
async fn options_and_context_arrive_verbatim() {
    let plugin = Plugin::builder("provider-plugin")
        .provider_stream(|params, events: ProviderEvents, _cx| async move {
            assert_eq!(params.options["maxTokens"], 1024);
            assert_eq!(
                params.context["messages"][0]["content"].as_str().unwrap(),
                "check"
            );
            events.error("seen", None).await?;
            Ok(())
        })
        .build();
    let fixture = spawn(plugin).await;
    PluginConnection::provider_stream(&fixture.client, &stream_params("check"))
        .await
        .unwrap();
    let captured = wait_for_events(&fixture.stub, 1).await;
    assert_eq!(captured[0].1["error"]["errorMessage"], "seen");
    fixture.plugin_task.abort();
}
