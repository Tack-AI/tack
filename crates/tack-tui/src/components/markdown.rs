//! Markdown component: renders markdown to styled lines via pulldown-cmark.
//! Port of tack-tui's `markdown.ts` rendering semantics (headings, code blocks,
//! lists, quotes, tables, links); mermaid fences render via an optional hook.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::component::Component;
use crate::line::{Line, Span};
use crate::style::{Color, Style};

/// Theme slots for markdown rendering.
#[derive(Clone, Copy, Debug)]
pub struct MarkdownTheme {
    pub heading: Style,
    pub code: Style,
    pub code_block: Style,
    pub code_block_border: Style,
    pub quote: Style,
    pub quote_border: Style,
    pub link: Style,
    pub link_url: Style,
    pub list_bullet: Style,
    pub hr: Style,
    /// Indent (in cells) prepended to code-block lines (TS markdown.codeBlockIndent).
    pub code_block_indent: u8,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        Self::dark()
    }
}

impl MarkdownTheme {
    /// TS pi dark.json md* slots.
    pub fn dark() -> Self {
        MarkdownTheme {
            heading: Style::new().bold().fg(Color::Rgb(0xf0, 0xc6, 0x74)),
            code: Style::new().fg(Color::Rgb(0x8a, 0xbe, 0xb7)),
            code_block: Style::new().fg(Color::Rgb(0xb5, 0xbd, 0x68)),
            code_block_border: Style::new().fg(Color::Rgb(0x80, 0x80, 0x80)),
            quote: Style::new().italic().fg(Color::Rgb(0x80, 0x80, 0x80)),
            quote_border: Style::new().fg(Color::Rgb(0x80, 0x80, 0x80)),
            link: Style::new().underline().fg(Color::Rgb(0x81, 0xa2, 0xbe)),
            link_url: Style::new().fg(Color::Rgb(0x66, 0x66, 0x66)),
            list_bullet: Style::new().fg(Color::Rgb(0x8a, 0xbe, 0xb7)),
            hr: Style::new().fg(Color::Rgb(0x80, 0x80, 0x80)),
            code_block_indent: 0,
        }
    }

    /// TS pi light.json md* slots.
    pub fn light() -> Self {
        MarkdownTheme {
            heading: Style::new().bold().fg(Color::Rgb(0x9a, 0x73, 0x26)),
            code: Style::new().fg(Color::Rgb(0x5a, 0x80, 0x80)),
            code_block: Style::new().fg(Color::Rgb(0x58, 0x84, 0x58)),
            code_block_border: Style::new().fg(Color::Rgb(0x6c, 0x6c, 0x6c)),
            quote: Style::new().italic().fg(Color::Rgb(0x6c, 0x6c, 0x6c)),
            quote_border: Style::new().fg(Color::Rgb(0x6c, 0x6c, 0x6c)),
            link: Style::new().underline().fg(Color::Rgb(0x54, 0x7d, 0xa7)),
            link_url: Style::new().fg(Color::Rgb(0x76, 0x76, 0x76)),
            list_bullet: Style::new().fg(Color::Rgb(0x58, 0x84, 0x58)),
            hr: Style::new().fg(Color::Rgb(0x6c, 0x6c, 0x6c)),
            code_block_indent: 0,
        }
    }
}

/// Markdown renderer component.
#[derive(Debug)]
pub struct Markdown {
    text: String,
    pub theme: MarkdownTheme,
    cache: Option<(u16, Vec<Line>)>,
}

impl Markdown {
    pub fn new(text: impl Into<String>, theme: MarkdownTheme) -> Self {
        Markdown {
            text: text.into(),
            theme,
            cache: None,
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.invalidate();
    }
}

impl Component for Markdown {
    fn render(&mut self, width: u16) -> Vec<Line> {
        if let Some((w, ref lines)) = self.cache
            && w == width
        {
            return lines.clone();
        }
        let lines = render_markdown(&self.text, width as usize, &self.theme);
        self.cache = Some((width, lines.clone()));
        lines
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}

/// pulldown-cmark is CommonMark-strict: an ordered list may only interrupt a
/// paragraph when it starts at 1. TS pi's `marked` interrupts on any start
/// number, and models routinely emit grouped lists like
/// "**Section:**\n4. ...\n5. ..." (no blank line before the first item).
/// Insert a blank line before such items so they parse as lists — the list
/// keeps its start number, so items render with their original numbering.
pub fn normalize_list_interruptions(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut in_fence: Option<char> = None; // '`' or '~'
    let mut prev: Option<&str> = None;
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        // Track fenced code blocks (``` or ~~~) so list-looking lines inside
        // them are left alone.
        if let Some(marker) = trimmed.chars().next().filter(|c| *c == '`' || *c == '~') {
            let run = trimmed.chars().take_while(|c| *c == marker).count();
            if run >= 3 {
                match in_fence {
                    None => in_fence = Some(marker),
                    Some(m) if m == marker => in_fence = None,
                    _ => {}
                }
            }
        }
        if in_fence.is_none()
            && ordered_item_number(trimmed).is_some_and(|n| n != 1)
            && prev.is_some_and(|p| {
                let p = p.trim_start();
                !p.trim().is_empty() && ordered_item_number(p).is_none() && !is_bullet_item(p)
            })
        {
            out.push('\n'); // blank line so the list may interrupt the paragraph
        }
        out.push_str(line);
        if lines.peek().is_some() {
            out.push('\n');
        }
        prev = Some(line);
    }
    out
}

/// Parse an ordered-list item marker ("4. ", "12) ") from a trimmed line.
fn ordered_item_number(trimmed: &str) -> Option<u64> {
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    let rest = &trimmed[digits.len()..];
    let mut chars = rest.chars();
    match chars.next() {
        Some('.') | Some(')') => {}
        _ => return None,
    }
    match chars.next() {
        None => {}
        Some(c) if c.is_whitespace() => {}
        _ => return None,
    }
    digits.parse().ok()
}

fn is_bullet_item(trimmed: &str) -> bool {
    let mut chars = trimmed.chars();
    matches!(chars.next(), Some('-' | '+' | '*')) && chars.next().is_some_and(char::is_whitespace)
}

/// Render markdown text into styled lines (used standalone too).
pub fn render_markdown(text: &str, width: usize, theme: &MarkdownTheme) -> Vec<Line> {
    render_markdown_with(text, width, theme, None)
}

/// Mermaid hook: called with the diagram source and content width for
/// ```mermaid code blocks; returning `Some` replaces the code block with
/// custom lines (e.g. a rendered image), `None` falls back to plain
/// code-block rendering.
pub type MermaidHook<'a> = &'a mut dyn FnMut(&str, usize) -> Option<Vec<Line>>;

/// Closed-ness of every mermaid fence in `text`, in document order. While a
/// mermaid fence is still streaming (unclosed), rendering its partial source
/// as an image would morph the image on every delta — the hook must only
/// fire for closed fences (partial source renders as plain code).
fn mermaid_fences_closed(text: &str) -> Vec<bool> {
    let mut states = Vec::new();
    let mut open: Option<bool> = None; // Some(true) if the open fence is mermaid
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if !is_fence {
            continue;
        }
        match open {
            None => {
                let lang = trimmed[3..].trim();
                let is_mermaid = lang.eq_ignore_ascii_case("mermaid");
                if is_mermaid {
                    states.push(false);
                }
                open = Some(is_mermaid);
            }
            Some(is_mermaid) => {
                if is_mermaid && let Some(last) = states.last_mut() {
                    *last = true;
                }
                open = None;
            }
        }
    }
    states
}

/// Render markdown with an optional mermaid hook (see [`MermaidHook`]).
pub fn render_markdown_with(
    text: &str,
    width: usize,
    theme: &MarkdownTheme,
    mut mermaid: Option<MermaidHook<'_>>,
) -> Vec<Line> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let normalized = normalize_list_interruptions(text);
    let mermaid_closed = if mermaid.is_some() {
        mermaid_fences_closed(&normalized)
    } else {
        Vec::new()
    };
    let mut mermaid_block_index = 0usize;
    let parser = Parser::new_ext(&normalized, options);
    let mut out: Vec<Line> = Vec::new();
    let mut inline: Line = Line::new();
    let mut style_stack: Vec<Style> = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_text = String::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut quote_depth = 0usize;
    let mut link_url: Option<String> = None;
    let mut in_table = false;
    let mut in_table_head = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut table_row: Vec<String> = Vec::new();
    let mut table_cell = String::new();
    // Span index where a pending bullet marker goes: TaskListMarker arrives
    // AFTER Start(Item), so the checkbox state can't be known at item start.
    let mut pending_bullet: Option<usize> = None;

    let flush_inline =
        |out: &mut Vec<Line>, inline: &mut Line, quote_depth: usize, theme: &MarkdownTheme| {
            if inline.is_empty() {
                return;
            }
            let mut line = std::mem::take(inline);
            if quote_depth > 0 {
                line.spans.insert(
                    0,
                    Span::styled("│ ".repeat(quote_depth), theme.quote_border),
                );
                for span in &mut line.spans {
                    span.style = span.style.merged_with(&theme.quote);
                }
            }
            out.extend(line.wrap(width.max(1)));
        };

    for event in parser {
        // A bullet item with no TaskListMarker gets the default "• " marker,
        // flushed before its first content/structural event.
        if !matches!(event, Event::TaskListMarker(_))
            && let Some(idx) = pending_bullet.take()
        {
            inline
                .spans
                .insert(idx, Span::styled("• ", theme.list_bullet));
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => {
                    style_stack.push(theme.heading);
                }
                Tag::CodeBlock(kind) => {
                    in_code_block = true;
                    code_lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                        pulldown_cmark::CodeBlockKind::Indented => String::new(),
                    };
                }
                Tag::List(start) => {
                    flush_inline(&mut out, &mut inline, quote_depth, theme);
                    list_stack.push(start);
                }
                Tag::Item => {
                    flush_inline(&mut out, &mut inline, quote_depth, theme);
                    let depth = list_stack.len() - 1;
                    inline.push(Span::plain("  ".repeat(depth)));
                    match list_stack.last_mut().expect("list stack") {
                        Some(n) => {
                            let marker = format!("{n}. ");
                            *n += 1;
                            inline.push(Span::styled(marker, theme.list_bullet));
                        }
                        None => {
                            // Bullet/task marker deferred: TaskListMarker
                            // (when present) is emitted after Start(Item).
                            pending_bullet = Some(inline.spans.len());
                        }
                    }
                }
                Tag::BlockQuote(_) => {
                    flush_inline(&mut out, &mut inline, quote_depth, theme);
                    quote_depth += 1;
                }
                Tag::Strong => style_stack.push(Style::new().bold()),
                Tag::Emphasis => style_stack.push(Style::new().italic()),
                Tag::Strikethrough => {
                    style_stack.push(Style {
                        strikethrough: true,
                        ..Style::default()
                    });
                }
                Tag::Link { dest_url, .. } => {
                    link_url = Some(dest_url.to_string());
                    style_stack.push(theme.link);
                }
                Tag::Table(_) => {
                    in_table = true;
                    table_rows.clear();
                }
                Tag::TableHead => {
                    in_table_head = true;
                    table_row = Vec::new();
                }
                Tag::TableRow => {
                    table_row = Vec::new();
                }
                Tag::TableCell => {
                    table_cell = String::new();
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    flush_inline(&mut out, &mut inline, quote_depth, theme);
                    out.push(Line::new());
                }
                TagEnd::Heading(_) => {
                    style_stack.pop();
                    flush_inline(&mut out, &mut inline, quote_depth, theme);
                    out.push(Line::new());
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    let indent = " ".repeat(theme.code_block_indent as usize);
                    let mermaid_lines = if code_lang.trim().eq_ignore_ascii_case("mermaid") {
                        let index = mermaid_block_index;
                        mermaid_block_index += 1;
                        // Only closed fences render as images (see
                        // mermaid_fences_closed): an unclosed fence is still
                        // streaming and its partial source must stay a plain
                        // code block until it completes.
                        if mermaid_closed.get(index).copied().unwrap_or(true) {
                            mermaid.as_deref_mut().and_then(|hook| {
                                hook(code_text.trim_end_matches('\n'), width.saturating_sub(4))
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(image) = mermaid_lines {
                        out.extend(image);
                    } else {
                        for spans in crate::syntax::highlight_block(
                            code_text.trim_end_matches('\n'),
                            &code_lang,
                        ) {
                            let mut rendered = Line::new();
                            rendered.push(Span::plain(indent.clone()));
                            rendered.push(Span::styled("│ ", theme.code_block_border));
                            rendered.push(Span::plain(" "));
                            for mut span in spans {
                                span.style = span.style.merged_with(&theme.code_block);
                                rendered.push(span);
                            }
                            out.push(rendered);
                        }
                    }
                    code_text.clear();
                    out.push(Line::new());
                }
                TagEnd::List(_) => {
                    flush_inline(&mut out, &mut inline, quote_depth, theme);
                    list_stack.pop();
                    if list_stack.is_empty() {
                        out.push(Line::new());
                    }
                }
                TagEnd::Item => {
                    flush_inline(&mut out, &mut inline, quote_depth, theme);
                }
                TagEnd::BlockQuote(_) => {
                    flush_inline(&mut out, &mut inline, quote_depth, theme);
                    quote_depth = quote_depth.saturating_sub(1);
                    if quote_depth == 0 {
                        out.push(Line::new());
                    }
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                    style_stack.pop();
                }
                TagEnd::Link => {
                    // Append the URL dimmed when it differs from the text.
                    if let Some(url) = link_url.take() {
                        let shown = inline.text();
                        if !shown.ends_with(&url) && !url.is_empty() {
                            inline.push(Span::styled(format!(" ({url})"), theme.link_url));
                        }
                    }
                    style_stack.pop();
                }
                TagEnd::TableHead => {
                    in_table_head = false;
                    table_rows.push(std::mem::take(&mut table_row));
                }
                TagEnd::TableRow => {
                    if !in_table_head {
                        table_rows.push(std::mem::take(&mut table_row));
                    }
                }
                TagEnd::TableCell => {
                    table_row.push(std::mem::take(&mut table_cell));
                }
                TagEnd::Table => {
                    in_table = false;
                    render_table(&table_rows, width, theme, &mut out);
                    table_rows.clear();
                    out.push(Line::new());
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_text.push_str(&text);
                } else if in_table {
                    table_cell.push_str(&text);
                } else {
                    let base = *style_stack.last().expect("style stack");
                    inline.push(Span::styled(text.to_string(), base));
                }
            }
            Event::Code(code) => {
                if in_table {
                    table_cell.push_str(&code);
                } else {
                    inline.push(Span::styled(format!("`{code}`"), theme.code));
                }
            }
            Event::SoftBreak => inline.push(Span::plain(" ")),
            Event::HardBreak => {
                flush_inline(&mut out, &mut inline, quote_depth, theme);
            }
            Event::Rule => {
                flush_inline(&mut out, &mut inline, quote_depth, theme);
                out.push(Line::styled("─".repeat(width.max(4)), theme.hr));
                out.push(Line::new());
            }
            Event::TaskListMarker(checked) => {
                if let Some(idx) = pending_bullet.take() {
                    let marker = if checked { "☑ " } else { "☐ " };
                    inline
                        .spans
                        .insert(idx, Span::styled(marker, theme.list_bullet));
                }
            }
            _ => {}
        }
    }
    flush_inline(&mut out, &mut inline, quote_depth, theme);
    // Trim trailing blank lines.
    while out.last().is_some_and(Line::is_empty) {
        out.pop();
    }
    let _ = code_lang;
    out
}

fn render_table(rows: &[Vec<String>], width: usize, theme: &MarkdownTheme, out: &mut Vec<Line>) {
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    // Natural column widths, capped so one wide cell can't starve the others.
    let mut col_widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            col_widths[i] = col_widths[i].max(crate::line::grapheme_width(cell));
        }
    }
    // Chrome: "│ " per column + trailing "│".
    let chrome = cols * 2 + 1;
    let available = width.saturating_sub(chrome).max(cols * 3);
    let natural: usize = col_widths.iter().sum();
    if natural > available {
        // Fair share first (small columns keep their natural width), then
        // distribute the leftover to the widest columns. Floor of 3 cells
        // keeps content readable instead of collapsing to "…".
        let base = (available / cols).max(3);
        let mut widths: Vec<usize> = col_widths.iter().map(|w| (*w).min(base)).collect();
        let mut leftover = available.saturating_sub(widths.iter().sum()) as i64;
        while leftover > 0 {
            // Give one cell to each column that's below its natural width,
            // widest deficit first.
            let Some((best, _)) = widths
                .iter()
                .enumerate()
                .filter(|(i, w)| **w < col_widths[*i])
                .max_by_key(|(i, w)| col_widths[*i] - **w)
            else {
                break;
            };
            widths[best] += 1;
            leftover -= 1;
        }
        col_widths = widths;
    }

    for (r, row) in rows.iter().enumerate() {
        // Wrap each cell into its column; the row takes as many visual lines
        // as its tallest cell.
        let wrapped: Vec<Vec<Line>> = (0..cols)
            .map(|i| {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let w = col_widths[i].max(1);
                let mut pieces = Line::plain(cell.to_string()).wrap(w);
                for piece in &mut pieces {
                    piece.pad_right(w, Style::default());
                }
                pieces
            })
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for sub in 0..height {
            let mut line = Line::new();
            for (i, cell_lines) in wrapped.iter().enumerate() {
                line.push(Span::styled("│ ", theme.quote_border));
                let style = if r == 0 {
                    Style::new().bold()
                } else {
                    Style::default()
                };
                match cell_lines.get(sub) {
                    Some(piece) => {
                        let mut piece = piece.clone();
                        for span in &mut piece.spans {
                            span.style = span.style.merged_with(&style);
                        }
                        for span in piece.spans {
                            line.push(span);
                        }
                    }
                    None => line.push(Span::plain(" ".repeat(col_widths[i]))),
                }
            }
            line.push(Span::styled("│", theme.quote_border));
            out.push(line);
        }
        if r == 0 {
            out.push(Line::styled(
                "─".repeat((chrome + col_widths.iter().sum::<usize>()).min(width)),
                theme.quote_border,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_basics() {
        let lines = render_markdown(
            "# Title\n\nSome **bold** and `code`.\n\n- a\n- b\n",
            40,
            &MarkdownTheme::default(),
        );
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(texts.iter().any(|t| t.contains("Title")));
        assert!(
            texts
                .iter()
                .any(|t| t.contains("Some **bold") || t.contains("bold"))
        );
        assert!(texts.iter().any(|t| t.contains("• a")));
        assert!(texts.iter().any(|t| t.contains("• b")));
    }

    #[test]
    fn renders_code_block_with_border() {
        let lines = render_markdown(
            "```rust\nfn main() {}\n```\n",
            40,
            &MarkdownTheme::default(),
        );
        assert!(lines.iter().any(|l| l.text().contains("│  fn main() {}")));
    }

    #[test]
    fn mermaid_hook_fires_only_for_closed_fences() {
        let mut calls: Vec<String> = Vec::new();
        let mut hook = |source: &str, _w: usize| -> Option<Vec<Line>> {
            calls.push(source.to_string());
            Some(vec![Line::plain("IMAGE")])
        };
        // Closed fence: hook fires, image replaces the code block.
        let closed = render_markdown_with(
            "```mermaid\nflowchart LR; A-->B\n```\n",
            40,
            &MarkdownTheme::default(),
            Some(&mut hook),
        );
        assert_eq!(calls.len(), 1);
        assert!(closed.iter().any(|l| l.text() == "IMAGE"));

        // Unclosed fence (still streaming): hook must NOT fire; the partial
        // source stays a plain code block.
        let mut calls2: Vec<String> = Vec::new();
        let mut hook2 = |source: &str, _w: usize| -> Option<Vec<Line>> {
            calls2.push(source.to_string());
            Some(vec![Line::plain("IMAGE")])
        };
        let open = render_markdown_with(
            "```mermaid\nflowchart LR; A-->\n",
            40,
            &MarkdownTheme::default(),
            Some(&mut hook2),
        );
        assert!(calls2.is_empty());
        assert!(!open.iter().any(|l| l.text() == "IMAGE"));
        assert!(open.iter().any(|l| l.text().contains("flowchart LR; A-->")));
    }

    #[test]
    fn task_list_markers_track_their_own_item() {
        // Regression: TaskListMarker arrives after Start(Item), but the
        // marker was chosen at item start — every item rendered the
        // PREVIOUS item's checkbox state (first always "•").
        let lines = render_markdown(
            "- [x] done\n- [ ] todo\n- plain\n",
            40,
            &MarkdownTheme::default(),
        );
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(texts.iter().any(|t| t.contains("☑ done")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("☐ todo")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("• plain")), "{texts:?}");
    }

    #[test]
    fn renders_table() {
        let lines = render_markdown(
            "| a | b |\n|---|---|\n| 1 | 2 |\n",
            40,
            &MarkdownTheme::default(),
        );
        assert!(lines.iter().any(|l| l.text().contains('│')));
    }

    #[test]
    fn table_keeps_all_content_when_narrow() {
        // Wide cells in a narrow viewport: wrap instead of truncating to "…".
        let md = "| Name | Description |\n|---|---|\n| alpha | a fairly long description of the thing |\n| beta | short |\n";
        let lines = render_markdown(md, 30, &MarkdownTheme::default());
        let text: String = lines.iter().map(Line::text).collect::<Vec<_>>().join("\n");
        assert!(!text.contains('…'), "no truncation expected:\n{text}");
        // Content survives across wrapped sub-rows (never lost or "…"ed).
        assert!(text.contains("alph"), "{text}");
        assert!(text.contains("beta"), "{text}");
        assert!(text.contains("fairly long"), "{text}");
        assert!(text.contains("description of the"), "{text}");
        // Rows wrap: more visual lines than table rows.
        let row_lines = lines.iter().filter(|l| l.text().starts_with('│')).count();
        assert!(row_lines > 4, "expected wrapped sub-rows:\n{text}");
    }

    #[test]
    fn table_body_first_cell_not_dropped() {
        // Regression: the first body cell used to leak out of the table.
        let lines = render_markdown(
            "| h1 | h2 |\n|---|---|\n| body1 | body2 |\n",
            40,
            &MarkdownTheme::default(),
        );
        let text: String = lines.iter().map(Line::text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("body1"), "{text}");
        assert!(text.contains("body2"), "{text}");
    }

    #[test]
    fn ordered_list_starting_above_one_after_paragraph_gets_own_lines() {
        // Regression (marked parity): "**Section:**\n4. ...\n5. ..." without a
        // blank line — CommonMark absorbs the items into the paragraph; TS
        // pi's marked renders each item on its own line with its number.
        let md = "1. first\n2. second\n3. third\n\n**Section A:**\n4. fourth\n5. fifth\n\n**Section B:**\n9. ninth\n";
        let lines = render_markdown(md, 120, &MarkdownTheme::default());
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        let find = |needle: &str| texts.iter().position(|t| t.contains(needle));
        let (a, b, c) = (find("Section A:"), find("4. fourth"), find("5. fifth"));
        assert!(a.is_some() && b.is_some() && c.is_some(), "{texts:?}");
        assert!(
            a < b && b < c,
            "items must follow the heading, one per line: {texts:?}"
        );
        assert!(
            find("9. ninth").is_some(),
            "start number preserved: {texts:?}"
        );
    }

    #[test]
    fn normalize_skips_fenced_code_and_continuing_lists() {
        // List-looking lines inside fences are untouched.
        let md = "```\n2. not a list\n```\n";
        assert_eq!(normalize_list_interruptions(md), md);
        // Consecutive items (3. → 4.) stay in one list — no blank inserted.
        let md = "3. a\n4. b\n";
        assert_eq!(normalize_list_interruptions(md), md);
        // After a blank line nothing to do.
        let md = "para\n\n2. item\n";
        assert_eq!(normalize_list_interruptions(md), md);
        // Paragraph interrupted by a "4." item gets the blank line.
        let md = "**Head:**\n4. item\n5. next\n";
        assert_eq!(
            normalize_list_interruptions(md),
            "**Head:**\n\n4. item\n5. next\n"
        );
    }
}
