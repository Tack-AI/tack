#![allow(unsafe_code)]
//! ACP integration tests: in-process client over a tokio duplex stream,
//! driving initialize → session/new → prompt → cancel against a scripted
//! provider.
#![allow(clippy::unwrap_used)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use agent_client_protocol::{
    Agent, AgentSideConnection, CancelNotification, Client, ClientSideConnection, ContentBlock,
    CreateTerminalRequest, CreateTerminalResponse, InitializeRequest, KillTerminalRequest,
    KillTerminalResponse, LoadSessionRequest, NewSessionRequest, PermissionOptionKind,
    PromptRequest, ProtocolVersion, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, StopReason, TerminalExitStatus, TerminalId,
    TerminalOutputRequest, TerminalOutputResponse, TextContent, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse,
};
use serde_json::{Value, json};
use tack_ai::{
    AssistantMessage, ContentBlock as TackContentBlock, Context, Model, ModelCost, Provider,
    StopReason as TackStopReason, StreamOptions, event_stream,
};
use tack_app::acp::agent::TackAcpAgent;
use tack_app::settings::Settings;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ---------------------------------------------------------------------------
// Scripted provider
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ScriptedProvider {
    scripts: Mutex<Vec<AssistantMessage>>,
}

impl Provider for ScriptedProvider {
    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: StreamOptions,
    ) -> tack_ai::AssistantMessageEventStream {
        let (sender, stream) = event_stream();
        let message = {
            let mut scripts = self.scripts.lock().unwrap();
            if scripts.is_empty() {
                let mut m = AssistantMessage::pending(model);
                m.stop_reason = TackStopReason::Error;
                m.error_message = Some("no script left".into());
                m
            } else {
                scripts.remove(0)
            }
        };
        tokio::spawn(async move {
            let _ = sender.push(tack_ai::AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            for (i, block) in message.content.iter().enumerate() {
                if let TackContentBlock::Text { text, .. } = block {
                    let _ = sender.push(tack_ai::AssistantMessageEvent::TextStart {
                        content_index: i,
                        partial: message.clone(),
                    });
                    let _ = sender.push(tack_ai::AssistantMessageEvent::TextDelta {
                        content_index: i,
                        delta: text.clone(),
                        partial: message.clone(),
                    });
                }
            }
            match message.stop_reason {
                TackStopReason::Error | TackStopReason::Aborted => {
                    sender.finish(tack_ai::AssistantMessageEvent::Error {
                        reason: message.stop_reason,
                        error: message,
                    });
                }
                reason => sender.finish(tack_ai::AssistantMessageEvent::Done { reason, message }),
            }
        });
        stream
    }
}

fn test_model() -> Model {
    Model {
        id: "mock".into(),
        name: "Mock".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![tack_ai::InputKind::Text],
        cost: ModelCost::default(),
        context_window: 200_000,
        max_tokens: 4096,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn assistant_text(text: &str) -> AssistantMessage {
    let mut m = AssistantMessage::pending(&test_model());
    m.stop_reason = TackStopReason::Stop;
    m.content = vec![TackContentBlock::text(text)];
    m.usage.input = 120;
    m.usage.output = 30;
    m.usage.total_tokens = 150;
    m
}

fn assistant_tool_call(id: &str, name: &str, args: Value) -> AssistantMessage {
    let mut m = AssistantMessage::pending(&test_model());
    m.stop_reason = TackStopReason::ToolUse;
    m.content = vec![TackContentBlock::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        thought_signature: None,
        namespace: None,
    }];
    m
}

// ---------------------------------------------------------------------------
// Recording test client
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ClientState {
    updates: Vec<agent_client_protocol::SessionUpdate>,
    permission_requests: usize,
    permission_response_kind: PermissionOptionKind,
}

impl Default for ClientState {
    fn default() -> Self {
        ClientState {
            updates: Vec::new(),
            permission_requests: 0,
            permission_response_kind: PermissionOptionKind::AllowOnce,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct TestClient {
    state: Arc<Mutex<ClientState>>,
}

#[async_trait::async_trait(?Send)]
impl Client for TestClient {
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        let (kind, option_id) = {
            let mut state = self.state.lock().unwrap();
            state.permission_requests += 1;
            let kind = state.permission_response_kind;
            let option_id = args
                .options
                .iter()
                .find(|o| o.kind == kind)
                .map(|o| o.option_id.clone())
                .unwrap_or_else(|| args.options[0].option_id.clone());
            (kind, option_id)
        };
        let _ = kind;
        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
        ))
    }

    async fn session_notification(
        &self,
        args: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        self.state.lock().unwrap().updates.push(args.update);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    client_conn: Rc<ClientSideConnection>,
    client: TestClient,
    session_dir: tempfile::TempDir,
}

/// Shared agent dir for all tests in this binary (sessions land under
/// per-cwd subdirectories, so a single root is safe).
fn test_agent_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        std::mem::forget(dir); // keep the tempdir alive for the process
        unsafe { std::env::set_var("TACK_AGENT_DIR", &path) };
        path
    })
}

async fn harness(scripts: Vec<AssistantMessage>) -> Harness {
    test_agent_dir();
    let provider = Arc::new(ScriptedProvider {
        scripts: Mutex::new(scripts),
    });
    test_agent_dir();
    let session_dir = tempfile::tempdir().unwrap();

    let (agent_bytes, client_bytes) = tokio::io::duplex(1 << 20);
    let (agent_read, agent_write) = tokio::io::split(agent_bytes);
    let (client_read, client_write) = tokio::io::split(client_bytes);

    let shared: tack_app::acp::SharedConn = Rc::new(RefCell::new(None));
    let agent = TackAcpAgent::with_dependencies(
        shared.clone(),
        test_model(),
        provider,
        std::sync::Arc::new(tack_ai::oauth::StaticAuth::from(Some(
            "test-key".to_string(),
        ))),
        Settings::default(),
        None,
    );
    let (agent_conn, agent_io) = AgentSideConnection::new(
        agent,
        agent_write.compat_write(),
        agent_read.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    shared.borrow_mut().replace(Rc::new(agent_conn));
    tokio::task::spawn_local(agent_io);

    let client = TestClient::default();
    let (client_conn, client_io) = ClientSideConnection::new(
        client.clone(),
        client_write.compat_write(),
        client_read.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    tokio::task::spawn_local(client_io);

    Harness {
        client_conn: Rc::new(client_conn),
        client,
        session_dir,
    }
}

fn text_block(s: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(s))
}

/// Poll the recorded client updates until `pred` holds (10ms interval, 5s
/// timeout) instead of a fixed sleep; modeled on `wait_for_event` in the
/// tack-ext-wasm tests. Panics on timeout.
async fn wait_for_update(
    client: &TestClient,
    what: &str,
    pred: impl Fn(&[agent_client_protocol::SessionUpdate]) -> bool,
) {
    for _ in 0..500 {
        {
            let state = client.state.lock().unwrap();
            if pred(&state.updates) {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

macro_rules! acp_test {
    ($name:ident, $body:expr) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            let local = tokio::task::LocalSet::new();
            local
                .run_until(async move {
                    $body.await;
                })
                .await;
        }
    };
}

acp_test!(initialize_and_prompt_text, async {
    let h = harness(vec![assistant_text("hello from tack")]).await;
    let init = h
        .client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    assert_eq!(init.protocol_version, ProtocolVersion::V1);

    let session = h
        .client_conn
        .new_session(NewSessionRequest::new(h.session_dir.path().to_path_buf()))
        .await
        .unwrap();

    let response = h
        .client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("hi")],
        ))
        .await
        .unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    // The client received the assistant text as message chunks.
    let text: String = h
        .client
        .state
        .lock()
        .unwrap()
        .updates
        .iter()
        .filter_map(|u| match u {
            agent_client_protocol::SessionUpdate::AgentMessageChunk(chunk) => {
                match &chunk.content {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect();
    assert!(text.contains("hello from tack"), "updates: {text:?}");
});

acp_test!(tool_call_with_permission_flow, async {
    // write is a mutating tool → triggers the permission flow in ask mode.
    let h = harness(vec![
        assistant_tool_call(
            "t1",
            "write",
            json!({"path": "hello.txt", "content": "hi there"}),
        ),
        assistant_text("file written"),
    ])
    .await;
    h.client.state.lock().unwrap().permission_response_kind = PermissionOptionKind::AllowOnce;

    h.client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    let session = h
        .client_conn
        .new_session(NewSessionRequest::new(h.session_dir.path().to_path_buf()))
        .await
        .unwrap();
    let response = h
        .client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("read it")],
        ))
        .await
        .unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    // Permission was requested exactly once.
    assert_eq!(h.client.state.lock().unwrap().permission_requests, 1);

    // Tool call lifecycle reached the client.
    let updates = h.client.state.lock().unwrap();
    let saw_tool_call = updates
        .updates
        .iter()
        .any(|u| matches!(u, agent_client_protocol::SessionUpdate::ToolCall(tc) if tc.tool_call_id.0.as_ref() == "t1"));
    let saw_completed = updates.updates.iter().any(|u| {
        matches!(u, agent_client_protocol::SessionUpdate::ToolCallUpdate(u)
            if u.fields.status == Some(agent_client_protocol::ToolCallStatus::Completed))
    });
    assert!(saw_tool_call);
    assert!(saw_completed);
    drop(updates);

    // Session file persisted with the tool result (under the test agent dir).
    fn find_jsonl(dir: &std::path::Path) -> bool {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries.flatten().any(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        find_jsonl(&p)
                    } else {
                        p.extension().is_some_and(|x| x == "jsonl")
                    }
                })
            })
            .unwrap_or(false)
    }
    assert!(find_jsonl(test_agent_dir()), "session jsonl should exist");
});

acp_test!(permission_denied_blocks_tool, async {
    let h = harness(vec![
        assistant_tool_call("t1", "read", json!({"path": "hello.txt"})),
        assistant_text("could not read"),
    ])
    .await;
    h.client.state.lock().unwrap().permission_response_kind = PermissionOptionKind::RejectOnce;

    h.client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    let session = h
        .client_conn
        .new_session(NewSessionRequest::new(h.session_dir.path().to_path_buf()))
        .await
        .unwrap();
    let response = h
        .client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("read it")],
        ))
        .await
        .unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let updates = h.client.state.lock().unwrap();
    let saw_failed = updates.updates.iter().any(|u| {
        matches!(u, agent_client_protocol::SessionUpdate::ToolCallUpdate(u)
            if u.fields.status == Some(agent_client_protocol::ToolCallStatus::Failed))
    });
    assert!(saw_failed, "denied tool should end Failed");
});

acp_test!(cancel_returns_cancelled, async {
    // A hanging provider: emits start, then waits for cancellation.
    #[derive(Debug)]
    struct HangingProvider {
        // Signalled once the provider stream is hanging (Start pushed), so
        // the test can cancel deterministically instead of sleeping.
        started: Arc<tokio::sync::Notify>,
    }
    impl Provider for HangingProvider {
        fn stream(
            &self,
            model: &Model,
            _context: &Context,
            options: StreamOptions,
        ) -> tack_ai::AssistantMessageEventStream {
            let (sender, stream) = event_stream();
            let model = model.clone();
            let cancel = options.cancel.clone();
            let started = self.started.clone();
            tokio::spawn(async move {
                let partial = AssistantMessage::pending(&model);
                let _ = sender.push(tack_ai::AssistantMessageEvent::Start { partial });
                started.notify_one();
                cancel.cancelled().await;
                let mut m = AssistantMessage::pending(&model);
                m.stop_reason = TackStopReason::Aborted;
                m.error_message = Some("aborted".into());
                sender.finish(tack_ai::AssistantMessageEvent::Error {
                    reason: TackStopReason::Aborted,
                    error: m,
                });
            });
            stream
        }
    }

    test_agent_dir();
    let session_dir = tempfile::tempdir().unwrap();
    let (agent_bytes, client_bytes) = tokio::io::duplex(1 << 20);
    let (agent_read, agent_write) = tokio::io::split(agent_bytes);
    let (client_read, client_write) = tokio::io::split(client_bytes);
    let shared: tack_app::acp::SharedConn = Rc::new(RefCell::new(None));
    let provider_started = Arc::new(tokio::sync::Notify::new());
    let agent = TackAcpAgent::with_dependencies(
        shared.clone(),
        test_model(),
        Arc::new(HangingProvider {
            started: provider_started.clone(),
        }),
        std::sync::Arc::new(tack_ai::oauth::StaticAuth::from(Some("k".to_string()))),
        Settings::default(),
        None,
    );
    let (agent_conn, agent_io) = AgentSideConnection::new(
        agent,
        agent_write.compat_write(),
        agent_read.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    shared.borrow_mut().replace(Rc::new(agent_conn));
    tokio::task::spawn_local(agent_io);

    let client = TestClient::default();
    let (client_conn, client_io) = ClientSideConnection::new(
        client.clone(),
        client_write.compat_write(),
        client_read.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    tokio::task::spawn_local(client_io);
    let client_conn = Rc::new(client_conn);

    client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    let session = client_conn
        .new_session(NewSessionRequest::new(session_dir.path().to_path_buf()))
        .await
        .unwrap();

    let prompt_conn = client_conn.clone();
    let prompt_session = session.session_id.clone();
    let prompt_task = tokio::task::spawn_local(async move {
        prompt_conn
            .prompt(PromptRequest::new(prompt_session, vec![text_block("hang")]))
            .await
    });

    // Wait until the provider stream is actually hanging before cancelling
    // (event-driven via Notify instead of a fixed sleep).
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider_started.notified(),
    )
    .await
    .expect("provider stream should start within 5s");
    client_conn
        .cancel(CancelNotification::new(session.session_id.clone()))
        .await
        .unwrap();

    let response = prompt_task.await.unwrap().unwrap();
    assert_eq!(response.stop_reason, StopReason::Cancelled);
});

// ---------------------------------------------------------------------------
// M7: load_session replay + ACP terminal backend
// ---------------------------------------------------------------------------

acp_test!(load_session_replays_history, async {
    // Session 1: run a prompt so the session file has content.
    let h = harness(vec![assistant_text("replayed answer")]).await;
    h.client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    let session = h
        .client_conn
        .new_session(NewSessionRequest::new(h.session_dir.path().to_path_buf()))
        .await
        .unwrap();
    h.client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("original question")],
        ))
        .await
        .unwrap();

    // Clear recorded updates, then load the session again.
    h.client.state.lock().unwrap().updates.clear();
    h.client_conn
        .load_session(LoadSessionRequest::new(
            session.session_id.clone(),
            h.session_dir.path().to_path_buf(),
        ))
        .await
        .unwrap();

    // Replay is async (spawn_local notifications) — poll until both
    // directions have been replayed instead of a fixed sleep.
    wait_for_update(&h.client, "session replay", |updates| {
        let saw_user = updates.iter().any(|u| {
            matches!(u, agent_client_protocol::SessionUpdate::UserMessageChunk(c)
                if matches!(&c.content, ContentBlock::Text(t) if t.text.contains("original question")))
        });
        let saw_agent = updates.iter().any(|u| {
            matches!(u, agent_client_protocol::SessionUpdate::AgentMessageChunk(c)
                if matches!(&c.content, ContentBlock::Text(t) if t.text.contains("replayed answer")))
        });
        saw_user && saw_agent
    })
    .await;
    let updates = h.client.state.lock().unwrap();
    let saw_user = updates.updates.iter().any(|u| {
        matches!(u, agent_client_protocol::SessionUpdate::UserMessageChunk(c)
            if matches!(&c.content, ContentBlock::Text(t) if t.text.contains("original question")))
    });
    let saw_agent = updates.updates.iter().any(|u| {
        matches!(u, agent_client_protocol::SessionUpdate::AgentMessageChunk(c)
            if matches!(&c.content, ContentBlock::Text(t) if t.text.contains("replayed answer")))
    });
    assert!(
        saw_user && saw_agent,
        "replay should include user and agent messages: {:?}",
        updates.updates.len()
    );
});

/// A fake client terminal: runs nothing, returns canned output after a tick.
#[derive(Clone, Debug, Default)]
struct TerminalClient {
    state: Arc<Mutex<ClientState>>,
    created: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait(?Send)]
impl Client for TerminalClient {
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        let _ = args;
        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                permission_option_id_for_test(),
            )),
        ))
    }

    async fn session_notification(
        &self,
        args: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        self.state.lock().unwrap().updates.push(args.update);
        Ok(())
    }

    async fn create_terminal(
        &self,
        args: CreateTerminalRequest,
    ) -> agent_client_protocol::Result<CreateTerminalResponse> {
        self.created.lock().unwrap().push(args.command.clone());
        Ok(CreateTerminalResponse::new(TerminalId::new("term-1")))
    }

    async fn terminal_output(
        &self,
        _args: TerminalOutputRequest,
    ) -> agent_client_protocol::Result<TerminalOutputResponse> {
        Ok(TerminalOutputResponse::new(
            "canned output".to_string(),
            false,
        ))
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: WaitForTerminalExitRequest,
    ) -> agent_client_protocol::Result<WaitForTerminalExitResponse> {
        Ok(WaitForTerminalExitResponse::new(
            TerminalExitStatus::new().exit_code(0),
        ))
    }

    async fn kill_terminal(
        &self,
        _args: KillTerminalRequest,
    ) -> agent_client_protocol::Result<KillTerminalResponse> {
        Ok(KillTerminalResponse::new())
    }

    async fn release_terminal(
        &self,
        _args: ReleaseTerminalRequest,
    ) -> agent_client_protocol::Result<ReleaseTerminalResponse> {
        Ok(ReleaseTerminalResponse::new())
    }
}

fn permission_option_id_for_test() -> agent_client_protocol::PermissionOptionId {
    agent_client_protocol::PermissionOptionId::new("allow_once")
}

acp_test!(bash_uses_client_terminal_when_offered, async {
    let provider = Arc::new(ScriptedProvider {
        scripts: Mutex::new(vec![
            assistant_tool_call("t1", "bash", json!({"command": "echo hi"})),
            assistant_text("done"),
        ]),
    });
    let session_dir = tempfile::tempdir().unwrap();
    test_agent_dir();

    let (agent_bytes, client_bytes) = tokio::io::duplex(1 << 20);
    let (agent_read, agent_write) = tokio::io::split(agent_bytes);
    let (client_read, client_write) = tokio::io::split(client_bytes);

    let shared: tack_app::acp::SharedConn = Rc::new(RefCell::new(None));
    let agent = TackAcpAgent::with_dependencies(
        shared.clone(),
        test_model(),
        provider,
        std::sync::Arc::new(tack_ai::oauth::StaticAuth::from(Some("k".to_string()))),
        Settings::default(),
        None,
    );
    let (agent_conn, agent_io) = AgentSideConnection::new(
        agent,
        agent_write.compat_write(),
        agent_read.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    shared.borrow_mut().replace(Rc::new(agent_conn));
    tokio::task::spawn_local(agent_io);

    let client = TerminalClient::default();
    let created = client.created.clone();
    let (client_conn, client_io) = ClientSideConnection::new(
        client.clone(),
        client_write.compat_write(),
        client_read.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    tokio::task::spawn_local(client_io);
    let client_conn = Rc::new(client_conn);

    // Advertise terminal capability.
    let mut init = InitializeRequest::new(ProtocolVersion::V1);
    init.client_capabilities.terminal = true;
    client_conn.initialize(init).await.unwrap();

    let session = client_conn
        .new_session(NewSessionRequest::new(session_dir.path().to_path_buf()))
        .await
        .unwrap();
    let response = client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("run echo")],
        ))
        .await
        .unwrap();

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(created.lock().unwrap().as_slice(), &["echo hi".to_string()]);

    // The tool completed with the canned terminal output.
    let updates = client.state.lock().unwrap();
    let completed_with_output = updates.updates.iter().any(|u| {
        matches!(u, agent_client_protocol::SessionUpdate::ToolCallUpdate(upd)
            if upd.fields.status == Some(agent_client_protocol::ToolCallStatus::Completed)
                && upd.fields.content.as_ref().is_some_and(|c| !c.is_empty()))
    });
    assert!(completed_with_output);
});

acp_test!(provider_error_is_surfaced_as_chunk, async {
    // Provider that always errors (simulates missing API key etc).
    let mut error_msg = AssistantMessage::pending(&test_model());
    error_msg.stop_reason = TackStopReason::Error;
    error_msg.error_message = Some("No API key for provider: kimi-coding".into());
    let h = harness(vec![error_msg]).await;

    h.client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    let session = h
        .client_conn
        .new_session(NewSessionRequest::new(h.session_dir.path().to_path_buf()))
        .await
        .unwrap();
    let response = h
        .client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("hi")],
        ))
        .await
        .unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    // The error text must reach the client as a visible chunk.
    wait_for_update(&h.client, "error chunk", |updates| {
        updates.iter().any(|u| {
            matches!(u, agent_client_protocol::SessionUpdate::AgentMessageChunk(c)
                if matches!(&c.content, ContentBlock::Text(t) if t.text.contains("No API key")))
        })
    })
    .await;
    let updates = h.client.state.lock().unwrap();
    let saw_error_chunk = updates.updates.iter().any(|u| {
        matches!(u, agent_client_protocol::SessionUpdate::AgentMessageChunk(c)
            if matches!(&c.content, ContentBlock::Text(t) if t.text.contains("No API key")))
    });
    assert!(
        saw_error_chunk,
        "error should be surfaced as agent chunk: {} updates",
        updates.updates.len()
    );
});

// ---------------------------------------------------------------------------
// Modes / models / config options
// ---------------------------------------------------------------------------

acp_test!(modes_models_config_advertised_and_switchable, async {
    let h = harness(vec![assistant_text("ok")]).await;
    h.client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    let session = h
        .client_conn
        .new_session(NewSessionRequest::new(h.session_dir.path().to_path_buf()))
        .await
        .unwrap();

    // Modes advertised.
    let modes = session.modes.expect("modes state");
    let mode_ids: Vec<&str> = modes
        .available_modes
        .iter()
        .map(|m| m.id.0.as_ref())
        .collect();
    assert!(
        mode_ids.contains(&"ask")
            && mode_ids.contains(&"acceptEdits")
            && mode_ids.contains(&"bypass")
            && mode_ids.contains(&"plan")
    );
    assert_eq!(modes.current_mode_id.0.as_ref(), "ask");

    // Models advertised (mock provider has no catalog — available list may be empty,
    // but current model id is set).
    let models = session.models.expect("models state");
    assert_eq!(models.current_model_id.0.as_ref(), "mock");

    // Config options: effort selector with current value off.
    let options = session.config_options.expect("config options");
    let thinking = options
        .iter()
        .find(|o| o.id.0.as_ref() == "thinking")
        .unwrap();
    let agent_client_protocol::SessionConfigKind::Select(select) = &thinking.kind else {
        panic!("select kind")
    };
    assert_eq!(select.current_value.0.as_ref(), "off");

    // Switch mode to plan.
    h.client_conn
        .set_session_mode(agent_client_protocol::SetSessionModeRequest::new(
            session.session_id.clone(),
            agent_client_protocol::SessionModeId::new("plan"),
        ))
        .await
        .unwrap();

    // Switch effort to high via config option.
    let updated = h
        .client_conn
        .set_session_config_option(agent_client_protocol::SetSessionConfigOptionRequest::new(
            session.session_id.clone(),
            agent_client_protocol::SessionConfigId::new("thinking"),
            agent_client_protocol::SessionConfigValueId::new("high"),
        ))
        .await
        .unwrap();
    let thinking = updated
        .config_options
        .iter()
        .find(|o| o.id.0.as_ref() == "thinking")
        .unwrap();
    let agent_client_protocol::SessionConfigKind::Select(select) = &thinking.kind else {
        panic!("select kind")
    };
    assert_eq!(select.current_value.0.as_ref(), "high");

    // Unknown mode rejected.
    assert!(
        h.client_conn
            .set_session_mode(agent_client_protocol::SetSessionModeRequest::new(
                session.session_id.clone(),
                agent_client_protocol::SessionModeId::new("nope"),
            ))
            .await
            .is_err()
    );
});

acp_test!(plan_mode_blocks_mutating_tools, async {
    let h = harness(vec![
        assistant_tool_call("t1", "write", json!({"path": "x.txt", "content": "x"})),
        assistant_text("blocked"),
    ])
    .await;
    h.client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    let session = h
        .client_conn
        .new_session(NewSessionRequest::new(h.session_dir.path().to_path_buf()))
        .await
        .unwrap();
    h.client_conn
        .set_session_mode(agent_client_protocol::SetSessionModeRequest::new(
            session.session_id.clone(),
            agent_client_protocol::SessionModeId::new("plan"),
        ))
        .await
        .unwrap();

    let response = h
        .client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("write x.txt")],
        ))
        .await
        .unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    // The write tool must have been blocked (Failed) without a permission prompt.
    assert_eq!(h.client.state.lock().unwrap().permission_requests, 0);
    assert!(!h.session_dir.path().join("x.txt").exists());
    let updates = h.client.state.lock().unwrap();
    let saw_failed = updates.updates.iter().any(|u| {
        matches!(u, agent_client_protocol::SessionUpdate::ToolCallUpdate(upd)
            if upd.fields.status == Some(agent_client_protocol::ToolCallStatus::Failed))
    });
    assert!(saw_failed);
});

acp_test!(bypass_mode_skips_permission_prompts, async {
    let h = harness(vec![
        assistant_tool_call("t1", "write", json!({"path": "x.txt", "content": "x"})),
        assistant_text("wrote"),
    ])
    .await;
    h.client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    let session = h
        .client_conn
        .new_session(NewSessionRequest::new(h.session_dir.path().to_path_buf()))
        .await
        .unwrap();
    h.client_conn
        .set_session_mode(agent_client_protocol::SetSessionModeRequest::new(
            session.session_id.clone(),
            agent_client_protocol::SessionModeId::new("bypass"),
        ))
        .await
        .unwrap();

    h.client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("write x.txt")],
        ))
        .await
        .unwrap();

    assert_eq!(h.client.state.lock().unwrap().permission_requests, 0);
    assert!(h.session_dir.path().join("x.txt").exists());
});

acp_test!(usage_update_and_slash_commands, async {
    let h = harness(vec![assistant_text("analysis done")]).await;
    h.client_conn
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .unwrap();
    let session = h
        .client_conn
        .new_session(NewSessionRequest::new(h.session_dir.path().to_path_buf()))
        .await
        .unwrap();

    // Available commands advertised.
    wait_for_update(&h.client, "available commands", |updates| {
        updates.iter().any(|u| {
            matches!(u, agent_client_protocol::SessionUpdate::AvailableCommandsUpdate(c)
                if c.available_commands.iter().any(|cmd| cmd.name == "compact"))
        })
    })
    .await;
    {
        let updates = h.client.state.lock().unwrap();
        let saw_commands = updates.updates.iter().any(|u| {
            matches!(u, agent_client_protocol::SessionUpdate::AvailableCommandsUpdate(c)
                if c.available_commands.iter().any(|cmd| cmd.name == "compact"))
        });
        assert!(saw_commands, "available commands should be advertised");
    }

    // Prompt produces a usage update.
    h.client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("analyze this project")],
        ))
        .await
        .unwrap();
    wait_for_update(&h.client, "usage and title updates", |updates| {
        updates
            .iter()
            .any(|u| matches!(u, agent_client_protocol::SessionUpdate::UsageUpdate(_)))
            && updates.iter().any(|u| {
                matches!(
                    u,
                    agent_client_protocol::SessionUpdate::SessionInfoUpdate(_)
                )
            })
    })
    .await;
    let (saw_usage, saw_title) = {
        let updates = h.client.state.lock().unwrap();
        let saw_usage = updates
            .updates
            .iter()
            .any(|u| matches!(u, agent_client_protocol::SessionUpdate::UsageUpdate(_)));
        let saw_title = updates.updates.iter().any(|u| {
            matches!(
                u,
                agent_client_protocol::SessionUpdate::SessionInfoUpdate(_)
            )
        });
        (saw_usage, saw_title)
    };
    assert!(saw_usage, "usage update expected after assistant message");
    assert!(saw_title, "session title update expected");

    // /rules command replies inline without hitting the provider.
    let before = h.client.state.lock().unwrap().updates.len();
    let response = h
        .client_conn
        .prompt(PromptRequest::new(
            session.session_id.clone(),
            vec![text_block("/rules")],
        ))
        .await
        .unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    wait_for_update(&h.client, "inline /rules reply", |updates| {
        updates[before..].iter().any(|u| {
            matches!(u, agent_client_protocol::SessionUpdate::AgentMessageChunk(c)
                if matches!(&c.content, ContentBlock::Text(t) if t.text.contains("rules") || t.text.contains("Rules")))
        })
    })
    .await;
    let reply = {
        let updates = h.client.state.lock().unwrap();
        let new_updates = &updates.updates[before..];
        new_updates.iter().any(|u| {
            matches!(u, agent_client_protocol::SessionUpdate::AgentMessageChunk(c)
                if matches!(&c.content, ContentBlock::Text(t) if t.text.contains("rules") || t.text.contains("Rules")))
        })
    };
    assert!(reply, "/rules should reply inline");
});

acp_test!(initialize_negotiates_protocol_version, async {
    let h = harness(vec![]).await;
    // A client advertising a NEWER protocol than the agent supports must get
    // the agent's latest version back, not its own echoed (ACP spec:
    // "the agent responds with the latest version it supports").
    let future_version: ProtocolVersion = serde_json::from_value(json!(99)).unwrap();
    let init = h
        .client_conn
        .initialize(InitializeRequest::new(future_version))
        .await
        .unwrap();
    assert_eq!(init.protocol_version, ProtocolVersion::LATEST);
});
