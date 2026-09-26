//! Stdio smoke test: spawn the real `tack acp` binary and verify every
//! stdout line is a valid JSON-RPC envelope (any stray println! on stdout
//! would break the protocol).
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

struct Agent {
    child: Child,
    stdin: ChildStdin,
    /// Option so the reader can move onto a helper thread (bounded read)
    /// and back between reads.
    stdout: Option<BufReader<std::process::ChildStdout>>,
    agent_dir: PathBuf,
    stderr_log: PathBuf,
}

fn spawn_agent() -> Agent {
    let agent_dir = tempfile::tempdir().unwrap();
    // stderr goes to a file (not null): when the agent dies at startup —
    // observed on Windows CI — the panic message and crash.log are the
    // only way to see why. Kept inside the leaked agent dir.
    let stderr_log = agent_dir.path().join("acp-stderr.log");
    let stderr = std::fs::File::create(&stderr_log).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tack"))
        .arg("acp")
        .env("TACK_AGENT_DIR", agent_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    let dir = agent_dir.path().to_path_buf();
    // Leak the tempdir so it outlives the child.
    std::mem::forget(agent_dir);
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    Agent {
        child,
        stdin,
        stdout: Some(stdout),
        agent_dir: dir,
        stderr_log,
    }
}

impl Agent {
    /// Startup-failure evidence: the panic hook appends to crash.log, and
    /// TackAcpAgent::new reports provider/model setup on stderr.
    fn diagnostics(&self) -> String {
        let read_tail = |path: &std::path::Path| -> String {
            let Ok(bytes) = std::fs::read(path) else {
                return String::new();
            };
            let text = String::from_utf8_lossy(&bytes);
            let start = text.len().saturating_sub(4096);
            text[start..].to_string()
        };
        let crash = read_tail(&self.agent_dir.join("crash.log"));
        let stderr = read_tail(&self.stderr_log);
        format!(
            "\n--- agent stderr ---\n{stderr}\n--- crash.log ---\n{}",
            if crash.is_empty() {
                "<none>".to_string()
            } else {
                crash
            }
        )
    }
}

fn send(stdin: &mut ChildStdin, value: serde_json::Value) {
    let mut line = serde_json::to_string(&value).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
}

fn read_jsonrpc_line(agent: &mut Agent) -> serde_json::Value {
    // Bound the read: an agent alive-but-silent at startup (observed as a
    // startup death on Windows CI) would otherwise block read_line
    // forever and the job dies at its timeout with zero diagnostics.
    let mut stdout = agent.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = stdout.read_line(&mut line).map(|_| line);
        // On timeout the receiver is gone; the send just fails.
        let _ = tx.send((result, stdout));
    });
    let Ok((result, returned)) = rx.recv_timeout(std::time::Duration::from_secs(60)) else {
        let _ = agent.child.kill();
        panic!(
            "agent produced no stdout line within 60s (wedged at startup?){}",
            agent.diagnostics()
        );
    };
    agent.stdout = Some(returned);
    let line = result.unwrap();
    assert!(
        !line.is_empty(),
        "agent closed stdout unexpectedly{}",
        agent.diagnostics()
    );
    let value: serde_json::Value = serde_json::from_str(line.trim_end())
        .unwrap_or_else(|e| panic!("stdout line is not valid JSON-RPC ({e}): {line:?}"));
    // JSON-RPC 2.0 envelope marker.
    assert_eq!(
        value["jsonrpc"],
        serde_json::json!("2.0"),
        "not a JSON-RPC envelope: {line}"
    );
    value
}

#[test]
fn acp_stdio_initialize_and_new_session() {
    let mut agent = spawn_agent();

    send(
        &mut agent.stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": false}
            }
        }),
    );
    let response = read_jsonrpc_line(&mut agent);
    assert_eq!(response["id"], serde_json::json!(1));
    assert!(
        response["result"]["protocolVersion"].is_number(),
        "{response}"
    );
    assert!(response["result"]["agentCapabilities"].is_object());
    assert_eq!(
        response["result"]["agentInfo"]["name"],
        serde_json::json!("tack"),
        "{response}"
    );

    // session/new immediately after initialize (known client behavior).
    send(
        &mut agent.stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {"cwd": std::env::temp_dir().to_string_lossy(), "mcpServers": []}
        }),
    );
    let response = read_jsonrpc_line(&mut agent);
    assert_eq!(response["id"], serde_json::json!(2));
    assert!(response["result"]["sessionId"].is_string(), "{response}");

    drop(agent.stdin);
    let _ = agent.child.wait();
}

/// Regression: `tack --provider X acp` must honor the CLI flag — ACP mode
/// used to silently drop it and resolve the provider from settings/env only.
/// Asserts on the startup stderr line, so no credentials are needed.
#[test]
fn acp_honors_provider_flag_on_command_line() {
    let agent_dir = tempfile::tempdir().unwrap();
    // stdin null: the agent EOFs and exits right after startup, so
    // `output()` returns promptly.
    let output = Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(["--provider", "openai", "acp"])
        .env("TACK_AGENT_DIR", agent_dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("provider=openai"),
        "expected provider=openai on stderr, got: {stderr}"
    );
}
