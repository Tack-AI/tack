//! `tack debug-image`: terminal image protocol probe. Prints the detected
//! capabilities, then emits the same test image via Kitty, iTerm2, and
//! half-block so a user can report which ones their terminal renders.

use std::io::Write as _;

/// A 96x64 test card (four color quadrants + white cross) as PNG bytes.
fn test_png() -> Vec<u8> {
    let mut img = image::RgbaImage::new(96, 64);
    for y in 0..64u32 {
        for x in 0..96u32 {
            let mut px = if y < 32 {
                if x < 48 {
                    image::Rgba([200, 40, 40, 255])
                } else {
                    image::Rgba([40, 160, 60, 255])
                }
            } else if x < 48 {
                image::Rgba([40, 80, 200, 255])
            } else {
                image::Rgba([220, 180, 40, 255])
            };
            if x.abs_diff(y * 96 / 64) < 2 || (95 - x).abs_diff(y * 96 / 64) < 2 {
                px = image::Rgba([255, 255, 255, 255]);
            }
            img.put_pixel(x, y, px);
        }
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .expect("png encode");
    buf.into_inner()
}

pub fn run() -> anyhow::Result<()> {
    let caps = tack_tui::terminal::Capabilities::detect();
    println!(
        "TERM_PROGRAM={:?} TERM={:?}",
        std::env::var("TERM_PROGRAM").unwrap_or_default(),
        std::env::var("TERM").unwrap_or_default()
    );
    println!(
        "detected: kitty_images={} iterm2_images={} kitty_keyboard={} multiplexed={}",
        caps.kitty_images, caps.iterm2_images, caps.kitty_keyboard, caps.multiplexed
    );
    println!();

    let png = test_png();
    let mut out = std::io::stdout();

    println!("--- kitty (expect a color test card below this line) ---");
    for line in tack_tui::image::image_lines(
        &png,
        tack_tui::image::ImageProtocol::Kitty,
        48,
        9,
        18,
        96,
        64,
    ) {
        out.write_all(line.to_ansi().as_bytes())?;
        out.write_all(b"\r\n")?;
    }
    out.flush()?;

    println!("--- iterm2 (expect the same card below this line) ---");
    for line in tack_tui::image::image_lines(
        &png,
        tack_tui::image::ImageProtocol::ITerm2,
        48,
        9,
        18,
        96,
        64,
    ) {
        out.write_all(line.to_ansi().as_bytes())?;
        out.write_all(b"\r\n")?;
    }
    out.flush()?;

    println!("--- half-block (always expect a coarse card) ---");
    if let Some(lines) = tack_tui::image::half_block_lines(&png, 48) {
        for line in lines {
            out.write_all(line.to_ansi().as_bytes())?;
            out.write_all(b"\r\n")?;
        }
    }
    out.flush()?;
    Ok(())
}
