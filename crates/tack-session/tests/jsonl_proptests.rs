//! Property tests for the session JSONL parser: arbitrary lines must never
//! panic; serialized entries must round-trip.
#![allow(clippy::unwrap_used)]

use proptest::prelude::*;
use tack_session::{SessionEntry, SessionLine};

proptest! {
    /// Garbage lines: parse must return Some/None, never panic.
    #[test]
    fn session_line_parse_never_panics(line in ".*") {
        let _ = SessionLine::parse(&line);
    }

    /// Round-trip: serialized message entries parse back to the same value.
    #[test]
    fn message_entries_roundtrip(text in ".*", id in "[a-z0-9]{1,12}") {
        let entry = SessionEntry::Message {
            id: id.clone(),
            parent_id: None,
            timestamp: "2026-08-29T00:00:00Z".to_string(),
            message: tack_agent_core::AgentMessage::user(text),
        };
        let line = serde_json::to_string(&entry).unwrap();
        let parsed = SessionLine::parse(&line);
        match parsed {
            Some(SessionLine::Entry(parsed_entry)) => {
                let re_serialized = serde_json::to_string(&parsed_entry).unwrap();
                prop_assert_eq!(line, re_serialized);
            }
            other => panic!("expected entry, got {other:?}"),
        }
    }
}

/// SessionEntry is Serialize; confirm the variants used here exist.
#[test]
fn entry_shape_sanity() {
    let entry = SessionEntry::Message {
        id: "a".into(),
        parent_id: None,
        timestamp: "2026-08-29T00:00:00Z".into(),
        message: tack_agent_core::AgentMessage::user("hi"),
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains("\"message\""));
}
