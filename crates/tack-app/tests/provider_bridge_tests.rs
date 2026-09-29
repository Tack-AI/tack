//! Provider bridge integration tests (P7): the demo plugin serves a
//! deterministic fake model through the full stack — registration,
//! resolution, streaming, cancel, carrier death, policy, and provider
//! events.
#![cfg(feature = "ext")]
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

/// The tests below mutate the process-global `TACK_AGENT_DIR` (and the
/// process-global provider registries, keyed by unique provider ids) —
/// run them under a shared lock.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tack_ai::provider::StreamOptions;
use tack_ai::stream::AssistantMessageEvent;
use tack_ai::types::{Context, StopReason};
use tack_app::extension_host::ExtensionManager;
use tokio_util::sync::CancellationToken;

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

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let agent_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    let cwd = tempfile::tempdir().unwrap();
    tack_app::project_trust::set_decision(agent_dir.path(), cwd.path(), true, false);
    (agent_dir, cwd)
}

/// Load the demo plugin as a provider bridge (TACK_DEMO_PROVIDER spec +
/// optional stream delay) in headless print mode with the production
/// services wiring.
async fn load_provider_demo(
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    provider_id: &str,
    delay_ms: Option<u64>,
) -> (
    ExtensionManager,
    Arc<tack_app::ext_provider_bridge::ProviderBridgeState>,
) {
    let mut env = serde_json::Map::new();
    env.insert(
        "TACK_DEMO_PROVIDER".to_string(),
        json!({"id": provider_id, "models": [{"id": "fake-1", "contextWindow": 128000, "maxTokens": 4096}]})
            .to_string()
            .into(),
    );
    if let Some(delay_ms) = delay_ms {
        env.insert(
            "TACK_DEMO_PROVIDER_DELAY_MS".to_string(),
            delay_ms.to_string().into(),
        );
    }
    let ext_dir = agent_dir.join("extensions").join("demo");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("extension.json"),
        serde_json::json!({
            "name": "demo",
            "command": demo_plugin_bin(),
            "args": [],
            "env": env,
        })
        .to_string(),
    )
    .unwrap();
    let bridge_state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    let manager = ExtensionManager::load(
        cwd,
        agent_dir,
        "print",
        tack_app::ext_headless::HeadlessExtServices::new("print", true, bridge_state.clone()),
        true,
        Default::default(),
        bridge_state.clone(),
    )
    .await;
    (manager, bridge_state)
}

/// Wait until the plugin's on_ready registration lands in the
/// process-global runtime registry.
async fn wait_registered(provider_id: &str) {
    for _ in 0..250 {
        if tack_ai::providers::runtime_providers()
            .iter()
            .any(|p| p.id == provider_id)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("provider {provider_id} was not registered in time");
}

async fn cleanup(provider_id: &str) {
    tack_ai::providers::unregister_runtime_provider(provider_id);
    tack_ai::unregister_provider_bridge(provider_id);
}

fn context_with(text: &str) -> Context {
    Context {
        system_prompt: None,
        messages: vec![tack_ai::types::Message::user(text)],
        tools: vec![],
    }
}

/// Collect a provider stream to its terminal event.
async fn collect(
    mut stream: tack_ai::stream::AssistantMessageEventStream,
) -> (Vec<AssistantMessageEvent>, tack_ai::types::AssistantMessage) {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let terminal = event.is_terminal();
        events.push(event);
        if terminal {
            break;
        }
    }
    let message = stream.result().await;
    (events, message)
}

#[tokio::test]
async fn bridge_provider_streams_end_to_end() {
    let _guard = ENV_LOCK.lock().await;
    let (agent_dir, cwd) = setup();
    let provider_id = "demo-bridge-e2e";
    let (mut manager, _state) =
        load_provider_demo(cwd.path(), agent_dir.path(), provider_id, None).await;
    assert_eq!(manager.plugins.len(), 1, "{:?}", manager.plugins);
    assert!(
        manager.plugins[0].is_active(),
        "{:?}",
        manager.plugins[0].error
    );
    wait_registered(provider_id).await;

    // The bridged model resolves with the reserved api kind and a working
    // Provider — native UX parity in headless mode.
    let model = tack_app::model::resolve_model(provider_id, Some("fake-1"), agent_dir.path())
        .expect("bridged model resolves");
    assert_eq!(model.api, tack_ai::EXT_PROVIDER_BRIDGE_API);
    let provider = tack_ai::provider_for(&model).expect("bridged provider");
    let (events, message) = collect(provider.stream(
        &model,
        &context_with("hello world"),
        StreamOptions::default(),
    ))
    .await;
    let kinds: Vec<String> = events
        .iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(
        kinds.contains(&"thinkingDelta".to_string()) && kinds.contains(&"textDelta".to_string()),
        "full event flow: {kinds:?}"
    );
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert!(
        message.text().contains("echo: hello world"),
        "{}",
        message.text()
    );
    assert_eq!(message.usage.input, 10, "usage passes through");

    manager.shutdown().await;
    cleanup(provider_id).await;
}

#[tokio::test]
async fn bridge_registration_requires_declared_capability() {
    let _guard = ENV_LOCK.lock().await;
    let (agent_dir, cwd) = setup();
    // The demo plugin WITHOUT TACK_DEMO_PROVIDER does not declare
    // provider.stream.
    let ext_dir = agent_dir.path().join("extensions").join("demo");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("extension.json"),
        serde_json::json!({"name": "demo", "command": demo_plugin_bin(), "args": []}).to_string(),
    )
    .unwrap();
    let bridge_state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    let mut manager = ExtensionManager::load(
        cwd.path(),
        agent_dir.path(),
        "print",
        tack_app::ext_headless::HeadlessExtServices::new("print", true, bridge_state.clone()),
        true,
        Default::default(),
        bridge_state.clone(),
    )
    .await;
    assert!(
        manager.plugins[0].is_active(),
        "{:?}",
        manager.plugins[0].error
    );
    let plugin_id = manager.plugins[0].id.to_string();
    let err = tack_app::ext_provider_bridge::handle_register_provider(
        &bridge_state,
        json!({
            "plugin": plugin_id,
            "provider": {"id": "cap-gated", "bridge": true, "models": [{"id": "m"}]}
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.code,
        tack_ext::rpc3::ERR_CAPABILITY_NOT_GRANTED,
        "{err:?}"
    );
    assert!(err.message.contains("provider.stream"), "{err:?}");
    manager.shutdown().await;
}

#[tokio::test]
async fn cancel_aborts_a_slow_stream() {
    let _guard = ENV_LOCK.lock().await;
    let (agent_dir, cwd) = setup();
    let provider_id = "demo-bridge-cancel";
    let (mut manager, _state) =
        load_provider_demo(cwd.path(), agent_dir.path(), provider_id, Some(300)).await;
    wait_registered(provider_id).await;
    let model =
        tack_app::model::resolve_model(provider_id, Some("fake-1"), agent_dir.path()).unwrap();
    let provider = tack_ai::provider_for(&model).unwrap();
    let cancel = CancellationToken::new();
    let stream = provider.stream(
        &model,
        &context_with("take your time"),
        StreamOptions {
            cancel: cancel.clone(),
            ..Default::default()
        },
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let started = std::time::Instant::now();
    let (_events, message) = collect(stream).await;
    assert_eq!(message.stop_reason, StopReason::Aborted, "{message:?}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "cancel must terminate the stream promptly (plugin abort or grace)"
    );
    manager.shutdown().await;
    cleanup(provider_id).await;
}

#[tokio::test]
async fn carrier_death_fails_streams_and_unregisters() {
    let _guard = ENV_LOCK.lock().await;
    let (agent_dir, cwd) = setup();
    let provider_id = "demo-bridge-death";
    let (mut manager, _state) =
        load_provider_demo(cwd.path(), agent_dir.path(), provider_id, Some(500)).await;
    wait_registered(provider_id).await;
    let model =
        tack_app::model::resolve_model(provider_id, Some("fake-1"), agent_dir.path()).unwrap();
    let provider = tack_ai::provider_for(&model).unwrap();
    let stream = provider.stream(&model, &context_with("slow burn"), StreamOptions::default());
    // Kill the carrier mid-stream (graceful shutdown still EOFs the conn).
    manager.shutdown().await;
    let (_events, message) = collect(stream).await;
    assert_eq!(message.stop_reason, StopReason::Error, "{message:?}");
    // The dead plugin registered nothing: both the models and the serving
    // endpoint are gone (load-outcome semantics).
    for _ in 0..100 {
        let models_gone = !tack_ai::providers::runtime_providers()
            .iter()
            .any(|p| p.id == provider_id);
        let bridge_gone = tack_ai::provider_bridge(provider_id).is_none();
        if models_gone && bridge_gone {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("dead plugin's provider was not unregistered");
}

#[tokio::test]
async fn managed_policy_blocks_provider_serving() {
    let _guard = ENV_LOCK.lock().await;
    let (agent_dir, cwd) = setup();
    let provider_id = "demo-bridge-policy";
    // Learn the plugin id with a plain load, then reload under a managed
    // provider deny.
    let (mut manager, _state) =
        load_provider_demo(cwd.path(), agent_dir.path(), provider_id, None).await;
    let plugin_id = manager.plugins[0].id.to_string();
    manager.shutdown().await;
    tack_ai::providers::unregister_runtime_provider(provider_id);
    tack_ai::unregister_provider_bridge(provider_id);

    let policy = tack_app::plugin_policy::PluginPolicy::from_raw(
        &json!({"pluginPolicy": {"plugins": {plugin_id: {"provider": false}}}}),
        "test-managed".to_string(),
    )
    .expect("policy parses");
    let bridge_state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    let mut manager = ExtensionManager::load_with_policy(
        cwd.path(),
        agent_dir.path(),
        "print",
        tack_app::ext_headless::HeadlessExtServices::new("print", true, bridge_state.clone()),
        true,
        Default::default(),
        Some(policy),
        bridge_state,
    )
    .await;
    assert!(
        manager.plugins[0].policy_block.is_some(),
        "provider-denied plugin is policy-blocked: {:?}",
        manager.plugins[0]
    );
    // Nothing registered: the policy-blocked plugin never served.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !tack_ai::providers::runtime_providers()
            .iter()
            .any(|p| p.id == provider_id),
        "policy-blocked provider must not register"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn provider_event_surfaces_on_the_event_channel() {
    let _guard = ENV_LOCK.lock().await;
    let (agent_dir, cwd) = setup();
    let provider_id = "demo-bridge-events";
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tack_ai::set_provider_event_notifier(Some(Arc::new(move |event| {
        let _ = tx.send(event);
    })));
    let (mut manager, _state) =
        load_provider_demo(cwd.path(), agent_dir.path(), provider_id, None).await;
    wait_registered(provider_id).await;
    let model =
        tack_app::model::resolve_model(provider_id, Some("fake-1"), agent_dir.path()).unwrap();
    let provider = tack_ai::provider_for(&model).unwrap();
    let (_events, message) = collect(provider.stream(
        &model,
        &context_with("rate-limit"),
        StreamOptions::default(),
    ))
    .await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("provider event arrived")
        .expect("channel open");
    assert_eq!(event.kind, tack_ai::ProviderEventKind::RateLimited);
    assert_eq!(event.provider, provider_id);
    assert!(event.message.contains("rate limit"));
    tack_ai::set_provider_event_notifier(None);
    manager.shutdown().await;
    cleanup(provider_id).await;
}

// ---------------------------------------------------------------------------
// Plain (non-bridge) registration gates — in-memory duplex fixtures (the
// connection exists for attribution, capability, and death-watch liveness;
// nothing crosses the wire).
// ---------------------------------------------------------------------------

/// A silent peer side for the in-memory duplex.
struct SilentPeer;

#[async_trait::async_trait]
impl tack_ext::v3::PeerHandler for SilentPeer {}

/// A live host-side connection over an in-memory duplex, plus the raw
/// plugin-side stream: dropping it kills the carrier (host-side EOF).
fn duplex_connection() -> (
    Arc<dyn tack_ext::v3::PluginConnection>,
    tokio::io::DuplexStream,
) {
    let (host_side, plugin_side) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(host_side);
    let host_peer = tack_ext::v3::JsonRpcPeer::new(reader, writer, Arc::new(SilentPeer));
    (
        Arc::new(tack_ext::v3::HostClient::new(host_peer)),
        plugin_side,
    )
}

fn plain_spec(id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "baseUrl": "http://localhost:9/v1",
        "api": "openai-completions",
        "models": [{"id": "m1"}]
    })
}

fn is_registered(provider_id: &str) -> bool {
    tack_ai::providers::runtime_providers()
        .iter()
        .any(|p| p.id == provider_id)
}

/// A plain registration needs plugin attribution, a live connection, and
/// the declared `capabilities.provider.register` — the same gate posture
/// as the bridge path.
#[tokio::test]
async fn plain_registration_requires_attribution_connection_and_capability() {
    let _guard = ENV_LOCK.lock().await;
    let state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    // No attribution (TaggedServices injects `plugin` for real traffic).
    let err = tack_app::ext_provider_bridge::handle_register_provider(
        &state,
        json!({"provider": plain_spec("plain-noattr")}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, tack_ext::rpc3::ERR_INTERNAL, "{err:?}");
    // Attribution but no live connection.
    let err = tack_app::ext_provider_bridge::handle_register_provider(
        &state,
        json!({"plugin": "ghost@user", "provider": plain_spec("plain-ghost")}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, tack_ext::rpc3::ERR_PLUGIN_UNAVAILABLE, "{err:?}");
    // Live connection, capability not granted (undeclared).
    let (conn, _plugin_side) = duplex_connection();
    state.register_connection("plug@user", conn, false);
    state.set_provider_register_granted("plug@user", false);
    let err = tack_app::ext_provider_bridge::handle_register_provider(
        &state,
        json!({"plugin": "plug@user", "provider": plain_spec("plain-denied")}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.code,
        tack_ext::rpc3::ERR_CAPABILITY_NOT_GRANTED,
        "{err:?}"
    );
    assert!(err.message.contains("provider.register"), "{err:?}");
    assert!(!is_registered("plain-denied"));
}

/// With the capability granted, a plain registration lands in the
/// process-global runtime registry and resolves like a models.json custom.
#[tokio::test]
async fn plain_registration_with_capability_registers_and_resolves() {
    let _guard = ENV_LOCK.lock().await;
    let provider_id = "plain-granted";
    let state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    let (conn, _plugin_side) = duplex_connection();
    state.register_connection("plug@user", conn, false);
    state.set_provider_register_granted("plug@user", true);
    tack_app::ext_provider_bridge::handle_register_provider(
        &state,
        json!({"plugin": "plug@user", "provider": plain_spec(provider_id)}),
    )
    .await
    .unwrap();
    assert!(is_registered(provider_id));
    let agent_dir = tempfile::tempdir().unwrap();
    let model = tack_app::model::resolve_model(provider_id, Some("m1"), agent_dir.path())
        .expect("plain-registered model resolves");
    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.base_url, "http://localhost:9/v1");
    cleanup(provider_id).await;
}

/// A registration that arrives during the plugin's own initialize
/// handshake waits (fail-closed) for the capability answer; when the load
/// loop then policy-blocks the plugin, the pending registration is
/// rejected and nothing stays registered.
#[tokio::test]
async fn plain_registration_during_init_is_rejected_when_policy_blocked() {
    let _guard = ENV_LOCK.lock().await;
    let provider_id = "plain-initblock";
    let state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    let (conn, _plugin_side) = duplex_connection();
    state.register_connection("plug@user", conn, false);
    // The registration races the handshake: no capability answer yet.
    let pending = {
        let state = state.clone();
        tokio::spawn(async move {
            tack_app::ext_provider_bridge::handle_register_provider(
                &state,
                json!({"plugin": "plug@user", "provider": plain_spec(provider_id)}),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The load loop's policy-block sequence (declares provider.* while
    // managed policy denies it): grant nothing, withdraw the connection.
    state.set_provider_stream_granted("plug@user", false);
    state.set_provider_register_granted("plug@user", false);
    state.remove_connection("plug@user");
    let err = pending.await.unwrap().unwrap_err();
    assert_eq!(
        err.code,
        tack_ext::rpc3::ERR_CAPABILITY_NOT_GRANTED,
        "{err:?}"
    );
    assert!(!is_registered(provider_id));
}

/// A plugin that registered plain providers and then dies gets them
/// unregistered — load-outcome semantics, same as bridged providers.
#[tokio::test]
async fn plugin_death_unregisters_plain_providers() {
    let _guard = ENV_LOCK.lock().await;
    let provider_id = "plain-death";
    let state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    let (conn, plugin_side) = duplex_connection();
    state.register_connection("plug@user", conn, false);
    state.set_provider_register_granted("plug@user", true);
    tack_app::ext_provider_bridge::handle_register_provider(
        &state,
        json!({"plugin": "plug@user", "provider": plain_spec(provider_id)}),
    )
    .await
    .unwrap();
    assert!(is_registered(provider_id));
    // Carrier death: the plugin side of the duplex closes.
    drop(plugin_side);
    for _ in 0..100 {
        if !is_registered(provider_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("dead plugin's plain provider was not unregistered");
}

/// Runtime providers must not shadow a built-in id (they would inherit
/// the user's stored credentials for the built-in while steering requests
/// to a plugin-chosen endpoint).
#[tokio::test]
async fn registration_colliding_with_a_builtin_id_is_rejected() {
    let _guard = ENV_LOCK.lock().await;
    let state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    let (conn, _plugin_side) = duplex_connection();
    state.register_connection("plug@user", conn, true);
    state.set_provider_register_granted("plug@user", true);
    state.set_provider_stream_granted("plug@user", true);
    // Plain spec.
    let err = tack_app::ext_provider_bridge::handle_register_provider(
        &state,
        json!({"plugin": "plug@user", "provider": plain_spec("anthropic")}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, tack_ext::rpc3::ERR_INVALID_PARAMS, "{err:?}");
    assert!(err.message.contains("anthropic"), "{err:?}");
    // Bridge spec: equally rejected.
    let err = tack_app::ext_provider_bridge::handle_register_provider(
        &state,
        json!({"plugin": "plug@user", "provider": {"id": "anthropic", "bridge": true, "models": [{"id": "m1"}]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, tack_ext::rpc3::ERR_INVALID_PARAMS, "{err:?}");
    assert!(err.message.contains("anthropic"), "{err:?}");
    assert!(!is_registered("anthropic"));
}

/// `provider/event` forgery guard: events for a provider the emitting
/// plugin did not register are dropped; events for its own provider ride
/// the channel.
#[tokio::test]
async fn provider_event_requires_plugin_ownership() {
    let _guard = ENV_LOCK.lock().await;
    let provider_id = "plain-events";
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tack_ai::set_provider_event_notifier(Some(Arc::new(move |event| {
        let _ = tx.send(event);
    })));
    let state = tack_app::ext_provider_bridge::ProviderBridgeState::shared();
    let (conn, _plugin_side) = duplex_connection();
    state.register_connection("plug@user", conn, false);
    state.set_provider_register_granted("plug@user", true);
    tack_app::ext_provider_bridge::handle_register_provider(
        &state,
        json!({"plugin": "plug@user", "provider": plain_spec(provider_id)}),
    )
    .await
    .unwrap();
    // Spoofing a built-in provider id: dropped.
    state.route_provider_event(
        "plug@user",
        json!({"provider": "anthropic", "kind": "rateLimited", "message": "forged"}),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "forged provider/event must not reach the channel"
    );
    // Another plugin emitting for THIS plugin's provider: dropped.
    state.route_provider_event(
        "other@user",
        json!({"provider": provider_id, "kind": "warning", "message": "forged"}),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "provider/event from a non-owner must not reach the channel"
    );
    // The owning plugin's event: forwarded.
    state.route_provider_event(
        "plug@user",
        json!({"provider": provider_id, "kind": "warning", "message": "heads up"}),
    );
    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("own provider event arrived")
        .expect("channel open");
    assert_eq!(event.provider, provider_id);
    assert_eq!(event.kind, tack_ai::ProviderEventKind::Warning);
    tack_ai::set_provider_event_notifier(None);
    cleanup(provider_id).await;
}
