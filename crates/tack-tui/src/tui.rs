//! TUI core: overlay stack, input routing, render dispatch to the active
//! backend (main screen / alt screen).

use crate::component::Component;
use crate::input::InputEvent;
use crate::line::Line;
use crate::screen_alt::AltScreenRenderer;
use crate::screen_main::{CursorPosition, MainScreenRenderer};

/// Absolute or percentage size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeValue {
    Absolute(u16),
    Percent(u16),
}

impl SizeValue {
    fn resolve(self, reference: u16) -> u16 {
        match self {
            SizeValue::Absolute(v) => v,
            SizeValue::Percent(p) => (reference as u32 * p as u32 / 100) as u16,
        }
    }
}

/// Anchor position for overlays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

/// Overlay positioning/sizing options.
#[derive(Clone, Debug, Default)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub min_width: Option<u16>,
    pub max_height: Option<SizeValue>,
    pub anchor: OverlayAnchor,
    pub offset_x: i32,
    pub offset_y: i32,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    pub margin: u16,
    /// Overlay starts hidden (render nothing until shown).
    pub hidden: bool,
}

/// Handle to a live overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OverlayHandle(pub u64);

struct OverlayEntry {
    id: u64,
    component: Box<dyn Component>,
    options: OverlayOptions,
    hidden: bool,
}

impl std::fmt::Debug for OverlayEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayEntry")
            .field("id", &self.id)
            .finish()
    }
}

/// Which screen backend is active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TuiMode {
    /// Main screen + scrollback (default; output persists in history).
    #[default]
    Regular,
    /// Alternate screen, full-screen viewport.
    Fullscreen,
}

/// The TUI runtime. Owns overlays and the renderer; the app supplies main
/// content lines per frame.
#[derive(Debug)]
pub struct Tui {
    overlays: Vec<OverlayEntry>,
    next_overlay_id: u64,
    main: MainScreenRenderer,
    alt: AltScreenRenderer,
    pub mode: TuiMode,
    width: u16,
    height: u16,
}

impl Tui {
    pub fn new(mode: TuiMode) -> Self {
        let (width, height) = crate::terminal::size();
        Tui {
            overlays: Vec::new(),
            next_overlay_id: 1,
            main: MainScreenRenderer::new(),
            alt: AltScreenRenderer::new(),
            mode,
            width,
            height,
        }
    }

    /// terminal.clearOnShrink passthrough (main-screen renderer).
    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.main.clear_on_shrink = enabled;
    }

    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        for overlay in &mut self.overlays {
            overlay.component.invalidate();
        }
    }

    /// Switch the render backend at runtime; both renderers reset so the next
    /// frame is a full redraw.
    pub fn set_mode(&mut self, mode: TuiMode) {
        if self.mode != mode {
            self.mode = mode;
            self.main.reset();
            self.alt.reset();
        }
    }

    /// Show an overlay component; returns a handle.
    pub fn show_overlay(
        &mut self,
        component: Box<dyn Component>,
        options: OverlayOptions,
    ) -> OverlayHandle {
        let id = self.next_overlay_id;
        self.next_overlay_id += 1;
        let hidden = options.hidden;
        self.overlays.push(OverlayEntry {
            id,
            component,
            options,
            hidden,
        });
        OverlayHandle(id)
    }

    pub fn hide_overlay(&mut self, handle: OverlayHandle) {
        self.overlays.retain(|o| o.id != handle.0);
    }

    pub fn set_overlay_hidden(&mut self, handle: OverlayHandle, hidden: bool) {
        if let Some(entry) = self.overlays.iter_mut().find(|o| o.id == handle.0) {
            entry.hidden = hidden;
            entry.component.invalidate();
        }
    }

    pub fn has_overlays(&self) -> bool {
        self.overlays.iter().any(|o| !o.hidden)
    }

    /// Route input: visible overlays top-down first. Returns true when
    /// consumed by an overlay.
    pub fn handle_input(&mut self, event: &InputEvent) -> bool {
        for entry in self.overlays.iter_mut().rev() {
            if entry.hidden {
                continue;
            }
            if !entry.component.wants_key_release()
                && let InputEvent::Key(k) = event
                && k.is_release
            {
                continue;
            }
            if entry.component.handle_input(event) {
                return true;
            }
        }
        false
    }

    /// Render one frame: main content lines + composited overlays.
    /// Returns the cursor position found via CURSOR_MARKER, if any.
    pub fn render(
        &mut self,
        main_lines: Vec<Line>,
        out: &mut dyn std::io::Write,
    ) -> std::io::Result<Option<CursorPosition>> {
        self.render_with_fit(main_lines, out, false)
    }

    /// Render with a caller guarantee about line widths. `assume_fit: true`
    /// skips the per-frame "any line wider than the viewport" scan (a full
    /// grapheme-width pass over every line — expensive at large
    /// transcripts). Only pass true when the caller has verified all lines
    /// fit; overlong lines are then handled as usual when detected.
    pub fn render_with_fit(
        &mut self,
        main_lines: Vec<Line>,
        out: &mut dyn std::io::Write,
        assume_fit: bool,
    ) -> std::io::Result<Option<CursorPosition>> {
        let (width, height) = (self.width, self.height);
        let mut lines = main_lines;
        self.composite_overlays(&mut lines, width, height);
        match self.mode {
            TuiMode::Regular => self.main.render(&lines, width, height, assume_fit, out),
            TuiMode::Fullscreen => self.alt.render(&lines, width, height, assume_fit, out),
        }
    }

    /// Parts variant of [`Tui::render_with_fit`]: the frame is passed as
    /// concatenated slices, avoiding one contiguous `Vec<Line>` clone per
    /// frame — regular mode hands over the cached transcript item slices
    /// directly, so a keystroke at a large transcript costs O(changed
    /// rows) instead of O(transcript). Fullscreen mode and overlay
    /// compositing still join the parts (those frames are small).
    pub fn render_parts_with_fit(
        &mut self,
        parts: &[&[Line]],
        out: &mut dyn std::io::Write,
        assume_fit: bool,
    ) -> std::io::Result<Option<CursorPosition>> {
        let (width, height) = (self.width, self.height);
        if matches!(self.mode, TuiMode::Regular) && !self.has_overlays() {
            return self
                .main
                .render_parts(parts, width, height, assume_fit, out);
        }
        let mut lines: Vec<Line> = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
        for part in parts {
            lines.extend(part.iter().cloned());
        }
        self.composite_overlays(&mut lines, width, height);
        match self.mode {
            TuiMode::Regular => self.main.render(&lines, width, height, assume_fit, out),
            TuiMode::Fullscreen => self.alt.render(&lines, width, height, assume_fit, out),
        }
    }

    pub fn stop(&mut self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        match self.mode {
            TuiMode::Regular => self.main.stop(out),
            TuiMode::Fullscreen => self.alt.stop(out),
        }
    }

    /// Composite visible overlays over the main lines (in-place).
    fn composite_overlays(&mut self, lines: &mut Vec<Line>, width: u16, height: u16) {
        for index in 0..self.overlays.len() {
            let entry = &mut self.overlays[index];
            if entry.hidden {
                continue;
            }
            // Render the overlay into a temporary buffer sized to its area.
            let options = &entry.options;
            let margin = options.margin;
            let avail_w = width.saturating_sub(margin.saturating_mul(2));
            let overlay_width = options
                .width
                .map(|w| w.resolve(avail_w))
                .unwrap_or(avail_w)
                .max(options.min_width.unwrap_or(0))
                .min(avail_w)
                .max(1);
            let component = entry.component.as_mut();
            let rendered = component.render(overlay_width);
            let rendered = match options.max_height {
                Some(max) => {
                    let max = max.resolve(height) as usize;
                    if rendered.len() > max {
                        rendered[..max].to_vec()
                    } else {
                        rendered
                    }
                }
                None => rendered,
            };
            if rendered.is_empty() {
                continue;
            }
            let overlay_height = rendered.len() as u16;
            let (mut row, mut col) =
                anchor_position(options.anchor, width, height, overlay_width, overlay_height);
            if let Some(r) = options.row {
                row = r.resolve(height) as i32;
            }
            if let Some(c) = options.col {
                col = c.resolve(width) as i32;
            }
            row += options.offset_y;
            col += options.offset_x;
            let row = row.clamp(0, height.saturating_sub(overlay_height) as i32) as usize;
            let col = col.clamp(0, width.saturating_sub(overlay_width) as i32) as usize;

            // Splice overlay lines into the frame.
            if lines.len() < height as usize {
                lines.resize(height as usize, Line::new());
            }
            for (i, overlay_line) in rendered.into_iter().enumerate() {
                let target_row = row + i;
                if target_row >= lines.len() {
                    break;
                }
                let base = &lines[target_row];
                let mut merged = base.slice_cells(0, col);
                merged.pad_right(col, crate::style::Style::default());
                for span in overlay_line.spans {
                    merged.push(span);
                }
                let right = base.slice_cells(col + overlay_width as usize, usize::MAX);
                for span in right.spans {
                    merged.push(span);
                }
                lines[target_row] = merged;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn anchor_position(
    anchor: OverlayAnchor,
    width: u16,
    height: u16,
    overlay_width: u16,
    overlay_height: u16,
) -> (i32, i32) {
    let center_row = (height as i32 - overlay_height as i32) / 2;
    let center_col = (width as i32 - overlay_width as i32) / 2;
    match anchor {
        OverlayAnchor::Center => (center_row, center_col),
        OverlayAnchor::TopLeft => (0, 0),
        OverlayAnchor::TopRight => (0, width as i32 - overlay_width as i32),
        OverlayAnchor::BottomLeft => (height as i32 - overlay_height as i32, 0),
        OverlayAnchor::BottomRight => (
            height as i32 - overlay_height as i32,
            width as i32 - overlay_width as i32,
        ),
        OverlayAnchor::TopCenter => (0, center_col),
        OverlayAnchor::BottomCenter => (height as i32 - overlay_height as i32, center_col),
        OverlayAnchor::LeftCenter => (center_row, 0),
        OverlayAnchor::RightCenter => (center_row, width as i32 - overlay_width as i32),
    }
}
