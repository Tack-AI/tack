//! Context building from session entries. Port of the tree walk + compaction
//! handling in `session-manager.ts` (`buildSessionPath`, `buildContextEntries`,
//! `buildSessionContext`, `sessionEntryToContextMessages`).

use std::collections::HashMap;

use tack_agent_core::{
    AgentMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    CustomAgentMessage,
};
use tack_ai::{ContentBlock, InputContentBlock, UserContent};

use crate::entry::{ContextEditContent, SessionEntry};

/// Walk from the leaf to the root, producing the active path.
/// `leaf_id`: `Some(id)` walks from that entry; `None` uses the last entry.
/// `Some(None)` semantics in TS (leafId === null → empty path) are handled by
/// the caller via `branch()`; here `None` means "latest".
pub fn build_session_path(entries: &[SessionEntry], leaf_id: Option<&str>) -> Vec<SessionEntry> {
    let by_id: HashMap<&str, &SessionEntry> = entries.iter().map(|e| (e.id(), e)).collect();

    let leaf = match leaf_id {
        Some(id) => by_id.get(id).copied().or_else(|| entries.last()),
        None => entries.last(),
    };
    let Some(leaf) = leaf else { return Vec::new() };

    let mut path = vec![leaf.clone()];
    let mut current = leaf;
    // Cycle-safe: a corrupt session file with a parentId loop must
    // terminate (walked ids are unique on any well-formed path).
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::from([current.id()]);
    while let Some(parent_id) = current.parent_id() {
        if !seen.insert(parent_id) {
            break;
        }
        match by_id.get(parent_id) {
            Some(parent) => {
                path.push((*parent).clone());
                current = parent;
            }
            None => break,
        }
    }
    path.reverse();
    path
}

/// Project one entry into context messages (pi's sessionEntryToContextMessages).
pub fn session_entry_to_context_messages(entry: &SessionEntry) -> Vec<AgentMessage> {
    match entry {
        SessionEntry::Message { message, .. } => vec![message.clone()],
        SessionEntry::CustomMessage {
            custom_type,
            content,
            display,
            details,
            timestamp,
            ..
        } => {
            vec![AgentMessage::Custom(CustomAgentMessage {
                custom_type: custom_type.clone(),
                content: content.clone(),
                display: *display,
                details: details.clone(),
                // Entry timestamps are ISO strings; message timestamps are
                // epoch millis. Session reloads in TS pi keep the entry's
                // ISO string here via createCustomMessage(entry.timestamp);
                // we parse to millis and fall back to 0.
                timestamp: iso_to_millis(timestamp).unwrap_or(0),
            })]
        }
        SessionEntry::BranchSummary {
            summary,
            from_id,
            timestamp,
            ..
        } => {
            vec![AgentMessage::BranchSummary(BranchSummaryMessage {
                summary: summary.clone(),
                // The v3 entry-level "root" sentinel maps to a null fromId
                // (v4/upstream wire shape).
                from_id: (from_id != "root").then(|| from_id.clone()),
                timestamp: iso_to_millis(timestamp).unwrap_or(0),
            })]
        }
        SessionEntry::Compaction {
            summary,
            tokens_before,
            timestamp,
            retained_tail,
            system_message,
            ..
        } => {
            // Upstream #9548: the recorded prompt/tool state leads, then
            // the summary (`entry.systemMessage ? [entry.systemMessage,
            // summary] : [summary]`).
            let mut messages: Vec<AgentMessage> = system_message
                .clone()
                .map(AgentMessage::System)
                .into_iter()
                .collect();
            messages.push(AgentMessage::CompactionSummary(CompactionSummaryMessage {
                summary: summary.clone(),
                tokens_before: *tokens_before,
                timestamp: iso_to_millis(timestamp).unwrap_or(0),
            }));
            // Self-contained checkpoint: retained tail follows the summary.
            if let Some(tail) = retained_tail {
                messages.extend(tail.iter().cloned());
            }
            messages
        }
        _ => Vec::new(),
    }
}

/// Build the active, compaction-aware entry list (pi's buildContextEntries).
///
/// With a compaction on the path: the compaction entry first; then, when the
/// compaction has a `retainedTail` checkpoint, only entries *after* the
/// compaction; otherwise kept entries from `firstKeptEntryId` up to the
/// compaction plus entries after it.
pub fn build_context_entries(entries: &[SessionEntry], leaf_id: Option<&str>) -> Vec<SessionEntry> {
    let path = build_session_path(entries, leaf_id);
    let compaction = path
        .iter()
        .rev()
        .find(|e| matches!(e, SessionEntry::Compaction { .. }));

    let Some(SessionEntry::Compaction {
        id: compaction_id,
        first_kept_entry_id,
        retained_tail,
        ..
    }) = compaction
    else {
        return path;
    };

    let compaction_idx = match path.iter().position(|e| e.id() == compaction_id) {
        Some(i) => i,
        None => return path,
    };

    let mut context_entries = vec![path[compaction_idx].clone()];

    if retained_tail.is_some() {
        // Checkpoint form: retained tail is materialized on the entry itself.
        context_entries.extend(path[compaction_idx + 1..].iter().cloned());
        return context_entries;
    }

    let mut found_first_kept = false;
    for entry in &path[..compaction_idx] {
        if Some(entry.id()) == first_kept_entry_id.as_deref() {
            found_first_kept = true;
        }
        // Upstream #9548: kept system messages are skipped — the
        // compaction entry's systemMessage replaces them.
        if found_first_kept
            && !matches!(entry, SessionEntry::Message { message, .. } if matches!(message, AgentMessage::System(_)))
        {
            context_entries.push(entry.clone());
        }
    }
    context_entries.extend(path[compaction_idx + 1..].iter().cloned());
    context_entries
}

fn input_to_assistant_block(block: &InputContentBlock) -> ContentBlock {
    match block {
        InputContentBlock::Text {
            text,
            text_signature,
        } => ContentBlock::Text {
            text: text.clone(),
            text_signature: text_signature.clone(),
        },
        InputContentBlock::Image { data, mime_type } => ContentBlock::Image {
            data: data.clone(),
            mime_type: mime_type.clone(),
        },
    }
}

fn assistant_to_input_block(block: &ContentBlock) -> Option<InputContentBlock> {
    match block {
        ContentBlock::Text {
            text,
            text_signature,
        } => Some(InputContentBlock::Text {
            text: text.clone(),
            text_signature: text_signature.clone(),
        }),
        ContentBlock::Image { data, mime_type } => Some(InputContentBlock::Image {
            data: data.clone(),
            mime_type: mime_type.clone(),
        }),
        ContentBlock::Thinking { .. } | ContentBlock::ToolCall { .. } => None,
    }
}

fn replacement_user_content(replacement: &ContextEditContent) -> Option<UserContent> {
    match replacement {
        ContextEditContent::Text(text) => Some(UserContent::Text(text.clone())),
        ContextEditContent::InputBlocks(blocks) => Some(UserContent::Blocks(blocks.clone())),
        ContextEditContent::AssistantBlocks(blocks) => Some(UserContent::Blocks(
            blocks
                .iter()
                .map(assistant_to_input_block)
                .collect::<Option<Vec<_>>>()?,
        )),
    }
}

fn replacement_assistant_content(replacement: &ContextEditContent) -> Vec<ContentBlock> {
    match replacement {
        ContextEditContent::Text(text) => vec![ContentBlock::text(text.clone())],
        ContextEditContent::InputBlocks(blocks) => {
            blocks.iter().map(input_to_assistant_block).collect()
        }
        ContextEditContent::AssistantBlocks(blocks) => blocks.clone(),
    }
}

fn replacement_tool_result_content(
    replacement: &ContextEditContent,
) -> Option<Vec<InputContentBlock>> {
    match replacement {
        ContextEditContent::Text(text) => Some(vec![InputContentBlock::text(text.clone())]),
        ContextEditContent::InputBlocks(blocks) => Some(blocks.clone()),
        ContextEditContent::AssistantBlocks(blocks) => blocks
            .iter()
            .map(assistant_to_input_block)
            .collect::<Option<Vec<_>>>(),
    }
}

/// Apply a replacement to one projected message while retaining role and
/// metadata. Role-incompatible imported blocks leave the message unchanged
/// rather than producing a provider-invalid projection.
fn apply_context_edit_to_message(
    message: &AgentMessage,
    replacement: &ContextEditContent,
) -> AgentMessage {
    let mut projected = message.clone();
    match &mut projected {
        AgentMessage::User(user) => {
            if let Some(content) = replacement_user_content(replacement) {
                user.content = content;
            }
        }
        AgentMessage::Assistant(assistant) => {
            assistant.content = replacement_assistant_content(replacement);
        }
        AgentMessage::ToolResult(result) => {
            if let Some(content) = replacement_tool_result_content(replacement) {
                result.content = content;
            }
        }
        AgentMessage::Custom(custom) => {
            if let Some(content) = replacement_user_content(replacement) {
                custom.content = content;
            }
        }
        // Context edits are only defined for user, assistant, tool-result,
        // and custom-message targets. Other projected roles stay unchanged.
        _ => {}
    }
    projected
}

fn edit_target_and_replacement(
    entry: &SessionEntry,
) -> Option<(&str, Option<&ContextEditContent>)> {
    let SessionEntry::ContextEdit {
        target_id,
        replacement,
        ..
    } = entry
    else {
        return None;
    };
    Some((
        target_id.as_str(),
        replacement.as_ref().map(|replacement| &replacement.content),
    ))
}

/// Project one entry under the latest edit targeting that entry (pi's
/// `projectContextEntry`). A null replacement omits the contribution; a
/// content replacement changes only message content, never role/metadata.
pub fn project_context_entry(
    entry: &SessionEntry,
    edit: Option<&SessionEntry>,
) -> Vec<AgentMessage> {
    let messages = session_entry_to_context_messages(entry);
    let Some((_, replacement)) = edit.and_then(edit_target_and_replacement) else {
        return messages;
    };
    let Some(replacement) = replacement else {
        return Vec::new();
    };
    messages
        .iter()
        .map(|message| apply_context_edit_to_message(message, replacement))
        .collect()
}

fn retained_tail_source_ids(entry: &SessionEntry) -> Option<Vec<String>> {
    let SessionEntry::Compaction {
        details: Some(details),
        ..
    } = entry
    else {
        return None;
    };
    Some(
        details
            .get("tackRetainedTailSourceIds")
            .and_then(serde_json::Value::as_array)?
            .iter()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect(),
    )
}

/// Project one entry under all edits selected on the active branch. For a
/// migrated v4 checkpoint compaction, edits can also target the physical
/// pre-compaction entries that produced each materialized tail message; the
/// migration records those source ids in implementation-specific details.
fn project_context_entry_with_edits(
    entry: &SessionEntry,
    edits: &HashMap<String, &SessionEntry>,
) -> Vec<AgentMessage> {
    if let SessionEntry::Compaction {
        retained_tail: Some(retained_tail),
        ..
    } = entry
        && let Some(source_ids) = retained_tail_source_ids(entry)
        && !source_ids.is_empty()
    {
        let mut messages = session_entry_to_context_messages(entry);
        let retained_start = messages.len().saturating_sub(retained_tail.len());
        let source_offset = retained_tail.len().saturating_sub(source_ids.len());
        for (source_index, source_id) in source_ids.iter().enumerate() {
            let retained_index = source_offset + source_index;
            let message_index = retained_start + retained_index;
            let Some(edit) = edits.get(source_id) else {
                continue;
            };
            let Some((_, replacement)) = edit_target_and_replacement(edit) else {
                continue;
            };
            match replacement {
                None => {
                    if message_index < messages.len() {
                        messages.remove(message_index);
                    }
                }
                Some(replacement) if message_index < messages.len() => {
                    messages[message_index] =
                        apply_context_edit_to_message(&messages[message_index], replacement);
                }
                _ => {}
            }
        }
        return messages;
    }

    project_context_entry(entry, edits.get(entry.id()).copied())
}

#[derive(Clone, Debug)]
pub struct ProjectedSessionEntry {
    /// Raw append-only entry that owns this projected contribution.
    pub source_entry: SessionEntry,
    /// Model-visible messages after context edits; empty for metadata and omissions.
    pub messages: Vec<AgentMessage>,
}

#[derive(Debug)]
pub struct SessionProjection {
    pub entries: Vec<ProjectedSessionEntry>,
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<(String, String)>,
}

fn session_settings(path: &[SessionEntry]) -> (String, Option<(String, String)>) {
    let mut thinking_level = "off".to_string();
    let mut model: Option<(String, String)> = None;
    for entry in path {
        match entry {
            SessionEntry::ThinkingLevelChange {
                thinking_level: level,
                ..
            } => {
                thinking_level = level.clone();
            }
            SessionEntry::ModelChange {
                provider, model_id, ..
            } => {
                model = Some((provider.clone(), model_id.clone()));
            }
            SessionEntry::Message {
                message: AgentMessage::Assistant(a),
                ..
            } => {
                model = Some((a.provider.clone(), a.model.clone()));
            }
            _ => {}
        }
    }
    (thinking_level, model)
}

/// Build the provenance-preserving, compaction-aware model projection.
pub fn build_session_projection(
    entries: &[SessionEntry],
    leaf_id: Option<&str>,
) -> SessionProjection {
    let path = build_session_path(entries, leaf_id);
    let (thinking_level, model) = session_settings(&path);
    let context_entries = build_context_entries(entries, leaf_id);
    let mut edits: HashMap<String, &SessionEntry> = HashMap::new();
    for entry in &context_entries {
        if let SessionEntry::ContextEdit { target_id, .. } = entry {
            // Iteration is oldest to newest, so the latest edit wins.
            edits.insert(target_id.clone(), entry);
        }
    }
    let projected_entries: Vec<ProjectedSessionEntry> = context_entries
        .iter()
        .enumerate()
        .map(|(index, source_entry)| ProjectedSessionEntry {
            source_entry: source_entry.clone(),
            // Older compactions retained inside a newer compaction's raw
            // range do not contribute a second checkpoint.
            messages: if matches!(source_entry, SessionEntry::Compaction { .. }) && index > 0 {
                Vec::new()
            } else {
                project_context_entry_with_edits(source_entry, &edits)
            },
        })
        .collect();
    let messages = projected_entries
        .iter()
        .flat_map(|entry| entry.messages.iter().cloned())
        .collect();
    SessionProjection {
        entries: projected_entries,
        messages,
        thinking_level,
        model,
    }
}

#[derive(Debug)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<(String, String)>, // (provider, model_id)
}

/// Build the LLM-facing session context (pi's buildSessionContext).
pub fn build_session_context(entries: &[SessionEntry], leaf_id: Option<&str>) -> SessionContext {
    let projection = build_session_projection(entries, leaf_id);
    SessionContext {
        messages: projection.messages,
        thinking_level: projection.thinking_level,
        model: projection.model,
    }
}

/// ISO 8601 → epoch millis (message timestamps). Returns None on failure.
pub fn iso_to_millis(iso: &str) -> Option<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    Some(dt.timestamp_millis() as u64)
}

/// Epoch millis → ISO 8601 with millisecond precision (inverse of
/// [`iso_to_millis`] for values written by [`now_iso`]; matches JS
/// `Date.toISOString()`).
pub fn millis_to_iso(millis: u64) -> String {
    chrono::DateTime::from_timestamp_millis(millis as i64)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// Current time as ISO 8601 with millisecond precision (matches JS
/// `Date.toISOString()`).
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Short entry id: first 8 chars of a UUID v4 (matches TS generateId).
pub fn generate_id(existing: &dyn Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let id: String = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        if !existing(&id) {
            return id;
        }
    }
    uuid::Uuid::new_v4().to_string()
}

pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Reference to bash execution message creation used by apps (`!` commands).
pub fn bash_execution_message(command: &str, output: &str, exit_code: Option<i32>) -> AgentMessage {
    AgentMessage::BashExecution(BashExecutionMessage {
        command: command.to_string(),
        output: output.to_string(),
        exit_code,
        cancelled: false,
        truncated: false,
        full_output_path: None,
        exclude_from_context: None,
        timestamp: tack_ai::now_millis(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn msg_entry(id: &str, parent: Option<&str>) -> SessionEntry {
        SessionEntry::Message {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            message: AgentMessage::user(format!("msg {id}")),
        }
    }

    fn edit_entry(
        id: &str,
        parent: Option<&str>,
        target: &str,
        replacement: Option<ContextEditContent>,
    ) -> SessionEntry {
        SessionEntry::ContextEdit {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-01-01T00:00:01.000Z".to_string(),
            target_id: target.to_string(),
            replacement: replacement.map(|content| crate::ContextEditReplacement { content }),
        }
    }

    fn user_text(message: &AgentMessage) -> &str {
        let AgentMessage::User(user) = message else {
            panic!("expected user message: {message:?}");
        };
        let UserContent::Text(text) = &user.content else {
            panic!("expected text content");
        };
        text
    }

    #[test]
    fn context_edits_latest_wins_and_stay_branch_relative() {
        let entries = vec![
            msg_entry("target", None),
            edit_entry(
                "edit-1",
                Some("target"),
                "target",
                Some(ContextEditContent::Text("edited".to_string())),
            ),
            edit_entry("edit-2", Some("edit-1"), "target", None),
            edit_entry(
                "edit-3",
                Some("edit-2"),
                "target",
                Some(ContextEditContent::Text("restored".to_string())),
            ),
        ];

        let projected = build_session_projection(&entries, None);
        assert_eq!(projected.messages.len(), 1);
        assert_eq!(user_text(&projected.messages[0]), "restored");

        // Walking from before the edits reveals the raw contribution again.
        let original = build_session_projection(&entries, Some("target"));
        assert_eq!(user_text(&original.messages[0]), "msg target");
        // Raw history is untouched.
        let SessionEntry::Message { message, .. } = &entries[0] else {
            panic!("expected message");
        };
        assert_eq!(user_text(message), "msg target");
    }

    #[test]
    fn context_edit_omits_only_the_model_projection() {
        let entries = vec![
            msg_entry("target", None),
            msg_entry("other", Some("target")),
            edit_entry("edit", Some("other"), "target", None),
        ];
        let projected = build_session_projection(&entries, None);
        assert_eq!(projected.messages.len(), 1);
        assert_eq!(user_text(&projected.messages[0]), "msg other");
        assert_eq!(projected.entries.len(), 3, "edit entry keeps provenance");
        assert!(projected.entries[0].messages.is_empty());
    }

    #[test]
    fn string_replacements_normalize_for_array_only_roles() {
        let assistant_entry: SessionEntry = serde_json::from_value(serde_json::json!({
            "type": "message",
            "id": "assistant",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "original"}],
                "api": "test",
                "provider": "test",
                "model": "test",
                "usage": {
                    "input": 1,
                    "output": 1,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": 2,
                    "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}
                },
                "stopReason": "stop",
                "timestamp": 1
            }
        }))
        .unwrap();
        let result_entry: SessionEntry = serde_json::from_value(serde_json::json!({
            "type": "message",
            "id": "result",
            "parentId": "assistant",
            "timestamp": "2026-01-01T00:00:01.000Z",
            "message": {
                "role": "toolResult",
                "toolCallId": "call-1",
                "toolName": "read",
                "content": [{"type": "text", "text": "original result"}],
                "isError": false,
                "timestamp": 2
            }
        }))
        .unwrap();
        let entries = vec![
            assistant_entry,
            result_entry,
            edit_entry(
                "edit-a",
                Some("result"),
                "assistant",
                Some(ContextEditContent::Text(
                    "assistant replacement".to_string(),
                )),
            ),
            edit_entry(
                "edit-r",
                Some("edit-a"),
                "result",
                Some(ContextEditContent::Text("result replacement".to_string())),
            ),
        ];

        let projected = build_session_projection(&entries, None);
        assert!(matches!(
            &projected.messages[0],
            AgentMessage::Assistant(assistant)
                if assistant.content == vec![ContentBlock::text("assistant replacement")]
        ));
        assert!(matches!(
            &projected.messages[1],
            AgentMessage::ToolResult(result)
                if result.content == vec![InputContentBlock::text("result replacement")]
        ));
    }

    #[test]
    fn context_edit_json_shape_matches_upstream() {
        let parsed: SessionEntry = serde_json::from_value(serde_json::json!({
            "type": "context_edit",
            "id": "g6h7i8j9",
            "parentId": "f6g7h8i9",
            "timestamp": "2024-12-03T14:11:00.000Z",
            "targetId": "c3d4e5f6",
            "replacement": null
        }))
        .unwrap();
        assert!(matches!(
            parsed,
            SessionEntry::ContextEdit {
                replacement: None,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::json!({
                "type": "context_edit",
                "id": "g6h7i8j9",
                "parentId": "f6g7h8i9",
                "timestamp": "2024-12-03T14:11:00.000Z",
                "targetId": "c3d4e5f6",
                "replacement": null
            })
        );
    }

    #[test]
    fn path_walks_leaf_to_root() {
        let entries = vec![
            msg_entry("a", None),
            msg_entry("b", Some("a")),
            msg_entry("c", Some("b")),
        ];
        let path = build_session_path(&entries, Some("c"));
        assert_eq!(
            path.iter().map(SessionEntry::id).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        // Branching: walking from b excludes c.
        let path = build_session_path(&entries, Some("b"));
        assert_eq!(
            path.iter().map(SessionEntry::id).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    /// Regression: a parentId cycle in a corrupt file used to hang the
    /// walk forever.
    #[test]
    fn parent_cycle_terminates() {
        let entries = vec![msg_entry("a", Some("b")), msg_entry("b", Some("a"))];
        let path = build_session_path(&entries, Some("a"));
        assert_eq!(path.len(), 2, "cycle broken: {path:?}");
    }
}
