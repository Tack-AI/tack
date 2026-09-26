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
    );
    let server_task = tokio::spawn(tack_app::remote::serve_tcp_listener(listener, host));

    // Client: hello + create + prompt.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let client = RemoteClient::connect(stream, None).await.unwrap();
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
    );
    let server_task = tokio::spawn(tack_app::remote::serve_tcp_listener(listener, host));

    // No token → rejected.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let err = RemoteClient::connect(stream, None).await.unwrap_err();
    assert!(format!("{err:?}").contains("auth"), "{err:?}");

    // Wrong token → rejected.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let err = RemoteClient::connect(stream, Some("wrong".into()))
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("auth"), "{err:?}");

    // Right token → handshake succeeds.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let client = RemoteClient::connect(stream, Some("s3cret".into()))
        .await
        .unwrap();
    assert_eq!(client.snapshot.protocol_version, 1);

    server_task.abort();
}
