//! LLM branch summarization for tree navigation (port of
//! `core/compaction/branch-summarization.ts`): when the user jumps to another
//! point in the session tree (/tree, /fork), the branch being abandoned is
//! summarized by the LLM and stored as a `branch_summary` entry so the
//! context survives in the session file (byte-compatible with TS pi).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;
use tack_agent_core::AgentMessage;
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    FileOperations, build_summarization_context, complete_summarization, compute_file_lists,
    estimate_tokens, extract_file_ops_from_message, format_file_operations, serialize_conversation,
};
use crate::context::session_entry_to_context_messages;
use crate::entry::SessionEntry;
use crate::manager::SessionManager;

const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const BRANCH_SUMMARY_PROMPT: &str = r#"Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Result of a branch summarization call.
#[derive(Clone, Debug)]
pub struct BranchSummaryResult {
    pub summary: String,
    pub usage: tack_ai::Usage,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Collect entries to summarize when navigating from `old_leaf_id` to
/// `target_id`: everything from the old leaf back to (excluding) the common
/// ancestor, in chronological order. Compaction boundaries are included —
/// their summaries become context.
pub fn collect_entries_for_branch_summary(
    session: &SessionManager,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> (Vec<SessionEntry>, Option<String>) {
    let Some(old_leaf_id) = old_leaf_id else {
        return (Vec::new(), None);
    };
    let entries = session.entries();
    // id → entry lookup once up front: walking paths with a per-step
    // `entries.iter().find` is O(n·depth) on long sessions (same pattern
    // as context.rs).
    let by_id: HashMap<&str, &SessionEntry> = entries.iter().map(|e| (e.id(), e)).collect();

    // Root-first path from an entry back to the root (cycle-safe).
    let path_to = |start: &str| -> Vec<&SessionEntry> {
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        let mut current = Some(start);
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            let Some(entry) = by_id.get(id).copied() else {
                break;
            };
            path.push(entry);
            current = entry.parent_id();
        }
        path.reverse();
        path
    };

    let old_path: HashSet<&str> = path_to(old_leaf_id).iter().map(|e| e.id()).collect();
    let target_path = path_to(target_id);
    let common_ancestor = target_path
        .iter()
        .rev()
        .find(|e| old_path.contains(e.id()))
        .map(|e| e.id().to_string());

    let mut collected: Vec<SessionEntry> = Vec::new();
    let mut current = Some(old_leaf_id);
    while let Some(id) = current {
        if Some(id) == common_ancestor.as_deref() {
            break;
        }
        let Some(entry) = by_id.get(id) else {
            break;
        };
        collected.push((*entry).clone());
        current = entry.parent_id();
    }
    collected.reverse();
    (collected, common_ancestor)
}

/// Entry → messages for summarization (tool results skipped; tool-call
/// context lives in the assistant message).
fn messages_from_entry(entry: &SessionEntry) -> Vec<AgentMessage> {
    session_entry_to_context_messages(entry)
        .into_iter()
        .filter(|m| !matches!(m, AgentMessage::ToolResult(_)))
        .collect()
}

/// Newest-to-oldest fill within the token budget; file ops are collected from
/// ALL entries (including prior pi-generated branch summaries' details for
/// cumulative tracking).
fn prepare_branch_entries(
    entries: &[SessionEntry],
    token_budget: u64,
) -> (Vec<AgentMessage>, FileOperations) {
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut file_ops = FileOperations::default();
    let mut total = 0u64;

    for entry in entries {
        if let SessionEntry::BranchSummary {
            details: Some(details),
            from_hook,
            ..
        } = entry
            && *from_hook != Some(true)
        {
            if let Some(read) = details.get("readFiles").and_then(Value::as_array) {
                for f in read.iter().filter_map(Value::as_str) {
                    file_ops.read.insert(f.to_string());
                }
            }
            if let Some(modified) = details.get("modifiedFiles").and_then(Value::as_array) {
                for f in modified.iter().filter_map(Value::as_str) {
                    file_ops.edited.insert(f.to_string());
                }
            }
        }
    }

    // Entries are walked newest-first and the result must be chronological,
    // so messages are PUSHED in exact reverse output order (newest entry
    // first, each entry's messages newest-first) and the Vec is reversed
    // once at the end. (Was `insert(0, ...)` per message: O(n²) on long
    // branches because every insert shifts the whole Vec.)
    for entry in entries.iter().rev() {
        let entry_messages = messages_from_entry(entry);
        if entry_messages.is_empty() {
            continue;
        }
        for message in &entry_messages {
            extract_file_ops_from_message(message, &mut file_ops);
        }
        let tokens: u64 = entry_messages.iter().map(estimate_tokens).sum();
        if token_budget > 0 && total + tokens > token_budget {
            // Summary entries are important context — squeeze them in when
            // we're not already at 90% of the budget, then stop.
            if matches!(
                entry,
                SessionEntry::Compaction { .. } | SessionEntry::BranchSummary { .. }
            ) && (total as f64) < token_budget as f64 * 0.9
            {
                messages.extend(entry_messages.into_iter().rev());
            }
            break;
        }
        messages.extend(entry_messages.into_iter().rev());
        total += tokens;
    }
    messages.reverse();
    (messages, file_ops)
}

/// Generate a summary of abandoned branch entries (TS generateBranchSummary).
pub async fn generate_branch_summary(
    entries: &[SessionEntry],
    model: &tack_ai::Model,
    provider: &Arc<dyn tack_ai::Provider>,
    auth: &tack_ai::oauth::ResolvedAuth,
    reasoning: Option<tack_ai::ThinkingLevel>,
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<BranchSummaryResult, String> {
    let context_window = if model.context_window > 0 {
        model.context_window as u64
    } else {
        128_000
    };
    let token_budget = context_window.saturating_sub(16_384);
    let (messages, file_ops) = prepare_branch_entries(entries, token_budget);
    if messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: "No content to summarize".to_string(),
            usage: tack_ai::Usage::default(),
            read_files: Vec::new(),
            modified_files: Vec::new(),
        });
    }

    let llm_messages = AgentMessage::default_convert_to_llm(&messages);
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{BRANCH_SUMMARY_PROMPT}");

    let response = complete_summarization(
        model,
        provider,
        &build_summarization_context(&prompt_text),
        2048,
        auth,
        reasoning,
        session_id,
        cancel,
    )
    .await?;
    if response.stop_reason == tack_ai::StopReason::Aborted {
        return Err("branch summarization aborted".to_string());
    }

    let (read_files, modified_files) = compute_file_lists(&file_ops);
    let mut summary = format!("{BRANCH_SUMMARY_PREAMBLE}{}", response.text());
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    Ok(BranchSummaryResult {
        summary: if summary.trim().is_empty() {
            "No summary generated".to_string()
        } else {
            summary
        },
        usage: response.usage.clone(),
        read_files,
        modified_files,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::manager::SessionManager;

    fn push_user(session: &mut SessionManager, text: &str) -> String {
        session.append_message(AgentMessage::user(text)).unwrap()
    }

    #[test]
    fn collects_abandoned_branch_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut session =
            SessionManager::create(dir.path(), Some(dir.path().join("sessions"))).unwrap();
        let root = push_user(&mut session, "root");
        let a1 = push_user(&mut session, "branch A msg 1");
        let _a2 = push_user(&mut session, "branch A msg 2");
        // Jump back to root and start branch B.
        session.branch(&root).unwrap();
        let _b1 = push_user(&mut session, "branch B msg 1");

        // Navigate from branch A's leaf to the branch B leaf.
        let leaf_b = session.leaf_id().unwrap().to_string();
        let (entries, common) = collect_entries_for_branch_summary(&session, Some(&a1), &leaf_b);
        assert_eq!(common.as_deref(), Some(root.as_str()));
        assert_eq!(
            entries.len(),
            1,
            "only 'branch A msg 1' is abandoned: {entries:?}"
        );

        // From the A tip: abandoning everything above the root.
        let tip_a = {
            session.branch(&a1).unwrap();
            push_user(&mut session, "branch A msg 2 again")
        };
        session.branch(&leaf_b).unwrap();
        let (entries, common) = collect_entries_for_branch_summary(&session, Some(&tip_a), &leaf_b);
        assert_eq!(common.as_deref(), Some(root.as_str()));
        assert_eq!(entries.len(), 2, "a1 and the new tip are abandoned");
    }

    #[test]
    fn no_old_leaf_nothing_to_summarize() {
        let dir = tempfile::tempdir().unwrap();
        let mut session =
            SessionManager::create(dir.path(), Some(dir.path().join("sessions"))).unwrap();
        let root = push_user(&mut session, "root");
        let (entries, common) = collect_entries_for_branch_summary(&session, None, &root);
        assert!(entries.is_empty());
        assert_eq!(common, None);
    }
}
