//! Inline image rendering: Kitty graphics protocol and iTerm2 inline images
//! (port of `terminal-image.ts` emission). Renders as zero-width raw spans +
//! reserved blank rows so the diff renderer leaves the image alone when
//! unchanged.

use base64::Engine;

use crate::line::{Line, Span};

/// Terminal image protocol in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageProtocol {
    Kitty,
    ITerm2,
}

static NEXT_IMAGE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Cap on half-block fallback rows. Without it, a very tall image in a
/// terminal without Kitty/iTerm2 support would flood the transcript cache
/// with tens of thousands of RGB-styled spans that never leave.
const MAX_HALF_BLOCK_ROWS: u32 = 200;

/// Render an image (PNG bytes) as lines for the transcript. `max_width_cells`
/// caps display width; rows reserved = estimated height.
pub fn image_lines(
    png: &[u8],
    protocol: ImageProtocol,
    max_width_cells: u16,
    cell_pixel_width: u32,
    cell_pixel_height: u32,
    pixel_width: u32,
    pixel_height: u32,
) -> Vec<Line> {
    let id = NEXT_IMAGE_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // A zero cell size (unknown terminal geometry) must not divide by zero.
    let cell_pixel_width = cell_pixel_width.max(1);
    let cell_pixel_height = cell_pixel_height.max(1);
    let display_width_cells = if pixel_width == 0 {
        max_width_cells
    } else {
        pixel_width
            .div_ceil(cell_pixel_width)
            .min(max_width_cells as u32)
            .max(1) as u16
    };
    let rows = if pixel_height == 0 {
        10
    } else {
        // Rows follow the scaled width (u64 math: pixel_height * scale can
        // exceed u32 for very tall images).
        let scale = display_width_cells as u64 * cell_pixel_width as u64;
        let scaled_height = pixel_height as u64 * scale / pixel_width.max(1) as u64;
        scaled_height
            .div_ceil(cell_pixel_height as u64)
            .max(1)
            .min(u16::MAX as u64) as u16
    };
    let mut lines = Vec::new();
    match protocol {
        ImageProtocol::Kitty => {
            // The whole transmission goes in ONE line (TS terminal-image.ts):
            // per-chunk lines would each occupy a frame row, breaking the
            // layout with phantom rows. Continuation chunks carry ONLY m=;
            // some terminals (Warp) fail to reassemble when i= is repeated.
            // q=2 suppresses terminal responses (they'd be read as input).
            let params = format!("a=T,f=100,q=2,i={id},c={display_width_cells},r={rows}");
            // Base64 is encoded ONCE, straight into the final transmission
            // buffer (never a standalone base64 String that then gets
            // re-copied): 4096 base64 chars = exactly 3072 input bytes, and
            // 3072 % 3 == 0, so per-block encoding is byte-identical to
            // encoding the whole payload and chunking the text at 4096.
            const BLOCK: usize = 4096 / 4 * 3;
            let b64_len = png.len().div_ceil(3) * 4;
            let n_chunks = png.len().div_ceil(BLOCK);
            // delete seq + first-chunk params + per-chunk escape overhead.
            let mut transmission =
                String::with_capacity(b64_len + 32 + params.len() + n_chunks * 16);
            // Delete any previous placement of this id first: re-renders
            // re-emit the transmission, and stacking placements ghost.
            transmission.push_str(&format!("\x1b_Ga=d,d=i,i={id},q=2\x1b\\"));
            for (i, block) in png.chunks(BLOCK).enumerate() {
                let last = i + 1 == n_chunks;
                if i == 0 {
                    transmission.push_str(&format!("\x1b_G{params}"));
                    if !last {
                        transmission.push_str(",m=1");
                    }
                } else {
                    transmission.push_str(if last { "\x1b_Gm=0" } else { "\x1b_Gm=1" });
                }
                transmission.push(';');
                base64::engine::general_purpose::STANDARD.encode_string(block, &mut transmission);
                transmission.push_str("\x1b\\");
            }
            lines.push(Line::from_spans(vec![Span::raw(transmission)]));
        }
        ImageProtocol::ITerm2 => {
            // Same single-allocation discipline: encode the base64 straight
            // into the escape sequence's final buffer.
            let prefix = format!(
                "\x1b]1337;File=inline=1;width={display_width_cells};preserveAspectRatio=1:"
            );
            let mut out = String::with_capacity(prefix.len() + png.len().div_ceil(3) * 4 + 1);
            out.push_str(&prefix);
            base64::engine::general_purpose::STANDARD.encode_string(png, &mut out);
            out.push('\x07');
            lines.push(Line::from_spans(vec![Span::raw(out)]));
        }
    }
    // Reserve the image's rows so following content doesn't overlap.
    for _ in 1..rows {
        lines.push(Line::new());
    }
    lines
}

/// Estimate pixel size from PNG IHDR (no decode needed).
pub fn png_dimensions(png: &[u8]) -> (u32, u32) {
    // IHDR starts at byte 16: 4-byte BE width, 4-byte BE height.
    if png.len() >= 24 && &png[12..16] == b"IHDR" {
        let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        (width, height)
    } else {
        (0, 0)
    }
}

/// Half-block fallback renderer: "▀" with fg=top pixel / bg=bottom pixel
/// (truecolor) so images display in ANY terminal — Windows Terminal included —
/// when no Kitty/iTerm2 image protocol is available. Each terminal row shows
/// two pixel rows. Returns `None` when the PNG can't be decoded.
pub fn half_block_lines(png: &[u8], max_width_cells: u16) -> Option<Vec<Line>> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .ok()?
        .to_rgba8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let target_w = u32::from(max_width_cells).min(w).max(1);
    // Cap the pixel height at 2 pixel rows per terminal row.
    let target_h = (h * target_w / w).clamp(1, MAX_HALF_BLOCK_ROWS * 2);
    let img = image::imageops::resize(
        &img,
        target_w,
        target_h,
        image::imageops::FilterType::Triangle,
    );
    let mut lines = Vec::new();
    let mut y = 0;
    while y < target_h {
        let mut line = Line::new();
        for x in 0..target_w {
            let top = img.get_pixel(x, y);
            let bottom = if y + 1 < target_h {
                *img.get_pixel(x, y + 1)
            } else {
                image::Rgba([0, 0, 0, 0])
            };
            let fg = crate::style::Color::Rgb(top[0], top[1], top[2]);
            let bg = if bottom[3] == 0 {
                crate::style::Color::Default
            } else {
                crate::style::Color::Rgb(bottom[0], bottom[1], bottom[2])
            };
            line.push(Span::styled("▀", crate::style::Style::new().fg(fg).bg(bg)));
        }
        lines.push(line);
        y += 2;
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use image::ImageEncoder;

    /// Reference implementation of the ORIGINAL emission path: encode the
    /// whole payload to base64 first, then chunk the text at 4096 chars.
    /// The streaming per-block encoder in `image_lines` must produce the
    /// exact same terminal bytes.
    fn reference_kitty_transmission(
        png: &[u8],
        id: u32,
        display_width_cells: u16,
        rows: u16,
    ) -> String {
        let base64 = base64::engine::general_purpose::STANDARD.encode(png);
        let params = format!("a=T,f=100,q=2,i={id},c={display_width_cells},r={rows}");
        let mut transmission = String::new();
        transmission.push_str(&format!("\x1b_Ga=d,d=i,i={id},q=2\x1b\\"));
        let mut chunks = base64.as_bytes().chunks(4096).peekable();
        let mut first = true;
        while let Some(chunk) = chunks.next() {
            let chunk = String::from_utf8_lossy(chunk);
            if first {
                first = false;
                if chunks.peek().is_some() {
                    transmission.push_str(&format!("\x1b_G{params},m=1;{chunk}\x1b\\"));
                } else {
                    transmission.push_str(&format!("\x1b_G{params};{chunk}\x1b\\"));
                }
            } else if chunks.peek().is_some() {
                transmission.push_str(&format!("\x1b_Gm=1;{chunk}\x1b\\"));
            } else {
                transmission.push_str(&format!("\x1b_Gm=0;{chunk}\x1b\\"));
            }
        }
        transmission
    }

    /// Pseudo-random bytes (deterministic, no rng dep).
    fn noisy_bytes(n: usize) -> Vec<u8> {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 32) as u8
            })
            .collect()
    }

    /// Extract the image id out of a Kitty transmission line (the delete
    /// sequence is always present, even for an empty payload).
    fn kitty_id(transmission: &str) -> u32 {
        let marker = "\x1b_Ga=d,d=i,i=";
        let start = transmission.find(marker).unwrap() + marker.len();
        transmission[start..]
            .split([';', ',', '\x1b'])
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn kitty_streaming_encode_is_byte_identical_to_chunked_whole() {
        // Sizes straddling the 3072-byte block boundary (incl. 0 and exact
        // multiples) so padding/chunk-edge behavior is covered.
        for n in [0usize, 1, 3071, 3072, 3073, 6144, 10_000] {
            let png = noisy_bytes(n);
            let lines = image_lines(&png, ImageProtocol::Kitty, 40, 9, 18, 400, 200);
            let got = lines[0].text();
            let want = reference_kitty_transmission(&png, kitty_id(&got), 40, lines.len() as u16);
            assert_eq!(got, want, "byte mismatch at png len {n}");
        }
    }

    #[test]
    fn iterm2_encode_is_byte_identical_to_format_path() {
        for n in [0usize, 1, 2, 3, 100, 10_000] {
            let png = noisy_bytes(n);
            let lines = image_lines(&png, ImageProtocol::ITerm2, 40, 9, 18, 0, 0);
            let base64 = base64::engine::general_purpose::STANDARD.encode(&png);
            let want =
                format!("\x1b]1337;File=inline=1;width=40;preserveAspectRatio=1:{base64}\x07");
            assert_eq!(lines[0].text(), want, "byte mismatch at png len {n}");
        }
    }

    #[test]
    fn image_line_clones_share_the_payload() {
        // The whole point of `Arc<str>` span text: cloning a Line (diff
        // caches, transcript retention) bumps a refcount instead of copying
        // megabytes of base64.
        let png = noisy_bytes(5000);
        for protocol in [ImageProtocol::Kitty, ImageProtocol::ITerm2] {
            let lines = image_lines(&png, protocol, 40, 9, 18, 400, 200);
            let cloned = lines[0].clone();
            assert!(
                std::sync::Arc::ptr_eq(&lines[0].spans[0].text, &cloned.spans[0].text),
                "{protocol:?} payload was deep-copied on clone"
            );
        }
    }

    #[test]
    fn kitty_chunks_and_rows() {
        let png = vec![0u8; 5000];
        let lines = image_lines(&png, ImageProtocol::Kitty, 40, 9, 18, 400, 200);
        // The whole transmission lives in ONE line (per-chunk lines would
        // each occupy a frame row); continuation chunks carry only m=.
        let transmission = lines[0].text();
        assert!(
            transmission.contains("\x1b_Ga=d,d=i,"),
            "delete-first: {transmission:?}"
        );
        assert!(
            transmission.contains("\x1b_Ga=T,f=100,q=2,"),
            "params: {transmission:?}"
        );
        assert!(transmission.contains(",c=40,"), "{transmission:?}");
        // 5000 bytes → 6668 base64 chars → 2 chunks, all inside one line.
        assert_eq!(transmission.matches("\x1b_G").count(), 3); // delete + first + last
        assert!(transmission.contains("\x1b_Gm=1;") || transmission.contains("\x1b_Gm=0;"));
        assert!(lines.len() > 2, "reserved rows present");
        for line in &lines {
            assert_eq!(line.width(), 0);
        }
    }

    #[test]
    fn iterm2_single_escape() {
        let png = vec![1u8; 100];
        let lines = image_lines(&png, ImageProtocol::ITerm2, 40, 9, 18, 90, 90);
        assert!(lines[0].text().starts_with("\x1b]1337;File=inline=1;"));
    }

    #[test]
    fn zero_cell_size_does_not_panic() {
        // Regression: cell_pixel_width/height of 0 (unknown terminal
        // geometry) divided by zero.
        let lines = image_lines(&[0u8; 8], ImageProtocol::ITerm2, 40, 0, 0, 400, 200);
        assert!(!lines.is_empty());
    }

    #[test]
    fn png_ihdr() {
        // Minimal header: 8-byte magic + len + "IHDR" + w + h.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), (640, 480));
        assert_eq!(png_dimensions(b"nope"), (0, 0));
    }

    #[test]
    fn half_block_height_is_capped() {
        // Regression: a very tall image produced tens of thousands of
        // styled-span rows that lived in the transcript cache forever.
        let width = 16;
        let height = 20_000;
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 255]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        // max_width_cells > image width, so no downscale saves us: the cap
        // is the only thing bounding the row count.
        let lines = half_block_lines(&png, 80).unwrap();
        assert_eq!(lines.len(), MAX_HALF_BLOCK_ROWS as usize);
    }

    #[test]
    fn half_block_short_image_uncapped() {
        let width = 8;
        let height = 10;
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 255, 0, 255]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        let lines = half_block_lines(&png, 80).unwrap();
        assert_eq!(lines.len(), 5); // two pixel rows per terminal row
    }
}
