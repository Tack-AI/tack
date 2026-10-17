//! Edit application and diff generation. Port of
//! `packages/coding-agent/src/core/tools/edit-diff.ts`: exact + fuzzy
//! matching, line-ending preservation, and display/unified diffs (via the
//! `similar` crate instead of jsdiff).
//!
//! Matching is delegated to [`crate::edit_fuzzy`]: an exact-first ladder with
//! Levenshtein-scored fuzzy windows whose byte offsets address the original
//! content, so unchanged lines are preserved byte-for-byte and fuzzy hits at
//! a different nesting depth are re-indented via `adjust_indentation`.

use crate::edit_fuzzy::{
    DEFAULT_FUZZY_THRESHOLD, FindMatchOptions, FuzzyMatch, adjust_indentation,
    exact_occurrence_starts, find_match, format_exact_occurrences_error, format_match_error,
    format_occurrence_error,
};

/// Split a leading UTF-8 BOM from file content.
pub fn split_bom(content: &str) -> (String, &str) {
    if let Some(rest) = content.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}".to_string(), rest)
    } else {
        (String::new(), content)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
    /// Classic Mac OS style: lone '\r' with no '\n' anywhere.
    Cr,
}

pub fn detect_line_ending(content: &str) -> LineEnding {
    let crlf = content.find("\r\n");
    let lf = content.find('\n');
    match (crlf, lf) {
        (Some(c), Some(l)) if c < l => LineEnding::CrLf,
        (Some(_), Some(_)) => LineEnding::Lf,
        // No '\n' at all: a '\r'-only file must round-trip as CR, otherwise
        // normalize_to_lf (which maps lone '\r' → '\n') would rewrite the
        // whole file's line endings to LF on any edit.
        (None, None) if content.contains('\r') => LineEnding::Cr,
        _ => LineEnding::Lf,
    }
}

pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::CrLf => text.replace('\n', "\r\n"),
        LineEnding::Cr => text.replace('\n', "\r"),
        LineEnding::Lf => text.to_string(),
    }
}

/// How an anchor range treats its `to` anchor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RangeMode {
    /// Replace up to (but not including) the `to` anchor; the anchor line
    /// survives. The default for range edits.
    #[default]
    Exclusive,
    /// Replace the `to` anchor as well, plus the newline terminating its
    /// line (so deleting an anchored block does not leave a blank line).
    Inclusive,
}

#[derive(Clone, Debug)]
pub enum EditKind {
    /// Classic exact/fuzzy replacement of one unique region.
    Replace {
        old_text: String,
        /// Replace every exact occurrence instead of requiring uniqueness.
        all: bool,
        /// Optional guard for `all`: the exact expected occurrence count.
        count: Option<usize>,
    },
    /// Replace the region from `from` to `to`. Anchors are exact-only:
    /// fuzzy anchors could silently target the wrong region, so duplicates
    /// and misses are rejected with every candidate location listed.
    Range {
        from: String,
        /// `None` = end of file.
        to: Option<String>,
        mode: RangeMode,
        /// Optional guard: expected number of lines in the replaced range.
        expect_lines: Option<usize>,
    },
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub kind: EditKind,
    pub new_text: String,
}

impl Edit {
    /// One exact/fuzzy replacement of a unique region.
    pub fn replace(old_text: impl Into<String>, new_text: impl Into<String>) -> Self {
        Edit {
            kind: EditKind::Replace {
                old_text: old_text.into(),
                all: false,
                count: None,
            },
            new_text: new_text.into(),
        }
    }

    /// Replace every exact occurrence (optionally guarded by `count`).
    pub fn replace_all(
        old_text: impl Into<String>,
        new_text: impl Into<String>,
        count: Option<usize>,
    ) -> Self {
        Edit {
            kind: EditKind::Replace {
                old_text: old_text.into(),
                all: true,
                count,
            },
            new_text: new_text.into(),
        }
    }

    /// Replace the region between two exact anchors (`to = None`: EOF).
    pub fn range(
        from: impl Into<String>,
        to: Option<String>,
        mode: RangeMode,
        expect_lines: Option<usize>,
        new_text: impl Into<String>,
    ) -> Self {
        Edit {
            kind: EditKind::Range {
                from: from.into(),
                to,
                mode,
                expect_lines,
            },
            new_text: new_text.into(),
        }
    }

    /// LF-normalize every text field (matching always runs on normalized
    /// content; line endings are restored by the caller before writing).
    fn normalized(&self) -> Self {
        let kind = match &self.kind {
            EditKind::Replace {
                old_text,
                all,
                count,
            } => EditKind::Replace {
                old_text: normalize_to_lf(old_text),
                all: *all,
                count: *count,
            },
            EditKind::Range {
                from,
                to,
                mode,
                expect_lines,
            } => EditKind::Range {
                from: normalize_to_lf(from),
                to: to.as_deref().map(normalize_to_lf),
                mode: *mode,
                expect_lines: *expect_lines,
            },
        };
        Edit {
            kind,
            new_text: normalize_to_lf(&self.new_text),
        }
    }
}

#[derive(Debug)]
pub struct AppliedEdits {
    pub base_content: String,
    pub new_content: String,
    /// Number of regions replaced (occurrences for `all`, one per range).
    pub replacements: usize,
}

#[derive(Clone, Debug)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

fn apply_replacements(content: &str, replacements: &[MatchedEdit], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let index = replacement.match_index - offset;
        result.replace_range(
            index..index + replacement.match_length,
            &replacement.new_text,
        );
    }
    result
}

/// Apply edits to LF-normalized content. All edits match against the same
/// original; replacements apply in reverse offset order.
///
/// `EditKind::Replace` keeps the exact-first fuzzy ladder; `all` and
/// `EditKind::Range` are exact-only (a fuzzy anchor could silently target
/// the wrong region). Every edit is resolved before anything is applied, and
/// *all* failures are reported together — a multi-hunk call retries once with
/// every bad hunk fixed instead of discovering them one round-trip at a time.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEdits, String> {
    let normalized_edits: Vec<Edit> = edits.iter().map(Edit::normalized).collect();
    let single = normalized_edits.len() == 1;

    let mut matched: Vec<MatchedEdit> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (i, edit) in normalized_edits.iter().enumerate() {
        match resolve_edit(normalized_content, edit, i, path) {
            Ok(mut resolved) => matched.append(&mut resolved),
            Err(detail) => errors.push(if single {
                detail
            } else {
                format!("edits[{i}]: {detail}")
            }),
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n\n"));
    }

    matched.sort_by_key(|m| m.match_index);
    let mut overlap_errors: Vec<String> = Vec::new();
    for pair in matched.windows(2) {
        let (previous, current) = (&pair[0], &pair[1]);
        if previous.match_index + previous.match_length > current.match_index {
            overlap_errors.push(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }
    if !overlap_errors.is_empty() {
        return Err(overlap_errors.join("\n\n"));
    }

    let new_content = apply_replacements(normalized_content, &matched, 0);

    if normalized_content == new_content {
        return Err(if single {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        });
    }

    Ok(AppliedEdits {
        base_content: normalized_content.to_string(),
        new_content,
        replacements: matched.len(),
    })
}

/// Resolve one edit into concrete byte ranges on `content`.
fn resolve_edit(
    content: &str,
    edit: &Edit,
    edit_index: usize,
    path: &str,
) -> Result<Vec<MatchedEdit>, String> {
    match &edit.kind {
        EditKind::Replace {
            old_text,
            all,
            count,
        } => {
            if old_text.is_empty() {
                return Err(format!("oldText must not be empty in {path}."));
            }
            if count.is_some() && !all {
                return Err(format!("count requires all: true in {path}."));
            }
            if *all {
                replace_all_matches(content, old_text, *count, edit_index, &edit.new_text, path)
            } else {
                replace_unique_match(content, old_text, edit_index, &edit.new_text, path)
            }
        }
        EditKind::Range {
            from,
            to,
            mode,
            expect_lines,
        } => {
            if from.is_empty() {
                return Err(format!("from must not be empty in {path}."));
            }
            if to.as_deref().is_some_and(str::is_empty) {
                return Err(format!(
                    "to must not be empty in {path} (omit it to target EOF)."
                ));
            }
            range_match(
                content,
                from,
                to.as_deref(),
                *mode,
                *expect_lines,
                edit_index,
                &edit.new_text,
                path,
            )
        }
    }
}

/// Classic exact-then-fuzzy replacement of one unique region.
fn replace_unique_match(
    content: &str,
    old_text: &str,
    edit_index: usize,
    new_text: &str,
    path: &str,
) -> Result<Vec<MatchedEdit>, String> {
    let outcome = find_match(
        content,
        old_text,
        &FindMatchOptions {
            allow_fuzzy: true,
            threshold: Some(DEFAULT_FUZZY_THRESHOLD),
        },
    );
    if outcome.occurrences.is_some_and(|count| count > 1) {
        return Err(format_occurrence_error(path, &outcome));
    }
    let Some(found) = outcome.matched else {
        let detail = if outcome.fuzzy_skipped == Some(true) {
            format!(
                "Could not find a close enough match in {path}. The file is too large for fuzzy matching; oldText must match the file exactly (re-read the exact text and retry)."
            )
        } else {
            format_match_error(
                path,
                old_text,
                outcome.closest.as_ref(),
                DEFAULT_FUZZY_THRESHOLD,
                outcome.fuzzy_matches,
            )
        };
        return Err(detail);
    };
    // A fuzzy hit at a different nesting depth carries its own indentation;
    // re-indent the replacement to match.
    let adjusted = adjust_indentation(old_text, &found.actual_text, new_text);
    Ok(vec![MatchedEdit {
        edit_index,
        match_index: found.start_index,
        match_length: found.actual_text.len(),
        new_text: adjusted,
    }])
}

/// Replace every **exact** occurrence of `old_text`. Fuzzy matching is
/// deliberately disabled: "replace all fuzzy hits" is unpredictable. A
/// `count` guard makes the common bulk-edit mistake (renaming one of N
/// call sites) fail instead of silently applying.
fn replace_all_matches(
    content: &str,
    old_text: &str,
    count: Option<usize>,
    edit_index: usize,
    new_text: &str,
    path: &str,
) -> Result<Vec<MatchedEdit>, String> {
    /// Upper bound for one `all: true` edit. A short oldText can occur
    /// thousands of times; replacing them all is never what the caller
    /// wants, and the resulting diff would swamp the response.
    const MAX_REPLACE_ALL: usize = 512;
    let (starts, total) = exact_occurrence_starts(content, old_text, 0, MAX_REPLACE_ALL);
    if total == 0 {
        let outcome = find_match(
            content,
            old_text,
            &FindMatchOptions {
                allow_fuzzy: false,
                threshold: None,
            },
        );
        return Err(format!(
            "No exact occurrence of oldText in {path}; `all: true` requires exact matches (fuzzy matching is disabled).{}",
            closest_candidate_hint(old_text, outcome.closest.as_ref())
        ));
    }
    if total > MAX_REPLACE_ALL {
        return Err(format!(
            "oldText occurs {total} times in {path}; `all: true` is capped at {MAX_REPLACE_ALL} replacements. Use a more specific oldText."
        ));
    }
    // Overlapping exact occurrences ("aa" in "aaa") have no well-defined
    // replace-all result; reject rather than pick an order.
    for pair in starts.windows(2) {
        if pair[0] + old_text.len() > pair[1] {
            return Err(format!(
                "oldText occurrences overlap in {path} (offsets {} and {}); add context so they are disjoint, or use a single replacement.",
                pair[0], pair[1]
            ));
        }
    }
    if let Some(expected) = count
        && expected != total
    {
        return Err(format!(
            "Expected {expected} occurrence(s) of oldText in {path} (count), found {total}. Adjust count or oldText."
        ));
    }
    Ok(starts
        .into_iter()
        .map(|start| MatchedEdit {
            edit_index,
            match_index: start,
            match_length: old_text.len(),
            new_text: new_text.to_string(),
        })
        .collect())
}

/// Replace the region selected by two exact anchors.
#[allow(clippy::too_many_arguments)]
fn range_match(
    content: &str,
    from: &str,
    to: Option<&str>,
    mode: RangeMode,
    expect_lines: Option<usize>,
    edit_index: usize,
    new_text: &str,
    path: &str,
) -> Result<Vec<MatchedEdit>, String> {
    /// Occurrence previews shown on ambiguity (matches edit_fuzzy's limit).
    const MAX_ANCHOR_PREVIEWS: usize = 5;
    let (from_starts, from_total) = exact_occurrence_starts(content, from, 0, MAX_ANCHOR_PREVIEWS);
    if from_total == 0 {
        let outcome = find_match(
            content,
            from,
            &FindMatchOptions {
                allow_fuzzy: false,
                threshold: None,
            },
        );
        return Err(format!(
            "Could not find the `from` anchor in {path}; range anchors match exactly (no fuzzy).{}",
            closest_candidate_hint(from, outcome.closest.as_ref())
        ));
    }
    if from_total > 1 {
        return Err(format_exact_occurrences_error(
            path,
            content,
            "`from` anchor",
            &from_starts,
            from_total,
        ));
    }
    let from_start = from_starts[0];
    let from_end = from_start + from.len();

    let end = match to {
        None => content.len(),
        Some(to_text) => {
            let (to_starts, to_total) =
                exact_occurrence_starts(content, to_text, from_end, MAX_ANCHOR_PREVIEWS);
            if to_total == 0 {
                return Err(format!(
                    "Could not find the `to` anchor after the `from` anchor in {path}; range anchors match exactly (no fuzzy). Widen or re-read the `to` anchor."
                ));
            }
            if to_total > 1 {
                return Err(format_exact_occurrences_error(
                    path,
                    content,
                    "`to` anchor after `from`",
                    &to_starts,
                    to_total,
                ));
            }
            let to_start = to_starts[0];
            if mode == RangeMode::Inclusive {
                let mut end = to_start + to_text.len();
                // Inclusive means "the whole anchor line": consume the
                // line terminator so a delete doesn't leave a blank line
                // behind. (Anchors are text, not lines, so only the single
                // newline directly after the anchor is absorbed.)
                if content[end..].starts_with('\n') {
                    end += 1;
                }
                end
            } else {
                to_start
            }
        }
    };

    if let Some(expected) = expect_lines {
        let found = range_line_count(&content[from_start..end]);
        if expected != found {
            return Err(format!(
                "Expected {expected} line(s) in the range selected by the `from`/`to` anchors in {path} (expectLines), found {found}. Widen the anchors or fix expectLines."
            ));
        }
    }

    Ok(vec![MatchedEdit {
        edit_index,
        match_index: from_start,
        match_length: end - from_start,
        new_text: new_text.to_string(),
    }])
}

/// Number of lines a replaced range spans. A trailing newline terminates the
/// last line rather than starting an empty one, so "a\nb" and "a\nb\n" are
/// both two lines.
fn range_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|byte| *byte == b'\n').count() + usize::from(!text.ends_with('\n'))
    }
}

/// One compact "closest candidate" hint for exact-only anchor misses.
fn closest_candidate_hint(search: &str, closest: Option<&FuzzyMatch>) -> String {
    let Some(closest) = closest else {
        return String::new();
    };
    format!(
        "\n\nClosest candidate ({:.0}% similar) at line {}:\n  - {}\n  + {}",
        (closest.confidence * 100.0).round(),
        closest.start_line,
        search.lines().next().unwrap_or(""),
        closest.actual_text.lines().next().unwrap_or("")
    )
}

#[derive(Debug)]
pub struct DiffOutput {
    pub diff: String,
    pub first_changed_line: Option<usize>,
}

/// Display-oriented diff with line numbers and context (port of
/// `generateDiffString`).
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> DiffOutput {
    let diff = similar::TextDiff::from_lines(old_content, new_content);
    let old_lines = old_content.split('\n').count();
    let new_lines = new_content.split('\n').count();
    let line_num_width = old_lines.max(new_lines).to_string().len();

    // Collect change runs: (tag, lines) mirroring jsdiff's diffLines parts.
    #[derive(PartialEq)]
    enum Tag {
        Same,
        Added,
        Removed,
    }
    let mut parts: Vec<(Tag, Vec<String>)> = Vec::new();
    for change in diff.iter_all_changes() {
        let tag = match change.tag() {
            similar::ChangeTag::Equal => Tag::Same,
            similar::ChangeTag::Insert => Tag::Added,
            similar::ChangeTag::Delete => Tag::Removed,
        };
        let value = change.value();
        let mut lines: Vec<String> = value.split('\n').map(|s| s.to_string()).collect();
        if lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        if let Some(last) = parts.last_mut()
            && last.0 == tag
        {
            last.1.extend(lines);
            continue;
        }
        parts.push((tag, lines));
    }

    let mut output: Vec<String> = Vec::new();
    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    for (i, (tag, raw)) in parts.iter().enumerate() {
        if *tag != Tag::Same {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }
            for line in raw {
                match tag {
                    Tag::Added => {
                        output.push(format!(
                            "+{:>width$} {line}",
                            new_line_num,
                            width = line_num_width
                        ));
                        new_line_num += 1;
                    }
                    Tag::Removed => {
                        output.push(format!(
                            "-{:>width$} {line}",
                            old_line_num,
                            width = line_num_width
                        ));
                        old_line_num += 1;
                    }
                    Tag::Same => unreachable!(),
                }
            }
            last_was_change = true;
            continue;
        }

        let next_is_change = parts.get(i + 1).is_some_and(|(t, _)| *t != Tag::Same);
        let pad = " ".repeat(line_num_width);
        match (last_was_change, next_is_change) {
            (true, true) => {
                if raw.len() <= context_lines * 2 {
                    for line in raw {
                        output.push(format!(
                            " {:>width$} {line}",
                            old_line_num,
                            width = line_num_width
                        ));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    for line in &raw[..context_lines] {
                        output.push(format!(
                            " {:>width$} {line}",
                            old_line_num,
                            width = line_num_width
                        ));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                    output.push(format!(" {pad} ..."));
                    let skipped = raw.len() - context_lines * 2;
                    old_line_num += skipped;
                    new_line_num += skipped;
                    for line in &raw[raw.len() - context_lines..] {
                        output.push(format!(
                            " {:>width$} {line}",
                            old_line_num,
                            width = line_num_width
                        ));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            }
            (true, false) => {
                for line in raw.iter().take(context_lines) {
                    output.push(format!(
                        " {:>width$} {line}",
                        old_line_num,
                        width = line_num_width
                    ));
                    old_line_num += 1;
                    new_line_num += 1;
                }
                let skipped = raw.len() - raw.len().min(context_lines);
                if skipped > 0 {
                    output.push(format!(" {pad} ..."));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
            }
            (false, true) => {
                let skipped = raw.len().saturating_sub(context_lines);
                if skipped > 0 {
                    output.push(format!(" {pad} ..."));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
                for line in &raw[skipped..] {
                    output.push(format!(
                        " {:>width$} {line}",
                        old_line_num,
                        width = line_num_width
                    ));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            }
            (false, false) => {
                old_line_num += raw.len();
                new_line_num += raw.len();
            }
        }
        last_was_change = false;
    }

    DiffOutput {
        diff: output.join("\n"),
        first_changed_line,
    }
}

/// Standard unified patch (`--- path` / `+++ path` headers only).
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    similar::TextDiff::from_lines(old_content, new_content)
        .unified_diff()
        .context_radius(context_lines)
        .header(path, path)
        .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit::replace(old, new)
    }

    fn range(from: &str, to: Option<&str>, new: &str) -> Edit {
        Edit::range(
            from,
            to.map(str::to_string),
            RangeMode::Exclusive,
            None,
            new,
        )
    }

    #[test]
    fn exact_replacement() {
        let r = apply_edits_to_normalized_content("hello world", &[edit("world", "rust")], "f")
            .unwrap();
        assert_eq!(r.new_content, "hello rust");
    }

    #[test]
    fn not_found_error() {
        let e =
            apply_edits_to_normalized_content("hello", &[edit("xyz", "q")], "f.rs").unwrap_err();
        assert!(
            e.contains("Could not find a close enough match in f.rs"),
            "{e}"
        );
    }

    #[test]
    fn not_found_error_shows_closest_match() {
        let content = "function calculateTotal(items) {\n  return items.sum();\n}";
        let e = apply_edits_to_normalized_content(
            content,
            &[edit("function calcuateTotal(itemz) {", "x")],
            "f.rs",
        )
        .unwrap_err();
        assert!(e.contains("Closest match"), "{e}");
        assert!(e.contains("similarity threshold"), "{e}");
    }

    #[test]
    fn duplicate_error() {
        let e = apply_edits_to_normalized_content("a a", &[edit("a", "b")], "f.rs").unwrap_err();
        assert!(e.contains("Found 2 occurrences"));
    }

    #[test]
    fn overlap_error() {
        let e = apply_edits_to_normalized_content(
            "abcdef",
            &[edit("abc", "x"), edit("bcd", "y")],
            "f.rs",
        )
        .unwrap_err();
        assert!(e.contains("overlap"));
    }

    #[test]
    fn fuzzy_trailing_whitespace() {
        let content = "fn main() {  \n    foo();\n}\n";
        let r = apply_edits_to_normalized_content(
            content,
            &[edit("fn main() {\n    foo();", "fn main() {\n    bar();")],
            "f",
        )
        .unwrap();
        assert!(r.new_content.contains("bar();"));
        // Unchanged trailing line preserved.
        assert!(r.new_content.ends_with("}\n"));
    }

    #[test]
    fn fuzzy_smart_quotes() {
        let content = "let s = \u{201C}hello\u{201D};";
        let r = apply_edits_to_normalized_content(
            content,
            &[edit("let s = \"hello\";", "let s = \"bye\";")],
            "f",
        )
        .unwrap();
        assert!(r.new_content.contains("bye"));
    }

    #[test]
    fn no_change_error() {
        let e = apply_edits_to_normalized_content("abc", &[edit("abc", "abc")], "f").unwrap_err();
        assert!(e.contains("No changes made"));
    }

    #[test]
    fn multi_edit_reverse_order() {
        let r = apply_edits_to_normalized_content(
            "one two three",
            &[edit("one", "1"), edit("three", "3")],
            "f",
        )
        .unwrap();
        assert_eq!(r.new_content, "1 two 3");
    }

    #[test]
    fn crlf_roundtrip() {
        let content = "a\r\nb\r\nc\r\n";
        assert_eq!(detect_line_ending(content), LineEnding::CrLf);
        let normalized = normalize_to_lf(content);
        let r =
            apply_edits_to_normalized_content(normalized.as_str(), &[edit("b", "B")], "f").unwrap();
        let restored = restore_line_endings(&r.new_content, LineEnding::CrLf);
        assert_eq!(restored, "a\r\nB\r\nc\r\n");
    }

    /// Regression: CR-only (classic Mac) files must round-trip as CR —
    /// previously restore only knew LF/CRLF, so any edit rewrote the whole
    /// file's line endings to LF.
    #[test]
    fn cr_only_roundtrip() {
        let content = "a\rb\rc\r";
        assert_eq!(detect_line_ending(content), LineEnding::Cr);
        let normalized = normalize_to_lf(content);
        let r =
            apply_edits_to_normalized_content(normalized.as_str(), &[edit("b", "B")], "f").unwrap();
        let restored = restore_line_endings(&r.new_content, detect_line_ending(content));
        assert_eq!(restored, "a\rB\rc\r");
        // LF and CRLF detection unchanged.
        assert_eq!(detect_line_ending("a\nb\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\r\nb\n"), LineEnding::CrLf);
        assert_eq!(detect_line_ending("plain"), LineEnding::Lf);
        // Mixed lone-CR and LF: LF wins (matches previous behavior).
        assert_eq!(detect_line_ending("a\rb\nc"), LineEnding::Lf);
    }

    #[test]
    fn diff_string_has_markers() {
        let d = generate_diff_string("a\nb\nc\n", "a\nB\nc\n", 4);
        assert!(d.diff.contains("-2 b"));
        assert!(d.diff.contains("+2 B"));
        assert_eq!(d.first_changed_line, Some(2));
    }

    #[test]
    fn fuzzy_hit_is_reindented_to_matched_depth() {
        // The needle is authored at depth 0 but the file nests the block one
        // level deeper; the replacement must land at the matched depth.
        let content = "impl A {\n    fn run() {\n        step();\n    }\n}\n";
        let r = apply_edits_to_normalized_content(
            content,
            &[edit(
                "fn run() {\n    step();",
                "fn run() {\n    step();\n    step2();",
            )],
            "f",
        )
        .unwrap();
        assert!(
            r.new_content
                .contains("    fn run() {\n        step();\n        step2();"),
            "{}",
            r.new_content
        );
    }

    #[test]
    fn whitespace_only_old_text_unique_exact_match() {
        // A whitespace-only oldText normalizes to an empty fuzzy needle; it
        // must not be rejected as "Found N occurrences" when it matches
        // exactly once.
        let r =
            apply_edits_to_normalized_content("a b\nq  r\ns t", &[edit("  ", " ")], "f").unwrap();
        assert_eq!(r.new_content, "a b\nq r\ns t");
    }

    #[test]
    fn trailing_whitespace_old_text_unique_exact_match() {
        // "foo " matches exactly once; fuzzy counting ("foo") would see two
        // occurrences and falsely report a duplicate.
        let r =
            apply_edits_to_normalized_content("foo \nfoo\n", &[edit("foo ", "bar ")], "f").unwrap();
        assert_eq!(r.new_content, "bar \nfoo\n");
    }

    #[test]
    fn whitespace_only_old_text_not_found_is_error_not_silent_insert() {
        // Without an exact or high-confidence fuzzy match, a whitespace-only
        // needle must not splice new_text at offset 0.
        let e = apply_edits_to_normalized_content("abc", &[edit("  ", "xx")], "f").unwrap_err();
        assert!(e.contains("Could not find"), "{e}");
    }

    // ------------------------------------------------------------------
    // Anchor-range edits
    // ------------------------------------------------------------------

    #[test]
    fn range_exclusive_keeps_the_to_anchor() {
        let content = "head\n// BEGIN\none\ntwo\n// END\ntail\n";
        let r = apply_edits_to_normalized_content(
            content,
            &[range("// BEGIN", Some("// END"), "BRIDGE\n")],
            "f.rs",
        )
        .unwrap();
        assert_eq!(r.new_content, "head\nBRIDGE\n// END\ntail\n");
        assert_eq!(r.replacements, 1);
    }

    #[test]
    fn range_inclusive_replaces_the_to_anchor() {
        let content = "head\n// BEGIN\none\ntwo\n// END\ntail\n";
        let edit = Edit::range(
            "// BEGIN",
            Some("// END".to_string()),
            RangeMode::Inclusive,
            None,
            "BRIDGE\n",
        );
        let r = apply_edits_to_normalized_content(content, &[edit], "f.rs").unwrap();
        assert_eq!(r.new_content, "head\nBRIDGE\ntail\n");
    }

    #[test]
    fn range_without_to_replaces_through_eof() {
        let content = "keep\n// BEGIN\nold\nold2\n";
        let r = apply_edits_to_normalized_content(content, &[range("// BEGIN", None, "")], "f")
            .unwrap();
        assert_eq!(r.new_content, "keep\n");
    }

    #[test]
    fn range_duplicate_from_is_rejected_with_every_location() {
        let content = "// BEGIN\na\n// BEGIN\nb\n// END\n";
        let e = apply_edits_to_normalized_content(
            content,
            &[range("// BEGIN", Some("// END"), "x")],
            "f.rs",
        )
        .unwrap_err();
        assert!(
            e.contains("Found 2 occurrences of the `from` anchor"),
            "{e}"
        );
        assert!(e.contains("1 | "), "missing first preview: {e}");
        assert!(e.contains("3 | "), "missing second preview: {e}");
        assert!(e.contains("unique"), "{e}");
    }

    #[test]
    fn range_duplicate_to_after_from_is_rejected() {
        let content = "// BEGIN\na\n// END\nb\n// END\n";
        let e = apply_edits_to_normalized_content(
            content,
            &[range("// BEGIN", Some("// END"), "x")],
            "f.rs",
        )
        .unwrap_err();
        assert!(
            e.contains("2 occurrences of the `to` anchor after `from`"),
            "{e}"
        );
        assert!(e.contains("2 | "), "missing first to preview: {e}");
        assert!(e.contains("4 | "), "missing second to preview: {e}");
    }

    #[test]
    fn range_missing_anchor_suggests_closest_candidate() {
        let content = "head\n// BEGIN\none\n// END\n";
        let e = apply_edits_to_normalized_content(
            content,
            &[range("// BEGN", Some("// END"), "x")],
            "f.rs",
        )
        .unwrap_err();
        assert!(e.contains("Could not find the `from` anchor"), "{e}");
        assert!(e.contains("Closest candidate"), "{e}");
        assert!(e.contains("// BEGIN"), "{e}");
    }

    #[test]
    fn range_expect_lines_guards_mis_anchored_ranges() {
        let content = "// BEGIN\none\ntwo\n// END\n";
        let ok = Edit::range(
            "// BEGIN",
            Some("// END".to_string()),
            RangeMode::Exclusive,
            Some(3),
            "x",
        );
        assert!(
            apply_edits_to_normalized_content(content, &[ok], "f").is_ok(),
            "3-line range should pass expectLines"
        );
        let bad = Edit::range(
            "// BEGIN",
            Some("// END".to_string()),
            RangeMode::Exclusive,
            Some(1),
            "x",
        );
        let e = apply_edits_to_normalized_content(content, &[bad], "f").unwrap_err();
        assert!(e.contains("Expected 1 line(s)"), "{e}");
        assert!(e.contains("found 3"), "{e}");
    }

    #[test]
    fn range_and_replace_apply_against_the_original() {
        let content = "A\nB\nC\nD\n";
        let r = apply_edits_to_normalized_content(
            content,
            &[range("A", Some("B"), "X\n"), edit("D", "Z")],
            "f",
        )
        .unwrap();
        assert_eq!(r.new_content, "X\nB\nC\nZ\n");
        assert_eq!(r.replacements, 2);
    }

    #[test]
    fn range_overlapping_another_edit_is_rejected() {
        let content = "A\nB\nC\n";
        let e = apply_edits_to_normalized_content(
            content,
            &[range("A", Some("C"), "x"), edit("B", "y")],
            "f",
        )
        .unwrap_err();
        assert!(e.contains("overlap"), "{e}");
    }

    // ------------------------------------------------------------------
    // Replace-all
    // ------------------------------------------------------------------

    #[test]
    fn all_replaces_every_exact_occurrence() {
        let r = apply_edits_to_normalized_content(
            "foo foo foo\n",
            &[Edit::replace_all("foo", "bar", None)],
            "f",
        )
        .unwrap();
        assert_eq!(r.new_content, "bar bar bar\n");
        assert_eq!(r.replacements, 3);
    }

    #[test]
    fn all_count_guard_mismatch_is_rejected() {
        let e = apply_edits_to_normalized_content(
            "foo foo foo\n",
            &[Edit::replace_all("foo", "bar", Some(2))],
            "f.rs",
        )
        .unwrap_err();
        assert!(e.contains("Expected 2 occurrence(s)"), "{e}");
        assert!(e.contains("found 3"), "{e}");
        let ok = apply_edits_to_normalized_content(
            "foo foo foo\n",
            &[Edit::replace_all("foo", "bar", Some(3))],
            "f",
        )
        .unwrap();
        assert_eq!(ok.new_content, "bar bar bar\n");
    }

    #[test]
    fn all_requires_an_exact_match() {
        // Smart quotes only match through the fuzzy normalizer; replace-all
        // must not apply fuzzy hits.
        let content = "let s = \u{201C}hello\u{201D};\n";
        let e = apply_edits_to_normalized_content(
            content,
            &[Edit::replace_all("let s = \"hello\";", "x", None)],
            "f.rs",
        )
        .unwrap_err();
        assert!(e.contains("requires exact matches"), "{e}");
        assert!(e.contains("Closest candidate"), "{e}");
    }

    #[test]
    fn all_overlapping_occurrences_are_rejected() {
        let e =
            apply_edits_to_normalized_content("aaa", &[Edit::replace_all("aa", "b", None)], "f.rs")
                .unwrap_err();
        assert!(e.contains("occurrences overlap"), "{e}");
    }

    // ------------------------------------------------------------------
    // Failure aggregation
    // ------------------------------------------------------------------

    #[test]
    fn multiple_failed_edits_are_reported_together() {
        let e = apply_edits_to_normalized_content(
            "abc",
            &[edit("nope", "x"), edit("missing", "y")],
            "f.rs",
        )
        .unwrap_err();
        assert!(e.contains("edits[0]:"), "{e}");
        assert!(e.contains("edits[1]:"), "{e}");
    }

    #[test]
    fn multiple_failed_edits_include_duplicates_and_misses() {
        let e = apply_edits_to_normalized_content(
            "dup dup\n",
            &[edit("dup", "x"), edit("absent", "y")],
            "f.rs",
        )
        .unwrap_err();
        assert!(e.contains("edits[0]: Found 2 occurrences"), "{e}");
        assert!(e.contains("edits[1]:"), "{e}");
    }

    #[test]
    fn range_line_count_counts_terminated_lines() {
        assert_eq!(range_line_count(""), 0);
        assert_eq!(range_line_count("a"), 1);
        assert_eq!(range_line_count("a\n"), 1);
        assert_eq!(range_line_count("a\nb"), 2);
        assert_eq!(range_line_count("a\nb\n"), 2);
        assert_eq!(range_line_count("a\n\n"), 2);
    }

    #[test]
    fn fuzzy_offsets_splice_original_bytes() {
        // The fuzzy window's trailing whitespace is part of the replaced
        // range; surrounding lines stay byte-for-byte identical.
        let content = "fn main() {  \n    foo();  \n}\n";
        let r = apply_edits_to_normalized_content(
            content,
            &[edit("fn main() {\n    foo();", "fn main() {\n    bar();")],
            "f",
        )
        .unwrap();
        assert_eq!(r.new_content, "fn main() {\n    bar();\n}\n");
    }
}
