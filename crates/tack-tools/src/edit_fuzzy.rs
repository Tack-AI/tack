//! Fuzzy text matching for the edit tool: Levenshtein similarity, whole-block
//! fuzzy search with an exact-first ladder, uniqueness/dominance rules, and
//! indentation-aware replacement.
//!
//! Ported from oh-my-pi's `crates/pi-edit/src/fuzzy.rs` and the indentation
//! profiler in `crates/pi-edit/src/text.rs` (MIT License, Copyright (c) 2025
//! Mario Zechner, Can Bölük, Stencil Labs, Inc.). The patch-mode-only
//! sequence/context-line search is not ported.
//!
//! Deliberate deviations from the oh-my-pi original:
//! - `normalize_for_fuzzy` additionally folds curly quotes (`‘’“”`) and
//!   Unicode spaces, preserving the tolerance tack's previous matcher had.
//! - NFC normalization is omitted (Rust std has no unicode normalization and
//!   we avoid the extra dependency, matching tack's earlier decision).

/// Default similarity threshold for fuzzy matching.
pub const DEFAULT_FUZZY_THRESHOLD: f64 = 0.95;
/// Fallback threshold for line-based matching without indentation depth.
const FALLBACK_THRESHOLD: f64 = 0.8;
/// Number of surrounding lines in occurrence previews.
const OCCURRENCE_PREVIEW_CONTEXT: usize = 5;
/// Maximum displayed line length in occurrence previews.
const OCCURRENCE_PREVIEW_MAX_LEN: usize = 80;
/// Occurrence previews and indices recorded before truncation.
const MAX_RECORDED_MATCHES: usize = 5;
/// A fuzzy hit at or above this confidence can dominate weaker siblings.
const DOMINANT_FUZZY_MIN_CONFIDENCE: f64 = 0.97;
/// Minimum confidence gap for a dominant fuzzy hit.
const DOMINANT_FUZZY_DELTA: f64 = 0.08;
/// Estimated-cost ceiling for the fuzzy scan: roughly content_lines ×
/// target_lines × average line length. Each candidate window compares
/// every target line with a Levenshtein pass that is quadratic in line
/// length, so an exact-miss against a huge file would otherwise stall
/// the worker for minutes. Past the budget, fuzzy is skipped and the
/// caller reports a plain no-match (the model recovers by quoting
/// exact text). ~2M keeps normal source files fully fuzzy-capable.
const MAX_FUZZY_MATCH_COST: usize = 2_000_000;

// ---------------------------------------------------------------------------
// Text helpers (port of oh-my-pi text.rs)
// ---------------------------------------------------------------------------

/// JavaScript's `\s` / `String.prototype.trim` whitespace set (`WhiteSpace` +
/// `LineTerminator` productions).
const fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `String.prototype.trim` equivalent.
fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// Length in UTF-16 code units — JS `string.length`.
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// True when `line` has any non-whitespace content.
fn is_non_empty_line(line: &str) -> bool {
    !js_trim(line).is_empty()
}

/// Count leading space/tab characters.
fn count_leading_whitespace(line: &str) -> usize {
    line.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}

/// Leading space/tab prefix of `line`.
fn get_leading_whitespace(line: &str) -> &str {
    &line[..count_leading_whitespace(line)]
}

/// First indentation character used in `text` (space when none is found).
fn detect_indent_char(text: &str) -> char {
    text.split('\n')
        .map(get_leading_whitespace)
        .find(|ws| !ws.is_empty())
        .and_then(|ws| ws.chars().next())
        .unwrap_or(' ')
}

/// Normalize a line for fuzzy comparison: trim, fold quotes/dashes to ASCII,
/// collapse runs of spaces and tabs.
///
/// Superset of oh-my-pi's mapping: curly quotes (`‘’“”`) fold to ASCII and
/// Unicode spaces collapse like ASCII spaces, so a model quoting smart-quoted
/// or non-breaking text from rendered output still matches.
fn normalize_for_fuzzy(line: &str) -> String {
    let trimmed = js_trim(line);
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut in_space = false;
    for ch in trimmed.chars() {
        let mapped = match ch {
            '"' | '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{AB}' | '\u{BB}' => '"',
            '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '`' | '\u{B4}' => '\'',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            ' '
            | '\t'
            | '\u{A0}'
            | '\u{2002}'..='\u{200A}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        };
        if mapped == ' ' {
            if in_space {
                continue;
            }
            in_space = true;
        } else {
            in_space = false;
        }
        out.push(mapped);
    }
    out
}

// ---------------------------------------------------------------------------
// Indentation profiling and adjustment (port of oh-my-pi text.rs)
// ---------------------------------------------------------------------------

const fn gcd(left: usize, right: usize) -> usize {
    let (mut high, mut low) = (left, right);
    while low != 0 {
        let remainder = high % low;
        high = low;
        low = remainder;
    }
    high
}

/// Indentation statistics of a text block, used to re-indent replacements.
struct IndentProfile<'a> {
    lines: Vec<&'a str>,
    char: Option<char>,
    space_only: bool,
    tab_only: bool,
    mixed: bool,
    unit: usize,
    non_empty_count: usize,
}

fn build_indent_profile(text: &str) -> IndentProfile<'_> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut indent_counts = Vec::new();
    let mut char: Option<char> = None;
    let mut space_only = true;
    let mut tab_only = true;
    let mut mixed = false;
    let mut non_empty_count = 0;
    let mut unit = 0;

    for line in &lines {
        if !is_non_empty_line(line) {
            continue;
        }
        non_empty_count += 1;
        let indent = get_leading_whitespace(line);
        indent_counts.push(indent.len());
        let has_space = indent.contains(' ');
        let has_tab = indent.contains('\t');
        if has_space {
            tab_only = false;
        }
        if has_tab {
            space_only = false;
        }
        if has_space && has_tab {
            mixed = true;
        }
        if let Some(current) = indent.chars().next() {
            match char {
                None => char = Some(current),
                Some(existing) if existing != current => mixed = true,
                _ => {}
            }
        }
    }

    if space_only && non_empty_count > 0 {
        let mut current = 0;
        for count in indent_counts {
            if count == 0 {
                continue;
            }
            current = if current == 0 {
                count
            } else {
                gcd(current, count)
            };
        }
        unit = current;
    }
    if tab_only && non_empty_count > 0 {
        unit = 1;
    }

    IndentProfile {
        lines,
        char,
        space_only,
        tab_only,
        mixed,
        unit,
        non_empty_count,
    }
}

/// Replace pure-tab leading indentation with `spaces_per_tab` spaces per tab.
fn convert_leading_tabs_to_spaces(text: &str, spaces_per_tab: usize) -> String {
    if spaces_per_tab == 0 {
        return text.to_owned();
    }
    let converted: Vec<String> = text
        .split('\n')
        .map(|line| {
            let trimmed = line.trim_start_matches([' ', '\t']);
            if trimmed.is_empty() {
                return line.to_owned();
            }
            let leading = get_leading_whitespace(line);
            if !leading.contains('\t') || leading.contains(' ') {
                return line.to_owned();
            }
            let mut out = " ".repeat(leading.len() * spaces_per_tab);
            out.push_str(trimmed);
            out
        })
        .collect();
    converted.join("\n")
}

fn is_indentation_only_rewrite(old_text: &str, new_text: &str) -> bool {
    let old_lines: Vec<&str> = old_text.split('\n').collect();
    let new_lines: Vec<&str> = new_text.split('\n').collect();
    old_lines.len() == new_lines.len()
        && old_lines
            .iter()
            .zip(&new_lines)
            .all(|(a, b)| js_trim(a) == js_trim(b))
}

fn maybe_convert_tab_indentation(
    old: &IndentProfile<'_>,
    actual: &IndentProfile<'_>,
    new: &IndentProfile<'_>,
    new_text: &str,
) -> Option<String> {
    if !actual.space_only || !old.tab_only || !new.tab_only || actual.unit == 0 {
        return None;
    }
    for (old_line, actual_line) in old.lines.iter().zip(&actual.lines) {
        if !is_non_empty_line(old_line) || !is_non_empty_line(actual_line) {
            continue;
        }
        let old_indent = get_leading_whitespace(old_line);
        if old_indent.is_empty() {
            continue;
        }
        let actual_indent = get_leading_whitespace(actual_line);
        if actual_indent.len() != old_indent.len() * actual.unit {
            return None;
        }
    }
    Some(convert_leading_tabs_to_spaces(new_text, actual.unit))
}

fn compute_uniform_indent_delta(
    old: &IndentProfile<'_>,
    actual: &IndentProfile<'_>,
) -> Option<isize> {
    let mut delta: Option<isize> = None;
    for (old_line, actual_line) in old.lines.iter().zip(&actual.lines) {
        if !is_non_empty_line(old_line) || !is_non_empty_line(actual_line) {
            continue;
        }
        let current = count_leading_whitespace(actual_line) as isize
            - count_leading_whitespace(old_line) as isize;
        match delta {
            None => delta = Some(current),
            Some(existing) if existing != current => return None,
            _ => {}
        }
    }
    delta
}

fn apply_indent_delta(text: &str, delta: isize, indent_char: char) -> String {
    let adjusted: Vec<String> = text
        .split('\n')
        .map(|line| {
            if !is_non_empty_line(line) {
                return line.to_owned();
            }
            if delta > 0 {
                let mut out = String::with_capacity(line.len() + delta as usize);
                out.extend(std::iter::repeat_n(indent_char, delta as usize));
                out.push_str(line);
                return out;
            }
            let to_remove = ((-delta) as usize).min(count_leading_whitespace(line));
            line[to_remove..].to_owned()
        })
        .collect();
    adjusted.join("\n")
}

/// Re-indent `new_text` by the uniform indentation delta between the authored
/// `old_text` and the `actual_text` that was matched in the file.
///
/// A fuzzy hit at a different nesting depth lands with the right indentation.
pub fn adjust_indentation(old_text: &str, actual_text: &str, new_text: &str) -> String {
    if old_text == actual_text || is_indentation_only_rewrite(old_text, new_text) {
        return new_text.to_owned();
    }
    let old = build_indent_profile(old_text);
    let actual = build_indent_profile(actual_text);
    let new = build_indent_profile(new_text);

    if old.non_empty_count == 0 || actual.non_empty_count == 0 || new.non_empty_count == 0 {
        return new_text.to_owned();
    }
    if old.mixed || actual.mixed || new.mixed {
        return new_text.to_owned();
    }
    if let (Some(o), Some(a)) = (old.char, actual.char)
        && o != a
    {
        return maybe_convert_tab_indentation(&old, &actual, &new, new_text)
            .unwrap_or_else(|| new_text.to_owned());
    }
    let Some(delta) = compute_uniform_indent_delta(&old, &actual) else {
        return new_text.to_owned();
    };
    if delta == 0 {
        return new_text.to_owned();
    }
    if let (Some(n), Some(a)) = (new.char, actual.char)
        && n != a
    {
        return new_text.to_owned();
    }
    let indent_char = actual
        .char
        .or(old.char)
        .unwrap_or_else(|| detect_indent_char(actual_text));
    apply_indent_delta(new_text, delta, indent_char)
}

// ---------------------------------------------------------------------------
// Similarity primitives (port of oh-my-pi fuzzy.rs)
// ---------------------------------------------------------------------------

fn levenshtein_chars(a: &[char], b: &[char]) -> usize {
    if a == b {
        return 0;
    }
    let mut start = 0;
    let shared_limit = a.len().min(b.len());
    while start < shared_limit && a[start] == b[start] {
        start += 1;
    }
    let mut a_end = a.len();
    let mut b_end = b.len();
    while a_end > start && b_end > start && a[a_end - 1] == b[b_end - 1] {
        a_end -= 1;
        b_end -= 1;
    }
    let mut longer = &a[start..a_end];
    let mut shorter = &b[start..b_end];
    if longer.is_empty() {
        return shorter.len();
    }
    if shorter.is_empty() {
        return longer.len();
    }
    if shorter.len() > longer.len() {
        std::mem::swap(&mut longer, &mut shorter);
    }

    let mut row: Vec<usize> = (0..=shorter.len()).collect();
    for (line, &a_char) in longer.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = line + 1;
        for (column, &b_char) in shorter.iter().enumerate() {
            let cell = column + 1;
            let above = row[cell];
            row[cell] = if a_char == b_char {
                diagonal
            } else {
                (above + 1).min(row[cell - 1] + 1).min(diagonal + 1)
            };
            diagonal = above;
        }
    }
    row[shorter.len()]
}

/// Levenshtein edit distance over Unicode scalar values.
///
/// The TypeScript source used UTF-16 code units. Rust deliberately uses
/// Unicode scalar values, so astral characters count as one element.
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    levenshtein_chars(&a_chars, &b_chars)
}

/// Similarity in `[0, 1]`: `1 - distance / max_len`.
fn similarity(a: &str, b: &str) -> f64 {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let max_len = a_chars.len().max(b_chars.len());
    if max_len == 0 {
        return 1.0;
    }
    1.0 - levenshtein_chars(&a_chars, &b_chars) as f64 / max_len as f64
}

// ---------------------------------------------------------------------------
// Match location (port of oh-my-pi fuzzy.rs)
// ---------------------------------------------------------------------------

/// A located block of text.
#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyMatch {
    pub actual_text: String,
    /// Byte offset of the match start in the searched content.
    pub start_index: usize,
    /// 1-indexed line of the match start.
    pub start_line: u32,
    pub confidence: f64,
}

/// Outcome of [`find_match`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MatchOutcome {
    pub matched: Option<FuzzyMatch>,
    pub closest: Option<FuzzyMatch>,
    pub occurrences: Option<usize>,
    pub occurrence_lines: Option<Vec<u32>>,
    pub occurrence_previews: Option<Vec<String>>,
    pub fuzzy_matches: Option<usize>,
    pub dominant_fuzzy: Option<bool>,
    /// True when fuzzy matching was skipped because the estimated scan
    /// cost exceeded `MAX_FUZZY_MATCH_COST`: no exact match existed
    /// and no fuzzy attempt was made.
    pub fuzzy_skipped: Option<bool>,
}

/// Knobs for [`find_match`].
#[derive(Debug, Clone, Default)]
pub struct FindMatchOptions {
    pub allow_fuzzy: bool,
    /// Defaults to [`DEFAULT_FUZZY_THRESHOLD`].
    pub threshold: Option<f64>,
}

fn format_preview_window(lines: &[&str], center_index: usize) -> String {
    let start = center_index.saturating_sub(OCCURRENCE_PREVIEW_CONTEXT);
    let end = lines
        .len()
        .min(center_index + OCCURRENCE_PREVIEW_CONTEXT + 1);
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let truncated = if utf16_len(line) > OCCURRENCE_PREVIEW_MAX_LEN {
                let mut units = 0;
                let mut text = String::new();
                for ch in line.chars() {
                    let width = ch.len_utf16();
                    if units + width > OCCURRENCE_PREVIEW_MAX_LEN - 1 {
                        break;
                    }
                    text.push(ch);
                    units += width;
                }
                text.push('…');
                text
            } else {
                (*line).to_owned()
            };
            format!("  {} | {truncated}", start + offset + 1)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn find_exact_match_outcome(content: &str, target: &str) -> Option<MatchOutcome> {
    let mut first_index = None;
    let mut occurrences = 0;
    let mut recorded_indices = Vec::new();
    let mut search_start = 0;
    while search_start <= content.len().saturating_sub(target.len()) {
        let Some(relative) = content[search_start..].find(target) else {
            break;
        };
        let index = search_start + relative;
        first_index.get_or_insert(index);
        occurrences += 1;
        if recorded_indices.len() < MAX_RECORDED_MATCHES {
            recorded_indices.push(index);
        }
        // Step one CHARACTER, not the needle length: overlapping
        // occurrences must count too ("aa" occurs twice in "aaa"), per
        // the tool contract that oldText "must match a unique region".
        // A match start is a char boundary, so the next candidate start
        // is the next char boundary after `index`.
        search_start = index + 1;
        while search_start < content.len() && !content.is_char_boundary(search_start) {
            search_start += 1;
        }
    }
    let first_index = first_index?;
    if occurrences > 1 {
        let content_lines: Vec<&str> = content.split('\n').collect();
        let mut occurrence_lines = Vec::with_capacity(recorded_indices.len());
        let mut occurrence_previews = Vec::with_capacity(recorded_indices.len());
        for index in recorded_indices {
            let line_number = content[..index]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            occurrence_lines.push(line_number as u32);
            occurrence_previews.push(format_preview_window(&content_lines, line_number - 1));
        }
        return Some(MatchOutcome {
            occurrences: Some(occurrences),
            occurrence_lines: Some(occurrence_lines),
            occurrence_previews: Some(occurrence_previews),
            ..MatchOutcome::default()
        });
    }
    let start_line = content[..first_index]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32
        + 1;
    Some(MatchOutcome {
        matched: Some(FuzzyMatch {
            actual_text: target.to_owned(),
            start_index: first_index,
            start_line,
            confidence: 1.0,
        }),
        ..MatchOutcome::default()
    })
}

fn relative_indent_depths(lines: &[&str]) -> Vec<usize> {
    let indents: Vec<usize> = lines
        .iter()
        .map(|line| count_leading_whitespace(line))
        .collect();
    let non_empty_indents: Vec<usize> = lines
        .iter()
        .zip(&indents)
        .filter_map(|(line, indent)| is_non_empty_line(line).then_some(*indent))
        .collect();
    let min_indent = non_empty_indents.iter().copied().min().unwrap_or(0);
    let indent_unit = non_empty_indents
        .iter()
        .filter_map(|indent| indent.checked_sub(min_indent))
        .filter(|step| *step > 0)
        .min()
        .unwrap_or(1);
    lines
        .iter()
        .zip(indents)
        .map(|(line, indent)| {
            if !is_non_empty_line(line) || indent_unit == 0 {
                0
            } else {
                ((indent - min_indent) as f64 / indent_unit as f64).round() as usize
            }
        })
        .collect()
}

fn normalize_lines(lines: &[&str], include_depth: bool) -> Vec<String> {
    let depths = include_depth.then(|| relative_indent_depths(lines));
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let trimmed = js_trim(line);
            let prefix = depths
                .as_ref()
                .map_or_else(|| "|".to_owned(), |values| format!("{}|", values[index]));
            if trimmed.is_empty() {
                prefix
            } else {
                prefix + &normalize_for_fuzzy(trimmed)
            }
        })
        .collect()
}

fn line_offsets(lines: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for (index, line) in lines.iter().enumerate() {
        offsets.push(offset);
        offset += line.len() + usize::from(index + 1 < lines.len());
    }
    offsets
}

#[derive(Debug)]
struct BestFuzzyMatch {
    best: Option<FuzzyMatch>,
    above_threshold_count: usize,
    second_best_score: f64,
}

fn best_fuzzy_match_core(
    content_lines: &[&str],
    target_lines: &[&str],
    offsets: &[usize],
    threshold: f64,
    include_depth: bool,
) -> BestFuzzyMatch {
    let target_normalized = normalize_lines(target_lines, include_depth);
    let mut best = None;
    let mut best_score = -1.0;
    let mut second_best_score = -1.0;
    let mut above_threshold_count = 0;
    for start in 0..=content_lines.len() - target_lines.len() {
        let start_index = offsets[start];
        let window = &content_lines[start..start + target_lines.len()];
        let window_normalized = normalize_lines(window, include_depth);
        let score = target_normalized
            .iter()
            .zip(&window_normalized)
            .map(|(target, actual)| {
                // Identical normalized lines score 1.0; skip the
                // Levenshtein allocation+DP for the common case of a
                // near-identical window (result-identical fast path).
                if target == actual {
                    1.0
                } else {
                    similarity(target, actual)
                }
            })
            .sum::<f64>()
            / target_lines.len() as f64;
        if score >= threshold {
            above_threshold_count += 1;
        }
        if score > best_score {
            second_best_score = best_score;
            best_score = score;
            best = Some(FuzzyMatch {
                actual_text: window.join("\n"),
                start_index,
                start_line: start as u32 + 1,
                confidence: score,
            });
        } else if score > second_best_score {
            second_best_score = score;
        }
    }
    BestFuzzyMatch {
        best,
        above_threshold_count,
        second_best_score,
    }
}

/// Cheap estimate of the fuzzy scan cost: content_lines × target_lines
/// × average content line length. Each candidate window runs a
/// per-line-pair Levenshtein (quadratic in line length), so long lines
/// must count extra; counting newline bytes keeps this O(n) with a
/// memcmp-speed scan.
fn fuzzy_match_cost_estimate(content: &str, target: &str) -> usize {
    let content_lines = content.bytes().filter(|b| *b == b'\n').count() + 1;
    let target_lines = target.bytes().filter(|b| *b == b'\n').count() + 1;
    let avg_line_len = (content.len() / content_lines).max(1);
    content_lines
        .saturating_mul(target_lines)
        .saturating_mul(avg_line_len)
}

fn best_fuzzy_match(content: &str, target: &str, threshold: f64) -> BestFuzzyMatch {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let target_lines: Vec<&str> = target.split('\n').collect();
    if target.is_empty() || target_lines.len() > content_lines.len() {
        return BestFuzzyMatch {
            best: None,
            above_threshold_count: 0,
            second_best_score: 0.0,
        };
    }
    let offsets = line_offsets(&content_lines);
    let mut result =
        best_fuzzy_match_core(&content_lines, &target_lines, &offsets, threshold, true);
    if result
        .best
        .as_ref()
        .is_some_and(|best| best.confidence < threshold && best.confidence >= FALLBACK_THRESHOLD)
    {
        let without_depth =
            best_fuzzy_match_core(&content_lines, &target_lines, &offsets, threshold, false);
        if without_depth.best.as_ref().is_some_and(|candidate| {
            result
                .best
                .as_ref()
                .is_none_or(|best| candidate.confidence > best.confidence)
        }) {
            result = without_depth;
        }
    }
    result
}

/// Locate `target` in `content`: exact first, then fuzzy when allowed.
///
/// The returned match's byte offsets address the original `content`, so
/// replacements splice at exact authored byte boundaries regardless of the
/// normalization used for scoring.
pub fn find_match(content: &str, target: &str, options: &FindMatchOptions) -> MatchOutcome {
    if target.is_empty() {
        return MatchOutcome::default();
    }
    if let Some(exact) = find_exact_match_outcome(content, target) {
        return exact;
    }
    if fuzzy_match_cost_estimate(content, target) > MAX_FUZZY_MATCH_COST {
        // Exact matching failed and the fuzzy scan is too expensive for
        // this input: return a plain no-match so the caller's error can
        // tell the model to quote exact text instead of burning minutes
        // on a blocking Levenshtein sweep.
        return MatchOutcome {
            fuzzy_skipped: Some(true),
            ..MatchOutcome::default()
        };
    }
    let threshold = options.threshold.unwrap_or(DEFAULT_FUZZY_THRESHOLD);
    let result = best_fuzzy_match(content, target, threshold);
    let Some(best) = result.best else {
        return MatchOutcome::default();
    };
    if options.allow_fuzzy && best.confidence >= threshold {
        if result.above_threshold_count == 1 {
            return MatchOutcome {
                matched: Some(best.clone()),
                closest: Some(best),
                ..MatchOutcome::default()
            };
        }
        if result.above_threshold_count > 1
            && best.confidence >= DOMINANT_FUZZY_MIN_CONFIDENCE
            && best.confidence - result.second_best_score >= DOMINANT_FUZZY_DELTA
        {
            return MatchOutcome {
                matched: Some(best.clone()),
                closest: Some(best),
                fuzzy_matches: Some(result.above_threshold_count),
                dominant_fuzzy: Some(true),
                ..MatchOutcome::default()
            };
        }
    }
    MatchOutcome {
        closest: Some(best),
        fuzzy_matches: Some(result.above_threshold_count),
        ..MatchOutcome::default()
    }
}

// ---------------------------------------------------------------------------
// Error formatting (port of oh-my-pi fuzzy.rs)
// ---------------------------------------------------------------------------

fn first_different_line<'a>(old_lines: &'a [&str], new_lines: &'a [&str]) -> (&'a str, &'a str) {
    for index in 0..old_lines.len().max(new_lines.len()) {
        let old = old_lines.get(index).copied().unwrap_or("");
        let new = new_lines.get(index).copied().unwrap_or("");
        if old != new {
            return (old, new);
        }
    }
    (
        old_lines.first().copied().unwrap_or(""),
        new_lines.first().copied().unwrap_or(""),
    )
}

/// Format a match failure with the closest candidate and an actionable hint.
pub fn format_match_error(
    path: &str,
    search_text: &str,
    closest: Option<&FuzzyMatch>,
    threshold: f64,
    fuzzy_matches: Option<usize>,
) -> String {
    let Some(closest) = closest else {
        return format!("Could not find a close enough match in {path}.");
    };
    let similarity_percent = (closest.confidence * 100.0).round() as i64;
    let threshold_percent = (threshold * 100.0).round() as i64;
    let search_lines: Vec<&str> = search_text.split('\n').collect();
    let actual_lines: Vec<&str> = closest.actual_text.split('\n').collect();
    let (old_line, new_line) = first_different_line(&search_lines, &actual_lines);
    let hint = if fuzzy_matches.is_some_and(|count| count > 1) {
        format!(
            "Found {} high-confidence matches. Provide more context to make it unique.",
            fuzzy_matches.unwrap_or(0)
        )
    } else {
        format!("Closest match was below the {threshold_percent}% similarity threshold.")
    };
    format!(
        "Could not find a close enough match in {path}.\n\nClosest match ({similarity_percent}% similar) at line {}:\n  - {old_line}\n  + {new_line}\n{hint}",
        closest.start_line
    )
}

/// Format an ambiguity failure with per-occurrence context previews.
pub fn format_occurrence_error(path: &str, outcome: &MatchOutcome) -> String {
    let occurrences = outcome.occurrences.unwrap_or(0);
    let previews = outcome
        .occurrence_previews
        .as_ref()
        .map_or_else(String::new, |items| items.join("\n\n"));
    let more = if occurrences > MAX_RECORDED_MATCHES {
        format!(" (showing first {MAX_RECORDED_MATCHES} of {occurrences})")
    } else {
        String::new()
    };
    format!(
        "Found {occurrences} occurrences in {path}{more}:\n\n{previews}\n\nAdd more context lines to disambiguate."
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn options(allow_fuzzy: bool) -> FindMatchOptions {
        FindMatchOptions {
            allow_fuzzy,
            threshold: None,
        }
    }

    #[test]
    fn exact_match_and_multiple_occurrences() {
        let found = find_match("line1\nline2\nline3", "line2", &options(false));
        assert_eq!(
            found.matched.as_ref().map(|matched| matched.start_line),
            Some(2)
        );
        assert_eq!(
            found.matched.as_ref().map(|matched| matched.confidence),
            Some(1.0)
        );

        let multiple = find_match("foo\nbar\nfoo", "foo", &options(false));
        assert!(multiple.matched.is_none());
        assert_eq!(multiple.occurrences, Some(2));
        assert_eq!(multiple.occurrence_lines, Some(vec![1, 3]));
        assert_eq!(multiple.occurrence_previews.as_ref().map(Vec::len), Some(2));
    }

    /// Overlapping occurrences count toward uniqueness: "aa" appears
    /// twice in "aaa", so it must NOT be accepted as a unique match
    /// (previously the scan stepped by the needle length and counted 1).
    #[test]
    fn overlapping_exact_occurrences_are_not_unique() {
        let outcome = find_match("aaa", "aa", &options(false));
        assert!(outcome.matched.is_none());
        assert_eq!(outcome.occurrences, Some(2));
        assert_eq!(outcome.occurrence_lines, Some(vec![1, 1]));

        // Multi-byte needle: stepping one char (not one byte) must not
        // panic and must find the overlapping second occurrence.
        let outcome = find_match("\u{3B1}\u{3B1}\u{3B1}", "\u{3B1}\u{3B1}", &options(false));
        assert!(outcome.matched.is_none());
        assert_eq!(outcome.occurrences, Some(2));

        // Non-overlapping repeats are still counted as before.
        let outcome = find_match("ab\nab", "ab", &options(false));
        assert_eq!(outcome.occurrences, Some(2));
        assert_eq!(outcome.occurrence_lines, Some(vec![1, 2]));

        // A genuinely unique match still succeeds.
        let outcome = find_match("aab", "aa", &options(false));
        assert!(outcome.matched.is_some());
    }

    /// Oversized inputs skip the fuzzy sweep entirely (complexity circuit
    /// breaker): no exact match → plain no-match outcome, flagged so the
    /// caller can tell the model to quote exact text.
    #[test]
    fn oversized_input_skips_fuzzy() {
        // 5000 lines × ~200 chars × 3 target lines ≈ 3M > MAX_FUZZY_MATCH_COST.
        let line = "x".repeat(200);
        let content = std::iter::repeat_n(line.as_str(), 5000)
            .collect::<Vec<_>>()
            .join("\n");
        let target = format!(
            "{}\n{}\n{}",
            "y".repeat(200),
            "x".repeat(200),
            "x".repeat(200)
        );
        let outcome = find_match(&content, &target, &options(true));
        assert!(outcome.matched.is_none());
        assert!(outcome.closest.is_none());
        assert_eq!(outcome.fuzzy_skipped, Some(true));

        // Exact matches still work at any size (no fuzzy needed).
        let mut lines = vec!["x".repeat(200); 5000];
        lines[2500] = "unique needle line here".to_string();
        let content = lines.join("\n");
        let outcome = find_match(&content, "unique needle line here", &options(true));
        assert!(outcome.matched.is_some());
        assert!(outcome.fuzzy_skipped.is_none());
    }

    /// Under the cost ceiling, fuzzy behavior (including the identical-line
    /// fast path) is unchanged.
    #[test]
    fn normal_fuzzy_unchanged_below_cost_ceiling() {
        let content = (0..200)
            .map(|i| {
                if i == 100 {
                    "    let value = compute_thng();".to_string()
                } else {
                    format!("fn filler_{i}() {{}}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = find_match(&content, "  let value = compute_thing();", &options(true));
        let matched = outcome.matched.expect("fuzzy match expected");
        assert_eq!(matched.start_line, 101);
        assert!(matched.confidence >= DEFAULT_FUZZY_THRESHOLD);
        assert!(outcome.fuzzy_skipped.is_none());
    }

    #[test]
    fn tab_space_and_internal_whitespace_normalization() {
        for (content, target) in [
            ("\tfoo\n\t\tbar\n\tbaz", "  foo\n    bar\n  baz"),
            ("  foo\n    bar\n  baz", "\tfoo\n\t\tbar\n\tbaz"),
            ("   foo\n      bar\n   baz", "  foo\n    bar\n  baz"),
            ("foo   bar    baz", "foo bar baz"),
        ] {
            let outcome = find_match(content, target, &options(true));
            assert!(outcome.matched.is_some(), "failed to match {target:?}");
            assert!(outcome.matched.unwrap().confidence >= DEFAULT_FUZZY_THRESHOLD);
        }
    }

    #[test]
    fn fallback_ignores_inconsistent_indentation() {
        let outcome = find_match(
            "\t\t\tline1\n\t\t\tline2\n\t\tline3\n\t\t\tline4",
            "      line1\n      line2\n      line3\n      line4",
            &options(true),
        );
        assert!(outcome.matched.is_some());

        let varied = find_match(
            "  a\n    b\n   c\n    d",
            "  a\n    b\n    c\n    d",
            &options(true),
        );
        assert!(varied.matched.is_some());
    }

    #[test]
    fn single_line_trailing_space_and_empty_line_cases() {
        let single = find_match(
            "prefix\n\t\t\t\"value\",\nsuffix",
            "          \"value\",",
            &options(true),
        );
        assert!(single.matched.is_some());
        let trailing = find_match("line1  \nline2\t", "line1\nline2", &options(true));
        assert!(trailing.matched.is_some());
        let empty_line = find_match("line1\n\nline3", "line1\n\nline3", &options(false));
        assert_eq!(
            empty_line
                .matched
                .as_ref()
                .map(|matched| matched.confidence),
            Some(1.0)
        );
        assert_eq!(
            find_match("some content", "", &options(true)),
            MatchOutcome::default()
        );
        assert!(
            find_match(
                "short",
                "this is much longer than the content",
                &options(true)
            )
            .matched
            .is_none()
        );
    }

    #[test]
    fn smart_quotes_and_dashes_match() {
        // tack extension over oh-my-pi: curly quotes fold to ASCII.
        let outcome = find_match(
            "let s = \u{201C}hello\u{201D};",
            "let s = \"hello\";",
            &options(true),
        );
        assert!(outcome.matched.is_some());
        let dash = find_match("a \u{2014} b", "a - b", &options(true));
        assert!(dash.matched.is_some());
    }

    #[test]
    fn threshold_and_dominant_fuzzy_match() {
        let strict = FindMatchOptions {
            allow_fuzzy: true,
            threshold: Some(0.99),
        };
        assert!(
            find_match("function foo() {}", "function bar() {}", &strict)
                .matched
                .is_none()
        );
        let lenient = FindMatchOptions {
            allow_fuzzy: true,
            threshold: Some(0.7),
        };
        assert!(
            find_match("function foo() {}", "function bar() {}", &lenient)
                .matched
                .is_some()
        );

        let target = "a".repeat(50);
        let content = format!("{}b\n{}cccccc", "a".repeat(49), "a".repeat(44));
        let dominant = find_match(
            &content,
            &target,
            &FindMatchOptions {
                allow_fuzzy: true,
                threshold: Some(0.8),
            },
        );
        assert_eq!(dominant.dominant_fuzzy, Some(true));
        assert_eq!(dominant.fuzzy_matches, Some(2));
    }

    #[test]
    fn fuzzy_match_offsets_address_original_bytes() {
        // The fuzzy hit must splice into the original content, trailing
        // whitespace included.
        let content = "fn main() {  \n    foo();\n}\n";
        let outcome = find_match(content, "fn main() {\n    foo();", &options(true));
        let matched = outcome.matched.expect("fuzzy match");
        assert_eq!(matched.actual_text, "fn main() {  \n    foo();");
        assert_eq!(
            &content[matched.start_index..matched.start_index + matched.actual_text.len()],
            matched.actual_text
        );
    }

    #[test]
    fn match_error_formatting() {
        let closest = FuzzyMatch {
            actual_text: "alpha\ngamma".to_owned(),
            start_index: 10,
            start_line: 4,
            confidence: 0.874,
        };
        assert_eq!(
            format_match_error("src/a.ts", "alpha\nbeta", Some(&closest), 0.95, None),
            "Could not find a close enough match in src/a.ts.\n\nClosest match (87% similar) at line \
             4:\n  - beta\n  + gamma\nClosest match was below the 95% similarity threshold."
        );
        assert_eq!(
            format_match_error("src/a.ts", "alpha\nbeta", Some(&closest), 0.95, Some(3)),
            "Could not find a close enough match in src/a.ts.\n\nClosest match (87% similar) at line \
             4:\n  - beta\n  + gamma\nFound 3 high-confidence matches. Provide more context to make \
             it unique."
        );
        assert_eq!(
            format_match_error("src/a.ts", "x", None, 0.95, None),
            "Could not find a close enough match in src/a.ts."
        );
    }

    #[test]
    fn occurrence_error_includes_preview_limit_suffix() {
        let outcome = MatchOutcome {
            occurrences: Some(7),
            occurrence_previews: Some(vec!["preview".to_owned()]),
            ..MatchOutcome::default()
        };
        assert_eq!(
            format_occurrence_error("a.ts", &outcome),
            "Found 7 occurrences in a.ts (showing first 5 of 7):\n\npreview\n\nAdd more context \
             lines to disambiguate."
        );
    }

    #[test]
    fn adjust_indentation_shifts_uniformly() {
        assert_eq!(
            adjust_indentation("foo()\nbar()", "    foo()\n    bar()", "baz()\nqux()"),
            "    baz()\n    qux()"
        );
        assert_eq!(
            adjust_indentation("    foo()", "  foo()", "    bar()"),
            "  bar()"
        );
        assert_eq!(adjust_indentation("foo()", "foo()", "  bar()"), "  bar()");
    }

    #[test]
    fn adjust_indentation_converts_tabs_to_spaces() {
        assert_eq!(
            adjust_indentation("\tfoo()", "    foo()", "\tbar()\n\t\tbaz()"),
            "    bar()\n        baz()"
        );
    }
}
