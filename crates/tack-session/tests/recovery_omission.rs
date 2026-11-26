#![allow(clippy::unwrap_used)]

use std::path::Path;

use serde_json::json;
use tack_agent_core::AgentMessage;
use tack_ai::{
    AssistantMessage, ContentBlock, InputContentBlock, Model, StopReason, ToolResultMessage,
};
use tack_session::{
    ContextEditContent, ContextEditReplacement, SessionBackend, SessionEntry, SessionError,
    SessionManager, build_session_projection, omit_recovery_attempt,
};

fn assistant(text: &str, tool_call_ids: &[&str], stop_reason: StopReason) -> AgentMessage {
    let model = Model {
        id: "mock".into(),
        name: "mock".into(),
        api: "mock".into(),
        provider: "mock".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    let mut message = AssistantMessage::pending(&model);
    message.content = vec![ContentBlock::text(text)];
    message
        .content
        .extend(tool_call_ids.iter().map(|id| ContentBlock::ToolCall {
            id: (*id).to_string(),
            name: "read".into(),
            arguments: json!({}),
            arguments_error: None,
            thought_signature: None,
            namespace: None,
        }));
    message.stop_reason = stop_reason;
    message.timestamp = 1234;
    AgentMessage::Assistant(message)
}

fn tool_result(tool_call_id: &str, text: &str) -> AgentMessage {
    AgentMessage::ToolResult(ToolResultMessage {
        tool_call_id: tool_call_id.into(),
        tool_name: "read".into(),
        content: vec![InputContentBlock::text(text)],
        details: None,
        usage: None,
        is_error: true,
        timestamp: 1235,
    })
}

fn create_session(tmp: &tempfile::TempDir, backend: SessionBackend, id: &str) -> SessionManager {
    let cwd = tmp.path().join(format!("project-{id}"));
    std::fs::create_dir_all(&cwd).unwrap();
    SessionManager::create_with_id_and_backend(
        &cwd,
        Some(tmp.path().join("sessions")),
        id.into(),
        backend,
    )
    .unwrap()
}

fn context_edit_targets(session: &SessionManager, edit_ids: &[String]) -> Vec<String> {
    let entries = session.entries();
    edit_ids
        .iter()
        .map(|edit_id| {
            let entry = entries.iter().find(|entry| entry.id() == edit_id).unwrap();
            let SessionEntry::ContextEdit {
                target_id,
                replacement,
                ..
            } = entry
            else {
                panic!("expected context edit: {entry:?}");
            };
            assert!(replacement.is_none());
            target_id.clone()
        })
        .collect()
}

fn projected_message_count(session: &SessionManager, source_entry_id: &str) -> usize {
    build_session_projection(&session.entries(), session.leaf_id())
        .entries
        .iter()
        .filter(|entry| entry.source_entry.id() == source_entry_id)
        .flat_map(|entry| entry.messages.iter())
        .count()
}

#[test]
fn replacement_and_duplicate_messages_are_resolved_only_by_source_id() {
    let mut session = SessionManager::in_memory(Path::new("/work"));
    let duplicate_message = assistant("same partial", &["call-a", "call-b"], StopReason::Length);
    let duplicate_id = session.append_message(duplicate_message.clone()).unwrap();
    let target_id = session.append_message(duplicate_message).unwrap();
    let unrelated_id = session
        .append_message(tool_result("unrelated", "keep unrelated"))
        .unwrap();
    let result_a = session
        .append_message(tool_result("call-a", "omit A"))
        .unwrap();
    let result_b = session
        .append_message(tool_result("call-b", "omit B"))
        .unwrap();
    let next_assistant = session
        .append_message(assistant("next assistant", &[], StopReason::Stop))
        .unwrap();
    let later_result = session
        .append_message(tool_result("call-a", "keep later result"))
        .unwrap();

    // The replacement deliberately has no tool calls. Raw call ids still
    // identify the failed attempt's results, so omission cannot orphan them.
    session
        .append_context_edit(
            &target_id,
            Some(ContextEditReplacement {
                content: ContextEditContent::AssistantBlocks(vec![ContentBlock::text(
                    "edited partial",
                )]),
            }),
        )
        .unwrap();
    let edit_ids = omit_recovery_attempt(&mut session, &target_id).unwrap();
    assert_eq!(
        context_edit_targets(&session, &edit_ids),
        vec![target_id.clone(), result_a.clone(), result_b.clone()]
    );

    assert_eq!(projected_message_count(&session, &target_id), 0);
    assert_eq!(projected_message_count(&session, &result_a), 0);
    assert_eq!(projected_message_count(&session, &result_b), 0);
    for retained in [duplicate_id, unrelated_id, next_assistant, later_result] {
        assert_eq!(
            projected_message_count(&session, &retained),
            1,
            "{retained}"
        );
    }

    // Omission is append-only: even the replaced raw message remains exactly
    // equal to the duplicate, and both original results remain in history.
    let entries = session.entries();
    let raw_message = |id: &str| {
        entries
            .iter()
            .find_map(|entry| match entry {
                SessionEntry::Message { message, .. } if entry.id() == id => Some(message),
                _ => None,
            })
            .unwrap()
            .clone()
    };
    assert_eq!(
        raw_message(&target_id),
        assistant("same partial", &["call-a", "call-b"], StopReason::Length)
    );
    assert!(matches!(
        raw_message(&result_a),
        AgentMessage::ToolResult(result) if result.content == vec![InputContentBlock::text("omit A")]
    ));
    assert!(matches!(
        raw_message(&result_b),
        AgentMessage::ToolResult(result) if result.content == vec![InputContentBlock::text("omit B")]
    ));

    // A repeated recovery is a durable no-op, not a second assistant edit.
    let revision = session.revision();
    assert!(
        omit_recovery_attempt(&mut session, &target_id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(session.revision(), revision);
}

#[test]
fn assistant_only_prior_omission_can_still_clean_up_results() {
    let mut session = SessionManager::in_memory(Path::new("/work"));
    let assistant_id = session
        .append_message(assistant(
            "partial",
            &["call-a", "call-b"],
            StopReason::Length,
        ))
        .unwrap();
    let result_a = session.append_message(tool_result("call-a", "A")).unwrap();
    let result_b = session.append_message(tool_result("call-b", "B")).unwrap();
    session.append_context_edit(&result_a, None).unwrap();
    session.append_context_edit(&assistant_id, None).unwrap();

    // The assistant and result A already have omissions. Only the visible
    // matching result B needs a new edit; no duplicate edits are emitted.
    let edit_ids = omit_recovery_attempt(&mut session, &assistant_id).unwrap();
    assert_eq!(context_edit_targets(&session, &edit_ids), vec![result_b]);
    assert!(
        omit_recovery_attempt(&mut session, &assistant_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn resumed_length_attempt_and_results_disappear_from_projection_but_not_raw_history() {
    for (backend, id) in [
        (SessionBackend::Jsonl, "recovery-v3"),
        (SessionBackend::JsonlV4, "recovery-v4"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = create_session(&tmp, backend, id);
        session
            .append_message(AgentMessage::user("check status"))
            .unwrap();
        let assistant_id = session
            .append_message(assistant(
                "truncated response",
                &["call-a", "call-b"],
                StopReason::Length,
            ))
            .unwrap();
        let result_a = session
            .append_message(tool_result("call-a", "not executed A"))
            .unwrap();
        let result_b = session
            .append_message(tool_result("call-b", "not executed B"))
            .unwrap();
        let next_assistant = session
            .append_message(assistant("later assistant", &[], StopReason::Stop))
            .unwrap();
        let path = session.session_file().unwrap().to_path_buf();
        drop(session);

        // Resume first, then recover by the persisted source id. This is the
        // #11227 shape: pre-prompt recovery has no caller-supplied result list.
        let mut resumed = SessionManager::open_with_backend(&path, None, backend).unwrap();
        let edit_ids = omit_recovery_attempt(&mut resumed, &assistant_id).unwrap();
        assert_eq!(
            context_edit_targets(&resumed, &edit_ids),
            vec![assistant_id.clone(), result_a.clone(), result_b.clone()]
        );
        for omitted in [&assistant_id, &result_a, &result_b] {
            assert_eq!(projected_message_count(&resumed, omitted), 0);
        }
        assert_eq!(projected_message_count(&resumed, &next_assistant), 1);
        assert!(resumed.entries().iter().any(|entry| matches!(
            entry,
            SessionEntry::Message { message: AgentMessage::Assistant(message), .. }
                if entry.id() == assistant_id
                    && message.stop_reason == StopReason::Length
                    && message.text() == "truncated response"
        )));
        assert!(resumed.entries().iter().any(|entry| matches!(
            entry,
            SessionEntry::Message { message: AgentMessage::ToolResult(result), .. }
                if entry.id() == result_a && result.content == vec![InputContentBlock::text("not executed A")]
        )));

        // No compaction/retry is part of the helper. Even if a caller's later
        // compaction fails or is cancelled, these edits independently survive
        // another reopen and the raw messages remain available for audit.
        assert!(
            !resumed
                .entries()
                .iter()
                .any(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        );
        drop(resumed);
        let reopened = SessionManager::open_with_backend(&path, None, backend).unwrap();
        for omitted in [&assistant_id, &result_a, &result_b] {
            assert_eq!(projected_message_count(&reopened, omitted), 0);
        }
        assert!(reopened.entries().iter().any(|entry| matches!(
            entry,
            SessionEntry::Message { message: AgentMessage::Assistant(message), .. }
                if entry.id() == assistant_id && message.text() == "truncated response"
        )));
    }
}

#[test]
fn v4_materialized_retained_tail_uses_recorded_source_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let mut session = create_session(&tmp, SessionBackend::JsonlV4, "recovery-tail");
    session
        .append_message(AgentMessage::user("old input"))
        .unwrap();
    let failed = assistant("retained partial", &["call-tail"], StopReason::Length);
    let failed_result = tool_result("call-tail", "retained result");
    let assistant_id = session.append_message(failed.clone()).unwrap();
    let result_id = session.append_message(failed_result.clone()).unwrap();
    session
        .append_compaction(
            "summary",
            Some(assistant_id.clone()),
            100,
            Some(vec![failed, failed_result]),
            Some(json!({
                "tackRetainedTailSourceIds": [assistant_id, result_id]
            })),
            None,
        )
        .unwrap();
    let next_assistant = session
        .append_message(assistant("post-compaction", &[], StopReason::Stop))
        .unwrap();
    let path = session.session_file().unwrap().to_path_buf();
    drop(session);

    let mut resumed = SessionManager::open(&path, None).unwrap();
    let edit_ids = omit_recovery_attempt(&mut resumed, &assistant_id).unwrap();
    assert_eq!(
        context_edit_targets(&resumed, &edit_ids),
        vec![assistant_id.clone(), result_id.clone()]
    );
    let projected = resumed.build_session_context().messages;
    assert!(!projected.iter().any(|message| matches!(
        message,
        AgentMessage::Assistant(message) if message.text() == "retained partial"
    )));
    assert!(!projected.iter().any(|message| matches!(
        message,
        AgentMessage::ToolResult(result) if result.tool_call_id == "call-tail"
    )));
    assert!(projected.iter().any(|message| matches!(
        message,
        AgentMessage::Assistant(message) if message.text() == "post-compaction"
    )));
    assert_eq!(projected_message_count(&resumed, &next_assistant), 1);

    // The compaction checkpoint itself is raw history too; omission does not
    // rewrite its materialized tail in memory or on disk.
    assert!(resumed.entries().iter().any(|entry| matches!(
        entry,
        SessionEntry::Compaction { retained_tail: Some(tail), .. }
            if tail.iter().any(|message| matches!(
                message,
                AgentMessage::Assistant(message) if message.text() == "retained partial"
            ))
    )));
    drop(resumed);
    let reopened = SessionManager::open(&path, None).unwrap();
    assert!(!reopened.build_session_context().messages.iter().any(
        |message| matches!(message, AgentMessage::ToolResult(result) if result.tool_call_id == "call-tail")
    ));
}

#[test]
fn unresolved_or_illegal_sources_fail_without_writes_or_structural_guessing() {
    let mut session = SessionManager::in_memory(Path::new("/work"));
    let user_id = session
        .append_message(AgentMessage::user("not assistant"))
        .unwrap();
    let failed = assistant("same retained partial", &["call-tail"], StopReason::Length);
    let failed_result = tool_result("call-tail", "same retained result");
    let assistant_id = session.append_message(failed.clone()).unwrap();
    session.append_message(failed_result.clone()).unwrap();
    session
        .append_compaction(
            "summary without per-message provenance",
            Some(assistant_id.clone()),
            100,
            Some(vec![failed, failed_result]),
            None,
            None,
        )
        .unwrap();

    let before = session.entries();
    let revision = session.revision();
    for source_id in ["missing", user_id.as_str(), assistant_id.as_str()] {
        assert!(matches!(
            omit_recovery_attempt(&mut session, source_id),
            Err(SessionError::InvalidRecoveryOmission(_))
        ));
    }
    assert_eq!(session.revision(), revision);
    assert_eq!(session.entries().len(), before.len());
    assert!(
        !session
            .entries()
            .iter()
            .any(|entry| matches!(entry, SessionEntry::ContextEdit { .. }))
    );
}
