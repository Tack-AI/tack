//! tack-tui: terminal UI library, port of `@earendil-works/pi-tui`.
//!
//! Component model: components render to styled lines for a viewport width;
//! the TUI core routes input and renders through one of two backends
//! (main-screen scrollback with differential line updates, or full-screen
//! alternate screen).

pub mod clipboard;
pub mod component;
pub mod components;
pub mod image;
pub mod input;
pub mod keys;
pub mod line;
pub mod screen_alt;
pub mod screen_main;
pub mod style;
pub mod syntax;
pub mod terminal;
pub mod tui;
pub mod util;

pub use component::{CURSOR_MARKER, Component, Focusable};
pub use input::{InputEvent, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
pub use line::{Line, Span, sanitize};
pub use style::{Color, Style};
pub use tui::{OverlayAnchor, OverlayHandle, OverlayOptions, Tui, TuiMode};
