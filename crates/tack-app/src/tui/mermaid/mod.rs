//! Mermaid diagram rendering facade.
//!
//! With the `mermaid` feature (default): mermaid-rs-renderer → PNG → inline
//! image (Kitty/iTerm2) or text-art / half-block truecolor fallback. Without
//! it the whole resvg/usvg/fontdb stack is compiled out and ```mermaid code
//! blocks render as plain code blocks (same as the `mermaid = "off"`
//! setting).

#[cfg(not(feature = "mermaid"))]
use tack_tui::Line;
#[cfg(not(feature = "mermaid"))]
use tack_tui::image::ImageProtocol;

#[cfg(feature = "mermaid")]
mod full;

#[cfg(feature = "mermaid")]
pub use full::mermaid_lines;
#[cfg(all(test, feature = "mermaid"))]
pub(crate) use full::render_svg_for_test;

/// Slim-build stub: no renderer compiled in, always fall back to a plain
/// code block.
#[cfg(not(feature = "mermaid"))]
pub fn mermaid_lines(
    _source: &str,
    _width_cells: u16,
    _protocol: Option<ImageProtocol>,
) -> Option<Vec<Line>> {
    None
}
