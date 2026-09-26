//! Session persistence and compaction tests.
#![allow(clippy::unwrap_used)]

use serde_json::json;
use tack_agent_core::AgentMessage;
use tack_session::*;

fn session_fixture() -> String {
    // Hand-crafted from docs/session-format.md examples (v3). Real session
    // files always end with a newline; the v4 reader treats a missing
    // final newline as a torn tail, so the fixture must include one.
    let mut fixture = [
        r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/tmp/project"}"#,
        r#"{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"Hello","timestamp":1733234401000}}"#,
        r#"{"type":"message","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi!"}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":1733234402000}}"#,
        r#"{"type":"model_change","id":"d4e5f607","parentId":"b2c3d4e5","timestamp":"2024-12-03T14:05:00.000Z","provider":"openai","modelId":"gpt-4o"}"#,
        r#"{"type":"custom","id":"h8i9j0k1","parentId":"d4e5f607","timestamp":"2024-12-03T14:20:00.000Z","customType":"my-extension","data":{"count":42}}"#,
        r#"{"type":"custom_message","id":"i9j0k1l2","parentId":"h8i9j0k1","timestamp":"2024-12-03T14:25:00.000Z","customType":"my-extension","content":"Injected context...","display":true}"#,
        r#"{"type":"some_future_extension","id":"zzz99999","parentId":"i9j0k1l2","timestamp":"2024-12-03T14:26:00.000Z","whatever":{"nested":true}}"#,
    ]
    .join("\n");
    fixture.push('\n');
    fixture
}

#[test]
fn parses_ts_pi_session_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(&path, session_fixture()).unwrap();

    let manager = SessionManager::open(&path, None).unwrap();
    assert_eq!(manager.session_id(), "sess-1");
    let entries = manager.entries();
    // 2 messages + model_change + custom + custom_message + the unknown
    // extension record (preserved through the v4 migration as a custom
    // entry — tack never silently drops payloads).
    assert_eq!(entries.len(), 6);
    assert!(
        entries
            .iter()
            .any(|e| matches!(e, SessionEntry::Custom { custom_type, .. } if custom_type == "some_future_extension")),
        "unknown extension entry preserved as custom: {entries:?}"
    );

    let context = manager.build_session_context();
    // user + assistant + custom_message (custom entries don't contribute).
    assert_eq!(context.messages.len(), 3);
    assert_eq!(context.thinking_level, "off");
    assert_eq!(
        context.model,
        Some(("openai".to_string(), "gpt-4o".to_string()))
    );
}

#[test]
fn unknown_entries_preserved_on_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let fixture = session_fixture();
    std::fs::write(&path, &fixture).unwrap();

    let mut manager = SessionManager::open(&path, None).unwrap();
    manager.append_message(AgentMessage::user("more")).unwrap();

    let rewritten = std::fs::read_to_string(&path).unwrap();
    assert!(rewritten.contains("some_future_extension"));
    assert!(rewritten.contains("whatever"));
}

#[test]
fn create_append_reopen_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut manager = SessionManager::create(&cwd, Some(session_dir.clone())).unwrap();
    assert!(manager.session_file().is_some());
    manager.append_message(AgentMessage::user("hello")).unwrap();
    manager.append_thinking_level_change("high").unwrap();
    let file = manager.session_file().unwrap().to_path_buf();
    drop(manager);

    let manager = SessionManager::open(&file, None).unwrap();
    let context = manager.build_session_context();
    assert_eq!(context.messages.len(), 1);
    assert_eq!(context.thinking_level, "high");
}

#[test]
fn continue_recent_picks_latest() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut first = SessionManager::create(&cwd, Some(session_dir.clone())).unwrap();
    first
        .append_message(AgentMessage::user("first session"))
        .unwrap();
    let first_id = first.session_id().to_string();
    drop(first);

    let manager = SessionManager::continue_recent(&cwd, Some(session_dir)).unwrap();
    assert_eq!(manager.session_id(), first_id);
}

#[test]
fn branch_moves_leaf() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::in_memory(dir.path());
    let first = manager.append_message(AgentMessage::user("one")).unwrap();
    manager.append_message(AgentMessage::user("two")).unwrap();
    manager.append_message(AgentMessage::user("three")).unwrap();

    assert_eq!(manager.build_session_path().len(), 3);
    manager.branch(&first).unwrap();
    assert_eq!(manager.build_session_path().len(), 1);
    manager
        .append_message(AgentMessage::user("two-alt"))
        .unwrap();
    let path = manager.build_session_path();
    assert_eq!(path.len(), 2);
}

fn make_message_entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
    SessionEntry::Message {
        id: id.to_string(),
        parent_id: parent.map(str::to_string),
        timestamp: "2024-12-03T14:00:00.000Z".to_string(),
        message: AgentMessage::user(text),
    }
}

fn make_assistant_entry(id: &str, parent: &str, text: &str) -> SessionEntry {
    let mut assistant = tack_ai::AssistantMessage::pending(&tack_ai::Model {
        id: "m".into(),
        name: "m".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "http://x".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    });
    assistant.content = vec![tack_ai::ContentBlock::text(text)];
    assistant.stop_reason = tack_ai::StopReason::Stop;
    SessionEntry::Message {
        id: id.to_string(),
        parent_id: Some(parent.to_string()),
        timestamp: "2024-12-03T14:00:01.000Z".to_string(),
        message: AgentMessage::Assistant(assistant),
    }
}

fn make_tool_result_entry(id: &str, parent: &str, text: &str) -> SessionEntry {
    SessionEntry::Message {
        id: id.to_string(),
        parent_id: Some(parent.to_string()),
        timestamp: "2024-12-03T14:00:02.000Z".to_string(),
        message: AgentMessage::ToolResult(tack_ai::ToolResultMessage {
            tool_call_id: "t".into(),
            tool_name: "read".into(),
            content: vec![tack_ai::InputContentBlock::text(text)],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        }),
    }
}

#[test]
fn cut_point_never_cuts_at_tool_result() {
    // user(100 chars) -> assistant+toolcall -> toolResult(big) -> user -> ...
    let entries = vec![
        make_message_entry("e1", None, &"x".repeat(400)),
        make_assistant_entry("e2", "e1", "call"),
        make_tool_result_entry("e3", "e2", &"y".repeat(400)),
        make_message_entry("e4", Some("e3"), &"z".repeat(400)),
    ];
    // Budget small enough that we must cut — but e3 is a tool result.
    let cut = find_cut_point(&entries, 0, entries.len(), 50, 0);
    let cut_entry = &entries[cut.first_kept_entry_index];
    // Must be e4 (user) or e2 (assistant), never e3 (toolResult).
    assert!(!matches!(
        cut_entry,
        SessionEntry::Message {
            message: AgentMessage::ToolResult(_),
            ..
        }
    ));
}

#[test]
fn cut_point_respects_budget() {
    let entries: Vec<SessionEntry> = (0..10)
        .map(|i| {
            let id = format!("e{i}");
            let parent = if i == 0 {
                None
            } else {
                Some(format!("e{}", i - 1))
            };
            make_message_entry(&id, parent.as_deref(), &"x".repeat(400)) // 100 tokens each
        })
        .collect();
    let cut = find_cut_point(&entries, 0, entries.len(), 300, 0);
    // ~3 messages kept (300 tokens / 100 each).
    assert!(cut.first_kept_entry_index >= 6, "cut: {cut:?}");
    assert!(!cut.is_split_turn);
}

#[test]
fn should_compact_threshold() {
    let settings = DEFAULT_COMPACTION_SETTINGS;
    assert!(should_compact(200_000 - 16_384 + 1, 200_000, &settings));
    assert!(!should_compact(1000, 200_000, &settings));
    assert!(!should_compact(
        1_000_000,
        200_000,
        &CompactionSettings {
            enabled: false,
            ..settings
        }
    ));
}

#[test]
fn estimate_tokens_chars_over_4() {
    let msg = AgentMessage::user("x".repeat(400));
    assert_eq!(estimate_tokens(&msg), 100);
}

#[test]
fn compaction_checkpoint_retained_tail() {
    // A compaction entry with retainedTail acts as a self-contained
    // checkpoint: entries before it (even firstKept ones) are excluded.
    let entries = vec![
        make_message_entry("e1", None, "old stuff"),
        SessionEntry::Compaction {
            id: "c1".to_string(),
            parent_id: Some("e1".to_string()),
            timestamp: "2024-12-03T15:00:00.000Z".to_string(),
            summary: "summary text".to_string(),
            first_kept_entry_id: None,
            tokens_before: 5000,
            retained_tail: Some(vec![AgentMessage::user("kept tail")]),
            details: None,
            usage: None,
            from_hook: None,
            system_message: None,
            first_kept_entry_index: None,
        },
        make_message_entry("e2", Some("c1"), "after compaction"),
    ];
    let context = build_session_context(&entries, None);
    // compactionSummary + retained tail user + after-compaction user.
    assert_eq!(context.messages.len(), 3);
    assert!(matches!(
        &context.messages[0],
        AgentMessage::CompactionSummary(_)
    ));
    let AgentMessage::User(tail) = &context.messages[1] else {
        panic!()
    };
    assert!(matches!(&tail.content, tack_ai::UserContent::Text(t) if t == "kept tail"));
}

#[test]
fn serialize_conversation_formats() {
    let messages = vec![
        tack_ai::Message::user("hello"),
        tack_ai::Message::ToolResult(tack_ai::ToolResultMessage {
            tool_call_id: "t".into(),
            tool_name: "bash".into(),
            content: vec![tack_ai::InputContentBlock::text("x".repeat(3000))],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        }),
    ];
    let out = serialize_conversation(&messages);
    assert!(out.contains("[User]: hello"));
    assert!(out.contains("more characters truncated"));
}

#[test]
fn entry_json_matches_doc_examples() {
    // Our serialization must produce the doc's field names.
    let entry = SessionEntry::ModelChange {
        id: "d4e5f607".into(),
        parent_id: Some("c3d4e5f6".into()),
        timestamp: "2024-12-03T14:05:00.000Z".into(),
        provider: "openai".into(),
        model_id: "gpt-4o".into(),
    };
    let value = serde_json::to_value(&entry).unwrap();
    assert_eq!(
        value,
        json!({
            "type": "model_change",
            "id": "d4e5f607",
            "parentId": "c3d4e5f6",
            "timestamp": "2024-12-03T14:05:00.000Z",
            "provider": "openai",
            "modelId": "gpt-4o"
        })
    );
}

#[test]
fn labels_set_get_clear() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::in_memory(dir.path());
    let first = manager.append_message(AgentMessage::user("one")).unwrap();
    manager.append_message(AgentMessage::user("two")).unwrap();

    assert_eq!(manager.get_label(&first), None);
    manager
        .append_label_change(&first, Some("checkpoint-1".into()))
        .unwrap();
    assert_eq!(manager.get_label(&first), Some("checkpoint-1".to_string()));
    manager.append_label_change(&first, None).unwrap();
    assert_eq!(manager.get_label(&first), None);
    assert!(
        manager
            .append_label_change("nonexistent", Some("x".into()))
            .is_err()
    );
}

#[test]
fn branch_with_summary_moves_leaf_and_records() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::in_memory(dir.path());
    let first = manager.append_message(AgentMessage::user("one")).unwrap();
    manager.append_message(AgentMessage::user("two")).unwrap();

    manager
        .branch_with_summary(Some(&first), "explored approach A", None, None)
        .unwrap();
    let context = manager.build_session_context();
    // user "one" + branchSummary message in context.
    assert_eq!(context.messages.len(), 2);
    assert!(
        matches!(&context.messages[1], AgentMessage::BranchSummary(b) if b.summary == "explored approach A")
    );
}

#[test]
fn session_totals_aggregate_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::in_memory(dir.path());
    manager.append_message(AgentMessage::user("q")).unwrap();
    let mut assistant = tack_ai::AssistantMessage::pending(&tack_ai::Model {
        id: "m".into(),
        name: "m".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "http://x".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    });
    assistant.stop_reason = tack_ai::StopReason::Stop;
    assistant.usage.input = 100;
    assistant.usage.output = 50;
    assistant.usage.total_tokens = 150;
    manager
        .append_message(AgentMessage::Assistant(assistant))
        .unwrap();

    let totals = manager.session_totals();
    assert_eq!(totals.input, 100);
    assert_eq!(totals.total_tokens, 150);
}

// ---------------------------------------------------------------------------
// Transcript system messages (upstream #9548)
// ---------------------------------------------------------------------------

fn make_system_entry(id: &str, parent: Option<&str>, content: &str) -> SessionEntry {
    SessionEntry::Message {
        id: id.to_string(),
        parent_id: parent.map(str::to_string),
        timestamp: "2024-12-03T13:00:00.000Z".to_string(),
        message: AgentMessage::System(tack_ai::SystemMessage {
            content: tack_ai::UserContent::Text(content.to_string()),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
        }),
    }
}

/// Upstream session-manager.ts: a compaction entry's systemMessage projects
/// into the context BEFORE the summary, and kept system messages after the
/// firstKept boundary are skipped (the recorded state replaces them).
#[test]
fn compaction_system_message_projects_and_kept_system_messages_are_skipped() {
    let recorded = tack_ai::SystemMessage {
        content: tack_ai::UserContent::Text("recorded prompt".to_string()),
        sections: None,
        tools_added: None,
        tools_removed: None,
        timestamp: 42,
    };
    let entries = vec![
        make_system_entry("s1", None, "old prompt"),
        make_message_entry("e1", Some("s1"), "old stuff"),
        make_system_entry("s2", Some("e1"), "kept-range prompt update"),
        SessionEntry::Compaction {
            id: "c1".to_string(),
            parent_id: Some("s2".to_string()),
            timestamp: "2024-12-03T15:00:00.000Z".to_string(),
            summary: "summary text".to_string(),
            first_kept_entry_id: Some("s2".to_string()),
            tokens_before: 5000,
            retained_tail: None,
            details: None,
            usage: None,
            from_hook: None,
            system_message: Some(recorded),
            first_kept_entry_index: None,
        },
        make_message_entry("e2", Some("c1"), "after compaction"),
    ];
    let context = build_session_context(&entries, None);
    // [recorded system, compactionSummary, after-compaction user] — the
    // kept-range system message s2 is skipped.
    assert_eq!(context.messages.len(), 3, "{:?}", context.messages);
    let AgentMessage::System(system) = &context.messages[0] else {
        panic!("expected recorded system message first")
    };
    assert_eq!(
        tack_ai::transcript::content_text(&system.content),
        "recorded prompt"
    );
    assert!(matches!(
        &context.messages[1],
        AgentMessage::CompactionSummary(_)
    ));
    assert!(matches!(&context.messages[2], AgentMessage::User(_)));
}

/// Upstream appendCompaction: the manager records the replayed prompt/tool
/// state at the compaction boundary.
#[test]
fn append_compaction_records_replayed_system_state() {
    let cwd = std::env::temp_dir().join(format!("tack-sysmsg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let session_dir = cwd.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut manager = SessionManager::create(&cwd, Some(session_dir.clone())).unwrap();
    manager
        .append_message(AgentMessage::System(tack_ai::SystemMessage {
            content: tack_ai::UserContent::Text(String::new()),
            sections: Some(
                [(
                    "system-prompt".to_string(),
                    Some("You are Tack.".to_string()),
                )]
                .into_iter()
                .collect(),
            ),
            tools_added: Some(vec![tack_ai::ToolDefinition {
                name: "read".to_string(),
                description: "read tool".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                defer_loading: false,
                constrained_sampling: None,
            }]),
            tools_removed: None,
            timestamp: 1,
        }))
        .unwrap();
    manager.append_message(AgentMessage::user("hello")).unwrap();
    manager
        .append_compaction("sum", None, 1000, None, None, None)
        .unwrap();

    let file = manager.session_file().unwrap().to_path_buf();
    drop(manager);
    let manager = SessionManager::open(&file, None).unwrap();
    let context = manager.build_session_context();
    // v4 shape: the summary leads; the recorded system message heads the
    // materialized retained tail (see compaction_tail_with_system).
    assert!(matches!(
        &context.messages[0],
        AgentMessage::CompactionSummary(_)
    ));
    let AgentMessage::System(system) = &context.messages[1] else {
        panic!(
            "expected recorded system message, got {:?}",
            context.messages[1]
        )
    };
    assert_eq!(
        tack_ai::transcript::get_system_message_text(system),
        "You are Tack."
    );
    assert_eq!(
        system.tools_added.as_ref().map(|t| t.len()),
        Some(1),
        "tool state recorded at the boundary"
    );
    // Replay over the whole context yields the recorded prompt.
    assert_eq!(
        tack_ai::transcript::get_current_system_prompt(&context.messages),
        "You are Tack."
    );
    std::fs::remove_dir_all(&cwd).ok();
}

/// The v3 compaction `systemMessage` field round-trips through the legacy
/// v3 backend WITH the `role: "system"` discriminator, matching upstream's
/// session-manager.ts output byte-for-byte.
#[test]
fn v3_compaction_system_message_file_round_trip() {
    let cwd = std::env::temp_dir().join(format!("tack-v3sysmsg-{}", uuid::Uuid::new_v4()));
    let session_dir = cwd.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut manager = SessionManager::create_with_backend(
        &cwd,
        Some(session_dir.clone()),
        tack_session::SessionBackend::Jsonl,
    )
    .unwrap();
    manager
        .append_message(AgentMessage::System(tack_ai::SystemMessage {
            content: tack_ai::UserContent::Text(String::new()),
            sections: Some(
                [(
                    "system-prompt".to_string(),
                    Some("You are Tack.".to_string()),
                )]
                .into_iter()
                .collect(),
            ),
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
        }))
        .unwrap();
    manager.append_message(AgentMessage::user("hello")).unwrap();
    manager
        .append_compaction("sum", None, 1000, None, None, None)
        .unwrap();
    let file = manager.session_file().unwrap().to_path_buf();
    drop(manager);

    // The file holds a v3 compaction entry whose systemMessage carries
    // role: "system" and the replayed sections.
    let content = std::fs::read_to_string(&file).unwrap();
    let compaction_line = content
        .lines()
        .find(|l| l.contains(r#""type":"compaction""#))
        .expect("compaction line");
    let value: serde_json::Value = serde_json::from_str(compaction_line).unwrap();
    let system_message = &value["systemMessage"];
    assert_eq!(system_message["role"], "system");
    assert_eq!(system_message["sections"]["system-prompt"], "You are Tack.");

    // Legacy v3 reopen: context projects systemMessage before the summary.
    let manager = SessionManager::open_with_backend(
        &file,
        Some(session_dir.clone()),
        tack_session::SessionBackend::Jsonl,
    )
    .unwrap();
    let context = manager.build_session_context();
    assert!(
        matches!(&context.messages[0], AgentMessage::System(_)),
        "systemMessage projects first: {:?}",
        context.messages[0]
    );
    assert_eq!(
        tack_ai::transcript::get_current_system_prompt(&context.messages),
        "You are Tack."
    );
    std::fs::remove_dir_all(&cwd).ok();
}

// ---------------------------------------------------------------------------
// Dangling tool-call repair (interrupted runs)
// ---------------------------------------------------------------------------

/// A run killed mid-tool-execution leaves the assistant's tool calls
/// unanswered; opening for continuation repairs them with synthetic error
/// results (idempotent, ordered after the assistant message).
#[test]
fn repair_dangling_tool_calls_appends_missing_results() {
    let cwd = std::env::temp_dir().join(format!("tack-repair-{}", uuid::Uuid::new_v4()));
    let session_dir = cwd.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut manager = SessionManager::create(&cwd, Some(session_dir.clone())).unwrap();
    manager.append_message(AgentMessage::user("go")).unwrap();
    let mut assistant = tack_ai::AssistantMessage::pending(&tack_ai::Model {
        id: "m".into(),
        name: "m".into(),
        api: "a".into(),
        provider: "p".into(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![tack_ai::InputKind::Text],
        cost: Default::default(),
        context_window: 200_000,
        max_tokens: 8192,
        sampling_params: None,
        headers: None,
        compat: None,
    });
    assistant.stop_reason = tack_ai::StopReason::ToolUse;
    assistant.content = vec![
        tack_ai::ContentBlock::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
            thought_signature: None,
            namespace: None,
        },
        tack_ai::ContentBlock::ToolCall {
            id: "c2".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
            thought_signature: None,
            namespace: None,
        },
    ];
    manager
        .append_message(AgentMessage::Assistant(assistant))
        .unwrap();
    // One of the two results was recorded before the kill; the other dangles.
    manager
        .append_message(AgentMessage::ToolResult(tack_ai::ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "bash".into(),
            content: vec![tack_ai::InputContentBlock::text("done")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 1,
        }))
        .unwrap();

    // Only c2 is repaired; c1 is untouched. Then idempotent.
    assert_eq!(manager.repair_dangling_tool_calls().unwrap(), 1);
    assert_eq!(manager.repair_dangling_tool_calls().unwrap(), 0);

    let messages = manager.build_session_context().messages;
    let AgentMessage::ToolResult(result) = messages.last().unwrap() else {
        panic!("expected tool result, got {:?}", messages.last())
    };
    assert_eq!(result.tool_call_id, "c2");
    assert_eq!(result.tool_name, "read");
    assert!(result.is_error, "repaired result must be an error");
    assert_eq!(result.details.as_ref().unwrap()["repaired"], true);
    let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
        panic!()
    };
    assert!(text.contains("interrupted"), "{text}");

    std::fs::remove_dir_all(&cwd).ok();
}

/// A session whose final turn has no tool calls (or all answered) is a
/// no-op.
#[test]
fn repair_dangling_tool_calls_noop_when_complete() {
    let cwd = std::env::temp_dir().join(format!("tack-repair-{}", uuid::Uuid::new_v4()));
    let session_dir = cwd.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut manager = SessionManager::create(&cwd, Some(session_dir.clone())).unwrap();
    manager.append_message(AgentMessage::user("hi")).unwrap();
    assert_eq!(manager.repair_dangling_tool_calls().unwrap(), 0);
    std::fs::remove_dir_all(&cwd).ok();
}
