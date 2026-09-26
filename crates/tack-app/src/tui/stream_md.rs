//! Incremental markdown rendering for the streaming assistant partial.
//!
//! The streaming partial is re-rendered on every throttle window; a naive
//! render re-parses and re-highlights the whole accumulated text each time
//! (O(n²) over a stream). This module splits the text into a *stable
//! prefix* and a *tail*: the prefix ends at a block boundary that later
//! text cannot retroactively change (a closed code fence, or a blank line
//! between non-list blocks), so its render is cached keyed by
//! (content hash, width) and only the tail is re-rendered per pass.
//!
//! Output is identical to a full `render_markdown_with` by construction:
//!
//! - The prefix is rendered with a sentinel paragraph appended
//!   (`...\n\nSENTINEL\n`). The sentinel keeps the prefix's trailing
//!   inter-block blank lines materialized (a standalone render would trim
//!   them); popping the sentinel line yields exactly the lines a full
//!   render produces for the prefix, including the blank separator(s) the
//!   following tail content needs.
//! - Blocks sealed by a blank line or a closed fence cannot be re-opened
//!   by later text in CommonMark. The known exceptions are excluded:
//!   reference-style link definitions and HTML comments can reach
//!   backwards, so their presence anywhere in the text falls back to a
//!   full render (and drops the cached prefix).
//! - Loose/tight list rendering is retroactive (a later blank-separated
//!   item re-renders earlier items with blank separators), so a blank-line
//!   boundary next to a list-item line is never taken.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

use tack_tui::Line;
use tack_tui::components::markdown::{MarkdownTheme, MermaidHook, render_markdown_with};

/// Sentinel paragraph text: a single private-use char (never wraps at
/// width >= 1, never produced by markdown source).
const SENTINEL: &str = "\u{E000}";

/// Content fingerprint of a prefix slice (len + hash), matching the
/// fingerprint style used by the streaming throttle.
fn fingerprint(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(text.as_bytes());
    h.finish()
}

/// List-item marker at the start of a (trimmed) line: `- `, `* `, `+ `,
/// or `1. ` / `2) ` with any start number. Mirrors the ordered-item
/// parsing in tack-tui's markdown normalizer.
fn is_list_item(line: &str) -> bool {
    let t = line.trim_start();
    let mut chars = t.chars();
    if matches!(chars.next(), Some('-' | '+' | '*'))
        && chars.next().is_some_and(char::is_whitespace)
    {
        return true;
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 9 {
        return false;
    }
    let rest = &t[digits.len()..];
    let mut chars = rest.chars();
    if !matches!(chars.next(), Some('.') | Some(')')) {
        return false;
    }
    match chars.next() {
        None => true,
        Some(c) => c.is_whitespace(),
    }
}

/// Anything in the text that can retroactively change the rendering of
/// already-sealed blocks (see module docs). Conservative: false positives
/// just cost a full render.
fn has_segmentation_hazard(text: &str) -> bool {
    // HTML comments can span blank lines; the markdown parser would
    // treat blank-separated content inside them as one HTML block while
    // the tail render would not.
    if text.contains("<!--") {
        return true;
    }
    // Reference-style link definitions (`[label]: url`) rewrite earlier
    // `[label]` references anywhere in the document.
    text.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with('[') && t.contains("]:")
    })
}

/// Largest byte offset splitting `text` into a stable prefix and a
/// re-rendered tail, or 0 when there is no safe split (or the tail would
/// be blank-only, in which case the sentinel trick would keep one blank
/// line too many).
pub(crate) fn stable_boundary(text: &str) -> usize {
    // Line starts for offset math.
    let mut starts = Vec::with_capacity(text.len() / 40 + 2);
    starts.push(0usize);
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    let line = |k: usize| -> (&str, usize) {
        let start = starts[k];
        let after = if k + 1 < starts.len() {
            starts[k + 1]
        } else {
            text.len()
        };
        // `after` includes the newline; the line itself excludes it.
        let end = if after > start && text.as_bytes()[after - 1] == b'\n' {
            after - 1
        } else {
            after
        };
        (&text[start..end], after)
    };
    let non_blank_after =
        |k: usize| -> bool { (k + 1..starts.len()).any(|j| !line(j).0.trim().is_empty()) };
    let next_content = |k: usize| -> Option<&str> {
        (k + 1..starts.len())
            .map(|j| line(j).0)
            .find(|l| !l.trim().is_empty())
    };

    let mut boundary = 0usize;
    // Open fence: (marker byte, run length). Closing requires a line of
    // only the same marker, run >= opening run (CommonMark).
    let mut fence: Option<(u8, usize)> = None;
    let mut prev_content: Option<&str> = None;
    for k in 0..starts.len() {
        let (raw, after) = line(k);
        if let Some((marker, run)) = fence {
            // Inside a fence: only a closing line matters.
            let trimmed = raw.trim_start_matches(' ');
            if !trimmed.is_empty()
                && trimmed.len() >= run.max(3)
                && trimmed.bytes().all(|b| b == marker)
            {
                fence = None;
                prev_content = Some(raw);
                // A closed fence seals everything before it — as long as
                // real content follows (a blank-only tail trims away the
                // separator the sentinel keeps; see below).
                if after < text.len() && non_blank_after(k) {
                    boundary = after;
                }
            }
            continue;
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // Blank line: blocks before it are sealed... unless the block
            // on either side is a list item (loose/tight list rendering is
            // retroactive — see module docs).
            if after < text.len()
                && let (Some(prev), Some(next)) = (prev_content, next_content(k))
                && !is_list_item(prev)
                && !is_list_item(next)
            {
                boundary = after;
            }
            continue;
        }
        // Fence open? Up to 3 leading spaces (4+ would be indented code).
        let deindented = raw.trim_start_matches(' ');
        if !raw.starts_with('\t') && raw.len() - deindented.len() <= 3 {
            let first = deindented.as_bytes()[0];
            if first == b'`' || first == b'~' {
                let run = deindented.bytes().take_while(|&b| b == first).count();
                if run >= 3 {
                    let info = &deindented[run..];
                    // A backtick in the info string means this is not a
                    // fence at all in CommonMark.
                    if !(first == b'`' && info.contains('`')) {
                        fence = Some((first, run));
                        prev_content = Some(raw);
                        continue;
                    }
                }
            }
        }
        prev_content = Some(raw);
    }
    // A blank-only tail would trim to nothing in a full render, while the
    // sentinel-kept prefix retains its trailing separator — divergent.
    if boundary > 0 && text[boundary..].trim().is_empty() {
        return 0;
    }
    boundary
}

/// Cached stable-prefix render for one content block.
#[derive(Debug)]
struct PrefixRender {
    width: usize,
    /// Byte length of the stable prefix within the block's text.
    len: usize,
    hash: u64,
    /// Prefix render INCLUDING its trailing inter-block blank lines
    /// (sentinel trick — concatenating the tail render reproduces a full
    /// render byte-for-byte).
    lines: Vec<Line>,
}

/// Per-block incremental markdown cache for the streaming partial. Keyed
/// by content-block index (blocks only append during a stream). Cleared
/// when streaming ends.
#[derive(Default, Debug)]
pub struct StreamMarkdownCache {
    blocks: HashMap<usize, PrefixRender>,
    /// (full prefix renders, exact cache hits, incremental extensions) —
    /// test instrumentation.
    #[cfg(test)]
    stats: (usize, usize, usize),
}

impl StreamMarkdownCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
    }

    /// Render one markdown block, reusing the cached stable prefix when the
    /// split point, prefix content, and width all match.
    pub fn render(
        &mut self,
        block: usize,
        text: &str,
        width: usize,
        theme: &MarkdownTheme,
        mut mermaid: Option<MermaidHook<'_>>,
    ) -> Vec<Line> {
        let boundary = if has_segmentation_hazard(text) {
            0
        } else {
            stable_boundary(text)
        };
        if boundary == 0 {
            // No safe split (or a retroactive hazard): full render, and
            // drop any cached prefix — a hazard may already have
            // invalidated it.
            self.blocks.remove(&block);
            return render_markdown_with(text, width, theme, mermaid);
        }
        let hash = fingerprint(&text[..boundary]);
        // Route the hook through a local shim: MermaidHook<'a> =
        // &'a mut (dyn FnMut + 'a) is invariant in 'a, so a direct reborrow
        // (`as_deref_mut`) would have to live for the whole 'a and could not
        // be used again for the tail render. The shim is a fresh closure
        // that CAN be reborrowed per call.
        let mut shim = |src: &str, w: usize| -> Option<Vec<Line>> {
            match mermaid.as_deref_mut() {
                Some(h) => h(src, w),
                None => None,
            }
        };
        // Render a prefix/segment with a sentinel paragraph appended so its
        // trailing inter-block blanks survive the trailing-blank trim.
        // Returns None if the sentinel was absorbed (caller falls back).
        fn render_sealed(
            segment: &str,
            width: usize,
            theme: &MarkdownTheme,
            shim: &mut dyn FnMut(&str, usize) -> Option<Vec<Line>>,
        ) -> Option<Vec<Line>> {
            let segment = segment.trim_end_matches('\n');
            let mut src = String::with_capacity(segment.len() + SENTINEL.len() + 4);
            src.push_str(segment);
            src.push_str("\n\n");
            src.push_str(SENTINEL);
            src.push('\n');
            let mut lines = render_markdown_with(&src, width, theme, Some(shim));
            if lines.last().is_none_or(|l| l.text() != SENTINEL) {
                return None;
            }
            lines.pop();
            Some(lines)
        }
        // A non-blank tail that renders to NOTHING (e.g. an empty unclosed
        // fence) breaks the sentinel contract: a full render would trim the
        // separator blanks the prefix cache keeps. Render the whole text for
        // exactness (rare; the cache stays valid for later passes).
        macro_rules! extend_or_full {
            ($out:expr) => {{
                let tail = render_markdown_with(&text[boundary..], width, theme, Some(&mut shim));
                if tail.is_empty() {
                    return render_markdown_with(text, width, theme, Some(&mut shim));
                }
                $out.extend(tail);
                $out
            }};
        }
        // Exact hit: same split point, same prefix content, same width.
        if let Some(p) = self.blocks.get(&block)
            && p.width == width
            && p.len == boundary
            && p.hash == hash
        {
            #[cfg(test)]
            {
                self.stats.1 += 1;
            }
            let mut out = p.lines.clone();
            return extend_or_full!(out);
        }
        // Boundary advanced: render only the newly stabilized SEGMENT and
        // append it to the cached prefix — a long prose stream stays O(n)
        // overall instead of re-rendering the whole prefix per paragraph.
        if let Some(p) = self.blocks.get_mut(&block)
            && p.width == width
            && p.len < boundary
            && fingerprint(&text[..p.len]) == p.hash
            && let Some(segment_lines) =
                render_sealed(&text[p.len..boundary], width, theme, &mut shim)
        {
            #[cfg(test)]
            {
                self.stats.2 += 1;
            }
            p.lines.extend(segment_lines);
            p.len = boundary;
            p.hash = hash;
            let mut out = p.lines.clone();
            return extend_or_full!(out);
        }
        #[cfg(test)]
        {
            self.stats.0 += 1;
        }
        let Some(lines) = render_sealed(&text[..boundary], width, theme, &mut shim) else {
            // Sentinel absorbed into surrounding content (should not
            // happen) — fall back to a safe full render.
            self.blocks.remove(&block);
            return render_markdown_with(text, width, theme, Some(&mut shim));
        };
        let mut out = lines.clone();
        self.blocks.insert(
            block,
            PrefixRender {
                width,
                len: boundary,
                hash,
                lines,
            },
        );
        extend_or_full!(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn render_full(text: &str) -> Vec<String> {
        render_markdown_with(text, 80, &MarkdownTheme::dark(), None)
            .iter()
            .map(Line::text)
            .collect()
    }

    /// Every prefix of a growing stream renders identically via the
    /// incremental cache and a full render.
    #[test]
    fn incremental_matches_full_render_across_a_stream() {
        let full = concat!(
            "Here is some **bold** intro text that keeps growing.\n",
            "\n",
            "- one\n",
            "- two\n",
            "\n",
            "A paragraph after the list.\n",
            "\n",
            "```rust\n",
            "fn main() {\n",
            "    println!(\"hi\");\n",
            "}\n",
            "```\n",
            "\n",
            "After the code block, more prose with `inline` code.\n",
            "\n",
            "```python\n",
            "def f():\n",
            "    return 42\n",
            "```\n",
            "\n",
            "> a quote\n",
            "\n",
            "Final paragraph with a [link](https://example.com).\n",
        );
        let mut cache = StreamMarkdownCache::new();
        // Simulate streaming growth at arbitrary byte steps (char-boundary
        // safe: step by chars).
        let steps: Vec<usize> = (0..=full.chars().count())
            .step_by(3)
            .map(|i| {
                full.char_indices()
                    .nth(i)
                    .map(|(b, _)| b)
                    .unwrap_or(full.len())
            })
            .collect();
        for end in steps {
            let text = &full[..end];
            let expected = render_full(text);
            let got: Vec<String> = cache
                .render(0, text, 80, &MarkdownTheme::dark(), None)
                .iter()
                .map(Line::text)
                .collect();
            assert_eq!(got, expected, "diverged at byte {end} of:\n{text}");
        }
    }

    /// Loose/tight list retroactivity: blank-separated items re-render
    /// earlier items; the incremental render must track a full render.
    #[test]
    fn incremental_matches_full_render_with_lists() {
        let full = "- a\n- b\n\n- c\n\ntail paragraph\n\n- x\n";
        let mut cache = StreamMarkdownCache::new();
        for end in 1..=full.len() {
            if !full.is_char_boundary(end) {
                continue;
            }
            let text = &full[..end];
            let got: Vec<String> = cache
                .render(0, text, 80, &MarkdownTheme::dark(), None)
                .iter()
                .map(Line::text)
                .collect();
            assert_eq!(got, render_full(text), "diverged at {end}");
        }
    }

    /// Cache reuse: once a fence closes, growing the tail must NOT
    /// re-render the stable prefix.
    #[test]
    fn closed_prefix_is_reused_while_tail_grows() {
        let mut cache = StreamMarkdownCache::new();
        let base = "intro paragraph\n\n```rust\nfn main() {}\n```\n\ntail";
        let _ = cache.render(0, base, 80, &MarkdownTheme::dark(), None);
        assert_eq!(cache.stats.0, 1, "prefix rendered once");
        assert_eq!(cache.stats.1, 0);
        for extra in [" ", "more", " and more", "\n\neven more"] {
            let text = format!("{base}{extra}");
            let got: Vec<String> = cache
                .render(0, &text, 80, &MarkdownTheme::dark(), None)
                .iter()
                .map(Line::text)
                .collect();
            assert_eq!(got, render_full(&text));
        }
        assert_eq!(
            cache.stats.0, 1,
            "prefix not re-rendered: {:?}",
            cache.stats
        );
        assert_eq!(
            cache.stats.1, 3,
            "tail growth within a block served from cache"
        );
        assert_eq!(
            cache.stats.2, 1,
            "sealing the tail paragraph extends the prefix incrementally"
        );
    }

    /// Retroactive hazards (reference link definitions, HTML comments)
    /// fall back to full renders and stay consistent.
    #[test]
    fn hazards_fall_back_to_full_render() {
        let mut cache = StreamMarkdownCache::new();
        let stages = [
            "See [the docs] for details.\n\nMore text here.\n",
            "See [the docs] for details.\n\nMore text here.\n\n[the docs]: https://example.com\n",
            "Comment:\n\n<!--\nhidden\n\n-->\n\nafter\n",
        ];
        for text in stages {
            let got: Vec<String> = cache
                .render(0, text, 80, &MarkdownTheme::dark(), None)
                .iter()
                .map(Line::text)
                .collect();
            assert_eq!(got, render_full(text), "hazard text diverged:\n{text}");
        }
    }

    /// The reference-definition hazard must turn a previously-rendered
    /// `[label]` into a link — proving the fallback is load-bearing.
    #[test]
    fn late_reference_definition_renders_as_link() {
        let mut cache = StreamMarkdownCache::new();
        let before = "See [the docs] now.\n";
        let after = "See [the docs] now.\n\n[the docs]: https://example.com\n";
        let _ = cache.render(0, before, 80, &MarkdownTheme::dark(), None);
        let got: Vec<String> = cache
            .render(0, after, 80, &MarkdownTheme::dark(), None)
            .iter()
            .map(Line::text)
            .collect();
        let expected = render_full(after);
        assert_eq!(got, expected);
        assert!(
            expected.iter().any(|t| t.contains("https://example.com")),
            "link URL should appear: {expected:?}"
        );
    }

    #[test]
    fn boundary_split_points() {
        // Closed fence followed by content: split right after the fence.
        let text = "para\n\n```\ncode\n```\nmore\n";
        let b = stable_boundary(text);
        assert_eq!(&text[..b], "para\n\n```\ncode\n```\n");
        // Unclosed fence: no split inside it (blank lines in code!).
        let text = "para\n\n```\ncode\n\nmore code\n";
        assert_eq!(stable_boundary(text), "para\n\n".len());
        // Blank-only tail: no split.
        let text = "```\ncode\n```\n\n\n";
        assert_eq!(stable_boundary(text), 0);
        // List adjacency: no split next to list items.
        let text = "- a\n\n- b\n";
        assert_eq!(stable_boundary(text), 0);
        // Paragraphs: split at the blank line.
        let text = "one\n\ntwo\n";
        assert_eq!(stable_boundary(text), "one\n\n".len());
    }
}
