//! Structured observability: a tracing layer that appends every event as one
//! JSON object per line to `<agent dir>/logs/tack-<yyyymmdd>.jsonl`.
//! Enable via settings `observability: {"enabled": true, "level": "info"}`
//! or the `TACK_TRACE_FILE=1` env var (level from `TACK_TRACE_LEVEL`).
//!
//! Designed for post-mortem debugging of headless/remote sessions (serve,
//! rpc, acp): the JSONL stream carries timestamps, levels, targets, span
//! contexts and all structured fields — grep/jq friendly, no infra needed.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

/// Resolved observability config.
#[derive(Clone, Debug)]
pub struct ObservabilityConfig {
    pub enabled: bool,
    pub level: String,
}

impl ObservabilityConfig {
    /// From settings + env overrides (env wins).
    pub fn resolve(settings: &serde_json::Value) -> Self {
        Self::resolve_with(
            settings,
            std::env::var_os("TACK_TRACE_FILE").is_some(),
            std::env::var("TACK_TRACE_LEVEL").ok(),
        )
    }

    fn resolve_with(
        settings: &serde_json::Value,
        trace_file_env: bool,
        trace_level_env: Option<String>,
    ) -> Self {
        let section = settings.get("observability");
        let mut enabled = section
            .and_then(|s| s.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut level = section
            .and_then(|s| s.get("level"))
            .and_then(|v| v.as_str())
            .unwrap_or("info")
            .to_string();
        if trace_file_env {
            enabled = true;
        }
        if let Some(l) = trace_level_env {
            level = l;
        }
        ObservabilityConfig { enabled, level }
    }
}

fn log_path(agent_dir: &Path) -> PathBuf {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    agent_dir.join("logs").join(format!("tack-{days}.jsonl"))
}

struct FieldVisitor {
    fields: serde_json::Map<String, Value>,
}

/// Sensitive field-name fragments (case-insensitive substring match).
const SENSITIVE_KEYS: &[&str] = &[
    "token",
    "api_key",
    "apikey",
    "authorization",
    "secret",
    "password",
    "credential",
    "access_key",
    "refresh",
    "session_key",
    "cookie",
];

/// Sensitive URL query parameters redacted inside string values.
const SENSITIVE_PARAMS: &[&str] = &[
    "key",
    "token",
    "api_key",
    "access_token",
    "sig",
    "signature",
    "password",
    "credential",
];

/// Byte-wise ASCII case-insensitive search (ASCII needles never match
/// inside UTF-8 continuation bytes, so the hit is a char boundary).
fn find_ascii_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    if from > hay.len() || needle.len() > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
        .map(|i| from + i)
}

/// Redact a string value: Bearer tokens and sensitive URL query params.
pub fn redact_string(value: &str) -> String {
    let mut out = value.to_string();
    // "Bearer <token>" (case-insensitive). Skip already-redacted ones.
    let mut search_from = 0;
    while let Some(idx) = find_ascii_ci(&out, "bearer ", search_from) {
        let start = idx + "bearer ".len();
        let end = out[start..]
            .find(char::is_whitespace)
            .map(|i| start + i)
            .unwrap_or(out.len());
        if &out[start..end] != "***" {
            out.replace_range(start..end, "***");
        }
        search_from = start + 3; // past the (now-)redacted value
    }
    // URL query params: name=value pairs for sensitive names. Matching is
    // case-insensitive and the name may be prefixed by '-', '_' (covers
    // X-Amz-Signature=/X-Amz-Credential= and similar vendor schemes).
    for param in SENSITIVE_PARAMS {
        let marker = format!("{param}=");
        let mut search_from = 0;
        while let Some(idx) = find_ascii_ci(&out, &marker, search_from) {
            // Only redact in query-ish context (preceded by ?, &, - or _).
            let before = idx
                .checked_sub(1)
                .and_then(|i| out.as_bytes().get(i))
                .copied();
            let preceded = matches!(before, Some(b'?' | b'&' | b'-' | b'_'));
            let value_start = idx + marker.len();
            let value_end = out[value_start..]
                .find(['&', ' ', '"', '\''])
                .map(|i| value_start + i)
                .unwrap_or(out.len());
            let already = &out[value_start..value_end] == "***";
            if preceded && value_end > value_start && !already {
                out.replace_range(value_start..value_end, "***");
            }
            search_from = value_start + 3;
        }
    }
    // Rust-Debug struct dumps (`tracing::info!(?cfg)`): `field: "value"` /
    // `field: value` for sensitive field names.
    for key in SENSITIVE_KEYS {
        let marker = format!("{key}: ");
        let mut search_from = 0;
        while let Some(idx) = find_ascii_ci(&out, &marker, search_from) {
            let value_start = idx + marker.len();
            let quoted = out.as_bytes().get(value_start) == Some(&b'"');
            let content_start = if quoted { value_start + 1 } else { value_start };
            let value_end = if quoted {
                out[content_start..]
                    .find('"')
                    .map(|i| content_start + i)
                    .unwrap_or(out.len())
            } else {
                out[content_start..]
                    .find([',', ' ', '}'])
                    .map(|i| content_start + i)
                    .unwrap_or(out.len())
            };
            let already = &out[content_start..value_end] == "***";
            if value_end > content_start && !already {
                out.replace_range(content_start..value_end, "***");
            }
            search_from = content_start + 3;
        }
    }
    out
}

/// Redact sensitive keys/values in a field map before writing to disk.
pub fn redact_fields(fields: &mut serde_json::Map<String, Value>) {
    for (key, value) in fields.iter_mut() {
        let lower = key.to_lowercase();
        if SENSITIVE_KEYS.iter().any(|k| lower.contains(k)) {
            *value = Value::String("***".to_string());
            continue;
        }
        if let Some(s) = value.as_str() {
            *value = Value::String(redact_string(s));
        }
    }
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            Value::String(format!("{value:?}")),
        );
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
}

// ---------------------------------------------------------------------
// Managed audit sink
// ---------------------------------------------------------------------

/// Managed-settings audit sink (`auditSink: {url, token?, intervalMs?}`):
/// when the organization sets one, tracing events are also batched to the
/// HTTP endpoint. The local JSONL file stays. This is a managed-only key —
/// users/projects cannot set or disable it (enforced in init_tracing, which
/// reads the managed file directly).
#[derive(Clone, Debug)]
pub struct AuditSinkConfig {
    pub url: String,
    pub token: Option<String>,
    pub interval_ms: u64,
}

/// Read auditSink from the MANAGED settings layer only.
pub fn audit_sink_from_managed() -> Option<AuditSinkConfig> {
    let raw = std::fs::read_to_string(crate::settings::managed_settings_path())
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())?;
    let sink = raw.get("auditSink")?;
    let url = sink.get("url").and_then(Value::as_str)?.to_string();
    if url.is_empty() {
        return None;
    }
    Some(AuditSinkConfig {
        url,
        token: sink
            .get("token")
            .and_then(Value::as_str)
            .map(str::to_string),
        interval_ms: sink
            .get("intervalMs")
            .and_then(Value::as_u64)
            .unwrap_or(5_000)
            .max(500),
    })
}

/// Batching audit layer: events are JSON-serialized, buffered, and POSTed
/// (newline-delimited JSON body) on an interval or every 50 buffered events.
/// Clones share the buffer; the background uploader outlives registration.
pub struct AuditLayer {
    inner: Arc<AuditInner>,
}

/// Audit HTTP timeouts: a wedged endpoint must never pend the uploader
/// forever (that would silently grow the event buffer without bound).
/// 10s to connect, 30s for the whole request — generous for a LAN/WAN
/// collector, short enough to drop a dead endpoint at the next tick.
const AUDIT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const AUDIT_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Hard cap on buffered (not-yet-delivered) audit events. Past the cap new
/// events are DROPPED (not queued): the buffer is a delivery window, not a
/// store, and must never grow without bound when the endpoint is down.
/// The local JSONL layer keeps the full event stream regardless.
const MAX_BUFFERED_AUDIT_EVENTS: usize = 10_000;

struct AuditInner {
    buffer: Mutex<Vec<String>>,
    /// Flush signal: wakes the uploader immediately at 50 buffered events.
    flush_now: tokio::sync::Notify,
    /// Events dropped because the buffer hit MAX_BUFFERED_AUDIT_EVENTS.
    dropped: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for AuditLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditLayer").finish_non_exhaustive()
    }
}

impl AuditLayer {
    pub fn new(config: AuditSinkConfig) -> Self {
        let inner = Arc::new(AuditInner {
            buffer: Mutex::new(Vec::new()),
            flush_now: tokio::sync::Notify::new(),
            dropped: std::sync::atomic::AtomicU64::new(0),
        });
        let layer_bg = inner.clone();
        crate::task::spawn_guarded("audit-sink", async move {
            // Timeouts are load-bearing: without them a hung endpoint pends
            // send() forever and the buffer above grows without bound.
            let client = reqwest::Client::builder()
                .connect_timeout(AUDIT_CONNECT_TIMEOUT)
                .timeout(AUDIT_REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(config.interval_ms)) => {}
                    _ = layer_bg.flush_now.notified() => {}
                }
                let batch: Vec<String> = {
                    let mut buffer = layer_bg.buffer.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::take(&mut *buffer)
                };
                if batch.is_empty() {
                    continue;
                }
                let body = batch.join("\n");
                let mut request = client.post(&config.url).body(body);
                if let Some(token) = &config.token {
                    request = request.header("authorization", format!("Bearer {token}"));
                }
                if let Err(e) = request.send().await {
                    // Never block or crash the agent on audit delivery; the
                    // batch is dropped (local JSONL retains the events).
                    tracing::debug!("audit sink delivery failed: {e}");
                }
                let dropped = layer_bg
                    .dropped
                    .swap(0, std::sync::atomic::Ordering::Relaxed);
                if dropped > 0 {
                    tracing::warn!("audit sink dropped {dropped} event(s): buffer cap reached");
                }
            }
        });
        AuditLayer { inner }
    }
}

impl<S: tracing::Subscriber> Layer<S> for AuditLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor {
            fields: serde_json::Map::new(),
        };
        event.record(&mut visitor);
        redact_fields(&mut visitor.fields);
        let message = visitor
            .fields
            .remove("message")
            .and_then(|v| v.as_str().map(str::to_string))
            .map(|m| redact_string(&m))
            .unwrap_or_default();
        let line = json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            "level": event.metadata().level().to_string(),
            "target": event.metadata().target(),
            "message": message,
            "fields": Value::Object(visitor.fields),
        })
        .to_string();
        let mut buffer = self.inner.buffer.lock().unwrap_or_else(|e| e.into_inner());
        if buffer.len() >= MAX_BUFFERED_AUDIT_EVENTS {
            // Buffer full (endpoint down/slow): drop the NEW event and count
            // it. Never block the traced thread on audit delivery, and never
            // grow the buffer without bound. The warn fires once per burst —
            // and never while holding the buffer lock (the warning event
            // itself re-enters this layer and would deadlock a std::Mutex).
            drop(buffer);
            let dropped = self
                .inner
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if dropped == 0 {
                tracing::warn!(
                    "audit sink buffer full ({MAX_BUFFERED_AUDIT_EVENTS} events); dropping new audit events"
                );
            }
            return;
        }
        buffer.push(line);
        if buffer.len() >= 50 {
            drop(buffer);
            self.inner.flush_now.notify_one();
        }
    }
}

/// JSONL file appender layer.
///
/// Writes go through a bounded channel to a dedicated writer thread: the
/// layer's on_event runs on the logging thread (tokio workers included),
/// so a mutex around blocking file I/O here could wedge every tracing
/// thread behind a stalled disk. Overflow drops lines (debug log only).
const JSONL_CHANNEL_CAPACITY: usize = 4096;
pub struct JsonlLayer {
    tx: std::sync::mpsc::SyncSender<String>,
}

impl std::fmt::Debug for JsonlLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlLayer").finish_non_exhaustive()
    }
}

impl JsonlLayer {
    pub fn new(agent_dir: &Path) -> std::io::Result<Self> {
        let path = log_path(agent_dir);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        // A dedicated writer thread owns the file: on_event runs on
        // WHATEVER thread logs (tokio workers included), and a stalled
        // log filesystem must not wedge them behind a mutex held during
        // blocking I/O. Producers try_send and drop when the writer
        // falls behind — losing a debug log line beats losing the agent.
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(JSONL_CHANNEL_CAPACITY);
        std::thread::Builder::new()
            .name("observability-jsonl".to_string())
            .spawn(move || {
                let mut file = std::io::BufWriter::new(file);
                while let Ok(line) = rx.recv() {
                    let _ = writeln!(file, "{line}");
                    let _ = file.flush();
                }
            })?;
        Ok(JsonlLayer { tx })
    }

    fn write_line(&self, value: &Value) {
        let _ = self.tx.try_send(value.to_string());
    }
}

impl<S: tracing::Subscriber> Layer<S> for JsonlLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor {
            fields: serde_json::Map::new(),
        };
        event.record(&mut visitor);
        // Redact credentials/secrets before anything hits the disk.
        redact_fields(&mut visitor.fields);
        let message = visitor
            .fields
            .remove("message")
            .and_then(|v| v.as_str().map(str::to_string))
            .map(|m| redact_string(&m))
            .unwrap_or_default();
        let line = json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            "level": event.metadata().level().to_string(),
            "target": event.metadata().target(),
            "message": message,
            "fields": Value::Object(visitor.fields),
        });
        self.write_line(&line);
    }

    fn on_close(&self, _id: tracing::span::Id, _ctx: Context<'_, S>) {}
}

/// Tracing targets that carry org-mandated audit records (policy
/// decisions, approval claims, plugin metrics, load telemetry).
const AUDIT_TARGETS: &[&str] = &[
    "plugin_policy",
    "plugin_approval",
    "plugin_metrics",
    "plugin_load",
];

/// The audit sink's filter: the configured level, but with every audit
/// target pinned at INFO. The managed audit sink is the org's record of
/// those decisions — a user-level `observability.level = "warn"` (or
/// TACK_TRACE_LEVEL) must not silently drop them while enforcement
/// continues.
fn audit_filter(level: &str) -> tracing_subscriber::EnvFilter {
    let mut filter = tracing_subscriber::EnvFilter::new(level);
    for target in AUDIT_TARGETS {
        filter = filter.add_directive(
            format!("{target}=info")
                .parse()
                .expect("audit directive parses"),
        );
    }
    filter
}

/// Install the JSONL layer when enabled. Call INSTEAD of the plain fmt init
/// (composes stderr fmt + optional file export + managed audit sink).
pub fn init_tracing(
    filter: tracing_subscriber::EnvFilter,
    agent_dir: &Path,
    settings: &serde_json::Value,
) {
    let config = ObservabilityConfig::resolve(settings);
    // A managed auditSink forces observability on — org visibility is the point.
    let audit = audit_sink_from_managed();
    let enabled = config.enabled || audit.is_some();
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let subscriber = tracing_subscriber::registry().with(filter).with(stderr);
    if !enabled {
        subscriber.init();
        return;
    }
    match JsonlLayer::new(agent_dir) {
        Ok(layer) => {
            let file_filter = tracing_subscriber::EnvFilter::new(config.level.clone());
            let subscriber = subscriber.with(layer.with_filter(file_filter));
            match audit {
                Some(sink) => {
                    let audit_filter = audit_filter(&config.level);
                    subscriber
                        .with(AuditLayer::new(sink).with_filter(audit_filter))
                        .init();
                }
                None => subscriber.init(),
            }
            tracing::info!(
                "observability: structured logs → {} (level {})",
                log_path(agent_dir).display(),
                config.level
            );
        }
        Err(e) => {
            // The managed audit sink is org-mandated visibility: keep it
            // even when the local JSONL file cannot be opened.
            match audit {
                Some(sink) => {
                    let audit_filter = audit_filter(&config.level);
                    subscriber
                        .with(AuditLayer::new(sink).with_filter(audit_filter))
                        .init();
                }
                None => subscriber.init(),
            }
            tracing::warn!("observability: cannot open log file: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn audit_filter_pins_audit_targets_at_info() {
        let rendered = audit_filter("warn").to_string();
        for target in AUDIT_TARGETS {
            assert!(
                rendered.contains(&format!("{target}=info")),
                "{target} must stay pinned at info: {rendered}"
            );
        }
    }

    #[test]
    fn config_env_overrides_settings() {
        let settings = json!({ "observability": { "enabled": false, "level": "warn" } });
        let config = ObservabilityConfig::resolve_with(&settings, true, Some("debug".into()));
        assert!(config.enabled);
        assert_eq!(config.level, "debug");
        let config = ObservabilityConfig::resolve_with(&settings, false, None);
        assert!(!config.enabled);
        assert_eq!(config.level, "warn");
    }

    #[test]
    fn redaction_covers_keys_bearer_and_urls() {
        let mut fields = serde_json::json!({
            "Authorization": "Bearer sk-abc123",
            "api_key": "xyz",
            "url": "https://api.example.com/v1?key=secret123&other=ok",
            "note": "calling with Bearer tok_live_99"
        });
        redact_fields(fields.as_object_mut().unwrap());
        assert_eq!(fields["Authorization"], "***");
        assert_eq!(fields["api_key"], "***");
        let url = fields["url"].as_str().unwrap();
        assert!(url.contains("key=***"), "{url}");
        assert!(url.contains("other=ok"), "{url}");
        let note = fields["note"].as_str().unwrap();
        assert!(note.contains("Bearer ***"), "{note}");
        assert!(!note.contains("tok_live_99"));
    }

    #[test]
    fn redaction_covers_debug_struct_dumps() {
        let s = redact_string(
            r#"Config { api_key: "sk-live-123", endpoint: "https://x", refresh_token: "r-tok" }"#,
        );
        assert!(!s.contains("sk-live-123"), "{s}");
        assert!(!s.contains("r-tok"), "{s}");
        assert!(s.contains("api_key: \"***\""), "{s}");
        assert!(s.contains("endpoint: \"https://x\""), "{s}");
        // Unquoted Debug values.
        let s = redact_string("Auth { token: abc123, retries: 3 }");
        assert!(!s.contains("abc123"), "{s}");
        assert!(s.contains("retries: 3"), "{s}");
    }

    #[test]
    fn redaction_covers_aws_style_and_mixed_case_params() {
        let mut fields = serde_json::json!({
            "url": "https://s3.amazonaws.com/b/k?X-Amz-Signature=deadbeef99&X-Amz-Credential=AKIAEXAMPLE%2F2024&X-Amz-Expires=3600",
            "other": "https://api.example.com/v1?Token=tok_123&bearer=zzz"
        });
        redact_fields(fields.as_object_mut().unwrap());
        let url = fields["url"].as_str().unwrap();
        assert!(!url.contains("deadbeef99"), "{url}");
        assert!(!url.contains("AKIAEXAMPLE"), "{url}");
        assert!(url.contains("X-Amz-Expires=3600"), "{url}");
        let other = fields["other"].as_str().unwrap();
        assert!(!other.contains("tok_123"), "{other}");
    }

    #[test]
    fn jsonl_layer_writes_events() {
        let tmp = tempfile::tempdir().unwrap();
        let layer = JsonlLayer::new(tmp.path()).unwrap();
        layer.write_line(&json!({ "msg": "hello" }));
        let dir = tmp.path().join("logs");
        // Writes cross a channel + writer thread now: poll briefly instead
        // of assuming the line is already on disk.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let content = std::fs::read_dir(&dir)
                .ok()
                .and_then(|mut entries| entries.next())
                .and_then(|entry| entry.ok())
                .and_then(|entry| std::fs::read_to_string(entry.path()).ok())
                .unwrap_or_default();
            if content.contains("hello") {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "log line never landed; file content: {content:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

// (test) End-to-end: events land at the HTTP sink, batched.
#[cfg(test)]
mod audit_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn audit_sink_receives_batched_events() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncReadExt as _;
            // A single read() is NOT a whole request: headers and body can
            // land in separate TCP segments (this test flaked under parallel
            // test load with a headers-only buffer). Read until the header
            // block is complete AND the Content-Length body bytes arrived.
            let mut buf = Vec::with_capacity(1024);
            let mut chunk = [0u8; 8192];
            loop {
                if let Some(headers_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..headers_end]);
                    let body_len = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= headers_end + 4 + body_len {
                        break;
                    }
                }
                if buf.len() > 64 * 1024 {
                    break; // oversized: assert against whatever arrived
                }
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break; // EOF: assert against whatever arrived
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
        });

        let layer = AuditLayer::new(AuditSinkConfig {
            url: format!("http://{addr}/audit"),
            token: Some("audit-secret".into()),
            interval_ms: 50,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = {
            use tracing_subscriber::util::SubscriberInitExt as _;
            subscriber.set_default()
        };
        tracing::info!(answer = 42, "audit-test-event");

        let request = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap();
        assert!(request.contains("Bearer audit-secret"), "{request}");
        assert!(request.contains("audit-test-event"), "{request}");
    }
}
