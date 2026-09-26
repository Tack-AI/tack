//! Syntax highlighting for markdown code blocks via syntect (port of
//! `utils/syntax-highlight.ts` — syntect styles map to our fg colors).
//!
//! Two content-keyed caches sit behind [`highlight_block`]:
//!
//! - **Memo** (exact renders): a bounded LRU keyed by the full code
//!   fingerprint. Highlight output is width-independent, so terminal
//!   resizes and scroll-back re-renders of evicted transcript entries hit
//!   this instead of re-paying the full syntect cost.
//! - **Growing blocks** (incremental): a streaming code fence only grows
//!   by appending, so its already-highlighted complete lines are retained
//!   together with syntect's `HighlightState`/`ParseState`; each re-render
//!   highlights only newly completed lines and the (ephemeral) partial
//!   last line. Output is identical to a fresh full highlight — the same
//!   state machine sees the same line sequence.
//!
//! Both caches validate by content (a 64-bit fingerprint for the memo,
//! an exact stored-text prefix match for growing blocks) and fall back
//! to a full highlight on any mismatch, so a wrong key can only cost
//! performance, never correctness.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::sync::{Mutex, OnceLock};

use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, HighlightState, ThemeSet};
use syntect::parsing::{ParseState, SyntaxReference, SyntaxSet};

use crate::line::Span;
use crate::style::{Color, Style};

/// Exact-render memo capacity. Entries are a few KB to a few hundred KB
/// (one span vec per code line); count alone is not enough — a single
/// 50k-line minified blob would be many MB — so bytes are capped too.
const MEMO_CAP: usize = 32;
const MEMO_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Concurrently tracked growing blocks. One streaming message has one open
/// fence; 4 leaves room for a preview/dialog streaming at the same time.
const GROWING_CAP: usize = 4;
const GROWING_MAX_BYTES: usize = 8 * 1024 * 1024;

fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static syntect::highlighting::Theme {
    static THEME: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
    THEME.get_or_init(|| ThemeSet::load_defaults().themes["base16-ocean.dark"].clone())
}

fn to_style(style: syntect::highlighting::Style) -> Style {
    let mut out = Style::new().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.bold();
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.italic();
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.underline();
    }
    out
}

/// Highlight one code line into spans. Falls back to plain text for unknown
/// languages.
///
/// Stateless per call (a fresh highlighter per line breaks multi-line
/// constructs); callers highlighting more than one line should use
/// [`highlight_block`].
pub fn highlight_line(line: &str, lang: &str) -> Vec<Span> {
    if lang.is_empty() {
        return vec![Span::plain(line.to_string())];
    }
    let set = syntax_set();
    let Some(syntax) = set.find_syntax_by_token(lang) else {
        return vec![Span::plain(line.to_string())];
    };
    let mut highlighter = HighlightLines::new(syntax, theme());
    match highlighter.highlight_line(line, set) {
        Ok(regions) => regions
            .into_iter()
            .map(|(style, text)| Span::styled(text.to_string(), to_style(style)))
            .collect(),
        Err(_) => vec![Span::plain(line.to_string())],
    }
}

/// Highlight a whole code block (one highlighter across lines — multi-line
/// comments/strings highlight correctly). Memoized + incremental (module
/// docs); repeated calls with identical or append-only content are cheap.
pub fn highlight_block(code: &str, lang: &str) -> Vec<Vec<Span>> {
    let caches = CACHES.get_or_init(|| Mutex::new(HighlightCaches::default()));
    match caches.lock() {
        Ok(mut caches) => caches.highlight_block(code, lang),
        // Poisoned: a panic mid-update may have left inconsistent state —
        // bypass the caches rather than trust them.
        Err(_) => highlight_block_uncached(code, lang),
    }
}

/// [`highlight_block`] without the memo/incremental caches: benchmarks
/// measure the highlighter itself, and tests get a ground-truth reference.
pub fn highlight_block_uncached(code: &str, lang: &str) -> Vec<Vec<Span>> {
    if lang.is_empty() {
        return plain_lines(code);
    }
    let set = syntax_set();
    let Some(syntax) = set.find_syntax_by_token(lang) else {
        return plain_lines(code);
    };
    let mut highlighter = HighlightLines::new(syntax, theme());
    code.lines()
        .map(|line| highlight_one(&mut highlighter, line, set))
        .collect()
}

fn plain_lines(code: &str) -> Vec<Vec<Span>> {
    code.lines()
        .map(|l| vec![Span::plain(l.to_string())])
        .collect()
}

fn highlight_one(highlighter: &mut HighlightLines, line: &str, set: &SyntaxSet) -> Vec<Span> {
    match highlighter.highlight_line(line, set) {
        Ok(regions) => regions
            .into_iter()
            .map(|(style, text)| Span::styled(text.to_string(), to_style(style)))
            .collect(),
        Err(_) => vec![Span::plain(line.to_string())],
    }
}

fn fingerprint(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(text.as_bytes());
    h.finish()
}

/// (code hash, lang hash, code len): exact-render memo key.
type MemoKey = (u64, u64, usize);

fn memo_key(code: &str, lang: &str) -> MemoKey {
    (fingerprint(code), fingerprint(lang), code.len())
}

/// Identity of a growing block: lang + first line. Two different blocks
/// sharing both collide onto one entry — the prefix check then fails and
/// forces a (correct, just slower) fresh highlight.
fn growing_key(code: &str, lang: &str) -> u64 {
    let first_line = code.lines().next().unwrap_or("");
    let mut h = DefaultHasher::new();
    h.write(lang.as_bytes());
    h.write(b"\0");
    h.write(first_line.as_bytes());
    h.finish()
}

/// Rough retained-bytes estimate of rendered spans: text plus per-span
/// and per-line struct overhead (Arc header, Style, Vec). Used for cache
/// budgets, not accounting — an underestimate is fine, unbounded is not.
fn spans_bytes(spans: &[Vec<Span>]) -> usize {
    spans
        .iter()
        .map(|line| line.iter().map(|span| span.text.len() + 64).sum::<usize>() + 24)
        .sum()
}

/// A code block seen mid-stream: the complete lines highlighted so far,
/// their spans, and the syntect state positioned right after them.
/// `text` always ends at a line boundary (each retained line including
/// its '\n'), so `code.starts_with(text)` certifies append-only growth.
#[derive(Debug)]
struct GrowingEntry {
    key: u64,
    lang: String,
    text: String,
    spans: Vec<Vec<Span>>,
    highlight_state: HighlightState,
    parse_state: ParseState,
    memo_key: Option<MemoKey>,
    epoch: u64,
    /// text.len() + spans_bytes(spans), maintained incrementally.
    bytes: usize,
}

/// Split into (complete lines incl. newlines, partial last line). The
/// partial line is highlighted ephemerally (never retained): the next
/// delta may extend it, and re-highlighting an extended line from a
/// post-line state would not match a fresh render.
fn split_complete(code: &str) -> (&str, &str) {
    match code.rfind('\n') {
        Some(i) => code.split_at(i + 1),
        None => ("", code),
    }
}

#[derive(Debug)]
struct HighlightCaches {
    memo: HashMap<MemoKey, (u64, Vec<Vec<Span>>)>,
    memo_bytes: usize,
    growing: Vec<GrowingEntry>,
    growing_bytes: usize,
    epoch: u64,
    memo_cap: usize,
    memo_max_bytes: usize,
    growing_cap: usize,
    growing_max_bytes: usize,
}

impl Default for HighlightCaches {
    fn default() -> Self {
        HighlightCaches {
            memo: HashMap::new(),
            memo_bytes: 0,
            growing: Vec::new(),
            growing_bytes: 0,
            epoch: 0,
            memo_cap: MEMO_CAP,
            memo_max_bytes: MEMO_MAX_BYTES,
            growing_cap: GROWING_CAP,
            growing_max_bytes: GROWING_MAX_BYTES,
        }
    }
}

impl HighlightCaches {
    #[cfg(test)]
    fn with_limits(
        memo_cap: usize,
        memo_max_bytes: usize,
        growing_cap: usize,
        growing_max_bytes: usize,
    ) -> Self {
        HighlightCaches {
            memo_cap,
            memo_max_bytes,
            growing_cap,
            growing_max_bytes,
            ..HighlightCaches::default()
        }
    }

    fn tick(&mut self) -> u64 {
        self.epoch += 1;
        self.epoch
    }

    fn memo_get(&mut self, key: MemoKey) -> Option<Vec<Vec<Span>>> {
        let epoch = self.tick();
        let entry = self.memo.get_mut(&key)?;
        entry.0 = epoch;
        Some(entry.1.clone())
    }

    fn memo_remove(&mut self, key: &MemoKey) {
        if let Some((_, spans)) = self.memo.remove(key) {
            self.memo_bytes = self.memo_bytes.saturating_sub(spans_bytes(&spans));
        }
    }

    fn memo_insert(&mut self, key: MemoKey, spans: Vec<Vec<Span>>) {
        let epoch = self.tick();
        self.memo_remove(&key); // replace: drop the old entry's bytes first
        self.memo_bytes += spans_bytes(&spans);
        self.memo.insert(key, (epoch, spans));
        // Evict oldest-epoch entries until under both budgets (the newest
        // entry stays even if it alone exceeds them).
        while self.memo.len() > 1
            && (self.memo.len() > self.memo_cap || self.memo_bytes > self.memo_max_bytes)
        {
            let Some(oldest) = self
                .memo
                .iter()
                .min_by_key(|(_, (epoch, _))| *epoch)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.memo_remove(&oldest);
        }
    }

    fn highlight_block(&mut self, code: &str, lang: &str) -> Vec<Vec<Span>> {
        if lang.is_empty() {
            return plain_lines(code);
        }
        let set = syntax_set();
        let Some(syntax) = set.find_syntax_by_token(lang) else {
            return plain_lines(code);
        };

        let memo_key = memo_key(code, lang);
        if let Some(spans) = self.memo_get(memo_key) {
            return spans;
        }

        let (complete, partial) = split_complete(code);
        let key = growing_key(code, lang);

        // Reuse retained lines when this block grew append-only.
        if let Some(pos) = self.growing.iter().position(|e| {
            e.key == key
                && e.lang == lang
                && code.len() >= e.text.len()
                && code.starts_with(&e.text)
        }) {
            let epoch = self.tick();
            let added_bytes;
            {
                let entry = &mut self.growing[pos];
                entry.epoch = epoch;
                // Complete lines added since the last render.
                let delta = &complete[entry.text.len()..];
                let mut highlighter = HighlightLines::from_state(
                    theme(),
                    entry.highlight_state.clone(),
                    entry.parse_state.clone(),
                );
                let new_spans: Vec<Vec<Span>> = delta
                    .lines()
                    .map(|line| highlight_one(&mut highlighter, line, set))
                    .collect();
                added_bytes = delta.len() + spans_bytes(&new_spans);
                entry.bytes += added_bytes;
                entry.spans.extend(new_spans);
                entry.text.push_str(delta);
                let (highlight_state, parse_state) = highlighter.state();
                entry.highlight_state = highlight_state;
                entry.parse_state = parse_state;
            }
            self.growing_bytes += added_bytes;
            let entry = &mut self.growing[pos];

            let mut out = entry.spans.clone();
            if !partial.is_empty() {
                out.push(highlight_partial(
                    &entry.highlight_state,
                    &entry.parse_state,
                    partial,
                    set,
                ));
            }
            // Replace the previous intermediate memo entry: only the
            // latest state of a growing block may occupy a memo slot.
            if let Some(old) = entry.memo_key.replace(memo_key) {
                self.memo_remove(&old);
            }
            self.memo_insert(memo_key, out.clone());
            self.enforce_growing_budget(Some(pos));
            return out;
        }

        // First sight (or a non-append-only change): full highlight.
        // Retain only the complete lines; the partial last line is
        // highlighted ephemerally like any later delta's.
        let mut entry = self.fresh_entry(key, lang, complete, syntax, set);
        let mut out = entry.spans.clone();
        if !partial.is_empty() {
            out.push(highlight_partial(
                &entry.highlight_state,
                &entry.parse_state,
                partial,
                set,
            ));
        }
        entry.memo_key = Some(memo_key);
        self.growing_push(entry);
        self.memo_insert(memo_key, out.clone());
        out
    }

    /// Highlight `complete` (ends at a line boundary, or empty) from a
    /// fresh state into a new growing entry.
    fn fresh_entry(
        &self,
        key: u64,
        lang: &str,
        complete: &str,
        syntax: &SyntaxReference,
        set: &SyntaxSet,
    ) -> GrowingEntry {
        let mut highlighter = HighlightLines::new(syntax, theme());
        let spans: Vec<Vec<Span>> = complete
            .lines()
            .map(|line| highlight_one(&mut highlighter, line, set))
            .collect();
        let (highlight_state, parse_state) = highlighter.state();
        GrowingEntry {
            key,
            lang: lang.to_string(),
            text: complete.to_string(),
            bytes: complete.len() + spans_bytes(&spans),
            spans,
            highlight_state,
            parse_state,
            memo_key: None,
            epoch: self.epoch,
        }
    }

    /// Drop a growing entry and its memo slot, keeping the byte books.
    fn growing_remove(&mut self, pos: usize) {
        let removed = self.growing.remove(pos);
        self.growing_bytes = self.growing_bytes.saturating_sub(removed.bytes);
        if let Some(key) = removed.memo_key {
            self.memo_remove(&key);
        }
    }

    /// Evict oldest growing entries while over the byte budget. `keep`
    /// survives (it just served a render) even if alone over budget.
    fn enforce_growing_budget(&mut self, keep: Option<usize>) {
        while self.growing_bytes > self.growing_max_bytes && self.growing.len() > 1 {
            let Some(oldest) = self
                .growing
                .iter()
                .enumerate()
                .filter(|(i, _)| Some(*i) != keep)
                .min_by_key(|(_, e)| e.epoch)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.growing_remove(oldest);
        }
    }

    fn growing_push(&mut self, entry: GrowingEntry) {
        // A stale entry for the same block (non-append-only rewrite) is
        // superseded by this fresh one — drop it along with its memo slot.
        if let Some(pos) = self
            .growing
            .iter()
            .position(|e| e.key == entry.key && e.lang == entry.lang)
        {
            self.growing_remove(pos);
        }
        if self.growing.len() >= self.growing_cap
            && let Some(oldest) = self
                .growing
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.epoch)
                .map(|(i, _)| i)
        {
            self.growing_remove(oldest);
        }
        self.growing_bytes += entry.bytes;
        self.growing.push(entry);
        self.enforce_growing_budget(Some(self.growing.len() - 1));
    }
}

/// Highlight a partial last line from retained states without advancing
/// them (syntect exposes both states as `Clone` exactly for this).
fn highlight_partial(
    highlight_state: &HighlightState,
    parse_state: &ParseState,
    partial: &str,
    set: &SyntaxSet,
) -> Vec<Span> {
    let mut highlighter =
        HighlightLines::from_state(theme(), highlight_state.clone(), parse_state.clone());
    highlight_one(&mut highlighter, partial, set)
}

static CACHES: OnceLock<Mutex<HighlightCaches>> = OnceLock::new();

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn rust_code_gets_colored() {
        let spans = highlight_line("fn main() {}", "rust");
        assert!(
            spans.len() > 1,
            "expected multiple styled regions: {spans:?}"
        );
        assert_eq!(
            spans.iter().map(|s| &*s.text).collect::<String>(),
            "fn main() {}"
        );
    }

    #[test]
    fn unknown_lang_is_plain() {
        let spans = highlight_line("whatever", "notalang");
        assert_eq!(spans.len(), 1);
    }

    /// Multi-line constructs (here a block comment) must color identically
    /// whether the block arrived whole or line-by-line.
    #[test]
    fn incremental_matches_fresh() {
        let lines = [
            "fn main() {",
            "    /* a comment",
            "       spanning lines */",
            "    let s = \"string \\\" escaped\";",
            "    println!(\"{s}\");",
            "}",
        ];
        let mut caches = HighlightCaches::default();
        let mut acc = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                acc.push('\n');
            }
            acc.push_str(line);
            assert_eq!(
                caches.highlight_block(&acc, "rust"),
                highlight_block_uncached(&acc, "rust"),
                "diverged at {} lines: {acc:?}",
                i + 1
            );
        }
    }

    /// Deltas need not align to lines: the partial last line is extended,
    /// which must not corrupt the retained state.
    #[test]
    fn partial_line_growth() {
        let full = "let a = 1;\nlet b = 2;\nlet c = 3;";
        let mut caches = HighlightCaches::default();
        for end in [5, 12, 15, 24, 30, full.len()] {
            let acc = &full[..end];
            assert_eq!(
                caches.highlight_block(acc, "rust"),
                highlight_block_uncached(acc, "rust"),
                "diverged at byte {end}: {acc:?}"
            );
        }
    }

    /// Same first line + lang, different continuation: the prefix check
    /// must reject the stale entry instead of appending onto it.
    #[test]
    fn non_append_growth_resets() {
        let mut caches = HighlightCaches::default();
        let v1 = "fn main() {\n    let x = 1;\n}";
        let v2 = "fn main() {\n    let y = 2;";
        assert_eq!(
            caches.highlight_block(v1, "rust"),
            highlight_block_uncached(v1, "rust")
        );
        // Shorter, divergent content with the same first line.
        assert_eq!(
            caches.highlight_block(v2, "rust"),
            highlight_block_uncached(v2, "rust")
        );
        // And the original block still highlights correctly afterwards.
        assert_eq!(
            caches.highlight_block(v1, "rust"),
            highlight_block_uncached(v1, "rust")
        );
        // The rewrite superseded the stale entry: one entry per block.
        assert_eq!(caches.growing.len(), 1);
    }

    #[test]
    fn memo_returns_identical_on_repeat() {
        let mut caches = HighlightCaches::default();
        let code = "fn f() {}\nlet x = 1;";
        let first = caches.highlight_block(code, "rust");
        let second = caches.highlight_block(code, "rust");
        assert_eq!(first, second);
        assert_eq!(first, highlight_block_uncached(code, "rust"));
    }

    /// The growing-block memo slot must track the latest content only:
    /// extending a block must not leave stale memo entries behind.
    #[test]
    fn growing_block_replaces_memo_entry() {
        let mut caches = HighlightCaches::default();
        let v1 = "fn main() {";
        let v2 = "fn main() {\n}";
        caches.highlight_block(v1, "rust");
        assert_eq!(caches.memo.len(), 1);
        caches.highlight_block(v2, "rust");
        assert_eq!(caches.memo.len(), 1);
        assert!(caches.memo.contains_key(&memo_key(v2, "rust")));
    }

    #[test]
    fn memo_eviction_bounds_capacity() {
        let mut caches = HighlightCaches::default();
        for i in 0..MEMO_CAP + 10 {
            caches.highlight_block(&format!("let x{i} = {i};"), "rust");
        }
        assert!(caches.memo.len() <= MEMO_CAP);
        assert!(caches.growing.len() <= GROWING_CAP);
    }

    /// Big entries must evict by bytes even below the count cap.
    #[test]
    fn memo_byte_budget_evicts() {
        // ~4 KB spans per entry against a 9 KB budget → at most 2 survive.
        let mut caches = HighlightCaches::with_limits(32, 9 * 1024, 4, usize::MAX);
        for i in 0..5 {
            let code = format!("// {:>4000}\nlet x{i} = {i};", "");
            caches.highlight_block(&code, "rust");
        }
        assert!(
            caches.memo.len() <= 3,
            "{} entries survived the byte budget",
            caches.memo.len()
        );
        assert!(caches.memo_bytes <= 9 * 1024 + 8192);
    }

    /// Growing blocks likewise: a new block evicts the oldest when the
    /// combined retention exceeds the byte budget.
    #[test]
    fn growing_byte_budget_evicts_oldest() {
        // ~8 KB per block (4 KB text + 4 KB spans) against a 9 KB budget.
        let mut caches = HighlightCaches::with_limits(32, usize::MAX, 4, 9 * 1024);
        let a = format!("// {:>4000}\nlet a = 1;", "");
        let b = format!("/* {:>4000} */\nlet b = 2;", "");
        caches.highlight_block(&a, "rust");
        assert_eq!(caches.growing.len(), 1);
        caches.highlight_block(&b, "rust");
        assert_eq!(caches.growing.len(), 1);
        assert!(caches.growing_bytes <= 9 * 1024 + 8192);
        // Eviction never affects output: a re-renders via the fresh path.
        assert_eq!(
            caches.highlight_block(&a, "rust"),
            highlight_block_uncached(&a, "rust")
        );
    }

    /// A growing block's memo slot tracks the latest content only, with
    /// the byte books balanced across the replace.
    #[test]
    fn memo_replace_keeps_byte_books() {
        let mut caches = HighlightCaches::default();
        let code = "let x = 1;\nlet y = 2;";
        caches.highlight_block(code, "rust");
        let first = caches.memo_bytes;
        assert!(first > 0);
        caches.highlight_block(code, "rust"); // memo hit: no change
        assert_eq!(caches.memo_bytes, first);
        let extended = format!("{code}\nlet z = 3;");
        caches.highlight_block(&extended, "rust");
        assert!(!caches.memo.contains_key(&memo_key(code, "rust")));
        assert!(caches.memo.contains_key(&memo_key(&extended, "rust")));
        let rendered = caches.highlight_block(&extended, "rust");
        assert_eq!(caches.memo_bytes, spans_bytes(&rendered));
    }

    /// Unknown languages stay plain and never touch the caches.
    #[test]
    fn unknown_lang_bypasses_caches() {
        let mut caches = HighlightCaches::default();
        assert_eq!(
            caches.highlight_block("some code\nmore", "notalang"),
            plain_lines("some code\nmore")
        );
        assert!(caches.memo.is_empty() && caches.growing.is_empty());
    }
}
