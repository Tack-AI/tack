//! `V4Store`: format-4 JSONL session storage. Port of the core of
//! upstream `jsonl/storage.ts` (`JsonlStorage`): create/open, validated
//! commits appended as transaction lines, current-state queries, branch
//! and lane semantics, torn-tail repair, and transparent legacy v3
//! migration on open.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tack_agent_core::AgentMessage;

use super::codec::{self, ParsedSessionHeader};
use super::types::{
    LaneConfiguration, LaneState, V4_STORAGE_VERSION, V4CommitResult, V4Entry, V4Header, V4ListOp,
    V4NewWrite, V4UsageRow, V4ValueOp, V4Write,
};
use crate::context::generate_id;
use crate::fork_policy::{
    NS_BRANCH_TIP, NS_ENTRY_LABEL, NS_LANE_CONFIG, NS_LANE_STATE, NS_SESSION_NAME,
};

/// Errors from v4 storage operations.
#[derive(Debug, thiserror::Error)]
pub enum V4Error {
    /// Filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The file has no recognizable v4/v3 header on line 1.
    #[error("invalid JSONL storage {0}: missing or unsupported header")]
    MissingHeader(PathBuf),
    /// The header's `storageVersion` is not supported.
    #[error("session {id} uses unsupported storage version {version}")]
    UnsupportedStorageVersion { id: String, version: u32 },
    /// A transaction line failed to parse or validate.
    #[error("invalid JSONL storage {path}: line {line}: {reason}")]
    CorruptLine {
        path: PathBuf,
        line: usize,
        reason: String,
    },
    /// The file carries encrypted lines that cannot be decrypted (no key,
    /// wrong key, or tampered ciphertext) — fail loudly rather than
    /// silently dropping history (mirrors `SessionError::Encrypted`).
    #[error(
        "session file contains undecryptable encrypted entries (session key missing or wrong?): {0}"
    )]
    Encrypted(PathBuf),
    /// At-rest encryption of a transaction line failed; the line was NOT
    /// written (never degrade to plaintext silently).
    #[error("session entry encryption failed; transaction not written")]
    EncryptionFailed,
    /// A commit or replayed transaction reuses an entry/usage id.
    #[error("duplicate entry or usage id: {0}")]
    DuplicateId(String),
    /// An entry references a parent that does not exist.
    #[error("missing parent entry: {0}")]
    MissingParentEntry(String),
    /// Replayed sequences are not monotonic.
    #[error("non-monotonic storage sequence: {0}")]
    NonMonotonicSeq(u64),
    /// Fork planning or namespace projection failure.
    #[error(transparent)]
    ForkPolicy(#[from] crate::fork_policy::ForkPolicyError),
    /// Branch-scope fork of a branch that is not a complete configured
    /// lane (tip + config + state), upstream "is not a configured
    /// AgentLane".
    #[error("source branch {0:?} is not a configured AgentLane")]
    NotAConfiguredLane(String),
    /// The named branch has no `tack.branch.tip` row.
    #[error("unknown branch: {0:?}")]
    UnknownBranch(String),
    /// Legacy v3 record with a dangling or forward parent reference.
    #[error("legacy v3 entry {id} has a missing or forward parent at line {line}: {parent}")]
    MissingLegacyParent {
        id: String,
        line: usize,
        parent: String,
    },
    /// Legacy v3 record with an unresolvable cross-reference.
    #[error("missing legacy v3 entry reference: {0}")]
    MissingLegacyReference(String),
    /// Legacy v3 compaction whose `firstKeptEntryId` is not an ancestor.
    #[error("legacy v3 compaction {id} firstKeptEntryId is not on its parent branch: {first_kept}")]
    CompactionBoundaryNotOnBranch { id: String, first_kept: String },
}

/// In-memory current state folded from the transaction log (upstream
/// `InMemoryStorageState`). Values/lists hold only CURRENT rows; entries
/// are immutable and kept in sequence order.
#[derive(Debug, Default)]
struct V4State {
    /// Entries in commit order.
    entries: Vec<V4Entry>,
    /// id → position in `entries`.
    entry_index: HashMap<String, usize>,
    /// Usage ledger rows in commit order.
    usage_rows: Vec<V4UsageRow>,
    /// Id set mirroring `usage_rows` (append-only, like the ledger
    /// itself): `has_entry_or_usage_id` is called for EVERY committed
    /// entry/usage write, and a linear scan of the ledger made session
    /// lifetimes O(n²).
    usage_ids: HashSet<String>,
    /// Current scalar values: namespace → key → (seq, value). Nested maps
    /// (rather than one map keyed by `(String, String)`) so lookups take
    /// borrowed `&str` keys with zero allocation — `get_value`/`read_list`
    /// are hot paths and the tuple key forced two `String` allocations per
    /// query.
    values: HashMap<String, HashMap<String, (u64, Value)>>,
    /// Surviving list elements: namespace → key → elements in order.
    lists: HashMap<String, HashMap<String, Vec<(u64, Value)>>>,
    /// Next unassigned sequence number.
    next_seq: u64,
}

impl V4State {
    fn new() -> Self {
        V4State {
            next_seq: 1,
            ..V4State::default()
        }
    }

    fn has_entry_or_usage_id(&self, id: &str) -> bool {
        self.entry_index.contains_key(id) || self.usage_ids.contains(id)
    }

    fn get_parent(&self, entry_id: &str) -> Option<Option<String>> {
        self.entry_index
            .get(entry_id)
            .map(|&i| self.entries[i].base().parent_id.clone())
    }
}

/// Validate a batch of committed writes against current state (upstream
/// `validateCommittedWrites`): monotonic seqs, unique ids, existing
/// parents (parents inside the same transaction are allowed).
fn validate_committed(writes: &[V4Write], state: &V4State) -> Result<(), V4Error> {
    let mut previous_seq = state.next_seq.saturating_sub(1);
    let mut txn_ids: HashSet<&str> = HashSet::new();
    let mut txn_entry_ids: HashSet<&str> = HashSet::new();
    for write in writes {
        if write.seq() <= previous_seq {
            return Err(V4Error::NonMonotonicSeq(write.seq()));
        }
        previous_seq = write.seq();
        let (id, parent) = match write {
            V4Write::Entry { entry } => (entry.id(), entry.parent_id()),
            V4Write::Usage { row } => (row.id.as_str(), None),
            _ => continue,
        };
        if state.has_entry_or_usage_id(id) || !txn_ids.insert(id) {
            return Err(V4Error::DuplicateId(id.to_string()));
        }
        if let Some(parent) = parent
            && !state.entry_index.contains_key(parent)
            && !txn_entry_ids.contains(parent)
        {
            return Err(V4Error::MissingParentEntry(parent.to_string()));
        }
        if let V4Write::Entry { .. } = write {
            txn_entry_ids.insert(id);
        }
    }
    Ok(())
}

/// A format-4 JSONL session store. All writes go through
/// [`V4Store::commit`], which appends one transaction line; reads fold
/// the log into current state in memory.
#[derive(Debug)]
pub struct V4Store {
    path: PathBuf,
    header: V4Header,
    state: V4State,
    was_legacy_v3: bool,
}

impl V4Store {
    /// Create a new v4 session file (fails nothing if the file exists —
    /// like upstream, the header is published atomically, replacing any
    /// staged temp file).
    pub fn create(
        path: &Path,
        header: V4Header,
        initial_writes: Vec<V4NewWrite>,
    ) -> Result<Self, V4Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut store = V4Store {
            path: path.to_path_buf(),
            header,
            state: V4State::new(),
            was_legacy_v3: false,
        };
        let committed = store.prepare_commit(initial_writes)?;
        let mut content = String::new();
        content.push_str(&serde_json::to_string(&store.header).expect("header serializes"));
        content.push('\n');
        if !committed.is_empty() {
            // Encrypt the initial transaction too when a session key is
            // installed (the append path does; initial values can carry
            // sensitive payloads such as a session name).
            let line = codec::serialize_transaction(&committed);
            let out = if crate::crypto::session_key().is_some() {
                crate::crypto::encrypt_line(&line).ok_or(V4Error::EncryptionFailed)?
            } else {
                line
            };
            content.push_str(&out);
            content.push('\n');
        }
        write_atomic(path, &content)?;
        let writes = committed;
        store.apply_validated(writes);
        Ok(store)
    }

    /// Open a v4 session file, transparently migrating a legacy v3 file
    /// first (streaming, with a `.bak` backup; see
    /// [`super::migrate::migrate_v3_to_v4`]). A torn final line is
    /// truncated and the file rewritten, mirroring upstream `openV4`.
    pub fn open(path: &Path) -> Result<Self, V4Error> {
        let first_line = read_first_line(path)?;
        let Some(first_line) = first_line else {
            return Err(V4Error::MissingHeader(path.to_path_buf()));
        };
        match codec::parse_session_header(&first_line) {
            Some(ParsedSessionHeader::V4(header)) => Self::open_v4(path, header, false),
            Some(ParsedSessionHeader::LegacyV3(_)) => {
                super::migrate::migrate_v3_to_v4(path)?;
                let first_line = read_first_line(path)?
                    .ok_or_else(|| V4Error::MissingHeader(path.to_path_buf()))?;
                match codec::parse_session_header(&first_line) {
                    Some(ParsedSessionHeader::V4(header)) => Self::open_v4(path, header, true),
                    _ => Err(V4Error::MissingHeader(path.to_path_buf())),
                }
            }
            None => Err(V4Error::MissingHeader(path.to_path_buf())),
        }
    }

    fn open_v4(path: &Path, header: V4Header, was_legacy_v3: bool) -> Result<Self, V4Error> {
        if header.storage_version != V4_STORAGE_VERSION {
            return Err(V4Error::UnsupportedStorageVersion {
                id: header.id.clone(),
                version: header.storage_version,
            });
        }
        let content = std::fs::read_to_string(path)?;
        if crate::crypto::has_undecryptable_encrypted_line(&content) {
            return Err(V4Error::Encrypted(path.to_path_buf()));
        }
        let torn = !content.is_empty() && !content.ends_with('\n');
        // Upstream `splitCompleteLines`: the torn final segment is
        // discarded BEFORE replay, never parsed.
        let mut lines: Vec<&str> = content.lines().collect();
        if torn {
            lines.pop();
        }
        let mut store = V4Store {
            path: path.to_path_buf(),
            header,
            state: V4State::new(),
            was_legacy_v3,
        };
        for (index, &raw) in lines.iter().enumerate() {
            if index == 0 {
                continue; // header
            }
            let line_number = index + 1;
            let decrypted;
            let line = if crate::crypto::is_encrypted_line(raw.trim()) {
                decrypted = crate::crypto::decrypt_line(raw.trim()).ok_or_else(|| {
                    V4Error::CorruptLine {
                        path: path.to_path_buf(),
                        line: line_number,
                        reason: "undecryptable line".to_string(),
                    }
                })?;
                &decrypted
            } else {
                raw
            };
            let writes = codec::parse_transaction(line).map_err(|reason| V4Error::CorruptLine {
                path: path.to_path_buf(),
                line: line_number,
                reason,
            })?;
            validate_committed(&writes, &store.state).map_err(|e| V4Error::CorruptLine {
                path: path.to_path_buf(),
                line: line_number,
                reason: e.to_string(),
            })?;
            store.apply_validated(writes);
        }
        if let Some(next_seq) = store.header.next_seq
            && next_seq > store.state.next_seq
        {
            store.state.next_seq = next_seq;
        }
        if torn {
            // Drop the unterminated final line (upstream: torn tails are
            // truncated and the file republished).
            let mut rewritten = lines.join("\n");
            rewritten.push('\n');
            write_atomic(path, &rewritten)?;
        }
        Ok(store)
    }

    /// The storage header.
    pub fn header(&self) -> &V4Header {
        &self.header
    }

    /// The backing file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when [`V4Store::open`] migrated this file from legacy v3.
    pub fn was_legacy_v3(&self) -> bool {
        self.was_legacy_v3
    }

    /// The next sequence number a commit will assign.
    pub fn next_seq(&self) -> u64 {
        self.state.next_seq
    }

    /// All entries in commit order.
    pub fn entries(&self) -> &[V4Entry] {
        &self.state.entries
    }

    /// Look up one entry by id.
    pub fn get_entry(&self, id: &str) -> Option<&V4Entry> {
        self.state
            .entry_index
            .get(id)
            .map(|&i| &self.state.entries[i])
    }

    /// The usage ledger rows in commit order.
    pub fn usage_rows(&self) -> &[V4UsageRow] {
        &self.state.usage_rows
    }

    /// Current value at one scalar address.
    pub fn get_value(&self, namespace: &str, key: &str) -> Option<&Value> {
        self.state
            .values
            .get(namespace)
            .and_then(|inner| inner.get(key))
            .map(|(_, v)| v)
    }

    /// All current scalar values in a namespace (key, value) pairs,
    /// ordered by key.
    pub fn scan_values(&self, namespace: &str) -> Vec<(&str, &Value)> {
        let mut out: Vec<(&str, &Value)> = self
            .state
            .values
            .get(namespace)
            .map(|inner| {
                inner
                    .iter()
                    .map(|(key, (_, value))| (key.as_str(), value))
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// Surviving elements of one list, oldest first.
    pub fn read_list(&self, namespace: &str, key: &str) -> Vec<&Value> {
        self.state
            .lists
            .get(namespace)
            .and_then(|inner| inner.get(key))
            .map(|elements| elements.iter().map(|(_, v)| v).collect())
            .unwrap_or_default()
    }

    // --- branch / lane surface (WP06 minimal semantics) -------------------

    /// A branch's current tip: `None` = unknown branch, `Some(None)` =
    /// present but empty (mirrors upstream's `string | null | undefined`).
    pub fn branch_tip(&self, branch: &str) -> Option<Option<String>> {
        self.get_value(NS_BRANCH_TIP, branch).map(|v| match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
    }

    /// All branch names with a `tack.branch.tip` row, sorted.
    pub fn branches(&self) -> Vec<String> {
        self.scan_values(NS_BRANCH_TIP)
            .into_iter()
            .map(|(k, _)| k.to_string())
            .collect()
    }

    /// A lane's configuration, if present.
    pub fn lane_config(&self, lane: &str) -> Option<LaneConfiguration> {
        self.get_value(NS_LANE_CONFIG, lane)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// A lane's operation state, if present.
    pub fn lane_state(&self, lane: &str) -> Option<LaneState> {
        self.get_value(NS_LANE_STATE, lane)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// True when the lane has a tip, a config AND a state row (upstream's
    /// "complete configured AgentLane").
    pub fn has_complete_lane(&self, lane: &str) -> bool {
        self.branch_tip(lane).is_some()
            && self.get_value(NS_LANE_CONFIG, lane).is_some()
            && self.get_value(NS_LANE_STATE, lane).is_some()
    }

    /// Create a lane atomically: branch tip + configuration + idle state
    /// in one transaction (upstream: "supported writers create lane state
    /// atomically").
    pub fn create_lane(
        &mut self,
        name: &str,
        config: LaneConfiguration,
    ) -> Result<V4CommitResult, V4Error> {
        let tip = self
            .branch_tip(name)
            .map(|t| t.map(Value::String).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);
        self.commit(vec![
            V4NewWrite::ValueSet {
                namespace: NS_BRANCH_TIP.to_string(),
                key: name.to_string(),
                value: tip,
            },
            V4NewWrite::ValueSet {
                namespace: NS_LANE_CONFIG.to_string(),
                key: name.to_string(),
                value: serde_json::to_value(config).expect("lane config serializes"),
            },
            V4NewWrite::ValueSet {
                namespace: NS_LANE_STATE.to_string(),
                key: name.to_string(),
                value: serde_json::to_value(LaneState::idle()).expect("lane state serializes"),
            },
        ])
    }

    /// Append an entry extending a branch and move the branch tip, in one
    /// atomic transaction. The entry's parent is set to the current tip.
    pub fn append_entry(&mut self, branch: &str, mut entry: V4Entry) -> Result<String, V4Error> {
        let tip = self
            .branch_tip(branch)
            .ok_or_else(|| V4Error::UnknownBranch(branch.to_string()))?;
        entry.base_mut().parent_id = tip;
        let id = entry.id().to_string();
        self.commit(vec![
            V4NewWrite::Entry(entry),
            V4NewWrite::ValueSet {
                namespace: NS_BRANCH_TIP.to_string(),
                key: branch.to_string(),
                value: Value::String(id.clone()),
            },
        ])?;
        Ok(id)
    }

    /// Append a message to a branch (convenience over
    /// [`V4Store::append_entry`], minting an id).
    pub fn append_message(
        &mut self,
        branch: &str,
        message: AgentMessage,
    ) -> Result<String, V4Error> {
        let id = self.mint_entry_id();
        self.append_entry(branch, V4Entry::new_message(id, message))
    }

    /// Walk a branch from its tip to the root, returned root-first.
    pub fn scan_branch(&self, branch: &str) -> Result<Vec<V4Entry>, V4Error> {
        let tip = self
            .branch_tip(branch)
            .ok_or_else(|| V4Error::UnknownBranch(branch.to_string()))?;
        let mut path = Vec::new();
        let mut current = tip;
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                break; // corrupt cycle: terminate rather than loop
            }
            let Some(entry) = self.get_entry(&id) else {
                return Err(V4Error::MissingParentEntry(id));
            };
            path.push(entry.clone());
            current = entry.base().parent_id.clone();
        }
        path.reverse();
        Ok(path)
    }

    // --- session name / labels --------------------------------------------

    /// The session display name, if set.
    pub fn session_name(&self) -> Option<String> {
        match self.get_value(NS_SESSION_NAME, "") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    }

    /// Set the session display name.
    pub fn set_session_name(&mut self, name: &str) -> Result<V4CommitResult, V4Error> {
        self.commit(vec![V4NewWrite::ValueSet {
            namespace: NS_SESSION_NAME.to_string(),
            key: String::new(),
            value: Value::String(name.to_string()),
        }])
    }

    /// The current label of one entry, if any.
    pub fn get_label(&self, entry_id: &str) -> Option<String> {
        match self.get_value(NS_ENTRY_LABEL, entry_id) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    }

    /// Set or clear (None) an entry's label.
    pub fn set_label(
        &mut self,
        entry_id: &str,
        label: Option<String>,
    ) -> Result<V4CommitResult, V4Error> {
        let write = match label {
            Some(label) => V4NewWrite::ValueSet {
                namespace: NS_ENTRY_LABEL.to_string(),
                key: entry_id.to_string(),
                value: Value::String(label),
            },
            None => V4NewWrite::ValueDelete {
                namespace: NS_ENTRY_LABEL.to_string(),
                key: entry_id.to_string(),
            },
        };
        self.commit(vec![write])
    }

    /// Mint a unique entry id for this session.
    pub fn mint_entry_id(&self) -> String {
        generate_id(&|id| self.state.has_entry_or_usage_id(id))
    }

    // --- commit -------------------------------------------------------------

    /// Validate and append one transaction (upstream `commit`): sequences
    /// and the entry timestamp are assigned, the line is appended
    /// (encrypted when a session key is installed), then applied to
    /// current state. An empty write set is a no-op.
    pub fn commit(&mut self, writes: Vec<V4NewWrite>) -> Result<V4CommitResult, V4Error> {
        let committed = self.prepare_commit(writes)?;
        let result = V4CommitResult {
            first_seq: self.state.next_seq,
            seqs: committed.iter().map(V4Write::seq).collect(),
            timestamp: tack_ai::now_millis(),
        };
        if !committed.is_empty() {
            let line = codec::serialize_transaction(&committed);
            self.append_line(&line)?;
            self.apply_validated(committed);
        }
        Ok(result)
    }

    /// Assign seqs/timestamps and validate, without persisting.
    fn prepare_commit(&self, writes: Vec<V4NewWrite>) -> Result<Vec<V4Write>, V4Error> {
        let timestamp = tack_ai::now_millis();
        let first = self.state.next_seq;
        let committed: Vec<V4Write> = writes
            .into_iter()
            .enumerate()
            .map(|(i, w)| commit_write(w, first + i as u64, timestamp))
            .collect();
        validate_committed(&committed, &self.state)?;
        Ok(committed)
    }

    /// Fold committed writes into current state (upstream
    /// `applyValidated`).
    fn apply_validated(&mut self, writes: Vec<V4Write>) {
        for write in writes {
            self.state.next_seq = self.state.next_seq.max(write.seq() + 1);
            match write {
                V4Write::Entry { entry } => {
                    self.state
                        .entry_index
                        .insert(entry.id().to_string(), self.state.entries.len());
                    self.state.entries.push(entry);
                }
                V4Write::Usage { row } => {
                    self.state.usage_ids.insert(row.id.clone());
                    self.state.usage_rows.push(row);
                }
                V4Write::Value {
                    op:
                        V4ValueOp::Set {
                            namespace,
                            key,
                            value,
                            seq,
                        },
                } => {
                    self.state
                        .values
                        .entry(namespace)
                        .or_default()
                        .insert(key, (seq, value));
                }
                V4Write::Value {
                    op: V4ValueOp::Delete { namespace, key, .. },
                } => {
                    let empty = if let Some(inner) = self.state.values.get_mut(&namespace) {
                        inner.remove(&key);
                        inner.is_empty()
                    } else {
                        false
                    };
                    if empty {
                        self.state.values.remove(&namespace);
                    }
                }
                V4Write::List {
                    op:
                        V4ListOp::Append {
                            namespace,
                            key,
                            value,
                            seq,
                        },
                } => {
                    self.state
                        .lists
                        .entry(namespace)
                        .or_default()
                        .entry(key)
                        .or_default()
                        .push((seq, value));
                }
                V4Write::List {
                    op: V4ListOp::Delete { namespace, key, .. },
                } => {
                    let empty = if let Some(inner) = self.state.lists.get_mut(&namespace) {
                        inner.remove(&key);
                        inner.is_empty()
                    } else {
                        false
                    };
                    if empty {
                        self.state.lists.remove(&namespace);
                    }
                }
            }
        }
    }

    /// Serialize a transaction line for appending: encrypted when a
    /// session key is installed, and an encryption failure is a hard
    /// error (never degrade to plaintext silently — same rule as the v3
    /// append path).
    fn append_line(&self, line: &str) -> Result<(), V4Error> {
        let out = if crate::crypto::session_key().is_some() {
            crate::crypto::encrypt_line(line).ok_or(V4Error::EncryptionFailed)?
        } else {
            line.to_string()
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // ONE write for the whole line (no interleaving window).
        f.write_all(format!("{out}\n").as_bytes())?;
        Ok(())
    }

    // --- internals shared with fork.rs ---------------------------------------

    /// Parent lookup for fork planning (`None` = unknown entry).
    pub(crate) fn get_parent(&self, entry_id: &str) -> Option<Option<String>> {
        self.state.get_parent(entry_id)
    }

    /// Current scalar rows as (seq, namespace, key, value), for forks.
    pub(crate) fn current_values(&self) -> Vec<(u64, String, String, Value)> {
        self.state
            .values
            .iter()
            .flat_map(|(ns, inner)| {
                inner
                    .iter()
                    .map(|(key, (seq, value))| (*seq, ns.clone(), key.clone(), value.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Surviving list elements as (seq, namespace, key, value), for forks.
    pub(crate) fn surviving_list_elements(&self) -> Vec<(u64, String, String, Value)> {
        self.state
            .lists
            .iter()
            .flat_map(|(ns, inner)| {
                inner
                    .iter()
                    .flat_map(|(key, elements)| {
                        elements
                            .iter()
                            .map(|(seq, value)| (*seq, ns.clone(), key.clone(), value.clone()))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// Materialize a committed write (upstream `commitWrite`).
fn commit_write(write: V4NewWrite, seq: u64, timestamp: u64) -> V4Write {
    match write {
        V4NewWrite::Entry(mut entry) => {
            entry.base_mut().seq = seq;
            entry.base_mut().timestamp = timestamp;
            V4Write::Entry { entry }
        }
        V4NewWrite::Usage {
            id,
            usage,
            entry_id,
            adjustment,
            details,
        } => V4Write::Usage {
            row: V4UsageRow {
                id,
                seq,
                usage,
                entry_id,
                adjustment,
                details,
            },
        },
        V4NewWrite::ValueSet {
            namespace,
            key,
            value,
        } => V4Write::Value {
            op: V4ValueOp::Set {
                seq,
                namespace,
                key,
                value,
            },
        },
        V4NewWrite::ValueDelete { namespace, key } => V4Write::Value {
            op: V4ValueOp::Delete {
                seq,
                namespace,
                key,
            },
        },
        V4NewWrite::ListAppend {
            namespace,
            key,
            value,
        } => V4Write::List {
            op: V4ListOp::Append {
                seq,
                namespace,
                key,
                value,
            },
        },
        V4NewWrite::ListDelete { namespace, key } => V4Write::List {
            op: V4ListOp::Delete {
                seq,
                namespace,
                key,
            },
        },
    }
}

/// Read the first line of a file; `None` for an empty file.
fn read_first_line(path: &Path) -> Result<Option<String>, V4Error> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()))
}

/// Write `contents` to `path` via a same-directory temp file + rename
/// (the publish half of upstream `publishFileAtomically`).
pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<(), V4Error> {
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    let write_result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(V4Error::Io(e));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        // std rename does not replace an existing target on Windows.
        std::fs::remove_file(path)?;
        std::fs::rename(&tmp, path).map_err(|_| V4Error::Io(e))?;
    }
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::v4::types::LaneModel;

    fn header(id: &str) -> V4Header {
        V4Header::new(id.to_string(), "/work".to_string())
    }

    #[test]
    fn create_commit_reopen_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let mut store = V4Store::create(&path, header("s1"), vec![]).unwrap();
        let lane = LaneConfiguration {
            model: LaneModel {
                provider: "anthropic".to_string(),
                model_id: "claude".to_string(),
            },
            thinking_level: "high".to_string(),
            active_tool_names: vec![],
        };
        store.create_lane("main", lane.clone()).unwrap();
        let e1 = store
            .append_message("main", AgentMessage::user("one"))
            .unwrap();
        let e2 = store
            .append_message("main", AgentMessage::user("two"))
            .unwrap();
        store.set_session_name("demo").unwrap();
        store.set_label(&e1, Some("marked".to_string())).unwrap();
        store
            .commit(vec![
                V4NewWrite::ListAppend {
                    namespace: "app.log".to_string(),
                    key: String::new(),
                    value: Value::from("x"),
                },
                V4NewWrite::ListAppend {
                    namespace: "app.log".to_string(),
                    key: String::new(),
                    value: Value::from("y"),
                },
            ])
            .unwrap();
        drop(store);

        let store = V4Store::open(&path).unwrap();
        assert!(!store.was_legacy_v3());
        assert_eq!(store.header().id, "s1");
        assert_eq!(store.entries().len(), 2);
        assert_eq!(store.branch_tip("main"), Some(Some(e2.clone())));
        assert_eq!(store.lane_config("main"), Some(lane));
        assert_eq!(store.lane_state("main"), Some(LaneState::idle()));
        assert_eq!(store.session_name().as_deref(), Some("demo"));
        assert_eq!(store.get_label(&e1).as_deref(), Some("marked"));
        assert_eq!(
            store.read_list("app.log", ""),
            vec![&Value::from("x"), &Value::from("y")]
        );
        let path_entries = store.scan_branch("main").unwrap();
        assert_eq!(
            path_entries.iter().map(V4Entry::id).collect::<Vec<_>>(),
            vec![e1.as_str(), e2.as_str()]
        );
        // Sequences resume after the reopen.
        assert!(store.next_seq() > 1);
    }

    #[test]
    fn header_carries_next_seq_high_water_mark() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let mut store = V4Store::create(&path, header("s1"), vec![]).unwrap();
        store.set_session_name("x").unwrap();
        let next = store.next_seq();
        // nextSeq is only stamped by snapshot rewrites (migration/fork),
        // not by appends — the file header keeps nextSeq absent.
        let content = std::fs::read_to_string(&path).unwrap();
        let first = content.lines().next().unwrap();
        assert!(!first.contains("nextSeq"), "{first}");
        let reopened = V4Store::open(&path).unwrap();
        assert_eq!(reopened.next_seq(), next);
    }

    #[test]
    fn commit_validation_rejects_duplicate_ids_and_missing_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let mut store = V4Store::create(&path, header("s1"), vec![]).unwrap();
        store
            .create_lane(
                "main",
                LaneConfiguration {
                    model: LaneModel {
                        provider: "p".to_string(),
                        model_id: "m".to_string(),
                    },
                    thinking_level: "off".to_string(),
                    active_tool_names: vec![],
                },
            )
            .unwrap();
        let e1 = store
            .append_message("main", AgentMessage::user("one"))
            .unwrap();
        // Duplicate id.
        let dup = V4Entry::new_message(e1.clone(), AgentMessage::user("dup"));
        assert!(matches!(
            store.append_entry("main", dup),
            Err(V4Error::DuplicateId(_))
        ));
        // Missing parent: construct an entry whose parent dangles.
        let mut orphan = V4Entry::new_message("orphan".to_string(), AgentMessage::user("x"));
        orphan.base_mut().parent_id = Some("nope".to_string());
        assert!(matches!(
            store.commit(vec![V4NewWrite::Entry(orphan)]),
            Err(V4Error::MissingParentEntry(_))
        ));
    }

    #[test]
    fn torn_final_line_is_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let mut store = V4Store::create(&path, header("s1"), vec![]).unwrap();
        store.set_session_name("kept").unwrap();
        drop(store);
        // Simulate a torn tail.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"kind\":\"value\",\"op\":\"set\",\"seq\":99")
            .unwrap();
        drop(f);
        let store = V4Store::open(&path).unwrap();
        assert_eq!(store.session_name().as_deref(), Some("kept"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.ends_with('\n'));
        assert!(!content.contains("seq\":99"));
    }

    #[test]
    fn value_delete_and_list_delete_fold_current_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let mut store = V4Store::create(&path, header("s1"), vec![]).unwrap();
        store
            .commit(vec![
                V4NewWrite::ValueSet {
                    namespace: "app".to_string(),
                    key: "k".to_string(),
                    value: Value::from(1),
                },
                V4NewWrite::ListAppend {
                    namespace: "app.list".to_string(),
                    key: String::new(),
                    value: Value::from("a"),
                },
            ])
            .unwrap();
        store
            .commit(vec![
                V4NewWrite::ValueDelete {
                    namespace: "app".to_string(),
                    key: "k".to_string(),
                },
                V4NewWrite::ListAppend {
                    namespace: "app.list".to_string(),
                    key: String::new(),
                    value: Value::from("b"),
                },
            ])
            .unwrap();
        assert_eq!(store.get_value("app", "k"), None);
        assert_eq!(
            store.read_list("app.list", ""),
            vec![&Value::from("a"), &Value::from("b")]
        );
        store
            .commit(vec![V4NewWrite::ListDelete {
                namespace: "app.list".to_string(),
                key: String::new(),
            }])
            .unwrap();
        assert!(store.read_list("app.list", "").is_empty());
        // Reopen replays the same folds.
        let reopened = V4Store::open(&path).unwrap();
        assert!(reopened.read_list("app.list", "").is_empty());
        assert_eq!(reopened.get_value("app", "k"), None);
    }
}
