//! MCP OAuth CLI cancellation must not leave an uncancellable stdin reader
//! keeping the process alive after its whole-flow timeout.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

#[tokio::test]
async fn login_timeout_exits_with_stdin_still_open() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        let body = serde_json::json!({
            "issuer": format!("http://{address}"),
            "authorization_endpoint": format!("http://{address}/authorize"),
            "token_endpoint": format!("http://{address}/token"),
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(
        agent.join("mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "x": {
                    "url": format!("http://{address}/mcp"),
                    "oauth": {
                        "clientId": "client",
                        "authServerMetadataUrl": format!("http://{address}/metadata")
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    for program in ["open", "xdg-open"] {
        let path = bin.join(program);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_tack"))
        .arg("--agent-dir")
        .arg(&agent)
        .args(["mcp", "login", "x", "--timeout", "0.2"])
        .current_dir(dir.path())
        .env("PATH", path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Deliberately keep the pipe open without sending a line. Before the
    // runtime-shutdown fix, Tokio's blocked stdin reader kept the process here.
    let _open_stdin = child.stdin.take().unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(4), child.wait())
        .await
        .expect("mcp login remained alive after its timeout")
        .unwrap();
    assert!(
        !status.success(),
        "a timed-out sign-in should fail: {status}"
    );
}
