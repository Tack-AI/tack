//! Extension slash-commands dispatched from the TUI must never be
//! awaited inline on the UI loop: a plugin whose handler calls back
//! into the host (ui/notify, ui/select, …) is answered by that very
//! loop, so an inline await deadlocks until the 30s request timeout
//! (the loop never pumps the plugin's `ExtUiRequest`). The dispatcher
//! spawns the invoke and lands the result as `AppEvent::ExtCommandResult`.
#![cfg(feature = "ext")]
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use std::sync::Arc;

use tack_app::tui::{TuiApp, TuiOptions};

/// Path of the built v3 demo-plugin bin (fixture), derived from the test
/// binary location (…/target/<profile>/deps/…) — same helper as
/// extension_tests.rs.
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

/// The demo plugin's `/hello` handler answers with a ui/notify host
/// call. Before the off-loop dispatch fix this test froze for the full
/// 30s request timeout and surfaced "request timed out" instead.
#[tokio::test]
async fn ext_command_with_host_callback_does_not_deadlock() {
    let agent_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    unsafe { std::env::set_var("TACK_OFFLINE", "1") };
    std::fs::write(
        agent_dir.path().join("settings.json"),
        r#"{"updateCheck": false, "notifications": false}"#,
    )
    .unwrap();
    std::fs::write(
        agent_dir.path().join("auth.json"),
        r#"{"anthropic": {"type": "api_key", "key": "test-key"}}"#,
    )
    .unwrap();
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
    // Leak: the agent dir must outlive the app.
    std::mem::forget(agent_dir);

    let cwd = tempfile::tempdir().unwrap();
    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    let mut app = TuiApp::new(TuiOptions {
        model,
        auth: Arc::new(tack_ai::oauth::StaticAuth::from(Some(
            "test-key".to_string(),
        ))),
        thinking: None,
        cwd: cwd.path().to_path_buf(),
        continue_session: false,
        system_prompt: None,
        session_dir: None,
        flags: tack_app::cli_flags::CliFlags::default(),
    })
    .await
    .unwrap();
    app.test_close_dialog();

    // Exactly what the main loop does on Enter — must return at once,
    // not after the 30s request timeout.
    let start = std::time::Instant::now();
    app.on_submit("/hello".to_string()).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "extension command dispatch blocked the UI loop for {elapsed:?}"
    );

    // The spawned invoke and the plugin's ui/notify land as app events;
    // pump until the notice renders (the plugin round-trip is async).
    let mut frame = String::new();
    for _ in 0..500 {
        app.test_pump_events().await;
        let mut out = Vec::new();
        app.render(&mut out).unwrap();
        frame = String::from_utf8_lossy(&out).into_owned();
        if frame.contains("hello from the demo plugin!") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        frame.contains("hello from the demo plugin!"),
        "plugin ui/notify missing from transcript: {frame:?}"
    );
    assert!(
        !frame.contains("failed"),
        "extension command should not fail: {frame:?}"
    );
}
