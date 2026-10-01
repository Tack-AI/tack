//! End-to-end: a REAL plugin (the v3 demo fixture) behind the remote-mode
//! host services + ext bridge. Plugin commands (and their re-entrant
//! `ui/notify`), the `ui/select` dialog round trip, declarative widgets
//! and autocomplete all cross the bridge — the same wiring `tack serve`
//! uses. No sessions are created, so no process-global env is touched.
#![cfg(feature = "ext")]
#![allow(clippy::unwrap_used)]

use tack_app::extension_host::ExtensionManager;
use tack_app::remote::ext_bridge::RemoteExtBridge;

/// Path of the built v3 demo-plugin bin (fixture), derived from the test
/// binary location (…/target/<profile>/deps/…). Absent on scoped test
/// runs that didn't build the workspace — the test skips then.
fn demo_plugin_bin() -> Option<String> {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // profile/
    path.push(format!(
        "tack-v3-demo-plugin{}",
        std::env::consts::EXE_SUFFIX
    ));
    path.is_file().then(|| path.to_string_lossy().to_string())
}

async fn load_demo_remote(
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    bridge: std::sync::Arc<RemoteExtBridge>,
) -> ExtensionManager {
    let bin = demo_plugin_bin().expect("demo plugin bin");
    let ext_dir = agent_dir.join("extensions").join("demo");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("extension.json"),
        serde_json::json!({
            "name": "demo",
            "command": bin,
            "args": [],
        })
        .to_string(),
    )
    .unwrap();
    let bridge_state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    // The exact serve() wiring (remote mode): plugin UI crosses the
    // bridge instead of degrading.
    let services = tack_app::ext_headless::HeadlessExtServices::new_with_remote(
        "remote",
        true,
        bridge_state.clone(),
        bridge,
    );
    ExtensionManager::load(
        cwd,
        agent_dir,
        "remote",
        services,
        false,
        Default::default(),
        bridge_state,
    )
    .await
}

/// Wait until the bridge parks a dialog and return its request id.
async fn wait_for_parked_dialog(bridge: &RemoteExtBridge) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(id) = bridge.pending_dialog_ids().into_iter().next() {
            return id;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the plugin's dialog never parked"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn demo_plugin_surfaces_cross_the_remote_bridge() {
    if demo_plugin_bin().is_none() {
        eprintln!("demo plugin bin not built; skipping");
        return;
    }
    let agent_dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let bridge = RemoteExtBridge::new();
    // One fake dialog-capable client (no host attach: events drop, but
    // park/answer works — pending_dialog_ids is the observation seam).
    bridge.client_connected(true);
    let mut manager = load_demo_remote(cwd.path(), agent_dir.path(), bridge.clone()).await;
    assert_eq!(manager.plugins.len(), 1, "demo plugin must load");
    assert!(manager.plugins[0].is_active(), "{:?}", manager.plugins[0]);

    // Slash commands with their contributed descriptions.
    let specs = manager.command_specs();
    assert!(
        specs
            .iter()
            .any(|c| c.name == "hello" && c.description.as_deref() == Some("Say hello")),
        "{specs:?}"
    );
    // Invocation: the demo's handler calls ui/notify mid-invoke, which
    // the remote services answer inline (log path).
    let result = manager.invoke_command("hello", "").await.unwrap();
    assert_eq!(result, serde_json::json!({"ok": true}));

    // Declarative widgets registered at load; actions route to the
    // owning plugin (its on_widget_action fires a ui/notify).
    let keys: Vec<String> = manager.widgets().iter().map(|w| w.key.clone()).collect();
    assert!(
        keys.contains(&"demo@user:demo-status".to_string()),
        "{keys:?}"
    );
    assert!(
        keys.contains(&"demo@user:demo-list".to_string()),
        "{keys:?}"
    );
    assert!(manager.widget_action_route("ghost:nope").is_none());
    let route = manager
        .widget_action_route("demo@user:demo-list")
        .expect("action route for a live widget");
    route
        .notify("select".to_string(), Some("a".to_string()))
        .await;

    // Autocomplete provider, protocol-converted suggestions.
    let providers = manager.autocomplete_providers();
    let hash = providers
        .iter()
        .find(|p| p.key() == "demo@user:hash")
        .expect("hash provider");
    let info = hash.to_protocol();
    assert_eq!(info.trigger, "#");
    let suggestions = hash.provide_protocol("#al", 3).await;
    assert!(
        suggestions.iter().any(|s| s.value == "#alpha"),
        "{suggestions:?}"
    );
    assert!(
        !suggestions.iter().any(|s| s.value == "#wasm"),
        "query must filter: {suggestions:?}"
    );

    // The ui/select dialog round trip through the plugin TOOL: the
    // execution parks on the bridge until a client answers, then the
    // tool result carries the picked option.
    let tools = manager.tools();
    let select = tools
        .iter()
        .find(|t| t.name() == "ext__demo_user__hello_select")
        .expect("hello.select tool")
        .clone();
    let execution = tokio::spawn(async move {
        select
            .execute(
                "call-1",
                serde_json::json!({}),
                tokio_util::sync::CancellationToken::new(),
                &|_| {},
            )
            .await
    });
    let request_id = wait_for_parked_dialog(&bridge).await;
    assert!(bridge.answer_dialog(&request_id, false, Some(serde_json::json!("b"))));
    let result = execution.await.unwrap().unwrap();
    let text: String = result
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "picked: b", "{text}");

    // The cancel path maps to the plugin seeing no selection.
    let select = manager
        .tools()
        .into_iter()
        .find(|t| t.name() == "ext__demo_user__hello_select")
        .expect("hello.select tool");
    let execution = tokio::spawn(async move {
        select
            .execute(
                "call-2",
                serde_json::json!({}),
                tokio_util::sync::CancellationToken::new(),
                &|_| {},
            )
            .await
    });
    let request_id = wait_for_parked_dialog(&bridge).await;
    assert!(bridge.answer_dialog(&request_id, true, None));
    let result = execution.await.unwrap().unwrap();
    let text: String = result
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "picked: ", "{text}");

    // Losing the last dialog-capable client fails the NEXT dialog fast.
    bridge.client_disconnected(true);
    let select = manager
        .tools()
        .into_iter()
        .find(|t| t.name() == "ext__demo_user__hello_select")
        .expect("hello.select tool");
    let result = select
        .execute(
            "call-3",
            serde_json::json!({}),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await;
    assert!(
        result.is_err(),
        "no answerers: the tool call must fail, got {result:?}"
    );

    manager.shutdown().await;
}
