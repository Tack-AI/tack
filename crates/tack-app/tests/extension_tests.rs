//! End-to-end extension-host test (v3): spawn the built-in v3 demo
//! plugin via ExtensionManager, verify handshake, tool registration/
//! execution, command invocation, interception, event fan-out, widgets,
//! autocomplete, and the UI request bridge.
#![cfg(feature = "ext")]
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

/// The tests below all mutate the process-global `TACK_AGENT_DIR` — run
/// them under a shared lock so parallel test threads don't swap each
/// other's agent dir mid-load.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use std::sync::Arc;

use serde_json::Value;
use tack_app::extension_host::ExtensionManager;
use tack_ext::rpc3::ErrorObject;
use tack_ext::v3::PeerHandler;
use tokio::sync::Mutex;

/// Capture plugin→host requests for assertions.
#[derive(Default)]
struct FakeServices {
    requests: Mutex<Vec<(String, Value)>>,
}

impl FakeServices {
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
impl PeerHandler for FakeServices {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, ErrorObject> {
        self.requests
            .lock()
            .await
            .push((method.to_string(), params));
        Ok(Value::Null)
    }
}

/// Path of the built v3 demo-plugin bin (fixture), derived from the test
/// binary location (…/target/<profile>/deps/…).
fn demo_plugin_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // profile/
    path.push(format!(
        "tack-v3-demo-plugin{}",
        std::env::consts::EXE_SUFFIX
    ));
    assert!(
        path.is_file(),
        "demo plugin bin missing: {}",
        path.display()
    );
    path.to_string_lossy().to_string()
}

async fn load_demo(
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
) -> (ExtensionManager, Arc<FakeServices>) {
    let ext_dir = agent_dir.join("extensions").join("demo");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("extension.json"),
        serde_json::json!({
            "name": "demo",
            "command": demo_plugin_bin(),
            "args": [],
        })
        .to_string(),
    )
    .unwrap();
    let services = Arc::new(FakeServices::default());
    let manager = ExtensionManager::load(
        cwd,
        agent_dir,
        "tui",
        services.clone(),
        true,
        Default::default(),
    )
    .await;
    (manager, services)
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let agent_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    let cwd = tempfile::tempdir().unwrap();
    tack_app::project_trust::set_decision(agent_dir.path(), cwd.path(), true, false);
    (agent_dir, cwd)
}

#[tokio::test]
async fn demo_plugin_full_lifecycle() {
    let _guard = ENV_LOCK.lock().await;
    let (agent_dir, cwd) = setup();

    let (mut manager, services) = load_demo(cwd.path(), agent_dir.path()).await;
    assert_eq!(manager.plugins.len(), 1, "demo plugin should load");
    let plugin = &manager.plugins[0];
    assert!(plugin.is_active(), "plugin must be active: {plugin:?}");
    assert_eq!(plugin.id.to_string(), "demo@user");
    assert_eq!(plugin.version, "local");

    // Tools: registered and sanitized (ext__demo_user__hello_echo).
    let tools = manager.tools();
    assert_eq!(tools.len(), 3);
    let echo = tools
        .iter()
        .find(|t| t.name() == "ext__demo_user__hello_echo")
        .expect("echo tool");

    // Tool execution crosses to the plugin and back.
    let result = echo
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
    assert_eq!(text, "echo: {\"text\":\"hello-ext\"}");

    // Command invocation + its ui/notify side request.
    assert!(manager.command_names().contains(&"hello".to_string()));
    manager.invoke_command("hello", "").await.unwrap();
    let requests = services
        .wait_for_requests("ui/notify", |reqs| {
            reqs.iter().any(|(m, _)| m == "ui/notify")
        })
        .await;
    assert!(
        requests.iter().any(|(m, _)| m == "ui/notify"),
        "plugin's ui/notify should reach the host: {requests:?}"
    );

    // Event fan-out: agentStart triggers the plugin's ui/notify request.
    manager.notify("agentStart", serde_json::json!({})).await;
    let requests = services
        .wait_for_requests("second ui/notify", |reqs| {
            reqs.iter().filter(|(m, _)| m == "ui/notify").count() >= 2
        })
        .await;
    let notify_count = requests.iter().filter(|(m, _)| m == "ui/notify").count();
    assert!(
        notify_count >= 2,
        "agentStart should trigger another notify: {requests:?}"
    );

    // tool_call interception (declared): verdict allow passes through.
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

/// Headless run modes load the same plugins with degraded UI services.
#[tokio::test]
async fn demo_plugin_in_headless_mode() {
    let _guard = ENV_LOCK.lock().await;
    let (agent_dir, cwd) = setup();

    let ext_dir = agent_dir.path().join("extensions").join("demo");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("extension.json"),
        serde_json::json!({
            "name": "demo",
            "command": demo_plugin_bin(),
            "args": [],
        })
        .to_string(),
    )
    .unwrap();
    let services = tack_app::ext_headless::HeadlessExtServices::new("print", true);
    let mut manager = ExtensionManager::load(
        cwd.path(),
        agent_dir.path(),
        "print",
        services,
        true,
        Default::default(),
    )
    .await;
    assert_eq!(manager.plugins.len(), 1, "demo plugin should load headless");

    let tools = manager.tools();
    assert_eq!(tools.len(), 3);
    let echo = tools
        .iter()
        .find(|t| t.name() == "ext__demo_user__hello_echo")
        .expect("echo tool");
    let result = echo
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
    assert_eq!(text, "echo: {\"text\":\"headless\"}");

    manager.invoke_command("hello", "").await.unwrap();
    manager.shutdown().await;
}

/// Widgets land in the host registry keyed `<plugin-id>:<widget-id>`;
/// autocomplete round-trips; widget actions go back to the owning plugin.
#[tokio::test]
async fn demo_plugin_widgets_and_autocomplete() {
    let _guard = ENV_LOCK.lock().await;
    let (agent_dir, cwd) = setup();

    let (mut manager, services) = load_demo(cwd.path(), agent_dir.path()).await;
    assert_eq!(manager.plugins.len(), 1, "demo plugin should load");

    let keys: Vec<&str> = manager.widgets().iter().map(|w| w.key.as_str()).collect();
    assert_eq!(keys, vec!["demo@user:demo-status", "demo@user:demo-list"]);
    let status = &manager.widgets()[0];
    assert_eq!(
        status.spec.r#type,
        tack_ext::rpc3::WidgetKind::StatusLineSegment
    );
    assert_eq!(
        status.state,
        Some(serde_json::json!({"text": "demo:ok", "style": "info"}))
    );
    assert!(manager.widgets()[1].visible, "panel defaults to visible");

    // widgets/update via the manager: full-state replacement; unknown ids
    // tolerated.
    let update = tack_ext::rpc3::WidgetUpdateParams {
        id: "demo-status".to_string(),
        state: serde_json::json!({"text": "demo:busy", "style": "warning"}),
        visible: None,
    };
    assert!(manager.apply_widget_update("demo@user", &update));
    assert_eq!(
        manager.widgets()[0].state,
        Some(serde_json::json!({"text": "demo:busy", "style": "warning"}))
    );
    assert!(!manager.apply_widget_update(
        "demo@user",
        &tack_ext::rpc3::WidgetUpdateParams {
            id: "nope".to_string(),
            state: serde_json::json!({}),
            visible: None,
        }
    ));

    // Providers listed in register order; provide round-trips.
    let providers = manager.autocomplete_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].key, "demo@user:hash");
    assert_eq!(providers[0].spec.trigger, "#");
    let suggestions = manager
        .autocomplete_provide("demo@user:hash", "wa", 2)
        .await;
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].value, "#wasm");
    assert!(
        manager
            .autocomplete_provide("demo@user:nope", "x", 0)
            .await
            .is_empty()
    );

    // widget actions are delivered to the owning plugin; the demo plugin
    // echoes the action back as a ui/notify request.
    manager
        .notify_widget_action(
            "demo@user",
            tack_ext::rpc3::WidgetActionParams {
                id: "demo-list".to_string(),
                action: "select".to_string(),
                item_id: Some("b".to_string()),
            },
        )
        .await;
    let requests = services
        .wait_for_requests("widget action echo", |reqs| {
            reqs.iter().any(|(m, p)| {
                m == "ui/notify"
                    && p.get("message")
                        .and_then(Value::as_str)
                        .is_some_and(|msg| msg.contains("widget.action select b"))
            })
        })
        .await;
    assert!(
        requests.iter().any(|(m, _)| m == "ui/notify"),
        "widget action echo should reach the host: {requests:?}"
    );

    // Plugin death cleanup: after shutdown the registry can be drained.
    manager.shutdown().await;
    let removed = manager.remove_plugin_widgets("demo@user");
    assert_eq!(removed.len(), 2);
    assert!(manager.widgets().is_empty());
}
