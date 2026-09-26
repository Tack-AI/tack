//! Reproduction: cancel a run while a tool is mid-execution — the aborted
//! tool result must still be persisted to the session (a missing result
//! leaves a dangling tool call that transform backfills with the synthetic
//! "No result provided" placeholder on the next request).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tack_agent_core::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool, AgentToolResult,
    ToolExecutionMode, agent_loop,
};
use tack_ai::{
    AssistantMessage, AssistantMessageEventStream, ContentBlock, Model, Provider, StopReason,
    StreamOptions, event_stream,
};
use tack_session::SessionManager;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Scripted provider + cancel-aware slow tool (mirrors loop_tests fixtures)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ScriptedProvider {
    scripts: std::sync::Mutex<Vec<AssistantMessage>>,
}

impl ScriptedProvider {
    fn with_scripts(scripts: Vec<AssistantMessage>) -> Arc<Self> {
        Arc::new(ScriptedProvider {
            scripts: std::sync::Mutex::new(scripts),
        })
    }
}

impl Provider for ScriptedProvider {
    fn stream(
        &self,
        model: &Model,
        _context: &tack_ai::Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let (sender, stream) = event_stream();
        let message = {
            let mut scripts = self.scripts.lock().unwrap();
            if scripts.is_empty() {
                let mut m = AssistantMessage::pending(model);
                m.stop_reason = StopReason::Error;
                m.error_message = Some("scripted provider: no script left".into());
                m
            } else {
                scripts.remove(0)
            }
        };
        tokio::spawn(async move {
            let _ = sender.push(tack_ai::AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            let _ = sender.push(tack_ai::AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
        });
        stream
    }
}

fn test_model() -> Model {
    Model {
        id: "mock".to_string(),
        name: "mock".to_string(),
        api: "mock".to_string(),
        provider: "mock".to_string(),
        base_url: "http://localhost".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![tack_ai::InputKind::Text],
        cost: Default::default(),
        context_window: 200_000,
        max_tokens: 8192,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn assistant_with_tool_calls(calls: &[(&str, &str, Value)]) -> AssistantMessage {
    let mut m = AssistantMessage::pending(&test_model());
    m.stop_reason = StopReason::ToolUse;
    m.content = calls
        .iter()
        .map(|(id, name, args)| ContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.clone(),
            thought_signature: None,
            namespace: None,
        })
        .collect();
    m
}

fn assistant_text(text: &str) -> AssistantMessage {
    let mut m = AssistantMessage::pending(&test_model());
    m.stop_reason = StopReason::Stop;
    m.content = vec![ContentBlock::text(text)];
    m
}

/// A slow tool that, like the real bash tool, returns Err when the run's
/// cancel token fires mid-execution.
#[derive(Debug)]
struct SlowBashLikeTool {
    started: Arc<std::sync::Mutex<bool>>,
}

#[async_trait]
impl AgentTool for SlowBashLikeTool {
    fn name(&self) -> &'static str {
        "slowbash"
    }
    fn label(&self) -> &str {
        "slowbash"
    }
    fn description(&self) -> &str {
        "sleeps until cancelled"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _id: &str,
        _params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        *self.started.lock().unwrap() = true;
        // Wait up to 60s, returning the bash tool's Err on cancellation.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(60)) => {
                Ok(AgentToolResult::text("finished"))
            }
            _ = cancel.cancelled() => {
                Err("Command aborted".to_string())
            }
        }
    }
}

struct NoopHooks;
#[async_trait]
impl tack_agent_core::AgentHooks for NoopHooks {}

fn make_config(provider: Arc<dyn Provider>) -> AgentLoopConfig {
    AgentLoopConfig {
        model: test_model(),
        provider,
        hooks: Arc::new(NoopHooks),
        tool_execution: ToolExecutionMode::Parallel,
        reasoning: None,
        auth: Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        max_tokens: None,
        temperature: None,
        session_id: None,
        cache_retention: None,
        fallback_models: vec![],
        tool_pool: vec![],
        retry_cancel: None,
    }
}

// ---------------------------------------------------------------------------
// The repro
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_mid_tool_execution_still_persists_the_tool_result() {
    let cwd = tempfile::tempdir().unwrap();
    let session_dir = cwd.path().join("sessions");
    let manager = SessionManager::create(cwd.path(), Some(session_dir)).unwrap();
    let session_file = manager.session_file().unwrap().to_path_buf();
    let session_handle = Arc::new(Mutex::new(manager));

    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "slowbash", json!({}))]),
        assistant_text("done"),
    ]);
    let started = Arc::new(std::sync::Mutex::new(false));
    let tool = Arc::new(SlowBashLikeTool {
        started: started.clone(),
    });

    let cancel = CancellationToken::new();
    let context = AgentContext {
        system_prompt: None,
        messages: vec![],
        tools: vec![tool],
    };
    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        context,
        make_config(provider),
        cancel.clone(),
    );

    // The run.rs pump, faithfully: persist every MessageEnd message.
    let pump_session = session_handle.clone();
    let (done_tx, done_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            if let AgentEvent::MessageEnd { message } = &event
                && !matches!(message, AgentMessage::Custom(_))
            {
                pump_session
                    .lock()
                    .await
                    .append_message(message.clone())
                    .unwrap();
            }
        }
        let _ = stream.result().await;
        let _ = done_tx.send(());
    });

    // Wait for the tool to be mid-execution, then cancel (Esc/sendNow).
    for _ in 0..100 {
        if *started.lock().unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(*started.lock().unwrap(), "tool never started");
    cancel.cancel();
    done_rx.await.unwrap();

    // The session must contain a toolResult for t1 — otherwise the next
    // run's transform backfills the synthetic placeholder.
    let manager = SessionManager::open(&session_file, None).unwrap();
    let messages = manager.build_session_context().messages;
    let results: Vec<&AgentMessage> = messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::ToolResult(_)))
        .collect();
    assert_eq!(
        results.len(),
        1,
        "aborted tool result missing from the session: {messages:?}"
    );
    let AgentMessage::ToolResult(result) = results[0] else {
        unreachable!()
    };
    assert_eq!(result.tool_call_id, "t1");
    assert!(result.is_error, "aborted result must be an error");
}

/// Same scenario with the REAL tack-tools bash tool running a long command —
/// the closest reproduction of the incident (cancel kills the process
/// tree mid-command; the aborted result must still be persisted).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_real_bash_command_still_persists_the_tool_result() {
    let cwd = tempfile::tempdir().unwrap();
    let session_dir = cwd.path().join("sessions");
    let manager = SessionManager::create(cwd.path(), Some(session_dir)).unwrap();
    let session_file = manager.session_file().unwrap().to_path_buf();
    let session_handle = Arc::new(Mutex::new(manager));

    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "bash", json!({"command": "sleep 300"}))]),
        assistant_text("done"),
    ]);
    let services = tack_tools::default_services(cwd.path().to_path_buf());
    let tool: Arc<dyn AgentTool> = Arc::new(tack_tools::bash::BashTool::new(services));

    let cancel = CancellationToken::new();
    let context = AgentContext {
        system_prompt: None,
        messages: vec![],
        tools: vec![tool],
    };
    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        context,
        make_config(provider),
        cancel.clone(),
    );

    let pump_session = session_handle.clone();
    let (done_tx, done_rx) = oneshot::channel::<()>();
    let (started_tx, mut started_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                let _ = started_tx.try_send(());
            }
            if let AgentEvent::MessageEnd { message } = &event
                && !matches!(message, AgentMessage::Custom(_))
            {
                pump_session
                    .lock()
                    .await
                    .append_message(message.clone())
                    .unwrap();
            }
        }
        let _ = stream.result().await;
        let _ = done_tx.send(());
    });

    // Cancel once the sleep is actually running, then let the run finish.
    tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
        .await
        .expect("tool never started");
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(30), done_rx)
        .await
        .expect("run never finished after cancel");

    let manager = SessionManager::open(&session_file, None).unwrap();
    let messages = manager.build_session_context().messages;
    let results: Vec<&AgentMessage> = messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::ToolResult(_)))
        .collect();
    assert_eq!(
        results.len(),
        1,
        "aborted bash result missing from the session: {messages:?}"
    );
    let AgentMessage::ToolResult(result) = results[0] else {
        unreachable!()
    };
    assert_eq!(result.tool_call_id, "t1");
    assert!(result.is_error, "aborted result must be an error");
}
