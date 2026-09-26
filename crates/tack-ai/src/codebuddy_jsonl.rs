//! CodeBuddy native session files (JSONL rebuild + `--resume`).
//!
//! Port of the reference plugin's `cb-session-io.ts` / `session-verify.ts`
//! (pi-codebuddy-sdk). When tack-ai's context diverges from the CLI session
//! (compaction, history edit, abort), the provider rewrites CodeBuddy's own
//! session file from tack-ai's history and respawns the CLI with `--resume`:
//! the rebuilt conversation keeps its multi-turn structure and the CLI's
//! session cache, instead of being flattened into a single synthetic user
//! message (the transcript-replay fallback in codebuddy.rs).
//!
//! Format (CodeBuddy `~/.codebuddy/projects/<path-hash>/<session-id>.jsonl`):
//! one JSON record per line, `id`/`parentId` chained:
//! ```json
//! {"id":"…","timestamp":1730000000000,"type":"message","role":"user",
//!  "content":[{"type":"input_text","text":"…"}],
//!  "providerData":{"agent":"sdk"},"sessionId":"…","cwd":"…"}
//! {"id":"…","parentId":"…","timestamp":…,"type":"message","role":"assistant",
//!  "status":"completed","content":[{"type":"output_text","text":"…"}],…}
//! ```
//!
//! The projection is deliberately lossy (reference: piToCbMessages) — the
//! JSONL record types carry text only; tool calls/results become inline
//! markers (`[tool:name]`, `[tool_result:id]`), images `[image]`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::types::{ContentBlock, InputContentBlock, Message, UserContent};

/// CodeBuddy config dir: `CODEBUDDY_CONFIG_DIR` or `~/.codebuddy`.
pub(crate) fn codebuddy_dir() -> PathBuf {
    std::env::var_os("CODEBUDDY_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codebuddy"))
}

/// CodeBuddy PathUtils.compressPath parity (reference: projectPathToHash).
pub(crate) fn project_path_to_hash(project_path: &str) -> String {
    let normalized = project_path.trim_end_matches('/');
    let normalized = if normalized.is_empty() {
        project_path
    } else {
        normalized
    };
    let replaced: String = normalized
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect();
    // Strip leading/trailing dashes, collapse runs of dashes.
    let mut out = String::with_capacity(replaced.len());
    let mut last_dash = true; // strips the leading run
    for c in replaced.chars() {
        if c == '-' {
            if !last_dash {
                out.push(c);
            }
            last_dash = true;
        } else {
            out.push(c);
            last_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Session file path under an explicit base dir.
fn session_jsonl_path_in(base: &Path, session_id: &str, cwd: &str) -> PathBuf {
    base.join("projects")
        .join(project_path_to_hash(cwd))
        .join(format!("{session_id}.jsonl"))
}

/// Session file path for a session id + project cwd.
pub(crate) fn session_jsonl_path(session_id: &str, cwd: &str) -> PathBuf {
    session_jsonl_path_in(&codebuddy_dir(), session_id, cwd)
}

/// Random uuid v4 (built on the oauth module's CSPRNG hex).
pub(crate) fn new_uuid() -> String {
    let h = crate::oauth::random_hex(16);
    // RFC 4122 variant (10xx): the first nibble of the 4th group must be
    // 8/9/a/b — setting only the version nibble ('4') leaves the variant
    // bits random, which strict uuid parsers reject.
    let variant = (u8::from_str_radix(&h[16..17], 16).expect("hex nibble") & 0x3) | 0x8;
    format!(
        "{}-{}-4{}-{:x}{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32]
    )
}

/// Lossy text projection of pi messages into JSONL records
/// (reference: piToCbMessages). Each entry is (role, text).
pub(crate) fn pi_to_cb_records(messages: &[Message]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let text = match &user.content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            InputContentBlock::Text { text, .. } => text.clone(),
                            InputContentBlock::Image { .. } => "[image]".to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                out.push(("user".to_string(), text));
            }
            Message::Assistant(assistant) => {
                let text = assistant
                    .content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text, .. } => text.clone(),
                        ContentBlock::ToolCall { name, .. } => format!("[tool:{name}]"),
                        ContentBlock::Thinking { .. } => "[thinking]".to_string(),
                        ContentBlock::Image { .. } => "[image]".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push(("assistant".to_string(), text));
            }
            // Tool results become user records with an inline marker.
            Message::ToolResult(result) => {
                out.push((
                    "user".to_string(),
                    format!("[tool_result:{}]", result.tool_call_id),
                ));
            }
            // System messages become user records with an inline marker
            // (codebuddy has no system role in this record format).
            Message::System(system) => {
                let text = match &system.content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            InputContentBlock::Text { text, .. } => Some(text.clone()),
                            InputContentBlock::Image { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join(
                            "
",
                        ),
                };
                if !text.is_empty() {
                    out.push(("user".to_string(), format!("[system]{text}")));
                }
            }
        }
    }
    out
}

/// Write the session JSONL (reference: writeCbJsonl). One shared timestamp,
/// records chained by id/parentId; the first record has no parentId.
pub(crate) fn write_session_jsonl(
    session_id: &str,
    cwd: &str,
    records: &[(String, String)],
) -> Result<PathBuf, String> {
    write_session_jsonl_in(&codebuddy_dir(), session_id, cwd, records)
}

/// write_session_jsonl under an explicit base dir (tests).
fn write_session_jsonl_in(
    base: &Path,
    session_id: &str,
    cwd: &str,
    records: &[(String, String)],
) -> Result<PathBuf, String> {
    let path = session_jsonl_path_in(base, session_id, cwd);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("session dir create failed: {e}"))?;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut lines: Vec<String> = Vec::with_capacity(records.len());
    let mut parent_id: Option<String> = None;
    for (role, text) in records {
        let id = new_uuid();
        let mut record = json!({
            "id": id,
            "timestamp": timestamp,
            "type": "message",
            "role": role,
            "providerData": { "agent": "sdk" },
            "sessionId": session_id,
            "cwd": cwd,
        });
        if let Some(parent) = &parent_id {
            record["parentId"] = json!(parent);
        }
        if role == "assistant" {
            record["status"] = json!("completed");
            record["content"] = json!([{ "type": "output_text", "text": text }]);
        } else {
            record["content"] = json!([{ "type": "input_text", "text": text }]);
        }
        lines.push(record.to_string());
        parent_id = Some(id);
    }
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    std::fs::write(&path, body).map_err(|e| format!("session write failed: {e}"))?;
    Ok(path)
}

/// Post-write integrity check (reference: session-verify.ts). Returns
/// warnings; empty means the file is sound.
pub(crate) fn verify_written_session(
    path: &Path,
    expected_session_id: &str,
    expected_record_count: usize,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        warnings.push(format!(
            "file unreadable after save — path={}",
            path.display()
        ));
        return warnings;
    };
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() != expected_record_count {
        warnings.push(format!(
            "record count mismatch — expected={expected_record_count} actual={} path={}",
            lines.len(),
            path.display()
        ));
        return warnings;
    }
    let first: Result<Value, _> = serde_json::from_str(lines[0]);
    let last: Result<Value, _> = serde_json::from_str(lines[lines.len() - 1]);
    match (first, last) {
        (Ok(first), Ok(last)) => {
            if first.get("sessionId").and_then(Value::as_str) != Some(expected_session_id)
                || last.get("sessionId").and_then(Value::as_str) != Some(expected_session_id)
            {
                warnings.push(format!(
                    "sessionId drift — expected={expected_session_id} first={:?} last={:?}",
                    first.get("sessionId"),
                    last.get("sessionId")
                ));
            }
        }
        _ => warnings.push(format!("malformed JSONL — path={}", path.display())),
    }
    warnings
}

/// Best-effort removal of a session's file + companion dir (reference:
/// deleteSession) — currently unused (in-place rewrites truncate), kept
/// for future ephemeral-session cleanup.
#[allow(dead_code)]
pub(crate) fn delete_session(session_id: &str, cwd: &str) {
    let path = session_jsonl_path(session_id, cwd);
    let _ = std::fs::remove_file(path);
    let dir = codebuddy_dir()
        .join("projects")
        .join(project_path_to_hash(cwd))
        .join(session_id);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::types::{AssistantMessage, ToolResultMessage};

    #[test]
    fn project_hash_matches_reference() {
        assert_eq!(
            project_path_to_hash("/data/github/tack"),
            "data-github-tack"
        );
        assert_eq!(project_path_to_hash("/"), "");
        assert_eq!(
            project_path_to_hash("C:\\Users\\me\\proj"),
            "C-Users-me-proj"
        );
        assert_eq!(
            project_path_to_hash("/a//b/"),
            "a-b",
            "trailing slashes + collapsed runs"
        );
    }

    #[test]
    fn uuid_v4_shape() {
        for _ in 0..64 {
            let id = new_uuid();
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(parts.len(), 5, "{id}");
            assert_eq!(parts[0].len(), 8);
            assert_eq!(parts[2].len(), 4);
            assert!(parts[2].starts_with('4'), "version nibble: {id}");
            assert_eq!(parts[3].len(), 4);
            assert!(
                parts[3].starts_with(['8', '9', 'a', 'b']),
                "RFC 4122 variant nibble: {id}"
            );
        }
    }

    #[test]
    fn records_project_lossy_text() {
        let assistant = AssistantMessage {
            content: vec![
                ContentBlock::text("hello"),
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    thinking_signature: None,
                    redacted: None,
                },
                ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "bash".into(),
                    arguments: json!({}),
                    thought_signature: None,
                    namespace: None,
                },
            ],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            diagnostics: None,
            usage: crate::types::Usage::zero(),
            stop_reason: crate::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        };
        let result = ToolResultMessage {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            content: vec![InputContentBlock::text("ok")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        };
        let records = pi_to_cb_records(&[
            Message::user("hi"),
            Message::Assistant(assistant),
            Message::ToolResult(result),
        ]);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0], ("user".to_string(), "hi".to_string()));
        assert_eq!(records[1].0, "assistant");
        assert_eq!(records[1].1, "hello\n[thinking]\n[tool:bash]");
        assert_eq!(
            records[2],
            ("user".to_string(), "[tool_result:t1]".to_string())
        );
    }

    #[test]
    fn write_and_verify_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![
            ("user".to_string(), "one".to_string()),
            ("assistant".to_string(), "two".to_string()),
        ];
        let path = write_session_jsonl_in(dir.path(), "sid-1", "/tmp/proj", &records).unwrap();
        assert!(verify_written_session(&path, "sid-1", 2).is_empty());
        // parentId chain: first record has none, second points at first.
        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert!(lines[0].get("parentId").is_none());
        assert_eq!(
            lines[1].get("parentId").and_then(Value::as_str),
            lines[0].get("id").and_then(Value::as_str)
        );
        assert_eq!(lines[1]["content"][0]["type"], "output_text");
        assert_eq!(lines[1]["status"], "completed");
        assert_eq!(lines[0]["providerData"]["agent"], "sdk");
        // Verify catches a wrong id / wrong count.
        assert!(!verify_written_session(&path, "other", 2).is_empty());
        assert!(!verify_written_session(&path, "sid-1", 5).is_empty());
    }
}
