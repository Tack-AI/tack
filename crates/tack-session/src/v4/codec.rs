//! Line-level codec for v4 JSONL storage: header detection (v4 vs legacy
//! v3) and transaction parse/serialize. Port of upstream
//! `jsonl/codec.ts` + the parsing half of `jsonl/io.ts`.

use serde_json::Value;

use super::types::{V4_FORMAT_VERSION, V4Header, V4Write};
use crate::entry::SessionHeader;

/// A parsed first line: either a v4 storage header or a legacy v3 session
/// header (upstream `JsonlParsedSessionHeader`).
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedSessionHeader {
    /// Format-4 storage header.
    V4(V4Header),
    /// Legacy v3 session header (`{type:"session", version:3, ...}`).
    LegacyV3(SessionHeader),
}

/// Validate a decoded v4 header (upstream `isJsonlStorageHeader`).
fn is_valid_v4_header(header: &V4Header) -> bool {
    header.kind == "header" && header.v == V4_FORMAT_VERSION && header.storage_version >= 1
}

/// Parse the first line of a session file (upstream
/// `parseJsonlSessionHeader`). The line must already be decrypted —
/// headers are never encrypted in tack.
pub fn parse_session_header(line: &str) -> Option<ParsedSessionHeader> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("kind").and_then(Value::as_str) == Some("header") {
        let header: V4Header = serde_json::from_value(value).ok()?;
        return is_valid_v4_header(&header).then_some(ParsedSessionHeader::V4(header));
    }
    if value.get("type").and_then(Value::as_str) == Some("session")
        && value.get("version").and_then(Value::as_u64) == Some(3)
    {
        let header: SessionHeader = serde_json::from_value(value).ok()?;
        return Some(ParsedSessionHeader::LegacyV3(header));
    }
    None
}

/// Validate one committed write (upstream `parseCommittedWrite`): `seq`
/// must be ≥ 1; entry timestamps ≥ 0 is guaranteed by `u64`.
fn validate_write(write: &V4Write) -> Result<(), String> {
    if write.seq() < 1 {
        return Err("Invalid JSONL write seq".to_string());
    }
    Ok(())
}

/// Parse one transaction line into its writes (upstream
/// `parseJsonlTransaction`): a line is one write object or an array of
/// write objects. Empty arrays decode to zero writes.
pub fn parse_transaction(line: &str) -> Result<Vec<V4Write>, String> {
    let value: Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("not valid JSON: {e}"))?;
    let values: Vec<Value> = match value {
        Value::Array(items) => items,
        single => vec![single],
    };
    let mut writes = Vec::with_capacity(values.len());
    for item in values {
        let write: V4Write =
            serde_json::from_value(item).map_err(|e| format!("invalid transaction write: {e}"))?;
        validate_write(&write)?;
        writes.push(write);
    }
    Ok(writes)
}

/// Serialize one transaction (upstream `serializeJsonlTransaction`):
/// a single write is a bare object, several writes an array.
pub fn serialize_transaction(writes: &[V4Write]) -> String {
    match writes {
        [single] => serde_json::to_string(single).expect("write serializes"),
        multi => serde_json::to_string(multi).expect("writes serialize"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::v4::types::{V4Entry, V4EntryBase, V4ValueOp};

    #[test]
    fn detects_v4_and_legacy_v3_headers() {
        let v4 =
            r#"{"v":4,"kind":"header","id":"s1","storageVersion":1,"createdAt":100,"cwd":"/work"}"#;
        let Some(ParsedSessionHeader::V4(header)) = parse_session_header(v4) else {
            panic!("expected v4 header");
        };
        assert_eq!(header.id, "s1");
        assert_eq!(header.created_at, 100);

        let v3 = r#"{"type":"session","version":3,"id":"s2","timestamp":"2026-01-01T00:00:00Z","cwd":"/work"}"#;
        let Some(ParsedSessionHeader::LegacyV3(header)) = parse_session_header(v3) else {
            panic!("expected legacy v3 header");
        };
        assert_eq!(header.id, "s2");

        // Unsupported: wrong version, wrong kind, garbage.
        assert!(
            parse_session_header(
                r#"{"v":3,"kind":"header","id":"s","storageVersion":1,"createdAt":0,"cwd":"/"}"#
            )
            .is_none()
        );
        assert!(
            parse_session_header(
                r#"{"type":"session","version":2,"id":"s","timestamp":"t","cwd":"/"}"#
            )
            .is_none()
        );
        assert!(parse_session_header("{not json").is_none());
    }

    #[test]
    fn transaction_single_write_is_bare_object() {
        let write = V4Write::Value {
            op: V4ValueOp::Set {
                seq: 1,
                namespace: "tack.session.name".to_string(),
                key: String::new(),
                value: Value::String("n".to_string()),
            },
        };
        let line = serialize_transaction(std::slice::from_ref(&write));
        assert!(!line.starts_with('['), "{line}");
        let parsed = parse_transaction(&line).unwrap();
        assert_eq!(parsed, vec![write]);
    }

    #[test]
    fn transaction_multiple_writes_is_array() {
        let writes = vec![
            V4Write::Entry {
                entry: V4Entry::Message {
                    base: V4EntryBase {
                        id: "e1".to_string(),
                        parent_id: None,
                        seq: 1,
                        timestamp: 5,
                    },
                    message: tack_agent_core::AgentMessage::user("hi"),
                    terminate: None,
                },
            },
            V4Write::Value {
                op: V4ValueOp::Set {
                    seq: 2,
                    namespace: "tack.branch.tip".to_string(),
                    key: "main".to_string(),
                    value: Value::String("e1".to_string()),
                },
            },
        ];
        let line = serialize_transaction(&writes);
        assert!(line.starts_with('['), "{line}");
        let parsed = parse_transaction(&line).unwrap();
        assert_eq!(parsed, writes);
    }

    #[test]
    fn rejects_seq_zero_and_malformed_writes() {
        assert!(
            parse_transaction(
                r#"{"kind":"value","op":"delete","seq":0,"namespace":"a","key":"b"}"#
            )
            .is_err()
        );
        assert!(parse_transaction(r#"{"kind":"nope","seq":1}"#).is_err());
        assert!(parse_transaction("{not json").is_err());
        // Unknown value op.
        assert!(
            parse_transaction(r#"{"kind":"value","op":"merge","seq":1,"namespace":"a","key":"b"}"#)
                .is_err()
        );
    }
}
