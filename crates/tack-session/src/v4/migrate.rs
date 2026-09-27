//! Streaming legacy v3 → v4 migration (port of upstream
//! `jsonl/legacy-v3.ts`). Two passes over the file, bounded memory:
//!
//! * Pass 1 streams line by line building only a structural index
//!   (id/parent/type/small metadata per record — never payloads), assigns
//!   retained entries fresh ids and sequences, and derives session name,
//!   labels, branch tip, lane configuration and imported usage.
//! * Pass 2 re-streams the file, materializing retained entries as v4
//!   transaction lines. Compaction retained tails are rebuilt from a cache
//!   holding only the messages some tail actually references.
//!
//! The rewrite is crash-safe: the original file is copied to `<path>.bak`
//! and the new content is staged in a temp file atomically renamed over
//! the original. Encryption state is preserved — undecryptable input fails
//! with [`V4Error::Encrypted`], and output transaction lines are encrypted
//! iff a session key is installed.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tack_agent_core::{
    AgentMessage, BranchSummaryMessage, CompactionSummaryMessage, CustomAgentMessage,
};
use tack_ai::Usage;

use super::V4Error;
use super::codec::{self, ParsedSessionHeader};
use super::types::{
    V4_STORAGE_VERSION, V4Entry, V4EntryBase, V4Header, V4UsageRow, V4ValueOp, V4Write,
};
use crate::context::{generate_id, iso_to_millis};
use crate::entry::SessionEntry;
use crate::fork_policy::{
    NS_BRANCH_TIP, NS_ENTRY_LABEL, NS_LANE_CONFIG, NS_LANE_STATE, NS_SESSION_NAME,
    idle_lane_state_value,
};

/// What a migration did (counts only — payloads are never retained).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct V4MigrationReport {
    /// v3 records kept as v4 entries (message/custom/custom_message/
    /// branch_summary/compaction plus the change records model_change/
    /// thinking_level_change/session_info/label as custom entries).
    pub retained_entries: usize,
    /// v3 records dropped structurally: `active_tools_change` entries
    /// and id-less malformed records (unknown/extension types are
    /// RETAINED as custom entries). Payloads survive in the `.bak`.
    pub discarded_entries: usize,
    /// Derived current-state value writes (name, labels, tip, lane).
    pub derived_values: usize,
}

/// One scanned v3 record in the structural index.
#[derive(Debug)]
struct IndexEntry {
    legacy_id: String,
    parent_id: Option<String>,
    /// Retained: the freshly minted v4 id. Discarded: the parent's mapped
    /// id (`None` when the record hangs off the root).
    mapped_id: Option<String>,
    retained: bool,
    /// Assigned sequence (retained entries only).
    seq: u64,
    kind: IndexKind,
}

#[derive(Debug)]
enum IndexKind {
    Message,
    Custom,
    CustomMessage,
    BranchSummary,
    Compaction {
        first_kept_entry_id: Option<String>,
        has_retained_tail: bool,
    },
    Label {
        target_id: String,
        label: Option<String>,
    },
    ModelChange {
        provider: String,
        model_id: String,
    },
    ThinkingLevelChange {
        level: String,
    },
    ActiveToolsChange {
        names: Vec<String>,
    },
    SessionInfo {
        name: Option<String>,
    },
    /// Unknown/extension record type: structure preserved, payload
    /// dropped (survives in the `.bak`).
    Other,
}

impl IndexKind {
    /// Entries that can produce a context message inside a compaction
    /// retained tail (upstream: retained && type !== "custom").
    fn produces_context_message(&self) -> bool {
        matches!(
            self,
            IndexKind::Message
                | IndexKind::CustomMessage
                | IndexKind::BranchSummary
                | IndexKind::Compaction { .. }
        )
    }
}

/// The pass-1 outcome: structural index plus everything needed to emit
/// derived values.
#[derive(Debug)]
struct MigrationIndex {
    /// File order.
    entries: Vec<IndexEntry>,
    by_legacy_id: HashMap<String, usize>,
    imported_usage: Usage,
    name: Option<String>,
    final_id: Option<String>,
    /// Next unassigned sequence after retained entries.
    next_seq: u64,
    /// Legacy ids whose context message must be cached in pass 2 for
    /// compaction tail reconstruction.
    required_tail_message_ids: HashSet<String>,
}

impl MigrationIndex {
    fn get(&self, legacy_id: &str) -> Result<&IndexEntry, V4Error> {
        self.by_legacy_id
            .get(legacy_id)
            .map(|&i| &self.entries[i])
            .ok_or_else(|| V4Error::MissingLegacyReference(legacy_id.to_string()))
    }

    /// Resolve a legacy id to its v4 id; discarded records resolve to
    /// their nearest retained ancestor (upstream `createLegacyIdResolver`).
    fn resolve(&self, legacy_id: Option<&str>) -> Result<Option<String>, V4Error> {
        match legacy_id {
            None => Ok(None),
            Some(id) => Ok(self.get(id)?.mapped_id.clone()),
        }
    }
}

/// Decrypt one line when needed; undecryptable encrypted lines are a hard
/// error (mirrors `SessionManager::open`'s guard).
fn decrypt_if_needed(raw: &str, path: &Path) -> Result<String, V4Error> {
    let trimmed = raw.trim();
    if crate::crypto::is_encrypted_line(trimmed) {
        crate::crypto::decrypt_line(trimmed).ok_or_else(|| V4Error::Encrypted(path.to_path_buf()))
    } else {
        Ok(trimmed.to_string())
    }
}

/// Iterate complete (newline-terminated) lines after the header; a torn
/// final line is ignored, mirroring upstream's `line.terminated` check.
struct CompleteLines {
    reader: std::io::BufReader<std::fs::File>,
    buf: String,
    done: bool,
}

impl CompleteLines {
    fn open(path: &Path) -> Result<Self, V4Error> {
        Ok(CompleteLines {
            reader: std::io::BufReader::new(std::fs::File::open(path)?),
            buf: String::new(),
            done: false,
        })
    }

    /// Next complete line (without terminator); `None` at EOF or when the
    /// final line lacks a terminator.
    fn next_line(&mut self) -> Result<Option<String>, V4Error> {
        if self.done {
            return Ok(None);
        }
        self.buf.clear();
        let n = self.reader.read_line(&mut self.buf)?;
        if n == 0 {
            self.done = true;
            return Ok(None);
        }
        if !self.buf.ends_with('\n') {
            self.done = true;
            return Ok(None); // torn tail
        }
        let line = self.buf.trim_end_matches(['\n', '\r']).to_string();
        Ok(Some(line))
    }
}

fn json_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn json_string(value: &Value, key: &str) -> Option<String> {
    json_str(value, key).map(str::to_string)
}

fn json_opt_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        _ => None,
    })
}

/// Extract the structural metadata of one v3 record (pass 1). The record
/// `type` decides retained vs discarded; unknown types fold structurally.
fn index_kind(record_type: &str, value: &Value) -> IndexKind {
    match record_type {
        "message" => IndexKind::Message,
        "custom" => IndexKind::Custom,
        "custom_message" => IndexKind::CustomMessage,
        "branch_summary" => IndexKind::BranchSummary,
        "compaction" => IndexKind::Compaction {
            first_kept_entry_id: json_opt_string(value, "firstKeptEntryId"),
            has_retained_tail: value.get("retainedTail").is_some_and(|v| v.is_array()),
        },
        "label" => IndexKind::Label {
            target_id: json_string(value, "targetId").unwrap_or_default(),
            label: json_opt_string(value, "label"),
        },
        "model_change" => IndexKind::ModelChange {
            provider: json_string(value, "provider").unwrap_or_default(),
            model_id: json_string(value, "modelId").unwrap_or_default(),
        },
        "thinking_level_change" => IndexKind::ThinkingLevelChange {
            level: json_string(value, "thinkingLevel").unwrap_or_default(),
        },
        "active_tools_change" => IndexKind::ActiveToolsChange {
            names: value
                .get("activeToolNames")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        },
        "session_info" => IndexKind::SessionInfo {
            name: json_opt_string(value, "name"),
        },
        _ => IndexKind::Other,
    }
}

fn is_retained(kind: &IndexKind) -> bool {
    // The v3 "change" records (model/thinking/session_info/label) are
    // retained as custom entries — tack's session context rebuilds
    // model/thinking state from these entries, so folding them away would
    // lose it on resume. They are ALSO folded into derived values
    // (lane config, session name, labels) for v4-native consumers.
    // Unknown/extension record types (`Other`) are likewise retained as
    // custom entries carrying their full payload: tack's standing rule is
    // no silent data loss (upstream errors out on these).
    matches!(
        kind,
        IndexKind::Message
            | IndexKind::Custom
            | IndexKind::CustomMessage
            | IndexKind::BranchSummary
            | IndexKind::Compaction { .. }
            | IndexKind::ModelChange { .. }
            | IndexKind::ThinkingLevelChange { .. }
            | IndexKind::SessionInfo { .. }
            | IndexKind::Label { .. }
            | IndexKind::Other
    )
}

/// Usage attributable to one v3 record (upstream `legacyEntryUsage`):
/// assistant/toolResult message usage plus compaction/branch-summary LLM
/// usage.
fn legacy_entry_usage(record_type: &str, value: &Value) -> Option<Usage> {
    let usage_value = match record_type {
        "message" => {
            let role = value.pointer("/message/role")?;
            if role == "assistant" || role == "toolResult" {
                value.pointer("/message/usage")?
            } else {
                return None;
            }
        }
        "compaction" | "branch_summary" => value.get("usage")?,
        _ => return None,
    };
    serde_json::from_value(usage_value.clone()).ok()
}

fn add_usage(total: &mut Usage, u: &Usage) {
    total.input += u.input;
    total.output += u.output;
    total.cache_read += u.cache_read;
    total.cache_write += u.cache_write;
    add_optional(&mut total.cache_write_1h, u.cache_write_1h);
    add_optional(&mut total.reasoning, u.reasoning);
    total.total_tokens += u.total_tokens;
    total.cost.input += u.cost.input;
    total.cost.output += u.cost.output;
    total.cost.cache_read += u.cost.cache_read;
    total.cost.cache_write += u.cost.cache_write;
    total.cost.total += u.cost.total;
}

/// Sum an optional token class (upstream `addUsage` sums `cacheWrite1h`
/// and `reasoning` too): `Some` when either side carries a value.
fn add_optional(total: &mut Option<u64>, u: Option<u64>) {
    if let Some(v) = u {
        *total = Some(total.unwrap_or(0) + v);
    }
}

/// Pass 1: stream the file, building the structural index.
fn scan_v3_file(path: &Path, header_cwd: &str) -> Result<MigrationIndex, V4Error> {
    let _ = header_cwd; // identity was already validated by the caller
    let mut lines = CompleteLines::open(path)?;
    // Skip the header line.
    let _ = lines.next_line()?;
    let mut entries: Vec<IndexEntry> = Vec::new();
    let mut by_legacy_id: HashMap<String, usize> = HashMap::new();
    let mut minted: HashSet<String> = HashSet::new();
    let mut imported_usage = Usage::zero();
    let mut name = None;
    let mut final_id = None;
    let mut next_seq = 1u64;
    let mut line_number = 1usize;

    while let Some(raw) = lines.next_line()? {
        line_number += 1;
        let decrypted = decrypt_if_needed(&raw, path)?;
        let value: Value = serde_json::from_str(&decrypted).map_err(|e| V4Error::CorruptLine {
            path: path.to_path_buf(),
            line: line_number,
            reason: format!("not valid JSON: {e}"),
        })?;
        let record_type = json_str(&value, "type").unwrap_or("");
        // Records without an id (malformed/ancient) are skipped rather
        // than fatal: auto-migration happens on session OPEN, and a hard
        // error would lock the user out of the whole session. Payloads
        // survive in the `.bak`.
        let Some(legacy_id) = json_string(&value, "id") else {
            continue;
        };
        let parent_id = match value.get("parentId") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            // A non-string parentId can never resolve (upstream fails the
            // same lookup); re-rooting the record would silently corrupt
            // the tree, so fail loudly instead.
            Some(other) => {
                return Err(V4Error::MissingLegacyParent {
                    id: legacy_id.clone(),
                    line: line_number,
                    parent: other.to_string(),
                });
            }
        };
        if by_legacy_id.contains_key(&legacy_id) {
            return Err(V4Error::CorruptLine {
                path: path.to_path_buf(),
                line: line_number,
                reason: format!("duplicate legacy v3 entry id: {legacy_id}"),
            });
        }
        let kind = index_kind(record_type, &value);
        let retained = is_retained(&kind);
        // Parent mappings fold during the scan: a discarded record maps to
        // its parent's mapped id, so references only ever need one lookup.
        let mapped_parent = match &parent_id {
            None => Some(None),
            Some(parent) => match by_legacy_id.get(parent) {
                Some(&i) => Some(entries[i].mapped_id.clone()),
                None => {
                    return Err(V4Error::MissingLegacyParent {
                        id: legacy_id.clone(),
                        line: line_number,
                        parent: parent.clone(),
                    });
                }
            },
        };
        let (mapped_id, seq) = if retained {
            let id = generate_id(&|id: &str| minted.contains(id));
            minted.insert(id.clone());
            let seq = next_seq;
            next_seq += 1;
            (Some(id), seq)
        } else {
            (mapped_parent.clone().flatten(), 0)
        };
        if let IndexKind::SessionInfo { name: n } = &kind {
            name = n.clone();
        }
        if let Some(u) = legacy_entry_usage(record_type, &value) {
            add_usage(&mut imported_usage, &u);
        }
        by_legacy_id.insert(legacy_id.clone(), entries.len());
        entries.push(IndexEntry {
            legacy_id: legacy_id.clone(),
            parent_id,
            mapped_id,
            retained,
            seq,
            kind,
        });
        final_id = Some(legacy_id);
    }

    let mut index = MigrationIndex {
        entries,
        by_legacy_id,
        imported_usage,
        name,
        final_id,
        next_seq,
        required_tail_message_ids: HashSet::new(),
    };
    collect_required_tail_message_ids(&mut index)?;
    Ok(index)
}

/// Mark every legacy entry whose context message is needed to rebuild a
/// compaction retained tail (upstream `collectRequiredTailMessageIds`).
///
/// A compaction WITHOUT a materialized checkpoint tail is only migratable
/// when its boundary is knowable: `firstKeptEntryId` must be present and
/// on the compaction's parent ancestry. Anything else would migrate into
/// a silently truncated context — fail the migration instead (the `.bak`
/// preserves the original, and the v3 backend can still open the file).
fn collect_required_tail_message_ids(index: &mut MigrationIndex) -> Result<(), V4Error> {
    let mut required = HashSet::new();
    for entry in &index.entries {
        let IndexKind::Compaction {
            first_kept_entry_id,
            has_retained_tail,
        } = &entry.kind
        else {
            continue;
        };
        if *has_retained_tail {
            continue; // checkpoint form: the tail is already materialized
        }
        let Some(first_kept) = first_kept_entry_id else {
            return Err(V4Error::MissingCompactionBoundary {
                id: entry.legacy_id.clone(),
            });
        };
        if entry.parent_id.is_none() {
            // The ancestry walk starts at the parent; a root-hung
            // compaction can never reach its boundary (upstream throws
            // the same way by falling off the walk).
            return Err(V4Error::CompactionBoundaryNotOnBranch {
                id: entry.legacy_id.clone(),
                first_kept: first_kept.clone(),
            });
        }
        // Walk physical ancestry from the compaction's parent through
        // firstKeptEntryId, inclusive (upstream `retainedTailStructure`).
        let mut current = entry.parent_id.clone();
        while let Some(id) = current {
            let ancestor = index.get(&id)?;
            if ancestor.retained && ancestor.kind.produces_context_message() {
                required.insert(id.clone());
            }
            if id == *first_kept {
                break;
            }
            current = ancestor.parent_id.clone();
            if current.is_none() {
                return Err(V4Error::CompactionBoundaryNotOnBranch {
                    id: entry.legacy_id.clone(),
                    first_kept: first_kept.clone(),
                });
            }
        }
    }
    index.required_tail_message_ids = required;
    Ok(())
}

/// Recover the lane configuration by walking the final entry's ancestry,
/// consuming the nearest change of each kind (upstream
/// `selectedConfiguration`): a lane config exists only when BOTH a model
/// and a thinking level are recoverable.
fn selected_configuration(index: &MigrationIndex) -> Option<Value> {
    let mut model: Option<(String, String)> = None;
    let mut thinking_level: Option<String> = None;
    let mut active_tool_names: Option<Vec<String>> = None;
    let mut current = index.final_id.clone();
    while let Some(id) = current {
        let entry = index.get(&id).ok()?;
        match &entry.kind {
            IndexKind::ModelChange { provider, model_id } if model.is_none() => {
                model = Some((provider.clone(), model_id.clone()));
            }
            IndexKind::ThinkingLevelChange { level } if thinking_level.is_none() => {
                thinking_level = Some(level.clone());
            }
            IndexKind::ActiveToolsChange { names } if active_tool_names.is_none() => {
                active_tool_names = Some(names.clone());
            }
            _ => {}
        }
        current = entry.parent_id.clone();
    }
    let (provider, model_id) = model?;
    let thinking_level = thinking_level?;
    Some(serde_json::json!({
        "model": {"provider": provider, "modelId": model_id},
        "thinkingLevel": thinking_level,
        "activeToolNames": active_tool_names.unwrap_or_default(),
    }))
}

/// Derive the current-state value writes (upstream
/// `normalizeLegacyV3Values`): session name, surviving labels, branch tip,
/// and — when recoverable — the main lane's config + idle state.
fn derive_value_writes(index: &MigrationIndex) -> Result<Vec<(String, String, Value)>, V4Error> {
    let mut values: Vec<(String, String, Value)> = Vec::new();
    // Upstream writes the name only when truthy: an empty-string name is
    // skipped (and thereby erases any earlier one).
    if let Some(name) = &index.name
        && !name.is_empty()
    {
        values.push((
            NS_SESSION_NAME.to_string(),
            String::new(),
            Value::String(name.clone()),
        ));
    }
    // Latest label per target wins; cleared labels are dropped (upstream
    // treats an empty-string label as cleared too). Targets resolve
    // through the id mapping; targets resolving to the root (null) are
    // skipped (upstream behavior).
    let mut labels: HashMap<String, Option<String>> = HashMap::new();
    let mut label_order: Vec<String> = Vec::new();
    for entry in &index.entries {
        let IndexKind::Label { target_id, label } = &entry.kind else {
            continue;
        };
        let Some(target) = index.resolve(Some(target_id))? else {
            continue;
        };
        if !labels.contains_key(&target) {
            label_order.push(target.clone());
        }
        labels.insert(target, label.clone().filter(|l| !l.is_empty()));
    }
    for target in label_order {
        if let Some(Some(label)) = labels.get(&target) {
            values.push((
                NS_ENTRY_LABEL.to_string(),
                target,
                Value::String(label.clone()),
            ));
        }
    }
    let tip = index.resolve(index.final_id.as_deref())?;
    values.push((
        NS_BRANCH_TIP.to_string(),
        "main".to_string(),
        tip.map(Value::String).unwrap_or(Value::Null),
    ));
    if let Some(config) = selected_configuration(index) {
        values.push((NS_LANE_CONFIG.to_string(), "main".to_string(), config));
        values.push((
            NS_LANE_STATE.to_string(),
            "main".to_string(),
            idle_lane_state_value(),
        ));
    }
    Ok(values)
}

/// An entry timestamp as epoch millis, lenient like upstream's
/// `Date.parse`: RFC-3339 first, then RFC-2822 and date-only forms.
/// Anything else yields `None` and the caller substitutes the nearest
/// known timestamp (upstream propagates NaN, which JSON renders as
/// `null`; tack's `u64` timestamps cannot). One malformed timestamp must
/// never abort the whole migration.
fn lenient_timestamp_millis(timestamp: &str) -> Option<u64> {
    if let Some(ms) = iso_to_millis(timestamp) {
        return Some(ms);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(timestamp) {
        return Some(dt.timestamp_millis() as u64);
    }
    let date = chrono::NaiveDate::parse_from_str(timestamp, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis() as u64)
}

/// Project one v3 entry into a context message for a compaction retained
/// tail (upstream `projectContextMessage`). `fallback_ts` substitutes for
/// an unparseable entry timestamp (see [`lenient_timestamp_millis`]).
fn project_context_message(
    entry: &SessionEntry,
    index: &MigrationIndex,
    fallback_ts: u64,
) -> Result<Option<AgentMessage>, V4Error> {
    Ok(match entry {
        SessionEntry::Message { message, .. } => Some(message.clone()),
        SessionEntry::CustomMessage {
            custom_type,
            content,
            display,
            details,
            timestamp,
            ..
        } => Some(AgentMessage::Custom(CustomAgentMessage {
            custom_type: custom_type.clone(),
            content: content.clone(),
            display: *display,
            details: details.clone(),
            timestamp: lenient_timestamp_millis(timestamp).unwrap_or(fallback_ts),
        })),
        SessionEntry::BranchSummary {
            summary,
            from_id,
            timestamp,
            ..
        } => {
            if summary.is_empty() {
                None
            } else {
                let from = if from_id == "root" {
                    None
                } else {
                    index.resolve(Some(from_id))?
                };
                Some(AgentMessage::BranchSummary(BranchSummaryMessage {
                    summary: summary.clone(),
                    // v4/upstream encode a root source as `fromId: null`.
                    from_id: from,
                    timestamp: lenient_timestamp_millis(timestamp).unwrap_or(fallback_ts),
                }))
            }
        }
        SessionEntry::Compaction {
            summary,
            tokens_before,
            timestamp,
            ..
        } => Some(AgentMessage::CompactionSummary(CompactionSummaryMessage {
            summary: summary.clone(),
            tokens_before: *tokens_before,
            timestamp: lenient_timestamp_millis(timestamp).unwrap_or(fallback_ts),
        })),
        _ => None,
    })
}

/// Materialize one retained v3 record as a v4 entry (upstream
/// `normalizeRetainedEntry`). `fallback_ts` substitutes for an
/// unparseable entry timestamp (see [`lenient_timestamp_millis`]).
fn materialize_entry(
    entry: &SessionEntry,
    indexed: &IndexEntry,
    index: &MigrationIndex,
    tail_cache: &HashMap<String, AgentMessage>,
    fallback_ts: u64,
) -> Result<V4Entry, V4Error> {
    let base = V4EntryBase {
        id: indexed
            .mapped_id
            .clone()
            .ok_or_else(|| V4Error::MissingLegacyReference(indexed.legacy_id.clone()))?,
        parent_id: index.resolve(indexed.parent_id.as_deref())?,
        seq: indexed.seq,
        timestamp: lenient_timestamp_millis(entry.timestamp()).unwrap_or(fallback_ts),
    };
    Ok(match entry {
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
            message: AgentMessage::Custom(CustomAgentMessage {
                custom_type: custom_type.clone(),
                content: content.clone(),
                display: *display,
                details: details.clone(),
                timestamp: lenient_timestamp_millis(timestamp).unwrap_or(fallback_ts),
            }),
            base,
            terminate: None,
        },
        SessionEntry::BranchSummary {
            from_id,
            summary,
            details,
            usage,
            from_hook,
            ..
        } => {
            let from = if from_id == "root" {
                None
            } else {
                index.resolve(Some(from_id))?
            };
            V4Entry::BranchSummary {
                base,
                from_id: from,
                summary: summary.clone(),
                details: details.clone(),
                usage: usage.clone(),
                from_hook: from_hook.unwrap_or(false),
            }
        }
        SessionEntry::Compaction {
            summary,
            tokens_before,
            retained_tail,
            details,
            usage,
            from_hook,
            system_message,
            ..
        } => {
            let tail = match retained_tail {
                // v3 checkpoint form: the tail is materialized already.
                Some(tail) => crate::v4_bridge::compaction_tail_with_system(
                    system_message,
                    &Some(tail.clone()),
                ),
                // Rebuilt tail: the replayed system message leads it too
                // (preserving `systemMessage` is unconditional — see
                // compaction_tail_with_system).
                None => crate::v4_bridge::compaction_tail_with_system(
                    system_message,
                    &Some(rebuild_retained_tail(indexed, index, tail_cache)?),
                ),
            };
            V4Entry::Compaction {
                base,
                summary: summary.clone(),
                retained_tail: tail,
                tokens_before: *tokens_before,
                details: details.clone(),
                usage: usage.clone(),
                from_hook: from_hook.unwrap_or(false),
            }
        }
        SessionEntry::Custom {
            custom_type, data, ..
        } => V4Entry::Custom {
            base,
            custom_type: custom_type.clone(),
            data: data.clone(),
        },
        // Change records are kept as custom entries (same shapes the live
        // v4 write path produces — see v4_bridge), so migrated and
        // live-written v4 files share one read path.
        SessionEntry::ModelChange {
            provider, model_id, ..
        } => V4Entry::Custom {
            base,
            custom_type: crate::v4_bridge::CT_MODEL_CHANGE.to_string(),
            data: Some(serde_json::json!({ "provider": provider, "modelId": model_id })),
        },
        SessionEntry::ThinkingLevelChange { thinking_level, .. } => V4Entry::Custom {
            base,
            custom_type: crate::v4_bridge::CT_THINKING_LEVEL_CHANGE.to_string(),
            data: Some(serde_json::json!({ "thinkingLevel": thinking_level })),
        },
        SessionEntry::SessionInfo { name, .. } => V4Entry::Custom {
            base,
            custom_type: crate::v4_bridge::CT_SESSION_INFO.to_string(),
            data: Some(serde_json::json!({ "name": name })),
        },
        SessionEntry::Label {
            target_id, label, ..
        } => V4Entry::Custom {
            base,
            custom_type: crate::v4_bridge::CT_LABEL.to_string(),
            data: Some(serde_json::json!({
                "targetId": index.resolve(Some(target_id))?,
                "label": label,
            })),
        },
    })
}

/// Rebuild a compaction's retained tail from the pass-2 message cache,
/// oldest first (upstream `retainedTailStructure` + reverse). Boundary
/// validation already happened in pass 1
/// ([`collect_required_tail_message_ids`]); the error arms here are
/// defensive.
fn rebuild_retained_tail(
    compaction: &IndexEntry,
    index: &MigrationIndex,
    tail_cache: &HashMap<String, AgentMessage>,
) -> Result<Vec<AgentMessage>, V4Error> {
    let IndexKind::Compaction {
        first_kept_entry_id: Some(first_kept),
        ..
    } = &compaction.kind
    else {
        return Err(V4Error::MissingCompactionBoundary {
            id: compaction.legacy_id.clone(),
        });
    };
    let mut tail = Vec::new();
    let mut current = compaction.parent_id.clone();
    while let Some(id) = current {
        let ancestor = index.get(&id)?;
        if let Some(message) = tail_cache.get(&id) {
            tail.push(message.clone());
        }
        if id == *first_kept {
            break;
        }
        current = ancestor.parent_id.clone();
        if current.is_none() {
            return Err(V4Error::CompactionBoundaryNotOnBranch {
                id: compaction.legacy_id.clone(),
                first_kept: first_kept.clone(),
            });
        }
    }
    tail.reverse();
    Ok(tail)
}

/// Normalize the v3 header into a v4 one (upstream
/// `normalizeLegacyV3Header`): `parentSession` (a file path) resolves to
/// the parent's id when readable, else survives as
/// `legacyParentSessionPath`.
fn normalize_header(
    legacy: &crate::entry::SessionHeader,
    created_at: u64,
) -> Result<V4Header, V4Error> {
    let mut header = V4Header {
        v: super::types::V4_FORMAT_VERSION,
        kind: "header".to_string(),
        id: legacy.id.clone(),
        storage_version: V4_STORAGE_VERSION,
        created_at,
        cwd: legacy.cwd.clone(),
        parent_session_id: None,
        legacy_parent_session_path: None,
        next_seq: None,
    };
    if let Some(parent_path) = &legacy.parent_session {
        let resolved = read_first_line_of(Path::new(parent_path))
            .ok()
            .flatten()
            .and_then(|line| codec::parse_session_header(&line))
            .map(|parsed| match parsed {
                ParsedSessionHeader::V4(h) => h.id,
                ParsedSessionHeader::LegacyV3(h) => h.id,
            });
        match resolved {
            Some(id) => header.parent_session_id = Some(id),
            None => header.legacy_parent_session_path = Some(parent_path.clone()),
        }
    }
    Ok(header)
}

/// Read a file's first line for header sniffing. Unlike
/// [`CompleteLines`] an unterminated first line still counts (upstream's
/// parent-session sniff does not require the terminator either).
fn read_first_line_of(path: &Path) -> Result<Option<String>, std::io::Error> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut buf = String::new();
    if reader.read_line(&mut buf)? == 0 {
        return Ok(None);
    }
    let line = buf.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
        return Ok(None);
    }
    Ok(Some(line.to_string()))
}

/// Render one transaction line for the migrated file, encrypting when a
/// session key is installed (an encryption failure aborts the migration
/// BEFORE the original file is replaced — never degrade to plaintext).
fn render_out_line(write: &V4Write) -> Result<String, V4Error> {
    let line = codec::serialize_transaction(std::slice::from_ref(write));
    if crate::crypto::session_key().is_some() {
        crate::crypto::encrypt_line(&line).ok_or(V4Error::EncryptionFailed)
    } else {
        Ok(line)
    }
}

#[cfg(unix)]
fn set_file_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_file_private(_path: &Path) {}

/// Migrate a legacy v3 session file to format 4 in place, streaming.
///
/// The original is preserved as `<path>.bak`. Opening the migrated file
/// through [`super::V4Store::open`] replays the same state. Errors leave
/// the original file untouched.
pub fn migrate_v3_to_v4(path: &Path) -> Result<V4MigrationReport, V4Error> {
    // Read and validate the header first.
    let first =
        read_first_line_of(path)?.ok_or_else(|| V4Error::MissingHeader(path.to_path_buf()))?;
    let Some(ParsedSessionHeader::LegacyV3(legacy_header)) = codec::parse_session_header(&first)
    else {
        return Err(V4Error::MissingHeader(path.to_path_buf()));
    };
    let created_at =
        iso_to_millis(&legacy_header.timestamp).ok_or_else(|| V4Error::CorruptLine {
            path: path.to_path_buf(),
            line: 1,
            reason: format!("invalid header timestamp: {}", legacy_header.timestamp),
        })?;

    // Pass 1: structural index.
    let index = scan_v3_file(path, &legacy_header.cwd)?;
    let value_writes = derive_value_writes(&index)?;
    let mut header = normalize_header(&legacy_header, created_at)?;
    // All sequences are known after pass 1: retained entries took
    // 1..index.next_seq, values and the usage row follow.
    header.next_seq = Some(index.next_seq + value_writes.len() as u64 + 1);

    // Backup first: a failed migration must leave the original intact.
    let mut bak_name = path.as_os_str().to_owned();
    bak_name.push(".bak");
    let bak = PathBuf::from(bak_name);
    std::fs::copy(path, &bak)?;
    set_file_private(&bak);

    // Pass 2: stream again, materializing retained entries.
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);

    let mut report = V4MigrationReport::default();
    let write_result = (|| {
        let mut out = std::fs::File::create(&tmp)?;
        out.write_all(
            serde_json::to_string(&header)
                .expect("header serializes")
                .as_bytes(),
        )?;
        out.write_all(b"\n")?;
        let mut seq = index.next_seq;
        let mut tail_cache: HashMap<String, AgentMessage> = HashMap::new();
        let mut lines = CompleteLines::open(path)?;
        // Guard against the source changing between the two passes
        // (upstream re-checks the header identity and per-line id/type).
        let pass2_header = lines.next_line()?;
        let pass2_header = pass2_header
            .as_deref()
            .and_then(codec::parse_session_header);
        match pass2_header {
            Some(ParsedSessionHeader::LegacyV3(ref h))
                if h.id == legacy_header.id && h.cwd == legacy_header.cwd => {}
            _ => {
                return Err(V4Error::CorruptLine {
                    path: path.to_path_buf(),
                    line: 1,
                    reason: "legacy v3 source changed during migration".to_string(),
                });
            }
        }
        let mut line_number = 1usize;
        // Fallback for unparseable timestamps: the previous record's
        // timestamp, seeded with the header creation time.
        let mut last_timestamp = created_at;
        while let Some(raw) = lines.next_line()? {
            line_number += 1;
            let decrypted = decrypt_if_needed(&raw, path)?;
            let value: Value =
                serde_json::from_str(&decrypted).map_err(|e| V4Error::CorruptLine {
                    path: path.to_path_buf(),
                    line: line_number,
                    reason: format!("not valid JSON: {e}"),
                })?;
            let Some(legacy_id) = json_string(&value, "id") else {
                // Mirror pass 1: id-less records are skipped, not fatal.
                report.discarded_entries += 1;
                continue;
            };
            let indexed = index.get(&legacy_id)?;
            if !indexed.retained {
                report.discarded_entries += 1;
                continue;
            }
            // Records that cannot materialize as a typed SessionEntry —
            // unknown/extension types, and payloads that no longer fit
            // the typed schema — are preserved as custom entries with
            // their full payload: no silent data loss, and one bad record
            // never aborts the migration (upstream passes payloads
            // through verbatim).
            let entry = if matches!(indexed.kind, IndexKind::Other) {
                None
            } else {
                match parse_retained_entry(&value) {
                    Some(entry) => Some(entry),
                    None => {
                        // Best effort: a record some compaction tail needs
                        // still contributes its raw message payload.
                        if index.required_tail_message_ids.contains(&legacy_id)
                            && let Some(raw_message) = value.pointer("/message").cloned()
                            && let Ok(message) = serde_json::from_value::<AgentMessage>(raw_message)
                        {
                            tail_cache.insert(legacy_id.clone(), message);
                        }
                        None
                    }
                }
            };
            let v4_entry = match &entry {
                Some(entry) => {
                    if index.required_tail_message_ids.contains(&legacy_id)
                        && let Some(message) =
                            project_context_message(entry, &index, last_timestamp)?
                    {
                        tail_cache.insert(legacy_id.clone(), message);
                    }
                    materialize_entry(entry, indexed, &index, &tail_cache, last_timestamp)?
                }
                None => payload_preserving_entry(&value, indexed, &index, last_timestamp)?,
            };
            last_timestamp = v4_entry.base().timestamp;
            out.write_all(render_out_line(&V4Write::Entry { entry: v4_entry })?.as_bytes())?;
            out.write_all(b"\n")?;
            report.retained_entries += 1;
        }
        // Derived values.
        for (namespace, key, value) in &value_writes {
            let write = V4Write::Value {
                op: V4ValueOp::Set {
                    seq,
                    namespace: namespace.clone(),
                    key: key.clone(),
                    value: value.clone(),
                },
            };
            seq += 1;
            out.write_all(render_out_line(&write)?.as_bytes())?;
            out.write_all(b"\n")?;
            report.derived_values += 1;
        }
        // Imported usage as one adjustment row (upstream v3-import).
        let usage_row = V4Write::Usage {
            row: V4UsageRow {
                id: generate_id(&|_| false),
                seq,
                usage: index.imported_usage.clone(),
                entry_id: None,
                adjustment: true,
                details: Some(serde_json::json!({"source": "v3-import"})),
            },
        };
        out.write_all(render_out_line(&usage_row)?.as_bytes())?;
        out.write_all(b"\n")?;
        out.sync_all()?;
        Ok::<(), V4Error>(())
    })();

    // Publish over the original (temp + rename, same-directory).
    let result = write_result.and_then(|()| {
        rename_over(&tmp, path)?;
        set_file_private(path);
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    });
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(report)
}

/// Atomically replace `path` with the staged `tmp` file.
#[cfg(not(windows))]
fn rename_over(tmp: &Path, path: &Path) -> Result<(), V4Error> {
    // POSIX rename over an existing file is atomic; a failure leaves the
    // original untouched (and the staged tmp is cleaned up by the caller).
    std::fs::rename(tmp, path)?;
    Ok(())
}
/// Atomically replace `path` with the staged `tmp` file.
#[cfg(windows)]
fn rename_over(tmp: &Path, path: &Path) -> Result<(), V4Error> {
    if let Err(e) = std::fs::rename(tmp, path) {
        // Windows cannot rename over an existing file: remove the
        // original, then retry. The `.bak` copy made before pass 2 keeps
        // this (tiny) crash window recoverable.
        std::fs::remove_file(path)?;
        std::fs::rename(tmp, path).map_err(|_| V4Error::Io(e))?;
    }
    Ok(())
}

/// Build a payload-preserving custom entry for a record that cannot
/// materialize as a typed entry (unknown/extension record types, and
/// records whose payload fails the typed schema): `custom_type` is the
/// original record type and every non-structural field is kept in `data`.
fn payload_preserving_entry(
    value: &Value,
    indexed: &IndexEntry,
    index: &MigrationIndex,
    fallback_ts: u64,
) -> Result<V4Entry, V4Error> {
    let record_type = json_str(value, "type").unwrap_or("unknown");
    let mut payload = value.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("type");
        obj.remove("id");
        obj.remove("parentId");
        obj.remove("timestamp");
    }
    let timestamp = json_str(value, "timestamp")
        .and_then(lenient_timestamp_millis)
        .unwrap_or(fallback_ts);
    Ok(V4Entry::Custom {
        base: V4EntryBase {
            id: indexed
                .mapped_id
                .clone()
                .ok_or_else(|| V4Error::MissingLegacyReference(indexed.legacy_id.clone()))?,
            parent_id: index.resolve(indexed.parent_id.as_deref())?,
            seq: indexed.seq,
            timestamp,
        },
        custom_type: record_type.to_string(),
        data: Some(payload),
    })
}

/// Parse a retained record into a `SessionEntry` (pass 2 only decodes
/// retained records — discarded payloads are never materialized).
/// `None` when the payload no longer fits the typed schema; the caller
/// preserves it as a payload-carrying custom entry instead of aborting
/// the migration (mirrors `SessionLine::Unknown`'s preserve-don't-crash
/// rule and upstream's verbatim pass-through).
fn parse_retained_entry(value: &Value) -> Option<SessionEntry> {
    let mut value = value.clone();
    if value.get("type").and_then(Value::as_str) == Some("message")
        && let Some(message) = value.get_mut("message")
        && message.get("role").and_then(Value::as_str) == Some("hookMessage")
    {
        message["role"] = Value::String("custom".to_string());
    }
    serde_json::from_value(value).ok()
}
