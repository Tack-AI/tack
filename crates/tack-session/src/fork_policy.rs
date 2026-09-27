//! Fork namespace projection rules — the single closed classifier for how
//! current state crosses a fork. Ported function-by-function from upstream
//! TS pi `packages/agent/src/harness/session/fork-policy.ts` (WP08 §1.2):
//! introducing a new built-in `tack.*` namespace without declaring its fork
//! semantics must fail forks, never silently copy or drop state.

use serde_json::Value;

/// Session display name (scalar, key "").
pub const NS_SESSION_NAME: &str = "tack.session.name";
/// Entry labels, keyed by entry id.
pub const NS_ENTRY_LABEL: &str = "tack.entry.label";
/// Branch tips, keyed by branch name; value is the tip entry id or null.
pub const NS_BRANCH_TIP: &str = "tack.branch.tip";
/// Lane configurations, keyed by lane name.
pub const NS_LANE_CONFIG: &str = "tack.lane.config";
/// Lane operation states, keyed by lane name.
pub const NS_LANE_STATE: &str = "tack.lane.state";
/// Terminal operation result records (lane-lived, never forked).
pub const NS_OPERATION_RESULT: &str = "tack.result";
/// Open-operation state prefix (never forked).
pub const PREFIX_OPERATION: &str = "tack.op.";
/// Pending/deferred-write state prefix (never forked).
pub const PREFIX_PENDING: &str = "tack.pending.";

// Upstream TS pi reserved namespaces (same layout under `pi.*`). Tack
// never WRITES these, but a pi-written session can be opened and forked
// through tack: the fork classifier gives them upstream's own semantics
// (`fork-policy.ts`) so a forked pi session stays a clean pi session —
// runtime/operation state never crosses a fork and lane state resets to
// idle. The read-side fallback for pi sessions lives in `V4Store`.
/// Upstream session display name (scalar, key "").
pub const PI_SESSION_NAME: &str = "pi.session.name";
/// Upstream entry labels, keyed by entry id.
pub const PI_ENTRY_LABEL: &str = "pi.entry.label";
/// Upstream branch tips, keyed by branch name.
pub const PI_BRANCH_TIP: &str = "pi.branch.tip";
/// Upstream lane configurations, keyed by lane name.
pub const PI_LANE_CONFIG: &str = "pi.lane.config";
/// Upstream lane operation states, keyed by lane name.
pub const PI_LANE_STATE: &str = "pi.lane.state";
/// Upstream terminal operation result records (never forked).
const PI_OPERATION_RESULT: &str = "pi.result";
/// Upstream open-operation state prefix (never forked).
const PI_PREFIX_OPERATION: &str = "pi.op.";
/// Upstream pending/deferred-write state prefix (never forked).
const PI_PREFIX_PENDING: &str = "pi.pending.";

/// Errors raised by fork planning and namespace projection.
#[derive(Debug, thiserror::Error)]
pub enum ForkPolicyError {
    /// A reserved `tack`/`tack.*` namespace with current surviving state has no
    /// declared fork semantics (upstream: "Unknown reserved fork namespace").
    #[error("unknown reserved fork namespace: {0}")]
    UnknownReservedNamespace(String),
    /// Branch-scope fork naming a branch with no `tack.branch.tip` row.
    #[error("unknown source branch: {0:?}")]
    UnknownBranch(String),
    /// The source entry tree is corrupt (parent link missing mid-walk).
    #[error("corrupt source branch: missing parent {0}")]
    MissingParent(String),
    /// `entry_id` is not on the source branch's tip ancestry.
    #[error("fork entry {entry} is not on source branch {branch:?}")]
    EntryNotOnBranch { entry: String, branch: String },
}

/// Where a branch fork cuts relative to the requested entry (upstream
/// `ForkOptions.position`, default `"at"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ForkPosition {
    /// The destination tip is the requested entry itself.
    #[default]
    At,
    /// The destination tip is the requested entry's parent (may be null).
    Before,
}

/// How current state is projected into the fork destination (upstream
/// `ForkCurrentStatePlan`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkCurrentStatePlan {
    /// Branch scope: only the named branch survives, with a rewritten tip.
    Branch {
        branch: String,
        destination_tip: Option<String>,
    },
    /// Tree scope: the complete immutable tree and all branches survive.
    Tree,
}

/// One current scalar row or surviving list element presented to the fork
/// classifier (upstream `CommittedValueSetWrite | CommittedListAppendWrite`
/// minus the already-validated `seq`).
#[derive(Clone, Debug, PartialEq)]
pub enum ForkStateWrite {
    /// Current value of a scalar address.
    ValueSet {
        namespace: String,
        key: String,
        value: Value,
    },
    /// One surviving element of a list address.
    ListAppend {
        namespace: String,
        key: String,
        value: Value,
    },
}

impl ForkStateWrite {
    /// The write's namespace.
    pub fn namespace(&self) -> &str {
        match self {
            ForkStateWrite::ValueSet { namespace, .. }
            | ForkStateWrite::ListAppend { namespace, .. } => namespace,
        }
    }

    /// The write's key within its namespace.
    pub fn key(&self) -> &str {
        match self {
            ForkStateWrite::ValueSet { key, .. } | ForkStateWrite::ListAppend { key, .. } => key,
        }
    }

    /// Replace the write's value (used for tip rewrites and lane resets).
    fn with_value(self, value: Value) -> Self {
        match self {
            ForkStateWrite::ValueSet { namespace, key, .. } => ForkStateWrite::ValueSet {
                namespace,
                key,
                value,
            },
            ForkStateWrite::ListAppend { namespace, key, .. } => ForkStateWrite::ListAppend {
                namespace,
                key,
                value,
            },
        }
    }
}

/// The fresh idle lane state written to every forked lane (upstream
/// `{ currentOperationId: null, lastOperationId: null, inbox: [] }`).
pub fn idle_lane_state_value() -> Value {
    serde_json::json!({
        "currentOperationId": null,
        "lastOperationId": null,
        "inbox": [],
    })
}

/// Validate a branch-scope fork against the source branch and record the
/// selected ancestry. Port of upstream `selectBranchFork`.
///
/// * `tip`: the source branch tip — `None` models upstream's `undefined`
///   (unknown branch), `Some(None)` a present null tip.
/// * `get_parent`: parent lookup — `None` models upstream's `undefined`
///   (corrupt tree), `Some(None)` the root.
/// * `select_entry`: called for every entry copied to the destination (the
///   destination tip's ancestry, honoring `position`).
pub fn select_branch_fork(
    branch: &str,
    entry_id: Option<&str>,
    position: ForkPosition,
    tip: Option<Option<String>>,
    get_parent: &dyn Fn(&str) -> Option<Option<String>>,
    select_entry: &mut dyn FnMut(&str),
) -> Result<ForkCurrentStatePlan, ForkPolicyError> {
    let tip = tip.ok_or_else(|| ForkPolicyError::UnknownBranch(branch.to_string()))?;
    let requested: Option<String> = entry_id.map(str::to_string).or_else(|| tip.clone());
    let mut found = requested.is_none();
    let mut destination_tip: Option<String> = None;
    let mut current = tip;
    while let Some(id) = current {
        let parent = get_parent(&id).ok_or_else(|| ForkPolicyError::MissingParent(id.clone()))?;
        if requested.as_deref() == Some(id.as_str()) {
            found = true;
            destination_tip = if position == ForkPosition::Before {
                parent.clone()
            } else {
                Some(id.clone())
            };
            if position != ForkPosition::Before {
                select_entry(&id);
            }
        } else if found {
            select_entry(&id);
        }
        current = parent;
    }
    if !found {
        return Err(ForkPolicyError::EntryNotOnBranch {
            entry: requested.unwrap_or_default(),
            branch: branch.to_string(),
        });
    }
    Ok(ForkCurrentStatePlan::Branch {
        branch: branch.to_string(),
        destination_tip,
    })
}

/// Project one current scalar row or surviving list element into
/// destination state. Port of upstream `projectForkCurrentStateWrite`:
///
/// * `tack.session.name` → copied.
/// * `tack.entry.label` → copied iff the keyed entry is copied.
/// * `tack.branch.tip` → tree: verbatim; branch: only the named branch, with
///   its value rewritten to the planned destination tip.
/// * `tack.lane.config` → tree: verbatim; branch: only the named lane.
/// * `tack.lane.state` → kept under the same scope rule as configs, but the
///   value is always replaced with fresh idle state.
/// * `tack.result`, `tack.op.*`, `tack.pending.*` → excluded, both scopes.
/// * the exact namespace `tack` or any other `tack.*` → the fork FAILS.
/// * upstream `pi.*` namespaces → the same rules under the `pi` prefix.
/// * anything else (application state) → tree: copied; branch: excluded.
pub fn project_fork_current_state_write(
    write: &ForkStateWrite,
    plan: &ForkCurrentStatePlan,
    is_entry_copied: &dyn Fn(&str) -> bool,
) -> Result<Option<ForkStateWrite>, ForkPolicyError> {
    let namespace = write.namespace();
    match namespace {
        NS_SESSION_NAME => return Ok(Some(write.clone())),
        NS_ENTRY_LABEL => {
            return Ok(if is_entry_copied(write.key()) {
                Some(write.clone())
            } else {
                None
            });
        }
        NS_BRANCH_TIP => {
            return Ok(match plan {
                ForkCurrentStatePlan::Tree => Some(write.clone()),
                ForkCurrentStatePlan::Branch {
                    branch,
                    destination_tip,
                } if write.key() == branch => {
                    let tip = match destination_tip {
                        Some(tip) => Value::String(tip.clone()),
                        None => Value::Null,
                    };
                    Some(write.clone().with_value(tip))
                }
                ForkCurrentStatePlan::Branch { .. } => None,
            });
        }
        NS_LANE_CONFIG => {
            return Ok(match plan {
                ForkCurrentStatePlan::Tree => Some(write.clone()),
                ForkCurrentStatePlan::Branch { branch, .. } if write.key() == branch => {
                    Some(write.clone())
                }
                ForkCurrentStatePlan::Branch { .. } => None,
            });
        }
        NS_LANE_STATE => {
            return Ok(match plan {
                ForkCurrentStatePlan::Tree => {
                    Some(write.clone().with_value(idle_lane_state_value()))
                }
                ForkCurrentStatePlan::Branch { branch, .. } if write.key() == branch => {
                    Some(write.clone().with_value(idle_lane_state_value()))
                }
                ForkCurrentStatePlan::Branch { .. } => None,
            });
        }
        NS_OPERATION_RESULT => return Ok(None),
        _ => {}
    }
    if namespace.starts_with(PREFIX_OPERATION) || namespace.starts_with(PREFIX_PENDING) {
        return Ok(None);
    }
    if namespace == "tack" || namespace.starts_with("tack.") {
        return Err(ForkPolicyError::UnknownReservedNamespace(
            namespace.to_string(),
        ));
    }
    // Upstream `pi.*` reserved namespaces: identical semantics under the
    // upstream prefix (see the constants block above).
    match namespace {
        PI_SESSION_NAME => return Ok(Some(write.clone())),
        PI_ENTRY_LABEL => {
            return Ok(if is_entry_copied(write.key()) {
                Some(write.clone())
            } else {
                None
            });
        }
        PI_BRANCH_TIP => {
            return Ok(match plan {
                ForkCurrentStatePlan::Tree => Some(write.clone()),
                ForkCurrentStatePlan::Branch {
                    branch,
                    destination_tip,
                } if write.key() == branch => {
                    let tip = match destination_tip {
                        Some(tip) => Value::String(tip.clone()),
                        None => Value::Null,
                    };
                    Some(write.clone().with_value(tip))
                }
                ForkCurrentStatePlan::Branch { .. } => None,
            });
        }
        PI_LANE_CONFIG => {
            return Ok(match plan {
                ForkCurrentStatePlan::Tree => Some(write.clone()),
                ForkCurrentStatePlan::Branch { branch, .. } if write.key() == branch => {
                    Some(write.clone())
                }
                ForkCurrentStatePlan::Branch { .. } => None,
            });
        }
        PI_LANE_STATE => {
            return Ok(match plan {
                ForkCurrentStatePlan::Tree => {
                    Some(write.clone().with_value(idle_lane_state_value()))
                }
                ForkCurrentStatePlan::Branch { branch, .. } if write.key() == branch => {
                    Some(write.clone().with_value(idle_lane_state_value()))
                }
                ForkCurrentStatePlan::Branch { .. } => None,
            });
        }
        PI_OPERATION_RESULT => return Ok(None),
        _ => {}
    }
    if namespace.starts_with(PI_PREFIX_OPERATION) || namespace.starts_with(PI_PREFIX_PENDING) {
        return Ok(None);
    }
    if namespace == "pi" || namespace.starts_with("pi.") {
        return Err(ForkPolicyError::UnknownReservedNamespace(
            namespace.to_string(),
        ));
    }
    Ok(match plan {
        ForkCurrentStatePlan::Tree => Some(write.clone()),
        ForkCurrentStatePlan::Branch { .. } => None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn set(namespace: &str, key: &str, value: Value) -> ForkStateWrite {
        ForkStateWrite::ValueSet {
            namespace: namespace.to_string(),
            key: key.to_string(),
            value,
        }
    }

    fn branch_plan() -> ForkCurrentStatePlan {
        ForkCurrentStatePlan::Branch {
            branch: "main".to_string(),
            destination_tip: Some("tip-1".to_string()),
        }
    }

    #[test]
    fn session_name_is_always_copied() {
        for plan in [branch_plan(), ForkCurrentStatePlan::Tree] {
            let out = project_fork_current_state_write(
                &set(NS_SESSION_NAME, "", Value::String("s".into())),
                &plan,
                &|_| false,
            )
            .unwrap();
            assert!(out.is_some(), "{plan:?}");
        }
    }

    #[test]
    fn labels_follow_copied_entries() {
        let write = set(NS_ENTRY_LABEL, "e1", Value::String("lbl".into()));
        assert!(
            project_fork_current_state_write(&write, &branch_plan(), &|id| id == "e1")
                .unwrap()
                .is_some()
        );
        assert!(
            project_fork_current_state_write(&write, &branch_plan(), &|_| false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn branch_tip_is_rewritten_for_named_branch_only() {
        let named = set(NS_BRANCH_TIP, "main", Value::String("old-tip".into()));
        let out = project_fork_current_state_write(&named, &branch_plan(), &|_| true)
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            set(NS_BRANCH_TIP, "main", Value::String("tip-1".into())),
            "tip rewritten to the plan's destination tip"
        );
        let other = set(NS_BRANCH_TIP, "other", Value::String("x".into()));
        assert!(
            project_fork_current_state_write(&other, &branch_plan(), &|_| true)
                .unwrap()
                .is_none(),
            "other branches dropped on branch scope"
        );
        assert!(
            project_fork_current_state_write(&other, &ForkCurrentStatePlan::Tree, &|_| true)
                .unwrap()
                .is_some(),
            "tree scope keeps every branch tip verbatim"
        );
    }

    #[test]
    fn branch_tip_rewrite_can_yield_null() {
        let plan = ForkCurrentStatePlan::Branch {
            branch: "main".to_string(),
            destination_tip: None,
        };
        let named = set(NS_BRANCH_TIP, "main", Value::String("old".into()));
        let out = project_fork_current_state_write(&named, &plan, &|_| true)
            .unwrap()
            .unwrap();
        assert_eq!(out, set(NS_BRANCH_TIP, "main", Value::Null));
    }

    #[test]
    fn lane_state_is_reset_to_idle_on_both_scopes() {
        let busy = serde_json::json!({
            "currentOperationId": "op-1",
            "lastOperationId": "op-0",
            "inbox": ["e1"],
        });
        let write = set(NS_LANE_STATE, "main", busy);
        for plan in [branch_plan(), ForkCurrentStatePlan::Tree] {
            let out = project_fork_current_state_write(&write, &plan, &|_| true)
                .unwrap()
                .unwrap();
            assert_eq!(
                out,
                set(NS_LANE_STATE, "main", idle_lane_state_value()),
                "{plan:?}"
            );
        }
        let other_lane = set(NS_LANE_STATE, "other", idle_lane_state_value());
        assert!(
            project_fork_current_state_write(&other_lane, &branch_plan(), &|_| true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn lane_config_scoped_like_branch_tip_but_value_preserved() {
        let config = serde_json::json!({
            "model": {"provider": "anthropic", "modelId": "claude"},
            "thinkingLevel": "high",
            "activeToolNames": [],
        });
        let write = set(NS_LANE_CONFIG, "main", config.clone());
        let out = project_fork_current_state_write(&write, &branch_plan(), &|_| true)
            .unwrap()
            .unwrap();
        assert_eq!(out, set(NS_LANE_CONFIG, "main", config));
        let other = set(NS_LANE_CONFIG, "other", Value::Null);
        assert!(
            project_fork_current_state_write(&other, &branch_plan(), &|_| true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn operation_and_pending_state_is_excluded_on_both_scopes() {
        for ns in [
            NS_OPERATION_RESULT,
            "tack.op.meta",
            "tack.op.state",
            "tack.op.tool_args",
            "tack.pending.entry",
            "tack.pending.tool_output",
            "tack.pending.assistant_frame",
        ] {
            for plan in [branch_plan(), ForkCurrentStatePlan::Tree] {
                let out =
                    project_fork_current_state_write(&set(ns, "k", Value::Null), &plan, &|_| true)
                        .unwrap();
                assert!(out.is_none(), "{ns} must be excluded on {plan:?}");
            }
        }
    }

    #[test]
    fn unknown_reserved_pi_namespace_fails_the_fork() {
        for ns in ["tack", "tack.future.thing", "tack.branch", "tack.lane"] {
            for plan in [branch_plan(), ForkCurrentStatePlan::Tree] {
                let err =
                    project_fork_current_state_write(&set(ns, "k", Value::Null), &plan, &|_| true)
                        .unwrap_err();
                assert!(
                    matches!(err, ForkPolicyError::UnknownReservedNamespace(_)),
                    "{ns} on {plan:?}: {err:?}"
                );
            }
        }
    }

    /// Upstream `pi.*` rows get upstream fork semantics: operation and
    /// pending state never crosses a fork, lane state resets to idle,
    /// tips rewrite on branch scope, unknown pi namespaces fail closed.
    #[test]
    fn upstream_pi_namespaces_follow_upstream_rules() {
        for ns in [
            "pi.result",
            "pi.op.meta",
            "pi.op.state",
            "pi.pending.entry",
            "pi.pending.assistant_frame",
        ] {
            for plan in [branch_plan(), ForkCurrentStatePlan::Tree] {
                let out =
                    project_fork_current_state_write(&set(ns, "k", Value::Null), &plan, &|_| true)
                        .unwrap();
                assert!(out.is_none(), "{ns} must be excluded on {plan:?}");
            }
        }

        // Lane state resets to idle on both scopes.
        let busy = serde_json::json!({
            "currentOperationId": "op-1",
            "lastOperationId": "op-0",
            "inbox": [{"entryId": "e1", "kind": "steer"}],
        });
        let write = set("pi.lane.state", "main", busy);
        for plan in [branch_plan(), ForkCurrentStatePlan::Tree] {
            let out = project_fork_current_state_write(&write, &plan, &|_| true)
                .unwrap()
                .unwrap();
            assert_eq!(out, set("pi.lane.state", "main", idle_lane_state_value()));
        }

        // Branch tip rewrites to the destination tip on branch scope.
        let tip = set("pi.branch.tip", "main", Value::String("old".into()));
        let out = project_fork_current_state_write(&tip, &branch_plan(), &|_| true)
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            set("pi.branch.tip", "main", Value::String("tip-1".into()))
        );

        // Session name and labels follow the tack-namespace rules.
        let name = set("pi.session.name", "", Value::String("s".into()));
        assert!(
            project_fork_current_state_write(&name, &branch_plan(), &|_| false)
                .unwrap()
                .is_some()
        );
        let label = set("pi.entry.label", "e1", Value::String("l".into()));
        assert!(
            project_fork_current_state_write(&label, &branch_plan(), &|id| id == "e1")
                .unwrap()
                .is_some()
        );
        assert!(
            project_fork_current_state_write(&label, &branch_plan(), &|_| false)
                .unwrap()
                .is_none()
        );

        // Unknown upstream-reserved namespaces fail closed.
        for ns in ["pi", "pi.future.thing"] {
            let err = project_fork_current_state_write(
                &set(ns, "k", Value::Null),
                &ForkCurrentStatePlan::Tree,
                &|_| true,
            )
            .unwrap_err();
            assert!(matches!(err, ForkPolicyError::UnknownReservedNamespace(_)));
        }
    }

    #[test]
    fn application_state_tree_copies_branch_excludes() {
        let write = set("my-app.state", "k", Value::from(1));
        assert!(
            project_fork_current_state_write(&write, &ForkCurrentStatePlan::Tree, &|_| true)
                .unwrap()
                .is_some()
        );
        assert!(
            project_fork_current_state_write(&write, &branch_plan(), &|_| true)
                .unwrap()
                .is_none()
        );
        // Same rule for surviving list elements.
        let list = ForkStateWrite::ListAppend {
            namespace: "my-app.log".to_string(),
            key: String::new(),
            value: Value::from("x"),
        };
        assert!(
            project_fork_current_state_write(&list, &ForkCurrentStatePlan::Tree, &|_| true)
                .unwrap()
                .is_some()
        );
        assert!(
            project_fork_current_state_write(&list, &branch_plan(), &|_| true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn select_branch_fork_walks_and_selects_ancestry() {
        // Tree: r -> a -> b (tip)
        let parent = |id: &str| -> Option<Option<String>> {
            Some(match id {
                "b" => Some("a".to_string()),
                "a" => Some("r".to_string()),
                "r" => None,
                _ => return None,
            })
        };
        let mut selected: Vec<String> = Vec::new();
        let plan = select_branch_fork(
            "main",
            None,
            ForkPosition::At,
            Some(Some("b".to_string())),
            &parent,
            &mut |id| selected.push(id.to_string()),
        )
        .unwrap();
        assert_eq!(
            plan,
            ForkCurrentStatePlan::Branch {
                branch: "main".to_string(),
                destination_tip: Some("b".to_string()),
            }
        );
        assert_eq!(selected, vec!["b", "a", "r"]);
    }

    #[test]
    fn select_branch_fork_position_before_cuts_parent() {
        let parent = |id: &str| -> Option<Option<String>> {
            Some(match id {
                "b" => Some("a".to_string()),
                "a" => Some("r".to_string()),
                "r" => None,
                _ => return None,
            })
        };
        let mut selected: Vec<String> = Vec::new();
        let plan = select_branch_fork(
            "main",
            Some("a"),
            ForkPosition::Before,
            Some(Some("b".to_string())),
            &parent,
            &mut |id| selected.push(id.to_string()),
        )
        .unwrap();
        assert_eq!(
            plan,
            ForkCurrentStatePlan::Branch {
                branch: "main".to_string(),
                destination_tip: Some("r".to_string()),
            }
        );
        assert_eq!(selected, vec!["r"], "the cut entry itself is not copied");
    }

    #[test]
    fn select_branch_fork_before_root_yields_null_tip() {
        let parent = |_id: &str| -> Option<Option<String>> { Some(None) };
        let plan = select_branch_fork(
            "main",
            Some("r"),
            ForkPosition::Before,
            Some(Some("r".to_string())),
            &parent,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            plan,
            ForkCurrentStatePlan::Branch {
                branch: "main".to_string(),
                destination_tip: None,
            }
        );
    }

    #[test]
    fn select_branch_fork_rejects_unknown_branch_and_stray_entries() {
        let err = select_branch_fork("nope", None, ForkPosition::At, None, &|_| None, &mut |_| {})
            .unwrap_err();
        assert!(matches!(err, ForkPolicyError::UnknownBranch(_)), "{err:?}");

        let parent = |id: &str| -> Option<Option<String>> {
            Some(match id {
                "b" => Some("a".to_string()),
                "a" => None,
                _ => None,
            })
        };
        let err = select_branch_fork(
            "main",
            Some("elsewhere"),
            ForkPosition::At,
            Some(Some("b".to_string())),
            &parent,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(
            matches!(err, ForkPolicyError::EntryNotOnBranch { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn select_branch_fork_null_tip_without_entry_is_legal() {
        // Upstream: "a null source tip with no entryId" is legal and yields
        // a null destination tip.
        let plan = select_branch_fork(
            "main",
            None,
            ForkPosition::At,
            Some(None),
            &|_| None,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            plan,
            ForkCurrentStatePlan::Branch {
                branch: "main".to_string(),
                destination_tip: None,
            }
        );
    }
}
