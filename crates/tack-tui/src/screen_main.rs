//! Main-screen renderer: draws into the terminal's normal screen (content
//! stays in scrollback) with differential line updates. Port of
//! `tui-main-screen.ts` minus Kitty image bookkeeping (added in a later
//! milestone).

use std::io::Write;

use crate::component::CURSOR_MARKER;
use crate::line::Line;

/// (row, col) of the hardware cursor marker found in the rendered lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPosition {
    pub row: usize,
    pub col: usize,
}

/// Cheap identity of a rendered line for diff-reuse: span count plus, per
/// span, the Arc data pointer + text length + zero_width + style bits. Two
/// clones of the same cached render share Arc pointers, so unchanged lines
/// are detected without any string work. The pointer hash is only sound
/// while the Arcs stay alive — `previous_source` retains the source `Line`s
/// so a freed Arc's data pointer can never be recycled by the allocator for
/// different same-length content (an ABA false "unchanged" match).
fn line_fingerprint(line: &Line) -> u64 {
    // FxHash (rustc-hash): word-at-a-time rotate-xor-multiply. The
    // fingerprint runs over EVERY line of EVERY frame, so SipHash
    // (DefaultHasher) was measurable at large transcripts. Collision
    // resistance barely matters here: the retained source Lines already
    // make false matches practically impossible (see above).
    const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;
    let mut hash: u64 = 0;
    let mut add = |v: u64| {
        hash = (hash.rotate_left(5) ^ v).wrapping_mul(K);
    };
    add(line.spans.len() as u64);
    for span in &line.spans {
        add(std::sync::Arc::as_ptr(&span.text) as *const u8 as usize as u64);
        add(span.text.len() as u64);
        add(span.zero_width as u64);
        add(style_bits(&span.style));
    }
    hash
}

/// Pack a `Style` (a tiny POD: two optional colors + six attribute bits)
/// into one word for the fingerprint — cheaper than running `Hash` over
/// the struct per span per frame.
fn style_bits(style: &crate::Style) -> u64 {
    // 26 bits: 2-bit tag + payload (8-bit index or 24-bit RGB).
    let color_bits = |c: Option<crate::Color>| -> u64 {
        match c {
            None => 0,
            Some(crate::Color::Default) => 1,
            Some(crate::Color::Indexed(i)) => 2 | ((i as u64) << 2),
            Some(crate::Color::Rgb(r, g, b)) => {
                3 | ((r as u64) << 2) | ((g as u64) << 10) | ((b as u64) << 18)
            }
        }
    };
    let attrs = (style.bold as u64)
        | ((style.italic as u64) << 1)
        | ((style.underline as u64) << 2)
        | ((style.dim as u64) << 3)
        | ((style.inverse as u64) << 4)
        | ((style.strikethrough as u64) << 5);
    attrs | (color_bits(style.fg) << 8) | (color_bits(style.bg) << 36)
}

#[derive(Debug, Default)]
pub struct MainScreenRenderer {
    previous_lines: Vec<std::sync::Arc<str>>,
    /// Per-line fingerprints of the source `Line`s that produced
    /// `previous_lines` (same indices).
    previous_fps: Vec<u64>,
    /// The source `Line`s of the previous frame, retained alongside the
    /// rendered strings so the span `Arc`s stay alive: keeping the
    /// allocations alive is what makes the pointer-based
    /// `line_fingerprint` ABA-safe (see its doc comment).
    previous_source: Vec<Line>,
    previous_width: u16,
    previous_height: u16,
    cursor_row: usize,
    hardware_cursor_row: usize,
    max_lines_rendered: usize,
    previous_viewport_top: usize,
    stopped: bool,
    /// terminal.clearOnShrink: when the terminal gets shorter, clear the
    /// screen fully instead of diffing into the smaller viewport (TS tui).
    pub clear_on_shrink: bool,
    /// Kitty image ids whose pixel data has already been transmitted. Once
    /// the terminal has the bitmap (keyed by id), re-renders only need a
    /// tiny re-placement escape — re-sending megabytes of base64 per frame
    /// is what made streaming crawl with a mermaid diagram on screen.
    kitty_transmitted: std::collections::HashSet<u32>,
}

/// Extract (id, cols, rows) from a Kitty transmit+place escape line
/// (`\x1b_Ga=T,...,i={id},c={cols},r={rows};...`), as emitted by
/// `crate::image::image_lines`.
fn parse_kitty_transmission(line: &str) -> Option<(u32, &str, &str)> {
    let start = line.find("\x1b_Ga=T,")? + "\x1b_Ga=T,".len();
    let rest = &line[start..];
    let end = rest.find(';')?;
    let mut id = None;
    let mut cols = None;
    let mut rows = None;
    for kv in rest[..end].split(',') {
        let Some((key, value)) = kv.split_once('=') else {
            continue;
        };
        match key {
            "i" => id = value.parse().ok(),
            "c" => cols = Some(value),
            "r" => rows = Some(value),
            _ => {}
        }
    }
    Some((id?, cols?, rows?))
}

impl MainScreenRenderer {
    pub fn new() -> Self {
        MainScreenRenderer::default()
    }

    /// Forget all render state (next frame is a first render).
    pub fn reset(&mut self) {
        *self = MainScreenRenderer::default();
    }

    /// Emit a Kitty image line: verbatim the first time (the terminal
    /// receives the pixel data), then as a delete + tiny re-placement — the
    /// bitmap stays in terminal memory keyed by image id.
    fn kitty_line<'a>(&mut self, line: &'a str) -> std::borrow::Cow<'a, str> {
        let Some((id, cols, rows)) = parse_kitty_transmission(line) else {
            return std::borrow::Cow::Borrowed(line);
        };
        if self.kitty_transmitted.insert(id) {
            return std::borrow::Cow::Borrowed(line);
        }
        std::borrow::Cow::Owned(format!(
            "\x1b_Ga=d,d=i,i={id},q=2\x1b\\\x1b_Ga=p,q=2,i={id},c={cols},r={rows}\x1b\\"
        ))
    }

    /// Swap the diff caches to a freshly built frame. `changed` holds a
    /// clone of every row whose ANSI was rebuilt this frame (ascending
    /// row order); fingerprint-matched rows already have an identical
    /// `Line` retained in `previous_source`, so retention costs
    /// O(changed) clones instead of a full-frame `to_vec()` per frame.
    /// The retained source `Line`s keep the span `Arc`s alive: the
    /// pointer-based `line_fingerprint` is only sound while those `Arc`s
    /// can't be freed and their addresses recycled (ABA — see its doc).
    fn update_caches(
        &mut self,
        changed: Vec<(usize, Line)>,
        rendered: Vec<std::sync::Arc<str>>,
        fps: Vec<u64>,
    ) {
        let new_len = rendered.len();
        self.previous_lines = rendered;
        self.previous_fps = fps;
        for (row, line) in changed {
            if row < self.previous_source.len() {
                self.previous_source[row] = line;
            } else {
                // Rows past the old frame length can never match (no
                // previous entry) and arrive in ascending order, so
                // extension is a plain push.
                debug_assert_eq!(row, self.previous_source.len());
                self.previous_source.push(line);
            }
        }
        self.previous_source.truncate(new_len);
    }

    /// Render `lines` (already wrapped to `width`), diffing against the
    /// previous frame. Returns the cursor position if a marker was found.
    pub fn render(
        &mut self,
        lines: &[Line],
        width: u16,
        height: u16,
        assume_fit: bool,
        out: &mut dyn Write,
    ) -> std::io::Result<Option<CursorPosition>> {
        self.render_parts(std::slice::from_ref(&lines), width, height, assume_fit, out)
    }

    /// Render a frame given as concatenated parts. Equivalent to `render`
    /// on the joined lines but skips materializing one contiguous
    /// `Vec<Line>` per frame — regular mode passes the cached transcript
    /// item slices + tail + bottom directly, so a keystroke at a large
    /// transcript costs O(changed rows) instead of O(transcript).
    pub fn render_parts(
        &mut self,
        parts: &[&[Line]],
        width: u16,
        height: u16,
        assume_fit: bool,
        out: &mut dyn Write,
    ) -> std::io::Result<Option<CursorPosition>> {
        // The whole frame — diff bytes AND final cursor positioning — goes
        // out as ONE write. With separate writes a slow terminal displays
        // the hardware cursor at the end of the rewritten region until
        // the cursor-move bytes arrive (Termux + FrameWriter queue: a
        // blinking block at the end of the "Working…" line every frame).
        let mut frame: Vec<u8> = Vec::new();
        let result = self.render_buffered(parts, width, height, assume_fit, &mut frame);
        if result.is_ok() && !frame.is_empty() {
            out.write_all(&frame)?;
        }
        result
    }

    fn render_buffered(
        &mut self,
        parts: &[&[Line]],
        width: u16,
        height: u16,
        assume_fit: bool,
        out: &mut dyn Write,
    ) -> std::io::Result<Option<CursorPosition>> {
        if self.stopped {
            return Ok(None);
        }
        // A zero-height viewport (headless env, terminal squashed to 0 rows)
        // has no visible rows to diff against; rendering would underflow
        // `height as usize - 1` below.
        if height == 0 {
            return Ok(None);
        }
        // Lines wider than the viewport would auto-wrap in the terminal,
        // breaking the renderer's one-frame-line = one-terminal-row
        // accounting (misplaced cursor, stale/duplicated rows, blank bands).
        // Truncate defensively; slice_cells keeps zero-width spans (cursor
        // marker, Kitty/iTerm2 escapes) intact. The width scan is skipped
        // when the caller guarantees `assume_fit`.
        // Truncation needs one owned, contiguous frame, so the parts are
        // joined only on this (rare) defensive path.
        let truncated;
        let truncated_parts;
        let parts: &[&[Line]] = if !assume_fit
            && parts
                .iter()
                .flat_map(|p| p.iter())
                .any(|l| l.width() > width as usize)
        {
            truncated = parts
                .iter()
                .flat_map(|p| p.iter())
                .map(|l| {
                    let mut c = l.clone();
                    c.truncate(width as usize, false);
                    c
                })
                .collect::<Vec<Line>>();
            truncated_parts = [truncated.as_slice()];
            &truncated_parts
        } else {
            parts
        };
        let width_changed = self.previous_width != 0 && self.previous_width != width;
        let height_changed = self.previous_height != 0 && self.previous_height != height;
        let previous_buffer_length = if self.previous_height > 0 {
            self.previous_viewport_top + self.previous_height as usize
        } else {
            height as usize
        };
        let prev_viewport_top = if height_changed {
            previous_buffer_length.saturating_sub(height as usize)
        } else {
            self.previous_viewport_top
        };

        let mut hardware_cursor_row = self.hardware_cursor_row;

        // Extract the cursor marker, then emit ANSI strings. Lines whose
        // fingerprint matches the previous frame reuse the previous ANSI
        // string (an Arc clone) — steady-state frames build ANSI only for
        // changed lines instead of the full transcript.
        let total_len: usize = parts.iter().map(|p| p.len()).sum();
        let mut cursor_pos: Option<CursorPosition> = None;
        let mut new_lines: Vec<std::sync::Arc<str>> = Vec::with_capacity(total_len);
        let mut new_fps: Vec<u64> = Vec::with_capacity(total_len);
        // Rows whose ANSI was rebuilt this frame, each with a clone of its
        // source Line — `update_caches` retains exactly these.
        let mut changed: Vec<(usize, Line)> = Vec::new();
        for (row, line) in parts.iter().flat_map(|p| p.iter()).enumerate() {
            let fp = line_fingerprint(line);
            new_fps.push(fp);
            // Cache hit FIRST: identical source (same span Arcs) renders to
            // identical ANSI, so reuse it and skip both the ANSI build and
            // the cursor-marker substring scan (an O(bytes) scan over every
            // line per frame otherwise — the dominant per-keystroke cost at
            // large transcripts). Marker lines move the marker with the
            // cursor, so their fingerprint changes whenever the cursor does;
            // a matched marker line keeps the cursor where the previous
            // frame put it, which is still correct.
            if self.previous_fps.get(row) == Some(&fp) && self.previous_lines.get(row).is_some() {
                new_lines.push(self.previous_lines[row].clone());
                continue;
            }
            let mut ansi = String::new();
            let mut col = 0usize;
            for span in &line.spans {
                // Find marker inside the span text.
                if let Some(pos) = span.text.find(CURSOR_MARKER) {
                    cursor_pos = Some(CursorPosition {
                        row,
                        col: col + crate::line::grapheme_width(&span.text[..pos]),
                    });
                    let cleaned = span.text.replace(CURSOR_MARKER, "");
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
            changed.push((row, line.clone()));
            new_lines.push(std::sync::Arc::from(ansi));
        }

        let new_len = new_lines.len();
        // Full re-render helper.
        let full_render = |renderer: &mut Self,
                           new_lines: &[std::sync::Arc<str>],
                           clear: bool,
                           out: &mut dyn Write,
                           viewport_top: usize,
                           hardware_cursor_row: usize|
         -> std::io::Result<(usize, usize, usize)> {
            let mut buffer = String::from("\x1b[?2026h");
            if clear {
                buffer.push_str("\x1b[2J\x1b[H\x1b[3J"); // clear screen + scrollback
            }
            for (i, line) in new_lines.iter().enumerate() {
                if i > 0 {
                    buffer.push_str("\r\n");
                }
                let line = renderer.kitty_line(line.as_ref());
                buffer.push_str(&line);
            }
            buffer.push_str("\x1b[?2026l");
            out.write_all(buffer.as_bytes())?;
            let cursor_row = new_lines.len().saturating_sub(1);
            let max_lines = if clear {
                new_lines.len()
            } else {
                renderer.max_lines_rendered.max(new_lines.len())
            };
            let buffer_length = (height as usize).max(new_lines.len());
            let new_viewport_top = buffer_length.saturating_sub(height as usize);
            let _ = (viewport_top, hardware_cursor_row);
            renderer.max_lines_rendered = max_lines;
            renderer.cursor_row = cursor_row;
            renderer.hardware_cursor_row = cursor_row;
            Ok((cursor_row, cursor_row, new_viewport_top))
        };

        // First render: output everything (assumes clean screen).
        if self.previous_lines.is_empty() && !width_changed && !height_changed {
            let (cr, hcr, vtop) = full_render(
                self,
                &new_lines,
                false,
                out,
                prev_viewport_top,
                hardware_cursor_row,
            )?;
            self.update_caches(std::mem::take(&mut changed), new_lines, new_fps);
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = vtop;
            self.cursor_row = cr;
            self.hardware_cursor_row = hcr;
            self.position_cursor(cursor_pos, new_len, height, out)?;
            return Ok(cursor_pos);
        }

        // Width changes: wrapping changes → full re-render.
        if width_changed {
            let (cr, hcr, vtop) = full_render(
                self,
                &new_lines,
                true,
                out,
                prev_viewport_top,
                hardware_cursor_row,
            )?;
            self.update_caches(std::mem::take(&mut changed), new_lines, new_fps);
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = vtop;
            self.cursor_row = cr;
            self.hardware_cursor_row = hcr;
            self.position_cursor(cursor_pos, new_len, height, out)?;
            return Ok(cursor_pos);
        }

        if height_changed {
            // clearOnShrink: shrinking the terminal clears the screen first
            // (avoids diff artifacts when the old viewport no longer fits).
            if self.clear_on_shrink && height < self.previous_height {
                out.write_all(b"\x1b[2J\x1b[H")?;
            }
            let (cr, hcr, vtop) = full_render(
                self,
                &new_lines,
                true,
                out,
                prev_viewport_top,
                hardware_cursor_row,
            )?;
            self.update_caches(std::mem::take(&mut changed), new_lines, new_fps);
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = vtop;
            self.cursor_row = cr;
            self.hardware_cursor_row = hcr;
            self.position_cursor(cursor_pos, new_len, height, out)?;
            return Ok(cursor_pos);
        }

        // Find first/last changed lines.
        let mut first_changed: i64 = -1;
        let mut last_changed: i64 = -1;
        let max_lines = new_lines.len().max(self.previous_lines.len());
        for i in 0..max_lines {
            // Pointer equality first: unchanged rows reuse the previous
            // frame's Arc, so the common case skips the string compare
            // (a full-transcript memcmp per frame otherwise).
            let equal = match (self.previous_lines.get(i), new_lines.get(i)) {
                (Some(old), Some(new)) => std::sync::Arc::ptr_eq(old, new) || old == new,
                (old, new) => {
                    old.map(|s| s.as_ref()).unwrap_or("") == new.map(|s| s.as_ref()).unwrap_or("")
                }
            };
            if !equal {
                if first_changed == -1 {
                    first_changed = i as i64;
                }
                last_changed = i as i64;
            }
        }
        let appended = new_lines.len() > self.previous_lines.len();
        if appended {
            if first_changed == -1 {
                first_changed = self.previous_lines.len() as i64;
            }
            last_changed = new_lines.len() as i64 - 1;
        }
        let append_start =
            appended && first_changed == self.previous_lines.len() as i64 && first_changed > 0;

        // No changes: maybe just move the cursor.
        if first_changed == -1 {
            self.previous_viewport_top = prev_viewport_top;
            self.previous_height = height;
            self.position_cursor(cursor_pos, new_len, height, out)?;
            return Ok(cursor_pos);
        }

        // All changes are deleted lines: clear them without scrolling.
        if first_changed as usize >= new_lines.len() {
            if self.previous_lines.len() > new_lines.len() {
                let mut buffer = String::from("\x1b[?2026h");
                let target_row = new_lines.len().saturating_sub(1);
                if target_row < prev_viewport_top {
                    let (cr, hcr, vtop) = full_render(
                        self,
                        &new_lines,
                        true,
                        out,
                        prev_viewport_top,
                        hardware_cursor_row,
                    )?;
                    self.update_caches(std::mem::take(&mut changed), new_lines, new_fps);
                    self.previous_width = width;
                    self.previous_height = height;
                    self.previous_viewport_top = vtop;
                    self.cursor_row = cr;
                    self.hardware_cursor_row = hcr;
                    self.position_cursor(cursor_pos, new_len, height, out)?;
                    return Ok(cursor_pos);
                }
                let line_diff = target_row as i64 - hardware_cursor_row as i64;
                if line_diff > 0 {
                    buffer.push_str(&format!("\x1b[{line_diff}B"));
                } else if line_diff < 0 {
                    buffer.push_str(&format!("\x1b[{}A", -line_diff));
                }
                buffer.push('\r');
                let extra_lines = self.previous_lines.len() - new_lines.len();
                if extra_lines > height as usize {
                    let (cr, hcr, vtop) = full_render(
                        self,
                        &new_lines,
                        true,
                        out,
                        prev_viewport_top,
                        hardware_cursor_row,
                    )?;
                    self.update_caches(std::mem::take(&mut changed), new_lines, new_fps);
                    self.previous_width = width;
                    self.previous_height = height;
                    self.previous_viewport_top = vtop;
                    self.cursor_row = cr;
                    self.hardware_cursor_row = hcr;
                    self.position_cursor(cursor_pos, new_len, height, out)?;
                    return Ok(cursor_pos);
                }
                let clear_start_offset = if new_lines.is_empty() { 0 } else { 1 };
                if extra_lines > 0 && clear_start_offset > 0 {
                    buffer.push_str(&format!("\x1b[{clear_start_offset}B"));
                }
                for i in 0..extra_lines {
                    buffer.push_str("\r\x1b[2K");
                    if i < extra_lines - 1 {
                        buffer.push_str("\x1b[1B");
                    }
                }
                let move_back = extra_lines.saturating_sub(1) + clear_start_offset;
                if move_back > 0 {
                    buffer.push_str(&format!("\x1b[{move_back}A"));
                }
                buffer.push_str("\x1b[?2026l");
                out.write_all(buffer.as_bytes())?;
                self.cursor_row = target_row;
                self.hardware_cursor_row = target_row;
            }
            self.position_cursor(cursor_pos, new_len, height, out)?;
            self.update_caches(std::mem::take(&mut changed), new_lines, new_fps);
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = prev_viewport_top;
            return Ok(cursor_pos);
        }

        // Changes above the viewport can't be edited in place — those rows
        // live in the terminal's scrollback. But a collapse/expansion up
        // there (a long streaming thought folding into its "Thought for a
        // while" label when the run ends, or ctrl+t on old history) leaves
        // every screen row BELOW the change showing the same content at the
        // same screen row: only the top rows actually differ. Repaint just
        // those, from the top of the screen, instead of the old full clear
        // (\x1b[2J\x1b[H\x1b[3J) + whole-frame rewrite — that wiped the
        // scrollback and visibly flashed the entire screen on terminals
        // without synchronized output (Termux).
        if (first_changed as usize) < prev_viewport_top {
            let h = height as usize;
            let new_viewport_top = new_len.saturating_sub(h);
            // Count bottom screen rows already showing the right content
            // (unchanged rows sitting at the same screen rows).
            let mut stable_suffix = 0usize;
            while stable_suffix < h {
                let screen_row = h - 1 - stable_suffix;
                let old: &str = self
                    .previous_lines
                    .get(prev_viewport_top + screen_row)
                    .map_or("", |s| s.as_ref());
                let new: &str = new_lines
                    .get(new_viewport_top + screen_row)
                    .map_or("", |s| s.as_ref());
                if old == new {
                    stable_suffix += 1;
                } else {
                    break;
                }
            }
            let repaint_rows = h - stable_suffix;
            let mut buffer = String::from("\x1b[?2026h");
            if repaint_rows > 0 {
                buffer.push_str("\x1b[H"); // home: repaint starts at screen row 0
                for screen_row in 0..repaint_rows {
                    if screen_row > 0 {
                        buffer.push_str("\r\n");
                    }
                    buffer.push_str("\x1b[2K");
                    if let Some(line) = new_lines.get(new_viewport_top + screen_row) {
                        let line = self.kitty_line(line.as_ref());
                        buffer.push_str(&line);
                    }
                }
                hardware_cursor_row = new_viewport_top + repaint_rows - 1;
            } else {
                // Nothing on screen changed (the edit is entirely in the
                // scrollback region): the hardware cursor stays put —
                // translate its frame row into the new frame's coordinates.
                hardware_cursor_row = new_viewport_top
                    + hardware_cursor_row
                        .saturating_sub(prev_viewport_top)
                        .min(h - 1);
            }
            buffer.push_str("\x1b[?2026l");
            out.write_all(buffer.as_bytes())?;
            self.cursor_row = new_len.saturating_sub(1);
            self.hardware_cursor_row = hardware_cursor_row;
            self.max_lines_rendered = self.max_lines_rendered.max(new_len);
            self.update_caches(std::mem::take(&mut changed), new_lines, new_fps);
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = new_viewport_top;
            self.position_cursor(cursor_pos, new_len, height, out)?;
            return Ok(cursor_pos);
        }

        // Kitty graphics are anchored to screen rows, not the text grid —
        // any shift of an image's row requires a full re-anchor instead.
        let frame_has_kitty = new_lines.iter().any(|l| l.contains("\x1b_G"));

        // Render changed range only, wrapped in synchronized output.
        let mut buffer = String::from("\x1b[?2026h");
        let prev_viewport_bottom = prev_viewport_top + height as usize - 1;
        let move_target_row = if append_start {
            first_changed as usize - 1
        } else {
            first_changed as usize
        };
        // Kitty graphics must be re-anchored when the screen scrolls; take
        // the full-render path (re-emits delete+transmission at the new row).
        if frame_has_kitty && move_target_row > prev_viewport_bottom {
            let (cr, hcr, vtop) = full_render(
                self,
                &new_lines,
                true,
                out,
                prev_viewport_top,
                hardware_cursor_row,
            )?;
            self.update_caches(std::mem::take(&mut changed), new_lines, new_fps);
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = vtop;
            self.cursor_row = cr;
            self.hardware_cursor_row = hcr;
            self.position_cursor(cursor_pos, new_len, height, out)?;
            return Ok(cursor_pos);
        }
        if move_target_row > prev_viewport_bottom {
            let current_screen_row =
                (hardware_cursor_row.saturating_sub(prev_viewport_top)).min(height as usize - 1);
            let move_to_bottom = height as usize - 1 - current_screen_row;
            if move_to_bottom > 0 {
                buffer.push_str(&format!("\x1b[{move_to_bottom}B"));
            }
            let scroll = move_target_row - prev_viewport_bottom;
            for _ in 0..scroll {
                buffer.push_str("\r\n");
            }

            hardware_cursor_row = move_target_row;
        }

        let line_diff = move_target_row as i64 - hardware_cursor_row as i64;
        if line_diff > 0 {
            buffer.push_str(&format!("\x1b[{line_diff}B"));
        } else if line_diff < 0 {
            buffer.push_str(&format!("\x1b[{}A", -line_diff));
        }
        buffer.push_str(if append_start { "\r\n" } else { "\r" });

        let render_end = (last_changed as usize).min(new_lines.len() - 1);
        for (i, line) in new_lines
            .iter()
            .enumerate()
            .take(render_end + 1)
            .skip(first_changed as usize)
        {
            if i > first_changed as usize {
                buffer.push_str("\r\n");
            }
            // Skip lines unchanged within the rewritten range: clearing and
            // redrawing identical content is invisible on sync-output (2026)
            // terminals but flickers badly on terminals without it (Termux) —
            // e.g. a truecolor image sitting between a streaming thinking
            // block and the prompt. Unchanged index = already correct on
            // screen (growth only ever happens at the bottom).
            if self
                .previous_lines
                .get(i)
                .is_some_and(|old| std::sync::Arc::ptr_eq(old, line) || old == line)
            {
                continue;
            }
            buffer.push_str("\x1b[2K"); // clear the line first
            let line = self.kitty_line(line);
            buffer.push_str(&line);
        }

        // Shrink handling (TS): when the frame got shorter, clear the extra
        // rows below the new end, then move back up.
        let mut final_cursor_row = render_end;
        if self.previous_lines.len() > new_lines.len() {
            if render_end < new_lines.len() - 1 {
                let move_down = new_lines.len() - 1 - render_end;
                buffer.push_str(&format!("\x1b[{move_down}B"));
                final_cursor_row = new_lines.len() - 1;
            }
            let extra = self.previous_lines.len() - new_lines.len();
            for _ in 0..extra {
                buffer.push_str("\r\n\x1b[2K");
            }
            buffer.push_str(&format!("\x1b[{extra}A"));
        }

        buffer.push_str("\x1b[?2026l");
        out.write_all(buffer.as_bytes())?;

        self.cursor_row = new_lines.len().saturating_sub(1);
        self.hardware_cursor_row = final_cursor_row;
        self.max_lines_rendered = self.max_lines_rendered.max(new_lines.len());
        self.update_caches(std::mem::take(&mut changed), new_lines, new_fps);
        self.previous_width = width;
        self.previous_height = height;
        // TS tui-main-screen.ts:538 — the viewport must be recomputed from the
        // final cursor row; carrying a stale value forward drifts one row per
        // growth and eventually duplicates the tail block instead of
        // overwriting it.
        self.previous_viewport_top =
            prev_viewport_top.max(final_cursor_row.saturating_sub(height as usize - 1));
        self.position_cursor(cursor_pos, self.previous_lines.len(), height, out)?;
        Ok(cursor_pos)
    }

    /// Position the hardware cursor at the marker location (screen-relative).
    fn position_cursor(
        &mut self,
        cursor: Option<CursorPosition>,
        _line_count: usize,
        height: u16,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        let Some(pos) = cursor else { return Ok(()) };
        let screen_row = pos.row as i64 - self.previous_viewport_top as i64;
        if !(0..height as i64).contains(&screen_row) {
            return Ok(()); // scrolled off; hide cursor
        }
        let current_screen_row =
            self.hardware_cursor_row as i64 - self.previous_viewport_top as i64;
        let diff = screen_row - current_screen_row;
        let mut buffer = String::new();
        if diff > 0 {
            buffer.push_str(&format!("\x1b[{diff}B"));
        } else if diff < 0 {
            buffer.push_str(&format!("\x1b[{}A", -diff));
        }
        buffer.push('\r');
        if pos.col > 0 {
            buffer.push_str(&format!("\x1b[{}C", pos.col));
        }
        self.hardware_cursor_row = pos.row;
        out.write_all(buffer.as_bytes())
    }

    /// Move past the rendered content and stop (normal exit path: content
    /// stays in scrollback).
    pub fn stop(&mut self, out: &mut dyn Write) -> std::io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        if self.previous_lines.is_empty() {
            return Ok(());
        }
        let target_row = self.previous_lines.len();
        let line_diff = target_row as i64 - self.hardware_cursor_row as i64;
        let mut buffer = String::from(" ");
        if line_diff > 0 {
            buffer.push_str(&format!("\x1b[{line_diff}B"));
        } else if line_diff < 0 {
            buffer.push_str(&format!("\x1b[{}A", -line_diff));
        }
        buffer.push_str("\r\n");
        out.write_all(buffer.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::line::Span;

    /// A frame whose first line is a Kitty transmit+place escape with a
    /// large payload, followed by the reserved (blank) image rows and the
    /// given tail lines — mirrors `crate::image::image_lines` output.
    fn kitty_frame(tail: &[&str]) -> Vec<Line> {
        let payload = "PAYLOAD-".repeat(20_000); // ~160KB stand-in for base64
        let transmission =
            format!("\x1b_Ga=d,d=i,i=7,q=2\x1b\\\x1b_Ga=T,f=100,q=2,i=7,c=40,r=10;{payload}\x1b\\");
        let mut lines = vec![Line::from_spans(vec![Span::raw(transmission)])];
        for _ in 1..10 {
            lines.push(Line::new()); // reserved image rows
        }
        lines.extend(tail.iter().map(|t| Line::plain(*t)));
        lines
    }

    #[test]
    fn kitty_rerender_replaces_payload_with_tiny_placement() {
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        renderer
            .render(&kitty_frame(&["first"]), 80, 5, false, &mut out)
            .unwrap();
        let first = String::from_utf8_lossy(&out);
        assert!(first.contains("a=T"), "first render transmits: {first:?}");
        assert!(first.contains("PAYLOAD-"));

        // Any full re-render (here: width change) must re-anchor the image
        // with a cheap placement, not re-transmit the pixel data.
        out.clear();
        renderer
            .render(&kitty_frame(&["first"]), 120, 5, false, &mut out)
            .unwrap();
        let second = String::from_utf8_lossy(&out);
        assert!(
            !second.contains("PAYLOAD-"),
            "re-render re-transmitted the payload"
        );
        assert!(
            second.contains("\x1b_Ga=d,d=i,i=7,q=2\x1b\\\x1b_Ga=p,q=2,i=7,c=40,r=10\x1b\\"),
            "re-render missing delete+placement: {second:?}"
        );

        // And again on the next full re-render.
        out.clear();
        renderer
            .render(&kitty_frame(&["first"]), 100, 5, false, &mut out)
            .unwrap();
        let third = String::from_utf8_lossy(&out);
        assert!(
            !third.contains("PAYLOAD-"),
            "third render re-transmitted the payload"
        );
    }

    #[test]
    fn injected_escape_sequences_are_stripped_from_span_text() {
        // Model/tool output must never reach the terminal verbatim: an OSC
        // 52 clipboard write or cursor-movement CSI inside a styled span is
        // stripped, while intentional ANSI in zero-width raw spans
        // (hyperlinks, image protocols) passes through.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        let link = "\x1b]8;;https://example.com\x07link\x1b]8;;\x07";
        let frame = vec![
            Line::plain("safe\x1b]52;c;aGVsbG8=\x07\x1b[2Atext"),
            Line::from_spans(vec![Span::raw(link)]),
        ];
        renderer.render(&frame, 80, 24, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1b]52"), "OSC 52 leaked: {s:?}");
        assert!(!s.contains("\x1b[2A"), "cursor movement leaked: {s:?}");
        assert!(s.contains("safe]52;c;aGVsbG8=[2Atext"), "{s:?}");
        assert!(s.contains(link), "raw span was filtered: {s:?}");
    }

    #[test]
    fn fingerprint_cache_retains_source_lines() {
        // ABA safety: the diff cache must keep the previous frame's source
        // Lines alive. Otherwise a freed span Arc's data pointer can be
        // recycled by the allocator for different same-length content, the
        // pointer-based fingerprint false-matches, and a stale line stays
        // painted.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        let frame = vec![Line::plain("aaaa")];
        renderer.render(&frame, 80, 24, false, &mut out).unwrap();
        assert_eq!(renderer.previous_source, frame, "source not retained");
        drop(frame);
        // Same length + style, different content, freshly allocated (the
        // freed "aaaa" allocation is a prime reuse candidate): the line
        // must still be detected as changed and repainted.
        out.clear();
        let frame2 = vec![Line::plain("bbbb")];
        renderer.render(&frame2, 80, 24, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("bbbb"), "stale line was reused: {s:?}");
    }

    #[test]
    fn kitty_reset_forgets_transmissions() {
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        renderer
            .render(&kitty_frame(&["first"]), 80, 5, false, &mut out)
            .unwrap();
        renderer.reset();
        out.clear();
        renderer
            .render(&kitty_frame(&["first"]), 80, 5, false, &mut out)
            .unwrap();
        let frame = String::from_utf8_lossy(&out);
        assert!(
            frame.contains("PAYLOAD-"),
            "reset must re-transmit: {frame:?}"
        );
    }

    #[test]
    fn render_parts_matches_joined_render() {
        // The parts entry point must produce byte-identical output to
        // rendering the joined frame — regular mode renders through it.
        let chunks: Vec<Vec<Line>> = vec![
            vec![Line::plain("header"), Line::plain("body one")],
            vec![],
            vec![Line::plain("footer"), Line::plain("prompt> ")],
        ];
        let joined: Vec<Line> = chunks.iter().flat_map(|c| c.iter().cloned()).collect();
        let parts: Vec<&[Line]> = chunks.iter().map(|c| c.as_slice()).collect();

        let mut a = MainScreenRenderer::new();
        let mut out_a = Vec::new();
        a.render_parts(&parts, 80, 24, false, &mut out_a).unwrap();
        let mut b = MainScreenRenderer::new();
        let mut out_b = Vec::new();
        b.render(&joined, 80, 24, false, &mut out_b).unwrap();
        assert_eq!(out_a, out_b, "first frame diverged");

        // Change only the last line: both paths emit the same small diff.
        let chunks2: Vec<Vec<Line>> = vec![
            chunks[0].clone(),
            vec![],
            vec![Line::plain("footer"), Line::plain("prompt> x")],
        ];
        let joined2: Vec<Line> = chunks2.iter().flat_map(|c| c.iter().cloned()).collect();
        let parts2: Vec<&[Line]> = chunks2.iter().map(|c| c.as_slice()).collect();
        out_a.clear();
        out_b.clear();
        a.render_parts(&parts2, 80, 24, false, &mut out_a).unwrap();
        b.render(&joined2, 80, 24, false, &mut out_b).unwrap();
        assert_eq!(out_a, out_b, "diff frame diverged");
        assert!(
            out_a.len() < 200,
            "expected a small tail diff, got {} bytes",
            out_a.len()
        );
    }

    #[test]
    fn identical_frame_repaints_nothing() {
        // Steady-state keystroke: a frame whose lines all fingerprint-match
        // the previous render must emit no bytes at all.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        let frame = vec![Line::plain("line one"), Line::plain("line two")];
        renderer.render(&frame, 80, 24, false, &mut out).unwrap();
        out.clear();
        renderer.render(&frame, 80, 24, false, &mut out).unwrap();
        assert!(out.is_empty(), "unchanged frame repainted: {out:?}");
    }

    /// Numbered-rows frame helper: `Line::plain("row {i}")` for i in 0..n.
    fn numbered_rows(n: usize) -> Vec<Line> {
        (0..n).map(|i| Line::plain(format!("row {i}"))).collect()
    }

    #[test]
    fn collapse_straddling_viewport_repaints_only_changed_rows() {
        // Termux scenario: a thinking block taller than the visible screen
        // collapses to the one-row "Thought for a while" label. Old
        // behavior: \x1b[2J\x1b[H\x1b[3J + a rewrite of every frame row — a
        // visible full-screen flash (no synchronized output on Termux) that
        // also wiped the scrollback. Only the screen rows whose content
        // actually changed (revealed history + the label) need a repaint;
        // the stable bottom suffix stays untouched.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        // Old frame: 100 rows; the expanded thinking occupies rows 60..90,
        // so the viewport (rows 70..100) shows its tail + 10 stable rows.
        let old = numbered_rows(100);
        // New frame: rows 60..90 replaced by a single label row → 71 rows.
        let mut new = numbered_rows(60);
        new.push(Line::plain("◦ Thought for a while (5400 chars)"));
        new.extend((90..100).map(|i| Line::plain(format!("row {i}"))));
        renderer.render(&old, 80, 30, false, &mut out).unwrap();
        out.clear();
        renderer.render(&new, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1b[2J"), "full screen clear: {s:?}");
        assert!(!s.contains("\x1b[3J"), "scrollback wipe: {s:?}");
        // New viewport top = 71 - 30 = 41: revealed history rows 41..60 and
        // the label are rewritten (20 rows); nothing else.
        assert!(s.contains("row 41"), "revealed history row missing: {s:?}");
        assert!(s.contains("row 59"), "revealed history row missing: {s:?}");
        assert!(s.contains("Thought for a while"), "{s:?}");
        assert!(!s.contains("row 90"), "stable suffix rewritten: {s:?}");
        assert!(!s.contains("row 40"), "above-viewport row rewritten: {s:?}");
        assert_eq!(
            s.matches("\x1b[2K").count(),
            20,
            "expected exactly the 20 changed rows repainted: {s:?}"
        );
    }

    #[test]
    fn collapse_above_viewport_writes_nothing() {
        // A thinking block entirely above the viewport collapses (ctrl+t on
        // old history): every visible row keeps showing the same content,
        // so the frame must produce no row writes at all — and no clear.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        let old = numbered_rows(100);
        // Collapse rows 10..40 (29 rows removed); the viewport's content
        // (old rows 70..100) reappears unchanged at frame rows 41..71.
        let mut new = numbered_rows(10);
        new.push(Line::plain("◦ Thought for a while (2000 chars)"));
        new.extend((40..100).map(|i| Line::plain(format!("row {i}"))));
        renderer.render(&old, 80, 30, false, &mut out).unwrap();
        out.clear();
        renderer.render(&new, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1b[2J"), "full screen clear: {s:?}");
        assert!(!s.contains("\x1b[3J"), "scrollback wipe: {s:?}");
        assert!(!s.contains("row "), "visible rows rewritten: {s:?}");
        // Cursor bookkeeping must survive the no-op frame: a follow-up
        // append renders differentially at the bottom.
        let mut newer = new.clone();
        newer.push(Line::plain("tail"));
        out.clear();
        renderer.render(&newer, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("tail"), "{s:?}");
        assert!(!s.contains("\x1b[2J"), "{s:?}");
    }

    #[test]
    fn expand_above_viewport_writes_nothing() {
        // Growing the frame above the viewport (ctrl+t expanding old
        // history) shifts frame indices but leaves every visible row's
        // content unchanged — nothing to repaint, nothing to clear.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        let old = numbered_rows(100);
        let mut new = numbered_rows(10);
        new.extend((0..20).map(|i| Line::plain(format!("thought {i}"))));
        new.extend((10..100).map(|i| Line::plain(format!("row {i}"))));
        renderer.render(&old, 80, 30, false, &mut out).unwrap();
        out.clear();
        renderer.render(&new, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1b[2J"), "full screen clear: {s:?}");
        assert!(!s.contains("row "), "visible rows rewritten: {s:?}");
    }

    #[test]
    fn collapse_below_screen_height_repaints_without_clear() {
        // The collapse shrinks the frame to less than one screen: the whole
        // viewport is repainted (rows beyond the new frame end are blanked)
        // but still without clearing the screen or the scrollback.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        let old = numbered_rows(100);
        let mut new = vec![Line::plain("◦ Thought for a while (9000 chars)")];
        new.extend((90..100).map(|i| Line::plain(format!("row {i}"))));
        renderer.render(&old, 80, 30, false, &mut out).unwrap();
        out.clear();
        renderer.render(&new, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1b[2J"), "full screen clear: {s:?}");
        assert!(!s.contains("\x1b[3J"), "scrollback wipe: {s:?}");
        assert!(s.contains("Thought for a while"), "{s:?}");
        assert!(s.contains("row 90"), "{s:?}");
        // 30 rows rewritten: 11 content rows + 19 blanked rows.
        assert_eq!(s.matches("\x1b[2K").count(), 30, "{s:?}");
        // Follow-up append still diffs cleanly below the short frame.
        let mut newer = new.clone();
        newer.push(Line::plain("tail"));
        out.clear();
        renderer.render(&newer, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("tail"), "{s:?}");
        assert!(!s.contains("\x1b[2J"), "{s:?}");
    }
}
