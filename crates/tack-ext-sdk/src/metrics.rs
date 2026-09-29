//! Metrics sidecar client: the append-only writer for the host-provided
//! scratch file. The host validates EVERY drained line against the
//! plugin's declared operations schema (exact dimension sets, enum
//! values, finite numbers, ≤64 KiB / ≤100 lines per drain) before it
//! enters telemetry — write small, well-formed records only.

use std::io::Write as _;
use std::path::Path;

/// Append-only NDJSON writer for the metrics sidecar scratch file
/// (`cx.capabilities().metrics.scratch_file`).
///
/// ```no_run
/// # fn demo(cx: tack_ext_sdk::Cx) -> std::io::Result<()> {
/// use tack_ext_sdk::MetricsRecorder;
/// if let Some(metrics) = cx.capabilities().metrics.clone() {
///     let mut recorder = MetricsRecorder::new(&metrics.scratch_file)?;
///     recorder.record("review.run", 1.0, &[("outcome", "ok")])?;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct MetricsRecorder {
    file: std::fs::File,
}

impl MetricsRecorder {
    /// Open the scratch file for appending (created if absent — the host
    /// normally pre-creates it).
    pub fn new(scratch_file: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(MetricsRecorder {
            file: std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(scratch_file)?,
        })
    }

    /// Append one measurement line. `dimensions` must exactly match the
    /// operation's declared dimension set with enum-validated values —
    /// the host drops anything else as a sidecar violation. Non-finite
    /// values are rejected client-side.
    pub fn record(
        &mut self,
        operation: &str,
        value: f64,
        dimensions: &[(&str, &str)],
    ) -> std::io::Result<()> {
        if !value.is_finite() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "metric value must be finite",
            ));
        }
        let dimensions: serde_json::Map<String, serde_json::Value> = dimensions
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    serde_json::Value::String((*v).to_string()),
                )
            })
            .collect();
        let line = serde_json::json!({
            "operation": operation,
            "value": value,
            "dimensions": dimensions,
        });
        writeln!(self.file, "{line}")?;
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn records_ndjson_lines() {
        // No tempfile dep in this crate: a unique temp subdir does it.
        let dir =
            std::env::temp_dir().join(format!("tack-sdk-metrics-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metrics.ndjson");
        let mut recorder = MetricsRecorder::new(&path).unwrap();
        recorder
            .record("review.run", 1.0, &[("outcome", "ok")])
            .unwrap();
        recorder.record("review.cache_hit", 2.5, &[]).unwrap();
        assert!(recorder.record("x", f64::NAN, &[]).is_err());
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["operation"], "review.run");
        assert_eq!(first["dimensions"]["outcome"], "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
