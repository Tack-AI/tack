//! Repro: drive the REAL codebuddy CLI through tool turns.
//! Run: RUST_LOG=tack_ai=debug cargo run -p tack-ai --example cb_repro -- [model-id] [prompt]
#![allow(clippy::unwrap_used)]

use serde_json::json;
use tack_ai::codebuddy::CodeBuddyStreamProvider;
use tack_ai::{
    Context, InputContentBlock, Message, Provider, StreamOptions, ToolDefinition,
    ToolResultMessage, UserContent, UserMessage,
};
use tokio_util::sync::CancellationToken;

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
    let mut args = std::env::args().skip(1);
    let model_id = args
        .next()
        .unwrap_or_else(|| models.first().map(|m| m.id.clone()).unwrap_or_default());
    let prompt = args.next().unwrap_or_else(|| {
        "Use the bash tool to run exactly `echo hello-world`, then tell me what it printed.".into()
    });
    let model = models
        .iter()
        .find(|m| m.id == model_id)
        .unwrap_or_else(|| panic!("model {model_id} not in cache"))
        .clone();
    println!("using model {}", model.id);

    let provider = CodeBuddyStreamProvider;
    let options = StreamOptions {
        session_id: Some(format!("cb-repro-{}", std::process::id())),
        cancel: CancellationToken::new(),
        ..Default::default()
    };

    let mut context = Context {
        system_prompt: Some("You are a helpful assistant.".into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(prompt),
            timestamp: now(),
        })],
        tools: vec![bash_tool()],
    };

    // Mini agent loop: answer tool calls (fake output) until the model stops.
    for turn in 1..=6 {
        println!("=== TURN {turn} ===");
        let stream = provider.stream(&model, &context, options.clone());
        let message = tokio::time::timeout(std::time::Duration::from_secs(240), stream.result())
            .await
            .expect("TURN TIMED OUT — provider stuck");
        println!(
            "[turn{turn}] done: stop={:?} err={:?}",
            message.stop_reason, message.error_message
        );
        let mut calls = Vec::new();
        for block in &message.content {
            match block {
                tack_ai::ContentBlock::Text { text, .. } => println!("[turn{turn}][text] {text}"),
                tack_ai::ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    println!("[turn{turn}][tool_call] {name}({arguments}) id={id}");
                    calls.push((id.clone(), name.clone()));
                }
                _ => {}
            }
        }
        if calls.is_empty() {
            println!("=== OK (stopped after {turn} turn(s)) ===");
            return;
        }
        context.messages.push(Message::Assistant(message));
        for (id, name) in calls {
            context
                .messages
                .push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: id,
                    tool_name: name,
                    content: vec![InputContentBlock::text("hello-world\n")],
                    details: None,
                    usage: None,
                    is_error: false,
                    timestamp: now(),
                }));
        }
    }
    panic!("model kept calling tools after 6 turns");
}
