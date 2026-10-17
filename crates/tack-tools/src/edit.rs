//! The edit tool. Port of `tools/edit.ts` + `edit-diff.ts`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::edit_diff::{
    Edit, EditKind, RangeMode as EditRangeMode, apply_edits_to_normalized_content,
    detect_line_ending, generate_diff_string, generate_unified_patch, normalize_to_lf,
    restore_line_endings, split_bom,
};
use crate::path_utils::resolve_to_cwd;
use crate::services::ToolServices;

#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
struct EditEntry {
    /// Exact text for one targeted replacement. Omit when using `from`/`to`.
    /// Must match a unique region unless `all: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    old_text: Option<String>,
    /// Replacement text. Use an empty string to delete the matched/range text.
    new_text: String,
    /// First anchor of a range edit. Anchors match **exactly** (no fuzzy) and
    /// must each be unique in the file; duplicates are rejected with every
    /// match location listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    /// Second anchor of a range edit. Omit/null to replace through end of
    /// file. With `mode: "exclusive"` (default) the anchor text is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    /// Range end behavior: `exclusive` (default, keep the `to` anchor) or
    /// `inclusive` (replace the anchor too, plus its line terminator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<RangeMode>,
    /// Optional guard for range edits: expected number of lines in the
    /// replaced range. A mismatch rejects the whole call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expect_lines: Option<usize>,
    /// Replace every **exact** occurrence of `oldText` (fuzzy matching is
    /// disabled for replace-all).
    #[serde(default)]
    all: bool,
    /// Optional guard for `all: true`: the exact expected occurrence count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
enum RangeMode {
    Exclusive,
    Inclusive,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EditParams {
    /// Path to the file to edit (relative or absolute)
    path: String,
    /// One or more targeted edits. Each is matched against the original file,
    /// not incrementally; overlapping edits are rejected. If two changes
    /// touch the same block or nearby lines, merge them into one edit.
    edits: Vec<EditEntry>,
}

/// Validate the per-entry field combination and build the internal edit.
/// Structural mistakes (oldText + from, `to` without `from`, `all` on a
/// range, ...) are rejected before any file content is touched.
fn build_edit(index: usize, entry: EditEntry) -> Result<Edit, String> {
    let range = entry.from.is_some() || entry.to.is_some();
    if entry.old_text.is_some() && range {
        return Err(format!(
            "edits[{index}]: provide either oldText or from/to, not both."
        ));
    }
    if entry.from.is_none() && entry.to.is_some() {
        return Err(format!("edits[{index}]: `to` requires `from`."));
    }
    if entry.old_text.is_none() && entry.from.is_none() {
        return Err(format!(
            "edits[{index}]: each edit needs oldText (replacement) or from (range)."
        ));
    }
    if entry.from.is_some() && entry.all {
        return Err(format!(
            "edits[{index}]: `all` applies to oldText edits, not ranges."
        ));
    }
    if entry.from.is_some() && entry.count.is_some() {
        return Err(format!(
            "edits[{index}]: `count` applies to `all: true` oldText edits, not ranges."
        ));
    }
    if entry.from.is_none() && entry.mode.is_some() {
        return Err(format!(
            "edits[{index}]: `mode` applies to from/to ranges only."
        ));
    }
    if entry.from.is_none() && entry.expect_lines.is_some() {
        return Err(format!(
            "edits[{index}]: `expectLines` applies to from/to ranges only."
        ));
    }
    if entry.count.is_some() && !entry.all {
        return Err(format!("edits[{index}]: `count` requires `all: true`."));
    }
    let kind = match entry.from {
        Some(from) => EditKind::Range {
            from,
            to: entry.to,
            mode: match entry.mode.unwrap_or(RangeMode::Exclusive) {
                RangeMode::Exclusive => EditRangeMode::Exclusive,
                RangeMode::Inclusive => EditRangeMode::Inclusive,
            },
            expect_lines: entry.expect_lines,
        },
        None => EditKind::Replace {
            old_text: entry.old_text.unwrap_or_default(),
            all: entry.all,
            count: entry.count,
        },
    };
    Ok(Edit {
        kind,
        new_text: entry.new_text,
    })
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
    let source = value.get("oldText").is_some_and(Value::is_string)
        || value.get("from").is_some_and(Value::is_string);
    source && value.get("newText").is_some_and(Value::is_string)
}

/// Cap for the diff echoed back to the model on success. The full diff and
/// unified patch stay in `details` for the UI; the model only needs enough to
/// spot a mis-anchored range/accidental deletion without paying for a
/// several-hundred-line diff on every edit.
const MODEL_DIFF_MAX_LINES: usize = 40;

fn compact_diff(diff: &str) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= MODEL_DIFF_MAX_LINES {
        return diff.to_string();
    }
    let omitted = lines.len() - MODEL_DIFF_MAX_LINES;
    format!(
        "{}\n… ({omitted} more diff lines omitted)",
        lines[..MODEL_DIFF_MAX_LINES].join("\n")
    )
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
        "Edit a single file atomically. Three edit shapes are accepted in edits[], all matched against the original file (not incrementally; overlapping edits are rejected): (1) {oldText, newText}: replace one unique region (exact first, fuzzy fallback for near-misses); (2) {oldText, newText, all: true, count?: N}: replace every exact occurrence (fuzzy disabled; count guards the expected number); (3) {from, to?, newText, mode?: \"exclusive\"|\"inclusive\", expectLines?: N}: replace the region between two exact anchors; `to` omitted means through EOF, `mode` defaults to exclusive (the `to` anchor is kept), and expectLines guards the range size. Range anchors are exact-only and must each be unique: a duplicate anchor is rejected with every match location listed, never guessed and never all replaced. Prefer from/to for large contiguous deletions instead of quoting the whole block. If two changes touch the same block or nearby lines, merge them into one edit. Before writing, EVERY edit is validated; if any fails, the whole call is rejected, the file is left unchanged, and all failing hunks are reported together (with the closest candidate and a whitespace-only hint when applicable). Keep anchors/oldText as small as possible while unique; re-read exact text for recently changed files. Successful edits return a compact diff of the affected regions. One file per call; never batch edits for different files into one call."
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

        // Validate every entry's shape before touching the file, and report
        // all structural mistakes in one rejection (same contract as match
        // failures below).
        let mut edits: Vec<Edit> = Vec::with_capacity(params.edits.len());
        let mut shape_errors: Vec<String> = Vec::new();
        for (i, entry) in params.edits.iter().enumerate() {
            match build_edit(i, entry.clone()) {
                Ok(edit) => edits.push(edit),
                Err(err) => shape_errors.push(err),
            }
        }
        if !shape_errors.is_empty() {
            return Err(format!(
                "{}\n\nNo changes were made (edits apply atomically).",
                shape_errors.join("\n\n")
            ));
        }
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
            applied.replacements, params.path
        );
        if !diff.diff.is_empty() {
            message.push_str("\n\n");
            message.push_str(&compact_diff(&diff.diff));
        }
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

    fn message_of(result: &AgentToolResult) -> &str {
        match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text,
            _ => panic!("expected a text result"),
        }
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

    #[test]
    fn schema_exposes_range_and_replace_all_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = tool(tmp.path().to_path_buf()).parameters_schema();
        let text = schema.to_string();
        for key in [
            "oldText",
            "newText",
            "from",
            "to",
            "mode",
            "expectLines",
            "all",
            "count",
        ] {
            assert!(text.contains(key), "{key} missing from schema: {text}");
        }
    }

    /// The new optional fields (range anchors, `all`, guards) must survive
    /// the strict JSON-schema conversion used by constrained-sampling
    /// providers: optionals become nullable required properties.
    #[test]
    fn schema_supports_strict_constrained_sampling() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = tool(tmp.path().to_path_buf()).parameters_schema();
        let strict = tack_ai::constrained_sampling::make_strict_json_schema(&schema)
            .expect("edit schema must fit the strict subset");
        assert_eq!(strict["additionalProperties"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn range_edit_replaces_between_anchors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, "head\n// BEGIN\nold\nold2\n// END\ntail\n").unwrap();
        let tool = tool(tmp.path().to_path_buf());
        let result = run_edit(
            &tool,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{
                    "from": "// BEGIN",
                    "to": "// END",
                    "newText": "bridge\n",
                    "expectLines": 3
                }]
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "head\nbridge\n// END\ntail\n"
        );
        let message = message_of(&result);
        assert!(
            message.contains("Successfully replaced 1 block"),
            "{message}"
        );
        assert!(
            message.contains("bridge"),
            "compact diff missing: {message}"
        );
    }

    #[tokio::test]
    async fn range_duplicate_anchor_rejects_and_leaves_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        let original = "// BEGIN\na\n// BEGIN\nb\n// END\n";
        std::fs::write(&path, original).unwrap();
        let tool = tool(tmp.path().to_path_buf());
        let err = run_edit(
            &tool,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{ "from": "// BEGIN", "to": "// END", "newText": "" }]
            }),
        )
        .await
        .unwrap_err();
        assert!(err.contains("2 occurrences of the `from` anchor"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn replace_all_counts_each_occurrence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, "foo\nfoo\nfoo\n").unwrap();
        let tool = tool(tmp.path().to_path_buf());
        let result = run_edit(
            &tool,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{ "oldText": "foo", "newText": "bar", "all": true, "count": 3 }]
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "bar\nbar\nbar\n");
        assert!(
            message_of(&result).contains("Successfully replaced 3 block"),
            "{}",
            message_of(&result)
        );
    }

    #[tokio::test]
    async fn invalid_edit_shapes_are_rejected_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, "abc\n").unwrap();
        let tool = tool(tmp.path().to_path_buf());
        let cases = [
            (
                serde_json::json!({"path":"f.txt","edits":[{"oldText":"a","from":"a","newText":"x"}]}),
                "either oldText or from/to",
            ),
            (
                serde_json::json!({"path":"f.txt","edits":[{"to":"c","newText":"x"}]}),
                "`to` requires `from`",
            ),
            (
                serde_json::json!({"path":"f.txt","edits":[{"from":"a","newText":"x","all":true}]}),
                "`all` applies to oldText edits",
            ),
            (
                serde_json::json!({"path":"f.txt","edits":[{"oldText":"a","newText":"x","count":1}]}),
                "`count` requires `all: true`",
            ),
            (
                serde_json::json!({"path":"f.txt","edits":[{"oldText":"a","newText":"x","mode":"inclusive"}]}),
                "`mode` applies to from/to ranges",
            ),
        ];
        for (params, expected) in cases {
            let err = run_edit(&tool, params).await.unwrap_err();
            assert!(err.contains(expected), "expected {expected:?}, got: {err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc\n");
        }
    }

    #[tokio::test]
    async fn all_invalid_shapes_are_reported_together() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, "abc\n").unwrap();
        let tool = tool(tmp.path().to_path_buf());
        let err = run_edit(
            &tool,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    { "to": "c", "newText": "x" },
                    { "oldText": "a", "newText": "x", "count": 2 }
                ]
            }),
        )
        .await
        .unwrap_err();
        assert!(err.contains("edits[0]: `to` requires `from`"), "{err}");
        assert!(
            err.contains("edits[1]: `count` requires `all: true`"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc\n");
    }

    #[tokio::test]
    async fn success_message_caps_the_echoed_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        let body: String = (0..60).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let tool = tool(tmp.path().to_path_buf());
        let result = run_edit(
            &tool,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{ "from": "line 0\n", "to": "line 59\n", "newText": "" }]
            }),
        )
        .await
        .unwrap();
        let message = message_of(&result);
        assert!(message.contains("more diff lines omitted"), "{message}");
        // The full diff still lives in details for the UI.
        let full = result.details["diff"].as_str().unwrap();
        assert!(full.contains("line 0"), "{full}");
        assert!(full.contains("line 58"), "{full}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line 59\n");
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
