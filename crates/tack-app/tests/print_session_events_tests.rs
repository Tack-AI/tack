//! JSON print session-event ordering with a real lifecycle extension.
#![cfg(feature = "ext")]
#![allow(clippy::unwrap_used)]

use std::process::{Command, Stdio};

fn demo_plugin_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
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

#[test]
fn json_print_writes_header_before_session_start_entry_and_agent_events() {
    let agent_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        agent_dir.path().join("settings.json"),
        r#"{"updateCheck": false}"#,
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
            "env": {"TACK_DEMO_SESSION_MARKERS": "1"},
        })
        .to_string(),
    )
    .unwrap();

    // The prompt must terminate without a real provider/network. CodeBuddy
    // accepts an arbitrary model id and this non-executable stub fails at
    // spawn, after the startup lifecycle and agent_start have been emitted.
    let stub = work_dir.path().join("codebuddy-stub");
    std::fs::write(&stub, "not an executable\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tack"))
        .args([
            "--mode",
            "json",
            "--provider",
            "codebuddy",
            "--model",
            "some-model",
            "-p",
            "hello",
        ])
        .env("TACK_AGENT_DIR", agent_dir.path())
        .env("CODEBUDDY_PATH", &stub)
        .env("TACK_OFFLINE", "1")
        .current_dir(work_dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            // The provider failure is intentional; startup event ordering is
            // the assertion target, not a successful inference response.
            let _ = status;
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "print mode timed out\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("invalid JSONL ({error}): {line:?}\nstderr:\n{stderr}")
            })
        })
        .collect();

    assert!(lines.len() >= 3, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(lines[0]["type"], "session", "{lines:?}");
    assert!(
        matches!(lines[0]["version"].as_u64(), Some(3 | 4)),
        "{lines:?}"
    );
    assert!(lines[0]["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(
        lines[1]["type"], "entry_appended",
        "{lines:?}\nstderr:\n{stderr}"
    );
    assert_eq!(lines[1]["entry"]["customType"], "start-marker");
    assert_eq!(lines[1]["entry"]["data"], "startup");
    assert_eq!(lines[2]["type"], "agent_start", "{lines:?}");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line["entry"]["customType"] == "start-marker")
            .count(),
        1,
        "duplicate startup marker: {lines:?}"
    );

    // Persistence moved into SessionHooks before message_end. The event
    // consumer must not append a second copy.
    let session_dir = tack_session::default_session_dir(
        &work_dir.path().canonicalize().unwrap(),
        agent_dir.path(),
    );
    let sessions = tack_session::list_sessions(&session_dir);
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let session = tack_session::SessionManager::open(&sessions[0].path, None).unwrap();
    let entries = session.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.type_name() == "custom")
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.type_name() == "message")
            .filter(|entry| {
                matches!(
                    entry,
                    tack_session::SessionEntry::Message {
                        message: tack_agent_core::AgentMessage::Assistant(_),
                        ..
                    }
                )
            })
            .count(),
        1,
        "assistant was persisted more than once: {entries:?}"
    );
}
