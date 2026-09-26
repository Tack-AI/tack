use std::path::{Path, PathBuf};

use serde_json::Value;

use async_trait::async_trait;
use serde::Deserialize;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::services::ToolServices;

use super::convert::{Location, lsp_to_offset, uri_to_path};
use super::manager::{LspManager, format_diagnostics};

/// Collect (uri → TextEdits) from a WorkspaceEdit, then apply to disk.
/// One text edit: (start_line, start_col, end_line, end_col, new_text).
type TextEdit = (u32, u32, u32, u32, String);

/// Is `path` inside one of the workspace roots? Symlinks are resolved on
/// both sides so a root reached through a link still matches.
fn path_in_roots(path: &Path, roots: &[PathBuf]) -> bool {
    let canonical = dunce::canonicalize(path).ok();
    roots.iter().any(|root| {
        let root_canonical = dunce::canonicalize(root).unwrap_or_else(|_| root.clone());
        match &canonical {
            Some(c) => c.starts_with(&root_canonical),
            // Unresolvable target (e.g. does not exist): fall back to a
            // lexical check against both spellings of the root.
            None => path.starts_with(root) || path.starts_with(&root_canonical),
        }
    })
}

pub(crate) fn apply_workspace_edit(
    edit: &Value,
    checkpoints: &crate::checkpoint::CheckpointManager,
    roots: &[PathBuf],
) -> Result<Vec<(PathBuf, usize)>, String> {
    // Gather uri → [{range, newText}] from both `changes` and
    // `documentChanges` shapes.
    let mut per_file: Vec<(PathBuf, Vec<TextEdit>)> = Vec::new();
    let mut outside: Vec<String> = Vec::new();
    let mut push = |uri: &str, range: &Value, new_text: &Value| {
        let Some(path) = uri_to_path(uri) else { return };
        // A compromised or buggy server must not write outside the
        // workspace via a crafted file:// URI.
        if !path_in_roots(&path, roots) {
            outside.push(uri.to_string());
            return;
        }
        let start = &range["start"];
        let end = &range["end"];
        let entry = (
            start["line"].as_u64().unwrap_or(0) as u32,
            start["character"].as_u64().unwrap_or(0) as u32,
            end["line"].as_u64().unwrap_or(0) as u32,
            end["character"].as_u64().unwrap_or(0) as u32,
            new_text.as_str().unwrap_or_default().to_string(),
        );
        match per_file.iter_mut().find(|(p, _)| *p == path) {
            Some((_, edits)) => edits.push(entry),
            None => per_file.push((path, vec![entry])),
        }
    };
    if let Some(changes) = edit["changes"].as_object() {
        for (uri, edits) in changes {
            for e in edits.as_array().into_iter().flatten() {
                push(uri, &e["range"], &e["newText"]);
            }
        }
    }
    if let Some(doc_changes) = edit["documentChanges"].as_array() {
        for change in doc_changes {
            let uri = change["textDocument"]["uri"].as_str().unwrap_or_default();
            for e in change["edits"].as_array().into_iter().flatten() {
                push(uri, &e["range"], &e["newText"]);
            }
        }
    }
    if !outside.is_empty() {
        return Err(format!(
            "refusing to apply edits outside the workspace: {}",
            outside.join(", ")
        ));
    }
    if per_file.is_empty() {
        return Err("rename produced no edits (symbol not renameable here?)".to_string());
    }

    // Phase 1: read every file and compute its post-edit content, validating
    // all ranges up front. Only when EVERY file computes cleanly do we touch
    // disk — otherwise a bad range in file N would leave files 1..N-1
    // half-renamed behind an error (partial application).
    let mut prepared: Vec<(PathBuf, String, usize)> = Vec::new();
    for (path, mut edits) in per_file {
        let content = super::manager::read_lsp_file(&path)?;
        // Apply in reverse document order so offsets stay valid.
        edits.sort_by_key(|(sl, sc, _, _, _)| std::cmp::Reverse((*sl, *sc)));
        let mut text = content;
        // Track the previous (higher) edit's start to reject overlapping
        // ranges — replacing both would silently corrupt the file.
        let mut prev_start: Option<usize> = None;
        for (sl, sc, el, ec, new_text) in &edits {
            let start = lsp_to_offset(&text, *sl, *sc);
            let end = lsp_to_offset(&text, *el, *ec);
            if start > end || end > text.len() {
                return Err(format!(
                    "invalid edit range in {} ({sl}:{sc}-{el}:{ec})",
                    path.display()
                ));
            }
            if let Some(prev) = prev_start
                && end > prev
            {
                return Err(format!(
                    "overlapping edits in {} ({sl}:{sc}-{el}:{ec} overlaps a later edit)",
                    path.display()
                ));
            }
            prev_start = Some(start);
            text.replace_range(start..end, new_text);
        }
        prepared.push((path, text, edits.len()));
    }

    // Phase 2: checkpoint + write. All ranges are known-good at this point.
    let mut summary = Vec::new();
    for (path, text, count) in prepared {
        checkpoints.record(&path);
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        summary.push((path, count));
    }
    Ok(summary)
}

// ---------------------------------------------------------------------
// lsp tool (diagnostics + navigation)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LspParams {
    /// diagnostics (default) | definition | references | implementation |
    /// symbols | workspace_symbols | hover | code_actions | rename |
    /// incoming_calls | outgoing_calls
    operation: Option<String>,
    /// File path. Omit (diagnostics only) for all files with problems.
    path: Option<String>,
    /// 1-based line number (definition/references/rename/hover/code_actions)
    line: Option<u32>,
    /// 1-based column in UTF-16 code units — the same units diagnostics and
    /// locations report, so reported positions round-trip (emoji count 2)
    /// (definition/references/rename/hover/code_actions)
    column: Option<u32>,
    /// Symbol name — alternative to line/column for definition/references/
    /// implementation/hover/incoming_calls/outgoing_calls. Resolved via
    /// project-wide symbol search; ambiguous names list candidates.
    name: Option<String>,
    /// New name (rename only)
    new_name: Option<String>,
    /// Query substring (workspace_symbols only; empty = all symbols)
    query: Option<String>,
    /// 0-based code-action index to apply (code_actions only; requires
    /// line/column so the action can be re-resolved deterministically)
    apply: Option<u64>,
    /// Seconds to wait for the language server (diagnostics; default 5, max 60).
    wait: Option<f64>,
}

pub struct LspTool {
    services: ToolServices,
}

impl LspTool {
    pub fn new(services: ToolServices) -> Self {
        LspTool { services }
    }
}

impl std::fmt::Debug for LspTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspTool").finish()
    }
}

fn format_locations(title: &str, locations: &[Location], cwd: &Path) -> String {
    if locations.is_empty() {
        return format!("{title}: no results.");
    }
    let mut out = format!("{title} ({}):\n", locations.len());
    for loc in locations.iter().take(50) {
        let display = loc.path.strip_prefix(cwd).unwrap_or(&loc.path);
        out.push_str(&format!(
            "  {}:{}:{}\n",
            display.display(),
            loc.line,
            loc.column
        ));
    }
    if locations.len() > 50 {
        out.push_str(&format!("  … and {} more\n", locations.len() - 50));
    }
    out.trim_end().to_string()
}

#[async_trait]
impl AgentTool for LspTool {
    fn name(&self) -> &'static str {
        "lsp"
    }
    fn label(&self) -> &str {
        "lsp"
    }
    fn description(&self) -> &str {
        "Language-server (LSP) integration. Operations: diagnostics (default — compile/type \
         errors for a file, or all files when path is omitted; says so when the background \
         type check is still running), definition (go to definition), references (find usages), \
         implementation (trait/interface impls), symbols (file outline), workspace_symbols \
         (project-wide symbol search), hover (type/signature info at a position), \
         incoming_calls/outgoing_calls (call hierarchy — who calls this / what it calls), \
         code_actions (quickfixes for the file's diagnostics; list first, then apply=<index> \
         with line/column to apply one), rename (project-wide rename, applied to disk). \
         Positions are 1-based line/column; columns are UTF-16 code units, exactly as \
         diagnostics and locations report them (pass those back unchanged). Instead of \
         line/column you may pass name=<symbol> (exact project-wide match; ambiguous \
         names return candidates to disambiguate). \
         Workflow: grep for text first when you don't know where something lives, then use \
         definition/references/incoming_calls to navigate precisely; use diagnostics after \
         editing to verify, and code_actions to apply server-suggested fixes. The server \
         starts lazily; the first query may take longer while it indexes."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<LspParams>()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: LspParams =
            serde_json::from_value(params).map_err(|e| format!("invalid lsp params: {e}"))?;
        let operation = params.operation.as_deref().unwrap_or("diagnostics");
        let manager = &self.services.lsp;

        let path = params
            .path
            .as_deref()
            .map(|p| crate::path_utils::resolve_to_cwd(p, &self.services.cwd));

        // Operations that need a position: explicit line/column, or a
        // symbol name resolved via workspace-wide search.
        let position = |path: &Option<PathBuf>,
                        manager: &LspManager|
         -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(PathBuf, u32, u32), String>> + Send>,
        > {
            let path = path.clone();
            let name = params.name.clone();
            let line = params.line;
            let column = params.column;
            let cwd = self.services.cwd.clone();
            let manager = manager.clone();
            Box::pin(async move {
                if let Some(name) = name {
                    if line.is_some() || column.is_some() {
                        return Err("pass either name or line+column, not both".to_string());
                    }
                    let matches = manager.resolve_symbol(path.as_deref(), &name).await.ok_or(
                        "no language server available to resolve the name (pass path \
                             so one can be started)",
                    )?;
                    match matches.len() {
                        0 => Err(format!("no symbol named {name:?} in the workspace")),
                        1 => {
                            let s = &matches[0];
                            Ok((s.path.clone(), s.line, s.column))
                        }
                        _ => {
                            let mut err = format!(
                                "{name:?} is ambiguous ({} matches) — pass path+line+column:\n",
                                matches.len()
                            );
                            for s in matches.iter().take(10) {
                                let display = s.path.strip_prefix(&cwd).unwrap_or(&s.path);
                                err.push_str(&format!(
                                    "  {}:{}:{} ({})",
                                    display.display(),
                                    s.line,
                                    s.column,
                                    s.kind
                                ));
                            }
                            Err(err)
                        }
                    }
                } else {
                    let path = path.ok_or("this operation requires path")?;
                    let line = line.ok_or("this operation requires line (1-based)")?;
                    let column = column.ok_or("this operation requires column (1-based)")?;
                    Ok((path, line, column))
                }
            })
        };

        match operation {
            "diagnostics" => {
                let wait =
                    std::time::Duration::from_secs_f64(params.wait.unwrap_or(5.0).clamp(0.5, 60.0));
                match path {
                    Some(path) => {
                        let Some(spec) = manager.spec_for(&path) else {
                            return Err("No language server configured for this file type. \
                                 Configure one via the lspServers setting."
                                .to_string());
                        };
                        match manager.diagnostics_with_state(&path, wait).await {
                            Some((diags, state)) if diags.is_empty() => {
                                let mut text = format!("No problems found in {}.", path.display());
                                if let Some(note) = state.clean_confidence_note(
                                    super::client::verifier_hint(&spec.command),
                                ) {
                                    text.push_str(&format!(" ({note})"));
                                }
                                Ok(AgentToolResult::text(text))
                            }
                            Some((diags, state)) => {
                                let errors = diags.iter().filter(|d| d.severity == 1).count();
                                let warnings = diags.iter().filter(|d| d.severity == 2).count();
                                let mut text = format_diagnostics(&path, &diags, 30);
                                text.push_str(&format!(
                                    "\n({} error(s), {} warning(s), {} total)",
                                    errors,
                                    warnings,
                                    diags.len()
                                ));
                                if let Some(note) = state.more_may_follow_note() {
                                    text.push_str(&format!("\n({note})"));
                                }
                                if let Some(message) = state.message {
                                    text.push_str(&format!("\nserver status: {message}"));
                                }
                                Ok(AgentToolResult::text(text))
                            }
                            None => Err(format!(
                                "Language server is unavailable for {} (failed to start; see logs).",
                                path.display()
                            )),
                        }
                    }
                    None => {
                        let all = manager.workspace_diagnostics();
                        if all.is_empty() {
                            return Ok(AgentToolResult::text(
                                "No problems reported on any open file. \
                                 Query a specific path first to start the language server.",
                            ));
                        }
                        let mut text = String::new();
                        for (path, diags) in all.iter().take(20) {
                            text.push_str(&format_diagnostics(path, diags, 10));
                            text.push('\n');
                        }
                        Ok(AgentToolResult::text(text))
                    }
                }
            }
            "definition" | "references" | "implementation" => {
                let (path, line, column) = position(&path, manager).await?;
                let result = match operation {
                    "definition" => manager.definition(&path, line, column).await,
                    "implementation" => manager.implementation(&path, line, column).await,
                    _ => manager.references(&path, line, column).await,
                };
                match result {
                    Some(locations) => Ok(AgentToolResult::text(format_locations(
                        operation,
                        &locations,
                        &self.services.cwd,
                    ))),
                    None => Err("language server unavailable or returned an error".to_string()),
                }
            }
            "incoming_calls" | "outgoing_calls" => {
                let (path, line, column) = position(&path, manager).await?;
                let incoming = operation == "incoming_calls";
                match manager.call_hierarchy(&path, line, column, incoming).await {
                    Some(calls) if calls.is_empty() => Ok(AgentToolResult::text(format!(
                        "{operation}: no results (no symbol here, or the server doesn't support call hierarchy)."
                    ))),
                    Some(calls) => {
                        let mut out = format!("{operation} ({}):\n", calls.len());
                        for call in calls.iter().take(50) {
                            let display = call
                                .path
                                .strip_prefix(&self.services.cwd)
                                .unwrap_or(&call.path);
                            out.push_str(&format!(
                                "  {} ({}) — {}:{}:{}",
                                call.name,
                                call.kind,
                                display.display(),
                                call.line,
                                call.column
                            ));
                            if !call.call_lines.is_empty() {
                                let lines: Vec<String> = call
                                    .call_lines
                                    .iter()
                                    .take(8)
                                    .map(|l| l.to_string())
                                    .collect();
                                out.push_str(&format!(
                                    "  [call sites: lines {}]",
                                    lines.join(", ")
                                ));
                            }
                            out.push('\n');
                        }
                        if calls.len() > 50 {
                            out.push_str(&format!("  … and {} more\n", calls.len() - 50));
                        }
                        Ok(AgentToolResult::text(out.trim_end().to_string()))
                    }
                    None => Err("language server unavailable or returned an error".to_string()),
                }
            }
            "symbols" => {
                let path = path.ok_or("symbols requires path")?;
                match manager.document_symbols(&path).await {
                    Some(symbols) if symbols.is_empty() => {
                        Ok(AgentToolResult::text("no symbols reported for this file."))
                    }
                    Some(symbols) => {
                        let mut out = format!("Symbols in {}:\n", path.display());
                        for sym in symbols.iter().take(200) {
                            out.push_str(&format!(
                                "{}{} ({})\n",
                                "  ".repeat(sym.depth + 1),
                                sym.name,
                                sym.kind
                            ));
                        }
                        Ok(AgentToolResult::text(out.trim_end().to_string()))
                    }
                    None => Err("language server unavailable or returned an error".to_string()),
                }
            }
            "workspace_symbols" => {
                let path =
                    path.ok_or("workspace_symbols requires path (any file of the language)")?;
                let query = params.query.as_deref().unwrap_or("");
                match manager.workspace_symbols(&path, query).await {
                    Some(symbols) if symbols.is_empty() => Ok(AgentToolResult::text(format!(
                        "No workspace symbols matching {query:?}."
                    ))),
                    Some(symbols) => {
                        let mut out = format!(
                            "Workspace symbols matching {query:?} ({}):\n",
                            symbols.len()
                        );
                        for sym in symbols.iter().take(100) {
                            let display = sym
                                .path
                                .strip_prefix(&self.services.cwd)
                                .unwrap_or(&sym.path);
                            out.push_str(&format!(
                                "  {}:{}:{} {} ({})\n",
                                display.display(),
                                sym.line,
                                sym.column,
                                sym.name,
                                sym.kind
                            ));
                        }
                        if symbols.len() > 100 {
                            out.push_str(&format!("  … and {} more\n", symbols.len() - 100));
                        }
                        Ok(AgentToolResult::text(out.trim_end().to_string()))
                    }
                    None => Err("language server unavailable or returned an error".to_string()),
                }
            }
            "hover" => {
                let (path, line, column) = position(&path, manager).await?;
                match manager.hover(&path, line, column).await {
                    Some(Some(text)) => Ok(AgentToolResult::text(text)),
                    Some(None) => Ok(AgentToolResult::text(
                        "No hover information at this position.",
                    )),
                    None => Err("language server unavailable or returned an error".to_string()),
                }
            }
            "code_actions" => {
                let path = path.ok_or("code_actions requires path")?;
                if let Some(index) = params.apply {
                    let line = params.line.ok_or("apply requires line (1-based)")?;
                    let column = params.column.ok_or("apply requires column (1-based)")?;
                    // Serialize with edit/write/bash mutations while the
                    // server-computed edits hit disk.
                    let _guard = self.services.mutation_lock.lock().await;
                    match manager
                        .apply_code_action(
                            &path,
                            line,
                            column,
                            index as usize,
                            &self.services.checkpoints,
                        )
                        .await
                    {
                        Some(Ok(summary)) => Ok(AgentToolResult::text(summary)),
                        Some(Err(e)) => Err(format!("apply failed: {e}")),
                        None => Err("language server unavailable".to_string()),
                    }
                } else {
                    if params.line.is_some() != params.column.is_some() {
                        return Err(
                            "code_actions requires both line and column, or neither".to_string()
                        );
                    }
                    let focus = params.line.zip(params.column);
                    match manager
                        .code_actions(&path, focus, std::time::Duration::from_secs(5))
                        .await
                    {
                        Some(actions) if actions.is_empty() => Ok(AgentToolResult::text(
                            "No code actions available (sync diagnostics first if the file was just edited).",
                        )),
                        Some(actions) => {
                            let mut out = format!(
                                "Code actions for {} (apply with operation=code_actions, apply=<index>, plus path/line/column):\n",
                                path.display()
                            );
                            for (i, action) in actions.iter().enumerate().take(30) {
                                let title = action["title"].as_str().unwrap_or("untitled");
                                let kind = action["kind"].as_str().unwrap_or("command");
                                let preferred = if action["isPreferred"].as_bool() == Some(true) {
                                    " [preferred]"
                                } else {
                                    ""
                                };
                                let applyable = if action.get("edit").is_some() {
                                    ""
                                } else {
                                    " (server command — not applyable)"
                                };
                                out.push_str(&format!(
                                    "  [{i}] {title} ({kind}){preferred}{applyable}\n"
                                ));
                            }
                            if actions.len() > 30 {
                                out.push_str(&format!("  … and {} more\n", actions.len() - 30));
                            }
                            Ok(AgentToolResult::text(out.trim_end().to_string()))
                        }
                        None => Err("language server unavailable or returned an error".to_string()),
                    }
                }
            }
            "rename" => {
                let (path, line, column) = position(&path, manager).await?;
                let new_name = params
                    .new_name
                    .as_deref()
                    .ok_or("rename requires new_name")?;
                if new_name.trim().is_empty() {
                    return Err("new_name must not be empty".to_string());
                }
                // Serialize with edit/write/bash mutations while the
                // server-computed edits hit disk.
                let _guard = self.services.mutation_lock.lock().await;
                match manager
                    .rename(&path, line, column, new_name, &self.services.checkpoints)
                    .await
                {
                    Some(Ok(summary)) => {
                        let total: usize = summary.iter().map(|(_, n)| n).sum();
                        let mut out = format!(
                            "Renamed to {new_name:?}: {total} edit(s) across {} file(s):\n",
                            summary.len()
                        );
                        for (file, count) in &summary {
                            let display = file.strip_prefix(&self.services.cwd).unwrap_or(file);
                            out.push_str(&format!("  {} ({count} edit(s))\n", display.display()));
                        }
                        Ok(AgentToolResult::text(out.trim_end().to_string()))
                    }
                    Some(Err(e)) => Err(format!("rename failed: {e}")),
                    None => Err("language server unavailable".to_string()),
                }
            }
            other => Err(format!(
                "unknown operation {other:?} \
                 (diagnostics|definition|references|implementation|symbols|workspace_symbols|hover|incoming_calls|outgoing_calls|code_actions|rename)"
            )),
        }
    }
}

/// Backward-compatible alias (0.2.0 shipped this as `diagnostics`).
pub type DiagnosticsTool = LspTool;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::lsp::convert::path_to_uri;
    use serde_json::json;

    /// Multi-file WorkspaceEdits apply to every file, mapping UTF-16 ranges
    /// to byte offsets (emoji/CJK before the range must not corrupt edits).
    #[test]
    fn workspace_edit_applies_multi_file_with_utf16_offsets() {
        let tmp = tempfile::tempdir().unwrap();
        let file_a = tmp.path().join("a.rs");
        let file_b = tmp.path().join("b.rs");
        std::fs::write(&file_a, "let 😀x = old_name;\nuse_it(old_name);\n").unwrap();
        std::fs::write(&file_b, "fn f() { old_name(); }\n").unwrap();
        let uri_a = path_to_uri(&file_a);
        let uri_b = path_to_uri(&file_b);
        // `old_name` on line 0 of a.rs starts at UTF-16 col 11
        // ("let " 4 + emoji 2 + "x = " 4 = 10 units before it → col 10).
        let edit = json!({
            "changes": {
                uri_a: [
                    { "range": { "start": { "line": 0, "character": 10 }, "end": { "line": 0, "character": 18 } }, "newText": "new_name" },
                    { "range": { "start": { "line": 1, "character": 7 }, "end": { "line": 1, "character": 15 } }, "newText": "new_name" }
                ],
                uri_b: [
                    { "range": { "start": { "line": 0, "character": 9 }, "end": { "line": 0, "character": 17 } }, "newText": "new_name" }
                ]
            }
        });
        let checkpoints = crate::checkpoint::CheckpointManager::new();
        let summary =
            apply_workspace_edit(&edit, &checkpoints, &[tmp.path().to_path_buf()]).unwrap();
        let total: usize = summary.iter().map(|(_, n)| n).sum();
        assert_eq!(total, 3);
        assert_eq!(
            std::fs::read_to_string(&file_a).unwrap(),
            "let 😀x = new_name;\nuse_it(new_name);\n"
        );
        assert_eq!(
            std::fs::read_to_string(&file_b).unwrap(),
            "fn f() { new_name(); }\n"
        );
    }

    /// Regression: a WorkspaceEdit spanning multiple files where a LATER
    /// file's range is invalid must not leave earlier files modified — the
    /// old code wrote each file as it went, so the error path returned with
    /// a partial rename on disk (and the checkpoint only covered the
    /// pre-edit state, nothing rolled back automatically).
    #[test]
    fn workspace_edit_is_atomic_across_files() {
        let tmp = tempfile::tempdir().unwrap();
        let file_a = tmp.path().join("a.rs");
        let file_b = tmp.path().join("b.rs");
        let original_a = "old_name\n";
        let original_b = "old_name\n";
        std::fs::write(&file_a, original_a).unwrap();
        std::fs::write(&file_b, original_b).unwrap();
        let uri_a = path_to_uri(&file_a);
        let uri_b = path_to_uri(&file_b);
        // Valid edit in a.rs; range past EOF in b.rs (line 99 doesn't exist
        // → clamps to end, start > end → invalid).
        let edit = json!({
            "changes": {
                uri_a: [
                    { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 8 } }, "newText": "new_name" }
                ],
                uri_b: [
                    { "range": { "start": { "line": 99, "character": 5 }, "end": { "line": 0, "character": 8 } }, "newText": "new_name" }
                ]
            }
        });
        let checkpoints = crate::checkpoint::CheckpointManager::new();
        let err =
            apply_workspace_edit(&edit, &checkpoints, &[tmp.path().to_path_buf()]).unwrap_err();
        assert!(err.contains("invalid edit range"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&file_a).unwrap(),
            original_a,
            "earlier files must stay untouched when a later file fails"
        );
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), original_b);
    }

    /// Overlapping edits in one file are rejected instead of silently
    /// corrupting the buffer.
    #[test]
    fn workspace_edit_rejects_overlapping_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.rs");
        std::fs::write(&file, "abcdefghij\n").unwrap();
        let uri = path_to_uri(&file);
        let edit = json!({
            "changes": {
                uri: [
                    { "range": { "start": { "line": 0, "character": 2 }, "end": { "line": 0, "character": 6 } }, "newText": "X" },
                    { "range": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 8 } }, "newText": "Y" }
                ]
            }
        });
        let checkpoints = crate::checkpoint::CheckpointManager::new();
        let err =
            apply_workspace_edit(&edit, &checkpoints, &[tmp.path().to_path_buf()]).unwrap_err();
        assert!(err.contains("overlapping"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "abcdefghij\n");
    }

    /// `documentChanges` shape (the other WorkspaceEdit encoding) applies too.
    #[test]
    fn workspace_edit_applies_document_changes_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.rs");
        std::fs::write(&file, "before\n").unwrap();
        let uri = path_to_uri(&file);
        let edit = json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": uri, "version": 1 },
                    "edits": [
                        { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 6 } }, "newText": "after" }
                    ]
                }
            ]
        });
        let checkpoints = crate::checkpoint::CheckpointManager::new();
        let summary =
            apply_workspace_edit(&edit, &checkpoints, &[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "after\n");
    }

    /// A server-supplied URI outside the workspace roots must be refused,
    /// with in-workspace files left untouched.
    #[test]
    fn workspace_edit_rejects_paths_outside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let inside = tmp.path().join("in.rs");
        std::fs::write(&inside, "old\n").unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("out.rs");
        std::fs::write(&outside, "old\n").unwrap();
        let edit = json!({
            "changes": {
                path_to_uri(&inside): [
                    { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } }, "newText": "new" }
                ],
                path_to_uri(&outside): [
                    { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } }, "newText": "new" }
                ]
            }
        });
        let checkpoints = crate::checkpoint::CheckpointManager::new();
        let err =
            apply_workspace_edit(&edit, &checkpoints, &[tmp.path().to_path_buf()]).unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
        // Nothing applied at all (validation happens before any write).
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "old\n");
    }
}
