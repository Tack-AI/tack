//! The read tool. Port of `tools/read.ts` core logic, including image
//! resize (2000x2000 / 4.5MB base64 cap) via the `image` crate.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::path_utils::resolve_read_path;
use crate::services::ToolServices;
use crate::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadParams {
    /// Path to the file to read (relative or absolute)
    path: String,
    /// Line number to start reading from (1-indexed)
    offset: Option<usize>,
    /// Maximum number of lines to read
    limit: Option<usize>,
}

const IMAGE_MIME_TYPES: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
];

fn detect_image_mime_type(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    IMAGE_MIME_TYPES
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, m)| *m)
}

// --- image processing (port of image-resize-core.ts) ------------------------

/// 4.5MB of base64 payload — headroom below Anthropic's 5MB limit.
const MAX_IMAGE_BASE64_BYTES: usize = (4.5 * 1024.0 * 1024.0) as usize;
const MAX_IMAGE_DIMENSION: u32 = 2000;
/// Raw image files beyond this are refused up front (decoded pixels are
/// several times larger than the compressed file).
const MAX_IMAGE_FILE_BYTES: u64 = 128 * 1024 * 1024;
/// Decode-time caps (decompression-bomb defense): far above any legitimate
/// photo, far below anything that can exhaust memory.
const MAX_DECODE_DIMENSION: u32 = 32_768;
const MAX_DECODE_ALLOC: u64 = 1 << 30; // 1 GiB

/// Decode an image with resource limits. `image::load_from_memory` has no
/// caps, so a crafted file (e.g. a 100000x100000 PNG a few KB on disk)
/// would allocate gigabytes.
fn load_image_limited(data: &[u8]) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    reader.decode().ok()
}

fn base64_len(raw_bytes: usize) -> usize {
    raw_bytes.div_ceil(3) * 4
}

#[derive(Debug)]
pub struct ProcessedImage {
    pub data: String,
    pub mime_type: String,
    pub note: Option<String>,
}

/// Decode, downscale to 2000x2000 max, and re-encode under the base64 byte
/// cap (PNG vs JPEG q80, pick smaller; then cheaper JPEG; then progressive
/// downscale). Returns None if the image can't be decoded.
pub fn process_image(data: &[u8], mime_type: &str) -> Option<ProcessedImage> {
    use base64::Engine;
    let encode = base64::engine::general_purpose::STANDARD;

    // Passthrough when already small enough.
    if base64_len(data.len()) <= MAX_IMAGE_BASE64_BYTES {
        let img = load_image_limited(data)?;
        if img.width() <= MAX_IMAGE_DIMENSION && img.height() <= MAX_IMAGE_DIMENSION {
            return Some(ProcessedImage {
                data: encode.encode(data),
                mime_type: mime_type.to_string(),
                note: None,
            });
        }
    }

    let img = load_image_limited(data)?;
    let original = (img.width(), img.height());
    let mut current = img.thumbnail(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION);

    fn encode_png(img: &image::DynamicImage) -> Option<Vec<u8>> {
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
        Some(buf.into_inner())
    }
    fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Option<Vec<u8>> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let rgb = img.to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality)
            .encode_image(&rgb)
            .ok()?;
        Some(buf.into_inner())
    }

    for attempt in 0u8..12 {
        let mut candidates: Vec<(Vec<u8>, &str)> = Vec::new();
        if attempt == 0
            && let Some(png) = encode_png(&current)
        {
            candidates.push((png, "image/png"));
        }
        let quality = 80u8.saturating_sub(attempt.saturating_sub(1) * 10).max(30);
        if let Some(jpg) = encode_jpeg(&current, quality) {
            candidates.push((jpg, "image/jpeg"));
        }
        candidates.sort_by_key(|(bytes, _)| bytes.len());
        if let Some((bytes, mime)) = candidates.into_iter().next()
            && base64_len(bytes.len()) <= MAX_IMAGE_BASE64_BYTES
        {
            let note = if current.width() < original.0 || current.height() < original.1 {
                Some(format!(
                    "[Image resized from {}x{} to {}x{}]",
                    original.0,
                    original.1,
                    current.width(),
                    current.height()
                ))
            } else {
                None
            };
            return Some(ProcessedImage {
                data: encode.encode(bytes),
                mime_type: mime.to_string(),
                note,
            });
        }
        // Progressive downscale.
        let next_w = (current.width() / 2).max(1);
        let next_h = (current.height() / 2).max(1);
        if next_w == current.width() && next_h == current.height() {
            break;
        }
        current = current.thumbnail(next_w, next_h);
    }
    None
}

pub struct ReadTool {
    services: ToolServices,
}

/// Bytes of a single line retained in memory. Only needs to exceed the
/// output byte budget so truncate_head can flag first_line_exceeds_limit;
/// the full original length is tracked separately.
const COLLECT_LINE_CAP: usize = DEFAULT_MAX_BYTES + 1;

/// One line of the split('\n') view of a file, memory-capped.
#[derive(Debug)]
struct WindowLine {
    /// Lossy text, capped at COLLECT_LINE_CAP bytes.
    text: String,
    /// Original byte length (excluding the '\n' terminator).
    original_len: usize,
}

#[derive(Debug)]
struct WindowedRead {
    lines: Vec<WindowLine>,
    /// Line count using split('\n') semantics (a trailing newline leaves a
    /// final empty entry; an empty file is one empty line).
    total_lines: usize,
}

/// Stream `path` line by line, collecting the window starting at
/// `start_line` (0-based) with at most `max_lines` lines and roughly the
/// output byte budget — the file itself is never fully loaded, so
/// multi-GB logs cost bounded memory regardless of offset.
fn read_line_window(
    path: &std::path::Path,
    start_line: usize,
    max_lines: usize,
) -> std::io::Result<WindowedRead> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut lines: Vec<WindowLine> = Vec::new();
    let mut collected_bytes = 0usize;
    let mut total_lines = 0usize;
    let mut index = 0usize;
    let mut ended_with_newline = true; // empty file: trailing empty split piece
    loop {
        // Read one logical line (terminator excluded from the kept text).
        // fill_buf/consume keeps memory bounded by the BufReader buffer,
        // so a single 5GB line cannot exhaust it either.
        let mut kept: Vec<u8> = Vec::new();
        let mut original_len = 0usize;
        let mut terminated = false;
        let mut any = false;
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                break; // EOF
            }
            let (chunk, hit_newline) = match available.iter().position(|&b| b == b'\n') {
                Some(i) => (&available[..i], true),
                None => (available, false),
            };
            any = true;
            original_len += chunk.len();
            if kept.len() < COLLECT_LINE_CAP {
                let keep = (COLLECT_LINE_CAP - kept.len()).min(chunk.len());
                kept.extend_from_slice(&chunk[..keep]);
            }
            let consumed = chunk.len() + usize::from(hit_newline);
            reader.consume(consumed);
            if hit_newline {
                terminated = true;
                break;
            }
        }
        if !any {
            break; // EOF exactly at a line boundary
        }
        total_lines += 1;
        // Always collect at least the first window line (so the
        // first-line-exceeds-budget message keeps working); afterwards stop
        // once the output budget could no longer fit another line.
        if index >= start_line
            && lines.len() < max_lines
            && (collected_bytes <= DEFAULT_MAX_BYTES || lines.is_empty())
        {
            collected_bytes += original_len + 1;
            lines.push(WindowLine {
                text: String::from_utf8_lossy(&kept).into_owned(),
                original_len,
            });
        }
        index += 1;
        ended_with_newline = terminated;
        if !terminated {
            break; // final line without a trailing newline
        }
    }
    if ended_with_newline {
        // split('\n') semantics: a trailing '\n' (or an empty file) leaves a
        // final empty entry.
        if index >= start_line && lines.len() < max_lines {
            lines.push(WindowLine {
                text: String::new(),
                original_len: 0,
            });
        }
        total_lines += 1;
    }
    Ok(WindowedRead { lines, total_lines })
}

impl ReadTool {
    pub fn new(services: ToolServices) -> Self {
        ReadTool { services }
    }
}

impl std::fmt::Debug for ReadTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadTool").finish()
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn label(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<ReadParams>()
    }

    fn constrained_sampling(&self) -> Option<tack_ai::constrained_sampling::ConstrainedSampling> {
        crate::prefer_strict_sampling()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: ReadParams =
            serde_json::from_value(params).map_err(|e| format!("invalid read params: {e}"))?;
        if cancel.is_cancelled() {
            return Err("Operation aborted".to_string());
        }

        let absolute = resolve_read_path(&params.path, &self.services.cwd);
        let meta = std::fs::metadata(&absolute)
            .map_err(|e| format!("Could not read file: {}. {e}", params.path))?;
        if meta.is_dir() {
            return Err(format!(
                "{} is a directory, use ls to list its contents",
                params.path
            ));
        }

        if let Some(mime) = detect_image_mime_type(&absolute) {
            if meta.len() > MAX_IMAGE_FILE_BYTES {
                return Ok(AgentToolResult::text(format!(
                    "Read image file [{mime}]\n[Image omitted: file is {}, exceeds the {} limit.]",
                    format_size(meta.len() as usize),
                    format_size(MAX_IMAGE_FILE_BYTES as usize)
                )));
            }
            // Blocking IO + CPU-heavy decode/re-encode belong off the
            // async worker (same pattern as grep/find).
            let read_path = params.path.clone();
            let image_path = absolute.clone();
            let processed = tokio::task::spawn_blocking(move || {
                let data = std::fs::read(&image_path)
                    .map_err(|e| format!("Could not read file: {read_path}. {e}"))?;
                Ok::<Option<ProcessedImage>, String>(process_image(&data, mime))
            })
            .await
            .map_err(|e| format!("read image task failed: {e}"))??;
            if cancel.is_cancelled() {
                return Err("Operation aborted".to_string());
            }
            let Some(processed) = processed else {
                return Ok(AgentToolResult::text(format!(
                    "Read image file [{mime}]\n[Image omitted: could not be processed/resized below the inline image size limit.]"
                )));
            };
            let mut note = format!("Read image file [{}]", processed.mime_type);
            if let Some(extra) = processed.note {
                note.push_str(&format!("\n{extra}"));
            }
            return Ok(AgentToolResult {
                content: vec![
                    tack_ai::InputContentBlock::text(note),
                    tack_ai::InputContentBlock::Image {
                        data: processed.data,
                        mime_type: processed.mime_type,
                    },
                ],
                details: Value::Object(serde_json::Map::new()),
                usage: None,
                terminate: false,
                added_tool_names: None,
            });
        }

        let start_line = params.offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
        let start_line_display = start_line + 1;
        // Windowed read: only the requested lines (bounded by the output
        // budget) are held in memory; the rest of the file is streamed for
        // the total line count. Without an explicit limit, collect one
        // lookahead line past the truncation budget so truncate_head flags
        // truncation exactly as a full read would.
        let max_collect = params
            .limit
            .unwrap_or(crate::truncate::DEFAULT_MAX_LINES.saturating_add(1));
        // Windowed read on a blocking thread: the BufRead scan walks the
        // whole file for the total line count, so multi-GB logs must not
        // stall the async runtime (same pattern as grep/find).
        let read_path = params.path.clone();
        let window_path = absolute.clone();
        let windowed = tokio::task::spawn_blocking(move || {
            read_line_window(&window_path, start_line, max_collect)
                .map_err(|e| format!("Could not read file: {read_path}. {e}"))
        })
        .await
        .map_err(|e| format!("read task failed: {e}"))??;
        if cancel.is_cancelled() {
            return Err("Operation aborted".to_string());
        }
        let total_file_lines = windowed.total_lines;
        let window = windowed.lines;

        if start_line >= total_file_lines {
            return Err(format!(
                "Offset {} is beyond end of file ({} lines total)",
                params.offset.unwrap_or(0),
                total_file_lines
            ));
        }

        // Lines in the selected range (before output-budget truncation).
        let selected_line_count =
            (total_file_lines - start_line).min(params.limit.unwrap_or(usize::MAX));
        let selected = window
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let user_limited_lines = params.limit.map(|_| selected_line_count);

        let truncation = truncate_head(&selected, None, None);
        let output_text = if truncation.first_line_exceeds_limit {
            let first_line_size = format_size(window.first().map(|l| l.original_len).unwrap_or(0));
            format!(
                "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES),
                params.path
            )
        } else if truncation.truncated {
            let end_line_display = start_line_display + truncation.output_lines - 1;
            let next_offset = end_line_display + 1;
            let mut text = truncation.content.clone();
            if truncation.truncated_by == Some(crate::truncate::TruncatedBy::Lines) {
                text.push_str(&format!(
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
                ));
            } else {
                text.push_str(&format!(
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                    format_size(DEFAULT_MAX_BYTES)
                ));
            }
            text
        } else if let Some(limited) = user_limited_lines {
            if start_line + limited < total_file_lines {
                let remaining = total_file_lines - (start_line + limited);
                let next_offset = start_line + limited + 1;
                format!(
                    "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                    truncation.content
                )
            } else {
                truncation.content.clone()
            }
        } else {
            truncation.content.clone()
        };

        let details = if truncation.truncated {
            serde_json::json!({ "truncation": {
                "truncatedBy": truncation.truncated_by.as_ref().map(|t| match t {
                    crate::truncate::TruncatedBy::Lines => "lines",
                    crate::truncate::TruncatedBy::Bytes => "bytes",
                }),
                "totalLines": selected_line_count,
                "outputLines": truncation.output_lines,
            }})
        } else {
            Value::Object(serde_json::Map::new())
        };

        Ok(AgentToolResult {
            content: vec![tack_ai::InputContentBlock::text(output_text)],
            details,
            usage: None,
            terminate: false,
            added_tool_names: None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn text_of(result: &AgentToolResult) -> String {
        match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn huge_limit_does_not_overflow() {
        // start_line + limit must saturate, not overflow (panic in debug).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let tool = ReadTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "path": "f.txt", "limit": u64::MAX }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("a\nb\nc"), "{text}");
        assert!(!text.contains("more lines"), "{text}");
    }

    #[tokio::test]
    async fn offset_and_limit_line_math() {
        let tmp = tempfile::tempdir().unwrap();
        let content = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("f.txt"), content).unwrap();
        let tool = ReadTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "path": "f.txt", "offset": 3, "limit": 2 }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.starts_with("line3\nline4"), "{text}");
        assert!(
            text.contains("6 more lines in file. Use offset=5 to continue."),
            "{text}"
        );

        // Offset beyond EOF is a clean error.
        let err = tool
            .execute(
                "2",
                serde_json::json!({ "path": "f.txt", "offset": 999 }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(err.contains("beyond end of file"), "{err}");
    }

    /// Windowed reading: a deep offset into a many-line file must return the
    /// exact window and the exact total (the file is streamed, not fully
    /// materialized).
    #[tokio::test]
    async fn deep_offset_in_large_file() {
        let tmp = tempfile::tempdir().unwrap();
        let total = 100_000;
        let content = (1..=total)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("big.log"), &content).unwrap();
        let tool = ReadTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "path": "big.log", "offset": 99_999, "limit": 2 }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.starts_with("line99999\nline100000"), "{text}");
        assert!(!text.contains("more lines"), "{text}");

        // Beyond-EOF error reports the streamed total.
        let err = tool
            .execute(
                "2",
                serde_json::json!({ "path": "big.log", "offset": 100_001 }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(err.contains("100000 lines total"), "{err}");
    }

    /// split('\n') parity: a trailing newline leaves a final empty entry in
    /// the total line count; default reads of >2000-line files truncate with
    /// the same notice as the old full-read path.
    #[tokio::test]
    async fn line_count_and_truncation_parity() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("nl.txt"), "a\nb\n").unwrap();
        let tool = ReadTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let err = tool
            .execute(
                "1",
                serde_json::json!({ "path": "nl.txt", "offset": 4 }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(err.contains("3 lines total"), "{err}");

        let content = (1..=5000)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("many.txt"), &content).unwrap();
        let result = tool
            .execute(
                "2",
                serde_json::json!({ "path": "many.txt" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("[Showing lines 1-2000 of 5000. Use offset=2001 to continue.]"),
            "{text}"
        );
    }

    /// A single line larger than the byte budget is flagged (with its true
    /// size) instead of being materialized in full.
    #[tokio::test]
    async fn single_huge_line_is_flagged_not_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        let big_line = "x".repeat(2 * 1024 * 1024);
        std::fs::write(tmp.path().join("long.txt"), format!("{big_line}\nshort\n")).unwrap();
        let tool = ReadTool::new(ToolServices::new(tmp.path().to_path_buf()));
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "path": "long.txt" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("Line 1 is 2.0MB, exceeds 50.0KB limit"),
            "{text}"
        );
    }

    /// Decompression-bomb defense: a PNG declaring absurd dimensions must
    /// fail the decode limits instead of allocating gigabytes.
    #[test]
    fn oversized_png_dimensions_are_rejected() {
        let png = minimal_png(100_000, 100_000);
        assert!(load_image_limited(&png).is_none());
        assert!(process_image(&png, "image/png").is_none());
    }

    /// Build a structurally valid PNG header (signature + IHDR) with the
    /// given dimensions; enough for the decoder to learn the dimensions.
    fn minimal_png(width: u32, height: u32) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
        let crc = crc32(&ihdr);
        png.extend_from_slice(&((ihdr.len() - 4) as u32).to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc.to_be_bytes());
        png
    }
}
