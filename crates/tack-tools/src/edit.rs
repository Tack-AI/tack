//! The edit tool. Port of `tools/edit.ts` + `edit-diff.ts`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, split_bom,
};
use crate::path_utils::resolve_to_cwd;
use crate::services::ToolServices;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EditEntry {
    /// Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.
    #[serde(rename = "oldText")]
    old_text: String,
    /// Replacement text for this targeted edit.
    #[serde(rename = "newText")]
    new_text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EditParams {
    /// Path to the file to edit (relative or absolute)
    path: String,
    /// One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.
    edits: Vec<EditEntry>,
}

/// Port of `prepareEditArguments`: models sometimes send `edits` as a JSON
/// string, a single edit object, or legacy top-level oldText/newText.
fn prepare_edit_arguments(input: Value) -> Value {
    let Value::Object(mut args) = input else {
        return input;
    };

    let edits_key = "edits";
    let normalized: Option<Vec<Value>> = match args.get(edits_key) {
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(arr)) => Some(arr),
            Ok(single @ Value::Object(_)) if is_single_edit(&single) => Some(vec![single]),
            _ => None,
        },
        Some(single @ Value::Object(_)) if is_single_edit(single) => Some(vec![single.clone()]),
        _ => None,
    };
    if let Some(edits) = normalized {
        args.insert(edits_key.to_string(), Value::Array(edits));
    }

    // Legacy top-level oldText/newText.
    let legacy_old = args
        .get("oldText")
        .and_then(Value::as_str)
        .map(str::to_string);
    let legacy_new = args
        .get("newText")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let (Some(old_text), Some(new_text)) = (legacy_old, legacy_new) {
        let mut edits: Vec<Value> = match args.get(edits_key) {
            Some(Value::Array(arr)) => arr.clone(),
            _ => Vec::new(),
        };
        edits.push(serde_json::json!({ "oldText": old_text, "newText": new_text }));
        args.insert(edits_key.to_string(), Value::Array(edits));
        args.remove("oldText");
        args.remove("newText");
    }

    Value::Object(args)
}

fn is_single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string)
        && value.get("newText").is_some_and(Value::is_string)
}

pub struct EditTool {
    services: ToolServices,
}

impl EditTool {
    pub fn new(services: ToolServices) -> Self {
        EditTool { services }
    }
}

impl std::fmt::Debug for EditTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditTool").finish()
    }
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn label(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes. ATOMIC: all oldText values are validated against the original file before anything is written — if ANY edit fails to match, the ENTIRE call is rejected and the file is left unchanged. One file per call; never batch edits for different files into one call. If you have not read the exact text recently (long files, compacted context), re-read it with the read tool or check uniqueness with grep before editing. On failure, use the reported closest match to correct oldText and retry the whole call."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<EditParams>()
    }

    fn constrained_sampling(&self) -> Option<tack_ai::constrained_sampling::ConstrainedSampling> {
        crate::prefer_strict_sampling()
    }

    fn prepare_arguments(&self, args: Value) -> Value {
        prepare_edit_arguments(args)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: EditParams =
            serde_json::from_value(params).map_err(|e| format!("invalid edit params: {e}"))?;
        if params.edits.is_empty() {
            return Err(
                "Edit tool input is invalid. edits must contain at least one replacement.".into(),
            );
        }
        let absolute = resolve_to_cwd(&params.path, &self.services.cwd);

        let _guard = self.services.mutation_lock.lock().await;
        if cancel.is_cancelled() {
            return Err("Operation aborted".to_string());
        }

        let raw = tokio::fs::read(&absolute)
            .await
            .map_err(|e| format!("Could not edit file: {}. {e}", params.path))?;
        if cancel.is_cancelled() {
            return Err("Operation aborted".to_string());
        }
        // Reject non-UTF-8 files outright: a lossy decode would replace
        // invalid bytes with U+FFFD and writing the result back would
        // silently corrupt the original bytes.
        let raw_content = std::str::from_utf8(&raw).map_err(|_| {
            format!(
                "Could not edit file: {}. File is not valid UTF-8 (binary or another encoding); refusing to edit to avoid corrupting it.",
                params.path
            )
        })?;

        // Strip BOM before matching; the model won't include it in oldText.
        let (bom, content) = split_bom(raw_content);
        let original_ending = detect_line_ending(content);
        let normalized = normalize_to_lf(content);

        let edits: Vec<Edit> = params
            .edits
            .iter()
            .map(|e| Edit {
                old_text: e.old_text.clone(),
                new_text: e.new_text.clone(),
            })
            .collect();
        // Matching + applying is CPU-bound (an exact-miss falls back to
        // a fuzzy Levenshtein sweep over the whole file): keep it off
        // the async worker. The cost circuit breaker in edit_fuzzy
        // bounds the worst case; everything moved into the closure is
        // owned, and the error is a plain String.
        let match_path = params.path.clone();
        let applied = tokio::task::spawn_blocking(move || {
            apply_edits_to_normalized_content(&normalized, &edits, &match_path)
        })
        .await
        .map_err(|e| format!("edit apply task failed: {e}"))?
        .map_err(|e| format!("{e}\n\nNo changes were made (edits apply atomically)."))?;

        // Checkpoint the pre-edit state before writing (first touch per turn).
        self.services.checkpoints.record(&absolute);

        let final_content = format!(
            "{bom}{}",
            restore_line_endings(&applied.new_content, original_ending)
        );
        tokio::fs::write(&absolute, final_content)
            .await
            .map_err(|e| format!("Could not write file: {}. {e}", params.path))?;
        if cancel.is_cancelled() {
            return Err("Operation aborted".to_string());
        }

        let diff = generate_diff_string(&applied.base_content, &applied.new_content, 4);
        let patch =
            generate_unified_patch(&params.path, &applied.base_content, &applied.new_content, 4);

        // LSP feedback loop: surface fresh diagnostics with the edit result
        // so the model sees breakage immediately (lock released first — the
        // diagnostics wait must not serialize other file operations).
        drop(_guard);
        let mut message = format!(
            "Successfully replaced {} block(s) in {}.",
            params.edits.len(),
            params.path
        );
        if let Some(summary) = crate::lsp::post_edit_summary(&self.services.lsp, &absolute).await {
            message.push_str("\n\n");
            message.push_str(&summary);
        }

        Ok(AgentToolResult {
            content: vec![tack_ai::InputContentBlock::text(message)],
            details: serde_json::json!({
                "diff": diff.diff,
                "patch": patch,
                "firstChangedLine": diff.first_changed_line,
            }),
            usage: None,
            terminate: false,
            added_tool_names: None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tool(cwd: std::path::PathBuf) -> EditTool {
        EditTool::new(ToolServices::new(cwd))
    }

    async fn run_edit(tool: &EditTool, params: Value) -> Result<AgentToolResult, String> {
        tool.execute("t1", params, CancellationToken::new(), &|_| {})
            .await
    }

    /// Regression: editing a non-UTF-8 file must fail cleanly and leave the
    /// original bytes untouched (previously from_utf8_lossy replaced invalid
    /// bytes with U+FFFD and the corrupted text was written back).
    #[tokio::test]
    async fn non_utf8_file_is_rejected_and_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("latin1.txt");
        let original: &[u8] = b"caf\xE9 na\xEFve\n";
        std::fs::write(&path, original).unwrap();

        let tool = tool(tmp.path().to_path_buf());
        let err = run_edit(
            &tool,
            serde_json::json!({
                "path": "latin1.txt",
                "edits": [{ "oldText": "caf", "newText": "tea" }]
            }),
        )
        .await
        .unwrap_err();
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), original, "file corrupted");
    }

    #[tokio::test]
    async fn utf8_edit_still_works() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let tool = tool(tmp.path().to_path_buf());
        run_edit(
            &tool,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{ "oldText": "world", "newText": "rust" }]
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello rust\n");
    }

    /// The fuzzy cost circuit breaker: on an exact-miss against a huge
    /// file the tool fails fast with an actionable error instead of
    /// running a minutes-long Levenshtein sweep.
    #[tokio::test]
    async fn huge_file_skips_fuzzy_with_actionable_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.txt");
        // ~1MB, uniform lines: no exact match possible for oldText.
        let line = "x".repeat(200);
        let content = std::iter::repeat_n(line.as_str(), 5000)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();
        let tool = tool(tmp.path().to_path_buf());
        // 3-line oldText pushes the estimate over the breaker ceiling
        // (5000 content lines × 3 target lines × ~200 chars ≈ 3M).
        let old_text = format!(
            "{}\n{}\n{}",
            "y".repeat(200),
            "z".repeat(200),
            "w".repeat(200)
        );
        let err = run_edit(
            &tool,
            serde_json::json!({
                "path": "big.txt",
                "edits": [{ "oldText": old_text, "newText": "nope" }]
            }),
        )
        .await
        .unwrap_err();
        assert!(err.contains("too large for fuzzy matching"), "{err}");
        assert!(err.contains("No changes were made"), "{err}");
        // File untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
}
