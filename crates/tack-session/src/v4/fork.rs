//! Named-branch and tree forks over v4 storage (WP08 semantics). Port of
//! upstream `jsonl/fork.ts` (`runJsonlFork`), sourcing from the store's
//! in-memory current state — the equivalent of upstream's memory-backend
//! path — while applying the exact same closed classifier
//! ([`crate::fork_policy`]).

use std::collections::HashSet;
use std::path::Path;

use super::store::{V4Store, write_atomic};
use super::types::{ForkOptions, V4Header, V4ListOp, V4ValueOp, V4Write};
use super::{V4Error, codec};
use crate::fork_policy::{
    ForkCurrentStatePlan, ForkStateWrite, project_fork_current_state_write, select_branch_fork,
};

/// Fork `source` into a new v4 file at `destination_path` and open it.
///
/// * Branch scope copies only the named branch's ancestry (which must be a
///   complete configured lane), rewrites its tip, resets its lane state to
///   idle, and excludes all application state.
/// * Tree scope copies the complete immutable tree, every branch tip, all
///   lane configs with fresh idle lane states, and all current application
///   values/lists.
/// * Both scopes exclude usage rows, `tack.result`, `tack.op.*`, `tack.pending.*`,
///   and FAIL on any other reserved `tack`/`tack.*` namespace with current
///   surviving state ([`crate::fork_policy::ForkPolicyError::UnknownReservedNamespace`]).
///
/// Copied writes keep their source sequences; the destination header
/// records the source's `next_seq` high-water mark and
/// `parentSessionId = source.id`, matching upstream `runJsonlFork`. The
/// source is never modified.
pub fn run_v4_fork(
    source: &V4Store,
    destination_path: &Path,
    mut destination_header: V4Header,
    options: &ForkOptions,
) -> Result<V4Store, V4Error> {
    // Plan the fork and compute the copied-entry set.
    let (plan, copied_entries): (ForkCurrentStatePlan, Option<HashSet<String>>) = match options {
        ForkOptions::Tree { .. } => (ForkCurrentStatePlan::Tree, None),
        ForkOptions::Branch {
            branch,
            entry_id,
            position,
            ..
        } => {
            let mut selected: HashSet<String> = HashSet::new();
            let plan = select_branch_fork(
                branch,
                entry_id.as_deref(),
                *position,
                source.branch_tip(branch),
                &|id| source.get_parent(id),
                &mut |id| {
                    selected.insert(id.to_string());
                },
            )?;
            if !source.has_complete_lane(branch) {
                return Err(V4Error::NotAConfiguredLane(branch.clone()));
            }
            (plan, Some(selected))
        }
    };
    let is_entry_copied = |entry_id: &str| match &copied_entries {
        None => true,
        Some(set) => set.contains(entry_id),
    };

    if let Some(id) = match options {
        ForkOptions::Branch { id, .. } | ForkOptions::Tree { id } => id.clone(),
    } {
        destination_header.id = id;
    }
    destination_header.parent_session_id = Some(source.header().id.clone());
    destination_header.next_seq = Some(source.next_seq());

    // Collect projected writes: copied entries plus current state through
    // the closed classifier, all ordered by their preserved source seq.
    let mut out: Vec<V4Write> = Vec::new();
    for entry in source.entries() {
        if is_entry_copied(entry.id()) {
            out.push(V4Write::Entry {
                entry: entry.clone(),
            });
        }
    }
    for (seq, namespace, key, value) in source.current_values() {
        let projected = project_fork_current_state_write(
            &ForkStateWrite::ValueSet {
                namespace: namespace.clone(),
                key: key.clone(),
                value: value.clone(),
            },
            &plan,
            &is_entry_copied,
        )?;
        if let Some(ForkStateWrite::ValueSet {
            namespace,
            key,
            value,
        }) = projected
        {
            out.push(V4Write::Value {
                op: V4ValueOp::Set {
                    seq,
                    namespace,
                    key,
                    value,
                },
            });
        }
    }
    for (seq, namespace, key, value) in source.surviving_list_elements() {
        let projected = project_fork_current_state_write(
            &ForkStateWrite::ListAppend {
                namespace: namespace.clone(),
                key: key.clone(),
                value: value.clone(),
            },
            &plan,
            &is_entry_copied,
        )?;
        if let Some(ForkStateWrite::ListAppend {
            namespace,
            key,
            value,
        }) = projected
        {
            out.push(V4Write::List {
                op: V4ListOp::Append {
                    seq,
                    namespace,
                    key,
                    value,
                },
            });
        }
    }
    // Replay requires globally monotonic lines; source seqs are unique, so
    // a seq sort yields a valid transaction order (parents precede
    // children because commits assign parents smaller seqs).
    out.sort_by_key(V4Write::seq);

    // Stage and publish atomically (upstream `publishJsonl`); transaction
    // lines encrypt when a session key is installed, like commits do.
    let mut content = String::new();
    content.push_str(&serde_json::to_string(&destination_header).expect("header serializes"));
    content.push('\n');
    for write in &out {
        let line = codec::serialize_transaction(std::slice::from_ref(write));
        let line = if crate::crypto::session_key().is_some() {
            crate::crypto::encrypt_line(&line).ok_or(V4Error::EncryptionFailed)?
        } else {
            line
        };
        content.push_str(&line);
        content.push('\n');
    }
    if let Some(parent) = destination_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(destination_path, &content)?;
    V4Store::open(destination_path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fork_policy::{ForkPolicyError, ForkPosition};
    use crate::v4::types::{LaneConfiguration, LaneModel, V4NewWrite};
    use serde_json::Value;

    fn lane_config() -> LaneConfiguration {
        LaneConfiguration {
            model: LaneModel {
                provider: "anthropic".to_string(),
                model_id: "claude".to_string(),
            },
            thinking_level: "high".to_string(),
            active_tool_names: vec!["read".to_string()],
        }
    }

    fn source_store(dir: &Path) -> V4Store {
        let mut store = V4Store::create(
            &dir.join("src.jsonl"),
            V4Header::new("src".to_string(), "/work".to_string()),
            vec![],
        )
        .unwrap();
        store.create_lane("main", lane_config()).unwrap();
        store
            .append_message("main", tack_agent_core::AgentMessage::user("root"))
            .unwrap();
        store
            .append_message("main", tack_agent_core::AgentMessage::user("child"))
            .unwrap();
        store
    }

    #[test]
    fn tree_fork_copies_tree_lanes_and_app_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = source_store(tmp.path());
        source.set_session_name("demo").unwrap();
        source
            .commit(vec![V4NewWrite::ValueSet {
                namespace: "my-app.state".to_string(),
                key: "k".to_string(),
                value: Value::from(1),
            }])
            .unwrap();
        let dest_path = tmp.path().join("dest.jsonl");
        let dest = run_v4_fork(
            &source,
            &dest_path,
            V4Header::new("dest".to_string(), "/work".to_string()),
            &ForkOptions::Tree { id: None },
        )
        .unwrap();
        assert_eq!(dest.header().parent_session_id.as_deref(), Some("src"));
        assert_eq!(dest.entries().len(), source.entries().len());
        assert_eq!(dest.session_name().as_deref(), Some("demo"));
        assert_eq!(dest.get_value("my-app.state", "k"), Some(&Value::from(1)));
        assert!(dest.has_complete_lane("main"));
        assert_eq!(
            dest.lane_state("main"),
            Some(crate::v4::types::LaneState::idle())
        );
        assert_eq!(dest.branch_tip("main"), source.branch_tip("main"));
        assert_eq!(dest.next_seq(), source.next_seq());
    }

    #[test]
    fn branch_fork_copies_ancestry_only_and_resets_lane() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = source_store(tmp.path());
        let root = source.entries()[0].id().to_string();
        source
            .set_label(&root, Some("root-label".to_string()))
            .unwrap();
        source
            .commit(vec![V4NewWrite::ValueSet {
                namespace: "my-app.state".to_string(),
                key: "k".to_string(),
                value: Value::from(1),
            }])
            .unwrap();

        let dest = run_v4_fork(
            &source,
            &tmp.path().join("dest.jsonl"),
            V4Header::new("dest".to_string(), "/work".to_string()),
            &ForkOptions::Branch {
                branch: "main".to_string(),
                entry_id: Some(root.clone()),
                position: ForkPosition::At,
                id: None,
            },
        )
        .unwrap();
        // Only the root entry was copied.
        assert_eq!(dest.entries().len(), 1);
        assert_eq!(dest.entries()[0].id(), root);
        assert_eq!(dest.branch_tip("main"), Some(Some(root.clone())));
        // Label survived (its entry is copied); app state did not.
        assert_eq!(dest.get_label(&root).as_deref(), Some("root-label"));
        assert_eq!(dest.get_value("my-app.state", "k"), None);
        // Lane: config copied, state fresh idle.
        assert_eq!(dest.lane_config("main"), Some(lane_config()));
        assert_eq!(
            dest.lane_state("main"),
            Some(crate::v4::types::LaneState::idle())
        );
    }

    #[test]
    fn branch_fork_requires_configured_lane() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = source_store(tmp.path());
        // A data-only branch: tip without lane config/state.
        source
            .commit(vec![V4NewWrite::ValueSet {
                namespace: "tack.branch.tip".to_string(),
                key: "raw".to_string(),
                value: Value::Null,
            }])
            .unwrap();
        let err = run_v4_fork(
            &source,
            &tmp.path().join("dest.jsonl"),
            V4Header::new("dest".to_string(), "/work".to_string()),
            &ForkOptions::Branch {
                branch: "raw".to_string(),
                entry_id: None,
                position: ForkPosition::At,
                id: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, V4Error::NotAConfiguredLane(_)), "{err:?}");
    }

    #[test]
    fn unknown_reserved_namespace_fails_fork_with_surviving_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = source_store(tmp.path());
        source
            .commit(vec![V4NewWrite::ValueSet {
                namespace: "tack.future.thing".to_string(),
                key: "k".to_string(),
                value: Value::from(1),
            }])
            .unwrap();
        let err = run_v4_fork(
            &source,
            &tmp.path().join("dest.jsonl"),
            V4Header::new("dest".to_string(), "/work".to_string()),
            &ForkOptions::Tree { id: None },
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                V4Error::ForkPolicy(ForkPolicyError::UnknownReservedNamespace(_))
            ),
            "{err:?}"
        );
        // ... but historical (deleted) writes at that namespace do NOT
        // fail the fork — only current surviving state does.
        source
            .commit(vec![V4NewWrite::ValueDelete {
                namespace: "tack.future.thing".to_string(),
                key: "k".to_string(),
            }])
            .unwrap();
        run_v4_fork(
            &source,
            &tmp.path().join("dest2.jsonl"),
            V4Header::new("dest2".to_string(), "/work".to_string()),
            &ForkOptions::Tree { id: None },
        )
        .unwrap();
    }

    #[test]
    fn usage_and_operation_state_are_never_copied() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = source_store(tmp.path());
        source
            .commit(vec![
                V4NewWrite::Usage {
                    id: "u1".to_string(),
                    usage: tack_ai::Usage::zero(),
                    entry_id: None,
                    adjustment: false,
                    details: None,
                },
                V4NewWrite::ValueSet {
                    namespace: "tack.result".to_string(),
                    key: "op-1".to_string(),
                    value: Value::Null,
                },
                V4NewWrite::ValueSet {
                    namespace: "tack.op.meta".to_string(),
                    key: "op-1".to_string(),
                    value: Value::Null,
                },
            ])
            .unwrap();
        let dest = run_v4_fork(
            &source,
            &tmp.path().join("dest.jsonl"),
            V4Header::new("dest".to_string(), "/work".to_string()),
            &ForkOptions::Tree { id: None },
        )
        .unwrap();
        assert!(dest.usage_rows().is_empty());
        assert_eq!(dest.get_value("tack.result", "op-1"), None);
        assert_eq!(dest.get_value("tack.op.meta", "op-1"), None);
    }
}
