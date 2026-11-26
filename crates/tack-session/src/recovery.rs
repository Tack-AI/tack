//! Durable omission of a failed assistant attempt from model context.

use std::collections::HashSet;

use tack_agent_core::AgentMessage;

use crate::context::{
    build_session_path, build_session_projection, is_valid_context_edit,
    session_projection_message_slots,
};
use crate::{SessionEntry, SessionError, SessionManager};

fn invalid_recovery_omission(message: impl Into<String>) -> SessionError {
    SessionError::InvalidRecoveryOmission(message.into())
}

fn active_target<'a>(
    active_path: &'a [SessionEntry],
    target_id: &str,
) -> Result<&'a SessionEntry, SessionError> {
    active_path
        .iter()
        .find(|entry| entry.id() == target_id)
        .ok_or_else(|| {
            invalid_recovery_omission(format!(
                "source entry {target_id} is not on the active session path"
            ))
        })
}

fn validate_target(
    active_path: &[SessionEntry],
    target_id: &str,
    expected_role: &str,
) -> Result<(), SessionError> {
    let target = active_target(active_path, target_id)?;
    let expected_role_matches = matches!(
        (expected_role, target),
        (
            "assistant",
            SessionEntry::Message {
                message: AgentMessage::Assistant(_),
                ..
            },
        ) | (
            "tool result",
            SessionEntry::Message {
                message: AgentMessage::ToolResult(_),
                ..
            },
        )
    );
    if !expected_role_matches || !is_valid_context_edit(target, None) {
        return Err(invalid_recovery_omission(format!(
            "source entry {target_id} is not a legal {expected_role} omission target"
        )));
    }
    Ok(())
}

/// Persistently omit a failed assistant attempt and its matching tool results.
///
/// `assistant_source_entry_id` is the durable id carried by
/// [`crate::ProjectedSessionEntry::source_entry`], not a message index or a
/// structural message fingerprint. The assistant is resolved in the current
/// compaction-aware projection, including when its visible content came from
/// a replacement edit. Matching results are collected in projection order
/// after that assistant and before the next visible assistant.
///
/// Every selected target is validated before the first write, then omitted by
/// [`SessionManager::append_context_edit`] with `replacement: null`. Raw
/// history is never changed. The returned ids are the appended context-edit
/// entry ids, in assistant/result projection order. Repeating the operation is
/// idempotent for already omitted targets and can still clean up a matching
/// result left behind by an earlier assistant-only omission.
///
/// Materialized compaction-tail messages need their recorded source ids. If
/// provenance is absent, or any selected target is invalid, this returns an
/// error rather than guessing from structurally equal messages. This helper
/// deliberately performs no compaction, retry, event emission, or mutation of
/// an agent loop's in-memory message state; its caller owns those concerns.
pub fn omit_recovery_attempt(
    session: &mut SessionManager,
    assistant_source_entry_id: &str,
) -> Result<Vec<String>, SessionError> {
    let entries = session.entries();
    let leaf_id = session.leaf_id();
    let active_path = build_session_path(&entries, leaf_id);

    // Reject an existing non-assistant id explicitly. Otherwise the source
    // must resolve to exactly one provenance-bearing slot in the projection;
    // raw existence alone is insufficient (it may be compacted away or on
    // another branch).
    if active_path
        .iter()
        .any(|entry| entry.id() == assistant_source_entry_id)
    {
        validate_target(&active_path, assistant_source_entry_id, "assistant")?;
    }

    let projection = build_session_projection(&entries, leaf_id);
    let slots = session_projection_message_slots(&projection);
    let matching_slots = slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.source_entry_id.as_deref() == Some(assistant_source_entry_id))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let assistant_index = match matching_slots.as_slice() {
        [index] => *index,
        [] => {
            return Err(invalid_recovery_omission(format!(
                "cannot resolve assistant source entry {assistant_source_entry_id} in the current projection"
            )));
        }
        _ => {
            return Err(invalid_recovery_omission(format!(
                "assistant source entry {assistant_source_entry_id} is ambiguous in the current projection"
            )));
        }
    };

    let raw_assistant = match active_target(&active_path, assistant_source_entry_id)? {
        SessionEntry::Message {
            message: AgentMessage::Assistant(assistant),
            ..
        } => assistant,
        _ => {
            return Err(invalid_recovery_omission(format!(
                "source entry {assistant_source_entry_id} is not an assistant message"
            )));
        }
    };
    let assistant_slot = &slots[assistant_index];
    let projected_assistant = match &assistant_slot.message {
        Some(AgentMessage::Assistant(assistant)) => Some(assistant),
        None => None, // Already omitted; retain the slot for idempotent repair.
        Some(_) => {
            return Err(invalid_recovery_omission(format!(
                "projected source entry {assistant_source_entry_id} is not an assistant message"
            )));
        }
    };

    // Usually these sets are identical. Including both prevents a content
    // replacement from orphaning results for calls on the raw failed attempt,
    // while also covering calls introduced by an imported replacement.
    let mut tool_call_ids = raw_assistant
        .tool_calls()
        .map(|(id, _, _)| id.to_string())
        .collect::<HashSet<_>>();
    if let Some(assistant) = projected_assistant {
        tool_call_ids.extend(assistant.tool_calls().map(|(id, _, _)| id.to_string()));
    }

    let mut target_ids = Vec::new();
    if projected_assistant.is_some() {
        target_ids.push(assistant_source_entry_id.to_string());
    }
    for slot in &slots[assistant_index + 1..] {
        let Some(message) = &slot.message else {
            // Already omitted messages neither delimit the visible attempt
            // nor need another context edit.
            continue;
        };
        match message {
            AgentMessage::Assistant(_) => break,
            AgentMessage::ToolResult(result) if tool_call_ids.contains(&result.tool_call_id) => {
                let target_id = slot.source_entry_id.as_deref().ok_or_else(|| {
                    invalid_recovery_omission(format!(
                        "cannot resolve the source entry for tool result {} after assistant {assistant_source_entry_id}",
                        result.tool_call_id
                    ))
                })?;
                // Validate now, before the assistant edit is appended, so an
                // illegal result target cannot cause a predictable partial
                // omission. Append-time I/O errors are still propagated by
                // append_context_edit.
                validate_target(&active_path, target_id, "tool result")?;
                target_ids.push(target_id.to_string());
            }
            _ => {}
        }
    }

    let mut edit_ids = Vec::with_capacity(target_ids.len());
    for target_id in target_ids {
        edit_ids.push(session.append_context_edit(&target_id, None)?);
    }
    Ok(edit_ids)
}
