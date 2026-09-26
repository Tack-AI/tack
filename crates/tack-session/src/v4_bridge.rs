//! Bridge between [`SessionManager`](crate::SessionManager)'s v3-shaped
//! in-memory entries and the v4 transaction log: entry mapping in both
//! directions, mirrored current-state value writes (session name, labels,
//! lane configuration), and a lenient whole-file scanner for
//! list/search/stats-style consumers.
//!
//! The four v3 "change" entry kinds (`model_change`,
//! `thinking_level_change`, `session_info`, `label`) have no native v4
//! entry type. They are stored as v4 **custom** entries (keeping the
//! history needed by `build_session_context`, `get_entries` and export)
//! AND mirrored into v4 current-state values (lane configuration, session
//! name, entry labels) so v4-native tooling and fork projection see the
//! correct state. The migration ([`crate::v4::migrate`]) produces the
//! same shapes, so there is exactly one read path.

use serde_json::{Value, json};
use tack_agent_core::{AgentMessage, CustomAgentMessage};

use crate::context::{iso_to_millis, millis_to_iso};
use crate::entry::SessionEntry;
use crate::fork_policy::{
    NS_BRANCH_TIP, NS_ENTRY_LABEL, NS_LANE_CONFIG, NS_LANE_STATE, NS_SESSION_NAME,
    idle_lane_state_value,
};
use crate::v4::codec::{self, ParsedSessionHeader};
use crate::v4::types::{
    V4Entry, V4EntryBase, V4Header, V4NewWrite, V4UsageRow, V4ValueOp, V4Write,
};

/// custom_type tags used for the v3 change entries inside v4 files.
pub(crate) const CT_MODEL_CHANGE: &str = "model_change";
pub(crate) const CT_THINKING_LEVEL_CHANGE: &str = "thinking_level_change";
pub(crate) const CT_SESSION_INFO: &str = "session_info";
pub(crate) const CT_LABEL: &str = "label";

/// The branch every SessionManager-driven session writes to.
pub(crate) const MAIN_BRANCH: &str = "main";

/// Convert one in-memory entry into a v4 entry placeholder (seq/timestamp
/// are assigned by the store at commit; parent is taken as given).
pub(crate) fn session_entry_to_v4(entry: &SessionEntry) -> V4Entry {
    let base = V4EntryBase {
        id: entry.id().to_string(),
        parent_id: entry.parent_id().map(str::to_string),
        seq: 0,
        timestamp: iso_to_millis(entry.timestamp()).unwrap_or(0),
    };
    match entry {
        SessionEntry::Message { message, .. } => V4Entry::Message {
            base,
            message: message.clone(),
            terminate: None,
        },
        SessionEntry::CustomMessage {
            custom_type,
            content,
            display,
            details,
            timestamp,
            ..
        } => V4Entry::Message {
            base,
            message: AgentMessage::Custom(CustomAgentMessage {
                custom_type: custom_type.clone(),
                content: content.clone(),
                display: *display,
                details: details.clone(),
                timestamp: iso_to_millis(timestamp).unwrap_or(0),
            }),
            terminate: None,
        },
        SessionEntry::Compaction {
            summary,
            tokens_before,
            retained_tail,
            details,
            usage,
            from_hook,
            system_message,
            ..
        } => V4Entry::Compaction {
            base,
            summary: summary.clone(),
            retained_tail: compaction_tail_with_system(system_message, retained_tail),
            tokens_before: *tokens_before,
            details: details.clone(),
            usage: usage.clone(),
            from_hook: from_hook.unwrap_or(false),
        },
        SessionEntry::BranchSummary {
            from_id,
            summary,
            details,
            usage,
            from_hook,
            ..
        } => V4Entry::BranchSummary {
            base,
            // v3 encodes a root source as the "root" sentinel; v4 uses null.
            from_id: (from_id != "root").then(|| from_id.clone()),
            summary: summary.clone(),
            details: details.clone(),
            usage: usage.clone(),
            from_hook: from_hook.unwrap_or(false),
        },
        SessionEntry::Custom {
            custom_type, data, ..
        } => V4Entry::Custom {
            base,
            custom_type: custom_type.clone(),
            data: data.clone(),
        },
        SessionEntry::ModelChange {
            provider, model_id, ..
        } => V4Entry::Custom {
            base,
            custom_type: CT_MODEL_CHANGE.to_string(),
            data: Some(json!({ "provider": provider, "modelId": model_id })),
        },
        SessionEntry::ThinkingLevelChange { thinking_level, .. } => V4Entry::Custom {
            base,
            custom_type: CT_THINKING_LEVEL_CHANGE.to_string(),
            data: Some(json!({ "thinkingLevel": thinking_level })),
        },
        SessionEntry::SessionInfo { name, .. } => V4Entry::Custom {
            base,
            custom_type: CT_SESSION_INFO.to_string(),
            data: Some(json!({ "name": name })),
        },
        SessionEntry::Label {
            target_id, label, ..
        } => V4Entry::Custom {
            base,
            custom_type: CT_LABEL.to_string(),
            data: Some(json!({ "targetId": target_id, "label": label })),
        },
    }
}

/// v4 has no `systemMessage` field on compaction entries. To keep the
/// recorded prompt/tool state readable by BOTH tack and upstream v4
/// readers (which project `summary + retainedTail`), the replayed system
/// message leads the materialized tail. Upstream's own v3→v4 migration
/// drops the field; tack preserves it losslessly.
pub(crate) fn compaction_tail_with_system(
    system_message: &Option<tack_ai::SystemMessage>,
    retained_tail: &Option<Vec<AgentMessage>>,
) -> Vec<AgentMessage> {
    let mut tail: Vec<AgentMessage> = system_message
        .clone()
        .map(AgentMessage::System)
        .into_iter()
        .collect();
    if let Some(t) = retained_tail {
        tail.extend(t.iter().cloned());
    }
    tail
}

/// Rebuild an in-memory entry from a committed v4 entry.
pub(crate) fn v4_to_session_entry(entry: &V4Entry) -> SessionEntry {
    let base = entry.base();
    let id = base.id.clone();
    let parent_id = base.parent_id.clone();
    let timestamp = millis_to_iso(base.timestamp);
    match entry {
        V4Entry::Message {
            message: AgentMessage::Custom(custom),
            ..
        } => SessionEntry::CustomMessage {
            id,
            parent_id,
            timestamp,
            custom_type: custom.custom_type.clone(),
            content: custom.content.clone(),
            display: custom.display,
            details: custom.details.clone(),
        },
        V4Entry::Message { message, .. } => SessionEntry::Message {
            id,
            parent_id,
            timestamp,
            message: message.clone(),
        },
        V4Entry::Compaction {
            summary,
            retained_tail,
            tokens_before,
            details,
            usage,
            from_hook,
            ..
        } => SessionEntry::Compaction {
            id,
            parent_id,
            timestamp,
            summary: summary.clone(),
            // v4 compactions always carry the materialized checkpoint, so
            // the first-kept pointer is never needed to rebuild context.
            first_kept_entry_id: None,
            tokens_before: *tokens_before,
            retained_tail: Some(retained_tail.clone()),
            details: details.clone(),
            usage: usage.clone(),
            from_hook: Some(*from_hook),
            // v4 has no systemMessage field; the replayed system message
            // leads retained_tail (see compaction_tail_with_system).
            system_message: None,
            first_kept_entry_index: None,
        },
        V4Entry::BranchSummary {
            from_id,
            summary,
            details,
            usage,
            from_hook,
            ..
        } => SessionEntry::BranchSummary {
            id,
            parent_id,
            timestamp,
            from_id: from_id.clone().unwrap_or_else(|| "root".to_string()),
            summary: summary.clone(),
            details: details.clone(),
            usage: usage.clone(),
            from_hook: Some(*from_hook),
        },
        V4Entry::Custom {
            custom_type, data, ..
        } => custom_back(id, parent_id, timestamp, custom_type, data),
    }
}

/// Map a v4 custom entry back to a typed v3 change entry when it carries
/// one of our tags; anything else stays a plain custom entry.
fn custom_back(
    id: String,
    parent_id: Option<String>,
    timestamp: String,
    custom_type: &str,
    data: &Option<Value>,
) -> SessionEntry {
    let fallback = || SessionEntry::Custom {
        id: id.clone(),
        parent_id: parent_id.clone(),
        timestamp: timestamp.clone(),
        custom_type: custom_type.to_string(),
        data: data.clone(),
    };
    let Some(data) = data else {
        return fallback();
    };
    match custom_type {
        CT_MODEL_CHANGE => {
            match (
                data.get("provider").and_then(Value::as_str),
                data.get("modelId").and_then(Value::as_str),
            ) {
                (Some(provider), Some(model_id)) => SessionEntry::ModelChange {
                    id,
                    parent_id,
                    timestamp,
                    provider: provider.to_string(),
                    model_id: model_id.to_string(),
                },
                _ => fallback(),
            }
        }
        CT_THINKING_LEVEL_CHANGE => match data.get("thinkingLevel").and_then(Value::as_str) {
            Some(level) => SessionEntry::ThinkingLevelChange {
                id,
                parent_id,
                timestamp,
                thinking_level: level.to_string(),
            },
            None => fallback(),
        },
        CT_SESSION_INFO => SessionEntry::SessionInfo {
            id,
            parent_id,
            timestamp,
            name: data
                .get("name")
                .and_then(|v| v.as_str().map(str::to_string)),
        },
        CT_LABEL => match data.get("targetId").and_then(Value::as_str) {
            Some(target_id) => SessionEntry::Label {
                id,
                parent_id,
                timestamp,
                target_id: target_id.to_string(),
                label: data
                    .get("label")
                    .and_then(|v| v.as_str().map(str::to_string)),
            },
            None => fallback(),
        },
        _ => fallback(),
    }
}

/// Lane-configuration tracker for the live write path: the lane config
/// value is only (re)written once BOTH a model and a thinking level are
/// known (mirroring the migration's `selectedConfiguration` rule).
#[derive(Debug, Default)]
pub(crate) struct LaneTracker {
    /// Most recent (provider, model_id) seen.
    pub model: Option<(String, String)>,
    /// Most recent thinking level seen.
    pub thinking: Option<String>,
}

impl LaneTracker {
    /// Seed from a store's existing lane configuration (open path).
    pub(crate) fn from_config(config: Option<&crate::v4::types::LaneConfiguration>) -> Self {
        match config {
            Some(config) => LaneTracker {
                model: Some((config.model.provider.clone(), config.model.model_id.clone())),
                thinking: Some(config.thinking_level.clone()),
            },
            None => LaneTracker::default(),
        }
    }
}

/// The full write set for appending one entry to the main branch: the
/// entry itself, the branch-tip move, mirrored current-state values, and
/// a usage ledger row when the entry carries LLM usage.
///
/// `usage_row_id` is minted by the caller (the store checks id
/// uniqueness); `existing_tool_names` preserves the lane config's tool
/// list across rewrites.
pub(crate) fn live_append_writes(
    entry: &SessionEntry,
    tracker: &mut LaneTracker,
    existing_tool_names: Vec<String>,
    usage_row_id: String,
) -> Vec<V4NewWrite> {
    let mut writes = vec![
        V4NewWrite::Entry(session_entry_to_v4(entry)),
        V4NewWrite::ValueSet {
            namespace: NS_BRANCH_TIP.to_string(),
            key: MAIN_BRANCH.to_string(),
            value: Value::String(entry.id().to_string()),
        },
    ];
    match entry {
        SessionEntry::SessionInfo { name, .. } => match name {
            Some(name) => writes.push(V4NewWrite::ValueSet {
                namespace: NS_SESSION_NAME.to_string(),
                key: String::new(),
                value: Value::String(name.clone()),
            }),
            None => writes.push(V4NewWrite::ValueDelete {
                namespace: NS_SESSION_NAME.to_string(),
                key: String::new(),
            }),
        },
        SessionEntry::Label {
            target_id, label, ..
        } => match label {
            Some(label) => writes.push(V4NewWrite::ValueSet {
                namespace: NS_ENTRY_LABEL.to_string(),
                key: target_id.clone(),
                value: Value::String(label.clone()),
            }),
            None => writes.push(V4NewWrite::ValueDelete {
                namespace: NS_ENTRY_LABEL.to_string(),
                key: target_id.clone(),
            }),
        },
        SessionEntry::ModelChange {
            provider, model_id, ..
        } => {
            tracker.model = Some((provider.clone(), model_id.clone()));
        }
        SessionEntry::ThinkingLevelChange { thinking_level, .. } => {
            tracker.thinking = Some(thinking_level.clone());
        }
        _ => {}
    }
    // Lane configuration mirrors the latest model + thinking level once
    // both are known (state row is idempotent).
    if matches!(
        entry,
        SessionEntry::ModelChange { .. } | SessionEntry::ThinkingLevelChange { .. }
    ) && let (Some((provider, model_id)), Some(thinking)) = (&tracker.model, &tracker.thinking)
    {
        writes.push(V4NewWrite::ValueSet {
            namespace: NS_LANE_CONFIG.to_string(),
            key: MAIN_BRANCH.to_string(),
            value: json!({
                "model": { "provider": provider, "modelId": model_id },
                "thinkingLevel": thinking,
                "activeToolNames": existing_tool_names,
            }),
        });
        writes.push(V4NewWrite::ValueSet {
            namespace: NS_LANE_STATE.to_string(),
            key: MAIN_BRANCH.to_string(),
            value: idle_lane_state_value(),
        });
    }
    // Usage ledger: assistant turns plus compaction/branch-summary passes.
    let usage = match entry {
        SessionEntry::Message {
            message: AgentMessage::Assistant(a),
            ..
        } if !matches!(
            a.stop_reason,
            tack_ai::StopReason::Error | tack_ai::StopReason::Aborted
        ) =>
        {
            Some(&a.usage)
        }
        SessionEntry::Compaction { usage, .. } | SessionEntry::BranchSummary { usage, .. } => {
            usage.as_ref()
        }
        _ => None,
    };
    if let Some(usage) = usage {
        writes.push(V4NewWrite::Usage {
            id: usage_row_id,
            usage: usage.clone(),
            entry_id: Some(entry.id().to_string()),
            adjustment: false,
            details: None,
        });
    }
    writes
}

/// A lenient whole-file scan of a v4 session file, for read-only
/// consumers (session listing, full-text search, usage stats). Undecryptable
/// or malformed lines are skipped (the strict path is
/// [`crate::v4::V4Store::open`]).
#[derive(Debug)]
pub struct V4FileScan {
    /// The storage header.
    pub header: V4Header,
    /// Entries in commit order, mapped back to v3-shaped session entries.
    pub entries: Vec<SessionEntry>,
    /// Usage ledger rows in commit order.
    pub usage_rows: Vec<V4UsageRow>,
    /// Current session-name value, if any.
    pub session_name: Option<String>,
}

/// Extract the display text of a user message (text blocks joined),
/// borrowing the content — no message clone.
fn user_prompt_text(content: &tack_ai::UserContent) -> String {
    match content {
        tack_ai::UserContent::Text(t) => t.clone(),
        tack_ai::UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// A shallow summary of a v4 session file for picker UIs (`/resume`).
///
/// Unlike [`scan_v4_file_content`] this never builds `SessionEntry`
/// values: transaction lines are substring-prefiltered (the same memchr
/// trick the v3 listing path uses — fully JSON-parsing every transaction
/// line of every session made the picker O(total bytes parsed), and huge
/// sessions dominated its latency), and user messages are counted from
/// the parsed-but-owned writes WITHOUT the `v4_to_session_entry` deep
/// clone (which copies image base64 payloads per line).
#[derive(Debug)]
pub struct V4FileSummary {
    /// The storage header.
    pub header: V4Header,
    /// Current `tack.session.name` value, if any.
    pub session_name: Option<String>,
    /// Name from the first `session_info` custom entry, if any (fallback
    /// when no session-name value exists).
    pub session_info_name: Option<String>,
    /// Number of user messages in the log.
    pub user_message_count: usize,
    /// First user message text (preview).
    pub first_prompt: Option<String>,
}

/// Shallow-scan v4 file content for the `/resume` listing; `None` when
/// the first line is not a v4 header.
pub fn scan_v4_file_summary(content: &str) -> Option<V4FileSummary> {
    let mut lines = content.lines();
    let first = lines.next()?;
    let Some(ParsedSessionHeader::V4(header)) = codec::parse_session_header(first) else {
        return None;
    };
    let mut summary = V4FileSummary {
        header,
        session_name: None,
        session_info_name: None,
        user_message_count: 0,
        first_prompt: None,
    };
    for raw in lines {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let decrypted;
        let line = if crate::crypto::is_encrypted_line(trimmed) {
            match crate::crypto::decrypt_line(trimmed) {
                Some(plain) => {
                    decrypted = plain;
                    decrypted.as_str()
                }
                None => continue,
            }
        } else {
            trimmed
        };
        // Cheap substring pre-filter: only lines that can carry a user
        // message, the session-name value, or a session_info entry are
        // worth a full JSON parse. (Both compact and spaced `":`
        // serializations are accepted; false positives just cost one
        // parse and are rejected by the match below.)
        let has_user_msg =
            line.contains("\"role\":\"user\"") || line.contains("\"role\": \"user\"");
        let has_session_name = line.contains(NS_SESSION_NAME);
        let has_session_info = line.contains("\"customType\":\"session_info\"")
            || line.contains("\"customType\": \"session_info\"");
        if !has_user_msg && !has_session_name && !has_session_info {
            continue;
        }
        let Ok(writes) = codec::parse_transaction(line) else {
            continue;
        };
        for write in writes {
            match write {
                V4Write::Entry { entry } => match &entry {
                    V4Entry::Message {
                        message: AgentMessage::User(u),
                        ..
                    } => {
                        summary.user_message_count += 1;
                        if summary.first_prompt.is_none() {
                            summary.first_prompt = Some(user_prompt_text(&u.content));
                        }
                    }
                    V4Entry::Custom {
                        custom_type,
                        data: Some(data),
                        ..
                    } if custom_type == CT_SESSION_INFO && summary.session_info_name.is_none() => {
                        summary.session_info_name = data
                            .get("name")
                            .and_then(|v| v.as_str().map(str::to_string));
                    }
                    _ => {}
                },
                V4Write::Value {
                    op:
                        V4ValueOp::Set {
                            namespace, value, ..
                        },
                } if namespace == NS_SESSION_NAME => {
                    summary.session_name = value.as_str().map(str::to_string);
                }
                V4Write::Value {
                    op: V4ValueOp::Delete { namespace, .. },
                } if namespace == NS_SESSION_NAME => {
                    summary.session_name = None;
                }
                _ => {}
            }
        }
    }
    Some(summary)
}

/// Scan v4 file content; `None` when the first line is not a v4 header.
pub fn scan_v4_file_content(content: &str) -> Option<V4FileScan> {
    let mut lines = content.lines();
    let first = lines.next()?;
    let Some(ParsedSessionHeader::V4(header)) = codec::parse_session_header(first) else {
        return None;
    };
    let mut scan = V4FileScan {
        header,
        entries: Vec::new(),
        usage_rows: Vec::new(),
        session_name: None,
    };
    for raw in lines {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let decrypted;
        let line = if crate::crypto::is_encrypted_line(trimmed) {
            match crate::crypto::decrypt_line(trimmed) {
                Some(plain) => {
                    decrypted = plain;
                    decrypted.as_str()
                }
                None => continue,
            }
        } else {
            trimmed
        };
        let Ok(writes) = codec::parse_transaction(line) else {
            continue;
        };
        for write in writes {
            match write {
                V4Write::Entry { entry } => {
                    scan.entries.push(v4_to_session_entry(&entry));
                }
                V4Write::Usage { row } => scan.usage_rows.push(row),
                V4Write::Value {
                    op:
                        V4ValueOp::Set {
                            namespace, value, ..
                        },
                } if namespace == NS_SESSION_NAME => {
                    scan.session_name = value.as_str().map(str::to_string);
                }
                V4Write::Value {
                    op: V4ValueOp::Delete { namespace, .. },
                } if namespace == NS_SESSION_NAME => {
                    scan.session_name = None;
                }
                _ => {}
            }
        }
    }
    Some(scan)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::v4::V4Store;

    /// Build a v4 file with user/assistant messages, a session_info entry
    /// and a session-name value; return the file content.
    fn demo_v4_content() -> String {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let mut store = V4Store::create(
            &path,
            V4Header::new("s1".to_string(), "/work".to_string()),
            vec![],
        )
        .unwrap();
        store
            .commit(vec![V4NewWrite::ValueSet {
                namespace: NS_BRANCH_TIP.to_string(),
                key: MAIN_BRANCH.to_string(),
                value: Value::Null,
            }])
            .unwrap();
        // First user message: text + image blocks (image data must not be
        // deep-cloned into the summary).
        store
            .append_message(
                MAIN_BRANCH,
                AgentMessage::User(tack_ai::UserMessage {
                    content: tack_ai::UserContent::Blocks(vec![
                        tack_ai::InputContentBlock::text("hello world"),
                        tack_ai::InputContentBlock::Image {
                            data: "aGVsbG8=".repeat(1024),
                            mime_type: "image/png".to_string(),
                        },
                    ]),
                    timestamp: 1,
                }),
            )
            .unwrap();
        // An assistant-only line (no user substring) — the summary scan
        // must skip parsing it, and the count must stay exact even when
        // user text itself contains a role marker.
        store
            .append_message(
                MAIN_BRANCH,
                AgentMessage::user("contains \"role\":\"user\" inside"),
            )
            .unwrap();
        store
            .append_entry(
                MAIN_BRANCH,
                V4Entry::Custom {
                    base: V4EntryBase {
                        id: "info1".to_string(),
                        parent_id: None,
                        seq: 0,
                        timestamp: 0,
                    },
                    custom_type: CT_SESSION_INFO.to_string(),
                    data: Some(json!({ "name": "from-info" })),
                },
            )
            .unwrap();
        store.set_session_name("from-value").unwrap();
        std::fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn summary_scan_matches_full_scan_without_entry_clones() {
        let content = demo_v4_content();
        let summary = scan_v4_file_summary(&content).unwrap();
        assert_eq!(summary.header.id, "s1");
        // Two user messages; the role marker inside user text is not
        // double counted (the pre-filter false positive is rejected by
        // the parse + match).
        assert_eq!(summary.user_message_count, 2);
        assert_eq!(summary.first_prompt.as_deref(), Some("hello world"));
        assert_eq!(summary.session_name.as_deref(), Some("from-value"));
        assert_eq!(summary.session_info_name.as_deref(), Some("from-info"));

        // Parity with the full scan it replaces at the /resume call site.
        let full = scan_v4_file_content(&content).unwrap();
        let full_count = full
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    SessionEntry::Message {
                        message: AgentMessage::User(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(summary.user_message_count, full_count);
        assert_eq!(summary.session_name, full.session_name);
    }

    #[test]
    fn summary_scan_rejects_non_v4_content() {
        assert!(scan_v4_file_summary("{\"type\":\"session\"}\n").is_none());
        assert!(scan_v4_file_summary("").is_none());
    }
}
