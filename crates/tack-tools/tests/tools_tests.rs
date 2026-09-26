//! Integration tests for the built-in tools on temp directories.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use tack_agent_core::AgentTool;
use tack_tools::*;
use tokio_util::sync::CancellationToken;

fn noop_update(_: tack_agent_core::AgentToolResult) {}

async fn run_tool(tool: &dyn AgentTool, params: Value) -> Result<String, String> {
    // The agent loop runs prepare_arguments + validation in preflight;
    // mirror that here.
    let params = tool.prepare_arguments(params);
    tool.validate_arguments(&params)?;
    let result = tool
        .execute("test-call", params, CancellationToken::new(), &noop_update)
        .await?;
    Ok(result
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn services_in(dir: &tempfile::TempDir) -> ToolServices {
    tack_tools::default_services(dir.path().to_path_buf())
}

#[tokio::test]
async fn write_then_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let services = services_in(&dir);
    let write = WriteTool::new(services.clone());
    let read = ReadTool::new(services);

    let out = run_tool(
        &write,
        json!({"path": "sub/hello.txt", "content": "line1\nline2\nline3"}),
    )
    .await
    .unwrap();
    assert!(out.contains("Successfully wrote"));

    let out = run_tool(&read, json!({"path": "sub/hello.txt"}))
        .await
        .unwrap();
    assert_eq!(out, "line1\nline2\nline3");

    // Offset/limit.
    let out = run_tool(
        &read,
        json!({"path": "sub/hello.txt", "offset": 2, "limit": 1}),
    )
    .await
    .unwrap();
    assert!(out.starts_with("line2"));
    assert!(out.contains("more lines in file"));

    // Offset out of bounds.
    let err = run_tool(&read, json!({"path": "sub/hello.txt", "offset": 99}))
        .await
        .unwrap_err();
    assert!(err.contains("beyond end of file"));
}

#[tokio::test]
async fn edit_exact_and_fuzzy() {
    let dir = tempfile::tempdir().unwrap();
    let services = services_in(&dir);
    std::fs::write(
        dir.path().join("a.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();

    let edit = EditTool::new(services.clone());
    let out = run_tool(
        &edit,
        json!({
            "path": "a.rs",
            "edits": [{ "oldText": "println!(\"hi\");", "newText": "println!(\"bye\");" }]
        }),
    )
    .await
    .unwrap();
    assert!(out.contains("Successfully replaced 1 block"));
    assert!(
        std::fs::read_to_string(dir.path().join("a.rs"))
            .unwrap()
            .contains("bye")
    );

    // Legacy single-edit arguments (oldText/newText at top level).
    let out = run_tool(
        &edit,
        json!({"path": "a.rs", "oldText": "bye", "newText": "hi again"}),
    )
    .await
    .unwrap();
    assert!(out.contains("Successfully replaced"));

    // Missing text errors.
    let err = run_tool(
        &edit,
        json!({"path": "a.rs", "edits": [{"oldText": "nope", "newText": "x"}]}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("Could not find"));
}

#[tokio::test]
async fn edit_preserves_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let services = services_in(&dir);
    std::fs::write(dir.path().join("win.txt"), "a\r\nb\r\nc\r\n").unwrap();

    let edit = EditTool::new(services);
    run_tool(
        &edit,
        json!({"path": "win.txt", "edits": [{"oldText": "b", "newText": "B"}]}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("win.txt")).unwrap(),
        "a\r\nB\r\nc\r\n"
    );
}

#[tokio::test]
async fn ls_and_find_and_grep() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.path().join("src/nested/lib.rs"), "pub fn helper() {}\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "# hello\n").unwrap();

    let services = services_in(&dir);

    let ls = LsTool::new(services.clone());
    let out = run_tool(&ls, json!({})).await.unwrap();
    assert!(out.contains("README.md"));
    assert!(out.contains("src/"));

    let find = FindTool::new(services.clone());
    let out = run_tool(&find, json!({"pattern": "**/*.rs"}))
        .await
        .unwrap();
    assert!(out.contains("src/main.rs"));
    assert!(out.contains("src/nested/lib.rs"));
    assert!(!out.contains("README.md"));

    let grep = GrepTool::new(services.clone());
    let out = run_tool(&grep, json!({"pattern": "fn main"}))
        .await
        .unwrap();
    assert!(out.contains("src/main.rs:1: fn main() {}"));

    // Grep with context.
    let out = run_tool(&grep, json!({"pattern": "helper", "context": 0}))
        .await
        .unwrap();
    assert!(out.contains("src/nested/lib.rs:1: pub fn helper() {}"));

    // Grep literal + case insensitive.
    let out = run_tool(
        &grep,
        json!({"pattern": "FN MAIN", "literal": true, "ignoreCase": true}),
    )
    .await
    .unwrap();
    assert!(out.contains("main.rs"));

    // No matches.
    let out = run_tool(&grep, json!({"pattern": "zzz-no-match"}))
        .await
        .unwrap();
    assert_eq!(out, "No matches found");
}

#[tokio::test]
async fn bash_echo_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let services = services_in(&dir);
    if services.shell.is_none() {
        eprintln!("no shell available, skipping bash test");
        return;
    }
    let bash = BashTool::new(services);

    let out = run_tool(&bash, json!({"command": "echo hello && pwd"}))
        .await
        .unwrap();
    assert!(out.contains("hello"));

    let err = run_tool(&bash, json!({"command": "exit 3"}))
        .await
        .unwrap_err();
    assert!(err.contains("exited with code 3"));
}

#[tokio::test]
async fn bash_timeout_kills_process() {
    let dir = tempfile::tempdir().unwrap();
    let services = services_in(&dir);
    if services.shell.is_none() {
        return;
    }
    let bash = BashTool::new(services);
    let start = std::time::Instant::now();
    let err = run_tool(&bash, json!({"command": "sleep 30", "timeout": 1}))
        .await
        .unwrap_err();
    assert!(err.contains("timed out"), "unexpected error: {err}");
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn bash_cancel_kills_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let services = services_in(&dir);
    if services.shell.is_none() {
        return;
    }
    let bash = Arc::new(BashTool::new(services));
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        cancel2.cancel();
    });
    let result = bash
        .execute("t", json!({"command": "sleep 30"}), cancel, &noop_update)
        .await;
    let err = result.unwrap_err();
    assert!(err.contains("aborted"), "unexpected error: {err}");
}

#[tokio::test]
async fn bash_large_output_truncated_to_tail() {
    let dir = tempfile::tempdir().unwrap();
    let services = services_in(&dir);
    if services.shell.is_none() {
        return;
    }
    let bash = BashTool::new(services);
    let out = run_tool(
        &bash,
        json!({"command": "for i in $(seq 1 3000); do echo line-$i; done"}),
    )
    .await
    .unwrap();
    assert!(out.contains("line-3000"));
    assert!(!out.contains("line-1\n"));
    assert!(out.contains("Showing lines"));
    assert!(out.contains("Full output:"));
}

#[tokio::test]
async fn schema_validation_rejects_bad_params() {
    let dir = tempfile::tempdir().unwrap();
    let services = services_in(&dir);
    let read = ReadTool::new(services);
    assert!(read.validate_arguments(&json!({"path": 42})).is_err());
    assert!(read.validate_arguments(&json!({"path": "ok"})).is_ok());
}
