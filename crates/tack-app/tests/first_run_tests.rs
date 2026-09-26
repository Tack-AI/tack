//! First-run wizard test in its own binary: TACK_AGENT_DIR is process-global,
//! and the parallel tests in tui_tests.rs race it (each writes a settings.json
//! that would suppress the wizard). Alone in this process it is deterministic.
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use std::sync::Arc;

use tack_app::tui::{TuiApp, TuiOptions};

#[tokio::test]
async fn first_run_wizard_opens_and_saves_theme() {
    use tack_tui::{InputEvent, Key, KeyEvent};
    let cwd = tempfile::tempdir().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();
    // Fresh profile: NO settings.json → the first-run wizard must appear.
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    // Offline: the startup update check must not touch the network (a
    // fresh profile defaults it on).
    unsafe { std::env::set_var("TACK_OFFLINE", "1") };

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

    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(
        frame.contains("Welcome to tack"),
        "wizard missing:\n{frame}"
    );
    assert!(
        frame.contains("detected system appearance"),
        "detection hint missing:\n{frame}"
    );

    // Enter accepts the preselected theme → persisted to settings.json.
    app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Enter)))
        .await;
    let settings = std::fs::read_to_string(agent_dir.path().join("settings.json"))
        .expect("wizard selection must create settings.json");
    assert!(
        settings.contains("\"theme\""),
        "theme not saved: {settings}"
    );

    // Next launch with an existing settings.json must NOT show the wizard.
    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    let mut app2 = TuiApp::new(TuiOptions {
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
    let mut out2 = Vec::new();
    app2.render(&mut out2).unwrap();
    let frame2 = String::from_utf8_lossy(&out2);
    assert!(
        !frame2.contains("Welcome to tack"),
        "wizard re-shown:\n{frame2}"
    );
}
