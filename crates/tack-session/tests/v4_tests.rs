//! Integration tests for v4 session storage: format roundtrips
//! (plaintext + encrypted), streaming legacy v3 → v4 migration (plain +
//! encrypted fixtures), fork-policy projection over real stores, and a
//! conformance-style comparison of the v3/v4 observable entry streams.
//!
//! Unit-level fork-policy cases live in `src/fork_policy.rs`; fork
//! integration cases live in `src/v4/fork.rs`. This file covers the
//! end-to-end paths through real files.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use serde_json::Value;
use tack_agent_core::AgentMessage;
use tack_session::v4::{
    ForkOptions, LaneConfiguration, LaneModel, LaneState, V4Entry, V4Header, V4NewWrite, V4Store,
};
use tack_session::{ForkPosition, SessionBackend, SessionManager};

/// One fixed key for the whole test binary: the process-global session
/// key OnceLock can only ever hold one key, so every encrypted test
/// installs the SAME key (order-independent).
fn install_test_key() {
    tack_session::crypto::set_session_key([42u8; 32]);
}

fn lane_config(provider: &str, model: &str, thinking: &str) -> LaneConfiguration {
    LaneConfiguration {
        model: LaneModel {
            provider: provider.to_string(),
            model_id: model.to_string(),
        },
        thinking_level: thinking.to_string(),
        active_tool_names: vec![],
    }
}

fn header(id: &str) -> V4Header {
    V4Header::new(id.to_string(), "/work".to_string())
}

// --- roundtrip -----------------------------------------------------------

#[test]
fn v4_store_roundtrip_plaintext() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("s.jsonl");
    let mut store = V4Store::create(&path, header("s1"), vec![]).unwrap();
    store
        .create_lane("main", lane_config("anthropic", "claude", "high"))
        .unwrap();
    let e1 = store
        .append_message("main", AgentMessage::user("one"))
        .unwrap();
    store
        .append_message("main", AgentMessage::user("two"))
        .unwrap();
    store.set_session_name("demo").unwrap();
    store.set_label(&e1, Some("marked".to_string())).unwrap();
    drop(store);

    // The header is line 1; transactions follow, one per line.
    let content = std::fs::read_to_string(&path).unwrap();
    let first: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first["v"], 4);
    assert_eq!(first["kind"], "header");
    assert_eq!(first["storageVersion"], 1);

    let store = V4Store::open(&path).unwrap();
    assert!(!store.was_legacy_v3());
    assert_eq!(store.entries().len(), 2);
    assert_eq!(store.session_name().as_deref(), Some("demo"));
    assert_eq!(store.get_label(&e1).as_deref(), Some("marked"));
    assert!(store.has_complete_lane("main"));
}

#[test]
fn v4_store_roundtrip_encrypted() {
    install_test_key();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("enc.jsonl");
    let mut store = V4Store::create(&path, header("enc1"), vec![]).unwrap();
    store
        .create_lane("main", lane_config("anthropic", "claude", "high"))
        .unwrap();
    store
        .append_message("main", AgentMessage::user("v4 top secret"))
        .unwrap();
    drop(store);

    // Header stays plaintext; transaction lines are ciphertext.
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        !content.contains("v4 top secret"),
        "plaintext leaked: {content}"
    );
    let mut lines = content.lines();
    let first = lines.next().unwrap();
    assert!(first.contains("\"kind\":\"header\""), "{first}");
    for line in lines {
        assert!(
            tack_session::crypto::is_encrypted_line(line),
            "transaction line must be encrypted: {line}"
        );
    }

    // ... and the store reopens through the decrypting path.
    let store = V4Store::open(&path).unwrap();
    assert_eq!(store.entries().len(), 1);
    let V4Entry::Message { message, .. } = &store.entries()[0] else {
        panic!("expected message entry");
    };
    assert!(matches!(message, AgentMessage::User(_)));
}

#[test]
fn v4_open_rejects_undecryptable_files_loudly() {
    install_test_key();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("locked.jsonl");
    let mut store = V4Store::create(&path, header("locked"), vec![]).unwrap();
    store.set_session_name("x").unwrap();
    drop(store);
    // A tampered/foreign ciphertext line is undecryptable with ANY key.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    use std::io::Write;
    writeln!(f, "tack-enc:v1:not-valid-base64!!!").unwrap();
    drop(f);
    let err = V4Store::open(&path).unwrap_err();
    assert!(
        matches!(err, tack_session::v4::V4Error::Encrypted(_)),
        "{err:?}"
    );
}

// --- migration -----------------------------------------------------------

/// A v3 fixture exercising every retained and discarded record kind:
/// messages, model/thinking changes (→ lane config), a label, session
/// info (→ name), a branch summary, and a compaction whose retained tail
/// must be reconstructed from physical ancestry.
fn v3_fixture() -> String {
    concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\",\"timestamp\":1}}\n",
        "{\"type\":\"model_change\",\"id\":\"mc1\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"provider\":\"anthropic\",\"modelId\":\"claude-opus-4.5\"}\n",
        "{\"type\":\"thinking_level_change\",\"id\":\"tl1\",\"parentId\":\"mc1\",\"timestamp\":\"2026-01-01T00:00:03.000Z\",\"thinkingLevel\":\"high\"}\n",
        "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"tl1\",\"timestamp\":\"2026-01-01T00:00:04.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hi there\"}],\"api\":\"messages\",\"provider\":\"anthropic\",\"model\":\"claude-opus-4.5\",\"usage\":{\"input\":10,\"output\":5,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":15,\"cost\":{\"input\":0.0,\"output\":0.0,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":0.0}},\"stopReason\":\"stop\",\"timestamp\":2}}\n",
        "{\"type\":\"label\",\"id\":\"l1\",\"parentId\":\"m2\",\"timestamp\":\"2026-01-01T00:00:05.000Z\",\"targetId\":\"m1\",\"label\":\"important\"}\n",
        "{\"type\":\"session_info\",\"id\":\"si1\",\"parentId\":\"l1\",\"timestamp\":\"2026-01-01T00:00:06.000Z\",\"name\":\"my session\"}\n",
        "{\"type\":\"branch_summary\",\"id\":\"bs1\",\"parentId\":\"si1\",\"timestamp\":\"2026-01-01T00:00:07.000Z\",\"fromId\":\"m1\",\"summary\":\"branch recap\"}\n",
        "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":\"bs1\",\"timestamp\":\"2026-01-01T00:00:08.000Z\",\"summary\":\"compact sum\",\"firstKeptEntryId\":\"m2\",\"tokensBefore\":12000}\n",
    )
    .to_string()
}

fn write_v3(tmp: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
    let path = tmp.path().join(name);
    std::fs::write(&path, content).unwrap();
    path
}

#[test]
fn v3_file_migrates_transparently_on_open() {
    let tmp = tempfile::tempdir().unwrap();
    let original = v3_fixture();
    let path = write_v3(&tmp, "s.jsonl", &original);

    let store = V4Store::open(&path).unwrap();
    assert!(store.was_legacy_v3(), "open must report the migration");

    // Header rewritten to v4, identity preserved, nextSeq stamped.
    let content = std::fs::read_to_string(&path).unwrap();
    let first: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first["v"], 4);
    assert_eq!(first["kind"], "header");
    assert_eq!(first["id"], "s1");
    assert_eq!(first["cwd"], "/work");
    assert_eq!(first["createdAt"], serde_json::json!(1767225600000u64));
    // 8 retained entries + 5 values (name, label, tip, config, state) +
    // 1 usage row = 14 writes → nextSeq 15.
    assert_eq!(first["nextSeq"], serde_json::json!(15u64));

    // Backup of the pre-migration file.
    let bak = PathBuf::from(format!("{}.bak", path.display()));
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), original);

    // Retained entries: m1, mc1, tl1, m2, l1, si1, bs1, c1 — the change
    // records (model/thinking/label/session_info) are kept as custom
    // entries so the session context keeps model/thinking state.
    let entries = store.entries();
    assert_eq!(entries.len(), 8, "{entries:?}");
    let ids: Vec<&str> = entries.iter().map(V4Entry::id).collect();
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert!(
        !ids.iter()
            .any(|id| ["m1", "mc1", "tl1", "m2", "l1", "si1", "bs1", "c1"].contains(id)),
        "fresh ids are minted: {ids:?}"
    );
    assert_eq!(entries[0].parent_id(), None);
    for i in 1..8 {
        assert_eq!(entries[i].parent_id(), Some(entries[i - 1].id()));
    }
    // The change records round-trip as typed custom entries.
    assert!(
        matches!(&entries[1], V4Entry::Custom { custom_type, .. } if custom_type == "model_change")
    );
    assert!(
        matches!(&entries[2], V4Entry::Custom { custom_type, .. } if custom_type == "thinking_level_change")
    );

    // Derived values.
    assert_eq!(store.session_name().as_deref(), Some("my session"));
    assert_eq!(
        store.get_label(entries[0].id()).as_deref(),
        Some("important")
    );
    assert_eq!(
        store.branch_tip("main"),
        Some(Some(entries[7].id().to_string()))
    );
    let config = store.lane_config("main").expect("lane config derived");
    assert_eq!(config.model.provider, "anthropic");
    assert_eq!(config.model.model_id, "claude-opus-4.5");
    assert_eq!(config.thinking_level, "high");
    assert_eq!(store.lane_state("main"), Some(LaneState::idle()));

    // The branch summary's fromId resolved through the id mapping.
    let V4Entry::BranchSummary {
        from_id, summary, ..
    } = &entries[6]
    else {
        panic!("expected branch summary, got {:?}", entries[6]);
    };
    assert_eq!(from_id.as_deref(), Some(entries[0].id()));
    assert_eq!(summary, "branch recap");

    // The compaction tail was reconstructed from physical ancestry,
    // oldest first: m2's assistant message, then bs1's summary message.
    let V4Entry::Compaction {
        retained_tail,
        tokens_before,
        ..
    } = &entries[7]
    else {
        panic!("expected compaction, got {:?}", entries[7]);
    };
    assert_eq!(*tokens_before, 12000);
    assert_eq!(retained_tail.len(), 2, "{retained_tail:?}");
    assert!(matches!(retained_tail[0], AgentMessage::Assistant(_)));
    assert!(matches!(retained_tail[1], AgentMessage::BranchSummary(_)));

    // Imported usage survives as one adjustment row.
    let rows = store.usage_rows();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].adjustment);
    assert_eq!(rows[0].usage.input, 10);
    assert_eq!(rows[0].usage.output, 5);
    assert_eq!(
        rows[0].details,
        Some(serde_json::json!({"source": "v3-import"}))
    );

    // Reopening the migrated file is an ordinary v4 open.
    let reopened = V4Store::open(&path).unwrap();
    assert!(!reopened.was_legacy_v3());
    assert_eq!(reopened.entries().len(), 8);
    assert_eq!(reopened.next_seq(), 15);
}

#[test]
fn v3_migration_without_model_history_derives_no_lane() {
    let tmp = tempfile::tempdir().unwrap();
    // No model_change / thinking_level_change: a data-only branch.
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s2\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let store = V4Store::open(&path).unwrap();
    assert!(store.was_legacy_v3());
    assert_eq!(store.entries().len(), 1);
    assert_eq!(
        store.branch_tip("main"),
        Some(Some(store.entries()[0].id().to_string()))
    );
    assert!(store.lane_config("main").is_none());
    assert!(store.lane_state("main").is_none());
    assert!(!store.has_complete_lane("main"));

    // Upstream: branch forks of data-only reconstructed lanes reject;
    // tree scope remains available.
    let err = tack_session::v4::run_v4_fork(
        &store,
        &tmp.path().join("fork.jsonl"),
        header("f1"),
        &ForkOptions::Branch {
            branch: "main".to_string(),
            entry_id: None,
            position: ForkPosition::At,
            id: None,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, tack_session::v4::V4Error::NotAConfiguredLane(_)),
        "{err:?}"
    );

    tack_session::v4::run_v4_fork(
        &store,
        &tmp.path().join("fork-tree.jsonl"),
        header("f2"),
        &ForkOptions::Tree { id: None },
    )
    .unwrap();
}

#[test]
fn encrypted_v3_migrates_to_encrypted_v4() {
    install_test_key();
    let tmp = tempfile::tempdir().unwrap();
    // Build a v3 file whose entry lines are encrypted (tack at-rest
    // encryption semantics: plaintext header, encrypted entries).
    let entry_line = "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"migration secret\",\"timestamp\":1}}";
    let encrypted = tack_session::crypto::encrypt_line(entry_line).unwrap();
    let original = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"s3\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}}\n{encrypted}\n"
    );
    let path = write_v3(&tmp, "enc.jsonl", &original);

    let store = V4Store::open(&path).unwrap();
    assert!(store.was_legacy_v3());
    assert_eq!(store.entries().len(), 1);

    // The migrated file stays encrypted: no plaintext leak, header
    // plaintext, transaction lines ciphertext.
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        !content.contains("migration secret"),
        "plaintext leaked: {content}"
    );
    let mut lines = content.lines();
    assert!(lines.next().unwrap().contains("\"kind\":\"header\""));
    for line in lines {
        assert!(
            tack_session::crypto::is_encrypted_line(line),
            "migrated line must stay encrypted: {line}"
        );
    }
    // The backup keeps the ORIGINAL (also encrypted) content.
    let bak = PathBuf::from(format!("{}.bak", path.display()));
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), original);

    // The message decrypted correctly through the migration.
    let V4Entry::Message { message, .. } = &store.entries()[0] else {
        panic!("expected message entry");
    };
    let AgentMessage::User(u) = message else {
        panic!("expected user message");
    };
    let tack_ai::UserContent::Text(text) = &u.content else {
        panic!("expected text");
    };
    assert_eq!(text, "migration secret");
}

#[test]
fn v3_migration_preserves_existing_compaction_checkpoint_tail() {
    let tmp = tempfile::tempdir().unwrap();
    // A v3 compaction that already carries a materialized retainedTail
    // (tack checkpoint form): the tail must be kept verbatim, not
    // reconstructed.
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s4\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"kept\",\"timestamp\":1}}\n",
        "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"summary\":\"sum\",\"firstKeptEntryId\":\"m1\",\"tokensBefore\":100,\"retainedTail\":[{\"role\":\"user\",\"content\":\"kept\",\"timestamp\":1}]}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let store = V4Store::open(&path).unwrap();
    let V4Entry::Compaction { retained_tail, .. } = &store.entries()[1] else {
        panic!("expected compaction");
    };
    assert_eq!(retained_tail.len(), 1);
    let AgentMessage::User(u) = &retained_tail[0] else {
        panic!("expected user message in tail");
    };
    let tack_ai::UserContent::Text(text) = &u.content else {
        panic!("expected text");
    };
    assert_eq!(text, "kept");
}

#[test]
fn v3_migration_rejects_missing_parents_and_dangling_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    // Forward parent reference.
    let bad_parent = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s5\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":\"nope\",\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
    );
    let path = write_v3(&tmp, "bad.jsonl", bad_parent);
    let err = V4Store::open(&path).unwrap_err();
    assert!(
        matches!(err, tack_session::v4::V4Error::MissingLegacyParent { .. }),
        "{err:?}"
    );
    // The failed migration leaves the original untouched.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), bad_parent);

    // Compaction boundary not on the parent branch.
    let bad_boundary = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s6\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
        "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"message\":{\"role\":\"user\",\"content\":\"y\",\"timestamp\":2}}\n",
        "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-01T00:00:03.000Z\",\"summary\":\"s\",\"firstKeptEntryId\":\"m2\",\"tokensBefore\":10}\n",
    );
    let path = write_v3(&tmp, "bad2.jsonl", bad_boundary);
    let err = V4Store::open(&path).unwrap_err();
    assert!(
        matches!(
            err,
            tack_session::v4::V4Error::CompactionBoundaryNotOnBranch { .. }
        ),
        "{err:?}"
    );
}

// --- conformance: v3 manager stream vs migrated v4 stream -----------------

/// The observable message stream must survive the v3→v4 migration:
/// same messages, same order, same payloads (timestamps scrubbed — v3
/// stores ISO strings, v4 millis).
#[test]
fn migration_preserves_the_message_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut mgr =
        SessionManager::create_with_backend(&cwd, Some(session_dir.clone()), SessionBackend::Jsonl)
            .unwrap();
    mgr.append_message(AgentMessage::user("first")).unwrap();
    mgr.append_model_change("anthropic", "claude-opus-4.5")
        .unwrap();
    mgr.append_thinking_level_change("high").unwrap();
    mgr.append_message(AgentMessage::user("second")).unwrap();
    let session_id = mgr.session_id().to_string();
    let path = tack_session::find_session_by_id(&session_dir, &session_id).unwrap();

    let v3_messages: Vec<Value> = mgr
        .build_session_path()
        .iter()
        .filter_map(|e| match e {
            tack_session::SessionEntry::Message { message, .. } => {
                let mut v = serde_json::to_value(message).unwrap();
                scrub_timestamps(&mut v);
                Some(v)
            }
            _ => None,
        })
        .collect();
    drop(mgr);

    let store = V4Store::open(&path).unwrap();
    assert!(store.was_legacy_v3());
    let v4_messages: Vec<Value> = store
        .scan_branch("main")
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            V4Entry::Message { message, .. } => {
                let mut v = serde_json::to_value(message).unwrap();
                scrub_timestamps(&mut v);
                Some(v)
            }
            _ => None,
        })
        .collect();
    assert_eq!(v3_messages, v4_messages);
}

fn scrub_timestamps(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("timestamp");
            for v in map.values_mut() {
                scrub_timestamps(v);
            }
        }
        Value::Array(items) => {
            for v in items {
                scrub_timestamps(v);
            }
        }
        _ => {}
    }
}

// --- fork over a migrated store ------------------------------------------

#[test]
fn fork_of_migrated_store_applies_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_v3(&tmp, "s.jsonl", &v3_fixture());
    let mut store = V4Store::open(&path).unwrap();
    assert!(store.was_legacy_v3());
    store
        .commit(vec![V4NewWrite::ValueSet {
            namespace: "my-app.state".to_string(),
            key: "k".to_string(),
            value: Value::from(1),
        }])
        .unwrap();

    // Tree fork: everything except the usage ledger crosses.
    let tree = tack_session::v4::run_v4_fork(
        &store,
        &tmp.path().join("tree.jsonl"),
        header("t1"),
        &ForkOptions::Tree { id: None },
    )
    .unwrap();
    assert_eq!(tree.entries().len(), 8);
    assert_eq!(tree.usage_rows().len(), 0, "usage ledger excluded");
    assert_eq!(tree.get_value("my-app.state", "k"), Some(&Value::from(1)));
    assert_eq!(tree.session_name().as_deref(), Some("my session"));
    assert_eq!(tree.header().parent_session_id.as_deref(), Some("s1"));

    // Branch fork at the first entry: only that entry + the named lane.
    let first_id = store.entries()[0].id().to_string();
    let branch = tack_session::v4::run_v4_fork(
        &store,
        &tmp.path().join("branch.jsonl"),
        header("b1"),
        &ForkOptions::Branch {
            branch: "main".to_string(),
            entry_id: Some(first_id.clone()),
            position: ForkPosition::At,
            id: None,
        },
    )
    .unwrap();
    assert_eq!(branch.entries().len(), 1);
    assert_eq!(branch.branch_tip("main"), Some(Some(first_id.clone())));
    assert_eq!(branch.get_label(&first_id).as_deref(), Some("important"));
    assert_eq!(branch.get_value("my-app.state", "k"), None);
    assert!(branch.has_complete_lane("main"));
    assert_eq!(branch.lane_state("main"), Some(LaneState::idle()));
}

// --- regression tests for upstream-alignment fixes -------------------------

/// The replayed `systemMessage` is preserved on the REBUILD path too
/// (compaction without a materialized checkpoint tail): the tail is
/// rebuilt from ancestry, then the system message leads it.
#[test]
fn v3_migration_preserves_system_message_on_rebuilt_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s8\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"kept\",\"timestamp\":1}}\n",
        "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"summary\":\"sum\",\"firstKeptEntryId\":\"m1\",\"tokensBefore\":100,",
        "\"systemMessage\":{\"role\":\"system\",\"content\":\"\",\"sections\":{\"system-prompt\":\"Rebuilt prompt.\"},\"timestamp\":2}}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let store = V4Store::open(&path).unwrap();
    let V4Entry::Compaction { retained_tail, .. } = &store.entries()[1] else {
        panic!("expected compaction");
    };
    assert_eq!(retained_tail.len(), 2, "{retained_tail:?}");
    let AgentMessage::System(system) = &retained_tail[0] else {
        panic!(
            "expected leading system message, got {:?}",
            retained_tail[0]
        );
    };
    assert_eq!(
        system
            .sections
            .as_ref()
            .and_then(|s| s.get("system-prompt"))
            .and_then(|v| v.as_deref()),
        Some("Rebuilt prompt.")
    );
    // ... followed by the rebuilt tail (m1's user message).
    assert!(matches!(&retained_tail[1], AgentMessage::User(_)));
}

/// The imported usage adjustment row sums the optional token classes
/// (`cacheWrite1h`, `reasoning`) like upstream `addUsage`.
#[test]
fn v3_migration_imports_full_usage_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s9\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[],\"api\":\"messages\",\"provider\":\"anthropic\",\"model\":\"claude\",",
        "\"usage\":{\"input\":10,\"output\":5,\"cacheRead\":1,\"cacheWrite\":2,\"cacheWrite1h\":3,\"reasoning\":4,\"totalTokens\":25,\"cost\":{\"input\":0.0,\"output\":0.0,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":0.0}},\"stopReason\":\"stop\",\"timestamp\":2}}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let store = V4Store::open(&path).unwrap();
    let rows = store.usage_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].usage.input, 10);
    assert_eq!(rows[0].usage.cache_write_1h, Some(3));
    assert_eq!(rows[0].usage.reasoning, Some(4));
}

/// A retained record whose payload no longer fits the typed schema is
/// preserved as a payload-carrying custom entry — the migration must
/// not abort on one bad record (upstream passes payloads through).
#[test]
fn v3_migration_preserves_schema_drifting_records() {
    let tmp = tempfile::tempdir().unwrap();
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s10\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"ok\",\"timestamp\":1}}\n",
        // Unknown message role: cannot materialize as a typed entry.
        "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"message\":{\"role\":\"wizard\",\"content\":\"abra\",\"timestamp\":2},\"extraField\":{\"nested\":true}}\n",
        "{\"type\":\"message\",\"id\":\"m3\",\"parentId\":\"m2\",\"timestamp\":\"2026-01-01T00:00:03.000Z\",\"message\":{\"role\":\"user\",\"content\":\"after\",\"timestamp\":3}}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let store = V4Store::open(&path).unwrap();
    assert!(store.was_legacy_v3());
    assert_eq!(store.entries().len(), 3, "{:?}", store.entries());
    let V4Entry::Custom {
        custom_type, data, ..
    } = &store.entries()[1]
    else {
        panic!("expected custom entry, got {:?}", store.entries()[1]);
    };
    assert_eq!(custom_type, "message");
    let data = data.as_ref().expect("payload preserved");
    assert_eq!(data["message"]["role"], "wizard");
    assert_eq!(data["extraField"]["nested"], true);
    // The tree stays linked through the preserved record.
    assert_eq!(
        store.entries()[2].parent_id(),
        Some(store.entries()[1].id())
    );
}

/// Compactions whose tail can neither be read from a checkpoint nor
/// rebuilt (missing `firstKeptEntryId`, or a boundary unreachable from a
/// null parent) fail the migration instead of silently truncating the
/// context to an empty tail.
#[test]
fn v3_migration_rejects_boundaryless_compactions() {
    let tmp = tempfile::tempdir().unwrap();
    // No retainedTail AND no firstKeptEntryId.
    let no_boundary = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s11\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
        "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"summary\":\"s\",\"tokensBefore\":10}\n",
    );
    let path = write_v3(&tmp, "b1.jsonl", no_boundary);
    let err = V4Store::open(&path).unwrap_err();
    assert!(
        matches!(
            err,
            tack_session::v4::V4Error::MissingCompactionBoundary { .. }
        ),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), no_boundary);

    // A boundary that can never be reached from a null parent.
    let root_boundary = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s12\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
        "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"summary\":\"s\",\"firstKeptEntryId\":\"m1\",\"tokensBefore\":10}\n",
    );
    let path = write_v3(&tmp, "b2.jsonl", root_boundary);
    let err = V4Store::open(&path).unwrap_err();
    assert!(
        matches!(
            err,
            tack_session::v4::V4Error::CompactionBoundaryNotOnBranch { .. }
        ),
        "{err:?}"
    );
}

/// Unparseable timestamps fall back to the nearest known time instead of
/// aborting the migration (upstream's `Date.parse` tolerance); date-only
/// stamps parse at midnight UTC.
#[test]
fn v3_migration_tolerates_bad_timestamps() {
    let tmp = tempfile::tempdir().unwrap();
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s13\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"not a date\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
        "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-02\",\"message\":{\"role\":\"user\",\"content\":\"y\",\"timestamp\":2}}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let store = V4Store::open(&path).unwrap();
    assert!(store.was_legacy_v3());
    // Bad timestamp → the header creation time (the seed fallback).
    assert_eq!(store.entries()[0].base().timestamp, 1767225600000);
    // Date-only timestamp → midnight UTC of that day.
    assert_eq!(store.entries()[1].base().timestamp, 1767312000000);
}

/// Empty-string session names are skipped and empty-string labels count
/// as cleared (upstream's truthiness rules).
#[test]
fn v3_migration_skips_empty_names_and_labels() {
    let tmp = tempfile::tempdir().unwrap();
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s14\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
        "{\"type\":\"label\",\"id\":\"l1\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"targetId\":\"m1\",\"label\":\"real\"}\n",
        "{\"type\":\"label\",\"id\":\"l2\",\"parentId\":\"l1\",\"timestamp\":\"2026-01-01T00:00:03.000Z\",\"targetId\":\"m1\",\"label\":\"\"}\n",
        "{\"type\":\"session_info\",\"id\":\"si1\",\"parentId\":\"l2\",\"timestamp\":\"2026-01-01T00:00:04.000Z\",\"name\":\"\"}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let store = V4Store::open(&path).unwrap();
    assert_eq!(store.session_name(), None, "empty name not written");
    let m1 = store.entries()[0].id().to_string();
    assert_eq!(store.get_label(&m1), None, "empty label clears");
}

/// A non-string `parentId` can never resolve: fail loudly instead of
/// silently re-rooting the record (upstream fails the same lookup).
#[test]
fn v3_migration_rejects_non_string_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s15\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":123,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let err = V4Store::open(&path).unwrap_err();
    assert!(
        matches!(err, tack_session::v4::V4Error::MissingLegacyParent { .. }),
        "{err:?}"
    );
}

/// A `branchSummary` message projected into a compaction's retained tail
/// carries `fromId: null` for a root source (the v4/upstream wire
/// shape), not the v3 entry-level `"root"` sentinel.
#[test]
fn v3_migration_tail_branch_summary_uses_null_from_id() {
    let tmp = tempfile::tempdir().unwrap();
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s16\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
        "{\"type\":\"branch_summary\",\"id\":\"bs1\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"fromId\":\"root\",\"summary\":\"recap\"}\n",
        "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":\"bs1\",\"timestamp\":\"2026-01-01T00:00:03.000Z\",\"summary\":\"s\",\"firstKeptEntryId\":\"bs1\",\"tokensBefore\":10}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let store = V4Store::open(&path).unwrap();
    // The branch summary ENTRY maps the sentinel to a null fromId too.
    let V4Entry::BranchSummary { from_id, .. } = &store.entries()[1] else {
        panic!("expected branch summary");
    };
    assert_eq!(*from_id, None);
    let V4Entry::Compaction { retained_tail, .. } = &store.entries()[2] else {
        panic!("expected compaction");
    };
    assert_eq!(retained_tail.len(), 1, "{retained_tail:?}");
    let AgentMessage::BranchSummary(b) = &retained_tail[0] else {
        panic!("expected branchSummary message, got {:?}", retained_tail[0]);
    };
    assert_eq!(b.from_id, None);
    // ... and the serialized wire shape is literally `"fromId":null`.
    let json = serde_json::to_value(&retained_tail[0]).unwrap();
    assert_eq!(json["fromId"], serde_json::Value::Null);
}

// --- pi.* interoperability --------------------------------------------------

/// A pi-written session (upstream `pi.*` namespaces) resumes sensibly:
/// tip, lane configuration, session name and labels are honored as
/// read-only fallbacks; `tack.*` rows win once both exist.
#[test]
fn v4_store_reads_pi_namespaces_as_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pi.jsonl");
    let mut store = V4Store::create(&path, header("p1"), vec![]).unwrap();
    let entry_id = store
        .commit(vec![
            tack_session::v4::V4NewWrite::Entry(V4Entry::new_message(
                "e1".to_string(),
                AgentMessage::user("pi hello"),
            )),
            tack_session::v4::V4NewWrite::ValueSet {
                namespace: "pi.branch.tip".to_string(),
                key: "main".to_string(),
                value: Value::String("e1".to_string()),
            },
            tack_session::v4::V4NewWrite::ValueSet {
                namespace: "pi.lane.config".to_string(),
                key: "main".to_string(),
                value: serde_json::json!({
                    "model": {"provider": "anthropic", "modelId": "claude"},
                    "thinkingLevel": "high",
                    "activeToolNames": [],
                }),
            },
            tack_session::v4::V4NewWrite::ValueSet {
                namespace: "pi.session.name".to_string(),
                key: String::new(),
                value: Value::String("pi session".to_string()),
            },
            tack_session::v4::V4NewWrite::ValueSet {
                namespace: "pi.entry.label".to_string(),
                key: "e1".to_string(),
                value: Value::String("pi label".to_string()),
            },
        ])
        .unwrap();
    let _ = entry_id;

    assert_eq!(store.branch_tip("main"), Some(Some("e1".to_string())));
    let config = store.lane_config("main").expect("pi lane config honored");
    assert_eq!(config.model.model_id, "claude");
    assert_eq!(store.session_name().as_deref(), Some("pi session"));
    assert_eq!(store.get_label("e1").as_deref(), Some("pi label"));

    // Once a `tack.*` row exists it wins over the pi fallback.
    store
        .commit(vec![tack_session::v4::V4NewWrite::ValueSet {
            namespace: "tack.session.name".to_string(),
            key: String::new(),
            value: Value::String("tack name".to_string()),
        }])
        .unwrap();
    assert_eq!(store.session_name().as_deref(), Some("tack name"));

    // The fallback survives a reopen.
    drop(store);
    let store = V4Store::open(&path).unwrap();
    assert_eq!(store.branch_tip("main"), Some(Some("e1".to_string())));
    assert_eq!(store.get_label("e1").as_deref(), Some("pi label"));
}

/// Forking a pi-written session applies upstream fork semantics to the
/// `pi.*` rows: operation/pending/result state is excluded, lane state
/// resets to idle, and the branch tip crosses on tree scope.
#[test]
fn tree_fork_of_pi_session_excludes_runtime_state() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pi.jsonl");
    let mut store = V4Store::create(&path, header("p1"), vec![]).unwrap();
    store
        .commit(vec![
            tack_session::v4::V4NewWrite::Entry(V4Entry::new_message(
                "e1".to_string(),
                AgentMessage::user("pi hello"),
            )),
            tack_session::v4::V4NewWrite::ValueSet {
                namespace: "pi.branch.tip".to_string(),
                key: "main".to_string(),
                value: Value::String("e1".to_string()),
            },
            tack_session::v4::V4NewWrite::ValueSet {
                namespace: "pi.lane.state".to_string(),
                key: "main".to_string(),
                value: serde_json::json!({
                    "currentOperationId": "op-1",
                    "lastOperationId": null,
                    "inbox": [{"entryId": "e1", "kind": "steer"}],
                }),
            },
            tack_session::v4::V4NewWrite::ValueSet {
                namespace: "pi.op.meta".to_string(),
                key: "op-1".to_string(),
                value: serde_json::json!({"operationId": "op-1"}),
            },
            tack_session::v4::V4NewWrite::ValueSet {
                namespace: "pi.pending.entry".to_string(),
                key: "e2".to_string(),
                value: serde_json::json!({"type": "message"}),
            },
            tack_session::v4::V4NewWrite::ValueSet {
                namespace: "pi.result".to_string(),
                key: "op-0".to_string(),
                value: serde_json::json!({"status": "completed"}),
            },
        ])
        .unwrap();

    let fork = tack_session::v4::run_v4_fork(
        &store,
        &tmp.path().join("fork.jsonl"),
        header("f1"),
        &ForkOptions::Tree { id: None },
    )
    .unwrap();
    assert_eq!(fork.entries().len(), 1, "entries cross");
    assert_eq!(
        fork.branch_tip("main"),
        Some(Some("e1".to_string())),
        "pi tip crosses via the fallback"
    );
    // Runtime state is excluded; lane state is reset to idle.
    assert_eq!(fork.get_value("pi.op.meta", "op-1"), None);
    assert_eq!(fork.get_value("pi.pending.entry", "e2"), None);
    assert_eq!(fork.get_value("pi.result", "op-0"), None);
    assert_eq!(
        fork.get_value("pi.lane.state", "main"),
        Some(&serde_json::json!({
            "currentOperationId": null,
            "lastOperationId": null,
            "inbox": [],
        }))
    );
}

/// End-to-end: a pi-written session (upstream `pi.*` state, including
/// leftover mid-run operation rows) opens in tack and continues
/// natively — new entries link onto pi's tip, mirrored state moves to
/// `tack.*`, and everything keeps working when pi never reads the file
/// again.
#[test]
fn pi_session_continues_natively_in_tack() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pi.jsonl");
    let mut store = V4Store::create(&path, header("pi-1"), vec![]).unwrap();
    let msg = |id: &str, parent: Option<&str>, text: &str| V4Entry::Message {
        base: tack_session::v4::V4EntryBase {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            seq: 0,
            timestamp: 0,
        },
        message: AgentMessage::user(text),
        terminate: None,
    };
    // A pi session closed mid-run: tip/config/state under `pi.*`, plus
    // operation/pending junk tack must tolerate.
    store
        .commit(vec![
            V4NewWrite::Entry(msg("e1", None, "pi one")),
            V4NewWrite::Entry(msg("e2", Some("e1"), "pi two")),
            V4NewWrite::ValueSet {
                namespace: "pi.branch.tip".to_string(),
                key: "main".to_string(),
                value: Value::String("e2".to_string()),
            },
            V4NewWrite::ValueSet {
                namespace: "pi.lane.config".to_string(),
                key: "main".to_string(),
                value: serde_json::json!({
                    "model": {"provider": "anthropic", "modelId": "claude"},
                    "thinkingLevel": "medium",
                    "activeToolNames": ["read"],
                }),
            },
            V4NewWrite::ValueSet {
                namespace: "pi.lane.state".to_string(),
                key: "main".to_string(),
                value: serde_json::json!({
                    "currentOperationId": "op-1",
                    "lastOperationId": null,
                    "inbox": [],
                }),
            },
            V4NewWrite::ValueSet {
                namespace: "pi.op.meta".to_string(),
                key: "op-1".to_string(),
                value: serde_json::json!({"operationId": "op-1"}),
            },
            V4NewWrite::ValueSet {
                namespace: "pi.pending.entry".to_string(),
                key: "draft".to_string(),
                value: serde_json::json!({"type": "message"}),
            },
            V4NewWrite::ValueSet {
                namespace: "pi.session.name".to_string(),
                key: String::new(),
                value: Value::String("pi session".to_string()),
            },
            V4NewWrite::ValueSet {
                namespace: "pi.entry.label".to_string(),
                key: "e1".to_string(),
                value: Value::String("pi label".to_string()),
            },
        ])
        .unwrap();
    drop(store);

    // Opens as an ordinary v4 session, resuming from pi's tip.
    let mut mgr = SessionManager::open(&path, None).unwrap();
    assert_eq!(mgr.leaf_id(), Some("e2"));
    assert_eq!(mgr.entries().len(), 2);
    assert_eq!(mgr.get_label("e1").as_deref(), Some("pi label"));

    // Work continues natively: new entries must link onto pi's tip.
    mgr.append_model_change("anthropic", "claude-opus-4.5")
        .unwrap();
    mgr.append_thinking_level_change("high").unwrap();
    mgr.append_message(AgentMessage::user("tack three"))
        .unwrap();
    let label_entry = mgr
        .append_label_change("e1", Some("tack label".to_string()))
        .unwrap();
    let entries = mgr.entries();
    assert_eq!(entries.len(), 6, "{entries:?}");
    assert_eq!(
        entries[2].parent_id(),
        Some("e2"),
        "the first tack entry parents onto pi's tip"
    );
    assert_eq!(mgr.leaf_id(), Some(label_entry.as_str()));
    drop(mgr);

    // Store view: `tack.*` rows take precedence, `pi.*` still falls back
    // where no tack row exists.
    let store = V4Store::open(&path).unwrap();
    assert_eq!(
        store.branch_tip("main"),
        Some(Some(label_entry.clone())),
        "tack tip row wins"
    );
    let config = store.lane_config("main").unwrap();
    assert_eq!(config.model.model_id, "claude-opus-4.5");
    assert_eq!(config.thinking_level, "high");
    assert_eq!(
        config.active_tool_names,
        vec!["read".to_string()],
        "pi's tool list was preserved into the first tack lane config"
    );
    assert_eq!(
        store.session_name().as_deref(),
        Some("pi session"),
        "no tack name yet — pi fallback still applies"
    );
    assert_eq!(store.get_label("e1").as_deref(), Some("tack label"));
    assert!(store.has_complete_lane("main"));

    // Reopen: the hybrid session keeps working as a normal tack session.
    let mgr2 = SessionManager::open(&path, None).unwrap();
    assert_eq!(mgr2.leaf_id(), Some(label_entry.as_str()));
    assert_eq!(mgr2.entries().len(), 6);
    let ctx = mgr2.build_session_context();
    assert_eq!(ctx.thinking_level, "high");
    assert_eq!(
        ctx.model,
        Some(("anthropic".to_string(), "claude-opus-4.5".to_string()))
    );

    // ... and a tree fork stays clean (pi runtime state excluded/reset).
    let fork = tack_session::v4::run_v4_fork(
        &store,
        &tmp.path().join("fork.jsonl"),
        header("f1"),
        &ForkOptions::Tree { id: None },
    )
    .unwrap();
    assert_eq!(fork.entries().len(), 6);
    assert_eq!(fork.get_value("pi.op.meta", "op-1"), None);
    assert_eq!(fork.get_value("pi.pending.entry", "draft"), None);
    assert_eq!(fork.branch_tip("main"), Some(Some(label_entry)));
}

/// Helper kept for parity with other test modules.
#[allow(dead_code)]
fn path_display(p: &Path) -> String {
    p.display().to_string()
}

// --- SessionManager live v4 write path ------------------------------------

#[test]
fn session_manager_v4_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut mgr = SessionManager::create(&cwd, Some(session_dir.clone())).unwrap();
    let path = mgr.session_file().unwrap().to_path_buf();

    // New sessions are born v4.
    let first_line =
        std::io::BufRead::lines(std::io::BufReader::new(std::fs::File::open(&path).unwrap()))
            .next()
            .unwrap()
            .unwrap();
    assert!(
        first_line.contains("\"kind\":\"header\""),
        "v4 header: {first_line}"
    );

    let m1 = mgr.append_message(AgentMessage::user("first")).unwrap();
    mgr.append_model_change("anthropic", "claude-opus-4.5")
        .unwrap();
    mgr.append_thinking_level_change("high").unwrap();
    mgr.append_message(AgentMessage::user("second")).unwrap();
    mgr.append_session_info(Some("live v4".to_string()))
        .unwrap();
    mgr.append_label_change(&m1, Some("tag".to_string()))
        .unwrap();
    let leaf_before = mgr.leaf_id().unwrap().to_string();
    drop(mgr);

    // Store-level replay agrees with the manager's view.
    let store = V4Store::open(&path).unwrap();
    assert!(!store.was_legacy_v3(), "born-v4, no migration");
    assert_eq!(store.session_name().as_deref(), Some("live v4"));
    assert_eq!(store.get_label(&m1).as_deref(), Some("tag"));
    let config = store.lane_config("main").expect("lane config mirrored");
    assert_eq!(config.model.provider, "anthropic");
    assert_eq!(config.model.model_id, "claude-opus-4.5");
    assert_eq!(config.thinking_level, "high");
    assert!(store.has_complete_lane("main"));

    // Manager reopen: full entry fidelity (change entries typed).
    let mut mgr2 = SessionManager::open(&path, None).unwrap();
    use tack_session::SessionEntry;
    let entries = mgr2.entries();
    assert_eq!(entries.len(), 6, "{entries:?}");
    assert!(matches!(entries[0], SessionEntry::Message { .. }));
    assert!(matches!(entries[1], SessionEntry::ModelChange { .. }));
    assert!(matches!(
        entries[2],
        SessionEntry::ThinkingLevelChange { .. }
    ));
    assert!(matches!(entries[3], SessionEntry::Message { .. }));
    assert!(matches!(entries[4], SessionEntry::SessionInfo { .. }));
    assert!(matches!(entries[5], SessionEntry::Label { .. }));
    assert_eq!(mgr2.leaf_id(), Some(leaf_before.as_str()));
    let ctx = mgr2.build_session_context();
    assert_eq!(ctx.thinking_level, "high");
    assert_eq!(
        ctx.model,
        Some(("anthropic".to_string(), "claude-opus-4.5".to_string()))
    );
    assert_eq!(mgr2.get_label(&m1).as_deref(), Some("tag"));

    // Appends after reopen are v4 transactions and move the tip.
    let m3 = mgr2.append_message(AgentMessage::user("third")).unwrap();
    let store2 = V4Store::open(&path).unwrap();
    assert_eq!(store2.entries().len(), 7);
    assert_eq!(store2.branch_tip("main"), Some(Some(m3)));
}

#[test]
fn v3_session_continues_as_v4_after_open() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut mgr =
        SessionManager::create_with_backend(&cwd, Some(session_dir.clone()), SessionBackend::Jsonl)
            .unwrap();
    mgr.append_message(AgentMessage::user("one")).unwrap();
    mgr.append_message(AgentMessage::user("two")).unwrap();
    let path = mgr.session_file().unwrap().to_path_buf();
    drop(mgr);

    // Default open migrates the v3 file in place…
    let mut mgr = SessionManager::open(&path, None).unwrap();
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        on_disk
            .lines()
            .next()
            .unwrap()
            .contains("\"kind\":\"header\""),
        "migrated in place: {}",
        on_disk.lines().next().unwrap()
    );
    assert!(std::path::Path::new(&format!("{}.bak", path.display())).exists());

    // …and further appends are v4 transactions.
    let m3 = mgr.append_message(AgentMessage::user("three")).unwrap();
    let store = V4Store::open(&path).unwrap();
    assert_eq!(store.entries().len(), 3);
    assert_eq!(store.branch_tip("main"), Some(Some(m3.clone())));
    let mgr2 = SessionManager::open(&path, None).unwrap();
    assert_eq!(mgr2.entries().len(), 3);
    assert_eq!(mgr2.leaf_id(), Some(m3.as_str()));
}

#[test]
fn live_v4_fork_carries_parent_pointer_and_projection() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut mgr = SessionManager::create(&cwd, Some(session_dir.clone())).unwrap();
    mgr.append_message(AgentMessage::user("one")).unwrap();
    mgr.append_model_change("anthropic", "claude-opus-4.5")
        .unwrap();
    mgr.append_thinking_level_change("high").unwrap();
    mgr.append_message(AgentMessage::user("two")).unwrap();
    let src_id = mgr.session_id().to_string();
    let path = mgr.session_file().unwrap().to_path_buf();
    drop(mgr);

    let fork_dir = tmp.path().join("fork-sessions");
    let forked = SessionManager::fork_from_in(&path, &cwd, &fork_dir).unwrap();
    assert_ne!(forked.session_id(), src_id);
    assert_eq!(forked.entries().len(), 4, "stream copied");

    let fork_path = forked.session_file().unwrap().to_path_buf();
    let store = V4Store::open(&fork_path).unwrap();
    assert_eq!(
        store.header().parent_session_id.as_deref(),
        Some(src_id.as_str())
    );
    // Projection: the fork is a complete lane with a fresh idle state.
    assert!(store.has_complete_lane("main"));
    assert_eq!(store.lane_state("main"), Some(LaneState::idle()));
    assert_eq!(store.entries().len(), 4);

    // The fork diverges independently of the source.
    let mut forked = forked;
    forked
        .append_message(AgentMessage::user("fork only"))
        .unwrap();
    let src_store = V4Store::open(&path).unwrap();
    assert_eq!(src_store.entries().len(), 4);
}

#[test]
fn encrypted_v4_session_roundtrip() {
    install_test_key();
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut mgr = SessionManager::create(&cwd, Some(session_dir.clone())).unwrap();
    mgr.append_message(AgentMessage::user("v4 secret text"))
        .unwrap();
    mgr.append_session_info(Some("name secret".to_string()))
        .unwrap();
    let path = mgr.session_file().unwrap().to_path_buf();
    drop(mgr);

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        !content.contains("v4 secret text"),
        "plaintext leak: {content}"
    );
    assert!(
        !content.contains("name secret"),
        "plaintext leak: {content}"
    );
    // Header stays plaintext; every transaction line is ciphertext.
    assert!(
        content
            .lines()
            .next()
            .unwrap()
            .contains("\"kind\":\"header\"")
    );
    let tx_lines: Vec<&str> = content
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert!(!tx_lines.is_empty());
    assert!(
        tx_lines
            .iter()
            .all(|l| tack_session::crypto::is_encrypted_line(l.trim())),
        "all transaction lines encrypted: {content}"
    );

    // Roundtrip through the manager with the key installed.
    let mgr2 = SessionManager::open(&path, None).unwrap();
    assert_eq!(mgr2.entries().len(), 2);
    let names: Vec<Option<String>> = mgr2
        .entries()
        .iter()
        .filter_map(|e| match e {
            tack_session::SessionEntry::SessionInfo { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec![Some("name secret".to_string())]);
}

/// Upstream #9548: a v3 compaction's `systemMessage` survives the v3→v4
/// migration — tack prepends it to the v4 retainedTail (upstream drops
/// it; the prepended form is readable by both implementations).
#[test]
fn v3_migration_preserves_compaction_system_message() {
    let tmp = tempfile::tempdir().unwrap();
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s7\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/work\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"kept\",\"timestamp\":1}}\n",
        "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"summary\":\"sum\",\"firstKeptEntryId\":\"m1\",\"tokensBefore\":100,",
        "\"retainedTail\":[{\"role\":\"user\",\"content\":\"kept\",\"timestamp\":1}],",
        "\"systemMessage\":{\"role\":\"system\",\"content\":\"\",\"sections\":{\"system-prompt\":\"You are Tack.\"},\"timestamp\":2}}\n",
    );
    let path = write_v3(&tmp, "s.jsonl", content);
    let store = V4Store::open(&path).unwrap();
    let V4Entry::Compaction { retained_tail, .. } = &store.entries()[1] else {
        panic!("expected compaction");
    };
    assert_eq!(retained_tail.len(), 2, "{retained_tail:?}");
    let AgentMessage::System(system) = &retained_tail[0] else {
        panic!(
            "expected leading system message, got {:?}",
            retained_tail[0]
        );
    };
    assert_eq!(
        system
            .sections
            .as_ref()
            .and_then(|s| s.get("system-prompt"))
            .and_then(|v| v.as_deref()),
        Some("You are Tack.")
    );
    assert!(matches!(&retained_tail[1], AgentMessage::User(_)));
}
