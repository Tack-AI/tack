//! Alternate-screen renderer: full-frame writes into the alternate screen
//! with synchronized output. Mouse selection / search overlay land in the
//! T4 milestone; this is the frame pipeline they plug into.

use std::io::Write;

use crate::line::Line;
use crate::screen_main::CursorPosition;

#[derive(Debug, Default)]
pub struct AltScreenRenderer {
    previous: Vec<String>,
    stopped: bool,
}

impl AltScreenRenderer {
    pub fn new() -> Self {
        AltScreenRenderer::default()
    }

    /// Forget all render state (next frame is a first render).
    pub fn reset(&mut self) {
        self.previous.clear();
    }

    /// Full-frame render (lines are pre-wrapped to `width`, viewport-clipped
    /// to `height` by the caller/Tui).
    pub fn render(
        &mut self,
        lines: &[Line],
        width: u16,
        height: u16,
        assume_fit: bool,
        out: &mut dyn Write,
    ) -> std::io::Result<Option<CursorPosition>> {
        if self.stopped {
            return Ok(None);
        }
        // Same invariant as the main-screen renderer: overlong lines would
        // auto-wrap and corrupt the alt screen's fixed grid. The width scan
        // is skipped when the caller guarantees `assume_fit`.
        let truncated;
        let lines = if !assume_fit && lines.iter().any(|l| l.width() > width as usize) {
            truncated = lines
                .iter()
                .map(|l| {
                    let mut c = l.clone();
                    c.truncate(width as usize, false);
                    c
                })
                .collect::<Vec<Line>>();
            &truncated[..]
        } else {
            lines
        };
        let mut buffer = String::from("\x1b[?2026h\x1b[H"); // sync begin + home
        let mut cursor_pos = None;
        let max = (height as usize).min(lines.len());
        for (row, line) in lines.iter().take(max).enumerate() {
            let mut ansi = String::new();
            let mut col = 0usize;
            for span in &line.spans {
                if let Some(pos) = span.text.find(crate::component::CURSOR_MARKER) {
                    cursor_pos = Some(CursorPosition {
                        row,
                        col: col + crate::line::grapheme_width(&span.text[..pos]),
                    });
                    let cleaned = span.text.replace(crate::component::CURSOR_MARKER, "");
                    ansi.push_str(&span.style.ansi_prefix());
                    // The marker itself rides in a zero-width raw span; any
                    // styled text around it still gets sanitized.
                    if span.zero_width {
                        ansi.push_str(&cleaned);
                    } else {
                        ansi.push_str(&crate::line::sanitize(&cleaned));
                    }
                    ansi.push_str(span.style.ansi_suffix());
                } else {
                    ansi.push_str(&span.style.ansi_prefix());
                    ansi.push_str(&span.terminal_text());
                    ansi.push_str(span.style.ansi_suffix());
                }
                col += span.width();
            }
            if row > 0 {
                buffer.push_str("\r\n");
            }
            buffer.push_str("\x1b[2K");
            buffer.push_str(&ansi);
        }
        // Clear any leftover rows from a taller previous frame. The cursor
        // sits on the last written row (or row 0 when nothing was written),
        // so an empty frame must clear row 0 before moving down.
        if max == 0 {
            buffer.push_str("\x1b[2K");
        }
        for _ in max..self.previous.len().min(height as usize) {
            buffer.push_str("\r\n\x1b[2K");
        }
        buffer.push_str("\x1b[?2026l");
        if let Some(pos) = cursor_pos {
            buffer.push_str(&format!("\x1b[{};{}H", pos.row + 1, pos.col + 1));
        }
        out.write_all(buffer.as_bytes())?;
        self.previous = lines.iter().take(max).map(|l| l.text()).collect();
        Ok(cursor_pos)
    }

    pub fn stop(&mut self, out: &mut dyn Write) -> std::io::Result<()> {
        self.stopped = true;
        out.write_all(b"\x1b[?2026l")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn shrinking_to_empty_frame_clears_row_zero() {
        // Render a 3-row frame, then an empty frame. After the sync-begin +
        // home the cursor sits ON row 0; the leftover-row sweep used to start
        // with "\r\n", clearing rows 1..=N and leaving row 0 stale forever.
        let mut renderer = AltScreenRenderer::new();
        let mut out = Vec::new();
        let frame = vec![Line::plain("aaa"), Line::plain("bbb"), Line::plain("ccc")];
        renderer.render(&frame, 80, 24, false, &mut out).unwrap();
        out.clear();
        renderer.render(&[], 80, 24, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains("\x1b[H\x1b[2K"),
            "row 0 never cleared (stale content): {s:?}"
        );
    }

    #[test]
    fn shrinking_clears_leftover_rows_below_content() {
        let mut renderer = AltScreenRenderer::new();
        let mut out = Vec::new();
        let tall = vec![Line::plain("a"), Line::plain("b"), Line::plain("c")];
        renderer.render(&tall, 80, 24, false, &mut out).unwrap();
        out.clear();
        renderer
            .render(&[Line::plain("a")], 80, 24, false, &mut out)
            .unwrap();
        let s = String::from_utf8_lossy(&out);
        // Rows 2 and 3 (0-indexed 1..=2) must be cleared: 3 line-clears total
        // (row 0 rewritten, two leftover sweeps).
        assert_eq!(s.matches("\x1b[2K").count(), 3, "{s:?}");
    }
}
