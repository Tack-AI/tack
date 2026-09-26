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
