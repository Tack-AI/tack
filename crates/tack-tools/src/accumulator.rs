//! Streaming output accumulator with bounded memory and temp-file spillover.
//! Simplified port of `tools/output-accumulator.ts`: keeps the head of the
//! output in memory (bounded at 4MB) for snapshots; once the cap is hit,
//! everything streams into a temp file so the persisted `fullOutputPath`
//! really is the FULL output (the TS original streams to disk even earlier,
//! at 50KB).

use std::io::Write;
use std::path::PathBuf;

use crate::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationResult, format_size, truncate_tail,
    untruncated_fast,
};

// 4MB, not 32MB: snapshots are tail-truncated to DEFAULT_MAX_BYTES (50KB)
// anyway, so the in-memory buffer only ever feeds pre-spill snapshots —
// anything past the cap lands in the spill file and is just as complete.
// Every live background task holds one of these buffers, so the real cost
// is N tasks × the cap (32MB turned a handful of chatty tasks into 100+MB).
const MAX_IN_MEMORY_BYTES: usize = 4 * 1024 * 1024;

/// Hard cap on the spill file: an unbounded stream (`yes`, a build loop)
/// would otherwise fill the disk. Past the cap, output is discarded and
/// the snapshot is marked truncated-due-to-spill-limit; the in-memory
/// head keeps working, so snapshots stay useful.
const MAX_SPILL_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug)]
pub struct OutputSnapshot {
    pub content: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<PathBuf>,
}

/// Temp file receiving the overflow once the in-memory cap is hit.
#[derive(Debug)]
struct Spill {
    path: PathBuf,
    file: std::fs::File,
    /// Bytes written so far (drives the spill cap).
    written: usize,
    /// True once MAX_SPILL_BYTES was hit: further output is discarded.
    capped: bool,
}

impl Spill {
    fn write(&mut self, data: &[u8], max_bytes: usize) {
        if self.capped {
            return;
        }
        let take = data.len().min(max_bytes.saturating_sub(self.written));
        if take > 0 {
            let _ = self.file.write_all(&data[..take]);
            self.written += take;
        }
        if take < data.len() {
            self.capped = true;
        }
    }
}

#[derive(Debug)]
pub struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    max_in_memory: usize,
    buffer: Vec<u8>,
    temp_file_prefix: String,
    /// Some once the in-memory cap was hit: the buffer PLUS every subsequent
    /// chunk has been written here.
    spill: Option<Spill>,
    /// Lazily created on the first persist and reused (overwritten) after:
    /// background tasks snapshot(true) on a throttle, and a fresh random
    /// path per call leaked a temp file per snapshot.
    persist_path: std::sync::OnceLock<PathBuf>,
    max_spill_bytes: usize,
}

impl OutputAccumulator {
    pub fn new(temp_file_prefix: &str) -> Self {
        OutputAccumulator {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            max_in_memory: MAX_IN_MEMORY_BYTES,
            buffer: Vec::new(),
            temp_file_prefix: temp_file_prefix.to_string(),
            spill: None,
            persist_path: std::sync::OnceLock::new(),
            max_spill_bytes: MAX_SPILL_BYTES,
        }
    }

    #[cfg(test)]
    fn with_in_memory_cap(temp_file_prefix: &str, cap: usize) -> Self {
        OutputAccumulator {
            max_in_memory: cap,
            ..Self::new(temp_file_prefix)
        }
    }

    pub fn append(&mut self, data: &[u8]) {
        let max_spill = self.max_spill_bytes;
        if let Some(spill) = &mut self.spill {
            spill.write(data, max_spill);
            return;
        }
        let remaining = self.max_in_memory.saturating_sub(self.buffer.len());
        if data.len() <= remaining {
            self.buffer.extend_from_slice(data);
            return;
        }
        // Cap reached mid-chunk: keep exactly `max_in_memory` bytes in the
        // buffer and start the spill file with the buffer + the overflow so
        // the persisted full output stays complete for unbounded streams.
        self.buffer.extend_from_slice(&data[..remaining]);
        let Some(mut spill) = self.open_spill() else {
            return;
        };
        spill.write(&self.buffer, max_spill);
        spill.write(&data[remaining..], max_spill);
        self.spill = Some(spill);
    }

    fn open_spill(&self) -> Option<Spill> {
        let id: String = rand::random::<[u8; 8]>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let path = std::env::temp_dir().join(format!("{}-{id}.log", self.temp_file_prefix));
        let file = std::fs::File::create(&path).ok()?;
        Some(Spill {
            path,
            file,
            written: 0,
            capped: false,
        })
    }

    fn full_text(&self) -> String {
        String::from_utf8_lossy(&self.buffer).to_string()
    }

    /// Snapshot with tail truncation; persists the full output to a temp file
    /// when truncated (pi's `fullOutputPath` pattern).
    pub fn snapshot(&self, persist_if_truncated: bool) -> OutputSnapshot {
        let full = self.full_text();
        // Fast path: snapshots are taken on a throttle while output streams
        // in, and running the full split+count truncation over up to 4MB
        // every 100ms is wasted work when the buffer plainly fits both
        // budgets. Byte length is O(1); the newline scan is ~memcmp speed
        // and avoids building the line vec + join entirely.
        let fits_bytes = full.len() <= self.max_bytes;
        let fits_lines = fits_bytes && crate::truncate::count_lines(&full) <= self.max_lines;
        let truncation = if fits_lines {
            untruncated_fast(&full, self.max_lines, self.max_bytes)
        } else {
            truncate_tail(&full, Some(self.max_lines), Some(self.max_bytes))
        };
        let full_output_path = if truncation.truncated && persist_if_truncated {
            self.persist()
        } else {
            None
        };
        let mut content = truncation.content.clone();
        let mut truncation = truncation;
        if self.spill.as_ref().is_some_and(|spill| spill.capped) {
            // The spill file is INCOMPLETE: say so, or the footer would
            // claim it holds the full output.
            truncation.truncated = true;
            content.push_str(&format!(
                "\n\n[Output exceeded the {} spill limit; further output was discarded (truncated due to spill limit)]",
                format_size(self.max_spill_bytes)
            ));
        }
        OutputSnapshot {
            content,
            truncation,
            full_output_path,
        }
    }

    fn persist(&self) -> Option<PathBuf> {
        // Already spilling: the file holds the complete output.
        if let Some(spill) = &self.spill {
            let _ = spill.file.sync_data();
            return Some(spill.path.clone());
        }
        // Stable path per accumulator, created once: repeated persists
        // overwrite it instead of leaking a fresh temp file per call.
        let path = self.persist_path.get_or_init(|| {
            let id: String = rand::random::<[u8; 8]>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            std::env::temp_dir().join(format!("{}-{id}.log", self.temp_file_prefix))
        });
        std::fs::write(path, &self.buffer)
            .ok()
            .map(|_| path.clone())
    }

    /// Remove the file persist() created, if any. Called when the owning
    /// background task leaves the registry (evicted/pruned): until then a
    /// snapshot's `fullOutputPath` may still be handed to the model. The
    /// spill file's lifecycle is unchanged.
    pub fn cleanup_persisted(&self) {
        if let Some(path) = self.persist_path.get() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn small_output_stays_in_memory() {
        let mut acc = OutputAccumulator::with_in_memory_cap("tack-test", 64);
        acc.append(b"hello\n");
        let snap = acc.snapshot(true);
        assert!(!snap.truncation.truncated);
        assert_eq!(snap.content, "hello\n");
        assert!(snap.full_output_path.is_none());
    }

    /// Regression: the in-memory cap must hold exactly (previously a single
    /// chunk could push the buffer past it), and output beyond the cap must
    /// still land in the persisted full-output file (previously it was
    /// silently dropped while the footer claimed the file was complete).
    #[test]
    fn overflow_spills_to_complete_temp_file() {
        let mut acc = OutputAccumulator::with_in_memory_cap("tack-test", 16);
        acc.max_bytes = 4; // force truncation so persist kicks in
        acc.append(b"0123456789"); // 10 bytes, in memory
        acc.append(b"abcdefghij0123456789"); // 20 bytes: 6 fit, 14 spill
        assert_eq!(acc.buffer.len(), 16, "in-memory cap must hold exactly");

        let snap = acc.snapshot(true);
        assert!(snap.truncation.truncated);
        let path = snap.full_output_path.expect("spill file must be persisted");
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, b"0123456789abcdefghij0123456789");
        let _ = std::fs::remove_file(&path);
    }

    /// The snapshot fast path must agree with truncate_tail for content
    /// that fits (and must not skip truncation for content that doesn't).
    #[test]
    fn snapshot_fast_path_matches_truncate_tail_semantics() {
        for content in ["", "a", "a\n", "a\nb\n", "a\nb"]
            .iter()
            .map(|s| s.to_string())
        {
            let mut acc = OutputAccumulator::new("tack-test");
            acc.append(content.as_bytes());
            let snap = acc.snapshot(false);
            assert!(!snap.truncation.truncated, "{content:?}");
            assert_eq!(snap.content, content);
            assert_eq!(
                snap.truncation.total_lines,
                crate::truncate::count_lines(&content)
            );
        }
        // Over the line budget: still truncated.
        let mut acc = OutputAccumulator::new("tack-test");
        acc.append("l\n".repeat(3000).as_bytes());
        let snap = acc.snapshot(false);
        assert!(snap.truncation.truncated);
        assert_eq!(snap.truncation.output_lines, 2000);
    }

    #[test]
    fn appends_after_spill_go_straight_to_disk() {
        let mut acc = OutputAccumulator::with_in_memory_cap("tack-test", 8);
        acc.max_bytes = 4;
        acc.append(b"12345678"); // exactly at cap
        acc.append(b"AAA");
        acc.append(b"BBB");
        assert!(acc.spill.is_some());
        let snap = acc.snapshot(true);
        let path = snap.full_output_path.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"12345678AAABBB");
        let _ = std::fs::remove_file(&path);
    }

    /// Regression: repeated persist must reuse ONE stable temp path
    /// (previously every snapshot(true) created a fresh random file and
    /// leaked it), and cleanup_persisted removes that file.
    #[test]
    fn persist_reuses_stable_path_and_cleanup_removes_it() {
        let mut acc = OutputAccumulator::new("tack-test-persist");
        acc.max_bytes = 4; // truncated without spilling
        acc.append(b"0123456789");
        let first = acc.snapshot(true).full_output_path.unwrap();
        acc.append(b"more");
        let second = acc.snapshot(true).full_output_path.unwrap();
        assert_eq!(first, second, "persist must overwrite one stable path");
        assert_eq!(std::fs::read(&second).unwrap(), b"0123456789more");
        acc.cleanup_persisted();
        assert!(!second.exists(), "cleanup must remove the persist file");
    }

    /// The spill file is capped: output past the limit is discarded (not
    /// written to disk) and the snapshot says the full output is
    /// incomplete instead of claiming completeness.
    #[test]
    fn spill_cap_discards_and_marks_snapshot() {
        let mut acc = OutputAccumulator::with_in_memory_cap("tack-test-spillcap", 8);
        acc.max_bytes = 4;
        acc.max_spill_bytes = 16;
        acc.append(b"12345678"); // fills the in-memory buffer
        acc.append(b"ABCDEFGHIJ"); // 10 bytes: 8 fit the spill cap, 2 dropped
        acc.append(b"KLMNOP"); // past the cap: fully discarded
        let snap = acc.snapshot(true);
        let path = snap.full_output_path.unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"12345678ABCDEFGH",
            "spill file must stop at the cap"
        );
        assert!(snap.truncation.truncated);
        assert!(
            snap.content.contains("truncated due to spill limit"),
            "snapshot must mark the spill-cap truncation: {}",
            snap.content
        );
        let _ = std::fs::remove_file(&path);
    }
}
