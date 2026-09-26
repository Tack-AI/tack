//! RPC-mode MCP commands over the real stdio JSONL endpoint:
//! `set_mcp_servers` + `get_mcp_status`, using `tack mcp-serve` itself as
//! the stdio MCP server (no network, no LLM).
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

struct RpcAgent {
    child: Child,
    stdin: ChildStdin,
    /// Option so the reader can move onto a helper thread (bounded read)
    /// and back between reads.
    stdout: Option<BufReader<std::process::ChildStdout>>,
    agent_dir: std::path::PathBuf,
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
    }
}

impl Drop for RpcAgent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.agent_dir);
    }
}

fn send(stdin: &mut ChildStdin, value: serde_json::Value) {
    let mut line = serde_json::to_string(&value).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
}

/// Read lines until the response for `id` arrives (agent events and
/// responses to other commands are skipped). Bounded: a wedged agent must
/// fail the test with diagnostics instead of hanging the CI job.
fn read_response(agent: &mut RpcAgent, id: &str) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
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
            panic!("no stdout line within 90s waiting for response {id}");
        };
        agent.stdout = Some(returned);
        let line = result.unwrap();
        assert!(
            !line.is_empty(),
            "agent closed stdout while waiting for {id}"
        );
        let value: serde_json::Value = serde_json::from_str(line.trim_end())
            .unwrap_or_else(|e| panic!("stdout line is not valid JSON ({e}): {line:?}"));
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

#[test]
fn rpc_set_mcp_servers_and_status_roundtrip() {
    let mut agent = spawn_rpc();
    let tack_bin = env!("CARGO_BIN_EXE_tack");

    // set_mcp_servers: session-scoped injection (host createSession path).
    let r = command(
        &mut agent,
        "set-1",
        "set_mcp_servers",
        serde_json::json!({
            "servers": {
                "self-mcp": { "command": tack_bin, "args": ["mcp-serve"] }
            }
        }),
    );
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["data"]["servers"], serde_json::json!(["self-mcp"]), "{r}");

    // get_mcp_status (connect): the pool builds, the mcp-serve child lists
    // its tools.
    let r = command(
        &mut agent,
        "status-1",
        "get_mcp_status",
        serde_json::json!({}),
    );
    assert_eq!(r["success"], true, "{r}");
    let servers = r["data"]["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1, "{r}");
    assert_eq!(servers[0]["name"], "self-mcp");
    assert_eq!(servers[0]["transport"], "stdio");
    assert_eq!(servers[0]["status"], "connected", "{r}");
    let tool_count = servers[0]["toolCount"].as_u64().unwrap();
    assert!(
        tool_count >= 4,
        "mcp-serve exposes its tool set, got {tool_count} ({r})"
    );

    // get_mcp_status (status-only): served from the cached pool, still
    // connected without a reconnect.
    let r = command(
        &mut agent,
        "status-2",
        "get_mcp_status",
        serde_json::json!({ "connect": false }),
    );
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["data"]["servers"][0]["status"], "connected", "{r}");

    // Clearing the set empties the status view (and drops the pool,
    // killing the mcp-serve child).
    let r = command(
        &mut agent,
        "set-2",
        "set_mcp_servers",
        serde_json::json!({ "servers": {} }),
    );
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["data"]["servers"], serde_json::json!([]), "{r}");
    let r = command(
        &mut agent,
        "status-3",
        "get_mcp_status",
        serde_json::json!({ "connect": false }),
    );
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["data"]["servers"].as_array().unwrap().len(), 0, "{r}");
}
