//! Component interface (port of tack-tui's `Component`).

use crate::input::InputEvent;
use crate::line::Line;

/// Cursor position marker - APC (Application Program Command) sequence.
/// Zero-width; terminals ignore it. Focused components emit it at the cursor
/// position; the renderer finds and strips it, then positions the hardware
/// cursor there (for IME candidate windows).
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// All UI elements implement Component.
pub trait Component: std::fmt::Debug {
    /// Render to lines for the given viewport width.
    fn render(&mut self, width: u16) -> Vec<Line>;

    /// Handle input while focused. Return true if consumed.
    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let _ = event;
        false
    }

    /// If true, the component receives key *release* events (Kitty protocol).
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Invalidate cached rendering state (theme change / full re-render).
    fn invalidate(&mut self) {}
}

/// Components that can receive focus and show a hardware cursor.
pub trait Focusable {
    fn set_focused(&mut self, focused: bool);
    fn is_focused(&self) -> bool;
}
