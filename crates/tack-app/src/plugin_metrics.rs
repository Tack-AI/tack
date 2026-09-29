//! Plugin metrics sidecar (roadmap §9): declared-schema telemetry from
//! untrusted plugin processes. The plugin DECLARES its operations and
//! dimension enums at initialize; the host hands over a sandbox-
//! authorized scratch file (WASI-stdio WASM: a dedicated preopen,
//! audited) and validates every drain strictly — byte/line caps, exact
//! dimension sets, enum values, finite numbers, dedup — before anything
//! enters telemetry with plugin attribution (target `plugin_metrics`,
//! shipped via observability JSONL + the managed auditSink).
//!
//! Measurement lines are NDJSON, one object per line:
//!
//! ```json
//! {"operation": "review.run", "value": 1, "dimensions": {"outcome": "ok"}}
//! ```
//!
//! Scope: process and WASI-stdio WASM carriers. The WIT component world
//! imports no WASI interfaces (no fs at all), so component-carrier
//! declarations are voided with a load warning until a typed metrics
//! export lands in a future world version.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tack_ext::rpc3::{MetricOperation, MetricsDeclaration};

/// One active sidecar: scratch file + validated declaration + drain state.
#[derive(Debug)]
pub struct MetricsSidecar {
    /// Plugin id (attribution on every emitted measurement).
    plugin: String,
    /// Host path of the scratch file.
    path: PathBuf,
    /// The validated declaration.
    operations: BTreeMap<String, MetricOperation>,
    /// Read position (bytes consumed by previous drains).
    offset: u64,
    /// Strict-validation strikes; the sidecar is disabled at the cap.
    violations: u32,
    disabled: bool,
}

impl MetricsSidecar {
    pub fn new(plugin: String, path: PathBuf, declaration: MetricsDeclaration) -> Self {
        MetricsSidecar {
            plugin,
            path,
            operations: declaration.operations,
            offset: 0,
            violations: 0,
            disabled: false,
        }
    }

    /// The plugin this sidecar belongs to.
    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// The host path of the scratch file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Strike count (tests/debug).
    pub fn violations(&self) -> u32 {
        self.violations
    }

    /// Drain cursor (tests/debug).
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

/// Per-drain caps (roadmap §9): a drain reads at most 64 KiB / 100
/// lines; a plugin that out-writes the drain interval loses the excess
/// to a violation, so keep bursts small (the host drains every 30s).
pub const DRAIN_MAX_BYTES: u64 = 64 * 1024;
pub const DRAIN_MAX_LINES: usize = 100;
/// Strikes before the sidecar is disabled for the session.
const MAX_VIOLATIONS: u32 = 3;
/// Declared enums: at most 64 values of at most 64 chars.
const MAX_ENUM_VALUES: usize = 64;
const MAX_ENUM_VALUE_LEN: usize = 64;

/// `[a-z][a-z0-9_.]{0,63}` — operation and dimension identifiers.
fn ident_ok(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && ident.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

/// Validate a declared schema. ANY violation voids the whole
/// declaration (the caller drops it with a load warning) — a partially
/// valid schema would make drain validation ambiguous.
pub fn validate_declaration(declaration: &MetricsDeclaration) -> Result<(), String> {
    if declaration.operations.is_empty() {
        return Err("declares no operations".to_string());
    }
    for (operation, spec) in &declaration.operations {
        if !ident_ok(operation) {
            return Err(format!(
                "operation id {operation:?} must match [a-z][a-z0-9_.]{{0,63}}"
            ));
        }
        let dimensions = spec.dimensions.clone().unwrap_or_default();
        if dimensions.len() > 8 {
            return Err(format!(
                "operation {operation:?} declares {} dimensions (at most 8)",
                dimensions.len()
            ));
        }
        for (name, values) in &dimensions {
            if !ident_ok(name) {
                return Err(format!(
                    "dimension {name:?} of operation {operation:?} must match [a-z][a-z0-9_.]{{0,63}}"
                ));
            }
            if values.is_empty() || values.len() > MAX_ENUM_VALUES {
                return Err(format!(
                    "dimension {name:?} of operation {operation:?} declares {} values (1..={MAX_ENUM_VALUES})",
                    values.len()
                ));
            }
            if values
                .iter()
                .any(|v| v.is_empty() || v.len() > MAX_ENUM_VALUE_LEN)
            {
                return Err(format!(
                    "dimension {name:?} of operation {operation:?} has an empty or over-long value"
                ));
            }
        }
    }
    Ok(())
}

/// The scratch file location inside the plugin's data root:
/// `<data>/metrics/metrics.ndjson` (the parent dir doubles as the
/// dedicated WASM preopen).
pub fn scratch_path(data_dir: &Path) -> PathBuf {
    data_dir.join("metrics").join("metrics.ndjson")
}

/// Create/truncate the per-session scratch file; returns its host path.
pub fn create_scratch(data_dir: &Path) -> std::io::Result<PathBuf> {
    let path = scratch_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(&path)?;
    Ok(path)
}

/// One validated measurement.
#[derive(Debug, PartialEq)]
struct Measurement {
    operation: String,
    value: f64,
    dimensions: BTreeMap<String, String>,
}

/// Parse and validate one NDJSON line against the declaration.
fn validate_line(
    line: &str,
    operations: &BTreeMap<String, MetricOperation>,
) -> Result<Measurement, String> {
    let value: Value = serde_json::from_str(line).map_err(|e| format!("not a JSON object: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "not a JSON object".to_string())?;
    for key in object.keys() {
        if !matches!(key.as_str(), "operation" | "value" | "dimensions") {
            return Err(format!("unknown field {key:?}"));
        }
    }
    let operation = object
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `operation`".to_string())?;
    let Some(spec) = operations.get(operation) else {
        return Err(format!("undeclared operation {operation:?}"));
    };
    let number = object
        .get("value")
        .and_then(Value::as_f64)
        .ok_or_else(|| "missing `value`".to_string())?;
    if !number.is_finite() {
        return Err("value is not finite".to_string());
    }
    let declared = spec.dimensions.clone().unwrap_or_default();
    let mut dimensions = BTreeMap::new();
    match object.get("dimensions") {
        None | Some(Value::Null) => {}
        Some(Value::Object(given)) => {
            for (name, value) in given {
                let value = value
                    .as_str()
                    .ok_or_else(|| format!("dimension {name:?} is not a string"))?;
                dimensions.insert(name.clone(), value.to_string());
            }
        }
        Some(_) => return Err("`dimensions` is not an object".to_string()),
    }
    // Exact dimension sets: no missing, no extra.
    if dimensions.len() != declared.len() || !dimensions.keys().all(|k| declared.contains_key(k)) {
        return Err(format!(
            "operation {operation:?} requires exactly the declared dimension set {{{}}}",
            declared.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    for (name, value) in &dimensions {
        let allowed = &declared[name];
        if !allowed.iter().any(|a| a == value) {
            return Err(format!(
                "dimension {name:?} value {value:?} is not in the declared enum"
            ));
        }
    }
    Ok(Measurement {
        operation: operation.to_string(),
        value: number,
        dimensions,
    })
}

/// Drain new bytes from one sidecar into telemetry. Sync file I/O — the
/// drain is called from the interval task and from shutdown, never on a
/// hot path.
pub fn drain(sidecar: &mut MetricsSidecar) {
    if sidecar.disabled {
        return;
    }
    let violation = |sidecar: &mut MetricsSidecar, detail: String| {
        sidecar.violations += 1;
        tracing::warn!(
            target: "plugin_metrics",
            plugin = %sidecar.plugin,
            violations = sidecar.violations,
            "metrics sidecar violation: {detail}"
        );
        if sidecar.violations >= MAX_VIOLATIONS {
            sidecar.disabled = true;
            tracing::warn!(
                target: "plugin_metrics",
                plugin = %sidecar.plugin,
                "metrics sidecar disabled for this session after {MAX_VIOLATIONS} violations"
            );
        }
    };
    let Ok(len) = std::fs::metadata(&sidecar.path).map(|m| m.len()) else {
        return;
    };
    if len < sidecar.offset {
        // Truncated out from under us (never by the host): restart.
        sidecar.offset = 0;
    }
    if len == sidecar.offset {
        return;
    }
    let pending = len - sidecar.offset;
    if pending > DRAIN_MAX_BYTES {
        violation(
            sidecar,
            format!("drain of {pending} bytes exceeds the {DRAIN_MAX_BYTES}-byte cap"),
        );
        sidecar.offset = len; // drop the excess unvalidated
        return;
    }
    let mut bytes = Vec::new();
    {
        let Ok(mut file) = std::fs::File::open(&sidecar.path) else {
            return;
        };
        if file
            .seek(std::io::SeekFrom::Start(sidecar.offset))
            .and_then(|_| file.read_to_end(&mut bytes))
            .is_err()
        {
            return;
        }
    }
    // A trailing partial line is the plugin mid-write: hold it for the
    // next drain rather than parsing a torn record.
    let complete_up_to = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    if complete_up_to == 0 {
        return;
    }
    let text = String::from_utf8_lossy(&bytes[..complete_up_to]);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() > DRAIN_MAX_LINES {
        violation(
            sidecar,
            format!(
                "drain of {} lines exceeds the {DRAIN_MAX_LINES}-line cap",
                lines.len()
            ),
        );
        sidecar.offset = len;
        return;
    }
    let mut seen: HashSet<(String, String, u64)> = HashSet::new();
    for line in lines {
        match validate_line(line, &sidecar.operations) {
            Ok(measurement) => {
                // Dedup within the drain: identical (operation, dims,
                // value) triples collapse to one emission.
                let key = (
                    measurement.operation.clone(),
                    serde_json::to_string(&measurement.dimensions).unwrap_or_default(),
                    measurement.value.to_bits(),
                );
                if !seen.insert(key) {
                    continue;
                }
                tracing::info!(
                    target: "plugin_metrics",
                    plugin = %sidecar.plugin,
                    operation = %measurement.operation,
                    value = measurement.value,
                    dimensions = %serde_json::to_string(&measurement.dimensions)
                        .unwrap_or_default(),
                    "plugin metric {} = {}",
                    measurement.operation,
                    measurement.value
                );
            }
            Err(detail) => {
                violation(sidecar, detail);
                if sidecar.disabled {
                    break;
                }
            }
        }
    }
    sidecar.offset += complete_up_to as u64;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn declaration() -> MetricsDeclaration {
        serde_json::from_value(serde_json::json!({
            "operations": {
                "review.run": {
                    "description": "a review pass",
                    "dimensions": { "outcome": ["ok", "error"] }
                },
                "review.cache_hit": {}
            }
        }))
        .unwrap()
    }

    #[test]
    fn declaration_validation_is_all_or_nothing() {
        assert!(validate_declaration(&declaration()).is_ok());
        let too_many_dims: serde_json::Map<String, Value> = (0..9)
            .map(|i| (format!("d{i}"), serde_json::json!(["x"])))
            .collect();
        for bad in [
            serde_json::json!({"operations": {}}),
            serde_json::json!({"operations": {"Bad id": {}}}),
            serde_json::json!({"operations": {"ok": {"dimensions": too_many_dims}}}),
            serde_json::json!({"operations": {"ok": {"dimensions": {"d": []}}}}),
            serde_json::json!({"operations": {"ok": {"dimensions": {"Bad": ["x"]}}}}),
        ] {
            let decl: MetricsDeclaration = serde_json::from_value(bad).unwrap();
            assert!(validate_declaration(&decl).is_err(), "{decl:?}");
        }
    }

    #[test]
    fn line_validation_enforces_the_schema() {
        let ops = declaration().operations;
        assert!(
            validate_line(
                r#"{"operation": "review.run", "value": 1, "dimensions": {"outcome": "ok"}}"#,
                &ops
            )
            .is_ok()
        );
        // Zero declared dimensions ⇒ dimensions may be omitted, or must be empty.
        assert!(validate_line(r#"{"operation": "review.cache_hit", "value": 1}"#, &ops).is_ok());
        for bad in [
            r#"{"operation": "review.run", "value": 1}"#, // missing dimension
            r#"{"operation": "review.run", "value": 1, "dimensions": {"outcome": "maybe"}}"#, // enum
            r#"{"operation": "review.run", "value": 1, "dimensions": {"outcome": "ok", "x": "y"}}"#, // extra
            r#"{"operation": "nope", "value": 1}"#, // undeclared op
            r#"{"operation": "review.run", "value": "alot", "dimensions": {"outcome": "ok"}}"#, // non-number
            r#"{"operation": "review.run", "value": 1e999, "dimensions": {"outcome": "ok"}}"#, // non-finite
            r#"{"operation": "review.run", "value": 1, "dimensions": {"outcome": "ok"}, "extra": 1}"#, // unknown field
            "not json",
        ] {
            assert!(validate_line(bad, &ops).is_err(), "{bad} must be rejected");
        }
    }

    /// A plugin writes lines; the drain validates and advances; a torn
    /// final line waits for the next drain; hostile content is a
    /// violation, not a panic.
    #[test]
    fn drain_consumes_validates_and_holds_partials() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let path = create_scratch(&data_dir).unwrap();
        let mut sidecar = MetricsSidecar::new("demo@user".to_string(), path.clone(), declaration());

        std::fs::write(&path, "{\"operation\": \"review.run\", \"value\": 1, \"dimensions\": {\"outcome\": \"ok\"}}\n{\"operation\": \"review.cache_hit\", \"value\": 2}\n{\"operation\": \"review.run\", \"val").unwrap();
        drain(&mut sidecar);
        let after_first = sidecar.offset;
        assert!(after_first > 0, "valid lines consumed");
        assert!(
            after_first < std::fs::metadata(&path).unwrap().len(),
            "the torn tail is held"
        );
        assert_eq!(sidecar.violations, 0);

        // Finish the torn line: the next drain picks it up.
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"ue\": 3, \"dimensions\": {\"outcome\": \"error\"}}\n")
            .unwrap();
        drop(file);
        drain(&mut sidecar);
        assert_eq!(sidecar.offset, std::fs::metadata(&path).unwrap().len());
        assert_eq!(sidecar.violations, 0);

        // A schema violation counts a strike but keeps draining.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(
                b"{\"operation\": \"review.run\", \"value\": 1, \"dimensions\": {\"outcome\": \"maybe\"}}\n",
            )
            .unwrap();
        drain(&mut sidecar);
        assert_eq!(sidecar.violations, 1);
        assert!(!sidecar.disabled);

        // Two more strikes disable the sidecar.
        let mut striking = MetricsSidecar {
            violations: MAX_VIOLATIONS - 1,
            ..MetricsSidecar::new("demo@user".to_string(), path.clone(), declaration())
        };
        drain(&mut striking);
        assert!(striking.disabled, "{MAX_VIOLATIONS} strikes disable");
        // A disabled sidecar stops reading entirely.
        let before = striking.offset;
        drain(&mut striking);
        assert_eq!(striking.offset, before);
    }

    /// Over-cap drains are dropped wholesale with a violation.
    #[test]
    fn over_cap_drain_is_a_violation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create_scratch(tmp.path()).unwrap();
        let flood = format!(
            "{}\n",
            "{{\"operation\": \"review.cache_hit\", \"value\": 1}}".repeat(DRAIN_MAX_LINES + 1)
        );
        std::fs::write(&path, flood).unwrap();
        let mut sidecar = MetricsSidecar::new("demo@user".to_string(), path.clone(), declaration());
        drain(&mut sidecar);
        assert_eq!(sidecar.violations, 1);
        assert_eq!(
            sidecar.offset,
            std::fs::metadata(&path).unwrap().len(),
            "the excess is dropped, not parsed"
        );
    }
}
