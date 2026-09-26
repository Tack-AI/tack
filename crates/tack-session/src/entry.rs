//! Session entry types. Byte-compatible with pi's session format v3
//! (`packages/coding-agent/docs/session-format.md`). Unknown entry types
//! (written by TS pi extensions) are preserved as raw JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tack_agent_core::AgentMessage;
use tack_ai::Usage;

pub const CURRENT_SESSION_VERSION: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub entry_type: String, // always "session"
    #[serde(default)]
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(rename = "parentSession", skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

impl SessionHeader {
    pub fn new(id: String, timestamp: String, cwd: String, parent_session: Option<String>) -> Self {
        SessionHeader {
            entry_type: "session".to_string(),
            version: Some(CURRENT_SESSION_VERSION),
            id,
            timestamp,
            cwd,
            parent_session,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum SessionEntry {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        message: AgentMessage,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        #[serde(default)]
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange {
        #[serde(default)]
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    #[serde(rename = "compaction")]
    Compaction {
        #[serde(default)]
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        summary: String,
        /// Optional in v3: older entries only have firstKeptEntryId.
        #[serde(rename = "firstKeptEntryId", skip_serializing_if = "Option::is_none")]
        first_kept_entry_id: Option<String>,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        /// Self-contained checkpoint: materialized kept messages.
        #[serde(rename = "retainedTail", skip_serializing_if = "Option::is_none")]
        retained_tail: Option<Vec<AgentMessage>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(rename = "fromHook", skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
        /// Complete prompt and tool state at this compaction boundary
        /// (upstream #9548: `systemMessage`). Projected into the context
        /// before the summary. Serialized WITH the `role: "system"`
        /// discriminator, matching upstream.
        #[serde(
            rename = "systemMessage",
            default,
            skip_serializing_if = "Option::is_none",
            with = "tack_ai::serde_system_message"
        )]
        system_message: Option<tack_ai::SystemMessage>,
        /// v1-only field, consumed by the v1→v2 migration
        /// (firstKeptEntryIndex → firstKeptEntryId). Always None after
        /// migration; never written by current versions.
        #[serde(
            rename = "firstKeptEntryIndex",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        first_kept_entry_index: Option<usize>,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        #[serde(default)]
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(rename = "fromHook", skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    #[serde(rename = "custom")]
    Custom {
        #[serde(default)]
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    #[serde(rename = "custom_message")]
    CustomMessage {
        #[serde(default)]
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "customType")]
        custom_type: String,
        content: tack_ai::UserContent,
        display: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    #[serde(rename = "label")]
    Label {
        #[serde(default)]
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    #[serde(rename = "session_info")]
    SessionInfo {
        #[serde(default)]
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl SessionEntry {
    pub fn id(&self) -> &str {
        match self {
            SessionEntry::Message { id, .. }
            | SessionEntry::ModelChange { id, .. }
            | SessionEntry::ThinkingLevelChange { id, .. }
            | SessionEntry::Compaction { id, .. }
            | SessionEntry::BranchSummary { id, .. }
            | SessionEntry::Custom { id, .. }
            | SessionEntry::CustomMessage { id, .. }
            | SessionEntry::Label { id, .. }
            | SessionEntry::SessionInfo { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            SessionEntry::Message { parent_id, .. }
            | SessionEntry::ModelChange { parent_id, .. }
            | SessionEntry::ThinkingLevelChange { parent_id, .. }
            | SessionEntry::Compaction { parent_id, .. }
            | SessionEntry::BranchSummary { parent_id, .. }
            | SessionEntry::Custom { parent_id, .. }
            | SessionEntry::CustomMessage { parent_id, .. }
            | SessionEntry::Label { parent_id, .. }
            | SessionEntry::SessionInfo { parent_id, .. } => parent_id.as_deref(),
        }
    }

    pub fn set_id(&mut self, new_id: String) {
        match self {
            SessionEntry::Message { id, .. }
            | SessionEntry::ModelChange { id, .. }
            | SessionEntry::ThinkingLevelChange { id, .. }
            | SessionEntry::Compaction { id, .. }
            | SessionEntry::BranchSummary { id, .. }
            | SessionEntry::Custom { id, .. }
            | SessionEntry::CustomMessage { id, .. }
            | SessionEntry::Label { id, .. }
            | SessionEntry::SessionInfo { id, .. } => *id = new_id,
        }
    }

    pub fn set_parent_id(&mut self, new_parent: Option<String>) {
        match self {
            SessionEntry::Message { parent_id, .. }
            | SessionEntry::ModelChange { parent_id, .. }
            | SessionEntry::ThinkingLevelChange { parent_id, .. }
            | SessionEntry::Compaction { parent_id, .. }
            | SessionEntry::BranchSummary { parent_id, .. }
            | SessionEntry::Custom { parent_id, .. }
            | SessionEntry::CustomMessage { parent_id, .. }
            | SessionEntry::Label { parent_id, .. }
            | SessionEntry::SessionInfo { parent_id, .. } => *parent_id = new_parent,
        }
    }

    pub fn timestamp(&self) -> &str {
        match self {
            SessionEntry::Message { timestamp, .. }
            | SessionEntry::ModelChange { timestamp, .. }
            | SessionEntry::ThinkingLevelChange { timestamp, .. }
            | SessionEntry::Compaction { timestamp, .. }
            | SessionEntry::BranchSummary { timestamp, .. }
            | SessionEntry::Custom { timestamp, .. }
            | SessionEntry::CustomMessage { timestamp, .. }
            | SessionEntry::Label { timestamp, .. }
            | SessionEntry::SessionInfo { timestamp, .. } => timestamp,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            SessionEntry::Message { .. } => "message",
            SessionEntry::ModelChange { .. } => "model_change",
            SessionEntry::ThinkingLevelChange { .. } => "thinking_level_change",
            SessionEntry::Compaction { .. } => "compaction",
            SessionEntry::BranchSummary { .. } => "branch_summary",
            SessionEntry::Custom { .. } => "custom",
            SessionEntry::CustomMessage { .. } => "custom_message",
            SessionEntry::Label { .. } => "label",
            SessionEntry::SessionInfo { .. } => "session_info",
        }
    }
}

/// One line in a session file: a header, a known entry, or an unknown entry
/// preserved as raw JSON (TS pi extension entries).
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum SessionLine {
    Header(SessionHeader),
    Entry(SessionEntry),
    /// Unknown or unparseable entry type — preserved verbatim.
    Unknown(Value),
}

impl SessionLine {
    /// Parse one JSONL line. Malformed JSON yields `Unknown` with the raw
    /// string wrapped, mirroring TS pi's "skip malformed lines" tolerance
    /// without losing data. Encrypted lines (tack-enc:v1:) decrypt via the
    /// process-global session key when installed.
    pub fn parse(line: &str) -> Option<SessionLine> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let decrypted;
        let trimmed = if crate::crypto::is_encrypted_line(trimmed) {
            match crate::crypto::decrypt_line(trimmed) {
                Some(plain) => {
                    decrypted = plain;
                    &decrypted
                }
                None => {
                    // Bulk readers (session search/listing/stats) scan many
                    // files line by line: an unkeyed process hits one warn
                    // PER ENCRYPTED LINE, flooding stderr over the TUI.
                    // Warn once per process; follow-ups stay at debug.
                    // (Opening such a session still fails loudly —
                    // SessionManager::open rejects undecryptable lines.)
                    static WARNED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        tracing::warn!(
                            "session: undecryptable line skipped (key missing or wrong?); \
                             further skips logged at debug level"
                        );
                    } else {
                        tracing::debug!(
                            "session: undecryptable line skipped (key missing or wrong?)"
                        );
                    }
                    return None;
                }
            }
        } else {
            trimmed
        };
        let value: Value = serde_json::from_str(trimmed).ok()?;
        Some(SessionLine::from_value(value))
    }

    pub fn from_value(value: Value) -> SessionLine {
        let mut value = value;
        let type_tag = value.get("type").and_then(Value::as_str).unwrap_or("");
        if type_tag == "session" {
            return match serde_json::from_value::<SessionHeader>(value.clone()) {
                Ok(header) => SessionLine::Header(header),
                Err(_) => SessionLine::Unknown(value),
            };
        }
        // v2 → v3 migration: hookMessage role was renamed to custom.
        if type_tag == "message"
            && let Some(message) = value.get_mut("message")
            && message.get("role").and_then(Value::as_str) == Some("hookMessage")
        {
            message["role"] = Value::String("custom".to_string());
        }
        match serde_json::from_value::<SessionEntry>(value.clone()) {
            Ok(entry) => SessionLine::Entry(entry),
            Err(_) => SessionLine::Unknown(value),
        }
    }

    pub fn to_json(&self) -> String {
        match self {
            SessionLine::Header(h) => serde_json::to_string(h).expect("header serializes"),
            SessionLine::Entry(e) => serde_json::to_string(e).expect("entry serializes"),
            SessionLine::Unknown(v) => serde_json::to_string(v).expect("value serializes"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Empty/whitespace-only lines carry no data: skipped, not Unknown.
    #[test]
    fn parse_skips_empty_and_whitespace_lines() {
        assert!(SessionLine::parse("").is_none());
        assert!(SessionLine::parse("   ").is_none());
        assert!(SessionLine::parse("\t\r\n").is_none());
    }

    /// Malformed JSON cannot be preserved as JSON at all: dropped (TS pi's
    /// "skip malformed lines" tolerance).
    #[test]
    fn parse_drops_malformed_json() {
        assert!(SessionLine::parse("{not json").is_none());
        assert!(SessionLine::parse("\"unterminated").is_none());
    }

    /// Valid JSON that is not an object is kept verbatim as Unknown rather
    /// than dropped.
    #[test]
    fn parse_preserves_non_object_json_as_unknown() {
        let line = SessionLine::parse("[1,2,3]").unwrap();
        assert_eq!(line, SessionLine::Unknown(serde_json::json!([1, 2, 3])));
        assert_eq!(line.to_json(), "[1,2,3]");
    }

    /// Unknown/future entry types (TS pi extensions, newer format versions)
    /// survive a load + rewrite cycle with their full payload intact.
    #[test]
    fn unknown_entry_types_round_trip_with_payload_intact() {
        let raw =
            r#"{"type":"future_thing","id":"f1","nested":{"a":[1,2,{"b":null}],"z":"keep me"}}"#;
        let line = SessionLine::parse(raw).unwrap();
        let SessionLine::Unknown(value) = &line else {
            panic!("expected Unknown, got {line:?}");
        };
        assert_eq!(value, &serde_json::from_str::<Value>(raw).unwrap());
        // to_json -> parse is a fixed point (key order is normalized by
        // serde_json, the DATA is verbatim).
        let reparsed = SessionLine::parse(&line.to_json()).unwrap();
        assert_eq!(reparsed, line);
    }

    /// A known type tag with a malformed body falls back to Unknown with
    /// the raw object preserved — no partial decode, no silent data loss.
    #[test]
    fn malformed_known_type_falls_back_to_unknown() {
        let raw = r#"{"type":"message","id":"m1","message":"not-an-object"}"#;
        let line = SessionLine::parse(raw).unwrap();
        let SessionLine::Unknown(value) = &line else {
            panic!("expected Unknown, got {line:?}");
        };
        assert_eq!(value, &serde_json::from_str::<Value>(raw).unwrap());
    }

    /// A header-shaped line missing required fields must NOT be treated as
    /// a header (open() would find no header and reject the file) — and
    /// must not be dropped either.
    #[test]
    fn malformed_header_falls_back_to_unknown() {
        let line = SessionLine::parse(r#"{"type":"session","id":"s1"}"#).unwrap();
        assert!(matches!(line, SessionLine::Unknown(_)), "{line:?}");
    }

    /// Normal header and entry lines parse into their typed variants.
    #[test]
    fn parses_header_and_known_entry() {
        let header = SessionLine::parse(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00Z","cwd":"/work"}"#,
        )
        .unwrap();
        let SessionLine::Header(h) = header else {
            panic!("expected Header, got {header:?}");
        };
        assert_eq!(h.version, Some(3));
        assert_eq!(h.id, "s1");
        assert_eq!(h.cwd, "/work");

        let entry = SessionLine::parse(
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"hi","timestamp":1}}"#,
        )
        .unwrap();
        let SessionLine::Entry(SessionEntry::Message { id, message, .. }) = &entry else {
            panic!("expected message entry, got {entry:?}");
        };
        assert_eq!(id, "m1");
        assert!(matches!(message, AgentMessage::User(_)));
    }

    /// v2 → v3 migration (parse-time): the hookMessage role was renamed to
    /// custom. The entry must decode as a custom message with customType,
    /// content and display flag preserved, and must never serialize back
    /// with the old role.
    #[test]
    fn hook_message_role_is_renamed_to_custom() {
        let raw = concat!(
            "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,",
            "\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"role\":\"hookMessage\",",
            "\"customType\":\"hook:init\",\"content\":\"hook output\",\"display\":true,",
            "\"details\":{\"exit\":0},\"timestamp\":42}}"
        );
        let line = SessionLine::parse(raw).unwrap();
        let SessionLine::Entry(SessionEntry::Message {
            message: AgentMessage::Custom(custom),
            ..
        }) = &line
        else {
            panic!("expected custom message entry, got {line:?}");
        };
        assert_eq!(custom.custom_type, "hook:init");
        assert!(custom.display);
        assert_eq!(custom.timestamp, 42);
        assert_eq!(
            custom.details,
            Some(serde_json::json!({"exit": 0})),
            "details preserved"
        );
        let tack_ai::UserContent::Text(text) = &custom.content else {
            panic!("expected text content, got {:?}", custom.content);
        };
        assert_eq!(text, "hook output");

        let out = line.to_json();
        assert!(out.contains("\"role\":\"custom\""), "{out}");
        assert!(!out.contains("hookMessage"), "{out}");
    }

    /// The rename is narrowly scoped: only `type == "message"` lines are
    /// rewritten, so other entry types and Unknown lines pass through
    /// untouched even if they contain a role field.
    #[test]
    fn hook_message_rename_only_applies_to_message_entries() {
        // A non-message entry type carrying a role field stays Unknown with
        // the raw value untouched.
        let raw = r#"{"type":"future_thing","message":{"role":"hookMessage"}}"#;
        let line = SessionLine::parse(raw).unwrap();
        let SessionLine::Unknown(value) = &line else {
            panic!("expected Unknown, got {line:?}");
        };
        assert_eq!(
            value.pointer("/message/role").and_then(Value::as_str),
            Some("hookMessage"),
            "non-message lines are never rewritten"
        );

        // A regular user message is not affected by the rename.
        let line = SessionLine::parse(
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"t","message":{"role":"user","content":"hi","timestamp":1}}"#,
        )
        .unwrap();
        assert!(
            matches!(
                line,
                SessionLine::Entry(SessionEntry::Message {
                    message: AgentMessage::User(_),
                    ..
                })
            ),
            "{line:?}"
        );
    }
}
