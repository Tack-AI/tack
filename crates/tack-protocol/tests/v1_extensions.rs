//! Tests for the additive v1 protocol extensions (set_mode,
//! permission_request/permission_response, list_models) and the
//! forward-compatibility catch-alls.
//!
//! Compatibility strategy: every extension is either a NEW enum variant
//! (command/result/event) or a NEW optional field. Peers built before the
//! extension must not crash on unknown messages: the tagged enums carry an
//! `#[serde(other)] Unknown` catch-all, and unknown struct fields are
//! skipped by serde. These tests pin that behavior.
#![allow(clippy::unwrap_used)]

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use tack_protocol::schemas::*;
use tack_protocol::{decode_payload, encode_payload};

fn roundtrip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let bytes = encode_payload(value).unwrap();
    let decoded: T = decode_payload(&bytes).unwrap();
    assert_eq!(&decoded, value);
    // Canonical: re-encoding the decoded value yields identical bytes.
    assert_eq!(encode_payload(&decoded).unwrap(), bytes);
}

fn snapshot() -> SessionSnapshot {
    SessionSnapshot {
        id: "sess-1".into(),
        name: None,
        cwd: "/work".into(),
        created_at: 1,
        updated_at: 2,
        phase: SessionPhase::Idle,
        model: ModelRef {
            provider: "anthropic".into(),
            id: "k3".into(),
        },
        thinking_level: ThinkingLevel::Off,
        attached: true,
        locked: false,
        revision: 0,
        mode: None,
        transcript: vec![],
        queued_steer: vec![],
        queued_steer_count: 0,
    }
}

#[test]
fn session_mode_wire_names() {
    // Exact wire spellings (camelCase acceptEdits, matching TS pi / ACP).
    roundtrip(&SessionMode::Ask);
    roundtrip(&SessionMode::AcceptEdits);
    roundtrip(&SessionMode::Plan);
    roundtrip(&SessionMode::Bypass);
    let wire = |m: &SessionMode| decode_payload::<String>(&encode_payload(m).unwrap()).unwrap();
    assert_eq!(wire(&SessionMode::AcceptEdits), "acceptEdits");
    assert_eq!(wire(&SessionMode::Ask), "ask");
}

#[test]
fn new_commands_roundtrip() {
    roundtrip(&Command::SetMode {
        session_id: "s".into(),
        mode: SessionMode::AcceptEdits,
    });
    roundtrip(&Command::PermissionResponse {
        request_id: "perm-1".into(),
        decision: PermissionDecision::AllowAlways,
    });
    roundtrip(&Command::ListModels);

    // Tag spellings on the wire.
    let bytes = encode_payload(&Command::SetMode {
        session_id: "s".into(),
        mode: SessionMode::Plan,
    })
    .unwrap();
    let json: serde_json::Value = decode_payload(&bytes).unwrap();
    assert_eq!(json["command"], "set_mode");
    assert_eq!(json["sessionId"], "s");
    assert_eq!(json["mode"], "plan");

    let bytes = encode_payload(&Command::PermissionResponse {
        request_id: "r".into(),
        decision: PermissionDecision::Deny,
    })
    .unwrap();
    let json: serde_json::Value = decode_payload(&bytes).unwrap();
    assert_eq!(json["command"], "permission_response");
    assert_eq!(json["requestId"], "r");
    assert_eq!(json["decision"], "deny");
}

#[test]
fn new_command_results_roundtrip() {
    roundtrip(&CommandResult::SetMode {
        session: snapshot(),
    });
    roundtrip(&CommandResult::PermissionResponse);
    roundtrip(&CommandResult::ListModels { models: vec![] });
    roundtrip(&CommandResult::ListModels {
        models: vec![ModelMetadata {
            provider: "anthropic".into(),
            id: "k3".into(),
            name: "K3".into(),
            api: "anthropic-messages".into(),
            reasoning: true,
            input: vec!["text".into()],
            context_window: 200_000,
            max_tokens: 4096,
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 1.5,
            },
            supported_thinking_levels: vec![ThinkingLevel::Low, ThinkingLevel::High],
            authenticated: true,
        }],
    });
}

#[test]
fn permission_request_event_roundtrip() {
    let event = ServerEvent::PermissionRequest {
        session_id: "s".into(),
        request_id: "perm-1".into(),
        tool_call_id: "call-1".into(),
        tool_name: "bash".into(),
        title: "bash: cargo test".into(),
        input: serde_json::json!({"command": "cargo test"}),
    };
    roundtrip(&event);
    let bytes = encode_payload(&event).unwrap();
    let json: serde_json::Value = decode_payload(&bytes).unwrap();
    assert_eq!(json["type"], "permission_request");
    assert_eq!(json["sessionId"], "s");
    assert_eq!(json["requestId"], "perm-1");
    assert_eq!(json["toolCallId"], "call-1");
    assert_eq!(json["toolName"], "bash");
}

#[test]
fn snapshot_mode_is_optional_on_the_wire() {
    // mode: None → key absent (pre-extension encodings unchanged).
    let bytes = encode_payload(&snapshot()).unwrap();
    let json: serde_json::Value = decode_payload(&bytes).unwrap();
    assert!(json.get("mode").is_none(), "mode must be omitted when None");

    // mode: Some(..) → present, and decodes back.
    let with_mode = SessionSnapshot {
        mode: Some(SessionMode::Ask),
        ..snapshot()
    };
    roundtrip(&with_mode);
    let bytes = encode_payload(&with_mode).unwrap();
    let json: serde_json::Value = decode_payload(&bytes).unwrap();
    assert_eq!(json["mode"], "ask");

    // A pre-extension snapshot (no mode key) decodes to mode: None.
    let decoded: SessionSnapshot = decode_payload(&encode_payload(&snapshot()).unwrap()).unwrap();
    assert_eq!(decoded.mode, None);
}

#[test]
fn unknown_command_tag_decodes_as_catch_all() {
    // A NEWER peer sends a command this build does not know: the frame
    // must still decode (as Unknown) instead of failing the connection.
    let future = serde_json::json!({"command": "compact", "sessionId": "s"});
    let mut bytes = Vec::new();
    ciborium::into_writer(&future, &mut bytes).unwrap();
    let decoded: Command = decode_payload(&bytes).unwrap();
    assert_eq!(decoded, Command::Unknown);

    // Same for results, events, and top-level messages.
    let future = serde_json::json!({"command": "fork", "sessionId": "s"});
    let mut bytes = Vec::new();
    ciborium::into_writer(&future, &mut bytes).unwrap();
    assert_eq!(
        decode_payload::<CommandResult>(&bytes).unwrap(),
        CommandResult::Unknown
    );

    let future = serde_json::json!({"type": "cost_update", "total": 1.0});
    let mut bytes = Vec::new();
    ciborium::into_writer(&future, &mut bytes).unwrap();
    assert_eq!(
        decode_payload::<ServerEvent>(&bytes).unwrap(),
        ServerEvent::Unknown
    );

    let future = serde_json::json!({"type": "pong"});
    let mut bytes = Vec::new();
    ciborium::into_writer(&future, &mut bytes).unwrap();
    assert_eq!(
        decode_payload::<ServerMessage>(&bytes).unwrap(),
        ServerMessage::Unknown
    );
    assert_eq!(
        decode_payload::<ClientMessage>(&bytes).unwrap(),
        ClientMessage::Unknown
    );
}

#[test]
fn known_variants_still_decode_despite_catch_all() {
    // The catch-all must not swallow real variants.
    for command in [
        Command::List,
        Command::SetMode {
            session_id: "s".into(),
            mode: SessionMode::Bypass,
        },
        Command::ListModels,
    ] {
        let decoded: Command = decode_payload(&encode_payload(&command).unwrap()).unwrap();
        assert_eq!(decoded, command);
    }
    let event = ServerEvent::SessionRemoved {
        session_id: "s".into(),
    };
    assert_eq!(
        decode_payload::<ServerEvent>(&encode_payload(&event).unwrap()).unwrap(),
        event
    );
}

#[test]
fn nesting_depth_cap_still_applies_to_new_messages() {
    // The recursion limit guards the decoder regardless of message shape:
    // a permission_response wrapped in absurd nesting is rejected, a sane
    // one decodes.
    let mut deep = vec![0x81u8; tack_protocol::MAX_CBOR_NESTING_DEPTH + 32];
    deep.push(0xf6);
    assert!(decode_payload::<Command>(&deep).is_err());

    let ok = encode_payload(&Command::PermissionResponse {
        request_id: "r".into(),
        decision: PermissionDecision::AllowOnce,
    })
    .unwrap();
    assert!(decode_payload::<Command>(&ok).is_ok());
}
