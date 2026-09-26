//! Styled line model: the unit every component renders to. All width math is
//! in terminal cells (grapheme-aware); ANSI exists only at output time.

use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::style::Style;

/// Zero-width APC marker components emit at the hardware cursor position.
/// Renderers find and strip it, then place the real cursor there (IME).
pub use crate::component::CURSOR_MARKER;

/// A run of text in one style. The text is shared (`Arc<str>`): cloning a
/// Span/Line is a refcount bump, not a String copy — full-transcript frame
/// assembly at large contexts is dominated by these clones (TS pi gets the
/// same economics for free from immutable JS strings).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: Arc<str>,
    pub style: Style,
    /// Zero-width raw terminal output (escape sequences, image protocols).
    /// Excluded from width math; passes through to_ansi unstyled.
    pub zero_width: bool,
}

impl Span {
    pub fn styled(text: impl Into<Arc<str>>, style: Style) -> Self {
        Span {
            text: text.into(),
            style,
            zero_width: false,
        }
    }

    pub fn plain(text: impl Into<Arc<str>>) -> Self {
        Span {
            text: text.into(),
            style: Style::default(),
            zero_width: false,
        }
    }

    /// Raw terminal output (e.g. Kitty/iTerm2 image escapes): zero width, no
    /// SGR wrapping.
    pub fn raw(text: impl Into<Arc<str>>) -> Self {
        Span {
            text: text.into(),
            style: Style::default(),
            zero_width: true,
        }
    }

    pub fn width(&self) -> usize {
        if self.zero_width {
            0
        } else {
            grapheme_width(&self.text)
        }
    }

    /// Text safe to write to the terminal: zero-width raw spans (intentional
    /// ANSI — hyperlinks, image protocols, the cursor marker) pass through
    /// verbatim; everything else goes through [`sanitize`].
    pub fn terminal_text(&self) -> std::borrow::Cow<'_, str> {
        if self.zero_width {
            std::borrow::Cow::Borrowed(&self.text)
        } else {
            sanitize(&self.text)
        }
    }
}

/// One rendered line: a row of styled spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn new() -> Self {
        Line::default()
    }

    pub fn plain(text: impl Into<Arc<str>>) -> Self {
        Line {
            spans: vec![Span::plain(text)],
        }
    }

    pub fn styled(text: impl Into<Arc<str>>, style: Style) -> Self {
        Line {
            spans: vec![Span::styled(text, style)],
        }
    }

    pub fn from_spans(spans: Vec<Span>) -> Self {
        Line { spans }
    }

    pub fn push(&mut self, span: Span) {
        // Never merge zero-width raw spans (cursor markers, image escapes)
        // into styled text: the merged span would count toward width math
        // and could be fragmented by wrapping.
        if let Some(last) = self.spans.last_mut()
            && last.style == span.style
            && !last.zero_width
            && !span.zero_width
        {
            // Copy-on-write merge: spans being merged are freshly built by
            // the component (not shared), so this rebuild is render-time,
            // not frame-time.
            let mut merged = String::from(&*last.text);
            merged.push_str(&span.text);
            last.text = Arc::from(merged);
            return;
        }
        self.spans.push(span);
    }

    /// Visible width in terminal cells.
    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.width() == 0
    }

    /// Plain text content (no styling).
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| &*s.text).collect()
    }

    /// Pad with spaces (styled) to at least `width` cells.
    pub fn pad_right(&mut self, width: usize, style: Style) {
        let missing = width.saturating_sub(self.width());
        if missing > 0 {
            self.push(Span::styled(" ".repeat(missing), style));
        }
    }

    /// Hard-truncate to `width` cells, adding `…` when content was cut.
    pub fn truncate(&mut self, width: usize, ellipsis: bool) {
        let ellipsis = ellipsis && self.width() > width;
        let target = if ellipsis {
            width.saturating_sub(1)
        } else {
            width
        };
        *self = self.slice_cells(0, target);
        if ellipsis && width > 0 {
            let style = self.spans.last().map(|s| s.style).unwrap_or_default();
            self.push(Span::styled("…", style));
        }
    }

    /// Slice a cell range [start, start+len) out of the line. Zero-width spans
    /// pass through unconditionally (they attach to whatever cells survive).
    pub fn slice_cells(&self, start: usize, len: usize) -> Line {
        let mut out = Line::new();
        let mut col = 0usize;
        // Saturate: callers pass usize::MAX for "to the end" (overlay
        // compositing); start + len would overflow and panic.
        let end = start.saturating_add(len);
        for span in &self.spans {
            if span.zero_width {
                out.push(span.clone());
                continue;
            }
            for grapheme in span.text.graphemes(true) {
                let w = grapheme_width(grapheme);
                if w == 0 {
                    // Zero-width (combining) chars attach to the previous cell.
                    if col > start && col <= end {
                        out.push(Span::styled(grapheme, span.style));
                    }
                    continue;
                }
                if col + w > start && col < end {
                    out.push(Span::styled(grapheme, span.style));
                }
                col += w;
                // No early return here: zero-width raw spans (cursor marker,
                // image escapes) sitting past the cut point must still pass
                // through — the doc contract is "unconditionally".
            }
        }
        out
    }

    /// Word-wrap to `width` cells. Breaks at whitespace when possible,
    /// otherwise hard-breaks (CJK graphemes break freely). Styles preserved.
    pub fn wrap(&self, width: usize) -> Vec<Line> {
        if width == 0 {
            return vec![Line::new()];
        }
        let has_newline = self.spans.iter().any(|s| s.text.contains('\n'));
        if self.width() <= width && !has_newline {
            return vec![self.clone()];
        }
        // Group graphemes into runs by kind (whitespace run vs word) + style.
        // Zero-width raw spans ride along as their own runs. Newlines are
        // their own runs and never merge: a Line is exactly one terminal
        // row, so a raw '\n' must become a hard break, not emitted bytes.
        let mut words: Vec<(String, Style, bool, bool)> = Vec::new(); // (text, style, is_space, zero_width)
        for span in &self.spans {
            if span.zero_width {
                words.push((span.text.to_string(), span.style, false, true));
                continue;
            }
            for grapheme in span.text.graphemes(true) {
                let is_space = grapheme.trim().is_empty();
                let is_newline = grapheme.contains('\n');
                match words.last_mut() {
                    Some(last)
                        if last.1 == span.style
                            && last.2 == is_space
                            && !last.3
                            && !is_newline
                            && !last.0.contains('\n') =>
                    {
                        last.0.push_str(grapheme);
                    }
                    _ => words.push((grapheme.to_string(), span.style, is_space, false)),
                }
            }
        }

        let mut lines: Vec<Line> = Vec::new();
        let mut current = Line::new();
        let mut current_w = 0usize;
        for (word, style, is_space, zero_width) in words {
            if zero_width {
                current.push(Span {
                    text: word.into(),
                    style,
                    zero_width: true,
                });
                continue;
            }
            let word_w = grapheme_width(&word);
            if is_space {
                if word.contains('\n') {
                    // Hard break: end the current row, drop the newline run.
                    trim_trailing_spaces(&mut current);
                    lines.push(std::mem::take(&mut current));
                    current_w = 0;
                    continue;
                }
                if current_w + word_w <= width {
                    current.push(Span::styled(word, style));
                    current_w += word_w;
                }
                // else: drop whitespace at the wrap boundary
                continue;
            }
            if current_w > 0 && current_w + word_w > width {
                trim_trailing_spaces(&mut current);
                lines.push(std::mem::take(&mut current));
                current_w = 0;
            }
            if word_w <= width {
                current.push(Span::styled(word, style));
                current_w += word_w;
            } else {
                // Hard-break a word longer than the width.
                for grapheme in word.graphemes(true) {
                    let w = grapheme_width(grapheme);
                    if current_w + w > width && current_w > 0 {
                        lines.push(std::mem::take(&mut current));
                        current_w = 0;
                    }
                    current.push(Span::styled(grapheme, style));
                    current_w += w;
                }
            }
        }
        lines.push(current);
        lines
    }

    /// Emit ANSI for the whole line (SGR per span, reset at end if needed).
    pub fn to_ansi(&self) -> String {
        let mut out = String::new();
        let mut any_styled = false;
        for span in &self.spans {
            if span.text.is_empty() {
                continue;
            }
            out.push_str(&span.style.ansi_prefix());
            out.push_str(&span.terminal_text());
            out.push_str(span.style.ansi_suffix());
            any_styled |= !span.style.is_plain();
        }
        let _ = any_styled;
        out
    }
}

/// Strip terminal control characters (C0 0x00–0x1F incl. ESC, C1 0x80–0x9F,
/// DEL) from span text before it reaches the terminal: model/tool output
/// containing e.g. OSC 52 (`\x1b]52;c;…\x07`, clipboard write) or
/// cursor-movement escapes must never be emitted verbatim (the same threat
/// model as `terminal::set_title`'s filter). Tab is kept — the wrap/layout
/// code already treats it as an ordinary whitespace run — and a raw '\n'
/// cannot legitimately get here (wrap hard-breaks newlines into separate
/// rows first), so both are handled exactly as layout leaves them.
/// Zero-width raw spans bypass this entirely via `Span::terminal_text`.
pub fn sanitize(text: &str) -> std::borrow::Cow<'_, str> {
    fn strippable(c: char) -> bool {
        // char::is_control covers C0, C1 (U+0080–U+009F) and DEL (U+007F).
        c.is_control() && c != '\t'
    }
    if !text.chars().any(strippable) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.chars().filter(|c| !strippable(*c)).collect())
}

/// Cell width of a grapheme cluster (0 for combining/zero-width, 2 for wide).
pub fn grapheme_width(text: &str) -> usize {
    // Skip the zero-width hardware cursor marker used by focused components.
    if text == CURSOR_MARKER {
        return 0;
    }
    UnicodeWidthStr::width(text)
}

/// Strip trailing whitespace spans from a wrapped line.
fn trim_trailing_spaces(line: &mut Line) {
    while let Some(last) = line.spans.last_mut() {
        let trimmed = last.text.trim_end().to_string();
        let removed_all = trimmed.is_empty();
        last.text = Arc::from(trimmed);
        if !removed_all {
            return;
        }
        line.spans.pop();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::style::Color;

    #[test]
    fn width_counts_cells() {
        assert_eq!(Line::plain("hello").width(), 5);
        assert_eq!(Line::plain("你好").width(), 4);
        assert_eq!(Line::plain("").width(), 0);
    }

    #[test]
    fn truncate_with_ellipsis() {
        let mut line = Line::plain("hello world");
        line.truncate(6, true);
        assert_eq!(line.text(), "hello…");
        assert_eq!(line.width(), 6);
    }

    #[test]
    fn slice_respects_wide_chars() {
        let line = Line::plain("a你好b");
        assert_eq!(line.slice_cells(1, 2).text(), "你");
        assert_eq!(line.slice_cells(0, 4).text(), "a你好");
        assert_eq!(line.slice_cells(5, 1).text(), "b");
    }

    #[test]
    fn slice_to_end_with_max_len() {
        // Regression: overlay compositing calls slice_cells(col, usize::MAX);
        // start + len overflowed (debug panic / release wrap → dropped the
        // right side of every overlaid row).
        let line = Line::plain("hello world");
        assert_eq!(line.slice_cells(6, usize::MAX).text(), "world");
        assert_eq!(line.slice_cells(0, usize::MAX).text(), "hello world");
    }

    #[test]
    fn slice_keeps_zero_width_spans_after_cut_point() {
        // Zero-width raw spans must "pass through unconditionally" (doc
        // contract): a cursor marker / image escape sitting past the slice
        // end must survive, otherwise truncating an over-wide editor line
        // loses the hardware-cursor marker entirely.
        let line = Line::from_spans(vec![Span::plain("hello world"), Span::raw(CURSOR_MARKER)]);
        let sliced = line.slice_cells(0, 3);
        assert!(sliced.text().ends_with(CURSOR_MARKER), "{sliced:?}");
        assert_eq!(&*sliced.spans[0].text, "hel");
    }

    #[test]
    fn wrap_breaks_at_embedded_newlines() {
        // A Line is exactly one terminal row; a raw '\n' inside it would be
        // emitted verbatim by to_ansi and desync every renderer's row
        // accounting. wrap must hard-break instead.
        let lines = Line::plain("a\nb\n\nc").wrap(80);
        assert_eq!(
            lines.iter().map(Line::text).collect::<Vec<_>>(),
            vec!["a", "b", "", "c"]
        );
        let lines = Line::plain("one\r\ntwo").wrap(80);
        assert_eq!(
            lines.iter().map(Line::text).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        let lines = Line::plain("hello world foo").wrap(7);
        assert_eq!(
            lines.iter().map(Line::text).collect::<Vec<_>>(),
            vec!["hello", "world", "foo"]
        );
    }

    #[test]
    fn wrap_hard_breaks_long_words() {
        let lines = Line::plain("abcdefghij").wrap(4);
        assert_eq!(
            lines.iter().map(Line::text).collect::<Vec<_>>(),
            vec!["abcd", "efgh", "ij"]
        );
    }

    #[test]
    fn wrap_preserves_style() {
        let styled = Style::new().fg(Color::Indexed(1));
        let line = Line::from_spans(vec![Span::styled("aa bb", styled)]);
        let lines = line.wrap(3);
        assert!(
            lines
                .iter()
                .all(|l| l.spans.iter().all(|s| s.style == styled))
        );
    }

    #[test]
    fn ansi_roundtrip_shape() {
        let line = Line::from_spans(vec![
            Span::plain("a"),
            Span::styled("b", Style::new().bold()),
        ]);
        assert_eq!(line.to_ansi(), "a\x1b[1mb\x1b[0m");
    }

    #[test]
    fn sanitize_strips_osc52_clipboard_write() {
        let evil = "safe\x1b]52;c;aGVsbG8=\x07text";
        // Only the control chars go; the now-inert printable payload stays.
        assert_eq!(sanitize(evil), "safe]52;c;aGVsbG8=text");
        assert!(!sanitize(evil).chars().any(|c| c.is_control() && c != '\t'));
    }

    #[test]
    fn sanitize_strips_cursor_movement_and_c0() {
        // CSI cursor-up + CR + BEL + NUL: none may reach the terminal.
        let evil = "up\x1b[2A\r\x07\x00end";
        assert_eq!(sanitize(evil), "up[2Aend");
        // DEL is a control char too.
        assert_eq!(sanitize("a\x7fb"), "ab");
    }

    #[test]
    fn sanitize_strips_c1_range() {
        // U+0080–U+009F (here NEL and C1 CSI) are 2-byte UTF-8 sequences;
        // stripping the chars removes both bytes.
        let evil = "a\u{0085}\u{009B}5Hb";
        assert_eq!(sanitize(evil), "a5Hb");
    }

    #[test]
    fn sanitize_keeps_tab_and_wide_unicode() {
        // Tab survives (layout treats it as a whitespace run); wide CJK,
        // emoji and combining chars pass through untouched.
        let text = "a\tb 你好 🦀 e\u{0301}";
        assert!(matches!(sanitize(text), std::borrow::Cow::Borrowed(_)));
        assert_eq!(sanitize(text), text);
    }

    #[test]
    fn to_ansi_sanitizes_styled_spans_but_not_raw() {
        // Styled span text is sanitized; zero-width raw spans (hyperlinks,
        // image protocols) carry intentional ANSI and pass through verbatim.
        let link = "\x1b]8;;https://example.com\x07link\x1b]8;;\x07";
        let line = Line::from_spans(vec![Span::plain("\x1b]52;c;evil\x07"), Span::raw(link)]);
        let ansi = line.to_ansi();
        assert!(!ansi.contains("\x1b]52"), "OSC 52 leaked: {ansi:?}");
        assert!(ansi.contains(link), "raw span was filtered: {ansi:?}");
    }
}
