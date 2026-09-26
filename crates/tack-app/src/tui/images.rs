//! Image attachments: `@path` references in the editor expand to image
//! blocks (vision models) or inlined text files; clipboard image paste saves
//! a temp PNG and inserts the reference. Port of TS pi's image attachment +
//! paste-image behaviors.

use std::path::{Path, PathBuf};

use tack_ai::{InputContentBlock, InputKind, Model, UserContent};

/// Expand `@path` tokens in a prompt into content blocks (images attach as
/// image blocks when the model supports them; other files inline as text).
pub fn expand_attachments(text: &str, model: &Model, cwd: &Path) -> UserContent {
    expand_attachments_with(text, model, cwd, false)
}

/// `block_images` (settings images.blockImages): image attachments are never
/// sent to the LLM — they're inlined as text paths instead.
pub fn expand_attachments_with(
    text: &str,
    model: &Model,
    cwd: &Path,
    block_images: bool,
) -> UserContent {
    let tokens = parse_at_tokens(text);
    if tokens.is_empty() {
        return UserContent::Text(text.to_string());
    }
    let supports_images = model.input.contains(&InputKind::Image) && !block_images;
    let mut blocks: Vec<InputContentBlock> = Vec::new();
    let mut remaining = text.to_string();
    let mut attached = false;
    for (raw, path) in tokens {
        // Strip the token from the text part.
        remaining = remaining.replacen(&raw, &path.display().to_string(), 1);
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(&path)
        };
        let is_image = absolute
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp"
                )
            });
        if is_image && supports_images {
            match load_image_block(&absolute) {
                Ok(block) => {
                    blocks.push(block);
                    attached = true;
                }
                Err(e) => {
                    blocks.push(InputContentBlock::text(format!(
                        "[could not read image: {}: {e}]",
                        absolute.display()
                    )));
                }
            }
        } else {
            if let Ok(content) = std::fs::read_to_string(absolute.as_path()) {
                let truncated = if content.len() > 100_000 {
                    // Byte-based slicing can split a multi-byte UTF-8 char; fall
                    // back to the nearest char boundary to avoid a panic.
                    let mut end = 100_000;
                    while !content.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}…[truncated]", &content[..end])
                } else {
                    content
                };
                blocks.push(InputContentBlock::text(format!(
                    "\n<file path=\"{}\">\n{}\n</file>\n",
                    absolute.display(),
                    truncated
                )));
                attached = true;
            }
        }
    }
    if !attached {
        return UserContent::Text(text.to_string());
    }
    blocks.insert(0, InputContentBlock::text(remaining));
    UserContent::Blocks(blocks)
}

/// Parse `@path` tokens (quoted or bare). Returns (raw token, path).
pub fn parse_at_tokens(text: &str) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for (i, c) in text.char_indices() {
        if c != '@' {
            continue;
        }
        // Must start a token (start of text or after whitespace).
        if i > 0 && !text[..i].ends_with(char::is_whitespace) {
            continue;
        }
        let rest = &text[i + 1..];
        let (path, raw_len) = if let Some(quoted) = rest.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (quoted[..end].to_string(), end + 2),
                None => continue,
            }
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (rest[..end].to_string(), end)
        };
        if path.is_empty() {
            continue;
        }
        out.push((text[i..i + 1 + raw_len].to_string(), PathBuf::from(path)));
    }
    out
}

/// Largest image attachment accepted (decoded base64 inflates this by
/// ~4/3 on the wire; 20MB of bytes ≈ 27MB of base64).
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

fn load_image_block(path: &Path) -> Result<InputContentBlock, String> {
    use base64::Engine;
    // metadata() follows symlinks: reject anything that isn't a regular
    // file AFTER resolution — an unguarded read() on a named pipe would
    // block forever, and a device file could stream unbounded bytes.
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{} is too large ({} bytes, limit {} bytes)",
            path.display(),
            metadata.len(),
            MAX_IMAGE_BYTES
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| format!("{} has no extension", path.display()))?
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    Ok(InputContentBlock::Image {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type: mime.to_string(),
    })
}

/// Paste a clipboard image: save to a temp PNG, returns the path to insert.
pub fn paste_clipboard_image() -> Option<PathBuf> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    let rgba = image::RgbaImage::from_raw(
        image.width as u32,
        image.height as u32,
        image.bytes.into_owned(),
    )?;
    let path = std::env::temp_dir().join(format!("tack-paste-{}.png", tack_ai::now_millis()));
    image::DynamicImage::ImageRgba8(rgba).save(&path).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parses_at_tokens() {
        let tokens = parse_at_tokens("check @src/main.rs and @\"my file.txt\" end");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].1, PathBuf::from("src/main.rs"));
        assert_eq!(tokens[1].1, PathBuf::from("my file.txt"));
        // '@' inside an email is not a token.
        assert!(parse_at_tokens("mail me at a@b.com").is_empty());
    }

    #[test]
    fn image_load_rejects_non_regular_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        // Directory: not a regular file.
        let err = load_image_block(dir.path()).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        // Missing file: clear stat error.
        let err = load_image_block(&dir.path().join("nope.png")).unwrap_err();
        assert!(err.contains("cannot stat"), "{err}");
        // Over the byte cap (sparse file): clear size error, no read.
        let big = dir.path().join("big.png");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        drop(f);
        let err = load_image_block(&big).unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[test]
    fn expands_text_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello file").unwrap();
        let model = Model {
            id: "m".into(),
            name: "m".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputKind::Text, InputKind::Image],
            cost: Default::default(),
            context_window: 1000,
            max_tokens: 100,
            sampling_params: None,
            headers: None,
            compat: None,
        };
        let path = dir.path().join("notes.txt");
        let text = format!("read @{path}", path = path.display());
        let content = expand_attachments(&text, &model, dir.path());
        let UserContent::Blocks(blocks) = content else {
            panic!("expected blocks")
        };
        assert!(blocks.iter().any(
            |b| matches!(b, InputContentBlock::Text { text, .. } if text.contains("hello file"))
        ));
    }
}
