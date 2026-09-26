//! Format-4 storage types, field-aligned with upstream TS pi
//! `packages/agent/src/harness/session/jsonl/types.ts`, `commit.ts`,
//! `types.ts` (Entry, LaneConfiguration, LaneState, ForkOptions).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tack_agent_core::AgentMessage;
use tack_ai::Usage;

use crate::fork_policy::ForkPosition;

/// JSONL format version written in the v4 header (`v: 4`).
pub const V4_FORMAT_VERSION: u32 = 4;
/// Storage layout version (`storageVersion: 1`); anything else is rejected.
pub const V4_STORAGE_VERSION: u32 = 1;

/// The v4 storage header (line 1 of every v4 session file). Field names
/// match upstream `JsonlStorageHeader` byte-for-byte.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct V4Header {
    /// Format version; always [`V4_FORMAT_VERSION`].
    pub v: u32,
    /// Record kind; always `"header"`.
    pub kind: String,
    /// Session id.
    pub id: String,
    /// Storage layout version; always [`V4_STORAGE_VERSION`].
    #[serde(rename = "storageVersion")]
    pub storage_version: u32,
    /// Creation time, milliseconds since the Unix epoch.
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    /// Working directory the session belongs to.
    pub cwd: String,
    /// Id of the session this one was forked from, if any.
    #[serde(rename = "parentSessionId", skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// v3 fallback: the parent session's file path when its id could not
    /// be resolved during migration.
    #[serde(
        rename = "legacyParentSessionPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub legacy_parent_session_path: Option<String>,
    /// Sequence high-water mark, rewritten by snapshot-style rewrites
    /// (migration, forks).
    #[serde(rename = "nextSeq", skip_serializing_if = "Option::is_none")]
    pub next_seq: Option<u64>,
}

impl V4Header {
    /// A fresh v4 header for a new session (created now, storage v1).
    pub fn new(id: String, cwd: String) -> Self {
        V4Header {
            v: V4_FORMAT_VERSION,
            kind: "header".to_string(),
            id,
            storage_version: V4_STORAGE_VERSION,
            created_at: tack_ai::now_millis(),
            cwd,
            parent_session_id: None,
            legacy_parent_session_path: None,
            next_seq: None,
        }
    }
}

/// Structural fields every v4 entry carries (upstream `EntryBase`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct V4EntryBase {
    /// Entry id (unique within the session).
    pub id: String,
    /// Parent entry id; `None` is the tree root.
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    /// Session-wide sequence number assigned at commit (≥ 1).
    pub seq: u64,
    /// Commit time, milliseconds since the Unix epoch.
    pub timestamp: u64,
}

/// One immutable session entry (upstream `Entry`: message / compaction /
/// branch_summary / custom). Field names match the upstream JSON.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum V4Entry {
    /// A conversation message (user/assistant/toolResult/custom/...).
    #[serde(rename = "message")]
    Message {
        #[serde(flatten)]
        base: V4EntryBase,
        /// The message payload.
        message: AgentMessage,
        /// Upstream `terminate?: true` marker (lane-ending message).
        #[serde(skip_serializing_if = "Option::is_none")]
        terminate: Option<bool>,
    },
    /// A compaction checkpoint with a materialized retained tail.
    #[serde(rename = "compaction")]
    Compaction {
        #[serde(flatten)]
        base: V4EntryBase,
        /// The compaction summary text.
        summary: String,
        /// Self-contained checkpoint: materialized kept messages.
        #[serde(rename = "retainedTail")]
        retained_tail: Vec<AgentMessage>,
        /// Token count of the context before compaction.
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        /// Extension payload.
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        /// LLM usage of the compaction pass itself.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// True when produced by a hook rather than the agent loop.
        #[serde(rename = "fromHook")]
        from_hook: bool,
    },
    /// Summary of a branch abandoned by tree/fork navigation.
    #[serde(rename = "branch_summary")]
    BranchSummary {
        #[serde(flatten)]
        base: V4EntryBase,
        /// Entry the summary branches from; `None` is the tree root
        /// (v3 encoded this as the `"root"` sentinel).
        #[serde(rename = "fromId")]
        from_id: Option<String>,
        /// The summary text.
        summary: String,
        /// Extension payload.
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        /// LLM usage of the summarization pass.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// True when produced by a hook.
        #[serde(rename = "fromHook")]
        from_hook: bool,
    },
    /// Application-defined entry.
    #[serde(rename = "custom")]
    Custom {
        #[serde(flatten)]
        base: V4EntryBase,
        /// Application-defined type tag.
        #[serde(rename = "customType")]
        custom_type: String,
        /// Application-defined payload.
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
}

impl V4Entry {
    /// A message entry placeholder for [`crate::v4::V4Store::commit`]:
    /// `seq`/`timestamp`/`parentId` are assigned when committed.
    pub fn new_message(id: String, message: AgentMessage) -> Self {
        V4Entry::Message {
            base: V4EntryBase {
                id,
                parent_id: None,
                seq: 0,
                timestamp: 0,
            },
            message,
            terminate: None,
        }
    }

    /// A custom entry placeholder for [`crate::v4::V4Store::commit`].
    pub fn new_custom(id: String, custom_type: String, data: Option<Value>) -> Self {
        V4Entry::Custom {
            base: V4EntryBase {
                id,
                parent_id: None,
                seq: 0,
                timestamp: 0,
            },
            custom_type,
            data,
        }
    }

    /// The entry's id.
    pub fn id(&self) -> &str {
        &self.base().id
    }

    /// The entry's parent id (`None` = tree root).
    pub fn parent_id(&self) -> Option<&str> {
        self.base().parent_id.as_deref()
    }

    /// The entry's assigned sequence (0 before commit).
    pub fn seq(&self) -> u64 {
        self.base().seq
    }

    /// The structural base fields.
    pub fn base(&self) -> &V4EntryBase {
        match self {
            V4Entry::Message { base, .. }
            | V4Entry::Compaction { base, .. }
            | V4Entry::BranchSummary { base, .. }
            | V4Entry::Custom { base, .. } => base,
        }
    }

    /// Mutable access to the structural base fields (commit assigns
    /// `seq`/`timestamp`/`parentId` through this).
    pub fn base_mut(&mut self) -> &mut V4EntryBase {
        match self {
            V4Entry::Message { base, .. }
            | V4Entry::Compaction { base, .. }
            | V4Entry::BranchSummary { base, .. }
            | V4Entry::Custom { base, .. } => base,
        }
    }
}

/// A usage ledger row (upstream `UsageRow`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct V4UsageRow {
    /// Row id (unique within the session).
    pub id: String,
    /// Session-wide sequence number (≥ 1).
    pub seq: u64,
    /// Token/cost usage.
    pub usage: Usage,
    /// Entry this usage is attributed to, if any.
    #[serde(rename = "entryId", skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// True for ledger corrections (e.g. the v3-import row) rather than
    /// fresh consumption.
    pub adjustment: bool,
    /// Extension payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// One committed write — the unit of the v4 transaction log (upstream
/// `CommittedWrite`). A file line is one write or an array of writes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
#[allow(clippy::large_enum_variant)]
pub enum V4Write {
    /// An immutable entry insert.
    #[serde(rename = "entry")]
    Entry {
        /// The entry payload (carries its own `seq`/`timestamp`).
        #[serde(flatten)]
        entry: V4Entry,
    },
    /// A usage ledger insert.
    #[serde(rename = "usage")]
    Usage {
        /// The usage row.
        #[serde(flatten)]
        row: V4UsageRow,
    },
    /// A scalar value write.
    #[serde(rename = "value")]
    Value {
        /// Set or delete the addressed scalar.
        #[serde(flatten)]
        op: V4ValueOp,
    },
    /// A list write.
    #[serde(rename = "list")]
    List {
        /// Append one element or delete the whole list.
        #[serde(flatten)]
        op: V4ListOp,
    },
}

impl V4Write {
    /// The write's session-wide sequence number.
    pub fn seq(&self) -> u64 {
        match self {
            V4Write::Entry { entry } => entry.seq(),
            V4Write::Usage { row } => row.seq,
            V4Write::Value { op } => op.seq(),
            V4Write::List { op } => op.seq(),
        }
    }
}

/// Scalar value operation (upstream `CommittedValueSetWrite` /
/// `CommittedValueDeleteWrite`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum V4ValueOp {
    /// Set the addressed scalar.
    #[serde(rename = "set")]
    Set {
        /// Sequence number.
        seq: u64,
        /// Value namespace (e.g. `tack.branch.tip`, or application-owned).
        namespace: String,
        /// Key within the namespace ("" for singletons).
        key: String,
        /// New value.
        value: Value,
    },
    /// Delete the addressed scalar.
    #[serde(rename = "delete")]
    Delete {
        /// Sequence number.
        seq: u64,
        /// Value namespace.
        namespace: String,
        /// Key within the namespace.
        key: String,
    },
}

impl V4ValueOp {
    /// The operation's sequence number.
    pub fn seq(&self) -> u64 {
        match self {
            V4ValueOp::Set { seq, .. } | V4ValueOp::Delete { seq, .. } => *seq,
        }
    }
}

/// List operation (upstream `CommittedListAppendWrite` /
/// `CommittedListDeleteWrite`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum V4ListOp {
    /// Append one element.
    #[serde(rename = "append")]
    Append {
        /// Sequence number.
        seq: u64,
        /// List namespace.
        namespace: String,
        /// Key within the namespace.
        key: String,
        /// Element value.
        value: Value,
    },
    /// Delete the whole list (surviving elements are those appended
    /// after the last delete).
    #[serde(rename = "delete")]
    Delete {
        /// Sequence number.
        seq: u64,
        /// List namespace.
        namespace: String,
        /// Key within the namespace.
        key: String,
    },
}

impl V4ListOp {
    /// The operation's sequence number.
    pub fn seq(&self) -> u64 {
        match self {
            V4ListOp::Append { seq, .. } | V4ListOp::Delete { seq, .. } => *seq,
        }
    }
}

/// An uncommitted write supplied to [`crate::v4::V4Store::commit`]
/// (upstream `Write`); the store assigns `seq` (and entry `timestamp`).
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum V4NewWrite {
    /// Insert an entry; its `seq`/`timestamp` fields are overwritten and
    /// `parentId` is taken as given (use
    /// [`crate::v4::V4Store::append_entry`] for tip-relative appends).
    Entry(V4Entry),
    /// Insert a usage ledger row.
    Usage {
        /// Row id.
        id: String,
        /// Token/cost usage.
        usage: Usage,
        /// Attributed entry, if any.
        entry_id: Option<String>,
        /// Ledger correction flag.
        adjustment: bool,
        /// Extension payload.
        details: Option<Value>,
    },
    /// Set a scalar value.
    ValueSet {
        /// Namespace.
        namespace: String,
        /// Key.
        key: String,
        /// New value.
        value: Value,
    },
    /// Delete a scalar value.
    ValueDelete {
        /// Namespace.
        namespace: String,
        /// Key.
        key: String,
    },
    /// Append one list element.
    ListAppend {
        /// Namespace.
        namespace: String,
        /// Key.
        key: String,
        /// Element value.
        value: Value,
    },
    /// Delete a whole list.
    ListDelete {
        /// Namespace.
        namespace: String,
        /// Key.
        key: String,
    },
}

/// The outcome of a successful commit (upstream `CommitResult` minus the
/// stats, which tack computes on demand).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V4CommitResult {
    /// Sequence of the first write in the transaction.
    pub first_seq: u64,
    /// One sequence per committed write, in order.
    pub seqs: Vec<u64>,
    /// Commit timestamp (epoch millis).
    pub timestamp: u64,
}

/// Model selection inside a [`LaneConfiguration`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LaneModel {
    /// Provider id (e.g. "anthropic").
    pub provider: String,
    /// Model id within the provider.
    #[serde(rename = "modelId")]
    pub model_id: String,
}

/// Agent configuration of one lane (upstream `LaneConfiguration`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LaneConfiguration {
    /// Model selection.
    pub model: LaneModel,
    /// Thinking level ("off", "low", "medium", "high", ...).
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
    /// Enabled tool names.
    #[serde(rename = "activeToolNames")]
    pub active_tool_names: Vec<String>,
}

/// Operation state of one lane (upstream `LaneState`). tack never runs
/// operations, so forks and lane creation always use
/// [`LaneState::idle`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LaneState {
    /// Currently running operation, if any.
    #[serde(rename = "currentOperationId")]
    pub current_operation_id: Option<String>,
    /// Most recent completed operation, if any.
    #[serde(rename = "lastOperationId")]
    pub last_operation_id: Option<String>,
    /// Deferred entry ids awaiting checkpoint placement.
    pub inbox: Vec<Value>,
}

impl LaneState {
    /// Fresh idle state: no current/last operation, empty inbox.
    pub fn idle() -> Self {
        LaneState {
            current_operation_id: None,
            last_operation_id: None,
            inbox: Vec::new(),
        }
    }
}

/// Fork request, mirroring upstream `ForkOptions`: `scope` is mandatory,
/// branch scope requires a branch name; `position` defaults to `At`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkOptions {
    /// Copy only one named branch (which must be a complete configured
    /// lane) and its ancestry.
    Branch {
        /// Source branch name.
        branch: String,
        /// Entry to fork at; `None` = the current branch tip.
        entry_id: Option<String>,
        /// Cut at the entry or before it.
        position: ForkPosition,
        /// Explicit destination session id (overrides the header's).
        id: Option<String>,
    },
    /// Copy the complete immutable tree, all branches/lanes, and all
    /// current application state.
    Tree {
        /// Explicit destination session id (overrides the header's).
        id: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Field-level alignment with upstream JsonlStorageHeader: exact key
    /// names, optional fields omitted when absent.
    #[test]
    fn header_json_shape_matches_upstream() {
        let header = V4Header::new("s1".to_string(), "/work".to_string());
        let json = serde_json::to_value(&header).unwrap();
        assert_eq!(json["v"], 4);
        assert_eq!(json["kind"], "header");
        assert_eq!(json["id"], "s1");
        assert_eq!(json["storageVersion"], 1);
        assert_eq!(json["cwd"], "/work");
        assert!(json["createdAt"].is_u64());
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("parentSessionId"));
        assert!(!obj.contains_key("legacyParentSessionPath"));
        assert!(!obj.contains_key("nextSeq"));
    }

    /// Write JSON shapes match upstream CommittedWrite: kind/op tags,
    /// flattened entry fields.
    #[test]
    fn write_json_shapes_match_upstream() {
        let entry = V4Entry::Message {
            base: V4EntryBase {
                id: "e1".to_string(),
                parent_id: None,
                seq: 1,
                timestamp: 42,
            },
            message: AgentMessage::user("hi"),
            terminate: None,
        };
        let write = V4Write::Entry { entry };
        let json = serde_json::to_value(&write).unwrap();
        assert_eq!(json["kind"], "entry");
        assert_eq!(json["type"], "message");
        assert_eq!(json["id"], "e1");
        assert_eq!(json["parentId"], serde_json::Value::Null);
        assert_eq!(json["seq"], 1);
        assert_eq!(json["timestamp"], 42);
        assert!(json["message"].is_object());

        let set = V4Write::Value {
            op: V4ValueOp::Set {
                seq: 2,
                namespace: "tack.branch.tip".to_string(),
                key: "main".to_string(),
                value: Value::String("e1".to_string()),
            },
        };
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "value", "op": "set", "seq": 2,
                "namespace": "tack.branch.tip", "key": "main", "value": "e1",
            })
        );

        // Roundtrip through deserialize.
        let parsed: V4Write = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, set);
    }

    /// A compaction entry round-trips with upstream field names.
    #[test]
    fn compaction_entry_field_names() {
        let entry = V4Entry::Compaction {
            base: V4EntryBase {
                id: "c1".to_string(),
                parent_id: Some("e1".to_string()),
                seq: 3,
                timestamp: 100,
            },
            summary: "sum".to_string(),
            retained_tail: vec![],
            tokens_before: 12_000,
            details: None,
            usage: None,
            from_hook: false,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "compaction");
        assert!(json.get("retainedTail").is_some());
        assert_eq!(json["tokensBefore"], 12_000);
        assert_eq!(json["fromHook"], false);
        let parsed: V4Entry = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, entry);
    }
}
