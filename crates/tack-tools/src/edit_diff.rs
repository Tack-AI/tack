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
    DEFAULT_FUZZY_THRESHOLD, FindMatchOptions, adjust_indentation, find_match, format_match_error,
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

#[derive(Clone, Debug)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug)]
pub struct AppliedEdits {
    pub base_content: String,
    pub new_content: String,
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

/// Apply replacements computed against `base_content` (a normalized view) to
/// Apply edits to LF-normalized content. All edits match against the same
/// original; replacements apply in reverse offset order. Matching uses the
/// exact-first fuzzy ladder from [`crate::edit_fuzzy`]; match offsets address
/// the original content directly, so unchanged text is preserved
/// byte-for-byte and fuzzy replacements are re-indented to the matched depth.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEdits, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|e| Edit {
            old_text: normalize_to_lf(&e.old_text),
            new_text: normalize_to_lf(&e.new_text),
        })
        .collect();

    for (i, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(if normalized_edits.len() == 1 {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{i}].oldText must not be empty in {path}.")
            });
        }
    }

    let single = normalized_edits.len() == 1;
    let mut matched: Vec<MatchedEdit> = Vec::new();
    for (i, edit) in normalized_edits.iter().enumerate() {
        let outcome = find_match(
            normalized_content,
            &edit.old_text,
            &FindMatchOptions {
                allow_fuzzy: true,
                threshold: Some(DEFAULT_FUZZY_THRESHOLD),
            },
        );
        if outcome.occurrences.is_some_and(|count| count > 1) {
            let detail = format_occurrence_error(path, &outcome);
            return Err(if single {
                detail
            } else {
                format!("edits[{i}]: {detail}")
            });
        }
        let Some(found) = outcome.matched else {
            let detail = if outcome.fuzzy_skipped == Some(true) {
                format!(
                    "Could not find a close enough match in {path}. The file is too large for fuzzy matching; oldText must match the file exactly (re-read the exact text and retry)."
                )
            } else {
                format_match_error(
                    path,
                    &edit.old_text,
                    outcome.closest.as_ref(),
                    DEFAULT_FUZZY_THRESHOLD,
                    outcome.fuzzy_matches,
                )
            };
            return Err(if single {
                detail
            } else {
                format!("edits[{i}]: {detail}")
            });
        };
        // A fuzzy hit at a different nesting depth carries its own
        // indentation; re-indent the replacement to match.
        let adjusted = adjust_indentation(&edit.old_text, &found.actual_text, &edit.new_text);
        matched.push(MatchedEdit {
            edit_index: i,
            match_index: found.start_index,
            match_length: found.actual_text.len(),
            new_text: adjusted,
        });
    }

    matched.sort_by_key(|m| m.match_index);
    for pair in matched.windows(2) {
        let (previous, current) = (&pair[0], &pair[1]);
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let new_content = apply_replacements(normalized_content, &matched, 0);

    if normalized_content == new_content {
        return Err(if normalized_edits.len() == 1 {
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
    })
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
        Edit {
            old_text: old.to_string(),
            new_text: new.to_string(),
        }
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
