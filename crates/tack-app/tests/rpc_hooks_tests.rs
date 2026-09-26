//! RPC-mode session hooks over the real stdio JSONL endpoint:
//! `set_hooks` + SessionStart/UserPromptSubmit/Stop firing. Hermetic: no
//! network, no LLM — the observed behavior happens before/after the agent
//! loop (blocked prompts never reach the provider; SessionStart/Stop hooks
//! are file side effects around a provider-less turn that errors out).
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use tack_tools::shell::shell_quote;

struct RpcAgent {
    child: Child,
    stdin: ChildStdin,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    agent_dir: std::path::PathBuf,
    work_dir: std::path::PathBuf,
}

fn spawn_rpc() -> RpcAgent {
    let agent_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(["--mode", "rpc"])
        .env("TACK_AGENT_DIR", agent_dir.path())
        .current_dir(work_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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

fn read_response(agent: &mut RpcAgent, id: &str) -> serde_json::Value {
    loop {
        let value = read_line(agent);
        if value["type"] == "response" && value["id"] == id {
            return value;
        }
    }
}

fn command(
    agent: &mut RpcAgent,
    id: &str,
    command: &str,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut payload = serde_json::json!({ "type": command, "id": id });
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    send(&mut agent.stdin, payload);
    read_response(agent, id)
}

/// Send a prompt and read until BOTH the command response and the run's
/// agent_end arrived (either order — a hook-blocked prompt emits its
/// synthetic agent_end before the response).
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
    while response.is_none() || !agent_end_seen {
        let value = read_line(agent);
        if value["type"] == "response" && value["id"] == id {
            response = Some(value);
            continue;
        }
        if value["type"] == "agent_end" {
            agent_end_seen = true;
        }
        events.push(value);
    }
    (response.unwrap(), events)
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
