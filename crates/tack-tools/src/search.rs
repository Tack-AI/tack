//! grep / find / ls tools. The TS versions shell out to ripgrep/fd; tack
//! uses the `grep-*` + `ignore` crates in-process (Windows-friendly, no
//! external binaries). Output formats match the TS tools.

use async_trait::async_trait;
use grep_matcher::Matcher;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::path_utils::resolve_to_cwd;
use crate::services::ToolServices;
use crate::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head, truncate_line};

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

const GREP_DEFAULT_LIMIT: usize = 100;
/// Hard ceiling for the caller-supplied `limit`: unbounded limits let a
/// single call accumulate unbounded match lines in memory; the output is
/// truncated to 50KB anyway, so past this only the notices change.
const GREP_MAX_LIMIT: usize = 10_000;
/// Files larger than this are skipped: grep reads the whole file into
/// memory, and the output budget is 50KB anyway. Matches rg's practical
/// behavior of not churning through giant (usually generated) files.
const GREP_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Total bytes cached for context-line rendering across all matched
/// files (each up to GREP_MAX_FILE_BYTES). Past the cap, caching stops
/// and the result carries a notice; without it a broad -C search over
/// many large files multiplies the per-file cap unboundedly.
const GREP_CONTEXT_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GrepParams {
    /// Search pattern (regex or literal string)
    pattern: String,
    /// Directory or file to search (default: current directory)
    path: Option<String>,
    /// Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'
    glob: Option<String>,
    /// Case-insensitive search (default: false)
    #[serde(rename = "ignoreCase")]
    ignore_case: Option<bool>,
    /// Treat pattern as literal string instead of regex (default: false)
    literal: Option<bool>,
    /// Number of lines to show before and after each match (default: 0)
    context: Option<usize>,
    /// Maximum number of matches to return (default: 100)
    limit: Option<usize>,
}

pub struct GrepTool {
    services: ToolServices,
}

impl GrepTool {
    pub fn new(services: ToolServices) -> Self {
        GrepTool { services }
    }
}

impl std::fmt::Debug for GrepTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrepTool").finish()
    }
}

struct GrepMatch {
    file: std::path::PathBuf,
    line_number: usize,
    /// The matching line's text (kept so output needs no second read).
    line: String,
}

/// Outcome of the blocking walk+search pass.
struct GrepSearch {
    matches: Vec<GrepMatch>,
    /// Raw text of files WITH matches, for context-line rendering. Only
    /// populated when context lines were requested, only for matched
    /// files (capped by the match limit), and only up to
    /// GREP_CONTEXT_CACHE_MAX_BYTES in total — lines are re-split on
    /// demand for the few files involved.
    matched_files: std::collections::HashMap<std::path::PathBuf, String>,
    /// Cumulative bytes in `matched_files`.
    context_cache_bytes: usize,
    /// True once the context cache hit GREP_CONTEXT_CACHE_MAX_BYTES and
    /// stopped taking files (a notice is emitted).
    context_cache_truncated: bool,
    match_limit_reached: bool,
    oversized_files: usize,
    cancelled: bool,
}

/// Iterate lines without materializing a whole-file `Vec<String>`: `\n`,
/// `\r\n`, and a lone `\r` all terminate a line (mirroring the old
/// normalize-then-`split('\n')` behavior, including its trailing empty
/// segment when the text ends with a terminator).
fn for_each_line(text: &str, mut f: impl FnMut(usize, &str)) {
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut line_no = 1usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                f(line_no, &text[start..i]);
                line_no += 1;
                i += 1;
                start = i;
            }
            b'\r' => {
                f(line_no, &text[start..i]);
                line_no += 1;
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    f(line_no, &text[start..]);
}

/// Context-cache policy for [`grep_search`]: whether to retain the full
/// text of match-bearing files (only needed when rendering context
/// lines) and the cumulative byte budget for that retention.
struct ContextCacheCfg {
    enabled: bool,
    max_bytes: usize,
}

/// Walk + search, on a blocking thread: the walk stats every directory and
/// the search reads every file, neither belongs on an async worker.
fn grep_search(
    search_path: std::path::PathBuf,
    is_directory: bool,
    glob: Option<String>,
    regex: grep_regex::RegexMatcher,
    effective_limit: usize,
    cache: ContextCacheCfg,
    cancel: CancellationToken,
) -> GrepSearch {
    let mut out = GrepSearch {
        matches: Vec::new(),
        matched_files: std::collections::HashMap::new(),
        context_cache_bytes: 0,
        context_cache_truncated: false,
        match_limit_reached: false,
        oversized_files: 0,
        cancelled: false,
    };

    // Walk files (respecting .gitignore, including hidden files like rg --hidden).
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if is_directory {
        let mut builder = ignore::WalkBuilder::new(&search_path);
        builder
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true);
        let glob_override = glob.as_ref().map(|g| {
            let mut ob = ignore::overrides::OverrideBuilder::new(&search_path);
            let _ = ob.add(g);
            ob.build()
        });
        for entry in builder.build().flatten() {
            if cancel.is_cancelled() {
                out.cancelled = true;
                return out;
            }
            let path = entry.path();
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if let Some(Ok(ov)) = &glob_override
                && !ov.matched(path, false).is_whitelist()
            {
                continue;
            }
            files.push(path.to_path_buf());
        }
    } else {
        files.push(search_path.clone());
    }

    // Search each file, streaming line by line: no whole-file line vector
    // is built — only matching lines (bounded by effective_limit) and the
    // raw text of match-bearing files (for context) are kept.
    'files: for file in &files {
        if cancel.is_cancelled() {
            out.cancelled = true;
            return out;
        }
        if std::fs::metadata(file).map(|m| m.len()).unwrap_or(0) > GREP_MAX_FILE_BYTES {
            out.oversized_files += 1;
            continue;
        }
        let Ok(content) = std::fs::read(file) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&content) else {
            continue;
        }; // skip binary
        let mut file_matched = false;
        for_each_line(text, |line_no, line| {
            if out.match_limit_reached {
                return;
            }
            if regex.is_match(line.as_bytes()).unwrap_or(false) {
                out.matches.push(GrepMatch {
                    file: file.clone(),
                    line_number: line_no,
                    line: line.to_string(),
                });
                file_matched = true;
                if out.matches.len() >= effective_limit {
                    out.match_limit_reached = true;
                }
            }
        });
        if file_matched && cache.enabled {
            // Cache for context rendering only while the cumulative cap
            // holds; past it, stop caching entirely (one notice covers
            // every file that then renders without context).
            if out.context_cache_truncated || out.context_cache_bytes + text.len() > cache.max_bytes
            {
                out.context_cache_truncated = true;
            } else {
                out.context_cache_bytes += text.len();
                out.matched_files.insert(file.clone(), text.to_string());
            }
        }
        if out.match_limit_reached {
            break 'files;
        }
    }
    out
}

/// Lines of `text` as a vector (context rendering only; called for the few
/// match-bearing files, never during the walk).
fn split_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for_each_line(text, |_, line| lines.push(line.to_string()));
    lines
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn label(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents for a pattern (regex or literal string). Returns matching lines with file paths and line numbers, optionally with context lines. Respects .gitignore. Output is truncated to 50KB. Use limit to control the number of matches (default 100)."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<GrepParams>()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: GrepParams =
            serde_json::from_value(params).map_err(|e| format!("invalid grep params: {e}"))?;
        if cancel.is_cancelled() {
            return Err("Operation aborted".to_string());
        }

        let search_path = resolve_to_cwd(params.path.as_deref().unwrap_or("."), &self.services.cwd);
        if !search_path.exists() {
            return Err(format!("Path not found: {}", search_path.display()));
        }
        let is_directory = search_path.is_dir();
        let requested_limit = params.limit.unwrap_or(GREP_DEFAULT_LIMIT).max(1);
        let effective_limit = requested_limit.min(GREP_MAX_LIMIT);
        let limit_clamped = requested_limit > GREP_MAX_LIMIT;
        let context_lines = params.context.unwrap_or(0);

        let regex = if params.literal == Some(true) {
            grep_regex::RegexMatcherBuilder::new()
                .case_insensitive(params.ignore_case == Some(true))
                .build(&regex::escape(&params.pattern))
        } else {
            grep_regex::RegexMatcherBuilder::new()
                .case_insensitive(params.ignore_case == Some(true))
                .build(&params.pattern)
        }
        .map_err(|e| format!("invalid pattern: {e}"))?;

        // Walk + search on a blocking thread: directory stats and whole
        // file reads must not stall the async runtime.
        let search = tokio::task::spawn_blocking({
            let search_path = search_path.clone();
            let cancel = cancel.clone();
            move || {
                grep_search(
                    search_path,
                    is_directory,
                    params.glob.clone(),
                    regex,
                    effective_limit,
                    ContextCacheCfg {
                        enabled: context_lines > 0,
                        max_bytes: GREP_CONTEXT_CACHE_MAX_BYTES,
                    },
                    cancel,
                )
            }
        })
        .await
        .map_err(|e| format!("grep search task failed: {e}"))?;
        if search.cancelled {
            return Err("Operation aborted".to_string());
        }
        let matches = search.matches;
        let match_limit_reached = search.match_limit_reached;
        let oversized_files = search.oversized_files;
        let context_cache_truncated = search.context_cache_truncated;
        let matched_files = search.matched_files;

        if matches.is_empty() {
            return Ok(AgentToolResult::text("No matches found"));
        }

        let format_path = |file: &std::path::Path| -> String {
            if is_directory && let Ok(rel) = file.strip_prefix(&search_path) {
                let s = rel.to_string_lossy().replace('\\', "/");
                if !s.starts_with("..") {
                    return s;
                }
            }
            file.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        };

        let mut lines_truncated = false;
        let mut output_lines: Vec<String> = Vec::new();
        // Lazily split lines of a match-bearing file, only when context
        // lines are requested (matching itself streamed line by line).
        let mut context_cache: std::collections::HashMap<std::path::PathBuf, Option<Vec<String>>> =
            std::collections::HashMap::new();

        for m in &matches {
            let relative = format_path(&m.file);
            if context_lines == 0 {
                let (text, was) = truncate_line(m.line.trim_end_matches('\r'), None);
                if was {
                    lines_truncated = true;
                }
                output_lines.push(format!("{relative}:{}: {text}", m.line_number));
            } else {
                let file_lines = context_cache
                    .entry(m.file.clone())
                    .or_insert_with(|| matched_files.get(&m.file).map(|t| split_lines(t)));
                let Some(file_lines) = file_lines else {
                    output_lines.push(format!(
                        "{relative}:{}: (unable to read file)",
                        m.line_number
                    ));
                    continue;
                };
                let start = m.line_number.saturating_sub(context_lines).max(1);
                let end = m
                    .line_number
                    .saturating_add(context_lines)
                    .min(file_lines.len());
                for current in start..=end {
                    let line_text = file_lines.get(current - 1).cloned().unwrap_or_default();
                    let sanitized = line_text.trim_end_matches('\r');
                    let (text, was) = truncate_line(sanitized, None);
                    if was {
                        lines_truncated = true;
                    }
                    if current == m.line_number {
                        output_lines.push(format!("{relative}:{current}: {text}"));
                    } else {
                        output_lines.push(format!("{relative}-{current}- {text}"));
                    }
                }
            }
        }

        let raw = output_lines.join("\n");
        let truncation = truncate_head(&raw, Some(usize::MAX), None);
        let mut output = truncation.content.clone();

        let mut notices: Vec<String> = Vec::new();
        if limit_clamped {
            notices.push(format!("limit clamped to {GREP_MAX_LIMIT} (maximum)"));
        }
        if match_limit_reached {
            notices.push(format!(
                "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
                (effective_limit * 2).min(GREP_MAX_LIMIT)
            ));
        }
        if oversized_files > 0 {
            notices.push(format!(
                "{oversized_files} file(s) over {} skipped",
                format_size(GREP_MAX_FILE_BYTES as usize)
            ));
        }
        if context_cache_truncated {
            notices.push(format!(
                "context cache limit ({}) reached; some matches render without context lines",
                format_size(GREP_CONTEXT_CACHE_MAX_BYTES)
            ));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {} chars. Use read tool to see full lines",
                crate::truncate::GREP_MAX_LINE_LENGTH
            ));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        Ok(AgentToolResult::text(output))
    }
}

// ---------------------------------------------------------------------------
// find
// ---------------------------------------------------------------------------

const FIND_DEFAULT_LIMIT: usize = 1000;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FindParams {
    /// Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'
    pattern: String,
    /// Directory to search in (default: current directory)
    path: Option<String>,
    /// Maximum number of results (default: 1000)
    limit: Option<usize>,
}

pub struct FindTool {
    services: ToolServices,
}

impl FindTool {
    pub fn new(services: ToolServices) -> Self {
        FindTool { services }
    }
}

impl std::fmt::Debug for FindTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FindTool").finish()
    }
}

#[async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }
    fn label(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        "Find files by glob pattern (respects .gitignore). Returns relative paths, one per line. Output is truncated to 50KB or the result limit (default 1000)."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<FindParams>()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: FindParams =
            serde_json::from_value(params).map_err(|e| format!("invalid find params: {e}"))?;
        let search_path = resolve_to_cwd(params.path.as_deref().unwrap_or("."), &self.services.cwd);
        if !search_path.exists() {
            return Err(format!("Path not found: {}", search_path.display()));
        }
        let effective_limit = params.limit.unwrap_or(FIND_DEFAULT_LIMIT);

        let glob = globset::GlobBuilder::new(&params.pattern)
            .literal_separator(false)
            .build()
            .map_err(|e| format!("invalid glob pattern: {e}"))?
            .compile_matcher();

        // Walk on a blocking thread: the traversal stats every directory
        // entry and must not stall the async runtime.
        let (mut results, cancelled) = tokio::task::spawn_blocking({
            let search_path = search_path.clone();
            let cancel = cancel.clone();
            move || {
                let mut results: Vec<String> = Vec::new();
                let mut builder = ignore::WalkBuilder::new(&search_path);
                builder
                    .hidden(false)
                    .git_ignore(true)
                    .git_global(true)
                    .git_exclude(true);
                // Always skip node_modules/.git like the TS fd-based implementation.
                let mut overrides = ignore::overrides::OverrideBuilder::new(&search_path);
                let _ = overrides.add("!**/node_modules/**");
                let _ = overrides.add("!**/.git/**");
                if let Ok(ov) = overrides.build() {
                    builder.overrides(ov);
                }

                for entry in builder.build().flatten() {
                    if cancel.is_cancelled() {
                        return (results, true);
                    }
                    let path = entry.path();
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        continue;
                    }
                    let Ok(rel) = path.strip_prefix(&search_path) else {
                        continue;
                    };
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    if glob.is_match(rel_str.as_str()) {
                        results.push(rel_str);
                        if results.len() >= effective_limit {
                            break;
                        }
                    }
                }
                (results, false)
            }
        })
        .await
        .map_err(|e| format!("find walk task failed: {e}"))?;
        if cancelled {
            return Err("Operation aborted".to_string());
        }

        if results.is_empty() {
            return Ok(AgentToolResult::text("No files found matching pattern"));
        }
        results.sort();

        let limit_reached = results.len() >= effective_limit;
        let raw = results.join("\n");
        let truncation = truncate_head(&raw, Some(usize::MAX), None);
        let mut output = truncation.content.clone();

        let mut notices: Vec<String> = Vec::new();
        if limit_reached {
            notices.push(format!("{effective_limit} results limit reached"));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        Ok(AgentToolResult::text(output))
    }
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

const LS_DEFAULT_LIMIT: usize = 500;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LsParams {
    /// Directory to list (default: current directory)
    path: Option<String>,
    /// Maximum number of entries to return (default: 500)
    limit: Option<usize>,
}

pub struct LsTool {
    services: ToolServices,
}

impl LsTool {
    pub fn new(services: ToolServices) -> Self {
        LsTool { services }
    }
}

impl std::fmt::Debug for LsTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LsTool").finish()
    }
}

#[async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }
    fn label(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List directory contents. Returns entries sorted alphabetically (case-insensitive), with a trailing / for directories. Output is limited to 500 entries by default."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<LsParams>()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: LsParams =
            serde_json::from_value(params).map_err(|e| format!("invalid ls params: {e}"))?;
        let dir_path = resolve_to_cwd(params.path.as_deref().unwrap_or("."), &self.services.cwd);
        if !dir_path.exists() {
            return Err(format!("Path not found: {}", dir_path.display()));
        }
        if !dir_path.is_dir() {
            return Err(format!("Not a directory: {}", dir_path.display()));
        }
        let effective_limit = params.limit.unwrap_or(LS_DEFAULT_LIMIT);

        let mut entries: Vec<String> = Vec::new();
        let read_dir =
            std::fs::read_dir(&dir_path).map_err(|e| format!("Cannot read directory: {e}"))?;
        for entry in read_dir.flatten() {
            if cancel.is_cancelled() {
                return Err("Operation aborted".to_string());
            }
            let mut name = entry.file_name().to_string_lossy().to_string();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                name.push('/');
            }
            entries.push(name);
        }
        entries.sort_by_key(|a| a.to_lowercase());

        let mut limit_reached = false;
        if entries.len() > effective_limit {
            entries.truncate(effective_limit);
            limit_reached = true;
        }

        let raw = entries.join("\n");
        let truncation = truncate_head(&raw, Some(usize::MAX), None);
        let mut output = truncation.content.clone();

        let mut notices: Vec<String> = Vec::new();
        if limit_reached {
            notices.push(format!("{effective_limit} entries limit reached"));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        Ok(AgentToolResult::text(output))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn text_of(result: &AgentToolResult) -> String {
        match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn grep_huge_context_does_not_overflow() {
        // line_number + context must saturate, not overflow (debug panic).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let tool = GrepTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "pattern": "beta", "path": ".", "context": u64::MAX }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("f.txt:2: beta"), "{text}");
        assert!(text.contains("f.txt-1- alpha"), "{text}");
        assert!(text.contains("f.txt-3- gamma"), "{text}");
    }

    /// Oversized files are skipped (with a notice) instead of being read
    /// fully into memory; match-free files are not cached either.
    #[tokio::test]
    async fn grep_skips_oversized_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("small.txt"), "needle here\n").unwrap();
        let big = tmp.path().join("big.bin");
        // Sparse-ish large file containing the needle near the start.
        {
            use std::io::{Seek, Write};
            let mut f = std::fs::File::create(&big).unwrap();
            f.write_all(b"needle in giant file\n").unwrap();
            f.seek(std::io::SeekFrom::Start(GREP_MAX_FILE_BYTES + 1))
                .unwrap();
            f.write_all(b"x").unwrap();
        }
        let tool = GrepTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "pattern": "needle", "path": "." }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("small.txt:1: needle here"), "{text}");
        assert!(
            !text.contains("giant file"),
            "oversized file must be skipped: {text}"
        );
        assert!(text.contains("file(s) over"), "notice expected: {text}");
    }

    #[tokio::test]
    async fn grep_lone_carriage_return_line_numbers() {
        // A file with a lone '\r' shifts line numbering: matching and display
        // must agree (both normalize CR → LF).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "a\rbeta\nc\n").unwrap();
        let tool = GrepTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "pattern": "beta", "path": "." }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("f.txt:2: beta"), "{text}");
    }

    /// Streaming line matching: a large file with mixed \n / \r\n endings
    /// is scanned line by line (no whole-file line vector): line numbers
    /// and context lines must come out exact, and the match limit still
    /// stops the scan.
    #[tokio::test]
    async fn grep_streams_lines_with_exact_numbers_and_context() {
        let tmp = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for i in 1..=50_000usize {
            // A CRLF line in the middle must not shift numbering.
            content.push_str(if i == 25_000 {
                "filler\r\n"
            } else {
                "filler\n"
            });
        }
        content.push_str("needle at the end\n");
        std::fs::write(tmp.path().join("big.txt"), &content).unwrap();
        let tool = GrepTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "pattern": "needle", "path": ".", "context": 1 }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("big.txt:50001: needle at the end"), "{text}");
        assert!(text.contains("big.txt-50000- filler"), "{text}");

        // The limit still applies while streaming.
        let limited = tool
            .execute(
                "2",
                serde_json::json!({ "pattern": "filler", "path": ".", "limit": 5 }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&limited);
        assert_eq!(
            text.lines().filter(|l| l.contains(": filler")).count(),
            5,
            "{text}"
        );
        assert!(text.contains("5 matches limit reached"), "{text}");
    }

    /// With context=0 the matched-file cache stays empty (nothing to
    /// render context from), and the cumulative cache cap stops caching
    /// with a flag once exceeded.
    #[tokio::test]
    async fn grep_context_cache_is_conditional_and_capped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "needle a\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "needle b\n").unwrap();
        let regex = grep_regex::RegexMatcher::new("needle").unwrap();
        let search = grep_search(
            tmp.path().to_path_buf(),
            true,
            None,
            regex.clone(),
            100,
            ContextCacheCfg {
                enabled: false,
                max_bytes: GREP_CONTEXT_CACHE_MAX_BYTES,
            },
            CancellationToken::new(),
        );
        assert_eq!(search.matches.len(), 2);
        assert!(
            search.matched_files.is_empty(),
            "context=0 must not cache file texts"
        );
        assert!(!search.context_cache_truncated);

        // Cap smaller than one file: nothing cached, flag set.
        let search = grep_search(
            tmp.path().to_path_buf(),
            true,
            None,
            regex.clone(),
            100,
            ContextCacheCfg {
                enabled: true,
                max_bytes: 4,
            },
            CancellationToken::new(),
        );
        assert_eq!(search.matches.len(), 2);
        assert!(search.matched_files.is_empty());
        assert!(search.context_cache_truncated);

        // Room for exactly one file: first cached, second trips the cap.
        let search = grep_search(
            tmp.path().to_path_buf(),
            true,
            None,
            regex,
            100,
            ContextCacheCfg {
                enabled: true,
                max_bytes: "needle a\n".len(),
            },
            CancellationToken::new(),
        );
        assert!(search.context_cache_truncated);
        assert!(search.context_cache_bytes <= "needle a\n".len());
    }

    /// A caller-supplied limit above GREP_MAX_LIMIT is clamped, and the
    /// result says so.
    #[tokio::test]
    async fn grep_limit_is_clamped_with_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "needle\n".repeat(GREP_MAX_LIMIT + 100);
        std::fs::write(tmp.path().join("many.txt"), content).unwrap();
        let tool = GrepTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "pattern": "needle", "path": ".", "limit": usize::MAX }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains(&format!("limit clamped to {GREP_MAX_LIMIT}")),
            "{text}"
        );
        assert!(
            text.contains(&format!("{GREP_MAX_LIMIT} matches limit reached")),
            "{text}"
        );
    }

    #[tokio::test]
    async fn find_and_ls_basic() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        std::fs::write(tmp.path().join("sub/b.rs"), "").unwrap();

        let find = FindTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = find
            .execute(
                "1",
                serde_json::json!({ "pattern": "**/*.rs", "path": "." }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("a.rs"), "{text}");
        assert!(text.contains("sub/b.rs"), "{text}");

        let ls = LsTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = ls
            .execute(
                "2",
                serde_json::json!({ "path": "." }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("sub/"), "{text}");
        assert!(text.contains("a.rs"), "{text}");
    }
}
