//! End-to-end tests: a real plugin served over an in-memory duplex,
//! driven by the host-side `HostClient` — the same code paths the
//! process carrier uses.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tack_ext::rpc3::{
    AfterToolCallParams, AfterToolCallPatch, ApprovalReviewParams, AutocompleteProvideParams,
    AutocompleteProviderSpec, BeforeToolCallParams, ErrorObject, HostCapabilities, HostInfo,
    InitializeParams, LifecycleEventParams, RunMode, ToolCall, ToolExecuteParams, ToolOutput,
    TransformContextParams, VerdictAction, WidgetActionParams, method,
};
use tack_ext::v3::{HostClient, JsonRpcPeer, PeerHandler};
use tack_ext_sdk::{
    ApprovalDecision, ApprovalDecisionAction, Plugin, ToolSpec, allow, deny, text_output,
};

// ---------------------------------------------------------------------------
// Host stub: records plugin→host requests, answers canned responses
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HostStub {
    requests: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl PeerHandler for HostStub {
    async fn handle_request(&self, rpc_method: &str, params: Value) -> Result<Value, ErrorObject> {
        self.requests
            .lock()
            .unwrap()
            .push((rpc_method.to_string(), params.clone()));
        match rpc_method {
            method::UI_NOTIFY => Ok(Value::Null),
            method::UI_SELECT => Ok(json!("b")),
            method::UI_CONFIRM => Ok(json!(true)),
            method::SESSION_GET => Ok(json!({
                "sessionId": "s-1", "mode": "tui", "cwd": "/tmp", "trusted": true,
                "messageCount": 3
            })),
            method::CONFIG_GET => Ok(json!({"config": {"severity": "high"}})),
            method::EXEC_RUN => Err(ErrorObject {
                code: tack_ext::rpc3::ERR_POLICY_DENIED,
                message: "exec requires project trust".to_string(),
                data: None,
            }),
            _ => Err(ErrorObject {
                code: tack_ext::rpc3::ERR_METHOD_NOT_FOUND,
                message: format!("stub: unknown {rpc_method}"),
                data: None,
            }),
        }
    }
}

struct Fixture {
    client: HostClient,
    stub: Arc<HostStub>,
    plugin_task: tokio::task::JoinHandle<std::io::Result<()>>,
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
            widgets: Some(true),
            autocomplete: Some(true),
            session_control: Some(true),
            snapshot: Some(true),
            ui_dialogs: Some(true),
            exec: Some(true),
            provider_registration: None,
            metrics: None,
        },
        config: Some(json!({"severity": "high"})),
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
    let fixture = Fixture {
        client,
        stub,
        plugin_task,
    };
    fixture
        .client
        .initialize(&init_params())
        .await
        .expect("handshake");
    fixture
}

fn echo_tool() -> Plugin {
    Plugin::builder("test-plugin")
        .version("1.0.0")
        .tool(
            ToolSpec {
                name: "test.echo".to_string(),
                label: None,
                description: "echo".to_string(),
                parameters: json!({"type": "object"}),
            },
            |params, _cx| async move { Ok(text_output(format!("echo: {}", params.arguments))) },
        )
        .build()
}

fn tool_call(name: &str) -> ToolCall {
    ToolCall {
        tool_call_id: "c-1".to_string(),
        tool_name: name.to_string(),
        arguments: json!({"command": "ls"}),
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handshake_advertises_declared_capabilities() {
    let fixture = spawn(echo_tool()).await;
    // A second initialize is not part of the protocol; assert on the
    // first one's result instead — re-run initialize for inspection.
    let result = fixture.client.initialize(&init_params()).await.unwrap();
    assert_eq!(result.plugin.name, "test-plugin");
    assert_eq!(result.plugin.version.as_deref(), Some("1.0.0"));
    let tools = result.capabilities.tools.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "test.echo");
    assert!(result.capabilities.hooks.is_none(), "no hooks declared");
    drop(fixture);
}

#[tokio::test]
async fn handshake_rejects_incompatible_host_version() {
    let (s1, s2) = tokio::io::duplex(64 * 1024);
    let (r1, w1) = tokio::io::split(s1);
    let (r2, w2) = tokio::io::split(s2);
    let host_peer = JsonRpcPeer::new(r1, w1, Arc::new(HostStub::default()));
    let _plugin = tokio::spawn(echo_tool().run_on(r2, w2));
    let client = HostClient::new(host_peer);
    let mut params = init_params();
    params.protocol_version = "4.0.0".to_string();
    let err = client.initialize(&params).await.unwrap_err();
    assert!(
        err.to_string().contains("unsupported host protocol"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// tools/execute
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_execute_roundtrip() {
    let fixture = spawn(echo_tool()).await;
    let output = fixture
        .client
        .tool_execute(&ToolExecuteParams {
            name: "test.echo".to_string(),
            tool_call_id: "c-1".to_string(),
            arguments: json!({"x": 42}),
        })
        .await
        .unwrap();
    let ToolOutput { content, .. } = &output;
    assert_eq!(content[0].text.as_deref(), Some("echo: {\"x\":42}"));
}

#[tokio::test]
async fn tool_execute_unknown_tool_is_invalid_params() {
    let fixture = spawn(echo_tool()).await;
    let err = fixture
        .client
        .tool_execute(&ToolExecuteParams {
            name: "nope".to_string(),
            tool_call_id: "c-1".to_string(),
            arguments: json!({}),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tack_ext::rpc3::ERR_INVALID_PARAMS);
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn before_tool_call_verdicts() {
    let plugin = Plugin::builder("guard")
        .before_tool_call(|params, _cx| async move {
            if params.tool_call.tool_name == "bash" {
                return Ok(deny("no shell today"));
            }
            Ok(allow())
        })
        .build();
    let fixture = spawn(plugin).await;
    let verdict = fixture
        .client
        .before_tool_call(&BeforeToolCallParams {
            tool_call: tool_call("bash"),
            assistant_message: None,
        })
        .await
        .unwrap();
    assert_eq!(verdict.action, VerdictAction::Deny);
    assert_eq!(verdict.reason.as_deref(), Some("no shell today"));

    let verdict = fixture
        .client
        .before_tool_call(&BeforeToolCallParams {
            tool_call: tool_call("read"),
            assistant_message: None,
        })
        .await
        .unwrap();
    assert_eq!(verdict.action, VerdictAction::Allow);
}

#[tokio::test]
async fn after_tool_call_patch_and_transform_context_null() {
    let plugin = Plugin::builder("patcher")
        .after_tool_call(|_params: AfterToolCallParams, _cx| async move {
            Ok(Some(AfterToolCallPatch {
                content: None,
                details: Some(json!({"patched": true})),
                usage: None,
                terminate: None,
                is_error: None,
            }))
        })
        .transform_context(|_params: TransformContextParams, _cx| async move { Ok(None) })
        .build();
    let fixture = spawn(plugin).await;
    let patch = fixture
        .client
        .after_tool_call(&AfterToolCallParams {
            tool_call: tool_call("bash"),
            result: text_output("ok"),
            is_error: false,
        })
        .await
        .unwrap()
        .expect("patch");
    assert_eq!(patch.details, Some(json!({"patched": true})));

    // transform_context returning None serializes as a null result,
    // which the host client maps to None ("context unchanged").
    let rewritten = fixture
        .client
        .transform_context(&TransformContextParams {
            messages: vec![json!({"role": "user"})],
        })
        .await
        .unwrap();
    assert!(rewritten.is_none());
}

#[tokio::test]
async fn approval_review_passthrough_and_claim() {
    let plugin = Plugin::builder("approver")
        .approval_review(|params: ApprovalReviewParams, _cx| async move {
            if params.tool_call.tool_name == "bash" {
                return Ok(Some(ApprovalDecision {
                    action: ApprovalDecisionAction::AskUser,
                    reason: Some("human should see shell".to_string()),
                }));
            }
            Ok(None)
        })
        .build();
    let fixture = spawn(plugin).await;
    let review = |name: &str| ApprovalReviewParams {
        approval_id: "a-1".to_string(),
        tool_call: tool_call(name),
        approval_policy: "on-request".to_string(),
        evidence: None,
    };
    let decision = fixture
        .client
        .approval_review(&review("bash"))
        .await
        .unwrap();
    assert_eq!(decision.unwrap().action, ApprovalDecisionAction::AskUser);
    let decision = fixture
        .client
        .approval_review(&review("read"))
        .await
        .unwrap();
    assert!(decision.is_none(), "pass-through is a null result");
}

#[tokio::test]
async fn hook_not_declared_is_capability_not_granted() {
    let fixture = spawn(echo_tool()).await;
    let err = fixture
        .client
        .before_tool_call(&BeforeToolCallParams {
            tool_call: tool_call("bash"),
            assistant_message: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tack_ext::rpc3::ERR_CAPABILITY_NOT_GRANTED);
}

// ---------------------------------------------------------------------------
// events / widgets / autocomplete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_events_and_widget_actions_dispatch() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_events = seen.clone();
    let seen_actions = seen.clone();
    let plugin = Plugin::builder("observer")
        .events(&["turnStart"], move |params: LifecycleEventParams, _cx| {
            let seen = seen_events.clone();
            async move {
                seen.lock().unwrap().push(params.event);
            }
        })
        .widget(tack_ext_sdk::WidgetSpec {
            id: "list".to_string(),
            r#type: tack_ext_sdk::WidgetKind::ListPanel,
            priority: None,
            title: Some("items".to_string()),
            visible: None,
            initial: None,
        })
        .on_widget_action(move |params: WidgetActionParams, _cx| {
            let seen = seen_actions.clone();
            async move {
                seen.lock().unwrap().push(format!(
                    "{}:{}",
                    params.action,
                    params.item_id.unwrap_or_default()
                ));
            }
        })
        .build();
    let fixture = spawn(plugin).await;
    fixture
        .client
        .lifecycle_event("turnStart", json!({"turn": 1}))
        .await
        .unwrap();
    fixture
        .client
        .widget_action(&WidgetActionParams {
            id: "list".to_string(),
            action: "select".to_string(),
            item_id: Some("a.rs".to_string()),
        })
        .await
        .unwrap();
    for _ in 0..50 {
        if seen.lock().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let seen = seen.lock().unwrap();
    assert!(seen.contains(&"turnStart".to_string()), "{seen:?}");
    assert!(seen.contains(&"select:a.rs".to_string()), "{seen:?}");
}

#[tokio::test]
async fn autocomplete_roundtrip() {
    let plugin = Plugin::builder("complete")
        .autocomplete(
            AutocompleteProviderSpec {
                id: "issues".to_string(),
                trigger: "#".to_string(),
                description: None,
            },
            |params: AutocompleteProvideParams, _cx| async move {
                Ok(tack_ext_sdk::AutocompleteProvideResult {
                    suggestions: vec![tack_ext_sdk::AutocompleteSuggestion {
                        value: format!("#{}", params.query),
                        label: "issue".to_string(),
                        detail: None,
                        insert_text: None,
                    }],
                })
            },
        )
        .build();
    let fixture = spawn(plugin).await;
    let result = fixture
        .client
        .autocomplete_provide(&AutocompleteProvideParams {
            provider_id: "issues".to_string(),
            query: "42".to_string(),
            cursor_offset: 2,
        })
        .await
        .unwrap();
    assert_eq!(result.suggestions[0].value, "#42");
}

// ---------------------------------------------------------------------------
// plugin → host services
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_calls_host_services_from_a_tool() {
    let plugin = Plugin::builder("needy")
        .tool(
            ToolSpec {
                name: "test.ask".to_string(),
                label: None,
                description: "uses host services".to_string(),
                parameters: json!({"type": "object"}),
            },
            |_params, cx| async move {
                let host = cx.host();
                // Dialog answer round-trips through the host stub.
                let picked = host
                    .select("pick".to_string(), vec!["a".to_string(), "b".to_string()])
                    .await
                    .map_err(tack_ext_sdk::Error::from)?;
                // exec is policy-denied by the stub: the domain code survives.
                let exec_err = host.exec("rm -rf /".to_string(), None).await.unwrap_err();
                assert_eq!(exec_err.code(), tack_ext::rpc3::ERR_POLICY_DENIED);
                // initialize-delivered config is available without a call.
                let severity = cx.config().unwrap()["severity"].as_str().unwrap_or("");
                Ok(text_output(format!(
                    "picked={} severity={severity}",
                    picked.unwrap_or_default()
                )))
            },
        )
        .build();
    let fixture = spawn(plugin).await;
    let output = fixture
        .client
        .tool_execute(&ToolExecuteParams {
            name: "test.ask".to_string(),
            tool_call_id: "c-1".to_string(),
            arguments: json!({}),
        })
        .await
        .unwrap();
    assert_eq!(
        output.content[0].text.as_deref(),
        Some("picked=b severity=high")
    );
    let requests = fixture.stub.requests.lock().unwrap();
    assert!(
        requests.iter().any(|(m, _)| m == method::UI_SELECT),
        "{requests:?}"
    );
    assert!(
        requests.iter().any(|(m, _)| m == method::EXEC_RUN),
        "{requests:?}"
    );
}

// ---------------------------------------------------------------------------
// shutdown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_terminates_the_serve_loop() {
    let fixture = spawn(echo_tool()).await;
    fixture.client.shutdown().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), fixture.plugin_task)
        .await
        .expect("plugin should exit after shutdown");
    result.unwrap().unwrap();
}
