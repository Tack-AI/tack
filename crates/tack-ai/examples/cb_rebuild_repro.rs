//! Repro: verify a native JSONL rebuild keeps the REAL codebuddy CLI
//! calling tools. Simulates the post-compaction path: turn 1 establishes a
//! session, turn 2 diverges the history (a settled tool turn, as a
//! compaction would leave it) → native rebuild + --resume → the model must
//! STILL issue a bash tool call (the lossy text-marker projection made it
//! answer with literal "[thinking]…" text and stop instead).
//!
//! The follow-up asks for UNGUESSABLE output (`jot -r`) so the model
//! cannot shortcut the tool call; a control phase runs the same prompt on
//! a fresh (non-rebuilt) session for comparison.
//!
//! Run: CODEBUDDY_CONFIG_DIR=<writable-copy> cargo run -p tack-ai --example cb_rebuild_repro -- [model-id]
#![allow(clippy::unwrap_used)]

use serde_json::json;
use tack_ai::codebuddy::CodeBuddyStreamProvider;
use tack_ai::{
    ContentBlock, Context, InputContentBlock, Message, Provider, StreamOptions, ToolDefinition,
    ToolResultMessage, UserMessage,
};
use tokio_util::sync::CancellationToken;

const UNGUESSABLE: &str = "Use the bash tool to run exactly `jot -r 1 100000 999999`, then tell me the number it printed.";

fn bash_tool() -> ToolDefinition {
    ToolDefinition {
        name: "bash".into(),
        description: "Run a bash command and return its output".into(),
        parameters: json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        }),
        defer_loading: false,
        constrained_sampling: None,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn text_of(message: &tack_ai::AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

struct TurnOut {
    called_bash: bool,
    text: String,
}

/// Drive turns until the model stops calling tools (fake tool output
/// "424242\n" — the number the model must end up reporting).
async fn run_until_stop(
    provider: &CodeBuddyStreamProvider,
    model: &tack_ai::Model,
    mut context: Context,
    options: StreamOptions,
    label: &str,
) -> TurnOut {
    let mut called_bash = false;
    for turn in 1..=4 {
        let stream = provider.stream(model, &context, options.clone());
        let message = tokio::time::timeout(std::time::Duration::from_secs(240), stream.result())
            .await
            .unwrap_or_else(|_| panic!("{label} turn {turn} TIMED OUT — provider stuck"));
        let mut calls = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } => println!("[{label}][turn{turn}][text] {text}"),
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    println!("[{label}][turn{turn}][tool_call] {name}({arguments})");
                    calls.push((id.clone(), name.clone()));
                }
                _ => {}
            }
        }
        if calls.is_empty() {
            return TurnOut {
                called_bash,
                text: text_of(&message),
            };
        }
        called_bash = true;
        context.messages.push(Message::Assistant(message));
        for (id, name) in calls {
            context
                .messages
                .push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: id,
                    tool_name: name,
                    content: vec![InputContentBlock::text("424242\n")],
                    details: None,
                    usage: None,
                    is_error: false,
                    timestamp: now(),
                }));
        }
    }
    panic!("{label}: model kept calling tools after 4 turns")
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tack_ai=debug".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    tack_ai::codebuddy::refresh().await;
    let models = tack_ai::codebuddy::models();
    let model_id = std::env::args()
        .nth(1)
        .unwrap_or_else(|| models.first().map(|m| m.id.clone()).unwrap_or_default());
    let model = models
        .iter()
        .find(|m| m.id == model_id)
        .unwrap_or_else(|| panic!("model {model_id} not in cache"))
        .clone();
    println!("using model {}", model.id);
    let provider = CodeBuddyStreamProvider;

    // Phase A (control): fresh session, unguessable prompt.
    let control = run_until_stop(
        &provider,
        &model,
        Context {
            system_prompt: Some("You are a helpful assistant.".into()),
            messages: vec![Message::user(UNGUESSABLE)],
            tools: vec![bash_tool()],
        },
        StreamOptions {
            session_id: Some(format!("cb-ctrl-{}", std::process::id())),
            cancel: CancellationToken::new(),
            ..Default::default()
        },
        "control",
    )
    .await;
    println!(
        "[control] called_bash={} reports_424242={}",
        control.called_bash,
        control.text.contains("424242")
    );

    // Phase B: establish a session, then rebuild with post-compaction
    // shaped history (settled tool turn) and the same unguessable prompt.
    let options = StreamOptions {
        session_id: Some(format!("cb-rebuild-{}", std::process::id())),
        cancel: CancellationToken::new(),
        ..Default::default()
    };
    let stream = provider.stream(
        &model,
        &Context {
            system_prompt: Some("You are a helpful assistant.".into()),
            messages: vec![Message::user("say hi")],
            tools: vec![bash_tool()],
        },
        options.clone(),
    );
    let msg1 = tokio::time::timeout(std::time::Duration::from_secs(240), stream.result())
        .await
        .expect("setup turn TIMED OUT — provider stuck");
    println!("[setup] stop={:?}", msg1.stop_reason);

    // A realistic compaction tail: several varied settled bash turns, so
    // the resumed history establishes tool use as a pattern (a single
    // degenerate one-example history lets the model over-generalize).
    let mut settled: Vec<Message> = Vec::new();
    for (i, (cmd, out)) in [
        (
            "ls -la",
            "total 8\n-rw-r--r-- 1 zugle staff 12 Sep 30 a.txt",
        ),
        ("git status --short", "M README.md"),
        ("date +%F", "2026-09-30"),
    ]
    .into_iter()
    .enumerate()
    {
        settled.push(Message::Assistant(tack_ai::AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: format!("Run `{cmd}` to see."),
                    thinking_signature: None,
                    redacted: None,
                },
                ContentBlock::ToolCall {
                    id: format!("mcp__tack__bash_{i}_33cc44dd"),
                    name: "bash".into(),
                    arguments: json!({"command": cmd}),
                    thought_signature: None,
                    namespace: None,
                },
            ],
            api: msg1.api.clone(),
            provider: msg1.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            diagnostics: None,
            usage: tack_ai::Usage::zero(),
            stop_reason: tack_ai::StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now(),
        }));
        settled.push(Message::ToolResult(ToolResultMessage {
            tool_call_id: format!("mcp__tack__bash_{i}_33cc44dd"),
            tool_name: "bash".into(),
            content: vec![InputContentBlock::text(out)],
            details: None,
            usage: None,
            is_error: false,
            timestamp: now(),
        }));
    }
    let mut messages = vec![Message::User(UserMessage {
        content: tack_ai::UserContent::Text(
            "The conversation history before this point was compacted into a \
             summary: the user is testing tool calls."
                .into(),
        ),
        timestamp: now(),
    })];
    messages.extend(settled);
    messages.push(Message::user(UNGUESSABLE));
    let rebuilt = run_until_stop(
        &provider,
        &model,
        Context {
            system_prompt: Some("You are a helpful assistant.".into()),
            messages,
            tools: vec![bash_tool()],
        },
        options,
        "rebuilt",
    )
    .await;
    println!(
        "[rebuilt] called_bash={} reports_424242={}",
        rebuilt.called_bash,
        rebuilt.text.contains("424242")
    );

    let mut failed = false;
    if !control.called_bash || !control.text.contains("424242") {
        println!(
            "CONTROL WEAK: fresh session also skipped the tool call (model behavior baseline)"
        );
    }
    if !rebuilt.called_bash {
        println!("REGRESSION: post-rebuild model never called tools");
        failed = true;
    }
    if rebuilt.called_bash && !rebuilt.text.contains("424242") {
        println!("REGRESSION: post-rebuild model called tools but lost the thread");
        failed = true;
    }
    if failed {
        std::process::exit(1);
    }
    println!("=== OK: post-rebuild model kept calling tools ===");
}
