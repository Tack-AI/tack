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
    /// Phantom blank regions: `(logical_row, count)`, sorted by row.
    ///
    /// When content ABOVE the viewport shrinks with the cut straddling the
    /// viewport top (a streaming thinking block folding into its label at
    /// run end, compaction, ctrl+t), bottom-aligning the shorter frame
    /// would pull rows that already scrolled into the terminal's
    /// append-only scrollback back onto the screen — the "pullback
    /// duplication" a user sees when scrolling up. Splicing `count` blank
    /// rows at the collapse row keeps the retained frame long enough that
    /// the visible window stays anchored: the collapse leaves a blank band
    /// where the folded rows were, the label/summary renders right below
    /// it, and no stale row reappears. The blanks scroll up through the
    /// scrollback naturally as new output appends (no paired reclaim, no
    /// full-viewport repaints); once a region lies entirely above the
    /// window its blanks are safely in the scrollback and the region is
    /// garbage-collected from the retained frame (pure bookkeeping — no
    /// terminal output).
    phantoms: Vec<(usize, usize)>,
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

/// Materialize the candidate frame with phantom blank regions spliced in,
/// so the diff stays index-aligned with the retained previous frame.
/// Region positions are APP-row indices (each region anchors `count`
/// blanks before app row `pos`); they do not count other regions' blanks.
fn splice_phantom_rows(parts: &[&[Line]], phantoms: &[(usize, usize)]) -> Vec<Line> {
    let extra: usize = phantoms.iter().map(|r| r.1).sum();
    let base: usize = parts.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(base + extra);
    let mut src = parts.iter().flat_map(|p| p.iter());
    let mut consumed = 0usize;
    for &(pos, count) in phantoms {
        for line in src.by_ref().take(pos.saturating_sub(consumed)) {
            out.push(line.clone());
            consumed += 1;
        }
        out.extend((0..count).map(|_| Line::new()));
    }
    out.extend(src.cloned());
    out
}

/// Build the renderable rows for a candidate frame: per-row ANSI strings
/// (fingerprint-reused from the previous frame where possible), the
/// fingerprints, the rebuilt-row retention list, and the hardware cursor
/// marker position. Extracted so a collapse that re-anchors phantom
/// regions can rebuild the candidate within the same frame.
/// The renderable form of a candidate frame: per-row ANSI strings, their
/// source fingerprints, the rebuilt-row retention list, and the hardware
/// cursor marker position.
type AnsiRows = (
    Vec<std::sync::Arc<str>>,
    Vec<u64>,
    Vec<(usize, Line)>,
    Option<CursorPosition>,
);

fn build_ansi_rows(
    parts: &[&[Line]],
    previous_fps: &[u64],
    previous_lines: &[std::sync::Arc<str>],
) -> AnsiRows {
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
        // identical ANSI, so reuse it and skip both the ANSI build and the
        // cursor-marker substring scan (an O(bytes) scan over every line
        // per frame otherwise — the dominant per-keystroke cost at large
        // transcripts). Marker lines move the marker with the cursor, so
        // their fingerprint changes whenever the cursor does; a matched
        // marker line keeps the cursor where the previous frame put it,
        // which is still correct.
        if previous_fps.get(row) == Some(&fp) && previous_lines.get(row).is_some() {
            new_lines.push(previous_lines[row].clone());
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
    (new_lines, new_fps, changed, cursor_pos)
}

/// Convert a candidate (spliced) row index to an app-row index by
/// subtracting the phantom blanks sitting before it.
fn app_row_at(phantoms: &[(usize, usize)], spliced_row: usize) -> usize {
    let mut app = spliced_row;
    let mut offset = 0usize;
    for &(pos, count) in phantoms {
        let logical = pos + offset;
        if logical >= spliced_row {
            break;
        }
        app -= count.min(spliced_row - logical);
        offset += count;
    }
    app
}

/// First/last index at which the two frames differ (index-aligned
/// comparison; <0 first when identical). Pointer equality first:
/// unchanged rows reuse the previous frame's Arc, so the common case
/// skips the string compare (a full-transcript memcmp per frame
/// otherwise).
fn first_last_changed(
    previous_lines: &[std::sync::Arc<str>],
    new_lines: &[std::sync::Arc<str>],
) -> (i64, i64) {
    let mut first_changed: i64 = -1;
    let mut last_changed: i64 = -1;
    let max_lines = new_lines.len().max(previous_lines.len());
    for i in 0..max_lines {
        let equal = match (previous_lines.get(i), new_lines.get(i)) {
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
    (first_changed, last_changed)
}

/// Post-collapse realignment check for the phantom splice: after removing
/// `shrink` rows at `c`, the candidate must line back up with the
/// previous frame within a few rows of replacement content (the collapse
/// label, a compaction summary) — verified against at least one real row.
/// Returns the replacement-row count on success. Without a verified
/// realignment the shrink is a wholesale transcript replacement (/new,
/// tree jump, session load), where splicing blanks would leave the fresh
/// content floating above a phantom band instead of anchoring at the top
/// of the screen, so the caller keeps the classic repaint.
fn suffix_realigns(
    new: &[std::sync::Arc<str>],
    prev: &[std::sync::Arc<str>],
    c: usize,
    shrink: usize,
) -> Option<usize> {
    // A collapse at the very first row is indistinguishable from a
    // wholesale replacement — refuse those (thinking blocks always sit
    // below the banner/history in practice).
    if c == 0 {
        return None;
    }
    /// Replacement rows tolerated before the suffix must realign.
    const MAX_REPLACEMENT_ROWS: usize = 80;
    let scan_end = new.len().min(c + MAX_REPLACEMENT_ROWS + 1);
    for a in c..scan_end {
        let run = (new.len() - a).min(3);
        if run == 0 {
            break;
        }
        if a + shrink + run > prev.len() {
            continue;
        }
        if new[a..a + run] == prev[a + shrink..a + shrink + run] {
            return Some(a - c);
        }
    }
    None
}

impl MainScreenRenderer {
    pub fn new() -> Self {
        MainScreenRenderer::default()
    }

    /// Forget all render state (next frame is a first render).
    pub fn reset(&mut self) {
        *self = MainScreenRenderer::default();
    }

    /// Drop phantom regions whose blanks have scrolled entirely above the
    /// window (they live in the terminal's scrollback now). Pure
    /// bookkeeping — the terminal shows the same rows before and after;
    /// only the renderer's absolute row numbering shifts. Keeps the
    /// steady-state frame on the zero-copy parts path and the retained
    /// frame bounded by the real transcript length.
    fn gc_phantoms(&mut self) {
        // Regions anchor app rows; convert to retained-frame rows by
        // counting the blanks of all earlier regions.
        let mut logical = 0usize;
        let mut i = 0;
        while i < self.phantoms.len() {
            let (pos, count) = self.phantoms[i];
            logical += pos;
            if logical + count <= self.previous_viewport_top {
                self.previous_lines.drain(logical..logical + count);
                self.previous_fps.drain(logical..logical + count);
                self.previous_source.drain(logical..logical + count);
                self.previous_viewport_top -= count;
                self.cursor_row = self.cursor_row.saturating_sub(count);
                self.hardware_cursor_row = self.hardware_cursor_row.saturating_sub(count);
                self.max_lines_rendered = self.max_lines_rendered.saturating_sub(count);
                self.phantoms.remove(i);
            } else {
                logical += count;
                i += 1;
            }
        }
    }

    /// Strip every phantom region out of a freshly built candidate frame.
    /// Used by the full-clear paths: the scrollback wipe makes anchoring
    /// moot, and the rewritten frame must not carry the blanks forward.
    fn strip_phantom_rows(
        &mut self,
        new_lines: &mut Vec<std::sync::Arc<str>>,
        new_fps: &mut Vec<u64>,
        changed: &mut Vec<(usize, Line)>,
    ) {
        let regions = std::mem::take(&mut self.phantoms);
        // Candidate rows count earlier regions' blanks; strip in reverse
        // so earlier positions stay valid.
        let mut logical_positions: Vec<usize> = Vec::with_capacity(regions.len());
        let mut blanks_before = 0usize;
        for &(pos, count) in &regions {
            logical_positions.push(pos + blanks_before);
            blanks_before += count;
        }
        for (&logical, &(_, count)) in logical_positions.iter().zip(regions.iter()).rev() {
            new_lines.drain(logical..(logical + count).min(new_lines.len()));
            new_fps.drain(logical..(logical + count).min(new_fps.len()));
            let mut kept: Vec<(usize, Line)> = Vec::with_capacity(changed.len());
            for (row, line) in changed.drain(..) {
                if row < logical {
                    kept.push((row, line));
                } else if row >= logical + count {
                    kept.push((row - count, line));
                }
                // Rows inside the stripped region vanish with it.
            }
            *changed = kept;
        }
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
        self.gc_phantoms();
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
        // Phantom blanks: splice the recorded regions into the candidate
        // frame so it stays index-aligned with the retained previous frame.
        // Only materializes while regions are pending (between a collapse
        // and its blanks scrolling off into the terminal's scrollback);
        // steady-state frames keep the zero-copy parts path. `base_parts`
        // keeps the un-spliced frame for a region re-anchor rebuild.
        let phantom_joined;
        let phantom_slice;
        let base_parts = parts;
        let parts: &[&[Line]] = if self.phantoms.is_empty() {
            base_parts
        } else {
            phantom_joined = splice_phantom_rows(base_parts, &self.phantoms);
            phantom_slice = phantom_joined.as_slice();
            std::slice::from_ref(&phantom_slice)
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

        // Extract the cursor marker, then emit ANSI strings (fingerprint
        // reuse from the previous frame — see build_ansi_rows).
        let (mut new_lines, mut new_fps, mut changed, mut cursor_pos) =
            build_ansi_rows(parts, &self.previous_fps, &self.previous_lines);

        let new_len = new_lines.len();
        // Full re-render helper.
        #[allow(clippy::too_many_arguments)]
        let full_render = |renderer: &mut Self,
                           new_lines: &mut Vec<std::sync::Arc<str>>,
                           new_fps: &mut Vec<u64>,
                           changed: &mut Vec<(usize, Line)>,
                           clear: bool,
                           out: &mut dyn Write,
                           viewport_top: usize,
                           hardware_cursor_row: usize|
         -> std::io::Result<(usize, usize, usize)> {
            let mut buffer = String::from("\x1b[?2026h");
            if clear {
                buffer.push_str("\x1b[2J\x1b[H\x1b[3J"); // clear screen + scrollback
                // Anchoring is moot once the scrollback is wiped; the
                // rewritten frame must not carry the blanks forward.
                renderer.strip_phantom_rows(new_lines, new_fps, changed);
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
                &mut new_lines,
                &mut new_fps,
                &mut changed,
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
                &mut new_lines,
                &mut new_fps,
                &mut changed,
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
                &mut new_lines,
                &mut new_fps,
                &mut changed,
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
        let (mut first_changed, mut last_changed) =
            first_last_changed(&self.previous_lines, &new_lines);

        // Shrink straddling the viewport top (a streaming thinking block
        // folding into its label at run end, compaction, ctrl+t): splice
        // phantom blank rows at the collapse point so the retained frame
        // keeps its length and the on-screen window stays anchored. Rows
        // that already scrolled into the terminal's append-only scrollback
        // are never pulled back onto the screen (the "pullback
        // duplication"); the blanks scroll out through the scrollback
        // naturally as new output appends. Guarded by a suffix-realignment
        // check so wholesale transcript replacements keep the classic
        // repaint. Multi-site shrinks (ctrl+t collapsing several thinking
        // blocks at once) splice one site per frame — later sites show up
        // as a fresh shrink on the next frame.
        if new_lines.len() < self.previous_lines.len()
            && first_changed != -1
            && (first_changed as usize) < prev_viewport_top
        {
            let shrink = self.previous_lines.len() - new_lines.len();
            let mut c = first_changed as usize;
            if let Some(replaced) = suffix_realigns(&new_lines, &self.previous_lines, c, shrink) {
                // `c` is in candidate (spliced) coordinates; region
                // bookkeeping anchors app rows. Convert by subtracting the
                // phantom blanks sitting before `c`.
                let app_c = app_row_at(&self.phantoms, c);
                // Re-anchor existing regions to the new app frame: the
                // collapse removed app rows [app_c, app_c + shrink +
                // replaced). Regions anchoring inside the removed range
                // merge into the new collapse point; regions below it
                // shift up.
                let removed_end = app_c + shrink + replaced;
                let mut moved = false;
                for region in &mut self.phantoms {
                    if region.0 > app_c {
                        moved = true;
                        region.0 = if region.0 < removed_end {
                            app_c
                        } else {
                            region.0 - shrink
                        };
                    }
                }
                self.phantoms.sort_unstable();
                // Blanks needed to keep the collapse label and everything
                // below it at their current screen rows: the pullback
                // amount.
                let pad = (c + height as usize)
                    .saturating_sub(new_lines.len())
                    .min(shrink);
                if moved {
                    // The blanks in the already-built candidate sit at the
                    // pre-collapse positions; rebuild it with the
                    // re-anchored regions so this frame's repaint places
                    // the bands correctly (collapse frames are rare — one
                    // extra candidate build is fine).
                    let rebuilt = splice_phantom_rows(base_parts, &self.phantoms);
                    let (l, f, ch, cp) = build_ansi_rows(
                        std::slice::from_ref(&rebuilt.as_slice()),
                        &self.previous_fps,
                        &self.previous_lines,
                    );
                    new_lines = l;
                    new_fps = f;
                    changed = ch;
                    cursor_pos = cp;
                    (first_changed, last_changed) =
                        first_last_changed(&self.previous_lines, &new_lines);
                    // The collapse's spliced coordinate moved with the
                    // re-anchored bands; the app coordinate is unchanged.
                    c = first_changed as usize;
                }
                if pad > 0 {
                    let blank_fp = line_fingerprint(&Line::new());
                    let blank_ansi: std::sync::Arc<str> = std::sync::Arc::from("");
                    new_lines.splice(c..c, (0..pad).map(|_| blank_ansi.clone()));
                    new_fps.splice(c..c, std::iter::repeat_n(blank_fp, pad));
                    let split = changed.partition_point(|(row, _)| *row < c);
                    for (row, _) in &mut changed[split..] {
                        *row += pad;
                    }
                    changed.splice(split..split, (0..pad).map(|j| (c + j, Line::new())));
                    let at = self.phantoms.partition_point(|r| r.0 < app_c);
                    self.phantoms.insert(at, (app_c, pad));
                    // The suffix below the splice no longer index-aligns
                    // with the previous frame (pad < shrink);
                    // conservatively mark everything to the frame end as
                    // changed. The above-viewport repaint path below only
                    // reads `first_changed` — its stable-suffix scan
                    // bounds the actual repaint to the blank band + label.
                    last_changed = (self.previous_lines.len().max(new_lines.len()) - 1) as i64;
                }
            }
        } else if new_lines.len() > self.previous_lines.len()
            && first_changed != -1
            && (first_changed as usize) < prev_viewport_top
        {
            // Growth above the viewport (ctrl+o expanding old history):
            // shift the regions below the insertion so they stay anchored
            // to their app rows. The already-built candidate carries the
            // blanks at the pre-growth positions — cosmetic only (the
            // insertion is above the window, so is the misplacement).
            let growth = new_lines.len() - self.previous_lines.len();
            let app_c = app_row_at(&self.phantoms, first_changed as usize);
            for region in &mut self.phantoms {
                if region.0 > app_c {
                    region.0 += growth;
                }
            }
            self.phantoms.sort_unstable();
        }
        let new_len = new_lines.len();
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
                        &mut new_lines,
                        &mut new_fps,
                        &mut changed,
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
                        &mut new_lines,
                        &mut new_fps,
                        &mut changed,
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
        // live in the terminal's scrollback. Two shapes survive to here:
        //
        // - A collapse/expansion fully above the viewport (ctrl+t on old
        //   history) leaves every screen row showing the same content at
        //   the same screen row: only the top rows actually differ, so
        //   repaint just those instead of the old full clear
        //   (\x1b[2J\x1b[H\x1b[3J) + whole-frame rewrite — that wiped the
        //   scrollback and visibly flashed the entire screen on terminals
        //   without synchronized output (Termux).
        // - A straddling shrink the phantom splice refused (wholesale
        //   transcript replacement, or a guard mismatch): the window top
        //   moves up and pulled-back rows reappear on screen. That trades
        //   the scrollback/screen duplication for keeping a faithful
        //   scrollback tape — collapses that DO match the guard never
        //   reach here (see the phantom splice above).
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
                &mut new_lines,
                &mut new_fps,
                &mut changed,
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
    fn collapse_straddling_viewport_splices_phantoms_no_pullback() {
        // Termux scenario: a thinking block taller than the visible screen
        // collapses to the one-row "Thought for a while" label. The cut
        // straddles the viewport top, so bottom-aligning the shorter frame
        // would pull rows that already live in the (append-only) scrollback
        // back onto the screen — the pullback duplication. The phantom
        // splice keeps the window anchored instead: the thinking tail on
        // screen is replaced by a blank band + the label, the stable
        // bottom suffix stays untouched, and no history row is rewritten.
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
        // Phantom splice: 19 blanks (rows 60..79 logical) + the label are
        // rewritten (20 rows); the stable suffix (rows 90..99) is not, and
        // no pulled-back history row (41..60) appears.
        assert_eq!(renderer.phantoms, vec![(60, 19)], "phantom region");
        assert!(s.contains("Thought for a while"), "{s:?}");
        assert!(!s.contains("row 41"), "pulled-back row rewritten: {s:?}");
        assert!(!s.contains("row 59"), "pulled-back row rewritten: {s:?}");
        assert!(!s.contains("row 90"), "stable suffix rewritten: {s:?}");
        assert_eq!(
            s.matches("\x1b[2K").count(),
            20,
            "expected exactly the 19 blanks + label repainted: {s:?}"
        );

        // The phantom region keeps later frames index-aligned: an append
        // scrolls one blank into the scrollback (physical scroll path),
        // the rest of the screen stays put.
        let mut newer = numbered_rows(60);
        newer.push(Line::plain("◦ Thought for a while (5400 chars)"));
        newer.extend((90..100).map(|i| Line::plain(format!("row {i}"))));
        newer.push(Line::plain("fresh output"));
        out.clear();
        renderer.render(&newer, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("fresh output"), "{s:?}");
        assert!(!s.contains("\x1b[2J"), "{s:?}");
        assert!(!s.contains("row 41"), "late pullback: {s:?}");
        assert_eq!(renderer.phantoms, vec![(60, 19)], "region persists");
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

    #[test]
    fn phantom_region_is_garbage_collected_once_scrolled_above_window() {
        // After the collapse, each append scrolls one phantom blank into
        // the scrollback. Once the whole region lies above the window it
        // is dropped from the retained frame (pure bookkeeping), and the
        // renderer returns to the zero-copy steady state.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        let old = numbered_rows(100);
        let mut collapsed = numbered_rows(60);
        collapsed.push(Line::plain("◦ Thought for a while (5400 chars)"));
        collapsed.extend((90..100).map(|i| Line::plain(format!("row {i}"))));
        renderer.render(&old, 80, 30, false, &mut out).unwrap();
        renderer
            .render(&collapsed, 80, 30, false, &mut out)
            .unwrap();
        assert_eq!(renderer.phantoms, vec![(60, 19)]);
        // 19 appends flush all 19 blanks above the window; the GC runs at
        // the START of the frame once the region end reaches the window
        // top, so it takes one more frame after the 19th append.
        for k in 0..20usize {
            let mut frame = collapsed.clone();
            frame.extend((0..=k).map(|i| Line::plain(format!("append {i}"))));
            out.clear();
            renderer.render(&frame, 80, 30, false, &mut out).unwrap();
        }
        assert!(
            renderer.phantoms.is_empty(),
            "region not GC'd: {:?}",
            renderer.phantoms
        );
        // Bookkeeping survived the GC: the next append diffs against the
        // bottom instead of repainting the world.
        let mut frame = collapsed.clone();
        frame.extend((0..20).map(|i| Line::plain(format!("append {i}"))));
        frame.push(Line::plain("append 20"));
        out.clear();
        renderer.render(&frame, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("append 20"), "{s:?}");
        assert!(!s.contains("\x1b[2J"), "{s:?}");
        assert!(
            s.len() < 200,
            "expected a small tail diff after GC, got {} bytes",
            s.len()
        );
    }

    #[test]
    fn wholesale_replacement_rejects_phantom_splice() {
        // /new-style replacement: the old transcript vanishes wholesale
        // (nothing realigns) — the splice must NOT fire; the fresh frame
        // anchors at the top of the screen via the classic repaint.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        renderer
            .render(&numbered_rows(100), 80, 30, false, &mut out)
            .unwrap();
        out.clear();
        let fresh = vec![
            Line::plain("welcome to a fresh session"),
            Line::plain(""),
            Line::plain("> "),
        ];
        renderer.render(&fresh, 80, 30, false, &mut out).unwrap();
        assert!(
            renderer.phantoms.is_empty(),
            "splice misfired: {:?}",
            renderer.phantoms
        );
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("welcome to a fresh session"), "{s:?}");
        assert!(!s.contains("\x1b[2J"), "{s:?}");
    }

    #[test]
    fn expansion_above_viewport_shifts_pending_region() {
        // A collapse left a phantom band pending; ctrl+o expanding old
        // history above it must re-anchor the band to its app row.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        let old = numbered_rows(100);
        let mut collapsed = numbered_rows(60);
        collapsed.push(Line::plain("◦ Thought for a while (5400 chars)"));
        collapsed.extend((90..100).map(|i| Line::plain(format!("row {i}"))));
        renderer.render(&old, 80, 30, false, &mut out).unwrap();
        renderer
            .render(&collapsed, 80, 30, false, &mut out)
            .unwrap();
        assert_eq!(renderer.phantoms, vec![(60, 19)]);
        // Expand 20 rows at app row 10 (above the band's anchor).
        let mut expanded = numbered_rows(10);
        expanded.extend((0..20).map(|i| Line::plain(format!("thought {i}"))));
        expanded.extend((10..60).map(|i| Line::plain(format!("row {i}"))));
        expanded.push(Line::plain("◦ Thought for a while (5400 chars)"));
        expanded.extend((90..100).map(|i| Line::plain(format!("row {i}"))));
        out.clear();
        renderer.render(&expanded, 80, 30, false, &mut out).unwrap();
        assert_eq!(
            renderer.phantoms,
            vec![(80, 19)],
            "region shifted by the insertion"
        );
        // Steady-state append afterwards still diffs cleanly.
        let mut appended = expanded.clone();
        appended.push(Line::plain("tail"));
        out.clear();
        renderer.render(&appended, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("tail"), "{s:?}");
        assert!(!s.contains("\x1b[2J"), "{s:?}");
    }

    #[test]
    fn sequential_collapses_splice_independently() {
        // Two thinking blocks collapse in successive frames: each splice
        // anchors its own region; both sets of blanks flow out cleanly.
        let mut renderer = MainScreenRenderer::new();
        let mut out = Vec::new();
        let old = numbered_rows(100);
        renderer.render(&old, 80, 30, false, &mut out).unwrap();
        // First collapse: rows 60..90 → label (as in the straddle test).
        let mut step1 = numbered_rows(60);
        step1.push(Line::plain("◦ Thought A"));
        step1.extend((90..100).map(|i| Line::plain(format!("row {i}"))));
        renderer.render(&step1, 80, 30, false, &mut out).unwrap();
        assert_eq!(renderer.phantoms, vec![(60, 19)]);
        // Second collapse one frame later: rows 20..50 → another label.
        // The cut is fully ABOVE the current window, so no new blank band
        // is needed — the first region simply re-anchors to its label's
        // new app row (60 → 31) and the screen stays exactly put.
        let mut step2 = numbered_rows(20);
        step2.push(Line::plain("◦ Thought B"));
        step2.extend((50..60).map(|i| Line::plain(format!("row {i}"))));
        step2.push(Line::plain("◦ Thought A"));
        step2.extend((90..100).map(|i| Line::plain(format!("row {i}"))));
        out.clear();
        renderer.render(&step2, 80, 30, false, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1b[2J"), "{s:?}");
        assert!(
            !s.contains("row ") && !s.contains("Thought"),
            "fully-above-viewport collapse must not repaint rows: {s:?}"
        );
        assert_eq!(
            renderer.phantoms,
            vec![(31, 19)],
            "first region re-anchored to the label's new app row"
        );
        // And both regions GC as output streams.
        for k in 0..40usize {
            let mut frame = step2.clone();
            frame.extend((0..=k).map(|i| Line::plain(format!("append {i}"))));
            out.clear();
            renderer.render(&frame, 80, 30, false, &mut out).unwrap();
        }
        assert!(
            renderer.phantoms.is_empty(),
            "regions not GC'd: {:?}",
            renderer.phantoms
        );
    }
}
