//! End-to-end test for the CBOR remote session protocol: real server over
//! TCP loopback, real RemoteClient, scripted provider.
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use std::sync::Arc;

use std::sync::Mutex;
use tack_ai::{
    AssistantMessage, ContentBlock, Context, Model, ModelCost, Provider, StopReason, StreamOptions,
    event_stream,
};
use tack_app::settings::Settings;
use tack_protocol::RemoteClient;
use tack_protocol::schemas::{Command, CommandResult, ModelRef, ServerEvent, TranscriptProgress};

#[derive(Debug)]
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
                m.stop_reason = StopReason::Error;
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
                if let ContentBlock::Text { text, .. } = block {
                    let _ = sender.push(tack_ai::AssistantMessageEvent::TextDelta {
                        content_index: i,
                        delta: text.clone(),
                        partial: message.clone(),
                    });
                }
            }
            match message.stop_reason {
                StopReason::Error | StopReason::Aborted => {
                    sender.finish(tack_ai::AssistantMessageEvent::Error {
                        reason: message.stop_reason,
                        error: message,
                    })
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

#[tokio::test]
async fn remote_session_over_tcp() {
    // Scripted provider answer.
    let mut answer = AssistantMessage::pending(&test_model());
    answer.stop_reason = StopReason::Stop;
    answer.content = vec![ContentBlock::text("remote-ok")];
    answer.usage.input = 10;
    answer.usage.output = 5;
    answer.usage.total_tokens = 15;

    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        scripts: Mutex::new(vec![answer]),
    });

    // Hand-build a host with the scripted provider; serve on loopback.
    let agent_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let host = tack_app::remote::build_host(
        provider,
        test_model(),
        std::sync::Arc::new(tack_ai::oauth::StaticAuth::from(Some(
            "test-key".to_string(),
        ))),
        Settings::default(),
        None,
        tack_app::extension_host::ExtensionManager::default(),
        tack_app::mcp_sampling::SharedSamplingLlm::default(),
        tack_app::remote::ext_bridge::RemoteExtBridge::new(),
    );
    let server_task = tokio::spawn(tack_app::remote::serve_tcp_listener(listener, host));

    // Client: hello + create + prompt.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let client = RemoteClient::connect(stream, None, Vec::new())
        .await
        .unwrap();
    assert_eq!(client.snapshot.protocol_version, 1);

    let created = client
        .request(Command::Create {
            cwd: Some(agent_dir.path().to_string_lossy().to_string()),
            name: None,
            model: None,
            thinking_level: None,
        })
        .await
        .unwrap();
    let CommandResult::Create { session } = &created else {
        panic!("expected create")
    };
    let session_id = session.id.clone();
    assert_eq!(session.phase, tack_protocol::schemas::SessionPhase::Idle);

    let mut events = client.subscribe();
    let result = client
        .request(Command::Prompt {
            session_id: session_id.clone(),
            text: "hello".into(),
        })
        .await
        .unwrap();
    assert!(matches!(result, CommandResult::Prompt { .. }));

    // Wait for the final session snapshot event and check streamed progress.
    let mut saw_delta = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline.into(), events.recv())
            .await
            .unwrap()
            .unwrap();
        if let ServerEvent::SessionProgress { progress, .. } = &event
            && let TranscriptProgress::AssistantDelta { delta, .. } = progress
            && delta.contains("remote-ok")
        {
            saw_delta = true;
        }
        if matches!(&event, ServerEvent::SessionSnapshot { snapshot } if snapshot.phase == tack_protocol::schemas::SessionPhase::Idle)
        {
            break;
        }
    }
    assert!(saw_delta, "expected streamed text delta");

    // List shows the session.
    let listed = client.request(Command::List).await.unwrap();
    let CommandResult::List { sessions } = listed else {
        panic!()
    };
    assert!(sessions.iter().any(|s| s.id == session_id));

    // Attach returns the transcript.
    let attached = client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let CommandResult::Attach { session } = attached else {
        panic!()
    };
    assert!(session.transcript.len() >= 2);

    // set_thinking + set_model + abort + detach respond.
    client
        .request(Command::SetThinking {
            session_id: session_id.clone(),
            thinking_level: tack_protocol::schemas::ThinkingLevel::Low,
        })
        .await
        .unwrap();
    client
        .request(Command::SetModel {
            session_id: session_id.clone(),
            model: ModelRef {
                provider: "anthropic".into(),
                id: "mock".into(),
            },
        })
        .await
        .unwrap();
    // Unknown provider errors.
    client
        .request(Command::SetModel {
            session_id: session_id.clone(),
            model: ModelRef {
                provider: "nonexistent".into(),
                id: "x".into(),
            },
        })
        .await
        .unwrap_err();
    client
        .request(Command::Abort {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    client
        .request(Command::Detach { session_id })
        .await
        .unwrap();

    server_task.abort();
}

/// Token auth: clients without/with the wrong token are rejected at the
/// handshake; the right token connects.
#[tokio::test]
async fn serve_requires_auth_token_when_configured() {
    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        scripts: Mutex::new(vec![]),
    });
    // NOTE: this test never creates a session, so it must NOT touch the
    // process-global TACK_AGENT_DIR (it races remote_session_over_tcp).

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let host = tack_app::remote::build_host(
        provider,
        test_model(),
        std::sync::Arc::new(tack_ai::oauth::StaticAuth::from(Some(
            "test-key".to_string(),
        ))),
        Settings::default(),
        Some("s3cret".to_string()),
        tack_app::extension_host::ExtensionManager::default(),
        tack_app::mcp_sampling::SharedSamplingLlm::default(),
        tack_app::remote::ext_bridge::RemoteExtBridge::new(),
    );
    let server_task = tokio::spawn(tack_app::remote::serve_tcp_listener(listener, host));

    // No token → rejected.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let err = RemoteClient::connect(stream, None, Vec::new())
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("auth"), "{err:?}");

    // Wrong token → rejected.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let err = RemoteClient::connect(stream, Some("wrong".into()), Vec::new())
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("auth"), "{err:?}");

    // Right token → handshake succeeds.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let client = RemoteClient::connect(stream, Some("s3cret".into()), Vec::new())
        .await
        .unwrap();
    assert_eq!(client.snapshot.protocol_version, 1);

    server_task.abort();
}

// ---------------------------------------------------------------------------
// Extension surfaces (plugins) over the wire
// ---------------------------------------------------------------------------

/// Spin up a host with an attached ext bridge (no sessions are created in
/// these tests, so TACK_AGENT_DIR stays untouched).
async fn ext_server() -> (
    std::net::SocketAddr,
    Arc<tack_app::remote::ext_bridge::RemoteExtBridge>,
    Arc<tokio::sync::Mutex<tack_app::remote::SessionHost>>,
    tokio::task::JoinHandle<()>,
) {
    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        scripts: Mutex::new(vec![]),
    });
    let bridge = tack_app::remote::ext_bridge::RemoteExtBridge::new();
    let host = tack_app::remote::build_host(
        provider,
        test_model(),
        std::sync::Arc::new(tack_ai::oauth::StaticAuth::from(Some(
            "test-key".to_string(),
        ))),
        Settings::default(),
        None,
        tack_app::extension_host::ExtensionManager::default(),
        tack_app::mcp_sampling::SharedSamplingLlm::default(),
        bridge.clone(),
    );
    bridge.attach(&host).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_task = {
        let host = host.clone();
        tokio::spawn(async move {
            let _ = tack_app::remote::serve_tcp_listener(listener, host).await;
        })
    };
    (addr, bridge, host, server_task)
}

#[cfg(feature = "ext")]
fn widget_capabilities() -> Vec<String> {
    vec![tack_protocol::schemas::CAP_EXT_WIDGETS.to_string()]
}

fn dialog_capabilities() -> Vec<String> {
    vec![tack_protocol::schemas::CAP_EXT_DIALOGS.to_string()]
}

/// Widget events only reach connections that negotiated `ext_widgets`;
/// the pull surface (`list_ext_widgets`) works for every client.
/// (Ext-gated: registering a widget needs the real extension host.)
#[cfg(feature = "ext")]
#[tokio::test]
async fn ext_widget_events_require_capability() {
    let (addr, bridge, host, server_task) = ext_server().await;
    host.lock()
        .await
        .extensions_for_testing()
        .test_insert_widget(
            "plug",
            tack_ext::rpc3::WidgetSpec {
                id: "w1".to_string(),
                r#type: tack_ext::rpc3::WidgetKind::MarkdownPanel,
                title: Some("panel".to_string()),
                ..Default::default()
            },
        );

    let plain = RemoteClient::connect(
        tokio::net::TcpStream::connect(addr).await.unwrap(),
        None,
        Vec::new(),
    )
    .await
    .unwrap();
    let capable = RemoteClient::connect(
        tokio::net::TcpStream::connect(addr).await.unwrap(),
        None,
        widget_capabilities(),
    )
    .await
    .unwrap();
    // The server advertises its surfaces in the hello.
    assert_eq!(
        capable.server_capabilities,
        tack_protocol::schemas::SERVER_CAPABILITIES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
    let mut plain_events = plain.subscribe();
    let mut capable_events = capable.subscribe();

    bridge
        .apply_widget_update("plug", "w1", serde_json::json!({"md": "hi"}), None)
        .await;

    // The capable client receives the full new state.
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), capable_events.recv())
        .await
        .expect("widget event in time")
        .expect("events open");
    let ServerEvent::ExtWidgetUpdate { widget } = event else {
        panic!("expected ext_widget_update, got {event:?}")
    };
    assert_eq!(widget.key, "plug:w1");
    assert_eq!(widget.kind, "markdownPanel");
    assert_eq!(widget.state, Some(serde_json::json!({"md": "hi"})));
    assert_eq!(widget.rev, 1);

    // The pre-extension client sees nothing (its event stream is the
    // exact v1 set).
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), plain_events.recv())
            .await
            .is_err(),
        "no-capability client must not receive ext events"
    );

    // Pull surfaces are capability-free: even the plain client can list.
    let result = plain.request(Command::ListExtWidgets).await.unwrap();
    let CommandResult::ListExtWidgets { widgets } = result else {
        panic!("expected widget list")
    };
    assert_eq!(widgets.len(), 1);
    assert_eq!(widgets[0].state, Some(serde_json::json!({"md": "hi"})));

    // Removal broadcasts too (capable client only).
    bridge.remove_plugin_widgets("plug").await;
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), capable_events.recv())
        .await
        .expect("removal event in time")
        .expect("events open");
    assert!(
        matches!(event, ServerEvent::ExtWidgetsRemoved { plugin, keys } if plugin == "plug" && keys == ["plug:w1"])
    );
    let result = plain.request(Command::ListExtWidgets).await.unwrap();
    assert!(matches!(
        result,
        CommandResult::ListExtWidgets { widgets } if widgets.is_empty()
    ));

    server_task.abort();
}

/// The full dialog round trip over TCP: request broadcast → first answer
/// wins → dismissal broadcast → late answers error.
#[tokio::test]
async fn ext_dialog_round_trip_over_tcp() {
    use tack_app::remote::ext_bridge::ExtDialogSpec;
    use tack_protocol::schemas::ExtDialogKind;

    let (addr, bridge, _host, server_task) = ext_server().await;
    let client = RemoteClient::connect(
        tokio::net::TcpStream::connect(addr).await.unwrap(),
        None,
        dialog_capabilities(),
    )
    .await
    .unwrap();
    let mut events = client.subscribe();

    let ask = {
        let bridge = bridge.clone();
        tokio::spawn(async move {
            bridge
                .ask_dialog(ExtDialogSpec {
                    source: "plug".to_string(),
                    kind: ExtDialogKind::Select,
                    title: "Pick one".to_string(),
                    message: None,
                    options: vec!["a".to_string(), "b".to_string()],
                    placeholder: None,
                    fields: Vec::new(),
                })
                .await
        })
    };
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .expect("dialog event in time")
        .expect("events open");
    let ServerEvent::ExtDialogRequest {
        request_id,
        source,
        kind,
        options,
        ..
    } = event
    else {
        panic!("expected ext_dialog_request, got {event:?}")
    };
    assert_eq!(source, "plug");
    assert_eq!(kind, ExtDialogKind::Select);
    assert_eq!(options, vec!["a".to_string(), "b".to_string()]);

    client
        .request(Command::ExtDialogResponse {
            request_id: request_id.clone(),
            cancelled: false,
            value: Some(serde_json::json!("b")),
        })
        .await
        .unwrap();
    let answer = ask.await.unwrap().expect("dialog answered");
    assert!(!answer.cancelled);
    assert_eq!(answer.value, Some(serde_json::json!("b")));

    // The dismissal broadcast follows (other clients would dismiss).
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .expect("closed event in time")
        .expect("events open");
    assert!(matches!(event, ServerEvent::ExtDialogClosed { request_id: id } if id == request_id));

    // A late answer finds nothing.
    let err = client
        .request(Command::ExtDialogResponse {
            request_id,
            cancelled: false,
            value: None,
        })
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("not_found") || format!("{err:?}").contains("NotFound"),
        "{err:?}"
    );

    server_task.abort();
}

/// A dialog with NO dialog-capable client fails fast instead of parking
/// (plain-headless semantics), and disconnecting the last capable client
/// fails parked dialogs.
#[tokio::test]
async fn ext_dialog_fails_fast_without_capable_clients() {
    use tack_app::remote::ext_bridge::ExtDialogSpec;
    use tack_protocol::schemas::ExtDialogKind;

    let (addr, bridge, _host, server_task) = ext_server().await;
    // A client WITHOUT the capability does not count as an answerer.
    let _plain = RemoteClient::connect(
        tokio::net::TcpStream::connect(addr).await.unwrap(),
        None,
        Vec::new(),
    )
    .await
    .unwrap();
    let spec = || ExtDialogSpec {
        source: "plug".to_string(),
        kind: ExtDialogKind::Confirm,
        title: "Sure?".to_string(),
        message: None,
        options: Vec::new(),
        placeholder: None,
        fields: Vec::new(),
    };
    let err = bridge.ask_dialog(spec()).await.unwrap_err();
    assert!(err.contains("no dialog-capable"), "{err}");

    // A capable client connects → dialog parks; it disconnects → the
    // parked dialog fails.
    let capable = RemoteClient::connect(
        tokio::net::TcpStream::connect(addr).await.unwrap(),
        None,
        dialog_capabilities(),
    )
    .await
    .unwrap();
    // Wait until the server registered the connection (handshake side
    // effect is asynchronous to connect()).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while bridge.dialog_answerers() == 0 {
        assert!(std::time::Instant::now() < deadline, "never registered");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let ask = {
        let bridge = bridge.clone();
        tokio::spawn(async move { bridge.ask_dialog(spec()).await })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while bridge.pending_dialog_ids().is_empty() {
        assert!(std::time::Instant::now() < deadline, "never parked");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    drop(capable);
    let err = ask.await.unwrap().unwrap_err();
    assert!(err.contains("closed"), "{err}");

    server_task.abort();
}

/// Pull surfaces with no plugins loaded (over the wire): empty lists and
/// clean NotFound errors.
#[tokio::test]
async fn ext_surfaces_degrade_without_plugins_over_tcp() {
    let (addr, _bridge, _host, server_task) = ext_server().await;
    let client = RemoteClient::connect(
        tokio::net::TcpStream::connect(addr).await.unwrap(),
        None,
        Vec::new(),
    )
    .await
    .unwrap();

    let result = client.request(Command::ListExtCommands).await.unwrap();
    assert!(matches!(
        result,
        CommandResult::ListExtCommands { commands } if commands.is_empty()
    ));
    let result = client.request(Command::ListExtAutocomplete).await.unwrap();
    assert!(matches!(
        result,
        CommandResult::ListExtAutocomplete { providers } if providers.is_empty()
    ));
    let err = client
        .request(Command::InvokeExtCommand {
            name: "nope".to_string(),
            args: None,
        })
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("not_found") || format!("{err:?}").contains("NotFound"),
        "{err:?}"
    );

    server_task.abort();
}
