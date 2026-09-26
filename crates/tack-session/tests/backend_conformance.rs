//! Backend conformance suite (pattern borrowed from TS pi's
//! `packages/agent/src/harness/session/testing/conformance/session-repo.ts`):
//! the SAME scenario runs against every storage backend, and the observable
//! results must be structurally identical. Ids and timestamps are random per
//! run, so comparisons use a normalized fingerprint: each entry serialized to
//! JSON with `id` removed and `parentId` rewritten to the parent's positional
//! index in the entry list.
//!
//! Backends covered: legacy v3 JSONL, format-v4 JSONL (default) and SQLite
//! (experimental).
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use tack_agent_core::AgentMessage;
use tack_session::{SessionBackend, SessionLine, SessionManager};

/// Remove every `timestamp` key recursively (message payloads carry their
/// own millisecond timestamps, which differ across backends).
fn scrub_timestamps(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("timestamp");
            for v in map.values_mut() {
                scrub_timestamps(v);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                scrub_timestamps(v);
            }
        }
        _ => {}
    }
}

/// Normalize a manager's entry stream into a backend-independent fingerprint.
fn fingerprint(mgr: &SessionManager) -> Vec<String> {
    let entries = mgr.entries();
    let ids: Vec<&str> = entries.iter().map(|e| e.id()).collect();
    entries
        .iter()
        .map(|e| {
            let mut value: serde_json::Value =
                serde_json::from_str(&SessionLine::Entry((*e).clone()).to_json()).unwrap();
            scrub_timestamps(&mut value);
            let obj = value.as_object_mut().unwrap();
            obj.remove("id");
            match obj.remove("parentId") {
                Some(parent) if parent.is_string() => {
                    let idx = parent
                        .as_str()
                        .and_then(|p| ids.iter().position(|id| *id == p))
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "<dangling>".to_string());
                    obj.insert("parentIdx".to_string(), serde_json::Value::String(idx));
                }
                _ => {}
            }
            serde_json::to_string(&value).unwrap()
        })
        .collect()
}

struct BackendFixture {
    dir: tempfile::TempDir,
    cwd: PathBuf,
}

impl BackendFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        BackendFixture { dir, cwd }
    }

    fn session_dir(&self) -> PathBuf {
        self.dir.path().join("sessions")
    }

    fn create(&self, backend: SessionBackend) -> SessionManager {
        SessionManager::create_with_backend(&self.cwd, Some(self.session_dir()), backend).unwrap()
    }

    /// Re-open the most recently created session for the backend.
    fn reopen(&self, backend: SessionBackend, session_id: &str) -> SessionManager {
        match backend {
            SessionBackend::Sqlite => {
                SessionManager::open_sqlite(session_id, &self.session_dir()).unwrap()
            }
            _ => {
                let path = tack_session::find_session_by_id(&self.session_dir(), session_id)
                    .expect("jsonl session file exists");
                SessionManager::open(&path, Some(self.session_dir())).unwrap()
            }
        }
    }
}

const BACKENDS: [SessionBackend; 3] = [
    SessionBackend::Jsonl,
    SessionBackend::JsonlV4,
    SessionBackend::Sqlite,
];

/// Assert every backend produced the same fingerprint as the first.
fn assert_all_identical(fps: &[Vec<String>], what: &str) {
    for (i, fp) in fps.iter().enumerate().skip(1) {
        assert_eq!(
            &fps[0], fp,
            "{what}: {:?} vs {:?} divergence",
            BACKENDS[0], BACKENDS[i]
        );
    }
}

fn run_append_scenario(fx: &BackendFixture, backend: SessionBackend) -> Vec<String> {
    let mut mgr = fx.create(backend);
    mgr.append_message(AgentMessage::user("first user"))
        .unwrap();
    mgr.append_model_change("anthropic", "claude-opus-4.5")
        .unwrap();
    mgr.append_message(AgentMessage::user("second user"))
        .unwrap();
    mgr.append_thinking_level_change("high").unwrap();
    mgr.append_session_info(Some("conformance".to_string()))
        .unwrap();
    let fp = fingerprint(&mgr);

    // Reopening must reproduce the identical stream.
    let id = mgr.session_id().to_string();
    drop(mgr);
    let reopened = fx.reopen(backend, &id);
    assert_eq!(
        fp,
        fingerprint(&reopened),
        "reopen mismatch for {backend:?}"
    );
    fp
}

#[test]
fn append_sequence_is_backend_identical() {
    let fps: Vec<Vec<String>> = BACKENDS
        .iter()
        .map(|b| run_append_scenario(&BackendFixture::new(), *b))
        .collect();
    assert_all_identical(&fps, "append sequence");
}

fn run_branch_scenario(fx: &BackendFixture, backend: SessionBackend) -> Vec<String> {
    let mut mgr = fx.create(backend);
    let first = mgr.append_message(AgentMessage::user("root")).unwrap();
    mgr.append_message(AgentMessage::user("trunk a")).unwrap();
    mgr.append_message(AgentMessage::user("trunk b")).unwrap();

    // Branch back to the root and grow a second path.
    mgr.branch(&first).unwrap();
    mgr.append_message(AgentMessage::user("branch a")).unwrap();

    // The active path must be root -> branch a on both backends.
    let path = mgr.build_session_path();
    let path_texts: Vec<String> = path
        .iter()
        .map(|e| SessionLine::Entry(e.clone()).to_json())
        .map(|j| {
            let mut v: serde_json::Value = serde_json::from_str(&j).unwrap();
            scrub_timestamps(&mut v);
            let obj = v.as_object_mut().unwrap();
            obj.remove("id");
            obj.remove("parentId");
            serde_json::to_string(&v).unwrap()
        })
        .collect();
    assert_eq!(path.len(), 2, "branched path length for {backend:?}");
    path_texts
}

#[test]
fn branch_and_path_are_backend_identical() {
    let paths: Vec<Vec<String>> = BACKENDS
        .iter()
        .map(|b| run_branch_scenario(&BackendFixture::new(), *b))
        .collect();
    assert_all_identical(&paths, "branch path");
}

/// Forking must copy the full entry stream (minus the old header), record a
/// parentSession pointer, and give the fork an independent append sequence.
#[test]
fn fork_is_backend_identical() {
    for backend in BACKENDS {
        let fx = BackendFixture::new();
        let mut src = fx.create(backend);
        src.append_message(AgentMessage::user("one")).unwrap();
        src.append_message(AgentMessage::user("two")).unwrap();
        let src_fp = fingerprint(&src);
        let src_id = src.session_id().to_string();

        let mut forked = match backend {
            SessionBackend::Sqlite => {
                SessionManager::fork_from_sqlite(&src_id, &fx.cwd, &fx.session_dir()).unwrap()
            }
            _ => {
                let path = tack_session::find_session_by_id(&fx.session_dir(), &src_id).unwrap();
                SessionManager::fork_from_in(&path, &fx.cwd, &fx.session_dir()).unwrap()
            }
        };
        assert_eq!(
            src_fp,
            fingerprint(&forked),
            "fork must preserve the source stream for {backend:?}"
        );

        // The fork diverges independently: appending to it must not touch the
        // source session.
        forked
            .append_message(AgentMessage::user("fork only"))
            .unwrap();
        let src_reloaded = fx.reopen(backend, &src_id);
        assert_eq!(src_fp, fingerprint(&src_reloaded));
    }
}

/// Unknown entry ids must be rejected identically on both backends.
#[test]
fn branch_to_unknown_entry_errors_on_all_backends() {
    for backend in BACKENDS {
        let fx = BackendFixture::new();
        let mut mgr = fx.create(backend);
        mgr.append_message(AgentMessage::user("x")).unwrap();
        assert!(mgr.branch("no-such-entry").is_err(), "{backend:?}");
    }
}

/// v4 consistency: the observable message stream of a session must be
/// identical across the v3 JSONL backend, the SQLite backend, and the v3
/// file after its transparent v4 migration (discarded record kinds —
/// model/thinking changes, session_info — fold into v4 lane state and are
/// excluded from the comparison).
#[test]
fn message_stream_is_identical_across_v3_sqlite_and_v4() {
    let scenario = |mgr: &mut SessionManager| {
        mgr.append_message(AgentMessage::user("first user"))
            .unwrap();
        mgr.append_model_change("anthropic", "claude-opus-4.5")
            .unwrap();
        mgr.append_thinking_level_change("high").unwrap();
        mgr.append_message(AgentMessage::user("second user"))
            .unwrap();
        mgr.append_session_info(Some("conformance".to_string()))
            .unwrap();
    };
    let message_stream = |mgr: &SessionManager| -> Vec<String> {
        mgr.build_session_path()
            .iter()
            .filter_map(|e| match e {
                tack_session::SessionEntry::Message { message, .. } => {
                    let mut v = serde_json::to_value(message).unwrap();
                    scrub_timestamps(&mut v);
                    Some(serde_json::to_string(&v).unwrap())
                }
                _ => None,
            })
            .collect()
    };

    let mut streams: Vec<Vec<String>> = Vec::new();
    let mut jsonl_fx: Option<(BackendFixture, String)> = None;
    for backend in BACKENDS {
        let fx = BackendFixture::new();
        let mut mgr = fx.create(backend);
        scenario(&mut mgr);
        streams.push(message_stream(&mgr));
        if backend == SessionBackend::Jsonl {
            jsonl_fx = Some((fx, mgr.session_id().to_string()));
        }
    }
    assert_all_identical(&streams, "message stream");

    // Migrate the v3 JSONL file to v4 (transparent on open) and compare.
    let (fx, session_id) = jsonl_fx.unwrap();
    let path = tack_session::find_session_by_id(&fx.session_dir(), &session_id).unwrap();
    let store = tack_session::v4::V4Store::open(&path).unwrap();
    assert!(store.was_legacy_v3());
    let v4_stream: Vec<String> = store
        .scan_branch("main")
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            tack_session::v4::V4Entry::Message { message, .. } => {
                let mut v = serde_json::to_value(message).unwrap();
                scrub_timestamps(&mut v);
                Some(serde_json::to_string(&v).unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(streams[0], v4_stream, "v3 JSONL vs migrated v4 divergence");
    // The lane folded from model/thinking changes matches the v3 context.
    let config = store.lane_config("main").expect("lane config derived");
    assert_eq!(config.model.provider, "anthropic");
    assert_eq!(config.model.model_id, "claude-opus-4.5");
    assert_eq!(config.thinking_level, "high");
}
