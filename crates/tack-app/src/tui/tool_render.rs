//! Tool execution rendering (port of `tool-execution.ts` + `bash-execution.ts`
//! and `diff.ts`): per-tool call/result formatting, colored diffs, streaming
//! bash output.

use serde_json::Value;
use tack_agent_core::AgentToolResult;
use tack_tui::{Line, Span, Style};

use super::theme::Theme;

/// State of one tool call in the transcript.
#[derive(Clone, Debug)]
pub enum ToolState {
    Running {
        partial_output: String,
    },
    Done {
        result: AgentToolResult,
        is_error: bool,
    },
}

#[derive(Clone, Debug)]
pub struct ToolEntry {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    /// Fingerprint of `args`, refreshed at every assignment — lets the
    /// streaming update path (run.rs) compare new args against the cached
    /// old-side fingerprint instead of re-walking BOTH Values per delta.
    pub args_fp: (usize, u64),
    pub state: ToolState,
    pub expanded: bool,
}

/// Cheap (size, hash) fingerprint of a tool-args Value — replaces a full
/// structural Value equality walk on every streaming update (large
/// file-write arguments stream in hundreds of deltas).
pub(crate) fn args_fingerprint(v: &serde_json::Value) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    fn walk(v: &serde_json::Value, h: &mut impl Hasher) -> usize {
        match v {
            serde_json::Value::Null => {
                0u8.hash(h);
                4
            }
            serde_json::Value::Bool(b) => {
                1u8.hash(h);
                b.hash(h);
                5
            }
            serde_json::Value::Number(n) => {
                2u8.hash(h);
                let s = n.to_string();
                s.hash(h);
                s.len()
            }
            serde_json::Value::String(s) => {
                3u8.hash(h);
                s.hash(h);
                s.len() + 2
            }
            serde_json::Value::Array(a) => {
                4u8.hash(h);
                a.len().hash(h);
                2 + a.iter().map(|x| walk(x, h)).sum::<usize>()
            }
            serde_json::Value::Object(o) => {
                5u8.hash(h);
                o.len().hash(h);
                let mut size = 2;
                for (k, x) in o {
                    k.hash(h);
                    size += k.len() + 1 + walk(x, h);
                }
                size
            }
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let size = walk(v, &mut h);
    (size, h.finish())
}

/// One-line summary of a call (the title row). The row is: marker
/// (3 cols) + title — so the title budget is width-4 (marker + 1 col for
/// the ellipsis itself, otherwise the frame clips the "…" off the right
/// edge). The full text is rendered by `expanded_args_detail` when the
/// card is expanded.
pub fn tool_title(tool_name: &str, args: &Value, width: u16) -> String {
    let budget = (width as usize).saturating_sub(4).max(20);
    let short = |key: &str| {
        // The title is one terminal row: flatten embedded newlines
        // (multi-line bash commands) instead of emitting them raw.
        args.get(key)
            .and_then(Value::as_str)
            .map(|s| s.replace(['\n', '\r'], " "))
            .unwrap_or_default()
    };
    let full = match tool_name {
        "bash" => format!("$ {}", short("command")),
        "read" => format!("read {}", short("path")),
        "write" => format!("write {}", short("path")),
        "edit" => format!("edit {}", short("path")),
        "grep" => format!("grep {}", short("pattern")),
        "find" => format!("find {}", short("pattern")),
        "ls" => format!("ls {}", short("path")),
        other => unknown_tool_title(other, args),
    };
    if full.chars().count() > budget {
        format!("{}…", full.chars().take(budget - 1).collect::<String>())
    } else {
        full
    }
}

/// Title for a tool without a dedicated summary. Small args render exactly
/// as before (`{name} {compact-json}`); large args (a multi-KB MCP payload
/// that would be serialized in full and then truncated to one row anyway)
/// get a string-field summary or the bare tool name instead.
fn unknown_tool_title(tool_name: &str, args: &Value) -> String {
    /// Cheap size estimate (string lengths only — no allocation, no number
    /// formatting); anything under the cap takes the exact legacy path.
    fn approx_len(v: &Value) -> usize {
        match v {
            Value::String(s) => s.len(),
            Value::Array(a) => a.iter().map(approx_len).sum::<usize>() + 2,
            Value::Object(o) => {
                o.iter()
                    .map(|(k, x)| k.len() + approx_len(x))
                    .sum::<usize>()
                    + 2
            }
            _ => 16,
        }
    }
    const SERIALIZE_CAP: usize = 4096;
    if approx_len(args) <= SERIALIZE_CAP {
        return format!("{tool_name} {args}");
    }
    let hint = ["command", "path", "pattern", "query", "url"]
        .iter()
        .find_map(|key| args.get(key).and_then(Value::as_str))
        .map(|s| s.replace(['\n', '\r'], " "));
    match hint {
        Some(hint) => format!("{tool_name} {hint}"),
        None => tool_name.to_string(),
    }
}

/// Full argument text for the EXPANDED card (ctrl+o): the title row is
/// truncated, so without this the complete command/args are invisible
/// anywhere. bash shows the raw command (multi-line); path/pattern tools
/// show the full title value; other tools the compact JSON. Capped at 100
/// lines against pathological heredocs.
fn expanded_args_detail(tool_name: &str, args: &Value) -> Vec<String> {
    const MAX_DETAIL_LINES: usize = 100;
    let full = match tool_name {
        "bash" => args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        "read" | "write" | "edit" | "ls" => args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        "grep" | "find" => args
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => args.to_string(),
    };
    if full.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = full.lines().map(str::to_string).collect();
    if lines.len() > MAX_DETAIL_LINES {
        let remaining = lines.len() - MAX_DETAIL_LINES;
        lines.truncate(MAX_DETAIL_LINES);
        lines.push(crate::i18n::trf(
            "tool.more_lines",
            &[("remaining", &remaining.to_string())],
        ));
    }
    lines
}

impl ToolEntry {
    pub fn render(
        &self,
        width: u16,
        theme: &Theme,
        image_protocol: Option<tack_tui::image::ImageProtocol>,
        image_max_cells: Option<u16>,
    ) -> Vec<Line> {
        let w = width as usize;
        let mut lines = Vec::new();
        let (marker, bg) = match &self.state {
            ToolState::Running { .. } => ("●", theme.tool_pending_bg),
            ToolState::Done { is_error: true, .. } => ("✗", theme.tool_error_bg),
            ToolState::Done {
                is_error: false, ..
            } => ("✓", theme.tool_success_bg),
        };
        let title = tool_title(&self.tool_name, &self.args, width);
        let mut title_line = Line::new();
        title_line.push(Span::styled(
            format!(" {marker} "),
            theme.tool_title.merged_with(&bg),
        ));
        title_line.push(Span::styled(title, theme.tool_title.merged_with(&bg)));
        title_line.pad_right(w, bg);
        lines.push(title_line);

        // Expanded (ctrl+o): the full command/args below the title — the
        // title row truncates, and the body only shows OUTPUT.
        if self.expanded {
            for detail in expanded_args_detail(&self.tool_name, &self.args) {
                for mut wrapped in
                    Line::styled(detail, theme.tool_output).wrap(w.saturating_sub(2).max(1))
                {
                    wrapped
                        .spans
                        .insert(0, Span::styled("  ", Style::default()));
                    lines.push(wrapped);
                }
            }
        }

        // Body: result/diff/partial output.
        let body_lines = match &self.state {
            ToolState::Running { partial_output } if !partial_output.is_empty() => {
                preview_lines(partial_output, self.expanded, theme.tool_output)
            }
            ToolState::Done { result, .. } => {
                if let Some(diff) = result.details.get("diff").and_then(Value::as_str) {
                    render_diff(diff, theme)
                } else {
                    let text = result
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    preview_lines(&text, self.expanded, theme.tool_output)
                }
            }
            _ => Vec::new(),
        };
        let _ = body_lines.len();
        for mut line in body_lines {
            line.spans.insert(0, Span::styled("  ", Style::default()));
            line.truncate(w, false);
            lines.push(line);
        }

        // Image result blocks: inline when the terminal supports it.
        if let Some(protocol) = image_protocol
            && let ToolState::Done { result, .. } = &self.state
        {
            use base64::Engine;
            for block in &result.content {
                if let tack_ai::InputContentBlock::Image { data, mime_type } = block {
                    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
                        continue;
                    };
                    // Kitty requires PNG; convert other formats.
                    let png: Option<Vec<u8>> = if mime_type == "image/png" {
                        Some(bytes)
                    } else {
                        image::load_from_memory(&bytes).ok().and_then(|img| {
                            let mut buf = std::io::Cursor::new(Vec::new());
                            img.write_to(&mut buf, image::ImageFormat::Png)
                                .ok()
                                .map(|_| buf.into_inner())
                        })
                    };
                    if let Some(png) = png {
                        let (pw, ph) = tack_tui::image::png_dimensions(&png);
                        lines.extend(tack_tui::image::image_lines(
                            &png,
                            protocol,
                            image_max_cells.map_or_else(|| (width / 2).max(20), |c| c.max(1)),
                            9,
                            18,
                            pw,
                            ph,
                        ));
                    }
                }
            }
        }
        lines
    }
}

/// First N lines with a "more" hint (collapsed), or everything (expanded).
fn preview_lines(text: &str, expanded: bool, style: Style) -> Vec<Line> {
    const PREVIEW: usize = 10;
    let all: Vec<&str> = text.lines().collect();
    let (shown, remaining) = if !expanded && all.len() > PREVIEW {
        (&all[..PREVIEW], all.len() - PREVIEW)
    } else {
        (&all[..], 0)
    };
    let mut lines: Vec<Line> = shown
        .iter()
        .map(|l| Line::styled((*l).to_string(), style))
        .collect();
    if remaining > 0 {
        lines.push(Line::styled(
            crate::i18n::trf(
                "tool.more_lines_expand",
                &[("remaining", &remaining.to_string())],
            ),
            Style::new().dim(),
        ));
    }
    lines
}

/// Colored unified diff with +/- markers (word-level highlighting lands with
/// the edit tool details; here we color whole lines).
pub fn render_diff(diff: &str, theme: &Theme) -> Vec<Line> {
    diff.lines()
        .map(|line| {
            if line.starts_with("+++") || line.starts_with("---") {
                Line::styled(line.to_string(), Style::new().dim())
            } else if let Some(rest) = line.strip_prefix('+') {
                Line::from_spans(vec![
                    Span::styled("+", theme.diff_added),
                    Span::styled(rest.to_string(), theme.diff_added),
                ])
            } else if let Some(rest) = line.strip_prefix('-') {
                Line::from_spans(vec![
                    Span::styled("-", theme.diff_removed),
                    Span::styled(rest.to_string(), theme.diff_removed),
                ])
            } else {
                Line::styled(line.to_string(), Style::new().dim())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn unknown_tool_title_keeps_small_args_and_summarizes_large() {
        // Small args: byte-identical to the legacy `{name} {json}` display.
        let args = serde_json::json!({"q": "hello", "limit": 3});
        assert_eq!(
            unknown_tool_title("custom", &args),
            format!("custom {}", args)
        );
        // Large args: no full serialization; a string field summarizes.
        let big = serde_json::json!({
            "command": "run-the-thing",
            "payload": "x".repeat(10_000),
        });
        let title = unknown_tool_title("custom", &big);
        assert_eq!(title, "custom run-the-thing");
        // Large args without a known field: bare tool name.
        let big = serde_json::json!({"payload": "x".repeat(10_000)});
        assert_eq!(unknown_tool_title("custom", &big), "custom");
    }

    #[test]
    fn title_is_single_row_for_multiline_args() {
        // Multi-line bash commands / grep patterns: a raw '\n' in the title
        // row would be emitted verbatim and desync the renderer's
        // one-frame-line = one-terminal-row accounting.
        let title = tool_title(
            "bash",
            &serde_json::json!({"command": "ls\npwd\r\necho hi"}),
            80,
        );
        assert!(!title.contains(['\n', '\r']), "{title:?}");
        let title = tool_title("grep", &serde_json::json!({"pattern": "a\nb"}), 80);
        assert!(!title.contains(['\n', '\r']), "{title:?}");
        // JSON-serialized args ("other" tools) escape newlines already.
        let title = tool_title("custom", &serde_json::json!({"q": "a\nb"}), 80);
        assert!(!title.contains(['\n', '\r']), "{title:?}");
    }

    #[test]
    fn title_truncation_follows_terminal_width() {
        let command = "x".repeat(300);
        let args = &serde_json::json!({"command": command});
        let narrow = tool_title("bash", args, 40);
        let wide = tool_title("bash", args, 200);
        assert!(narrow.len() < wide.len(), "{narrow:?} vs {wide:?}");
        assert!(narrow.ends_with('…'));
        assert!(wide.ends_with('…'));
        assert!(!wide.contains("x".repeat(201).as_str()));
        // Title + the 3-col marker must fit the row, ellipsis included —
        // otherwise the frame clips the "…" off the right edge.
        for w in [20u16, 40, 80, 120, 200] {
            let title = tool_title("bash", args, w);
            assert!(
                title.chars().count() + 3 <= w.max(24) as usize,
                "w={w}: {title:?}"
            );
        }
    }

    #[test]
    fn expanded_card_shows_full_command() {
        // Regression: a long bash command was truncated in the title with
        // no way to see it in full — ctrl+o only expanded OUTPUT.
        let command = format!("echo start-{}-end", "y".repeat(200));
        let args = serde_json::json!({ "command": command });
        let entry = ToolEntry {
            tool_call_id: "t1".to_string(),
            tool_name: "bash".to_string(),
            args_fp: args_fingerprint(&args),
            args,
            state: ToolState::Done {
                result: AgentToolResult::text("done"),
                is_error: false,
            },
            expanded: true,
        };
        let lines = entry.render(120, &Theme::dark(), None, None);
        let text: String = lines
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n");
        // The detail rows wrap; the tail proves the FULL command rendered.
        let tail = &command[command.len() - 50..];
        assert!(text.contains(tail), "full command missing: {text:?}");
        // Detail rows concatenate back to the original command (wrapping
        // may eat whitespace at break points — compare squeezed).
        let detail: String = lines[1..lines.len() - 1]
            .iter()
            .map(|l| l.text().trim_start().to_string())
            .collect();
        let squeeze = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(squeeze(&detail), squeeze(&command));

        // Collapsed: command stays truncated, no detail rows.
        let entry = ToolEntry {
            expanded: false,
            ..entry
        };
        let lines = entry.render(120, &Theme::dark(), None, None);
        let text: String = lines
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains(tail), "collapsed leaked command");
    }

    #[test]
    fn preview_collapses_and_expands() {
        let text = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed = preview_lines(&text, false, Style::default());
        assert_eq!(collapsed.len(), 11); // 10 preview + hint
        assert!(collapsed[10].text().contains("10 more lines"));
        let expanded = preview_lines(&text, true, Style::default());
        assert_eq!(expanded.len(), 20);
    }
}
