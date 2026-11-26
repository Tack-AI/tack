#![allow(clippy::unwrap_used)]

use std::path::Path;

use tack_agent_core::AgentMessage;
use tack_ai::{
    AssistantMessage, ContentBlock, InputContentBlock, Model, StopReason, ToolResultMessage,
    UserContent,
};
use tack_session::{
    ContextEditContent, ContextEditReplacement, SessionBackend, SessionEntry, SessionManager,
};

fn assistant(text: &str) -> AgentMessage {
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
    message.stop_reason = StopReason::Stop;
    AgentMessage::Assistant(message)
}

fn tool_result(text: &str) -> AgentMessage {
    AgentMessage::ToolResult(ToolResultMessage {
        tool_call_id: "call-1".into(),
        tool_name: "read".into(),
        content: vec![InputContentBlock::text(text)],
        details: None,
        usage: None,
        is_error: false,
        timestamp: 1,
    })
}

fn replacement(content: ContextEditContent) -> Option<ContextEditReplacement> {
    Some(ContextEditReplacement { content })
}

fn text(content: &str) -> Option<ContextEditReplacement> {
    replacement(ContextEditContent::Text(content.to_string()))
}

fn user_text(message: &AgentMessage) -> &str {
    let AgentMessage::User(user) = message else {
        panic!("expected user message: {message:?}");
    };
    let UserContent::Text(text) = &user.content else {
        panic!("expected user text: {message:?}");
    };
    text
}

#[test]
fn append_context_edit_emits_exact_json_and_preserves_user_strings() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut session = SessionManager::create_with_id_and_backend(
        &cwd,
        Some(tmp.path().join("sessions")),
        "context-edit-json".into(),
        SessionBackend::Jsonl,
    )
    .unwrap();
    let user_id = session
        .append_message(AgentMessage::user("original"))
        .unwrap();
    let user_edit = session
        .append_context_edit(&user_id, text("edited user"))
        .unwrap();
    let assistant_id = session.append_message(assistant("answer")).unwrap();
    let assistant_edit = session
        .append_context_edit(&assistant_id, text("edited assistant"))
        .unwrap();

    let entries = session.entries();
    let user_entry = entries
        .iter()
        .find(|entry| entry.id() == user_edit)
        .unwrap();
    assert_eq!(
        serde_json::to_string(user_entry).unwrap(),
        format!(
            "{{\"type\":\"context_edit\",\"id\":\"{user_edit}\",\"parentId\":\"{user_id}\",\"timestamp\":\"{}\",\"targetId\":\"{user_id}\",\"replacement\":{{\"content\":\"edited user\"}}}}",
            user_entry.timestamp()
        )
    );
    let assistant_entry = entries
        .iter()
        .find(|entry| entry.id() == assistant_edit)
        .unwrap();
    assert_eq!(
        serde_json::to_string(assistant_entry).unwrap(),
        format!(
            "{{\"type\":\"context_edit\",\"id\":\"{assistant_edit}\",\"parentId\":\"{assistant_id}\",\"timestamp\":\"{}\",\"targetId\":\"{assistant_id}\",\"replacement\":{{\"content\":[{{\"type\":\"text\",\"text\":\"edited assistant\"}}]}}}}",
            assistant_entry.timestamp()
        )
    );
    let raw = std::fs::read_to_string(session.session_file().unwrap()).unwrap();
    assert_eq!(
        raw.lines().last().unwrap(),
        serde_json::to_string(assistant_entry).unwrap()
    );
}

#[test]
fn append_context_edit_is_latest_wins_and_branch_relative() {
    let mut session = SessionManager::in_memory(Path::new("/work"));
    let target = session
        .append_message(AgentMessage::user("original"))
        .unwrap();
    let first = session.append_context_edit(&target, text("first")).unwrap();
    let omitted = session.append_context_edit(&target, None).unwrap();
    let restored = session
        .append_context_edit(&target, text("restored"))
        .unwrap();
    assert_eq!(
        user_text(&session.build_session_context().messages[0]),
        "restored"
    );
    let entries = session.entries();
    assert_eq!(
        entries
            .iter()
            .find(|e| e.id() == first)
            .unwrap()
            .parent_id(),
        Some(target.as_str())
    );
    assert_eq!(
        entries
            .iter()
            .find(|e| e.id() == omitted)
            .unwrap()
            .parent_id(),
        Some(first.as_str())
    );
    assert_eq!(
        entries
            .iter()
            .find(|e| e.id() == restored)
            .unwrap()
            .parent_id(),
        Some(omitted.as_str())
    );

    session.branch(&target).unwrap();
    assert_eq!(
        user_text(&session.build_session_context().messages[0]),
        "original"
    );
    assert!(matches!(
        session.append_context_edit(&restored, None),
        Err(tack_session::SessionError::EntryNotOnActiveBranch(_))
    ));
}

#[test]
fn invalid_context_edits_do_not_change_leaf_revision_or_file() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut session = SessionManager::create_with_id_and_backend(
        &cwd,
        Some(tmp.path().join("sessions")),
        "context-edit-invalid".into(),
        SessionBackend::Jsonl,
    )
    .unwrap();
    let root = session.append_message(AgentMessage::user("root")).unwrap();
    let off_branch = session.append_message(assistant("off branch")).unwrap();
    session.branch(&root).unwrap();
    let result_id = session.append_message(tool_result("result")).unwrap();
    let metadata_id = session.append_custom_entry("metadata", None).unwrap();
    let file = session.session_file().unwrap().to_path_buf();
    let before = std::fs::read(&file).unwrap();
    let leaf = session.leaf_id().unwrap().to_string();
    let revision = session.revision();
    let thinking = || {
        replacement(ContextEditContent::AssistantBlocks(vec![
            ContentBlock::Thinking {
                thinking: "not input content".into(),
                thinking_signature: None,
                redacted: None,
            },
        ]))
    };

    assert!(matches!(
        session.append_context_edit("missing", None),
        Err(tack_session::SessionError::EntryNotFound(_))
    ));
    assert!(matches!(
        session.append_context_edit(&off_branch, None),
        Err(tack_session::SessionError::EntryNotOnActiveBranch(_))
    ));
    assert!(session.append_context_edit(&metadata_id, None).is_err());
    assert!(session.append_context_edit(&root, thinking()).is_err());
    assert!(session.append_context_edit(&result_id, thinking()).is_err());
    assert_eq!(session.leaf_id(), Some(leaf.as_str()));
    assert_eq!(session.revision(), revision);
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

#[test]
fn context_edit_supports_imported_custom_message_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("custom.jsonl");
    std::fs::write(
        &path,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"custom\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
            "{\"type\":\"custom_message\",\"id\":\"c1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"customType\":\"test\",\"content\":\"original\",\"display\":false}\n"
        ),
    )
    .unwrap();
    let mut session =
        SessionManager::open_with_backend(&path, None, SessionBackend::Jsonl).unwrap();
    session.append_context_edit("c1", text("edited")).unwrap();
    let AgentMessage::Custom(custom) = &session.build_session_context().messages[0] else {
        panic!("expected custom projection");
    };
    assert_eq!(custom.content, UserContent::Text("edited".into()));
    let raw = std::fs::read_to_string(path).unwrap();
    assert!(
        raw.lines()
            .last()
            .unwrap()
            .contains("\"replacement\":{\"content\":\"edited\"}")
    );
}

#[test]
fn context_edit_string_normalization_survives_v4_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut session = SessionManager::create(&cwd, Some(tmp.path().join("sessions"))).unwrap();
    let assistant_id = session.append_message(assistant("original")).unwrap();
    let result_id = session
        .append_message(tool_result("original result"))
        .unwrap();
    session
        .append_context_edit(&assistant_id, text("assistant replacement"))
        .unwrap();
    session
        .append_context_edit(&result_id, text("result replacement"))
        .unwrap();
    let path = session.session_file().unwrap().to_path_buf();
    drop(session);

    let session = SessionManager::open(&path, None).unwrap();
    for entry in session
        .entries()
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::ContextEdit { .. }))
    {
        let SessionEntry::ContextEdit {
            replacement: Some(replacement),
            ..
        } = entry
        else {
            unreachable!()
        };
        assert!(matches!(
            replacement.content,
            ContextEditContent::InputBlocks(_) | ContextEditContent::AssistantBlocks(_)
        ));
    }
    let projected = session.build_session_context().messages;
    let AgentMessage::Assistant(assistant) = &projected[0] else {
        panic!("expected assistant")
    };
    assert_eq!(
        assistant.content,
        vec![ContentBlock::text("assistant replacement")]
    );
    let AgentMessage::ToolResult(result) = &projected[1] else {
        panic!("expected tool result")
    };
    assert_eq!(
        result.content,
        vec![InputContentBlock::text("result replacement")]
    );
}
