//! End-to-end tack-ext test: spawn the built-in demo plugin via
//! ExtensionManager, verify handshake, tool registration/execution, command
//! invocation, event fan-out, and the UI request bridge.
#![cfg(feature = "ext")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::await_holding_lock)]
#![allow(unsafe_code)]

/// The tests below all mutate the process-global `TACK_AGENT_DIR` — run
/// them under a shared lock so parallel test threads don't swap each
/// other's agent dir mid-load (flaky "demo plugin should load").
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::sync::Arc;

use serde_json::Value;
use tack_app::extension_host::ExtensionManager;
use tack_ext::HostServices;
use tokio::sync::Mutex;

/// Capture plugin→host traffic for assertions.
#[derive(Default)]
struct FakeServices {
    requests: Mutex<Vec<(String, Value)>>,
}

impl FakeServices {
    /// Poll the recorded requests until `pred` holds (20ms interval, 5s
    /// timeout) instead of a fixed sleep; modeled on `wait_for_event` in the
    /// tack-ext-wasm tests. Returns a snapshot for assertions; panics on
    /// timeout.
    async fn wait_for_requests(
        &self,
        what: &str,
        pred: impl Fn(&[(String, Value)]) -> bool,
    ) -> Vec<(String, Value)> {
        for _ in 0..250 {
            {
                let requests = self.requests.lock().await;
                if pred(&requests) {
                    return requests.clone();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }
}

#[async_trait::async_trait]
impl HostServices for FakeServices {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.requests
            .lock()
            .await
            .push((method.to_string(), params));
        Ok(Value::Null)
    }
    async fn handle_event(&self, _event: &str, _payload: Value) {}
}

async fn load_demo(
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
) -> (ExtensionManager, Arc<FakeServices>) {
    // Manifest running THIS test binary's tack with the demo plugin.
    let ext_dir = agent_dir.join("extensions").join("demo");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("extension.json"),
        serde_json::json!({
            "name": "demo",
            "command": env!("CARGO_BIN_EXE_tack"),
            "args": ["ext-demo-plugin"],
        })
        .to_string(),
    )
    .unwrap();
    let services = Arc::new(FakeServices::default());
    let manager = ExtensionManager::load(cwd, agent_dir, "tui", services.clone(), true).await;
    (manager, services)
}

#[tokio::test]
async fn demo_plugin_full_lifecycle() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let agent_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    let cwd = tempfile::tempdir().unwrap();
    // Trust the project so its (empty) .pi dir passes the gate; the demo
    // plugin lives in the user extensions dir anyway.
    tack_app::project_trust::set_decision(agent_dir.path(), cwd.path(), true, false);

    let (mut manager, services) = load_demo(cwd.path(), agent_dir.path()).await;
    assert_eq!(manager.plugins.len(), 1, "demo plugin should load");

    // Tools: registered and named ext__demo__echo.
    let tools = manager.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "ext__demo__echo");

    // Tool execution crosses to the plugin and back.
    let result = tools[0]
        .execute(
            "call-1",
            serde_json::json!({ "text": "hello-ext" }),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
    let text = result
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "echo: hello-ext");

    // Command invocation + its ui.notify side request.
    assert!(manager.command_names().contains(&"hello".to_string()));
    manager.invoke_command("hello", "").await.unwrap();
    let requests = services
        .wait_for_requests("ui.notify", |reqs| {
            reqs.iter().any(|(m, _)| m == "ui.notify")
        })
        .await;
    assert!(
        requests.iter().any(|(m, _)| m == "ui.notify"),
        "plugin's ui.notify should reach the host: {requests:?}"
    );

    // Event fan-out: agent_start triggers the plugin's ui.notify request.
    manager.notify("agent_start", serde_json::json!({})).await;
    let requests = services
        .wait_for_requests("second ui.notify", |reqs| {
            reqs.iter().filter(|(m, _)| m == "ui.notify").count() >= 2
        })
        .await;
    let notify_count = requests.iter().filter(|(m, _)| m == "ui.notify").count();
    assert!(
        notify_count >= 2,
        "agent_start should trigger another notify: {requests:?}"
    );

    // tool_call interception (subscribed): verdict allow passes through.
    let hooks = manager.hooks();
    assert_eq!(hooks.len(), 1);
    let ctx_msg = tack_ai::AssistantMessage::pending(
        &tack_app::model::resolve_model(
            "anthropic",
            Some("k3"),
            &tack_session::default_agent_dir(),
        )
        .unwrap(),
    );
    let outcome = hooks[0]
        .before_tool_call(&tack_agent_core::BeforeToolCallContext {
            assistant_message: &ctx_msg,
            tool_call_id: "c1",
            tool_name: "read",
            args: &serde_json::json!({}),
            context: &[],
        })
        .await;
    assert!(matches!(
        outcome,
        tack_agent_core::BeforeToolCallOutcome::Allow
    ));

    manager.shutdown().await;
}

/// Headless run modes (print/rpc/acp) load the same plugins with degraded
/// UI services: tools/commands/events keep working, `ui.notify` is accepted
/// silently instead of hanging on a dialog.
#[tokio::test]
async fn demo_plugin_in_headless_mode() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let agent_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    let cwd = tempfile::tempdir().unwrap();
    tack_app::project_trust::set_decision(agent_dir.path(), cwd.path(), true, false);

    let ext_dir = agent_dir.path().join("extensions").join("demo");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("extension.json"),
        serde_json::json!({
            "name": "demo",
            "command": env!("CARGO_BIN_EXE_tack"),
            "args": ["ext-demo-plugin"],
        })
        .to_string(),
    )
    .unwrap();
    let services = tack_app::ext_headless::HeadlessExtServices::new("print", true);
    let mut manager =
        ExtensionManager::load(cwd.path(), agent_dir.path(), "print", services, true).await;
    assert_eq!(manager.plugins.len(), 1, "demo plugin should load headless");

    // Tools work identically in headless mode.
    let tools = manager.tools();
    assert_eq!(tools.len(), 1);
    let result = tools[0]
        .execute(
            "call-1",
            serde_json::json!({ "text": "headless" }),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
    let text = result
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "echo: headless");

    // The demo plugin's `hello` command fires ui.notify; headless services
    // resolve it immediately (degraded) instead of routing to a dialog.
    manager.invoke_command("hello", "").await.unwrap();

    manager.shutdown().await;
}

/// v2.1/v2.2 e2e: the demo plugin's declared widgets land in the host
/// registry keyed `<plugin>:<id>`; autocomplete.provide round-trips; a
/// widget.action is delivered back to the owning plugin.
#[tokio::test]
async fn demo_plugin_widgets_and_autocomplete() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let agent_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    let cwd = tempfile::tempdir().unwrap();
    tack_app::project_trust::set_decision(agent_dir.path(), cwd.path(), true, false);

    let (mut manager, services) = load_demo(cwd.path(), agent_dir.path()).await;
    assert_eq!(manager.plugins.len(), 1, "demo plugin should load");

    // v2.1: widgets registered from the register payload.
    let keys: Vec<&str> = manager.widgets().iter().map(|w| w.key.as_str()).collect();
    assert_eq!(keys, vec!["demo:demo-status", "demo:demo-list"]);
    let status = &manager.widgets()[0];
    assert_eq!(status.spec.kind, tack_ext::WidgetKind::StatusLineSegment);
    assert_eq!(
        status.state,
        Some(serde_json::json!({"text": "demo:ok", "style": "info"}))
    );
    assert!(manager.widgets()[1].visible, "panel defaults to visible");

    // widget.update via the manager: full-state replacement; unknown ids
    // tolerated.
    let update = tack_ext::WidgetUpdatePayload {
        id: "demo-status".to_string(),
        state: serde_json::json!({"text": "demo:busy", "style": "warning"}),
        visible: None,
    };
    assert!(manager.apply_widget_update("demo", &update));
    assert_eq!(
        manager.widgets()[0].state,
        Some(serde_json::json!({"text": "demo:busy", "style": "warning"}))
    );
    assert!(!manager.apply_widget_update("demo", &{
        tack_ext::WidgetUpdatePayload {
            id: "nope".to_string(),
            state: serde_json::json!({}),
            visible: None,
        }
    }));

    // v2.2: providers listed in register order; provide round-trips.
    let providers = manager.autocomplete_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].key, "demo:hash");
    assert_eq!(providers[0].spec.trigger, "#");
    let suggestions = manager.autocomplete_provide("demo:hash", "wa", 2).await;
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].value, "#wasm");
    assert_eq!(suggestions[0].label, "#wasm demo tag");
    // Unknown provider key → no suggestions (never an error to the UI).
    assert!(
        manager
            .autocomplete_provide("demo:nope", "x", 0)
            .await
            .is_empty()
    );

    // v2.1: widget.action is delivered to the owning plugin; the demo
    // plugin echoes it back as a ui.notify request.
    manager
        .notify_widget_action(
            "demo",
            tack_ext::WidgetActionPayload {
                id: "demo-list".to_string(),
                action: "select".to_string(),
                item_id: Some("b".to_string()),
            },
        )
        .await;
    let requests = services
        .wait_for_requests("widget.action echo", |reqs| {
            reqs.iter().any(|(m, p)| {
                m == "ui.notify"
                    && p.get("message")
                        .and_then(Value::as_str)
                        .is_some_and(|msg| msg.contains("widget.action"))
            })
        })
        .await;
    let echo = requests
        .iter()
        .filter(|(m, _)| m == "ui.notify")
        .filter_map(|(_, p)| p.get("message").and_then(Value::as_str).map(str::to_string))
        .find(|msg| msg.contains("widget.action"))
        .unwrap_or_default();
    assert!(echo.contains("demo-list"), "action id missing: {echo}");
    assert!(echo.contains("\"itemId\":\"b\""), "item id missing: {echo}");

    // Plugin death cleanup: after shutdown the registry can be drained.
    manager.shutdown().await;
    let removed = manager.remove_plugin_widgets("demo");
    assert_eq!(removed.len(), 2);
    assert!(manager.widgets().is_empty());
}
