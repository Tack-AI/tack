//! RPC-mode session hooks over the real stdio JSONL endpoint:
//! `set_hooks` + SessionStart/UserPromptSubmit/Stop firing. Hermetic: no
//! network, no LLM — the observed behavior happens before/after the agent
//! loop (blocked prompts never reach the provider; SessionStart/Stop hooks
//! are file side effects around a provider-less turn that errors out).
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use tack_tools::shell::shell_quote;

/// These tests spawn full stdio agents (and, for lifecycle traces, another
/// plugin process). Parallel startup/teardown is flaky in local sandboxes and
/// CI; holding one guard per agent keeps the process group bounded without
/// changing production behavior.
static RPC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct RpcAgent {
    _serial: std::sync::MutexGuard<'static, ()>,
    child: Child,
    stdin: ChildStdin,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    agent_dir: std::path::PathBuf,
    work_dir: std::path::PathBuf,
}

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

fn spawn_rpc() -> RpcAgent {
    spawn_rpc_with_markers(false)
}

fn spawn_rpc_with_markers(markers: bool) -> RpcAgent {
    let serial = RPC_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let agent_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    if markers {
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
    }
    let child = Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(["--mode", "rpc"])
        .env("TACK_AGENT_DIR", agent_dir.path())
        .current_dir(work_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if markers {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .spawn()
        .unwrap();
    let dir = agent_dir.path().to_path_buf();
    let work = work_dir.path().to_path_buf();
    // Leak both tempdirs so they outlive the child processes.
    std::mem::forget(agent_dir);
    std::mem::forget(work_dir);
    let mut child = child;
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    RpcAgent {
        _serial: serial,
        child,
        stdin,
        stdout: Some(stdout),
        agent_dir: dir,
        work_dir: work,
    }
}

impl Drop for RpcAgent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.agent_dir);
        let _ = std::fs::remove_dir_all(&self.work_dir);
    }
}

fn send(stdin: &mut ChildStdin, value: serde_json::Value) {
    let mut line = serde_json::to_string(&value).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
}

/// Read one stdout line with a bounded wait (wedged agents fail the test
/// instead of hanging CI).
fn read_line(agent: &mut RpcAgent) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut stdout = agent.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = stdout.read_line(&mut line).map(|_| line);
        let _ = tx.send((result, stdout));
    });
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let Ok((result, returned)) = rx.recv_timeout(remaining) else {
        let _ = agent.child.kill();
        panic!("no stdout line within 90s");
    };
    agent.stdout = Some(returned);
    let line = result.unwrap();
    assert!(!line.is_empty(), "agent closed stdout");
    serde_json::from_str(line.trim_end())
        .unwrap_or_else(|e| panic!("stdout line is not valid JSON ({e}): {line:?}"))
}

fn command(
    agent: &mut RpcAgent,
    id: &str,
    command: &str,
    extra: serde_json::Value,
) -> serde_json::Value {
    command_trace(agent, id, command, extra).0
}

/// Run one command while retaining every out-of-band event that precedes
/// its response. Session lifecycle assertions must not hide events the way
/// the ordinary response-only helper intentionally does.
fn command_trace(
    agent: &mut RpcAgent,
    id: &str,
    command: &str,
    extra: serde_json::Value,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut payload = serde_json::json!({ "type": command, "id": id });
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    send(&mut agent.stdin, payload);
    let mut events = Vec::new();
    loop {
        let value = read_line(agent);
        if value["type"] == "response" && value["id"] == id {
            return (value, events);
        }
        events.push(value);
    }
}

fn assert_start_marker(event: &serde_json::Value, reason: &str) -> String {
    assert_eq!(event["type"], "entry_appended", "{event}");
    assert_eq!(event["entry"]["customType"], "start-marker", "{event}");
    assert_eq!(event["entry"]["data"], reason, "{event}");
    event["entry"]["id"].as_str().unwrap().to_string()
}

/// Send a prompt and read until the command response and, when a run was
/// actually started, its agent_end arrived. A `handled` prompt has no run and
/// therefore no synthetic agent_end.
fn prompt_and_collect(
    agent: &mut RpcAgent,
    id: &str,
    message: &str,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    send(
        &mut agent.stdin,
        serde_json::json!({ "type": "prompt", "id": id, "message": message }),
    );
    let mut response = None;
    let mut events = Vec::new();
    let mut agent_end_seen = false;
    let mut handled = false;
    while response.is_none() || (!handled && !agent_end_seen) {
        let value = read_line(agent);
        if value["type"] == "response" && value["id"] == id {
            handled = value["data"]["disposition"] == "handled";
            response = Some(value);
            continue;
        }
        if value["type"] == "agent_end" {
            agent_end_seen = true;
        }
        events.push(value);
    }
    wait_until_idle(agent);
    (response.unwrap(), events)
}

/// Wait until the agent is fully idle: not streaming and nothing queued.
///
/// agent_end alone does NOT mean the agent accepts a fresh prompt: the pump
/// runs Stop hooks AFTER the terminal event and only then clears
/// is_streaming. A prompt sent in that window is queued as steering for a
/// run that already ended — it never starts a run and never emits an
/// agent_end (the client would hang on the next read). The Stop hook's
/// marker-file write is visible even earlier (before the hook process exits),
/// so file polling cannot close this race either; only the wire state can.
fn wait_until_idle(agent: &mut RpcAgent) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    for n in 1.. {
        let r = command(
            agent,
            &format!("idle-{n}"),
            "get_state",
            serde_json::json!({}),
        );
        if r["success"] == true
            && r["data"]["isStreaming"] == false
            && r["data"]["pendingMessageCount"] == 0
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agent did not go idle"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Poll a marker file until it has `expected` lines (post-agent_end hooks
/// race the reader — the pump runs them after writing the terminal event).
fn wait_for_file_lines(path: &std::path::Path, expected: usize) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if content.lines().count() >= expected {
            return content;
        }
        if std::time::Instant::now() > deadline {
            return content;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn rpc_prompt_started_response_precedes_agent_events() {
    let mut agent = spawn_rpc();
    send(
        &mut agent.stdin,
        serde_json::json!({ "type": "prompt", "id": "ordered", "message": "hello" }),
    );
    let response = read_line(&mut agent);
    assert_eq!(response["type"], "response", "{response}");
    assert_eq!(response["id"], "ordered", "{response}");
    assert_eq!(response["data"]["disposition"], "started", "{response}");
    let first_event = read_line(&mut agent);
    assert_eq!(first_event["type"], "agent_start", "{first_event}");
    loop {
        if read_line(&mut agent)["type"] == "agent_end" {
            break;
        }
    }
    wait_until_idle(&mut agent);
}

#[cfg(feature = "ext")]
#[test]
fn rpc_session_replacement_start_entries_precede_each_success_response() {
    let mut agent = spawn_rpc_with_markers(true);
    let startup = read_line(&mut agent);
    assert_start_marker(&startup, "startup");

    let (response, events) = command_trace(&mut agent, "new", "new_session", serde_json::json!({}));
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(events.len(), 1, "{events:?}");
    let target_id = assert_start_marker(&events[0], "new");

    let (response, events) = command_trace(
        &mut agent,
        "fork",
        "fork",
        serde_json::json!({"entryId": target_id}),
    );
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_start_marker(&events[0], "fork");

    let (response, events) = command_trace(&mut agent, "clone", "clone", serde_json::json!({}));
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_start_marker(&events[0], "fork");

    let state = command(&mut agent, "state", "get_state", serde_json::json!({}));
    let path = state["data"]["sessionFile"].as_str().unwrap();
    let (response, events) = command_trace(
        &mut agent,
        "switch",
        "switch_session",
        serde_json::json!({"path": path}),
    );
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_start_marker(&events[0], "resume");
}

#[test]
fn rpc_set_hooks_validates_shape() {
    let mut agent = spawn_rpc();
    let r = command(
        &mut agent,
        "bad-1",
        "set_hooks",
        serde_json::json!({ "hooks": ["not-an-object"] }),
    );
    assert_eq!(r["success"], false, "{r}");

    let r = command(
        &mut agent,
        "ok-1",
        "set_hooks",
        serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "hooks": [ { "type": "command", "command": "true" } ] }
                ],
                "Stop": [
                    { "hooks": [ { "type": "command", "command": "true" } ] },
                    { "matcher": "*", "hooks": [ { "type": "command", "command": "true" } ] }
                ]
            }
        }),
    );
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["data"]["handlers"]["UserPromptSubmit"], 1, "{r}");
    assert_eq!(r["data"]["handlers"]["Stop"], 2, "{r}");

    // null clears the session set.
    let r = command(&mut agent, "clr-1", "set_hooks", serde_json::json!({}));
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["data"]["handlers"], serde_json::json!({}), "{r}");
}

#[test]
fn rpc_user_prompt_submit_block_drops_prompt() {
    let mut agent = spawn_rpc();
    let r = command(
        &mut agent,
        "set-1",
        "set_hooks",
        serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "hooks": [ { "type": "command",
                        "command": "echo policy-violation 1>&2; exit 2" } ] }
                ]
            }
        }),
    );
    assert_eq!(r["success"], true, "{r}");

    let (r, events) = prompt_and_collect(&mut agent, "p-1", "hello");
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["data"]["disposition"], "handled", "{r}");
    let notice = events
        .iter()
        .find(|e| e["type"] == "hook_notice" && e["hookEvent"] == "UserPromptSubmit")
        .unwrap_or_else(|| panic!("missing hook_notice: {events:?}"));
    assert_eq!(notice["kind"], "block", "{events:?}");
    assert_eq!(notice["message"], "policy-violation", "{events:?}");
    // The prompt was dropped before the agent loop: no assistant activity.
    assert!(
        events
            .iter()
            .all(|e| e["type"] != "message_start" && e["type"] != "turn_start"),
        "blocked prompt reached the agent loop: {events:?}"
    );

    // A cleared set lets the next prompt through (it then errors out at the
    // provider, which is fine — the point is the hook no longer blocks).
    let r = command(&mut agent, "clr-1", "set_hooks", serde_json::json!({}));
    assert_eq!(r["success"], true, "{r}");
    let (r, events) = prompt_and_collect(&mut agent, "p-2", "hello again");
    assert_eq!(r["success"], true, "{r}");
    assert!(
        events
            .iter()
            .all(|e| !(e["type"] == "hook_notice" && e["kind"] == "block")),
        "cleared hooks still blocking: {events:?}"
    );
}

#[test]
fn rpc_session_start_and_stop_hooks_fire_around_turn() {
    let mut agent = spawn_rpc();
    let start_marker = agent.work_dir.join("session-start.log");
    let stop_marker = agent.work_dir.join("stop.log");
    let hooks = serde_json::json!({
        "hooks": {
            "SessionStart": [
                { "hooks": [ { "type": "command",
                    "command": format!("echo start >> {}", shell_quote(&start_marker)) } ] }
            ],
            "Stop": [
                { "hooks": [ { "type": "command",
                    "command": format!("echo stop >> {}", shell_quote(&stop_marker)) } ] }
            ]
        }
    });
    let r = command(&mut agent, "set-1", "set_hooks", hooks);
    assert_eq!(r["success"], true, "{r}");

    // The turn errors out at the provider (no auth in the sandbox) — hooks
    // must still fire around it.
    let (r, _) = prompt_and_collect(&mut agent, "p-1", "hi");
    assert_eq!(r["success"], true, "{r}");

    assert_eq!(
        wait_for_file_lines(&start_marker, 1).trim(),
        "start",
        "SessionStart hook did not fire at first prompt"
    );
    assert_eq!(
        wait_for_file_lines(&stop_marker, 1).trim(),
        "stop",
        "Stop hook did not fire after the terminal event"
    );

    // Second prompt in the same session: SessionStart must NOT refire
    // (one-shot per session), Stop fires again.
    let (r, _) = prompt_and_collect(&mut agent, "p-2", "again");
    assert_eq!(r["success"], true, "{r}");
    wait_for_file_lines(&stop_marker, 2);
    let starts = std::fs::read_to_string(&start_marker).unwrap_or_default();
    assert_eq!(
        starts.lines().count(),
        1,
        "SessionStart refired within one session: {starts:?}"
    );
    let stops = std::fs::read_to_string(&stop_marker).unwrap_or_default();
    assert_eq!(stops.lines().count(), 2, "Stop did not refire: {stops:?}");

    // new_session re-arms SessionStart.
    let r = command(&mut agent, "new-1", "new_session", serde_json::json!({}));
    assert_eq!(r["success"], true, "{r}");
    let (r, _) = prompt_and_collect(&mut agent, "p-3", "fresh");
    assert_eq!(r["success"], true, "{r}");
    wait_for_file_lines(&start_marker, 2);
    let starts = std::fs::read_to_string(&start_marker).unwrap_or_default();
    assert_eq!(
        starts.lines().count(),
        2,
        "SessionStart did not refire after new_session: {starts:?}"
    );
}

#[test]
fn rpc_user_prompt_submit_additional_context_prepends() {
    let mut agent = spawn_rpc();
    let context_marker = agent.work_dir.join("prompt-context.log");
    // The hook writes the prompt JSON it receives on stdin to a file and
    // answers with additionalContext; the agent loop then fails at the
    // provider but the SessionStart-free turn still ran the hook.
    let hooks = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [
                { "hooks": [ { "type": "command",
                    "command": format!("cat > {}; echo '{{\"hookSpecificOutput\":{{\"additionalContext\":\"HOOK-CTX\"}}}}'", shell_quote(&context_marker)) } ] }
            ]
        }
    });
    let r = command(&mut agent, "set-1", "set_hooks", hooks);
    assert_eq!(r["success"], true, "{r}");
    let (r, _) = prompt_and_collect(&mut agent, "p-1", "original-text");
    assert_eq!(r["success"], true, "{r}");
    let seen = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let content = std::fs::read_to_string(&context_marker).unwrap_or_default();
            if !content.trim().is_empty() || std::time::Instant::now() > deadline {
                break content;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    let input: serde_json::Value = serde_json::from_str(seen.trim())
        .unwrap_or_else(|e| panic!("hook stdin is not JSON ({e}): {seen:?}"));
    assert_eq!(input["hook_event_name"], "UserPromptSubmit", "{input}");
    assert_eq!(input["prompt"], "original-text", "{input}");
}
