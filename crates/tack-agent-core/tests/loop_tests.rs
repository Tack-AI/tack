#![allow(clippy::unwrap_used)]
//! Loop scenario tests driven by a scripted mock provider.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tack_agent_core::*;
use tack_ai::*;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// ScriptedProvider: replays a queue of scripted assistant messages.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ScriptedProvider {
    scripts: Mutex<Vec<AssistantMessage>>,
    /// Snapshot of the LLM-visible context on each call.
    seen_contexts: Arc<Mutex<Vec<Context>>>,
}

impl ScriptedProvider {
    fn with_scripts(scripts: Vec<AssistantMessage>) -> Arc<Self> {
        Arc::new(ScriptedProvider {
            scripts: Mutex::new(scripts),
            seen_contexts: Arc::new(Mutex::new(Vec::new())),
        })
    }
}

impl Provider for ScriptedProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.seen_contexts.lock().unwrap().push(context.clone());
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
            let _ = sender.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            // Emit coarse-grained content events for realism.
            for (i, block) in message.content.iter().enumerate() {
                match block {
                    ContentBlock::Text { text, .. } => {
                        let _ = sender.push(AssistantMessageEvent::TextStart {
                            content_index: i,
                            partial: message.clone(),
                        });
                        let _ = sender.push(AssistantMessageEvent::TextDelta {
                            content_index: i,
                            delta: text.clone(),
                            partial: message.clone(),
                        });
                        let _ = sender.push(AssistantMessageEvent::TextEnd {
                            content_index: i,
                            content: text.clone(),
                            partial: message.clone(),
                        });
                    }
                    ContentBlock::ToolCall { .. } => {
                        let _ = sender.push(AssistantMessageEvent::ToolCallStart {
                            content_index: i,
                            partial: message.clone(),
                        });
                        let _ = sender.push(AssistantMessageEvent::ToolCallEnd {
                            content_index: i,
                            tool_call: block.clone(),
                            partial: message.clone(),
                        });
                    }
                    _ => {}
                }
            }
            match message.stop_reason {
                StopReason::Error | StopReason::Aborted => {
                    sender.finish(AssistantMessageEvent::Error {
                        reason: message.stop_reason,
                        error: message,
                    });
                }
                reason => {
                    sender.finish(AssistantMessageEvent::Done { reason, message });
                }
            }
        });
        stream
    }
}

// ---------------------------------------------------------------------------
// Mock tools
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct MockTool {
    name: &'static str,
    delay: Duration,
    calls: Arc<Mutex<Vec<String>>>,
    terminate: bool,
}

#[async_trait]
impl AgentTool for MockTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn label(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "mock tool"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "input": { "type": "string" } } })
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        tokio::time::sleep(self.delay).await;
        self.calls
            .lock()
            .unwrap()
            .push(format!("{tool_call_id}:{params}"));
        let mut result = AgentToolResult::text(format!("ok:{tool_call_id}"));
        result.terminate = self.terminate;
        Ok(result)
    }
}

fn test_model() -> Model {
    Model {
        id: "mock".to_string(),
        name: "Mock".to_string(),
        api: "mock".to_string(),
        provider: "mock".to_string(),
        base_url: "http://localhost".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: ModelCost::default(),
        context_window: 100_000,
        max_tokens: 4096,
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

fn make_config(provider: Arc<dyn Provider>, hooks: Arc<dyn AgentHooks>) -> AgentLoopConfig {
    AgentLoopConfig {
        model: test_model(),
        provider,
        hooks,
        tool_execution: ToolExecutionMode::Parallel,
        reasoning: None,
        auth: std::sync::Arc::new(tack_ai::oauth::StaticAuth::default()),
        max_tokens: None,
        temperature: None,
        session_id: None,
        cache_retention: None,
        fallback_models: Vec::new(),
        tool_pool: Vec::new(),
        retry_cancel: None,
    }
}

async fn collect_events(
    mut stream: tack_ai::EventStream<AgentEvent, Vec<AgentMessage>>,
) -> (Vec<AgentEvent>, Vec<AgentMessage>) {
    let mut events = Vec::new();
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        let terminal = event.is_terminal();
        events.push(event);
        if terminal {
            break;
        }
    }
    let messages = stream.result().await;
    (events, messages)
}

fn tags(events: &[AgentEvent]) -> Vec<&'static str> {
    events.iter().map(AgentEvent::tag).collect()
}

/// Messages minus transcript-state system messages (upstream #9548): runs
/// whose tool loadout is not yet declared in the transcript begin with a
/// system update recording the declarations, which index-based assertions
/// ignore here.
fn without_system(messages: &[AgentMessage]) -> Vec<&AgentMessage> {
    messages
        .iter()
        .filter(|m| !matches!(m, AgentMessage::System(_)))
        .collect()
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_turn_tool_use() {
    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "mock", json!({"input": "a"}))]),
        assistant_text("done"),
    ]);
    let seen = provider.seen_contexts.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(MockTool {
        name: "mock",
        delay: Duration::ZERO,
        calls: calls.clone(),
        terminate: false,
    });

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![tool],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );
    let (events, messages) = collect_events(stream).await;

    // Event order: agent_start, turn_start, prompt msg start/end, assistant
    // msg lifecycle, tool execution, tool result msg, turn_end, turn_start,
    // second assistant msg, turn_end, agent_end.
    let tags = tags(&events);
    assert_eq!(tags[0], "agent_start");
    assert_eq!(tags[1], "turn_start");
    assert_eq!(tags.last(), Some(&"agent_end"));
    let turn_ends = tags.iter().filter(|t| **t == "turn_end").count();
    assert_eq!(turn_ends, 2);
    assert!(tags.contains(&"tool_execution_start"));
    assert!(tags.contains(&"tool_execution_end"));

    // Final messages: system tool declaration (first run declares the
    // loadout), user prompt, assistant w/ tool call, tool result, final
    // assistant.
    assert_eq!(messages.len(), 5);
    let AgentMessage::System(system) = &messages[0] else {
        panic!("expected leading system declaration, got {:?}", messages[0])
    };
    assert_eq!(
        system.tools_added.as_ref().map(|t| t.len()),
        Some(1),
        "initial loadout declared on the first run"
    );
    let messages = without_system(&messages);
    assert_eq!(messages.len(), 4);
    assert!(matches!(messages[2], AgentMessage::ToolResult(t) if !t.is_error));

    // Second LLM call saw the tool result.
    let second_call = &seen.lock().unwrap()[1];
    assert!(matches!(
        second_call.messages.last(),
        Some(Message::ToolResult(_))
    ));
}

#[tokio::test]
async fn truncated_message_tool_calls_are_failed_not_executed() {
    let mut truncated = assistant_with_tool_calls(&[("t1", "mock", json!({"input": "a"}))]);
    truncated.stop_reason = StopReason::Length;
    let provider = ScriptedProvider::with_scripts(vec![truncated, assistant_text("recovered")]);

    let calls = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(MockTool {
        name: "mock",
        delay: Duration::ZERO,
        calls: calls.clone(),
        terminate: false,
    });

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![tool],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );
    let (_, messages) = collect_events(stream).await;

    // Tool never ran; the tool result is an error mentioning truncation.
    assert!(calls.lock().unwrap().is_empty());
    let messages = without_system(&messages);
    let AgentMessage::ToolResult(result) = &messages[2] else {
        panic!("expected tool result")
    };
    assert!(result.is_error);
    let InputContentBlock::Text { text, .. } = &result.content[0] else {
        panic!()
    };
    assert!(text.contains("output token limit"));

    // Agent recovered on the next turn.
    assert!(
        matches!(&messages[3], AgentMessage::Assistant(a) if a.stop_reason == StopReason::Stop)
    );
}

#[tokio::test]
async fn fallback_chain_recovers_from_rate_limit() {
    let mut error = AssistantMessage::pending(&test_model());
    error.stop_reason = StopReason::Error;
    error.error_message = Some("HTTP 429: rate limit exceeded".into());
    let provider = ScriptedProvider::with_scripts(vec![error, assistant_text("recovered")]);

    let mut fallback = test_model();
    fallback.id = "fallback-model".into();
    // provider_for("mock") resolves to nothing, so the loop keeps the
    // scripted provider — exactly what this test wants to observe.
    let mut config = make_config(provider, Arc::new(NoopHooks));
    config.fallback_models = vec![fallback];

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        config,
        CancellationToken::new(),
    );
    let (events, messages) = collect_events(stream).await;

    // A fallback event fired with the right models.
    let fallback_event = events.iter().find_map(|e| match e {
        AgentEvent::ModelFallback { from, to, reason } => Some((from, to, reason)),
        _ => None,
    });
    let (from, to, reason) = fallback_event.expect("ModelFallback event");
    assert_eq!(from.id, test_model().id);
    assert_eq!(to.id, "fallback-model");
    assert!(reason.contains("429"));

    // The error turn ended, then a new turn produced the recovery.
    let last = messages.last().expect("messages");
    assert!(matches!(last, AgentMessage::Assistant(a) if a.stop_reason == StopReason::Stop));
    assert!(matches!(last, AgentMessage::Assistant(a) if a.text() == "recovered"));
}

#[tokio::test]
async fn non_retryable_error_does_not_fallback() {
    let mut error = AssistantMessage::pending(&test_model());
    error.stop_reason = StopReason::Error;
    error.error_message = Some("HTTP 401: invalid api key".into());
    let provider = ScriptedProvider::with_scripts(vec![error]);

    let mut config = make_config(provider, Arc::new(NoopHooks));
    config.fallback_models = vec![test_model()];

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        config,
        CancellationToken::new(),
    );
    let (events, messages) = collect_events(stream).await;
    assert!(!tags(&events).contains(&"model_fallback"));
    let AgentMessage::Assistant(a) = &messages[1] else {
        panic!()
    };
    assert_eq!(a.stop_reason, StopReason::Error);
}

#[tokio::test]
async fn error_stop_reason_short_circuits() {
    let mut error = AssistantMessage::pending(&test_model());
    error.stop_reason = StopReason::Error;
    error.error_message = Some("boom".into());
    let provider = ScriptedProvider::with_scripts(vec![error]);

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );
    let (events, messages) = collect_events(stream).await;

    let tags = tags(&events);
    assert!(!tags.contains(&"tool_execution_start"));
    // One turn only.
    assert_eq!(tags.iter().filter(|t| **t == "turn_start").count(), 1);
    let AgentMessage::Assistant(a) = &messages[1] else {
        panic!()
    };
    assert_eq!(a.stop_reason, StopReason::Error);
}

#[tokio::test]
async fn parallel_execution_order_guarantees() {
    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("slow", "slow", json!({})), ("fast", "fast", json!({}))]),
        assistant_text("done"),
    ]);

    let calls = Arc::new(Mutex::new(Vec::new()));
    let slow = Arc::new(MockTool {
        name: "slow",
        delay: Duration::from_millis(200),
        calls: calls.clone(),
        terminate: false,
    });
    let fast = Arc::new(MockTool {
        name: "fast",
        delay: Duration::ZERO,
        calls: calls.clone(),
        terminate: false,
    });

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![slow, fast],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );
    let (events, _) = collect_events(stream).await;

    // tool_execution_end in completion order (fast first).
    let end_order: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(end_order, vec!["fast", "slow"]);

    // Tool result messages in source order (slow first).
    let result_order: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(t),
            } => Some(t.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(result_order, vec!["slow", "fast"]);
}

#[tokio::test]
async fn abort_during_streaming() {
    // Provider script that never terminates on its own; we cancel instead.
    #[derive(Debug)]
    struct HangingProvider;
    impl Provider for HangingProvider {
        fn stream(
            &self,
            model: &Model,
            _context: &Context,
            options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let (sender, stream) = event_stream();
            let model = model.clone();
            let cancel = options.cancel.clone();
            tokio::spawn(async move {
                let partial = AssistantMessage::pending(&model);
                let _ = sender.push(AssistantMessageEvent::Start { partial });
                cancel.cancelled().await;
                let mut message = AssistantMessage::pending(&model);
                message.stop_reason = StopReason::Aborted;
                message.error_message = Some("Request was aborted".into());
                sender.finish(AssistantMessageEvent::Error {
                    reason: StopReason::Aborted,
                    error: message,
                });
            });
            stream
        }
    }

    let config = AgentLoopConfig {
        provider: Arc::new(HangingProvider),
        ..make_config(ScriptedProvider::with_scripts(vec![]), Arc::new(NoopHooks))
    };
    let cancel = CancellationToken::new();
    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        config,
        cancel.clone(),
    );

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });

    let (_, messages) = collect_events(stream).await;
    let AgentMessage::Assistant(a) = &messages[1] else {
        panic!()
    };
    assert_eq!(a.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn steering_messages_injected_mid_run() {
    struct SteeringHooks;
    #[async_trait]
    impl AgentHooks for SteeringHooks {
        async fn steering_messages(&self) -> Vec<AgentMessage> {
            // First call (loop start, before turn 1): nothing. Second call
            // (after turn 1): inject once. Then stay empty.
            static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            match CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                1 => vec![AgentMessage::user("steer!")],
                _ => vec![],
            }
        }
    }

    let provider = ScriptedProvider::with_scripts(vec![
        assistant_text("first answer"),
        assistant_text("after steering"),
    ]);
    let seen = provider.seen_contexts.clone();

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, Arc::new(SteeringHooks)),
        CancellationToken::new(),
    );
    let (events, _) = collect_events(stream).await;

    // Two turns happened even though the first assistant message had no tools.
    assert_eq!(
        tags(&events).iter().filter(|t| **t == "turn_start").count(),
        2
    );
    // The second LLM call saw the steering message.
    let second_call = &seen.lock().unwrap()[1];
    let has_steer = second_call.messages.iter().any(|m| {
        matches!(m, Message::User(u) if matches!(&u.content, UserContent::Text(t) if t == "steer!"))
    });
    assert!(has_steer);
}

#[tokio::test]
async fn before_tool_call_block_produces_error_result() {
    struct DenyHooks;
    #[async_trait]
    impl AgentHooks for DenyHooks {
        async fn before_tool_call(
            &self,
            _ctx: &BeforeToolCallContext<'_>,
        ) -> BeforeToolCallOutcome {
            BeforeToolCallOutcome::Block {
                reason: Some("denied by policy".into()),
                terminate: false,
            }
        }
    }

    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "mock", json!({}))]),
        assistant_text("ok"),
    ]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(MockTool {
        name: "mock",
        delay: Duration::ZERO,
        calls: calls.clone(),
        terminate: false,
    });

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![tool],
        },
        make_config(provider, Arc::new(DenyHooks)),
        CancellationToken::new(),
    );
    let (_, messages) = collect_events(stream).await;

    assert!(calls.lock().unwrap().is_empty());
    let messages = without_system(&messages);
    let AgentMessage::ToolResult(result) = &messages[2] else {
        panic!()
    };
    assert!(result.is_error);
    let InputContentBlock::Text { text, .. } = &result.content[0] else {
        panic!()
    };
    assert_eq!(text, "denied by policy");
}

#[tokio::test]
async fn unknown_tool_gets_error_result() {
    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "nonexistent", json!({}))]),
        assistant_text("ok"),
    ]);

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );
    let (_, messages) = collect_events(stream).await;

    let AgentMessage::ToolResult(result) = &messages[2] else {
        panic!()
    };
    assert!(result.is_error);
    let InputContentBlock::Text { text, .. } = &result.content[0] else {
        panic!()
    };
    assert!(text.contains("not found"));
}

/// Regression: the fallback retry must happen even when the failing turn
/// follows a turn with no tool calls (e.g. continued via steering). Before
/// the fix, `has_more_tool_calls` was still false from the previous turn, so
/// the `continue` after switching models exited the inner loop instead of
/// retrying — the fallback model was consumed but never used.
#[tokio::test]
async fn fallback_retries_after_toolless_turn() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Inject one steering message after the first turn, then stay empty.
    struct SteerOnceHooks(AtomicUsize);
    #[async_trait]
    impl AgentHooks for SteerOnceHooks {
        async fn steering_messages(&self) -> Vec<AgentMessage> {
            match self.0.fetch_add(1, Ordering::SeqCst) {
                1 => vec![AgentMessage::user("steer")],
                _ => vec![],
            }
        }
    }

    let mut error = AssistantMessage::pending(&test_model());
    error.stop_reason = StopReason::Error;
    error.error_message = Some("HTTP 429: rate limit exceeded".into());
    let provider = ScriptedProvider::with_scripts(vec![
        assistant_text("answer one"), // turn 1: no tool calls
        error,                        // turn 2 (after steering): rate limited
        assistant_text("recovered"),  // turn 3: must run on the fallback model
    ]);
    let seen = provider.seen_contexts.clone();

    let mut fallback = test_model();
    fallback.id = "fallback-model".into();
    let mut config = make_config(provider, Arc::new(SteerOnceHooks(AtomicUsize::new(0))));
    config.fallback_models = vec![fallback];

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        config,
        CancellationToken::new(),
    );
    let (events, messages) = collect_events(stream).await;

    assert!(
        tags(&events).contains(&"model_fallback"),
        "fallback must fire"
    );
    // The fallback model actually produced a turn.
    assert_eq!(seen.lock().unwrap().len(), 3, "expected three LLM calls");
    assert!(matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.text() == "recovered"));
}

/// pi's `prepareNextTurn` runs only when a next turn will actually happen:
/// at the start of the following turn, gated on a completed previous turn.
/// It must NOT run after the final turn of a run.
#[tokio::test]
async fn prepare_next_turn_only_runs_between_turns() {
    struct CountingHooks(Arc<Mutex<usize>>);
    #[async_trait]
    impl AgentHooks for CountingHooks {
        async fn prepare_next_turn(&self, _ctx: &TurnContext<'_>) -> Option<NextTurnUpdate> {
            *self.0.lock().unwrap() += 1;
            None
        }
    }

    // Single-turn run: prepare_next_turn never fires.
    let calls = Arc::new(Mutex::new(0usize));
    let provider = ScriptedProvider::with_scripts(vec![assistant_text("one shot")]);
    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, Arc::new(CountingHooks(calls.clone()))),
        CancellationToken::new(),
    );
    let _ = collect_events(stream).await;
    assert_eq!(
        *calls.lock().unwrap(),
        0,
        "no next turn -> no prepare_next_turn"
    );

    // Two-turn run (tool call, then text): exactly once.
    let calls = Arc::new(Mutex::new(0usize));
    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "mock", json!({}))]),
        assistant_text("done"),
    ]);
    let tool = Arc::new(MockTool {
        name: "mock",
        delay: Duration::ZERO,
        calls: Arc::new(Mutex::new(Vec::new())),
        terminate: false,
    });
    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![tool],
        },
        make_config(provider, Arc::new(CountingHooks(calls.clone()))),
        CancellationToken::new(),
    );
    let _ = collect_events(stream).await;
    assert_eq!(
        *calls.lock().unwrap(),
        1,
        "one turn boundary -> one prepare_next_turn"
    );
}

/// pi calls `shouldStopAfterTurn` before `prepareNextTurn`; when the run
/// stops, prepare_next_turn must not fire at all.
#[tokio::test]
async fn prepare_next_turn_not_called_when_stopping() {
    struct StopHooks {
        prepare_calls: Arc<Mutex<usize>>,
    }
    #[async_trait]
    impl AgentHooks for StopHooks {
        async fn prepare_next_turn(&self, _ctx: &TurnContext<'_>) -> Option<NextTurnUpdate> {
            *self.prepare_calls.lock().unwrap() += 1;
            None
        }
        async fn should_stop_after_turn(&self, _ctx: &TurnContext<'_>) -> bool {
            true
        }
    }

    let prepare_calls = Arc::new(Mutex::new(0usize));
    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "mock", json!({}))]),
        assistant_text("never reached"),
    ]);
    let tool = Arc::new(MockTool {
        name: "mock",
        delay: Duration::ZERO,
        calls: Arc::new(Mutex::new(Vec::new())),
        terminate: false,
    });
    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![tool],
        },
        make_config(
            provider,
            Arc::new(StopHooks {
                prepare_calls: prepare_calls.clone(),
            }),
        ),
        CancellationToken::new(),
    );
    let (events, _) = collect_events(stream).await;
    assert_eq!(
        tags(&events).iter().filter(|t| **t == "turn_start").count(),
        1
    );
    assert_eq!(
        *prepare_calls.lock().unwrap(),
        0,
        "stopped run must not prepare a next turn"
    );
}

/// A tool that panics during execution must degrade to an error tool result
/// that still matches the assistant's tool call (pi catches tool exceptions
/// and turns them into error results). Applies to both execution modes.
#[tokio::test]
async fn panicking_tool_yields_matching_error_result() {
    #[derive(Debug)]
    struct PanicTool;
    #[async_trait]
    impl AgentTool for PanicTool {
        fn name(&self) -> &'static str {
            "panic_tool"
        }
        fn label(&self) -> &str {
            "panic_tool"
        }
        fn description(&self) -> &str {
            "panics"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn execute(
            &self,
            _id: &str,
            _params: Value,
            _cancel: CancellationToken,
            _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
        ) -> Result<AgentToolResult, String> {
            panic!("boom inside tool");
        }
    }

    for mode in [ToolExecutionMode::Parallel, ToolExecutionMode::Sequential] {
        let provider = ScriptedProvider::with_scripts(vec![
            assistant_with_tool_calls(&[("t1", "panic_tool", json!({}))]),
            assistant_text("recovered"),
        ]);
        let mut config = make_config(provider, Arc::new(NoopHooks));
        config.tool_execution = mode;
        let stream = agent_loop(
            vec![AgentMessage::user("go")],
            AgentContext {
                system_prompt: None,
                messages: vec![],
                tools: vec![Arc::new(PanicTool)],
            },
            config,
            CancellationToken::new(),
        );
        let (events, messages) = collect_events(stream).await;

        let messages = without_system(&messages);
        let AgentMessage::ToolResult(result) = &messages[2] else {
            panic!("{mode:?}: tool result")
        };
        assert_eq!(
            result.tool_call_id, "t1",
            "{mode:?}: result must match the tool call"
        );
        assert_eq!(result.tool_name, "panic_tool");
        assert!(result.is_error, "{mode:?}: panic must be an error result");
        // tool_execution_end must fire so UIs don't hang on the tool.
        let end = events.iter().any(|e| matches!(e,
            AgentEvent::ToolExecutionEnd { tool_call_id, is_error: true, .. } if tool_call_id == "t1"));
        assert!(
            end,
            "{mode:?}: tool_execution_end must fire for the panicked tool"
        );
        // The loop survived and completed.
        assert!(
            matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.text() == "recovered")
        );
    }
}

/// Client-side tool search: a tool result carrying added_tool_names moves
/// the named tools from the deferred pool into the live context; the model
/// can then call them in later turns.
#[tokio::test]
async fn tool_search_activates_deferred_pool_tools() {
    use tack_agent_core::{AgentTool, AgentToolResult};

    /// Tool that "activates" another tool (what tool_search does).
    #[derive(Debug)]
    struct Activator;
    #[async_trait]
    impl AgentTool for Activator {
        fn name(&self) -> &'static str {
            "activator"
        }
        fn label(&self) -> &str {
            "activator"
        }
        fn description(&self) -> &str {
            "activates deferred tools"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn execute(
            &self,
            _id: &str,
            _params: Value,
            _cancel: CancellationToken,
            _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
        ) -> Result<AgentToolResult, String> {
            let mut result = AgentToolResult::text("activated deferred_tool");
            result.added_tool_names = Some(vec!["deferred_tool".to_string()]);
            Ok(result)
        }
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let deferred = Arc::new(MockTool {
        name: "deferred_tool",
        delay: Duration::ZERO,
        calls: calls.clone(),
        terminate: false,
    });

    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("c1", "activator", json!({}))]),
        assistant_with_tool_calls(&[("c2", "deferred_tool", json!({}))]),
        assistant_text("done"),
    ]);

    let mut config = make_config(provider, Arc::new(NoopHooks));
    config.tool_pool = vec![deferred];

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![Arc::new(Activator)],
        },
        config,
        CancellationToken::new(),
    );
    let (_, messages) = collect_events(stream).await;

    // The pooled tool actually executed.
    assert_eq!(calls.lock().unwrap().len(), 1);
    // The final assistant message is the "done" text.
    assert!(matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.text() == "done"));

    // Transcript state (upstream #9548): the run records system messages —
    // the initial loadout declaration and, after the activator ran, a
    // toolsAdded update for the activated pool tool. Replaying them yields
    // exactly the final executable set.
    let systems: Vec<&tack_ai::SystemMessage> =
        messages.iter().filter_map(|m| m.as_system()).collect();
    assert!(
        systems.iter().any(|s| s
            .tools_added
            .as_ref()
            .is_some_and(|t| t.iter().any(|t| t.name == "deferred_tool"))),
        "activation recorded as a toolsAdded system message: {systems:?}"
    );
    let replayed = tack_ai::transcript::get_current_tools(&messages);
    let mut names: Vec<&str> = replayed.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["activator", "deferred_tool"]);
}

/// The replayed transcript prompt wins over the loop's configured
/// system_prompt, and prompt changes are recorded as section updates.
#[tokio::test]
async fn transcript_system_messages_carry_prompt_state() {
    use tack_agent_core::agent_loop::SYSTEM_PROMPT_SECTION;

    let provider = ScriptedProvider::with_scripts(vec![assistant_text("ok")]);
    let seen = provider.seen_contexts.clone();

    let stream = agent_loop(
        vec![AgentMessage::user("hi")],
        AgentContext {
            system_prompt: Some("You are Tack.".to_string()),
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );
    let (_, messages) = collect_events(stream).await;

    // First message: the prompt-section update.
    let AgentMessage::System(system) = &messages[0] else {
        panic!("expected leading system update, got {:?}", messages[0])
    };
    let sections = system.sections.as_ref().expect("section patch");
    assert_eq!(
        sections.get(SYSTEM_PROMPT_SECTION),
        Some(&Some("You are Tack.".to_string()))
    );

    // The provider saw the replayed prompt as Context.system_prompt and NO
    // system roles inside the message list.
    let request = seen.lock().unwrap()[0].clone();
    assert_eq!(request.system_prompt.as_deref(), Some("You are Tack."));
    assert!(
        request
            .messages
            .iter()
            .all(|m| !matches!(m, Message::System(_))),
        "system roles are collapsed before the provider: {:?}",
        request.messages
    );

    // Second run with the same prompt: no new update (in sync).
    let provider = ScriptedProvider::with_scripts(vec![assistant_text("ok2")]);
    let stream = agent_loop(
        vec![AgentMessage::user("again")],
        AgentContext {
            system_prompt: Some("You are Tack.".to_string()),
            messages: messages.clone(),
            tools: vec![],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );
    let (_, messages2) = collect_events(stream).await;
    assert!(
        messages2
            .iter()
            .all(|m| !matches!(m, AgentMessage::System(_))),
        "steady state records no further system updates: {messages2:?}"
    );

    // Third run with a changed prompt: one section update.
    let provider = ScriptedProvider::with_scripts(vec![assistant_text("ok3")]);
    let stream = agent_loop(
        vec![AgentMessage::user("third")],
        AgentContext {
            system_prompt: Some("You are Tack v2.".to_string()),
            messages: messages.clone(),
            tools: vec![],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );
    let (_, messages3) = collect_events(stream).await;
    let updates: Vec<&tack_ai::SystemMessage> =
        messages3.iter().filter_map(|m| m.as_system()).collect();
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0]
            .sections
            .as_ref()
            .unwrap()
            .get(SYSTEM_PROMPT_SECTION),
        Some(&Some("You are Tack v2.".to_string()))
    );
}

/// Cancellation mid tool batch (sequential path): the remaining tool
/// calls must still get (aborted) results. Otherwise the assistant
/// message in the resumed context has tool calls with no matching result,
/// and providers reject the transcript with HTTP 400 on the next request.
#[tokio::test]
async fn cancel_mid_tool_batch_aborts_remaining_tool_calls_sequential() {
    /// Cancels the run's token inside execute (simulating Esc while the
    /// first tool of a batch is running), then returns normally.
    #[derive(Debug)]
    struct CancellerTool(CancellationToken);
    #[async_trait]
    impl AgentTool for CancellerTool {
        fn name(&self) -> &'static str {
            "canceller"
        }
        fn label(&self) -> &'static str {
            "canceller"
        }
        fn description(&self) -> &'static str {
            "cancels the run"
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
            let _ = cancel;
            self.0.cancel();
            Ok(AgentToolResult::text("cancelled the run"))
        }
    }

    let cancel = CancellationToken::new();
    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "canceller", json!({})), ("t2", "mock", json!({}))]),
        assistant_text("done"),
    ]);
    let mock_calls = Arc::new(Mutex::new(Vec::new()));
    let mock = Arc::new(MockTool {
        name: "mock",
        delay: Duration::ZERO,
        calls: mock_calls.clone(),
        terminate: false,
    });
    let mut config = make_config(provider, Arc::new(NoopHooks));
    config.tool_execution = ToolExecutionMode::Sequential;
    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![Arc::new(CancellerTool(cancel.clone())), mock],
        },
        config,
        cancel,
    );
    let (_, messages) = collect_events(stream).await;

    // Every tool call in the assistant message has a matching result.
    let results: Vec<&ToolResultMessage> = messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "no dangling tool calls");
    assert_eq!(results[0].tool_call_id, "t1");
    assert!(!results[0].is_error, "t1 ran to completion");
    assert_eq!(results[1].tool_call_id, "t2");
    assert!(results[1].is_error, "t2 was never executed");
    let InputContentBlock::Text { text, .. } = &results[1].content[0] else {
        panic!("text content")
    };
    assert_eq!(text, "Operation aborted");
    assert!(
        mock_calls.lock().unwrap().is_empty(),
        "t2's tool must not execute after cancellation"
    );
}

/// Same guarantee on the PARALLEL path when cancellation lands mid-
/// preflight (a hook cancels while gating the first call): the tool calls
/// that never started still get aborted results, so the resumed
/// transcript has no dangling tool calls.
#[tokio::test]
async fn cancel_during_parallel_preflight_aborts_unstarted_tool_calls() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Cancels the run token while gating the FIRST tool call (the second
    /// call's preflight then never runs).
    struct CancellingHooks(CancellationToken, AtomicUsize);
    #[async_trait]
    impl AgentHooks for CancellingHooks {
        async fn before_tool_call(
            &self,
            _ctx: &BeforeToolCallContext<'_>,
        ) -> BeforeToolCallOutcome {
            if self.1.fetch_add(1, Ordering::SeqCst) == 0 {
                self.0.cancel();
            }
            BeforeToolCallOutcome::Allow
        }
    }

    let cancel = CancellationToken::new();
    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "mock", json!({})), ("t2", "mock", json!({}))]),
        assistant_text("done"),
    ]);
    let mock_calls = Arc::new(Mutex::new(Vec::new()));
    let mock = Arc::new(MockTool {
        name: "mock",
        delay: Duration::ZERO,
        calls: mock_calls.clone(),
        terminate: false,
    });
    let mut config = make_config(
        provider,
        Arc::new(CancellingHooks(cancel.clone(), AtomicUsize::new(0))),
    );
    config.tool_execution = ToolExecutionMode::Parallel;
    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![mock],
        },
        config,
        cancel,
    );
    let (_, messages) = collect_events(stream).await;

    let results: Vec<&ToolResultMessage> = messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "no dangling tool calls");
    for (i, id) in ["t1", "t2"].iter().enumerate() {
        assert_eq!(results[i].tool_call_id, *id);
        assert!(results[i].is_error, "{id} must be an aborted error result");
        let InputContentBlock::Text { text, .. } = &results[i].content[0] else {
            panic!("text content")
        };
        assert_eq!(text, "Operation aborted");
    }
    assert!(
        mock_calls.lock().unwrap().is_empty(),
        "no tool may execute once the run is cancelled"
    );
}

/// Fallback retry must not re-fire prepare_next_turn for the SAME
/// completed turn: the retry is a continuation of the failed turn, not a
/// new completed-turn boundary. (Regression: hooks observing
/// prepare_next_turn — e.g. compaction triggers — ran twice per turn when
/// a fallback happened.)
#[tokio::test]
async fn fallback_retry_does_not_repeat_prepare_next_turn() {
    struct CountingHooks(Arc<Mutex<usize>>);
    #[async_trait]
    impl AgentHooks for CountingHooks {
        async fn prepare_next_turn(&self, _ctx: &TurnContext<'_>) -> Option<NextTurnUpdate> {
            *self.0.lock().unwrap() += 1;
            None
        }
    }

    let mut error = AssistantMessage::pending(&test_model());
    error.stop_reason = StopReason::Error;
    error.error_message = Some("HTTP 429: rate limit exceeded".into());
    let provider = ScriptedProvider::with_scripts(vec![
        assistant_with_tool_calls(&[("t1", "mock", json!({}))]), // turn 1 completes
        error,                                                   // turn 2 fails -> fallback
        assistant_text("recovered"),                             // retried turn 2
    ]);
    let tool = Arc::new(MockTool {
        name: "mock",
        delay: Duration::ZERO,
        calls: Arc::new(Mutex::new(Vec::new())),
        terminate: false,
    });
    let prepare_calls = Arc::new(Mutex::new(0usize));
    let mut fallback = test_model();
    fallback.id = "fallback-model".into();
    let mut config = make_config(provider, Arc::new(CountingHooks(prepare_calls.clone())));
    config.fallback_models = vec![fallback];

    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![tool],
        },
        config,
        CancellationToken::new(),
    );
    let (events, messages) = collect_events(stream).await;

    assert!(tags(&events).contains(&"model_fallback"));
    assert!(matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.text() == "recovered"));
    assert_eq!(
        *prepare_calls.lock().unwrap(),
        1,
        "prepare_next_turn must fire exactly once for the turn-1 boundary, not again for the retry"
    );
}

#[test]
fn fallback_worthy_500_boundary() {
    use tack_agent_core::agent_loop::is_fallback_worthy;
    // Trailing "500" must count (the old `"500 "` pattern missed it).
    assert!(is_fallback_worthy(Some("HTTP 500")));
    assert!(is_fallback_worthy(Some("500 Internal Server Error")));
    assert!(is_fallback_worthy(Some("provider returned error 500")));
    // Digit-adjacent occurrences are not status codes.
    assert!(!is_fallback_worthy(Some("context exceeds 5000 tokens")));
    assert!(!is_fallback_worthy(Some("1500")));
    // Unrelated errors stay non-fallback.
    assert!(!is_fallback_worthy(Some("invalid api key")));
    assert!(!is_fallback_worthy(None));
}

// ---------------------------------------------------------------------------
// Streaming update coalescing: fine-grained deltas must not flood consumers.
// ---------------------------------------------------------------------------

/// Streams one text block as many back-to-back deltas (faster than any
/// consumer could process them), mimicking real provider SSE granularity.
struct FineGrainedProvider {
    chunk: String,
    chunks: usize,
}

impl std::fmt::Debug for FineGrainedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FineGrainedProvider").finish()
    }
}

impl Provider for FineGrainedProvider {
    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let (sender, stream) = event_stream();
        let model = model.clone();
        let chunk = self.chunk.clone();
        let chunks = self.chunks;
        let full = chunk.repeat(chunks);
        tokio::spawn(async move {
            let mut partial = AssistantMessage::pending(&model);
            partial.stop_reason = StopReason::Stop;
            let _ = sender.push(AssistantMessageEvent::Start {
                partial: partial.clone(),
            });
            let _ = sender.push(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial.clone(),
            });
            for _ in 0..chunks {
                match partial.content.first_mut() {
                    Some(ContentBlock::Text { text, .. }) => text.push_str(&chunk),
                    _ => partial.content.push(ContentBlock::text(&chunk)),
                }
                let _ = sender.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: chunk.clone(),
                    partial: partial.clone(),
                });
            }
            let _ = sender.push(AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: full,
                partial: partial.clone(),
            });
            sender.finish(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: partial,
            });
        });
        stream
    }
}

/// Rapid stream deltas are coalesced instead of one `MessageUpdate` per
/// delta (regression: consumers re-render the full message per event, so
/// per-delta emission backlogs the unbounded event queue and the UI keeps
/// dribbling stale frames long after the run finished).
#[tokio::test]
async fn rapid_stream_deltas_are_coalesced() {
    let chunks = 200;
    let provider = Arc::new(FineGrainedProvider {
        chunk: "abc".to_string(),
        chunks,
    });
    let stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );
    let (events, messages) = collect_events(stream).await;

    // 200 deltas would be 200+ message_update events uncoalesced; a
    // back-to-back burst collapses to a handful (the pending delta flushed
    // by TextEnd plus the boundary event itself). Generous bound so a
    // heavily loaded CI box with a few >50ms scheduling stalls still passes.
    let updates: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| e.tag() == "message_update")
        .collect();
    assert!(
        updates.len() <= 10,
        "coalescing failed: {} message_update events for {chunks} deltas",
        updates.len()
    );
    assert!(!updates.is_empty());

    // The final update still carries the complete accumulated text.
    let Some(AgentEvent::MessageUpdate {
        message: AgentMessage::Assistant(last),
        ..
    }) = updates.last()
    else {
        panic!("expected assistant message_update");
    };
    assert_eq!(last.text(), "abc".repeat(chunks));

    // Concatenated delta text across updates is complete: delta-accumulating
    // consumers (print mode, remote transcripts) must not lose text.
    let mut joined = String::new();
    for update in &updates {
        if let AgentEvent::MessageUpdate {
            assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } = update
        {
            joined.push_str(delta);
        }
    }
    assert_eq!(joined, "abc".repeat(chunks));

    // MessageEnd carries the exact final message.
    assert!(
        matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.text() == "abc".repeat(chunks))
    );
}

// ---------------------------------------------------------------------------
// StallingProvider: one delta, then silence, then Done.
// ---------------------------------------------------------------------------

struct StallingProvider {
    stall: Duration,
}

impl std::fmt::Debug for StallingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StallingProvider").finish()
    }
}

impl Provider for StallingProvider {
    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let (sender, stream) = event_stream();
        let model = model.clone();
        let stall = self.stall;
        tokio::spawn(async move {
            let mut partial = AssistantMessage::pending(&model);
            partial.stop_reason = StopReason::Stop;
            let _ = sender.push(AssistantMessageEvent::Start {
                partial: partial.clone(),
            });
            let _ = sender.push(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial.clone(),
            });
            partial.content.push(ContentBlock::text("hello"));
            let _ = sender.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hello".to_string(),
                partial: partial.clone(),
            });
            // Provider goes quiet mid-block (slow generation / network stall).
            tokio::time::sleep(stall).await;
            sender.finish(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: partial,
            });
        });
        stream
    }
}

/// A coalesced delta window must flush on the 50ms timer even when no
/// further provider event arrives — otherwise already-generated text stays
/// invisible to consumers for the whole stall (regression: the window only
/// flushed when the NEXT delta/boundary event arrived).
#[tokio::test]
async fn pending_delta_flushes_when_provider_stalls() {
    let provider = Arc::new(StallingProvider {
        stall: Duration::from_millis(1200),
    });
    let mut stream = agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, Arc::new(NoopHooks)),
        CancellationToken::new(),
    );

    let start = std::time::Instant::now();
    let mut delta_at = None;
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        if let AgentEvent::MessageUpdate {
            assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } = &event
            && delta.contains("hello")
            && delta_at.is_none()
        {
            delta_at = Some(start.elapsed());
        }
        if event.is_terminal() {
            break;
        }
    }
    let _ = stream.result().await;

    let elapsed = delta_at.expect("the delta must be emitted as a message_update");
    assert!(
        elapsed < Duration::from_millis(800),
        "stalled window was not flushed by the 50ms timer; \
         the delta only reached consumers after {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Provider-bound tools: only advertised to their provider's models
// ---------------------------------------------------------------------------

/// Records the tool NAMES visible in each LLM context.
#[derive(Debug, Default)]
struct ToolListProvider {
    seen_tools: Arc<Mutex<Vec<Vec<String>>>>,
}

impl Provider for ToolListProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.seen_tools
            .lock()
            .unwrap()
            .push(context.tools.iter().map(|t| t.name.clone()).collect());
        let (sender, stream) = event_stream();
        let message = {
            let mut m = AssistantMessage::pending(model);
            m.stop_reason = StopReason::Stop;
            m.content.push(ContentBlock::text("done"));
            m
        };
        tokio::spawn(async move {
            let _ = sender.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            let _ = sender.push(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: message.clone(),
            });
            sender.end(message);
        });
        stream
    }
}

#[derive(Debug)]
struct ProviderBoundTool;

#[async_trait]
impl AgentTool for ProviderBoundTool {
    fn name(&self) -> &'static str {
        "bound"
    }
    fn label(&self) -> &str {
        "bound"
    }
    fn description(&self) -> &str {
        "bound to codebuddy"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn available_for_provider(&self, provider: &str) -> bool {
        provider == "codebuddy"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        Ok(AgentToolResult::text("bound"))
    }
}

/// A provider-bound tool is advertised only to its provider's models;
/// other providers never see it in the LLM context.
#[tokio::test(flavor = "multi_thread")]
async fn provider_bound_tools_filtered_from_llm_context() {
    for (provider_id, expect_bound) in [("mock", false), ("codebuddy", true)] {
        let provider = Arc::new(ToolListProvider::default());
        let seen = provider.seen_tools.clone();
        let mut config = make_config(provider, Arc::new(NoopHooks));
        config.model.provider = provider_id.to_string();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stream = agent_loop(
            vec![AgentMessage::user("go")],
            AgentContext {
                system_prompt: None,
                messages: vec![],
                tools: vec![
                    Arc::new(MockTool {
                        name: "mock",
                        delay: Duration::ZERO,
                        calls: calls.clone(),
                        terminate: false,
                    }),
                    Arc::new(ProviderBoundTool),
                ],
            },
            config,
            CancellationToken::new(),
        );
        let _ = collect_events(stream).await;
        let tools = &seen.lock().unwrap()[0];
        assert!(
            tools.contains(&"mock".to_string()),
            "{provider_id}: {tools:?}"
        );
        assert_eq!(
            tools.contains(&"bound".to_string()),
            expect_bound,
            "{provider_id}: {tools:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Overflow recovery (upstream agent-session _checkCompaction case 1):
// an overflow error triggers compact_for_overflow and one turn retry.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct OverflowHooks {
    calls: Mutex<usize>,
}

#[async_trait]
impl AgentHooks for OverflowHooks {
    async fn compact_for_overflow(&self) -> Option<Vec<AgentMessage>> {
        *self.calls.lock().unwrap() += 1;
        // Rebuilt post-compaction context (empty: history was summarized).
        Some(Vec::new())
    }
}

fn overflow_error() -> AssistantMessage {
    let mut m = AssistantMessage::pending(&test_model());
    m.stop_reason = StopReason::Error;
    m.error_message = Some("prompt is too long: 213462 tokens > 100000 maximum".into());
    m
}

#[tokio::test]
async fn overflow_error_compacts_and_retries_turn() {
    let provider =
        ScriptedProvider::with_scripts(vec![overflow_error(), assistant_text("recovered")]);
    let seen = provider.seen_contexts.clone();
    let hooks = Arc::new(OverflowHooks::default());

    let stream = agent_loop(
        vec![AgentMessage::user("hi")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, hooks.clone()),
        CancellationToken::new(),
    );
    let (_events, messages) = collect_events(stream).await;

    assert_eq!(*hooks.calls.lock().unwrap(), 1);
    // The retried turn produced the recovery message …
    let recovered = without_system(&messages)
        .iter()
        .any(|m| matches!(m, AgentMessage::Assistant(a) if a.text() == "recovered"));
    assert!(recovered, "messages: {messages:?}");
    // … and the retry saw the rebuilt (empty) context.
    let contexts = seen.lock().unwrap();
    assert_eq!(contexts.len(), 2);
    assert!(contexts[1].messages.is_empty());
}

#[tokio::test]
async fn overflow_recovery_budget_is_one_attempt() {
    // Two consecutive overflows: recovery fires once, then the error ends
    // the run (upstream `_overflowRecoveryAttempted`).
    let provider = ScriptedProvider::with_scripts(vec![overflow_error(), overflow_error()]);
    let hooks = Arc::new(OverflowHooks::default());

    let stream = agent_loop(
        vec![AgentMessage::user("hi")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, hooks.clone()),
        CancellationToken::new(),
    );
    let (_events, messages) = collect_events(stream).await;

    assert_eq!(*hooks.calls.lock().unwrap(), 1);
    let last = without_system(&messages).last().copied().unwrap();
    let AgentMessage::Assistant(last) = last else {
        panic!("expected assistant message, got {last:?}")
    };
    assert_eq!(last.stop_reason, StopReason::Error);
    assert!(
        last.error_message
            .as_deref()
            .unwrap()
            .contains("prompt is too long")
    );
}

#[tokio::test]
async fn non_overflow_error_does_not_trigger_recovery() {
    let mut other = AssistantMessage::pending(&test_model());
    other.stop_reason = StopReason::Error;
    other.error_message = Some("500 internal server error".into());
    let provider = ScriptedProvider::with_scripts(vec![other]);
    let hooks = Arc::new(OverflowHooks::default());

    let stream = agent_loop(
        vec![AgentMessage::user("hi")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, hooks.clone()),
        CancellationToken::new(),
    );
    let (_events, messages) = collect_events(stream).await;

    assert_eq!(*hooks.calls.lock().unwrap(), 0);
    let last = without_system(&messages).last().copied().unwrap();
    assert!(matches!(last, AgentMessage::Assistant(a) if a.stop_reason == StopReason::Error));
}

// ---------------------------------------------------------------------------
// Recoverable length-stop (upstream _checkCompaction case 2): a response cut
// off below the model's intended output limit triggers compact_for_overflow
// and one turn retry; a genuine max_tokens hit does not.
// ---------------------------------------------------------------------------

fn length_stop(output_tokens: u64) -> AssistantMessage {
    let mut m = AssistantMessage::pending(&test_model());
    m.stop_reason = StopReason::Length;
    m.content = vec![ContentBlock::text("truncated")];
    m.usage.output = output_tokens;
    m
}

#[tokio::test]
async fn recoverable_length_stop_compacts_and_retries_turn() {
    // Output (100) well below the model's 4096 max_tokens → recoverable.
    let provider =
        ScriptedProvider::with_scripts(vec![length_stop(100), assistant_text("recovered")]);
    let seen = provider.seen_contexts.clone();
    let hooks = Arc::new(OverflowHooks::default());

    let stream = agent_loop(
        vec![AgentMessage::user("hi")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, hooks.clone()),
        CancellationToken::new(),
    );
    let (_events, messages) = collect_events(stream).await;

    assert_eq!(*hooks.calls.lock().unwrap(), 1);
    let recovered = without_system(&messages)
        .iter()
        .any(|m| matches!(m, AgentMessage::Assistant(a) if a.text() == "recovered"));
    assert!(recovered, "messages: {messages:?}");
    // The retry saw the rebuilt (empty) context.
    let contexts = seen.lock().unwrap();
    assert_eq!(contexts.len(), 2);
    assert!(contexts[1].messages.is_empty());
}

#[tokio::test]
async fn length_recovery_budget_is_one_attempt() {
    // Two consecutive recoverable length-stops: recovery fires once, then the
    // second message falls through and ends the run (no tool calls to fail).
    let provider = ScriptedProvider::with_scripts(vec![length_stop(100), length_stop(100)]);
    let hooks = Arc::new(OverflowHooks::default());

    let stream = agent_loop(
        vec![AgentMessage::user("hi")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, hooks.clone()),
        CancellationToken::new(),
    );
    let (_events, messages) = collect_events(stream).await;

    assert_eq!(*hooks.calls.lock().unwrap(), 1);
    let last = without_system(&messages).last().copied().unwrap();
    assert!(matches!(last, AgentMessage::Assistant(a) if a.stop_reason == StopReason::Length));
}

#[tokio::test]
async fn user_capped_max_tokens_stop_does_not_compact() {
    // A max_tokens override below the model default (4096): output 1024 hits
    // the caller's cap, which is a genuine limit — not context pressure.
    // Judging against the model default would misread this as recoverable.
    let provider = ScriptedProvider::with_scripts(vec![length_stop(1024)]);
    let hooks = Arc::new(OverflowHooks::default());
    let mut config = make_config(provider, hooks.clone());
    config.max_tokens = Some(1024);

    let stream = agent_loop(
        vec![AgentMessage::user("hi")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        config,
        CancellationToken::new(),
    );
    let (_events, messages) = collect_events(stream).await;

    assert_eq!(*hooks.calls.lock().unwrap(), 0);
    let last = without_system(&messages).last().copied().unwrap();
    assert!(matches!(last, AgentMessage::Assistant(a) if a.stop_reason == StopReason::Length));
}

#[tokio::test]
async fn genuine_max_tokens_stop_does_not_compact() {
    // Output reached the model's max_tokens (4096): a genuine limit hit, not
    // context pressure — no compaction.
    let provider = ScriptedProvider::with_scripts(vec![length_stop(4096)]);
    let hooks = Arc::new(OverflowHooks::default());

    let stream = agent_loop(
        vec![AgentMessage::user("hi")],
        AgentContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        },
        make_config(provider, hooks.clone()),
        CancellationToken::new(),
    );
    let (_events, messages) = collect_events(stream).await;

    assert_eq!(*hooks.calls.lock().unwrap(), 0);
    let last = without_system(&messages).last().copied().unwrap();
    assert!(matches!(last, AgentMessage::Assistant(a) if a.stop_reason == StopReason::Length));
}
