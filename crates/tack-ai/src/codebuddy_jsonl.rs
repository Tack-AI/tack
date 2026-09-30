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
//! {"id":"…","parentId":"…","timestamp":…,"type":"function_call",
//!  "callId":"…","name":"mcp__tack__bash","arguments":"{…}",…}
//! {"id":"…","parentId":"…","timestamp":…,"type":"function_call_result",
//!  "callId":"…","name":"mcp__tack__bash","status":"completed",
//!  "output":{"type":"text","text":"[{…}]"},…}
//! ```
//!
//! The projection keeps the NATIVE record types the CLI writes itself
//! (`reasoning` / `function_call` / `function_call_result`) — a deliberate
//! deviation from the reference plugin's lossy piToCbMessages (text markers
//! `[tool:name]` / `[tool_result:id]`). The lossy form degrades the model
//! after a compaction-triggered rebuild: with zero function_call records in
//! the resumed history the model mimics the text markers and ends its turn
//! instead of calling tools (observed 2026-09-30 on kimi-k2.8-preview: the
//! post-compaction reply was the literal text "[thinking]\n…" with
//! finish_reason=stop — the turn ended and the session looked hung). Only
//! UNANSWERED calls (no matching result in the rebuilt prefix) and orphan
//! results still degrade to text markers: a resumed CLI must not see calls
//! it would park on, and a result record without its call is invalid.

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

/// One record in the rebuilt session JSONL (see module docs for why tool
/// turns project to native records instead of text markers).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CbRecord {
    /// `message` record (user → input_text; assistant → output_text +
    /// status completed). Assistant records carry the turn's `message_id`
    /// for providerData linkage (see below).
    Message {
        role: &'static str,
        text: String,
        message_id: Option<String>,
    },
    /// `reasoning` record for a thinking block.
    Reasoning { text: String, message_id: String },
    /// `function_call` record; `arguments` is the serialized JSON object
    /// string (CodeBuddy stores it stringified). `reasoning` duplicates the
    /// turn's thinking text, as the CLI's own records do.
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
        reasoning: Option<String>,
        message_id: String,
    },
    /// `function_call_result` record; `output_text` is the JSON-stringified
    /// MCP content-block array the CLI's own records carry.
    FunctionCallResult {
        call_id: String,
        name: String,
        output_text: String,
        message_id: String,
    },
}

/// API-side message id format (`01a0f265e42b7276ba769e8b12123374` — a uuid
/// without dashes). All records of one assistant turn (reasoning, message,
/// calls, results) share it; the replay groups turns by it. Unlinked
/// records confuse the grouping and can send the model into a reasoning
/// loop (observed with the real CLI: 95+ reasoning-only responses).
fn new_message_id() -> String {
    new_uuid().replace('-', "")
}

/// MCP-qualify a bare tack tool name the way the CLI sees it
/// (`mcp__tack__bash`).
fn mcp_tool_name(name: &str) -> String {
    format!("mcp__{}__{name}", crate::codebuddy::MCP_SERVER_NAME)
}

/// JSON-stringified MCP content-block array for a function_call_result's
/// `output.text` (real CLI records stringify `[{"type":"text",…}]`).
fn mcp_output_text(content: &[InputContentBlock]) -> String {
    let blocks: Vec<Value> = content
        .iter()
        .map(|b| match b {
            InputContentBlock::Text { text, .. } => json!({"type":"text","text":text}),
            InputContentBlock::Image { data, mime_type } => {
                json!({"type":"image","data":data,"mimeType":mime_type})
            }
        })
        .collect();
    if blocks.is_empty() {
        return json!([{"type":"text","text":""}]).to_string();
    }
    Value::Array(blocks).to_string()
}

/// Project pi messages into JSONL records. Settled tool turns (every call
/// has its result inside `messages`) become native
/// reasoning/function_call/function_call_result records in the CLI's own
/// ordering (reasoning → assistant text → calls); dangling calls and
/// orphan results degrade to the reference text markers.
pub(crate) fn pi_to_cb_records(messages: &[Message]) -> Vec<CbRecord> {
    use std::collections::{HashMap, HashSet};
    let answered: HashSet<&str> = messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some(r.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    // call_id → (M-qualified name, turn message_id) of every call record
    // emitted so far — pairs a result with its call; results for calls we
    // never emitted (or never saw) degrade to text markers.
    let mut emitted: HashMap<String, (String, String)> = HashMap::new();

    let mut out: Vec<CbRecord> = Vec::new();
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
                out.push(CbRecord::Message {
                    role: "user",
                    text,
                    message_id: None,
                });
            }
            Message::Assistant(assistant) => {
                let message_id = new_message_id();
                let mut thinking_parts: Vec<String> = Vec::new();
                let mut text_parts: Vec<String> = Vec::new();
                let mut calls: Vec<CbRecord> = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Thinking { thinking, .. } => {
                            if !thinking.is_empty() {
                                thinking_parts.push(thinking.clone());
                            }
                        }
                        ContentBlock::Text { text, .. } => text_parts.push(text.clone()),
                        ContentBlock::Image { .. } => text_parts.push("[image]".to_string()),
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            if answered.contains(id.as_str()) {
                                let qualified = mcp_tool_name(name);
                                emitted.insert(id.clone(), (qualified.clone(), message_id.clone()));
                                calls.push(CbRecord::FunctionCall {
                                    call_id: id.clone(),
                                    name: qualified,
                                    arguments: if arguments.is_object() {
                                        arguments.to_string()
                                    } else {
                                        "{}".to_string()
                                    },
                                    reasoning: None, // filled in below
                                    message_id: message_id.clone(),
                                });
                            } else {
                                // Dangling call (its result was cut or the
                                // turn was interrupted): a native record
                                // would leave the resumed CLI waiting on a
                                // result that never comes — text marker.
                                text_parts.push(format!("[tool:{name}]"));
                            }
                        }
                    }
                }
                // Real CLI ordering per turn: reasoning → assistant text →
                // calls; the reasoning text is duplicated onto each call's
                // providerData.reasoning.
                let thinking_text = thinking_parts.join("\n\n");
                if !thinking_text.is_empty() {
                    out.push(CbRecord::Reasoning {
                        text: thinking_text.clone(),
                        message_id: message_id.clone(),
                    });
                }
                let reasoning = (!thinking_text.is_empty()).then_some(thinking_text);
                for call in &mut calls {
                    if let CbRecord::FunctionCall {
                        reasoning: slot, ..
                    } = call
                    {
                        *slot = reasoning.clone();
                    }
                }
                let text = text_parts.join("\n");
                if !text.is_empty() {
                    out.push(CbRecord::Message {
                        role: "assistant",
                        text,
                        message_id: Some(message_id),
                    });
                }
                out.append(&mut calls);
            }
            Message::ToolResult(result) => {
                if let Some((name, message_id)) = emitted.get(&result.tool_call_id) {
                    out.push(CbRecord::FunctionCallResult {
                        call_id: result.tool_call_id.clone(),
                        name: name.clone(),
                        output_text: mcp_output_text(&result.content),
                        message_id: message_id.clone(),
                    });
                } else {
                    out.push(CbRecord::Message {
                        role: "user",
                        text: format!("[tool_result:{}]", result.tool_call_id),
                        message_id: None,
                    });
                }
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
                        .join("\n"),
                };
                if !text.is_empty() {
                    out.push(CbRecord::Message {
                        role: "user",
                        text: format!("[system]{text}"),
                        message_id: None,
                    });
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
    records: &[CbRecord],
) -> Result<PathBuf, String> {
    write_session_jsonl_in(&codebuddy_dir(), session_id, cwd, records)
}

/// write_session_jsonl under an explicit base dir (tests).
fn write_session_jsonl_in(
    base: &Path,
    session_id: &str,
    cwd: &str,
    records: &[CbRecord],
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
    // Session-wide request id (real CLI files share one across all turns).
    let conversation_request_id = new_message_id();
    for record in records {
        let id = new_uuid();
        let mut json = json!({
            "id": id,
            "timestamp": timestamp,
            "sessionId": session_id,
            "cwd": cwd,
        });
        if let Some(parent) = &parent_id {
            json["parentId"] = json!(parent);
        }
        // providerData mirrors the CLI's own linkage: every record carries
        // conversationRequestId; all records of one assistant turn share
        // messageId (real files also carry model/traceId — unknown at
        // rebuild time, and the resume tolerated their absence).
        let mut provider_data = json!({
            "agent": "sdk",
            "conversationRequestId": conversation_request_id,
        });
        match record {
            CbRecord::Message {
                role,
                text,
                message_id,
            } => {
                json["type"] = json!("message");
                json["role"] = json!(role);
                if *role == "assistant" {
                    json["status"] = json!("completed");
                    json["content"] = json!([{ "type": "output_text", "text": text }]);
                } else {
                    json["content"] = json!([{ "type": "input_text", "text": text }]);
                    provider_data["startsNewUserRequest"] = json!(true);
                }
                if let Some(message_id) = message_id {
                    provider_data["messageId"] = json!(message_id);
                }
            }
            CbRecord::Reasoning { text, message_id } => {
                json["type"] = json!("reasoning");
                json["content"] = json!([]);
                json["rawContent"] = json!([{ "type": "reasoning_text", "text": text }]);
                provider_data["messageId"] = json!(message_id);
            }
            CbRecord::FunctionCall {
                call_id,
                name,
                arguments,
                reasoning,
                message_id,
            } => {
                json["type"] = json!("function_call");
                json["callId"] = json!(call_id);
                json["name"] = json!(name);
                json["arguments"] = json!(arguments);
                provider_data["messageId"] = json!(message_id);
                if let Some(reasoning) = reasoning {
                    provider_data["reasoning"] = json!(reasoning);
                }
            }
            CbRecord::FunctionCallResult {
                call_id,
                name,
                output_text,
                message_id,
            } => {
                json["type"] = json!("function_call_result");
                json["name"] = json!(name);
                json["callId"] = json!(call_id);
                json["status"] = json!("completed");
                json["output"] = json!({ "type": "text", "text": output_text });
                provider_data["messageId"] = json!(message_id);
            }
        }
        json["providerData"] = provider_data;
        lines.push(json.to_string());
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
    // Empty file + empty expectation (native rebuild with no settled
    // prefix — split == 0) is sound; there is no first/last record to
    // check for sessionId drift.
    if lines.is_empty() {
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

    /// Assistant/ToolResult builder shared by the projection tests.
    fn tool_turn(call_id: &str, thinking: &str) -> (AssistantMessage, ToolResultMessage) {
        let assistant = AssistantMessage {
            content: vec![
                ContentBlock::text("hello"),
                ContentBlock::Thinking {
                    thinking: thinking.into(),
                    thinking_signature: None,
                    redacted: None,
                },
                ContentBlock::ToolCall {
                    id: call_id.into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls"}),
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
            tool_call_id: call_id.into(),
            tool_name: "bash".into(),
            content: vec![InputContentBlock::text("ok")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        };
        (assistant, result)
    }

    #[test]
    fn settled_tool_turn_projects_native_records() {
        let (assistant, result) = tool_turn("t1", "hmm");
        let records = pi_to_cb_records(&[
            Message::user("hi"),
            Message::Assistant(assistant),
            Message::ToolResult(result),
        ]);
        assert_eq!(records.len(), 5);
        // CLI ordering: reasoning → assistant text → function_call → result;
        // the whole turn shares one message_id.
        let CbRecord::Message {
            role: "user",
            text,
            message_id: None,
        } = &records[0]
        else {
            panic!("expected user message, got {:?}", records[0]);
        };
        assert_eq!(text, "hi");
        let CbRecord::Reasoning { text, message_id } = &records[1] else {
            panic!("expected reasoning, got {:?}", records[1]);
        };
        assert_eq!(text, "hmm");
        let turn_id = message_id.clone();
        assert_eq!(turn_id.len(), 32, "uuid without dashes: {turn_id}");
        assert_eq!(
            records[2],
            CbRecord::Message {
                role: "assistant",
                text: "hello".into(),
                message_id: Some(turn_id.clone()),
            }
        );
        assert_eq!(
            records[3],
            CbRecord::FunctionCall {
                call_id: "t1".into(),
                name: "mcp__tack__bash".into(),
                arguments: "{\"command\":\"ls\"}".into(),
                reasoning: Some("hmm".into()),
                message_id: turn_id.clone(),
            }
        );
        let CbRecord::FunctionCallResult {
            call_id,
            name,
            output_text,
            message_id,
        } = &records[4]
        else {
            panic!("expected function_call_result, got {:?}", records[4]);
        };
        assert_eq!(call_id, "t1");
        assert_eq!(name, "mcp__tack__bash");
        assert_eq!(*message_id, turn_id, "result shares the turn id");
        // Compare parsed — serde_json map key order is not stable.
        assert_eq!(
            serde_json::from_str::<Value>(output_text).unwrap(),
            json!([{"type":"text","text":"ok"}])
        );
    }

    /// A call whose result is not in the rebuilt slice (cut-point dropped
    /// it, or the turn was interrupted) must NOT become a native record —
    /// the resumed CLI would park on a result that never comes.
    #[test]
    fn dangling_call_degrades_to_text_marker() {
        let (assistant, _result) = tool_turn("t1", "");
        let records = pi_to_cb_records(&[Message::user("hi"), Message::Assistant(assistant)]);
        // Empty thinking emits no reasoning record; the call folds into the
        // assistant text as the reference marker.
        let [user, assistant] = &records[..] else {
            panic!("expected 2 records, got {records:?}");
        };
        assert!(matches!(
            user,
            CbRecord::Message {
                role: "user",
                message_id: None,
                ..
            }
        ));
        let CbRecord::Message {
            role: "assistant",
            text,
            message_id: Some(_),
        } = assistant
        else {
            panic!("expected assistant message, got {assistant:?}");
        };
        assert_eq!(text, "hello\n[tool:bash]");
    }

    /// A result without its call record (e.g. the call fell before the
    /// rebuilt slice) degrades to the reference user-text marker.
    #[test]
    fn orphan_result_degrades_to_text_marker() {
        let (_assistant, result) = tool_turn("t1", "");
        let records = pi_to_cb_records(&[Message::user("hi"), Message::ToolResult(result)]);
        assert_eq!(
            records,
            vec![
                CbRecord::Message {
                    role: "user",
                    text: "hi".into(),
                    message_id: None,
                },
                CbRecord::Message {
                    role: "user",
                    text: "[tool_result:t1]".into(),
                    message_id: None,
                },
            ]
        );
    }

    #[test]
    fn write_and_verify_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![
            CbRecord::Message {
                role: "user",
                text: "one".into(),
                message_id: None,
            },
            CbRecord::Message {
                role: "assistant",
                text: "two".into(),
                message_id: Some("m1".into()),
            },
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
        // Linkage mirrors real CLI files: session-wide request id; user
        // records start a request; assistant records carry messageId.
        assert_eq!(lines[0]["providerData"]["startsNewUserRequest"], true);
        assert!(
            lines[0]["providerData"]["conversationRequestId"]
                .as_str()
                .is_some_and(|s| s.len() == 32)
        );
        assert_eq!(
            lines[0]["providerData"]["conversationRequestId"],
            lines[1]["providerData"]["conversationRequestId"]
        );
        assert_eq!(lines[1]["providerData"]["messageId"], "m1");
        assert!(lines[0].get("messageId").is_none());
        // Verify catches a wrong id / wrong count.
        assert!(!verify_written_session(&path, "other", 2).is_empty());
        assert!(!verify_written_session(&path, "sid-1", 5).is_empty());
    }

    /// On-disk field shapes for the native record types, mirrored from real
    /// CLI session files (~/.codebuddy/projects/*/*.jsonl).
    #[test]
    fn native_records_serialize_cli_shape() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![
            CbRecord::Reasoning {
                text: "hmm".into(),
                message_id: "m1".into(),
            },
            CbRecord::FunctionCall {
                call_id: "t1".into(),
                name: "mcp__tack__bash".into(),
                arguments: "{\"command\":\"ls\"}".into(),
                reasoning: Some("hmm".into()),
                message_id: "m1".into(),
            },
            CbRecord::FunctionCallResult {
                call_id: "t1".into(),
                name: "mcp__tack__bash".into(),
                output_text: "[{\"type\":\"text\",\"text\":\"ok\"}]".into(),
                message_id: "m1".into(),
            },
        ];
        let path = write_session_jsonl_in(dir.path(), "sid-2", "/tmp/proj", &records).unwrap();
        assert!(verify_written_session(&path, "sid-2", 3).is_empty());
        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        // reasoning: content [] + rawContent reasoning_text, no role.
        assert_eq!(lines[0]["type"], "reasoning");
        assert_eq!(lines[0]["rawContent"][0]["type"], "reasoning_text");
        assert_eq!(lines[0]["rawContent"][0]["text"], "hmm");
        assert!(lines[0].get("role").is_none());
        // function_call: callId/name/arguments (stringified JSON).
        assert_eq!(lines[1]["type"], "function_call");
        assert_eq!(lines[1]["callId"], "t1");
        assert_eq!(lines[1]["name"], "mcp__tack__bash");
        assert_eq!(lines[1]["arguments"], "{\"command\":\"ls\"}");
        // function_call_result: status + output.text JSON-stringified blocks.
        assert_eq!(lines[2]["type"], "function_call_result");
        assert_eq!(lines[2]["callId"], "t1");
        assert_eq!(lines[2]["status"], "completed");
        assert_eq!(lines[2]["output"]["type"], "text");
        assert_eq!(
            lines[2]["output"]["text"],
            "[{\"type\":\"text\",\"text\":\"ok\"}]"
        );
        for line in &lines {
            assert_eq!(line["providerData"]["agent"], "sdk");
            assert_eq!(line["providerData"]["messageId"], "m1");
            assert_eq!(line["sessionId"], "sid-2");
        }
        // The turn's thinking duplicates onto the call's providerData.
        assert_eq!(lines[1]["providerData"]["reasoning"], "hmm");
    }

    /// Regression: a rebuild with no settled prefix (split == 0) writes an
    /// empty file and expects 0 records — verify must not index `lines[0]`
    /// on the empty vec (panicked in production after a compaction folded
    /// every assistant reply out of the context).
    #[test]
    fn verify_empty_session_is_sound() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session_jsonl_in(dir.path(), "sid-empty", "/tmp/proj", &[]).unwrap();
        assert!(verify_written_session(&path, "sid-empty", 0).is_empty());
        // A genuinely missing file still warns even when 0 are expected.
        let missing = dir.path().join("nope.jsonl");
        assert!(!verify_written_session(&missing, "sid-empty", 0).is_empty());
    }
}
