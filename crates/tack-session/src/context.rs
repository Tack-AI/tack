//! Context building from session entries. Port of the tree walk + compaction
//! handling in `session-manager.ts` (`buildSessionPath`, `buildContextEntries`,
//! `buildSessionContext`, `sessionEntryToContextMessages`).

use std::collections::HashMap;

use tack_agent_core::{
    AgentMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    CustomAgentMessage,
};

use crate::entry::SessionEntry;

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
                from_id: from_id.clone(),
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

#[derive(Debug)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<(String, String)>, // (provider, model_id)
}

/// Build the LLM-facing session context (pi's buildSessionContext).
pub fn build_session_context(entries: &[SessionEntry], leaf_id: Option<&str>) -> SessionContext {
    let path = build_session_path(entries, leaf_id);

    let mut thinking_level = "off".to_string();
    let mut model: Option<(String, String)> = None;
    for entry in &path {
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

    let messages = build_context_entries(entries, leaf_id)
        .iter()
        .flat_map(session_entry_to_context_messages)
        .collect();

    SessionContext {
        messages,
        thinking_level,
        model,
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
