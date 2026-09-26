//! e2e for the CodeBuddy native provider against a mock CLI
//! (`fixtures/mock_codebuddy.py`, speaking the stream-json protocol).
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use std::sync::Arc;

use serde_json::json;
use tack_ai::codebuddy::CODEBUDDY_API;
use tack_ai::{Context, Message, Model, Provider, StreamOptions, ToolDefinition};
use tokio_util::sync::CancellationToken;

fn mock_model() -> Model {
    model_with_id("mock-1")
}

fn model_with_id(id: &str) -> Model {
    Model {
        provider: "codebuddy".into(),
        id: id.into(),
        name: "Mock Model".into(),
        api: CODEBUDDY_API.into(),
        base_url: String::new(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![],
        cost: tack_ai::ModelCost::default(),
        context_window: 200_000,
        max_tokens: 32_768,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn options(session: &str) -> StreamOptions {
    StreamOptions {
        session_id: Some(session.to_string()),
        cancel: CancellationToken::new(),
        ..Default::default()
    }
}

fn python3_or_skip() -> bool {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 unavailable; skipping");
        return false;
    }
    true
}

fn echo_tool() -> ToolDefinition {
    ToolDefinition {
        name: "echo".into(),
        description: "echo back".into(),
        parameters: json!({"type":"object","properties":{"text":{"type":"string"}}}),
        defer_loading: false,
        constrained_sampling: None,
    }
}

struct MockEnv {
    argv_dir: std::path::PathBuf,
    config_dir: std::path::PathBuf,
}

/// Shared mock environment: CODEBUDDY_PATH + per-process temp dirs for the
/// argv/request logs, the CodeBuddy session store, and the tack agent dir
/// (keeps test data out of the real $HOME).
fn mock_env() -> MockEnv {
    let fixture = format!(
        "{}/tests/fixtures/mock_codebuddy.py",
        env!("CARGO_MANIFEST_DIR")
    );
    let base = std::env::temp_dir().join(format!("tack-cb-e2e-{}", std::process::id()));
    let argv_dir = base.join("argv");
    let config_dir = base.join("cb-config");
    let agent_dir = base.join("agent");
    std::fs::create_dir_all(&argv_dir).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    unsafe {
        std::env::set_var("CODEBUDDY_PATH", &fixture);
        std::env::set_var("CODEBUDDY_MOCK_ARGV_LOG_DIR", &argv_dir);
        std::env::set_var("CODEBUDDY_CONFIG_DIR", &config_dir);
        std::env::set_var("TACK_AGENT_DIR", &agent_dir);
    }
    MockEnv {
        argv_dir,
        config_dir,
    }
}

/// Fail fast instead of hanging forever: a protocol regression must not
/// leave an 800MB test process parked (OOMs this dev box).
async fn result_with_timeout(
    stream: tack_ai::AssistantMessageEventStream,
) -> tack_ai::AssistantMessage {
    tokio::time::timeout(std::time::Duration::from_secs(30), stream.result())
        .await
        .expect("stream timed out after 30s — provider is stuck")
}

/// One-at-a-time guard for the e2e suite: the tests share process-global
/// state (env vars, pid-keyed temp dirs, the provider's session registry),
/// and parallel runs on resource-tight Windows CI runners proved flaky.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serial guard + shared mock environment in one call, so no test can
/// forget the guard.
async fn serial_mock_env() -> (tokio::sync::MutexGuard<'static, ()>, MockEnv) {
    let guard = SERIAL.lock().await;
    (guard, mock_env())
}

/// Spawn flags + `--effort` mapping + stream_event partials: the turn's
/// content arrives only via stream_event deltas (the trailing assistant
/// message must be ignored), usage comes from message_start/message_delta.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_stream_event_partials_and_spawn_flags() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, env) = serial_mock_env().await;
    let argv_dir = &env.argv_dir;

    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-stream");
    let mut opts = options("cb-e2e-stream");
    opts.reasoning = Some(tack_ai::ThinkingLevel::High);
    let context = Context {
        system_prompt: Some("be brief".into()),
        messages: vec![Message::user("stream-hi")],
        tools: vec![],
    };
    let stream = provider.stream(&model, &context, opts);
    let msg = result_with_timeout(stream).await;
    assert_eq!(msg.stop_reason, tack_ai::StopReason::Stop);
    assert_eq!(msg.content.len(), 1, "{:?}", msg.content);
    let tack_ai::ContentBlock::Text { text, .. } = &msg.content[0] else {
        panic!("expected text, got {:?}", msg.content)
    };
    assert_eq!(text, "stream-echo: hi");
    // Aggregate input (30) for totals; total_tokens keeps the last
    // per-request size (7+2) from message_delta.
    assert_eq!(msg.usage.input, 30);
    assert_eq!(msg.usage.total_tokens, 9);

    // Spawn flags mirror the TS reference (SDK ProcessTransport.buildArgs).
    let argv: Vec<String> = serde_json::from_str(
        &std::fs::read_to_string(argv_dir.join("argv-mock-stream.json")).unwrap(),
    )
    .unwrap();
    let has = |flag: &str| argv.iter().any(|a| a == flag);
    assert!(has("--include-partial-messages"), "{argv:?}");
    assert!(has("--strict-mcp-config"), "{argv:?}");
    assert!(has("--setting-sources"), "{argv:?}");
    assert!(has("--system-prompt"), "{argv:?}");
    assert!(
        !has("--mcp-config"),
        "tools ride the SDK MCP control channel, not a config file: {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w == ["--permission-mode", "bypassPermissions"]),
        "{argv:?}"
    );
    assert!(argv.windows(2).any(|w| w == ["--tools", ""]), "{argv:?}");
    assert!(
        argv.windows(2).any(|w| w == ["--effort", "high"]),
        "{argv:?}"
    );

    // The initialize handshake declared tack's tools as an SDK MCP
    // server, and the mock listed them (empty for this context).
    let requests = std::fs::read_to_string(argv_dir.join("requests-mock-stream.log")).unwrap();
    assert!(
        requests.contains("CONTROL initialize sdkMcpServers=tack"),
        "{requests}"
    );
    assert!(requests.contains("MCP-SEND tools/list"), "{requests}");
}

/// Error `result` lines: the CLI reports failures in an `errors` string
/// array (Claude Code style — `errors_info` never existed on the wire).
/// Regression: the first implementation read `errors_info`, collapsing
/// every real failure (auth, rate limits, ...) into the opaque
/// "codebuddy turn failed". Scenario mirrors a CLI 2.156.0
/// unauthenticated capture.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_error_result_surfaces_errors_array() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, _env) = serial_mock_env().await;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-error");
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("auth-fail")],
        tools: vec![],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e-error"));
    let msg = result_with_timeout(stream).await;
    assert_eq!(msg.stop_reason, tack_ai::StopReason::Error);
    assert_eq!(
        msg.error_message.as_deref(),
        Some("Authentication required. Please use /login command to sign in to your account"),
    );
}

/// Tool call assembled from input_json deltas (stream_event path), then the
/// parked MCP flow completes exactly like the assistant-message path.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_stream_event_tool_call() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, env) = serial_mock_env().await;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-stream-tool");

    // --- Turn 1: tool call via stream_event partials -------------------------
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("stream-tool")],
        tools: vec![echo_tool()],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e-stream-tool"));
    let msg1 = result_with_timeout(stream).await;
    assert_eq!(msg1.stop_reason, tack_ai::StopReason::ToolUse);
    let tack_ai::ContentBlock::ToolCall {
        id,
        name,
        arguments,
        ..
    } = &msg1.content[0]
    else {
        panic!("expected tool call, got {:?}", msg1.content)
    };
    assert_eq!(id, "toolu_9");
    assert_eq!(name, "echo");
    assert_eq!(arguments["text"], "streamed");

    // The mock plays the CLI's SDK-MCP client itself: it issued the
    // tools/call as an mcp_message control_request right after the tool
    // boundary and is parked waiting for turn 2's sync to resolve it.

    // --- Turn 2: result delivery; session continues --------------------------
    let tool_result = tack_ai::ToolResultMessage {
        tool_call_id: "toolu_9".into(),
        tool_name: "echo".into(),
        content: vec![tack_ai::InputContentBlock::text("pong")],
        details: None,
        usage: None,
        is_error: false,
        timestamp: 0,
    };
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("stream-tool"),
            Message::Assistant(msg1.clone()),
            Message::ToolResult(tool_result),
        ],
        tools: vec![],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e-stream-tool"));
    let msg2 = result_with_timeout(stream).await;
    assert_eq!(msg2.stop_reason, tack_ai::StopReason::Stop);
    let tack_ai::ContentBlock::Text { text, .. } = &msg2.content[0] else {
        panic!("expected text, got {:?}", msg2.content)
    };
    assert_eq!(text, "tool said: pong");

    // The provider declared the SDK MCP server at initialize, advertised
    // the echo tool on tools/list, and answered the mock's tools/call.
    let requests =
        std::fs::read_to_string(env.argv_dir.join("requests-mock-stream-tool.log")).unwrap();
    assert!(
        requests.contains("CONTROL initialize sdkMcpServers=tack"),
        "{requests}"
    );
    assert!(requests.contains("MCP tools/list echo"), "{requests}");
    assert!(requests.contains("MCP-SEND tools/call"), "{requests}");
}

/// Parallel tool calls: the real CLI reuses ONE content index for every
/// tool_use block in a batched message and emits a single
/// content_block_stop. Both calls must be parked (a first-match stop
/// drops the second and its tool result later fails sync into a costly
/// transcript respawn).
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_stream_event_parallel_tool_calls() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, env) = serial_mock_env().await;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-parallel");

    // --- Turn 1: two tool_use blocks sharing one content index ----------
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("parallel-tool")],
        tools: vec![echo_tool()],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e-parallel"));
    let msg1 = result_with_timeout(stream).await;
    assert_eq!(msg1.stop_reason, tack_ai::StopReason::ToolUse);
    let calls: Vec<_> = msg1
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "{:?}", msg1.content);
    assert_eq!(calls[0].0, "toolu_p1");
    assert_eq!(calls[0].2["text"], "one");
    assert_eq!(calls[1].0, "toolu_p2");
    assert_eq!(calls[1].2["text"], "two");

    // --- Turn 2: both results resolve; the session continues natively ---
    let mut context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("parallel-tool"),
            Message::Assistant(msg1.clone()),
        ],
        tools: vec![],
    };
    for (id, name, _) in &calls {
        context
            .messages
            .push(Message::ToolResult(tack_ai::ToolResultMessage {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                content: vec![tack_ai::InputContentBlock::text("pong")],
                details: None,
                usage: None,
                is_error: false,
                timestamp: 0,
            }));
    }
    let stream = provider.stream(&model, &context, options("cb-e2e-parallel"));
    let msg2 = result_with_timeout(stream).await;
    assert_eq!(msg2.stop_reason, tack_ai::StopReason::Stop);
    let tack_ai::ContentBlock::Text { text, .. } = &msg2.content[0] else {
        panic!("expected text, got {:?}", msg2.content)
    };
    assert_eq!(text, "tool said: pong");

    // Exactly one CLI process: no sync failure, no transcript respawn.
    let requests =
        std::fs::read_to_string(env.argv_dir.join("requests-mock-parallel.log")).unwrap();
    assert_eq!(
        requests.lines().filter(|l| l.starts_with("SPAWN")).count(),
        1,
        "{requests}"
    );
    assert_eq!(
        requests
            .lines()
            .filter(|l| l.starts_with("MCP-SEND tools/call"))
            .count(),
        2,
        "{requests}"
    );
}

/// Tangled parallel calls (real 2.156.0 + deepseek-v4-pro wire): both
/// tool_use blocks start up front sharing ONE content index, then both
/// input_json streams arrive on that index (call 1 sees nothing, call 2
/// sees both payloads concatenated). The streamed arguments are
/// unrecoverable; the provider must adopt the COMPLETE arguments from the
/// tools/call MCP frames dispatched after the boundary. Regression: a
/// real session executed `bash` with `{}` → "command is a required
/// property".
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_stream_event_parallel_tangled_args() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, env) = serial_mock_env().await;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-tangled");

    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("parallel-tangled")],
        tools: vec![echo_tool()],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e-tangled"));
    let msg1 = result_with_timeout(stream).await;
    assert_eq!(msg1.stop_reason, tack_ai::StopReason::ToolUse);
    let calls: Vec<_> = msg1
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "{:?}", msg1.content);
    // The MCP tools/call frames carry the complete arguments; the tangled
    // stream alone would have produced {} and a concat-parse failure.
    assert_eq!(calls[0].0, "toolu_t1");
    assert_eq!(calls[0].2["text"], "one", "args adopted from tools/call");
    assert_eq!(calls[1].0, "toolu_t2");
    assert_eq!(calls[1].2["text"], "two", "args adopted from tools/call");

    // Turn 2: both results resolve natively (request_id pairing survives
    // the adoption; results arrive in block order).
    let mut context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("parallel-tangled"),
            Message::Assistant(msg1.clone()),
        ],
        tools: vec![],
    };
    for (id, name, _) in &calls {
        context
            .messages
            .push(Message::ToolResult(tack_ai::ToolResultMessage {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                content: vec![tack_ai::InputContentBlock::text("pong")],
                details: None,
                usage: None,
                is_error: false,
                timestamp: 0,
            }));
    }
    let stream = provider.stream(&model, &context, options("cb-e2e-tangled"));
    let msg2 = result_with_timeout(stream).await;
    assert_eq!(msg2.stop_reason, tack_ai::StopReason::Stop);
    let tack_ai::ContentBlock::Text { text, .. } = &msg2.content[0] else {
        panic!("expected text, got {:?}", msg2.content)
    };
    assert_eq!(text, "tool said: pong");
    let requests = std::fs::read_to_string(env.argv_dir.join("requests-mock-tangled.log")).unwrap();
    assert_eq!(
        requests.lines().filter(|l| l.starts_with("SPAWN")).count(),
        1,
        "{requests}"
    );
}

/// Tool input delivered ENTIRELY at content_block_start (no input_json
/// deltas at all): the provider must seed arguments from
/// content_block.input (reference: arguments ?? {} + parsePartialJson
/// fallback). Regression for the stream-only args path.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_stream_event_tool_input_at_start() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, _env) = serial_mock_env().await;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-input-start");

    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("tool-input-at-start")],
        tools: vec![echo_tool()],
    };
    let mut stream = provider.stream(&model, &context, options("cb-e2e-input-start"));
    let mut end_args = None;
    while let Some(event) = stream.next().await {
        if let tack_ai::AssistantMessageEvent::ToolCallEnd { tool_call, .. } = event
            && let tack_ai::ContentBlock::ToolCall { arguments, .. } = tool_call
        {
            end_args = Some(arguments);
        }
    }
    let msg = stream.result().await;
    assert_eq!(msg.stop_reason, tack_ai::StopReason::ToolUse);
    assert_eq!(
        end_args.as_ref().and_then(|a| a.get("text")),
        Some(&json!("seeded")),
        "ToolCallEnd must carry the block-start seed, got {end_args:?}"
    );
    let tack_ai::ContentBlock::ToolCall { arguments, .. } = &msg.content[0] else {
        panic!("expected tool call, got {:?}", msg.content)
    };
    assert_eq!(arguments["text"], "seeded");
}

/// API retry mid-turn: the CLI abandons the first attempt mid-thinking
/// (its block never stops) and restarts with a second message_start,
/// reusing content index 0. The abandoned block must be finalized
/// (ThinkingEnd) so it neither dangles nor captures the retry's deltas.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_stream_event_retry_finalizes_thinking() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, _env) = serial_mock_env().await;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-retry");

    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("retry-thinking")],
        tools: vec![],
    };
    let mut stream = provider.stream(&model, &context, options("cb-e2e-retry"));
    let mut thinking_ends: Vec<(usize, String)> = Vec::new();
    while let Some(event) = stream.next().await {
        if let tack_ai::AssistantMessageEvent::ThinkingEnd {
            content_index,
            content,
            ..
        } = event
        {
            thinking_ends.push((content_index, content));
        }
    }
    let msg = stream.result().await;
    assert_eq!(msg.stop_reason, tack_ai::StopReason::Stop);
    let thinking: Vec<&str> = msg
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, vec!["first attempt", "second attempt"]);
    // The abandoned block's ThinkingEnd fired at the retry's
    // message_start; the retry's block ended at its content_block_stop.
    assert!(
        thinking_ends.contains(&(0, "first attempt".to_string())),
        "abandoned block must be finalized: {thinking_ends:?}"
    );
    assert!(
        thinking_ends.contains(&(1, "second attempt".to_string())),
        "{thinking_ends:?}"
    );
}

/// Divergence (edited history) triggers the JSONL native rebuild: the
/// provider rewrites CodeBuddy's session file and respawns with --resume;
/// the unresolved tail is then delivered natively to the resumed CLI.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_jsonl_rebuild_on_divergence() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, env) = serial_mock_env().await;
    let argv_dir = &env.argv_dir;
    let config_dir = &env.config_dir;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-jsonl");

    // Turn 1: establish the session (mock's init gives session id
    // "mock-session-1").
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("say hi")],
        tools: vec![],
    };
    let msg1 =
        result_with_timeout(provider.stream(&model, &context, options("cb-e2e-jsonl"))).await;
    assert_eq!(msg1.stop_reason, tack_ai::StopReason::Stop);

    // Turn 2 with EDITED history → fingerprint mismatch → native rebuild:
    // JSONL written with the settled prefix [u1, a1'], respawn --resume,
    // then the unresolved tail ("again") is delivered to the resumed CLI.
    let mut edited = msg1.clone();
    edited.content = vec![tack_ai::ContentBlock::text("echo: EDITED")];
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("say hi"),
            Message::Assistant(edited.clone()),
            Message::user("again"),
        ],
        tools: vec![],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e-jsonl"));
    let msg2 = result_with_timeout(stream).await;
    assert_eq!(msg2.stop_reason, tack_ai::StopReason::Stop);
    let tack_ai::ContentBlock::Text { text, .. } = &msg2.content[0] else {
        panic!("expected text, got {:?}", msg2.content)
    };
    assert_eq!(text, "echo: again");

    // The mock saw --resume and found the session file.
    let requests = std::fs::read_to_string(argv_dir.join("requests-mock-jsonl.log")).unwrap();
    assert!(
        requests.contains("RESUME mock-session-mock-jsonl OK"),
        "{requests}"
    );
    // The session file holds the settled prefix (2 records, chained).
    // (project hash = cwd with [/\\:] → '-', dashes collapsed — the mock
    // computes the same; cargo test's cwd is the crate root.)
    let cwd = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let mut hash = String::new();
    let mut last_dash = true;
    for c in cwd.trim_end_matches('/').chars() {
        let c = if matches!(c, '/' | '\\' | ':') {
            '-'
        } else {
            c
        };
        if c == '-' {
            if !last_dash {
                hash.push(c);
            }
            last_dash = true;
        } else {
            hash.push(c);
            last_dash = false;
        }
    }
    while hash.ends_with('-') {
        hash.pop();
    }
    let jsonl = config_dir
        .join("projects")
        .join(hash)
        .join("mock-session-mock-jsonl.jsonl");
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&jsonl)
        .unwrap_or_else(|e| panic!("read {}: {e}", jsonl.display()))
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["role"], "user");
    assert_eq!(lines[1]["role"], "assistant");
    assert_eq!(lines[1]["content"][0]["text"], "echo: EDITED");
    for line in &lines {
        assert_eq!(line["sessionId"], "mock-session-mock-jsonl");
    }

    // Turn 3: native continuation on the resumed session — no second
    // rebuild, no third spawn.
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("say hi"),
            Message::Assistant(edited),
            Message::user("again"),
            Message::Assistant(msg2.clone()),
            Message::user("once more"),
        ],
        tools: vec![],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e-jsonl"));
    let msg3 = result_with_timeout(stream).await;
    assert_eq!(msg3.stop_reason, tack_ai::StopReason::Stop);
    let requests = std::fs::read_to_string(argv_dir.join("requests-mock-jsonl.log")).unwrap();
    assert_eq!(
        requests.lines().filter(|l| l.starts_with("SPAWN")).count(),
        2,
        "initial spawn + resume respawn only: {requests}"
    );
    assert_eq!(
        requests.lines().filter(|l| l.starts_with("RESUME")).count(),
        1,
        "{requests}"
    );
}

/// Cross-process resume: after the session registry is drained (simulated
/// tack restart), the first rebuild reuses the PERSISTED codebuddy
/// session id instead of rotating to a fresh one.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_cross_process_resume_reuses_cb_session() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, env) = serial_mock_env().await;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-xresume");

    // Turn 1: establishes the session; the init line's session id
    // ("mock-session-1") is persisted into the mapping.
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("say hi")],
        tools: vec![],
    };
    let msg1 =
        result_with_timeout(provider.stream(&model, &context, options("cb-e2e-xresume"))).await;
    assert_eq!(msg1.stop_reason, tack_ai::StopReason::Stop);

    // Simulate a tack restart: this session's registry entry is gone,
    // the mapping on disk survives. (close_session, NOT close_all — other
    // tests share the process-wide registry.)
    tack_ai::codebuddy::close_session("cb-e2e-xresume").await;

    // Turn 2 with continued history: a fresh registry entry can't sync
    // (assistant in tail) → native rebuild → must reuse "mock-session-1".
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("say hi"),
            Message::Assistant(msg1.clone()),
            Message::user("again"),
        ],
        tools: vec![],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e-xresume"));
    let msg2 = result_with_timeout(stream).await;
    assert_eq!(msg2.stop_reason, tack_ai::StopReason::Stop);
    let tack_ai::ContentBlock::Text { text, .. } = &msg2.content[0] else {
        panic!("expected text, got {:?}", msg2.content)
    };
    assert_eq!(text, "echo: again");

    let requests = std::fs::read_to_string(env.argv_dir.join("requests-mock-xresume.log")).unwrap();
    assert!(
        requests.contains("RESUME mock-session-mock-xresume OK"),
        "rebuild must resume the persisted session id: {requests}"
    );
    assert_eq!(
        requests.lines().filter(|l| l.starts_with("SPAWN")).count(),
        2,
        "initial spawn + resume respawn: {requests}"
    );
}

/// One-shot delegation (AskCodebuddy parity): fresh CLI, one prompt,
/// answer collected; spawn flags carry the mode's disallowedTools and
/// user,project setting sources (unlike provider sessions).
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_ask_codebuddy_one_shot() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, env) = serial_mock_env().await;
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tack_ai::codebuddy::ask_codebuddy(
            "hello delegate",
            tack_ai::codebuddy::AskMode::Read,
            None,
            None,
            CancellationToken::new(),
            &|_| {},
        ),
    )
    .await
    .expect("ask_codebuddy timed out after 30s — provider is stuck")
    .unwrap();
    assert_eq!(outcome.text, "echo: hello delegate");

    // No --model was passed → the mock logs argv under "unknown".
    let argv: Vec<String> = serde_json::from_str(
        &std::fs::read_to_string(env.argv_dir.join("argv-unknown.json")).unwrap(),
    )
    .unwrap();
    let has = |flag: &str| argv.iter().any(|a| a == flag);
    assert!(has("--disallowedTools"), "{argv:?}");
    assert!(has("Write"), "read mode blocks writes: {argv:?}");
    assert!(has("AskUserQuestion"), "always-blocked: {argv:?}");
    assert!(
        argv.windows(2)
            .any(|w| w == ["--setting-sources", "user,project"]),
        "{argv:?}"
    );
    assert!(
        !has("--tools"),
        "delegation keeps CodeBuddy's own tools: {argv:?}"
    );
    assert!(!has("--allowedTools"), "{argv:?}");
}

/// Model-only switches hot-swap via the set_model control request (SDK
/// Query.setModel parity): no respawn, CLI session + cache kept.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_set_model_hot_switch() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, env) = serial_mock_env().await;
    let argv_dir = &env.argv_dir;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);

    // Turn 1 on mock-hot-1.
    let model1 = model_with_id("mock-hot-1");
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("say hi")],
        tools: vec![],
    };
    let msg1 =
        result_with_timeout(provider.stream(&model1, &context, options("cb-e2e-setmodel"))).await;
    assert_eq!(msg1.stop_reason, tack_ai::StopReason::Stop);

    // Turn 2 on mock-hot-2: model-only change → set_model, NOT a respawn.
    let model2 = model_with_id("mock-hot-2");
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("say hi"),
            Message::Assistant(msg1.clone()),
            Message::user("say more"),
        ],
        tools: vec![],
    };
    let msg2 =
        result_with_timeout(provider.stream(&model2, &context, options("cb-e2e-setmodel"))).await;
    assert_eq!(msg2.stop_reason, tack_ai::StopReason::Stop);
    let tack_ai::ContentBlock::Text { text, .. } = &msg2.content[0] else {
        panic!("expected text, got {:?}", msg2.content)
    };
    assert_eq!(text, "echo: say more");

    // No respawn: mock-hot-2 was never spawned, and the set_model control
    // request reached the (single) CLI process.
    assert!(
        !argv_dir.join("argv-mock-hot-2.json").exists(),
        "model switch must not respawn the CLI"
    );
    let requests = std::fs::read_to_string(argv_dir.join("requests-mock-hot-1.log")).unwrap();
    assert_eq!(
        requests.lines().filter(|l| l.starts_with("SPAWN")).count(),
        1,
        "{requests}"
    );
    assert!(
        requests.contains("CONTROL set_model mock-hot-2"),
        "{requests}"
    );
}

/// Abort mid-generation (no parked tool calls) keeps the CLI session: the
/// interrupted turn is drained and the next turn continues natively — no
/// rebuild, no respawn.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_abort_without_parked_keeps_session() {
    if !python3_or_skip() {
        return;
    }
    let (_serial, env) = serial_mock_env().await;
    let argv_dir = &env.argv_dir;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = model_with_id("mock-abort");

    // Turn 1: normal.
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("say hi")],
        tools: vec![],
    };
    let msg1 =
        result_with_timeout(provider.stream(&model, &context, options("cb-e2e-abort"))).await;
    assert_eq!(msg1.stop_reason, tack_ai::StopReason::Stop);

    // Turn 2: "slow" stays in-flight; cancel mid-generation.
    let cancel = CancellationToken::new();
    let mut opts = options("cb-e2e-abort");
    opts.cancel = cancel.clone();
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("say hi"),
            Message::Assistant(msg1.clone()),
            Message::user("slow please"),
        ],
        tools: vec![],
    };
    let stream = provider.stream(&model, &context, opts);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        cancel.cancel();
    });
    let msg2 = result_with_timeout(stream).await;
    assert_eq!(msg2.stop_reason, tack_ai::StopReason::Aborted);

    // Turn 3: continues natively on the SAME session (aborted user message
    // stays in history; the partial assistant turn does not).
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("say hi"),
            Message::Assistant(msg1.clone()),
            Message::user("slow please"),
            Message::user("say bye"),
        ],
        tools: vec![],
    };
    let msg3 =
        result_with_timeout(provider.stream(&model, &context, options("cb-e2e-abort"))).await;
    assert_eq!(msg3.stop_reason, tack_ai::StopReason::Stop);
    let tack_ai::ContentBlock::Text { text, .. } = &msg3.content[0] else {
        panic!("expected text, got {:?}", msg3.content)
    };
    assert_eq!(text, "echo: say bye");

    // Exactly one CLI process ever served this session.
    let requests = std::fs::read_to_string(argv_dir.join("requests-mock-abort.log")).unwrap();
    assert_eq!(
        requests.lines().filter(|l| l.starts_with("SPAWN")).count(),
        1,
        "{requests}"
    );
    assert!(requests.contains("CONTROL interrupt"), "{requests}");
}

/// Full flow: plain text turn, then a tool round trip where the mock
/// CLI's SDK-MCP tools/call (an mcp_message control_request) stays parked
/// until the next stream() resolves it, and the CLI session continues
/// natively — including the stale assistant echo the real CLI re-yields
/// after the tool call resolves.
#[cfg_attr(
    windows,
    ignore = "multi-turn mock e2e hangs on Windows CI; Windows spawn path covered by spawn_cli_runs_cmd_shim"
)]
#[tokio::test(flavor = "multi_thread")]
async fn codebuddy_stream_text_and_tool_bridge() {
    if !python3_or_skip() {
        return;
    }
    // process-global by design; this suite relies on it.
    let (_serial, env) = serial_mock_env().await;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::codebuddy::CodeBuddyStreamProvider);
    let model = mock_model();

    // --- Turn 1: plain text -------------------------------------------------
    let context = Context {
        system_prompt: None,
        messages: vec![Message::user("say hi")],
        tools: vec![],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e"));
    let msg1 = result_with_timeout(stream).await;
    assert_eq!(msg1.stop_reason, tack_ai::StopReason::Stop);
    let tack_ai::ContentBlock::Text { text, .. } = &msg1.content[0] else {
        panic!("expected text, got {:?}", msg1.content)
    };
    assert_eq!(text, "echo: say hi");
    // result.usage (aggregate) fills input/output; total_tokens keeps the
    // last per-request size (5+3) for context estimation.
    assert_eq!(msg1.usage.input, 5);
    assert_eq!(msg1.usage.total_tokens, 8);

    // --- Turn 2: tool bridge -------------------------------------------------
    let echo_tool = ToolDefinition {
        name: "echo".into(),
        description: "echo back".into(),
        parameters: json!({"type":"object","properties":{"text":{"type":"string"}}}),
        defer_loading: false,
        constrained_sampling: None,
    };
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("say hi"),
            Message::Assistant(msg1.clone()),
            Message::user("use-echo-tool"),
        ],
        tools: vec![echo_tool],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e"));
    let msg2 = result_with_timeout(stream).await;
    assert_eq!(msg2.stop_reason, tack_ai::StopReason::ToolUse);
    let tack_ai::ContentBlock::ToolCall {
        id,
        name,
        arguments,
        ..
    } = &msg2.content[0]
    else {
        panic!("expected tool call, got {:?}", msg2.content)
    };
    assert_eq!(name, "echo");
    assert_eq!(id, "toolu_1");
    assert_eq!(arguments["text"], "hello-from-tool");

    // The mock plays the CLI's SDK-MCP client itself: it issued the
    // tools/call as an mcp_message control_request right after the tool
    // boundary and is parked waiting for turn 3's sync to resolve it.

    // --- Turn 3: deliver the tool result; CLI session continues --------------

    let tool_result = tack_ai::ToolResultMessage {
        tool_call_id: "toolu_1".into(),
        tool_name: "echo".into(),
        content: vec![tack_ai::InputContentBlock::text("pong")],
        details: None,
        usage: None,
        is_error: false,
        timestamp: 0,
    };
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::user("say hi"),
            Message::Assistant(msg1.clone()),
            Message::user("use-echo-tool"),
            Message::Assistant(msg2.clone()),
            Message::ToolResult(tool_result),
        ],
        tools: vec![],
    };
    let stream = provider.stream(&model, &context, options("cb-e2e"));
    let msg3 = result_with_timeout(stream).await;
    assert_eq!(msg3.stop_reason, tack_ai::StopReason::Stop);
    let tack_ai::ContentBlock::Text { text, .. } = &msg3.content[0] else {
        panic!("expected text, got {:?}", msg3.content)
    };
    assert_eq!(text, "tool said: pong");
    // Two API requests (10+20 input, 5+8 output): the aggregate (30/13) is
    // kept for totals, but total_tokens must be the LAST per-request size
    // (20+8=28), not the 43-token aggregate — context is not counted twice.
    assert_eq!(msg3.usage.input, 30);
    assert_eq!(msg3.usage.total_tokens, 28);

    // The provider declared the SDK MCP server at initialize and answered
    // the mock's tools/call with the tool result.
    let requests = std::fs::read_to_string(env.argv_dir.join("requests-mock-1.log")).unwrap();
    assert!(
        requests.contains("CONTROL initialize sdkMcpServers=tack"),
        "{requests}"
    );
    assert!(requests.contains("MCP-SEND tools/call"), "{requests}");
}
