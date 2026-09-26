//! Mermaid diagram rendering: mermaid-rs-renderer → PNG → inline image
//! (Kitty/iTerm2) or half-block truecolor fallback. PNGs are content-hash
//! cached in the temp dir so re-renders (every frame while the transcript
//! repaints) cost nothing; a zero-byte cache entry marks a failed render.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::PathBuf;

use tack_tui::Line;
use tack_tui::image::ImageProtocol;

fn hash_source(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(source.as_bytes());
    hasher.write_u8(u8::from(dark_terminal()));
    hasher.finish()
}

/// Diagram theme follows the terminal background: a glaring white PNG on a
/// dark TUI is hard to read, and vice versa (font size is bumped over the
/// crate default 14 for legibility at terminal scales). The font family is
/// generic `sans-serif`: the crate default (Inter…) is missing on most
/// systems and makes usvg emit a fallback warning per text element.
fn diagram_theme() -> mermaid_rs_renderer::Theme {
    let mut theme = if dark_terminal() {
        mermaid_rs_renderer::Theme::dark()
    } else {
        mermaid_rs_renderer::Theme::modern()
    };
    // 32px (crate default: 14): on half-block terminals each glyph must span
    // enough cells to stay legible; on Kitty the diagram simply shows larger.
    theme.font_size = 32.0;
    theme.font_family = "sans-serif".to_string();
    theme
}

fn dark_terminal() -> bool {
    crate::tui::theme::terminal_is_dark()
}

fn cache_path(source: &str) -> PathBuf {
    // v5: 32px font (half-block legibility). v4: 6× render scale.
    std::env::temp_dir().join(format!("tack-mermaid-v5-{:x}.png", hash_source(source)))
}

/// Upscale the SVG's width/height attributes (viewBox keeps the content
/// proportional). Render resolution and display size are decoupled:
/// RENDER_SCALE (6×) makes the PNG dense enough for HiDPI terminals (Warp on
/// a 2× display upscales logical pixels; a 3× source still looked soft),
/// while the DISPLAY size is always the diagram's logical size (physical /
/// effective scale) — the terminal downsamples the dense bitmap, which is
/// what keeps text crisp. (v4 displayed at 3× logical: a modest flowchart
/// filled 120×115 cells, and large diagrams whose render scale had been
/// clamped below the display scale were upscaled back up — huge AND
/// blurry.)
/// Large diagrams get a reduced scale: unclamped 6× on a 2000×3000 diagram
/// is 216M pixels and resvg fails to allocate the pixmap (the diagram then
/// silently falls back to a code block).
const RENDER_SCALE: f32 = 6.0;
/// Pixel budget for the rendered bitmap (~24MP, 4 bytes/pixel ≈ 96MB max).
const MAX_RENDER_PIXELS: f32 = 24_000_000.0;

/// Hard input limits. mermaid-rs-renderer parses and lays out recursively
/// and its cost grows super-linearly with graph size — measured on a
/// flowchart chain: 200k nodes overflow the stack (abort), 50k nodes take
/// over 10 minutes, and rendering runs synchronously on the UI render path, so
/// the whole TUI freezes. Sources past these limits fall back to plain
/// code-block rendering (None) instead of panicking or blocking.
const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_GRAPH_ELEMENTS: usize = 2000;

/// Cheap graph-size gate: no full parse, just byte length plus a statement/
/// edge estimate (line count vs. arrow count, whichever dominates — a chain
/// packs many edges on one line, a styled diagram spreads them over lines).
fn source_within_limits(source: &str) -> bool {
    if source.len() > MAX_SOURCE_BYTES {
        return false;
    }
    let arrows = source.matches("-->").count() + source.matches("---").count();
    let elements = source.lines().count().max(arrows);
    elements <= MAX_GRAPH_ELEMENTS
}

/// Scale the SVG by `scale` (clamped to the pixel budget); returns the
/// scaled SVG and the effective scale (needed for display-size math).
fn scale_svg(svg: &str, scale: f32) -> (String, f32) {
    let svg_dims = |text: &str| -> Option<(f32, f32)> {
        let tag_end = text.find('>')?;
        let tag = &text[..tag_end];
        let dim = |attr: &str| -> Option<f32> {
            let pos = tag.find(attr)? + attr.len();
            let end = text[pos..].find('"').map(|i| pos + i)?;
            text[pos..end].parse().ok()
        };
        Some((dim("width=\"")?, dim("height=\"")?))
    };
    let scale = if let Some((w, h)) = svg_dims(svg) {
        let max = (MAX_RENDER_PIXELS / (w * h).max(1.0)).sqrt();
        scale.min(max).max(1.0)
    } else {
        scale
    };
    let patch_attr = |text: &str, attr: &str| -> String {
        let Some(tag_end) = text.find('>') else {
            return text.to_string();
        };
        let tag = &text[..tag_end];
        let Some(attr_pos) = tag.find(attr) else {
            return text.to_string();
        };
        let value_start = attr_pos + attr.len();
        let Some(value_end) = text[value_start..].find('"').map(|i| value_start + i) else {
            return text.to_string();
        };
        let Ok(value) = text[value_start..value_end].parse::<f32>() else {
            return text.to_string();
        };
        format!(
            "{}{}{}",
            &text[..value_start],
            value * scale,
            &text[value_end..]
        )
    };
    let svg = patch_attr(svg, "width=\"");
    (patch_attr(&svg, "height=\""), scale)
}

/// Render mermaid source to SVG (theme applied at generation time).
/// Text-art mode gets a tighter layout (default padding/rank spacing is
/// tuned for bitmaps and produces excessive whitespace on a cell grid).
fn render_svg(source: &str, tight_layout: bool) -> Option<String> {
    let mut layout = mermaid_rs_renderer::LayoutConfig::default();
    let mut theme = diagram_theme();
    if tight_layout {
        layout.node_spacing = 20.0;
        layout.rank_spacing = 25.0;
        layout.node_padding_x = 8.0;
        layout.node_padding_y = 4.0;
        // Text art doesn't use the font for rendering; a small font keeps the
        // px coordinates compact (less whitespace on the cell grid).
        theme.font_size = 16.0;
    }
    mermaid_rs_renderer::render_with_options(
        source,
        mermaid_rs_renderer::RenderOptions { theme, layout },
    )
    .ok()
}

/// Test hook: the tight-layout SVG the text-art path renders.
#[cfg(test)]
pub(crate) fn render_svg_for_test(source: &str) -> Option<String> {
    render_svg(source, true)
}

/// Render mermaid source to PNG bytes plus the effective render scale
/// (cached). Failures are cached too — but only for NEGATIVE_CACHE_TTL:
/// a transient failure (e.g. an interrupted render) must not blacklist the
/// diagram forever.
fn render_png_cached(source: &str) -> Option<(Vec<u8>, f32)> {
    const NEGATIVE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
    let path = cache_path(source);
    if let Ok(bytes) = std::fs::read(&path) {
        if !bytes.is_empty() {
            let sidecar = path.with_extension("scale");
            let scale = std::fs::read_to_string(&sidecar)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(RENDER_SCALE);
            return Some((bytes, scale));
        }
        // Zero-byte = negative cache; honor only within the TTL.
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < NEGATIVE_CACHE_TTL);
        if fresh {
            return None;
        }
    }
    let theme = diagram_theme();
    let png = render_svg(source, false).and_then(|svg| {
        let (scaled, scale) = scale_svg(&svg, RENDER_SCALE);
        let config = mermaid_rs_renderer::RenderConfig::default();
        mermaid_rs_renderer::write_output_png(&scaled, &path, &config, &theme).ok()?;
        let _ = std::fs::write(path.with_extension("scale"), scale.to_string());
        std::fs::read(&path).ok().map(|bytes| (bytes, scale))
    });
    match png {
        Some(pair) => Some(pair),
        None => {
            tracing::warn!(
                "mermaid: failed to render diagram to PNG ({} chars of source)",
                source.len()
            );
            // Negative cache so broken diagrams don't re-render every frame.
            let _ = std::fs::write(&path, b"");
            None
        }
    }
}

/// Rendered-lines memo for `mermaid_lines`. Entries can embed a whole
/// base64 PNG per image line (Kitty/iTerm2 escapes), so a COUNT cap alone
/// can pin hundreds of MB: eviction is by estimated BYTE budget, oldest
/// (least recently used) first, with a generous entry-count cap on top.
struct LinesCache {
    map: std::collections::HashMap<(u64, u16, Option<ImageProtocol>), CacheEntry>,
    total_bytes: usize,
    /// Monotonic use counter; the smallest tick is the oldest entry.
    tick: u64,
}

struct CacheEntry {
    lines: Option<Vec<Line>>,
    bytes: usize,
    tick: u64,
}

/// ~128MB of rendered lines (base64 PNG payloads dominate).
const LINES_CACHE_BUDGET_BYTES: usize = 128 * 1024 * 1024;
/// Entry-count cap for the many-small-diagrams case.
const LINES_CACHE_MAX_ENTRIES: usize = 256;

/// Rough heap size of a rendered entry: span text (the base64 PNG lives
/// here) plus a fixed per-line struct overhead.
fn estimate_lines_bytes(lines: &Option<Vec<Line>>) -> usize {
    let Some(lines) = lines else {
        return 64;
    };
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.text.len() + 64).sum::<usize>())
        .sum::<usize>()
        + 128
}

impl LinesCache {
    /// Evict oldest entries until back within budget + count caps.
    fn evict_to_budget(&mut self) {
        while self.total_bytes > LINES_CACHE_BUDGET_BYTES
            || self.map.len() > LINES_CACHE_MAX_ENTRIES
        {
            let Some((&oldest_key, _)) = self.map.iter().min_by_key(|(_, e)| e.tick) else {
                break;
            };
            if let Some(entry) = self.map.remove(&oldest_key) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.bytes);
            }
        }
    }
}

/// Produce transcript lines for a mermaid code block, or `None` to fall back
/// to plain code rendering (parse failure).
///
/// The rendered lines are memoized per (source, width, protocol): the chat
/// renderer re-renders the streaming message on every delta, and re-running
/// PNG decode + half-block resize each frame burns CPU on slow devices, while
/// fresh Kitty escape lines (new image id per call) make the diff renderer
/// re-transmit the image every frame — visible as constant flicker.
pub fn mermaid_lines(
    source: &str,
    width_cells: u16,
    protocol: Option<ImageProtocol>,
) -> Option<Vec<Line>> {
    let key = (hash_source(source), width_cells, protocol);
    static CACHE: std::sync::Mutex<Option<LinesCache>> = std::sync::Mutex::new(None);
    let mut cache = crate::tui::lock_recover(&CACHE);
    let cache = cache.get_or_insert_with(|| LinesCache {
        map: Default::default(),
        total_bytes: 0,
        tick: 0,
    });
    cache.tick += 1;
    let tick = cache.tick;
    if let Some(entry) = cache.map.get_mut(&key) {
        entry.tick = tick;
        return entry.lines.clone();
    }
    let lines = mermaid_lines_uncached(source, width_cells, protocol);
    let bytes = estimate_lines_bytes(&lines);
    // An entry larger than the whole byte budget is NOT cached: inserting
    // it would evict everything else and then be evicted itself on the
    // next insert, so a pathological diagram would thrash the cache AND
    // still re-render every frame. Skipping the insert keeps the rest of
    // the cache intact with the same worst-case cost.
    if bytes > LINES_CACHE_BUDGET_BYTES {
        return lines;
    }
    cache.total_bytes = cache.total_bytes.saturating_add(bytes);
    cache.map.insert(
        key,
        CacheEntry {
            lines: lines.clone(),
            bytes,
            tick,
        },
    );
    cache.evict_to_budget();
    lines
}

fn mermaid_lines_uncached(
    source: &str,
    width_cells: u16,
    protocol: Option<ImageProtocol>,
) -> Option<Vec<Line>> {
    if !source_within_limits(source) {
        // Oversized diagram: None falls back to code-block rendering — the
        // renderer would otherwise stack-overflow (huge chains) or block
        // the UI thread for minutes (super-linear layout cost).
        tracing::warn!(
            "mermaid: refusing oversized diagram ({} bytes of source)",
            source.len()
        );
        return None;
    }
    match protocol {
        Some(protocol) => {
            let (png, scale) = render_png_cached(source)?;
            let (pw, ph) = tack_tui::image::png_dimensions(&png);
            // Display at the diagram's logical size (physical / effective
            // scale), never larger: the terminal downsamples the dense
            // bitmap, keeping lines and text crisp on standard and HiDPI
            // displays.
            let logical_w = (pw as f32 / scale) as u32;
            let logical_h = (ph as f32 / scale) as u32;
            Some(tack_tui::image::image_lines(
                &png,
                protocol,
                width_cells.clamp(20, 120),
                9,
                18,
                logical_w,
                logical_h,
            ))
        }
        None => {
            // No graphics protocol: text art (box-drawing frames + real text
            // labels) is far more readable than half-block pixels — the
            // grok-mermaid approach TS pi takes. It needs only the SVG, so a
            // PNG failure (oversized diagram) doesn't block it. Half-block
            // remains the fallback when the SVG structure is unfamiliar.
            if let Some(svg) = render_svg(source, true)
                && let Some(mut art) =
                    crate::tui::mermaid_text::render_text_art(&svg, width_cells as usize)
            {
                // The PNG may still exist for the full-resolution caption.
                let _ = render_png_cached(source);
                art.push(Line::plain(crate::i18n::trf(
                    "mermaid.full_resolution",
                    &[("path", &cache_path(source).display().to_string())],
                )));
                return Some(art);
            }
            let (png, scale) = render_png_cached(source)?;
            let (pw, _ph) = tack_tui::image::png_dimensions(&png);
            let logical_w = (pw as f32 / scale) as u32;
            let logical_cells = (logical_w.div_ceil(9) as u16).max(20);
            let mut lines = tack_tui::image::half_block_lines(
                &png,
                width_cells.clamp(20, 120).min(logical_cells),
            )?;
            lines.push(Line::plain(crate::i18n::trf(
                "mermaid.full_resolution",
                &[("path", &cache_path(source).display().to_string())],
            )));
            Some(lines)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn lines_cache_evicts_oldest_within_byte_budget() {
        let mut cache = LinesCache {
            map: Default::default(),
            total_bytes: 0,
            tick: 0,
        };
        let big = LINES_CACHE_BUDGET_BYTES / 2 + 1;
        for i in 0..3u64 {
            cache.tick += 1;
            let tick = cache.tick;
            cache.total_bytes += big;
            cache.map.insert(
                (i, 80, None),
                CacheEntry {
                    lines: None,
                    bytes: big,
                    tick,
                },
            );
            cache.evict_to_budget();
        }
        assert!(cache.total_bytes <= LINES_CACHE_BUDGET_BYTES);
        assert_eq!(cache.map.len(), 1);
        assert!(cache.map.contains_key(&(2, 80, None)), "newest survives");
    }

    #[test]
    fn lines_cache_caps_entry_count() {
        let mut cache = LinesCache {
            map: Default::default(),
            total_bytes: 0,
            tick: 0,
        };
        for i in 0..(LINES_CACHE_MAX_ENTRIES as u64 + 10) {
            cache.tick += 1;
            let tick = cache.tick;
            cache.total_bytes += 1;
            cache.map.insert(
                (i, 80, None),
                CacheEntry {
                    lines: None,
                    bytes: 1,
                    tick,
                },
            );
            cache.evict_to_budget();
        }
        assert_eq!(cache.map.len(), LINES_CACHE_MAX_ENTRIES);
    }

    #[test]
    fn renders_flowchart_to_png() {
        let (png, _scale) = render_png_cached("flowchart LR; A-->B-->C").expect("png");
        assert!(png.starts_with(b"\x89PNG"));
        let (w, h) = tack_tui::image::png_dimensions(&png);
        assert!(w > 0 && h > 0);
    }

    #[test]
    fn invalid_source_negative_cached() {
        assert_eq!(render_png_cached("this is not mermaid at all {{{"), None);
        // Second call hits the negative cache (same result, no error).
        assert_eq!(render_png_cached("this is not mermaid at all {{{"), None);
    }

    #[test]
    fn large_diagram_scale_is_clamped() {
        // A diagram far beyond the pixel budget must still render (reduced
        // scale) instead of failing and falling back to a code block.
        let mut source = String::from("flowchart TB\n");
        for i in 0..200 {
            source.push_str(&format!(
                "  n{i}[node number {i} with a longish label] --> n{}\n",
                i + 1
            ));
        }
        source.push_str("  n201[end]\n");
        let rendered = render_png_cached(&source);
        assert!(
            rendered.is_some(),
            "large diagram should render with clamped scale"
        );
        let (png, scale) = rendered.unwrap();
        assert!(
            scale < RENDER_SCALE,
            "scale should have been clamped: {scale}"
        );
        let (w, h) = tack_tui::image::png_dimensions(&png);
        assert!(
            (w as u64 * h as u64) < 30_000_000,
            "bitmap within budget: {w}x{h}"
        );
    }

    #[test]
    fn oversized_source_falls_back_without_panicking() {
        // Byte cap: a >64KB source must not reach the renderer.
        let huge = format!("flowchart LR\n{}", "x".repeat(MAX_SOURCE_BYTES));
        assert!(
            mermaid_lines_uncached(&huge, 60, None).is_none(),
            "byte-capped source falls back to code-block rendering"
        );

        // Element cap: a chain with far more than MAX_GRAPH_ELEMENTS edges
        // used to stack-overflow the parser / freeze the UI for minutes.
        let mut chain = String::from("flowchart LR\n  n0");
        for i in 1..(MAX_GRAPH_ELEMENTS * 2) {
            chain.push_str(&format!("--> n{i}"));
        }
        assert!(
            mermaid_lines_uncached(&chain, 60, None).is_none(),
            "edge-capped source falls back to code-block rendering"
        );

        // Sanity: a modest diagram still passes the gate.
        assert!(source_within_limits("flowchart LR; A-->B-->C"));
    }

    #[test]
    fn no_graphics_protocol_produces_text_art() {
        let lines = mermaid_lines("flowchart LR; A-->B", 60, None).expect("lines");
        assert!(!lines.is_empty());
        let text: String = lines
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains('┌'), "text art frames: {text}");
        assert!(text.contains("full resolution:"), "caption: {text}");
    }

    #[test]
    fn display_size_is_logical_not_scaled() {
        // The diagram must display at its logical SVG size (dense bitmap
        // downsampled by the terminal), never at a multiple of it — v4's 3×
        // display scale made diagrams huge and blurry.
        let source = "flowchart LR; A-->B-->C";
        let (png, scale) = render_png_cached(source).expect("png");
        let (pw, _ph) = tack_tui::image::png_dimensions(&png);
        let logical_w = (pw as f32 / scale) as u32;
        let lines = mermaid_lines(source, 200, Some(ImageProtocol::ITerm2)).expect("lines");
        let text = lines[0].text();
        let cells: u32 = text
            .split("width=")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .and_then(|v| v.parse().ok())
            .expect("iTerm2 escape carries width=<cells>");
        assert_eq!(cells, logical_w.div_ceil(9).min(120));

        // No-graphics terminals get text art sized to the terminal width.
        let art = mermaid_lines(source, 200, None).expect("lines");
        let text: String = art.iter().map(|l| l.text()).collect::<Vec<_>>().join("\n");
        assert!(text.contains('┌'), "{text}");
    }
}
