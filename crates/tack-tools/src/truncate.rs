//! Shared truncation utilities for tool outputs. Port of
//! `packages/coding-agent/src/core/tools/truncate.ts`.
//!
//! Two independent limits — whichever is hit first wins:
//! - line limit (default 2000)
//! - byte limit (default 50KB)
//!
//! Never returns partial lines (except the bash tail-truncation edge case).

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Clone, Debug)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Human-readable byte size (matches TS `formatSize`).
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn untruncated(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let lines = split_lines_for_counting(content);
    TruncationResult {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines: lines.len(),
        total_bytes: content.len(),
        output_lines: lines.len(),
        output_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Line count matching split_lines_for_counting (a trailing '\n' is a
/// terminator, not an extra line), computed without allocating.
pub fn count_lines(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let newlines = content.as_bytes().iter().filter(|&&b| b == b'\n').count();
    newlines + usize::from(!content.ends_with('\n'))
}

/// An untruncated result for content the caller has ALREADY verified fits
/// both budgets — skips the line-split allocation of the general path
/// (used by the streaming snapshot fast path).
pub fn untruncated_fast(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let lines = count_lines(content);
    TruncationResult {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines: lines,
        total_bytes: content.len(),
        output_lines: lines,
        output_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keep the first N lines/bytes (file reads).
pub fn truncate_head(
    content: &str,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> TruncationResult {
    let max_lines = max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let lines = split_lines_for_counting(content);
    if lines.len() <= max_lines && content.len() <= max_bytes {
        return untruncated(content, max_lines, max_bytes);
    }

    // First line alone exceeds the byte limit.
    if lines[0].len() > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines: lines.len(),
            total_bytes: content.len(),
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (i, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = line.len() + usize::from(i > 0); // +1 newline
        if output_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output.push(line);
        output_bytes += line_bytes;
    }
    if output.len() >= max_lines && output_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    if output.len() == lines.len() {
        // Every line was kept: the over-budget byte was the trailing
        // newline (a line terminator, not content). Not a truncation.
        return untruncated(content, max_lines, max_bytes);
    }

    let content_out = output.join("\n");
    TruncationResult {
        output_bytes: content_out.len(),
        output_lines: output.len(),
        content: content_out,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines: lines.len(),
        total_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a string to fit within a byte limit, from the end, on a UTF-8
/// boundary.
fn truncate_str_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// Keep the last N lines/bytes (bash output).
pub fn truncate_tail(
    content: &str,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> TruncationResult {
    let max_lines = max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let lines = split_lines_for_counting(content);
    if lines.len() <= max_lines && content.len() <= max_bytes {
        return untruncated(content, max_lines, max_bytes);
    }

    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    let mut partial: Option<String> = None;

    for line in lines.iter().rev() {
        if output.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!output.is_empty());
        if output_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if output.is_empty() {
                // Edge case: the last line alone exceeds the budget — take its tail.
                let truncated_line = truncate_str_to_bytes_from_end(line, max_bytes);
                output_bytes = truncated_line.len();
                partial = Some(truncated_line);
                last_line_partial = true;
            }
            break;
        }
        output.push(line);
        output_bytes += line_bytes;
    }
    if output.len() >= max_lines && output_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    if partial.is_none() && output.len() == lines.len() {
        // Every line was kept: see truncate_head.
        return untruncated(content, max_lines, max_bytes);
    }

    output.reverse();
    let content_out = match partial {
        Some(p) => p,
        None => output.join("\n"),
    };
    TruncationResult {
        output_bytes: content_out.len(),
        output_lines: output.len().max(usize::from(last_line_partial)),
        content: content_out,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines: lines.len(),
        total_bytes: content.len(),
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a single line to max chars with a `[truncated]` suffix (grep).
pub fn truncate_line(line: &str, max_chars: Option<usize>) -> (String, bool) {
    let max_chars = max_chars.unwrap_or(GREP_MAX_LINE_LENGTH);
    if line.chars().count() <= max_chars {
        return (line.to_string(), false);
    }
    let truncated: String = line.chars().take(max_chars).collect();
    (format!("{truncated}... [truncated]"), true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn no_truncation_needed() {
        let r = truncate_head("a\nb\nc", None, None);
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb\nc");
        assert_eq!(r.total_lines, 3);
    }

    #[test]
    fn head_line_limit() {
        let content = (1..=3000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_head(&content, Some(100), None);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(r.output_lines, 100);
        assert!(r.content.ends_with("100"));
    }

    #[test]
    fn head_byte_limit_no_partial_lines() {
        let line = "x".repeat(100);
        let content = vec![line; 10].join("\n");
        let r = truncate_head(&content, None, Some(250));
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(r.output_lines, 2); // 100 + 101
    }

    #[test]
    fn head_first_line_exceeds() {
        let content = format!("{}\nshort", "x".repeat(60 * 1024));
        let r = truncate_head(&content, None, None);
        assert!(r.first_line_exceeds_limit);
        assert_eq!(r.content, "");
    }

    #[test]
    fn tail_keeps_end() {
        let content = (1..=100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_tail(&content, Some(10), None);
        assert_eq!(r.output_lines, 10);
        assert!(r.content.starts_with("91"));
        assert!(r.content.ends_with("100"));
    }

    #[test]
    fn tail_partial_last_line_utf8_safe() {
        let content = "中".repeat(40 * 1024); // 3 bytes each, > 50KB
        let r = truncate_tail(&content, None, None);
        assert!(r.last_line_partial);
        assert!(r.output_bytes <= DEFAULT_MAX_BYTES);
        assert!(r.content.chars().all(|c| c == '中'));
    }

    #[test]
    fn grep_line_truncation() {
        let (text, was) = truncate_line(&"a".repeat(600), None);
        assert!(was);
        assert!(text.ends_with("... [truncated]"));
        let (_, was) = truncate_line("short", None);
        assert!(!was);
    }

    /// A trailing newline is a line TERMINATOR, not an extra byte of
    /// content: when every line fits within both budgets the result must
    /// not be flagged truncated (previously content one byte over the byte
    /// budget solely due to its trailing newline was reported as
    /// truncated-by-lines with the full content — a false positive that
    /// also mislabels the cause).
    #[test]
    fn trailing_newline_at_byte_budget_is_not_truncated() {
        let content = format!("{}\n", "x".repeat(100));
        let head = truncate_head(&content, None, Some(100));
        assert!(!head.truncated, "head: {head:?}");
        assert_eq!(head.truncated_by, None);
        let tail = truncate_tail(&content, None, Some(100));
        assert!(!tail.truncated, "tail: {tail:?}");
        assert_eq!(tail.truncated_by, None);

        // Genuinely over-budget content still truncates.
        let content = format!("{}\n{}", "x".repeat(100), "y".repeat(100));
        let head = truncate_head(&content, None, Some(150));
        assert!(head.truncated);
        assert_eq!(head.truncated_by, Some(TruncatedBy::Bytes));
    }
}
