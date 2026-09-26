//! Rendering pipeline (transcript materialization, caches, offsets).
//! Inherent `impl TuiApp` split out of `mod.rs` — pure code move, no
//! behavior change.

use super::*;

/// Transcript line-cache entry for one chat item. The rendered lines may
/// be evicted under memory pressure (fullscreen mode only — regular mode
/// materializes every part every frame), while the height survives so
/// `compute_offsets` never re-renders an evicted entry just to lay out.
#[derive(Clone, Default)]
pub(crate) struct LineCacheEntry {
    /// (width, lines, fits) — None before the first render or after
    /// eviction; a miss re-renders on demand.
    render: Option<(u16, Vec<Line>, bool)>,
    /// (width, height) — survives eviction.
    height: Option<(u16, usize)>,
}

impl LineCacheEntry {
    pub(crate) fn invalidate(&mut self) {
        *self = LineCacheEntry::default();
    }
}

/// Minimum interval between full markdown re-parses of the streaming
/// partial (small deltas reuse the previous render between re-parses).
const STREAM_RENDER_MIN_INTERVAL: Duration = Duration::from_millis(120);
/// ...unless at least this many new content bytes arrived — large jumps
/// (tool arguments, pasted blocks) must not lag behind the throttle.
const STREAM_RENDER_MIN_BYTES: usize = 512;

/// Cheap size/structure fingerprint of the streaming partial: total
/// content bytes + block count. Drives the re-parse throttle without
/// touching the markdown itself.
fn streaming_fingerprint(a: &tack_ai::AssistantMessage) -> (usize, usize) {
    let bytes = a
        .content
        .iter()
        .map(|b| match b {
            tack_ai::ContentBlock::Text { text, .. } => text.len(),
            tack_ai::ContentBlock::Thinking { thinking, .. } => thinking.len(),
            // Serializing tool args per frame would defeat the throttle;
            // a rough size is enough for a heuristic.
            tack_ai::ContentBlock::ToolCall { arguments, .. } => {
                arguments.as_str().map(str::len).unwrap_or(64)
            }
            tack_ai::ContentBlock::Image { data, .. } => data.len(),
        })
        .sum();
    (bytes, a.content.len())
}

/// Re-parse the streaming partial now? Immediately on structural change
/// (block count differs — includes the first frame), otherwise throttled
/// to STREAM_RENDER_MIN_INTERVAL, with a byte-delta escape hatch.
pub(crate) fn stream_render_due(
    mark: Option<(usize, usize, Instant)>,
    bytes: usize,
    blocks: usize,
) -> bool {
    let Some((last_bytes, last_blocks, at)) = mark else {
        return true;
    };
    blocks != last_blocks
        || bytes.abs_diff(last_bytes) >= STREAM_RENDER_MIN_BYTES
        || at.elapsed() >= STREAM_RENDER_MIN_INTERVAL
}

/// Fullscreen cache bounds (memory vs re-render churn). Rendered transcript
/// lines more than this many lines outside the visible window are dropped
/// from the caches (they re-render on demand when scrolled back into view).
const CACHE_KEEP_MARGIN_LINES: usize = 2_000;
/// Hard caps on rendered lines held by the transcript caches. At a few
/// hundred bytes per styled line these bound cache memory at ~10-20 MB
/// regardless of session length.
const LINE_CACHE_BUDGET_LINES: usize = 30_000;
const TOOL_CACHE_BUDGET_LINES: usize = 30_000;

/// Evict chat-entry renders outside `keep` (offsets) or over the line
/// budget (farthest from the window first). Heights survive eviction, so
/// `compute_offsets` stays O(1) per item and a later miss simply
/// re-renders. Returns the number of entries evicted.
pub(crate) fn evict_line_cache(
    cache: &mut [LineCacheEntry],
    spans: &[(usize, usize)],
    keep: (usize, usize),
    budget_lines: usize,
) -> usize {
    let mut evicted = 0;
    for (i, entry) in cache.iter_mut().enumerate() {
        let Some(&(start, end)) = spans.get(i) else {
            break;
        };
        if entry.render.is_some() && (end <= keep.0 || start >= keep.1) {
            entry.render = None;
            evicted += 1;
        }
    }
    let mut total: usize = cache
        .iter()
        .filter_map(|e| e.render.as_ref().map(|(_, lines, _)| lines.len()))
        .sum();
    if total > budget_lines {
        // Farthest from the window first (distance 0 = inside keep; those
        // go last, ties broken by distance from the window CENTER so the
        // middle of an over-budget window survives).
        let center = (keep.0 + keep.1) / 2;
        let mut rendered: Vec<usize> = (0..cache.len())
            .filter(|&i| cache[i].render.is_some())
            .collect();
        rendered.sort_by_key(|&i| {
            let (start, end) = spans.get(i).copied().unwrap_or((0, 0));
            let outside = if start >= keep.1 {
                start - keep.1
            } else {
                keep.0.saturating_sub(end)
            };
            (outside, ((start + end) / 2).abs_diff(center))
        });
        for &i in rendered.iter().rev() {
            if total <= budget_lines {
                break;
            }
            if let Some((_, lines, _)) = cache[i].render.take() {
                total -= lines.len();
                evicted += 1;
            }
        }
    }
    evicted
}

/// Evict tool-card renders outside `keep` or over the line budget.
/// `spans` is (tool id, start, end) per tool item in the transcript.
/// Heights live in the separate `tool_heights` map and survive; a miss
/// re-renders on demand. Returns the number of entries evicted.
pub(crate) fn evict_tool_cache(
    cache: &mut std::collections::HashMap<String, (u16, bool, u64, Vec<Line>, bool)>,
    spans: &[(&str, usize, usize)],
    keep: (usize, usize),
    budget_lines: usize,
) -> usize {
    let mut evicted = 0;
    for (id, start, end) in spans {
        if (*end <= keep.0 || *start >= keep.1) && cache.remove(*id).is_some() {
            evicted += 1;
        }
    }
    let mut total: usize = cache.values().map(|(_, _, _, lines, _)| lines.len()).sum();
    if total > budget_lines {
        let mut by_distance: Vec<&(&str, usize, usize)> = spans
            .iter()
            .filter(|(id, _, _)| cache.contains_key(*id))
            .collect();
        let center = (keep.0 + keep.1) / 2;
        by_distance.sort_by_key(|(_, start, end)| {
            let outside = if *start >= keep.1 {
                *start - keep.1
            } else {
                keep.0.saturating_sub(*end)
            };
            (outside, ((*start + *end) / 2).abs_diff(center))
        });
        for (id, _, _) in by_distance.into_iter().rev() {
            if total <= budget_lines {
                break;
            }
            if let Some((_, _, _, lines, _)) = cache.remove(*id) {
                total -= lines.len();
                evicted += 1;
            }
        }
    }
    evicted
}

impl TuiApp {
    /// Compose the frame: transcript + streaming + status + dialog + editor +
    /// footer. In fullscreen mode the transcript goes through the scroll
    /// viewport and the frame fills exactly the screen.
    pub(crate) fn media(&self) -> chat::Media {
        chat::Media {
            image_protocol: self.image_protocol,
            mermaid: self.mermaid_enabled,
            hide_thinking: self.settings.hide_thinking_block,
            expand_thinking: self.thinking_expanded,
            image_width_cells: self.settings.image_width_cells,
        }
    }

    /// All lines fit within `width` (no defensive truncation needed).
    fn lines_fit(lines: &[Line], width: u16) -> bool {
        lines.iter().all(|l| l.width() <= width as usize)
    }

    /// Rendered lines of the streaming partial at `width` (borrowed — no
    /// per-frame clone of a potentially huge Vec). Empty when not
    /// streaming or when the cached render is stale for this frame.
    fn streaming_lines(&self, width: u16) -> &[Line] {
        if self.streaming.is_none() {
            return &[];
        }
        // Stale rev reuses the cached render (throttled re-parses) — see
        // the regular-mode parts assembly for the full rationale.
        match &self.stream_render {
            Some((_, w, lines, _)) if *w == width => lines,
            _ => &[],
        }
    }

    /// Ensure `line_cache[i]` holds a current render for a chat entry
    /// (renders on miss/eviction; keeps the surviving height in sync).
    fn ensure_chat_render(&mut self, i: usize, width: u16) {
        if matches!(&self.line_cache[i].render, Some((w, _, _)) if *w == width) {
            return;
        }
        let chat::TranscriptItem::Chat(entry) = &self.items[i] else {
            return;
        };
        let media = self.media();
        let rendered = entry.render(width, &self.theme, media);
        let fits = Self::lines_fit(&rendered, width);
        let height = rendered.len();
        self.line_cache[i] = LineCacheEntry {
            render: Some((width, rendered, fits)),
            height: Some((width, height)),
        };
    }

    /// Render a tool card with caching (B): completed cards are immutable;
    /// running cards are keyed by partial-output length (output only
    /// appends). Args are NOT part of the key — they stream in before any
    /// output exists, so mutation sites (run.rs) invalidate the entry
    /// explicitly. The cache key includes width and the expanded flag.
    fn render_tool_cached(&mut self, id: &str, width: u16) -> (Vec<Line>, bool) {
        let Some(tool) = self.tools.get(id) else {
            return (Vec::new(), true);
        };
        let fingerprint: u64 = match &tool.state {
            tool_render::ToolState::Running { partial_output } => partial_output.len() as u64,
            tool_render::ToolState::Done { .. } => u64::MAX,
        };
        let expanded = tool.expanded;
        if let Some((w, e, fp, lines, cached_fits)) = self.tool_render_cache.get(id)
            && *w == width
            && *e == expanded
            && *fp == fingerprint
        {
            return (lines.clone(), *cached_fits);
        }
        let lines = tool.render(
            width,
            &self.theme,
            self.image_protocol,
            self.settings.image_width_cells,
        );
        let fits = Self::lines_fit(&lines, width);
        self.tool_heights
            .insert(id.to_string(), (width, expanded, fingerprint, lines.len()));
        self.tool_render_cache.insert(
            id.to_string(),
            (width, expanded, fingerprint, lines.clone(), fits),
        );
        (lines, fits)
    }

    pub fn render(&mut self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        let (width, height) = self.tui.size();
        let mut fits = true;

        // Pass 1: per-item offsets/heights (cheap — all renders are cached;
        // this also refreshes any stale cache entry). Enables windowed
        // materialization in fullscreen.
        self.compute_offsets(width);

        // Tail sections: streaming partial, status, todo panel (small, and
        // always at the end of the transcript). The streaming render itself
        // is BORROWED at frame-assembly time (streaming_lines) instead of
        // being cloned into `tail` per frame.
        let mut tail: Vec<Line> = Vec::new();
        if let Some(streaming) = &self.streaming {
            let stale = !matches!(
                &self.stream_render,
                Some((rev, w, _, _)) if *rev == self.stream_rev && *w == width
            );
            if stale {
                let (bytes, blocks) = streaming_fingerprint(streaming);
                // A width change must re-wrap immediately; content deltas
                // are throttled (stream_render_due) — the cached render is
                // reused for a frame or two between re-parses.
                let width_changed =
                    !matches!(&self.stream_render, Some((_, w, _, _)) if *w == width);
                if width_changed || stream_render_due(self.stream_render_mark, bytes, blocks) {
                    let media = self.media();
                    let rendered = chat::render_assistant_streaming(
                        streaming,
                        width,
                        &self.theme,
                        media,
                        &mut self.stream_md_cache,
                    );
                    let fits = Self::lines_fit(&rendered, width);
                    self.stream_render = Some((self.stream_rev, width, rendered, fits));
                    self.stream_render_mark = Some((bytes, blocks, Instant::now()));
                }
            }
            if let Some((_, _, _, item_fits)) = &self.stream_render {
                fits &= *item_fits;
            }
        } else {
            self.stream_render_mark = None;
            self.stream_md_cache.clear();
        }
        // Status indicator (spinner); plugin-provided label when idle.
        if let Some(status) = &mut self.status {
            let rendered = status.render(width, &self.theme);
            fits &= Self::lines_fit(&rendered, width);
            tail.extend(rendered);
        } else if let Some(label) = &self.ext_label {
            tail.push(Line::styled(format!(" {label}"), self.theme.muted));
        }
        // rpiv-todo panel: persistent task list above the editor.
        if let Ok(todo) = self.todo_state.try_lock()
            && !todo.is_empty()
        {
            tail.push(Line::styled(
                crate::i18n::tr("panel.todos"),
                self.theme.accent.bold(),
            ));
            for item in &todo.items {
                let (marker, style) = match item.status.as_str() {
                    "done" => ("☑", self.theme.muted),
                    "in_progress" => ("◐", self.theme.warning),
                    _ => ("☐", self.theme.text),
                };
                tail.push(Line::styled(
                    format!("  {marker} #{} {}", item.id, item.text),
                    style,
                ));
            }
        }
        // tack-ext declarative panels (v2.1): plugin-declared markdown/list
        // panels, host-rendered between the transcript and the editor.
        #[cfg(feature = "ext")]
        {
            let ext_panel_lines = self.render_ext_panels(width);
            fits &= Self::lines_fit(&ext_panel_lines, width);
            tail.extend(ext_panel_lines);
        }

        // Bottom sections (dialog/autocomplete/editor/footer) — needed up
        // front to size the fullscreen viewport.
        let mut bottom: Vec<Line> = Vec::new();
        if let Some(dialog) = &mut self.dialog {
            bottom.push(Line::new());
            bottom.extend(dialog.render(width, &self.theme));
        }
        if let Some(auto) = &mut self.autocomplete {
            bottom.extend(auto.list.render(width));
        }
        // Ctrl+R history reverse search bar (above the editor).
        if let Some(search) = &self.history_search {
            bottom.push(search.bar_line(&self.theme));
        }
        // editorPaddingX: horizontal padding around the editor (TS: 0-3).
        let pad_x = self.settings.editor_padding_x.min(width / 4);
        let mut editor_lines = self.editor.render(width.saturating_sub(pad_x * 2));
        if pad_x > 0 {
            for line in &mut editor_lines {
                line.spans
                    .insert(0, Span::plain(" ".repeat(pad_x as usize)));
            }
        }
        bottom.extend(editor_lines);

        // During a run the SessionManager is parked inside the run task and
        // `state.session` is an empty placeholder — show last-known stats
        // instead of dropping to zeros (TS shows live stats throughout).
        let stats = if self.running {
            &self.last_stats
        } else {
            // Recompute only when the session changed: session_totals() +
            // build_session_context() deep-clone every entry (messages, tool
            // outputs) several times over, which at large contexts takes
            // hundreds of ms — per keystroke, if done unconditionally.
            let key = (
                self.state.session.session_id().to_string(),
                self.state.session.revision(),
            );
            if self.stats_key.as_ref() != Some(&key) {
                // Session switch (new/resume/fork): sampling usage accrued
                // against the old session id must not leak into the new one.
                if self.stats_key.as_ref().map(|(id, _)| id) != Some(&key.0) {
                    self.mcp_sampling_stats = footer::FooterStats::default();
                }
                let totals = self.state.session.session_totals();
                let context = self.state.session.build_session_context();
                self.last_stats = self.stats_with_sampling(FooterStats {
                    input: totals.input,
                    output: totals.output,
                    cache_read: totals.cache_read,
                    cache_write: totals.cache_write,
                    cost: totals.cost.total,
                    context_tokens: tack_session::estimate_context_tokens(&context.messages).tokens,
                });
                self.stats_key = Some(key);
            }
            &self.last_stats
        };
        let session_name = None; // from last SessionInfo; cheap enough to skip per frame
        // tack-ext status-line segments (v2.1): priority order, theme styles.
        #[cfg(feature = "ext")]
        let ext_segments: Vec<(String, Style)> =
            widgets::status_segments(self.extensions.widgets())
                .into_iter()
                .map(|(text, style)| (text, widgets::status_style(style, &self.theme)))
                .collect();
        #[cfg(not(feature = "ext"))]
        let ext_segments: Vec<(String, Style)> = Vec::new();
        let footer_lines = footer::render_footer(
            &self.cwd,
            &self.state.model,
            self.state.thinking,
            stats,
            session_name,
            self.update_hint.as_deref(),
            lock_recover(&self.mode).as_str(),
            &ext_segments,
            width,
            &self.theme,
        );
        bottom.extend(footer_lines);
        fits &= Self::lines_fit(&bottom, width);

        if !self.fullscreen {
            // Regular mode: hand the renderer the frame as BORROWED PARTS
            // — the cached per-item transcript slices plus tail/bottom —
            // instead of materializing one contiguous Vec<Line> (and
            // cloning it twice more for last_frame + renderer retention)
            // per frame. With the renderer's fingerprint reuse this makes
            // a keystroke at a large transcript O(changed rows) instead
            // of O(transcript).
            fits &= Self::lines_fit(&tail, width);
            let debug_capture = std::mem::take(&mut self.debug_capture);
            let mut joined: Option<Vec<Line>> = None;
            let render_result = {
                // Entries evicted while in fullscreen (or never rendered at
                // this width) re-render on demand; compute_offsets keeps
                // only heights for those.
                for i in 0..self.items.len() {
                    if matches!(self.items[i], chat::TranscriptItem::Chat(_)) {
                        self.ensure_chat_render(i, width);
                    }
                }
                let mut parts: Vec<&[Line]> = Vec::with_capacity(self.items.len() + 3);
                for (i, item) in self.items.iter().enumerate() {
                    match item {
                        chat::TranscriptItem::Chat(_) => {
                            if let Some((_, lines, item_fits)) = &self.line_cache[i].render {
                                fits &= *item_fits;
                                parts.push(lines);
                            }
                        }
                        chat::TranscriptItem::Tool(id) => {
                            // compute_offsets (above) refreshed every cache
                            // entry; a missing entry renders zero lines.
                            if let Some((_, _, _, lines, item_fits)) =
                                self.tool_render_cache.get(id)
                            {
                                fits &= *item_fits;
                                parts.push(lines);
                            }
                        }
                    }
                }
                // Field-level borrow (not a &self method) so the parts vec
                // can coexist with the &mut self.tui borrow below.
                // A stale rev MUST still reuse the cached render: deltas
                // bump stream_rev every message, but re-parses are
                // throttled — treating a stale cache as "no streaming
                // lines" makes the partial vanish on throttled frames,
                // oscillating the frame height and triggering a full
                // rewrite every other frame (Termux refresh storm).
                let streaming: &[Line] = if self.streaming.is_none() {
                    &[]
                } else {
                    match &self.stream_render {
                        Some((_, w, lines, _)) if *w == width => lines,
                        _ => &[],
                    }
                };
                parts.push(streaming);
                parts.push(&tail);
                parts.push(&bottom);
                if debug_capture {
                    joined = Some(parts.iter().flat_map(|p| p.iter().cloned()).collect());
                }
                self.tui.render_parts_with_fit(&parts, out, fits)
            };
            let _ = render_result?;
            if let Some(frame) = joined {
                self.write_debug_dump(&frame);
            }
            return out.flush();
        }

        // Fullscreen: windowed materialization — only transcript items
        // intersecting the visible window (±overscan) are cloned.
        let search_h: u16 = if self.search.is_some() { 1 } else { 0 };
        let viewport_h = height
            .saturating_sub(bottom.len() as u16)
            .saturating_sub(search_h)
            .max(1);
        self.scroll.viewport_height = viewport_h;
        let stream_len = self.streaming_lines(width).len();
        let tail_start = self.transcript_total + stream_len;
        let total = tail_start + tail.len();
        let max_scroll = total.saturating_sub(viewport_h as usize);
        let top = if self.scroll.follow_end {
            max_scroll
        } else {
            self.scroll.scroll_top.min(max_scroll)
        };
        const OVERSCAN: usize = 64;
        let win_top = top.saturating_sub(OVERSCAN);
        let win_bot = (top + viewport_h as usize + OVERSCAN).min(total);

        // Bounded transcript caches: rendered lines far outside the window
        // are dropped (heights survive, misses re-render on demand), so a
        // long session's cache memory stays bounded by the budget instead
        // of growing with the transcript.
        {
            let keep = (
                win_top.saturating_sub(CACHE_KEEP_MARGIN_LINES),
                win_bot.saturating_add(CACHE_KEEP_MARGIN_LINES),
            );
            let spans: Vec<(usize, usize)> = (0..self.items.len())
                .map(|i| (self.offsets[i], self.offsets[i] + self.item_heights[i]))
                .collect();
            evict_line_cache(&mut self.line_cache, &spans, keep, LINE_CACHE_BUDGET_LINES);
            let tool_spans: Vec<(&str, usize, usize)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| match item {
                    chat::TranscriptItem::Tool(id) => Some((
                        id.as_str(),
                        self.offsets[i],
                        self.offsets[i] + self.item_heights[i],
                    )),
                    chat::TranscriptItem::Chat(_) => None,
                })
                .collect();
            evict_tool_cache(
                &mut self.tool_render_cache,
                &tool_spans,
                keep,
                TOOL_CACHE_BUDGET_LINES,
            );
        }

        let mut window_lines: Vec<Line> = Vec::new();
        let mut window_start = self.transcript_total;
        for i in 0..self.items.len() {
            let start = self.offsets[i];
            let end = start + self.item_heights[i];
            if end <= win_top || start >= win_bot {
                continue;
            }
            if window_lines.is_empty() {
                window_start = start;
            }
            match &self.items[i] {
                chat::TranscriptItem::Tool(id) => {
                    let id = id.clone();
                    let (rendered, item_fits) = self.render_tool_cached(&id, width);
                    fits &= item_fits;
                    window_lines.extend(rendered);
                }
                chat::TranscriptItem::Chat(_) => {
                    self.ensure_chat_render(i, width);
                    if let Some((_, lines, item_fits)) = &self.line_cache[i].render {
                        fits &= *item_fits;
                        window_lines.extend(lines.iter().cloned());
                    }
                }
            }
        }
        // Streaming + tail sections occupy [transcript_total, total);
        // include the windowed sub-slices when the window reaches them
        // (typically: always, in follow_end mode).
        if win_bot > self.transcript_total {
            if window_lines.is_empty() {
                window_start = win_top.max(self.transcript_total);
            }
            let streaming = self.streaming_lines(width);
            let s0 = win_top.max(self.transcript_total);
            let s1 = win_bot.min(tail_start);
            if s0 < s1 {
                window_lines.extend(
                    streaming[s0 - self.transcript_total..s1 - self.transcript_total]
                        .iter()
                        .cloned(),
                );
            }
            let t0 = win_top.max(tail_start);
            let t1 = win_bot.min(total);
            if t0 < t1 {
                window_lines.extend(tail[t0 - tail_start..t1 - tail_start].iter().cloned());
            }
            fits &= Self::lines_fit(&tail, width);
        }
        self.scroll
            .set_virtual_content(total, window_start, window_lines);

        let mut frame = self.scroll.render(width);
        // Jump-to-bottom pill (Claude Code style): while the user is
        // scrolled up (follow-end off), float a tappable pill on the last
        // transcript row. Clicking it — or pressing End — jumps back down
        // and resumes following new output.
        self.jump_pill = None;
        if !self.scroll.follow_end && viewport_h > 0 {
            let label = crate::i18n::t(self.lang, "fs.back_to_bottom", &[]);
            let pill_style = self.theme.text.merged_with(&self.theme.selected_bg).bold();
            let mut pill = Line::new();
            pill.push(Span::styled(format!(" {label} "), pill_style));
            let pill_w = pill.width();
            if pill_w < width as usize {
                let col = ((width as usize - pill_w) / 2) as u16;
                let mut row = Line::new();
                row.push(Span::plain(" ".repeat(col as usize)));
                row.push(Span::styled(format!(" {label} "), pill_style));
                row.pad_right(width as usize, Style::default());
                frame[viewport_h as usize - 1] = row;
                self.jump_pill = Some((viewport_h - 1, col, col + pill_w as u16));
            }
        }
        if let Some(search) = &self.search {
            let mut bar = Line::new();
            bar.push(Span::styled(" /", self.theme.accent));
            bar.push(Span::plain(search.query.clone()));
            bar.push(Span::styled(
                format!(
                    "  ({}/{})",
                    if search.matches.is_empty() {
                        0
                    } else {
                        search.current + 1
                    },
                    search.matches.len()
                ),
                self.theme.dim,
            ));
            bar.pad_right(width as usize, Style::default());
            frame.push(bar);
        }
        frame.extend(bottom);
        while (frame.len() as u16) < height {
            frame.push(Line::new());
        }
        frame.truncate(height as usize);

        // Search match highlight + drag selection overlay. Search scans the
        // FULL transcript (materialized on demand when the query changes) —
        // windowed content would limit matches to the visible area. The
        // content key additionally invalidates on appended/streamed content,
        // so matches track a live transcript instead of freezing at the
        // moment the search bar was edited.
        let content_key = (self.items.len(), self.stream_rev, self.transcript_total);
        let search_dirty = self
            .search
            .as_ref()
            .map(|s| {
                if s.dirty {
                    // Query edited: always recompute immediately.
                    return true;
                }
                if s.content_key == content_key {
                    return false;
                }
                // Content changed under a stable query (streaming/appended
                // transcript): throttle — stream_rev ticks per delta and a
                // full materialize+rescan per delta melts the frame budget.
                s.last_compute
                    .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(200))
            })
            .unwrap_or(false);
        if search_dirty {
            let mut full = self.materialize_transcript(width, &mut fits);
            full.extend(self.streaming_lines(width).iter().cloned());
            full.extend(tail.iter().cloned());
            if let Some(search) = &mut self.search {
                search.update(&full);
                search.content_key = content_key;
                search.last_compute = Some(std::time::Instant::now());
            }
        }
        if let Some(search) = &self.search
            && !search.query.is_empty()
        {
            let match_style = Style::new().bg(tack_tui::Color::Rgb(80, 70, 30));
            let scroll_top = self.scroll.scroll_top;
            for &row in &search.matches {
                let screen_row = row as i64 - scroll_top as i64;
                if screen_row >= 0
                    && let Some(line) = frame.get_mut(screen_row as usize)
                    && screen_row < viewport_h as i64
                {
                    for span in &mut line.spans {
                        span.style = span.style.merged_with(&match_style);
                    }
                }
            }
        }
        if let Some(selection) = self.selection {
            selection.highlight(&mut frame);
        }

        fits &= Self::lines_fit(&frame, width);
        // /debug capture: dump before the frame moves into last_frame.
        if std::mem::take(&mut self.debug_capture) {
            self.write_debug_dump(&frame);
        }
        self.last_frame = frame.clone();
        let _ = self.tui.render_with_fit(frame, out, fits)?;
        out.flush()
    }

    /// Per-item start offsets and heights (pass 1 of every frame). Heights
    /// come from the caches (a rendered entry, or the height that survived
    /// eviction), so this is a cheap integer pass. Absolute indices —
    /// windowing independent.
    fn compute_offsets(&mut self, width: u16) {
        if self.line_cache.len() != self.items.len() {
            self.line_cache
                .resize(self.items.len(), LineCacheEntry::default());
        }
        self.offsets.clear();
        self.item_heights.clear();
        self.prompt_rows.clear();
        let mut pos = 0usize;
        for i in 0..self.items.len() {
            self.offsets.push(pos);
            let height = match &self.items[i] {
                chat::TranscriptItem::Chat(entry) => {
                    if let chat::ChatEntry::User { .. } = entry {
                        self.prompt_rows.push(pos);
                    }
                    match self.line_cache[i].height {
                        Some((w, h)) if w == width => h,
                        _ => {
                            let media = self.media();
                            let rendered = entry.render(width, &self.theme, media);
                            let fits = Self::lines_fit(&rendered, width);
                            let height = rendered.len();
                            self.line_cache[i] = LineCacheEntry {
                                render: Some((width, rendered, fits)),
                                height: Some((width, height)),
                            };
                            height
                        }
                    }
                }
                chat::TranscriptItem::Tool(id) => {
                    let id = id.clone();
                    self.tool_height(&id, width)
                }
            };
            self.item_heights.push(height);
            pos += height;
        }
        self.transcript_total = pos;
    }

    /// A tool card's rendered height from the height cache (renders on
    /// miss). Does not clone lines; the height survives render eviction
    /// so an evicted card stays O(1) here.
    fn tool_height(&mut self, id: &str, width: u16) -> usize {
        let Some(tool) = self.tools.get(id) else {
            return 0;
        };
        let fingerprint: u64 = match &tool.state {
            tool_render::ToolState::Running { partial_output } => partial_output.len() as u64,
            tool_render::ToolState::Done { .. } => u64::MAX,
        };
        let expanded = tool.expanded;
        if let Some((w, e, fp, height)) = self.tool_heights.get(id)
            && *w == width
            && *e == expanded
            && *fp == fingerprint
        {
            return *height;
        }
        let lines = tool.render(
            width,
            &self.theme,
            self.image_protocol,
            self.settings.image_width_cells,
        );
        let height = lines.len();
        let fits = Self::lines_fit(&lines, width);
        self.tool_heights
            .insert(id.to_string(), (width, expanded, fingerprint, height));
        self.tool_render_cache
            .insert(id.to_string(), (width, expanded, fingerprint, lines, fits));
        height
    }

    /// Full transcript materialization (regular mode; fullscreen search).
    fn materialize_transcript(&mut self, width: u16, fits: &mut bool) -> Vec<Line> {
        let mut lines: Vec<Line> = Vec::new();
        for i in 0..self.items.len() {
            match &self.items[i] {
                chat::TranscriptItem::Chat(_) => {
                    self.ensure_chat_render(i, width);
                    if let Some((_, cached, item_fits)) = &self.line_cache[i].render {
                        *fits &= *item_fits;
                        lines.extend(cached.iter().cloned());
                    }
                }
                chat::TranscriptItem::Tool(id) => {
                    let id = id.clone();
                    let (rendered, item_fits) = self.render_tool_cached(&id, width);
                    *fits &= item_fits;
                    lines.extend(rendered);
                }
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streaming markdown throttle: first frame always renders; structural
    /// changes (new block) and large byte jumps render immediately; small
    /// deltas wait out the interval. Without it, every drain batch reparsed
    /// the whole partial (O(n²) over a stream).
    #[test]
    fn stream_render_throttle_policy() {
        // No mark yet (first streaming frame): render.
        assert!(stream_render_due(None, 10, 1));

        let now = Instant::now();
        let mark = Some((1000, 1, now));
        // Tiny delta within the interval: throttled.
        assert!(!stream_render_due(mark, 1000 + 100, 1));
        // Structural change: immediate.
        assert!(stream_render_due(mark, 1000, 2));
        // Large byte jump: immediate.
        assert!(stream_render_due(mark, 1000 + STREAM_RENDER_MIN_BYTES, 1));
        // Interval elapsed: render even for a tiny delta.
        let old = Some((
            1000,
            1,
            now - STREAM_RENDER_MIN_INTERVAL - Duration::from_millis(1),
        ));
        assert!(stream_render_due(old, 1001, 1));
    }

    fn cache_entry(lines: usize, width: u16) -> LineCacheEntry {
        LineCacheEntry {
            render: Some((width, vec![Line::new(); lines], true)),
            height: Some((width, lines)),
        }
    }

    /// Eviction drops renders far outside the window but keeps their
    /// heights, so layout stays correct and a miss just re-renders.
    #[test]
    fn line_cache_eviction_keeps_window_and_heights() {
        // 10 items × 100 lines each: offsets 0, 100, ..., 900.
        let mut cache: Vec<LineCacheEntry> = (0..10).map(|_| cache_entry(100, 80)).collect();
        let spans: Vec<(usize, usize)> = (0..10).map(|i| (i * 100, i * 100 + 100)).collect();
        // Window around lines 500-600 → keep [350, 700): items 3..=6
        // overlap the window and survive; item 7 starts AT 700 (fully
        // outside — window end is exclusive).
        let evicted = evict_line_cache(&mut cache, &spans, (350, 700), LINE_CACHE_BUDGET_LINES);
        assert_eq!(evicted, 6);
        for (i, entry) in cache.iter().enumerate() {
            if (3..=6).contains(&i) {
                assert!(entry.render.is_some(), "item {i} inside keep range");
            } else {
                assert!(entry.render.is_none(), "item {i} evicted");
            }
            // Height survives eviction either way.
            assert_eq!(entry.height, Some((80, 100)), "item {i} height");
        }
    }

    /// Over-budget caches evict farthest-from-window entries first.
    #[test]
    fn line_cache_eviction_enforces_budget() {
        let mut cache: Vec<LineCacheEntry> = (0..10).map(|_| cache_entry(100, 80)).collect();
        let spans: Vec<(usize, usize)> = (0..10).map(|i| (i * 100, i * 100 + 100)).collect();
        // Keep everything within the margin, but budget only 400 lines.
        let evicted = evict_line_cache(&mut cache, &spans, (0, 1000), 400);
        assert_eq!(evicted, 6);
        let remaining: usize = cache
            .iter()
            .filter_map(|e| e.render.as_ref().map(|(_, l, _)| l.len()))
            .sum();
        assert!(remaining <= 400, "remaining {remaining}");
        // The middle (closest to the window) survives; edges evicted.
        assert!(cache[4].render.is_some() && cache[5].render.is_some());
        assert!(cache[0].render.is_none() && cache[9].render.is_none());
    }

    #[test]
    fn tool_cache_eviction_by_distance_and_budget() {
        let mut cache = std::collections::HashMap::new();
        let ids: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let spans: Vec<(&str, usize, usize)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i * 100, i * 100 + 100))
            .collect();
        for (id, _, _) in &spans {
            cache.insert(
                id.to_string(),
                (80u16, false, u64::MAX, vec![Line::new(); 100], true),
            );
        }
        let evicted = evict_tool_cache(&mut cache, &spans, (350, 700), TOOL_CACHE_BUDGET_LINES);
        assert_eq!(evicted, 6);
        for i in 0..10 {
            assert_eq!(cache.contains_key(&format!("t{i}")), (3..=6).contains(&i));
        }
        // Budget: reinsert everything, cap at 400 lines.
        for (id, _, _) in &spans {
            cache.insert(
                id.to_string(),
                (80u16, false, u64::MAX, vec![Line::new(); 100], true),
            );
        }
        let evicted = evict_tool_cache(&mut cache, &spans, (0, 1000), 400);
        assert_eq!(evicted, 6);
        let remaining: usize = cache.values().map(|(_, _, _, l, _)| l.len()).sum();
        assert!(remaining <= 400, "remaining {remaining}");
        assert!(cache.contains_key("t4") && cache.contains_key("t5"));
        assert!(!cache.contains_key("t0") && !cache.contains_key("t9"));
    }
}
