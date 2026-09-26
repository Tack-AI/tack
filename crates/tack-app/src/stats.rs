//! `tack stats`: cross-session usage report. Scans every project session
//! under the agent dir (`~/.tack/agent/sessions/--<project>--/`), aggregates
//! token usage (input / output / cache read / cache write / thinking) broken
//! down by provider×model, estimates cost from the model catalog pricing,
//! and builds a daily time series.
//!
//! Read-only and defensive by design:
//! - JSONL session files are parsed line-by-line with `serde_json::Value`
//!   (NOT `SessionManager::open`, which rewrites old-version files on load);
//!   unknown/future entry types are ignored, malformed lines are skipped and
//!   counted.
//! - Encrypted lines (`tack-enc:v1:` prefix) are decrypted when a session
//!   key is installed in this process; otherwise they are skipped and
//!   counted, never a hard error.
//! - SQLite-backend sessions (`sessions.db`) are traversed through the
//!   tack-session public API; sessions that fail to load (e.g. undecryptable
//!   rows) are skipped and counted.
//!
//! Cost is an ESTIMATE from catalog pricing (`crates/tack-ai/catalog.json`
//! build-time snapshot, or the refreshed `<agentDir>/catalog.json` cache
//! when installed at startup). Models without pricing show `-` and are
//! excluded from cost totals. Compaction/branch-summary LLM calls carry no
//! model identity, so their cost falls back to the cost recorded in the
//! entry at runtime (shown under `(internal)` rows).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;

/// Default number of most-recent days shown in the daily time series.
pub const DEFAULT_DAILY_LIMIT: usize = 30;

/// Label for the provider column of LLM calls that carry no model identity
/// (compaction / branch-summary entries).
pub const INTERNAL_PROVIDER: &str = "(internal)";

/// Options for `tack stats`.
#[derive(Clone, Debug, Default)]
pub struct StatsOptions {
    /// Agent dir override (tests); defaults to `tack_session::default_agent_dir()`.
    pub agent_dir: Option<PathBuf>,
    /// `--since`: `YYYY-MM-DD` or `Nd` (N days ago).
    pub since: Option<String>,
    /// `--until`: `YYYY-MM-DD`.
    pub until: Option<String>,
    /// `--json`: machine-readable output.
    pub json: bool,
    /// `--dir`: only sessions for this project directory (a raw session dir
    /// is also accepted as a fallback).
    pub dir: Option<PathBuf>,
    /// Max days in the daily table (0 → [`DEFAULT_DAILY_LIMIT`]).
    pub daily_limit: usize,
}

// ---------------------------------------------------------------------------
// Date filtering
// ---------------------------------------------------------------------------

/// Parse a `--since` value: `YYYY-MM-DD`, or `Nd` = N days ago (so `7d`
/// covers roughly the last week, `0d` is today).
pub fn parse_since(value: &str, today: NaiveDate) -> Result<NaiveDate> {
    let value = value.trim();
    if let Some(days) = value.strip_suffix('d') {
        let n: i64 = days
            .trim()
            .parse()
            .with_context(|| format!("invalid --since {value:?}: expected YYYY-MM-DD or Nd"))?;
        anyhow::ensure!(n >= 0, "invalid --since {value:?}: day count must be >= 0");
        return Ok(today - chrono::Duration::days(n));
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .with_context(|| format!("invalid --since {value:?}: expected YYYY-MM-DD or Nd (e.g. 7d)"))
}

/// Parse an `--until` value: `YYYY-MM-DD` only.
pub fn parse_until(value: &str) -> Result<NaiveDate> {
    let value = value.trim();
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .with_context(|| format!("invalid --until {value:?}: expected YYYY-MM-DD"))
}

/// Extract the calendar day from an entry timestamp (`2026-08-30T07:40:30.183Z`).
/// Tolerant: only the leading `YYYY-MM-DD` must parse.
fn parse_day_str(timestamp: &str) -> Option<NaiveDate> {
    let day: String = timestamp.chars().take(10).collect();
    NaiveDate::parse_from_str(&day, "%Y-%m-%d").ok()
}

fn entry_day(v: &Value) -> Option<NaiveDate> {
    v.get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_day_str)
}

// ---------------------------------------------------------------------------
// Scanning (raw, defensive)
// ---------------------------------------------------------------------------

/// Token counters. `thinking` is a memo breakdown of `output` (not additive);
/// `total` is the provider-reported `totalTokens` (fallback: sum of the
/// billable buckets).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub thinking: u64,
    pub total: u64,
}

impl TokenTotals {
    fn add(&mut self, other: &TokenTotals) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.thinking += other.thinking;
        self.total += other.total;
    }

    fn from_usage(u: &tack_ai::Usage) -> Self {
        TokenTotals {
            input: u.input,
            output: u.output,
            cache_read: u.cache_read,
            cache_write: u.cache_write,
            thinking: u.reasoning.unwrap_or(0),
            total: if u.total_tokens > 0 {
                u.total_tokens
            } else {
                u.input + u.output + u.cache_read + u.cache_write
            },
        }
    }
}

/// One LLM usage observation (assistant turn, compaction, branch summary).
#[derive(Clone, Debug)]
pub struct UsageRecord {
    /// Calendar day of the entry; None when the timestamp is missing or
    /// unparsable (included in totals, excluded from the daily series).
    pub day: Option<NaiveDate>,
    pub provider: String,
    pub model: String,
    pub tokens: TokenTotals,
    /// Cost recorded in the entry at runtime (used for `internal` records,
    /// which carry no model identity to price against the catalog).
    pub stored_cost: Option<f64>,
    /// Compaction / branch-summary call: no model identity.
    pub internal: bool,
}

/// Where a scanned session came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScanSource {
    #[default]
    Jsonl,
    Sqlite,
}

/// Raw scan of one session (unfiltered; date filtering happens in
/// [`aggregate`]).
#[derive(Clone, Debug, Default)]
pub struct SessionScan {
    pub had_header: bool,
    /// Day of every `message` entry (None = unparsable timestamp).
    pub message_days: Vec<Option<NaiveDate>>,
    pub usages: Vec<UsageRecord>,
    /// Encrypted lines skipped because no key could decrypt them.
    pub encrypted_lines: usize,
    pub corrupt_lines: usize,
    pub source: ScanSource,
}

/// Counters for everything that could not be counted into usage.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Skipped {
    /// JSONL lines with a `tack-enc:v1:` prefix that could not be decrypted.
    pub encrypted_lines: usize,
    /// JSONL lines that were neither valid JSON nor encrypted payloads.
    pub corrupt_lines: usize,
    /// Session files that could not be read at all.
    pub unreadable_files: usize,
    /// SQLite-backend sessions included in the report.
    pub sqlite_sessions_included: usize,
    /// SQLite sessions skipped because their rows are undecryptable.
    pub sqlite_sessions_encrypted: usize,
    /// SQLite sessions skipped for any other load error.
    pub sqlite_sessions_unreadable: usize,
}

/// Result of scanning one or more project session dirs.
#[derive(Clone, Debug, Default)]
pub struct ScanOutcome {
    pub scans: Vec<SessionScan>,
    /// Project session dirs that exist and were scanned.
    pub project_dirs: usize,
    pub skipped: Skipped,
}

fn u64_at(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn tokens_from_value(usage: &Value) -> TokenTotals {
    let input = u64_at(usage, "input");
    let output = u64_at(usage, "output");
    let cache_read = u64_at(usage, "cacheRead");
    let cache_write = u64_at(usage, "cacheWrite");
    let total = u64_at(usage, "totalTokens");
    TokenTotals {
        input,
        output,
        cache_read,
        cache_write,
        thinking: u64_at(usage, "reasoning"),
        total: if total > 0 {
            total
        } else {
            input + output + cache_read + cache_write
        },
    }
}

fn stored_cost(usage: &Value) -> Option<f64> {
    usage
        .get("cost")
        .and_then(|c| c.get("total"))
        .and_then(Value::as_f64)
}

/// Classify one raw JSONL line into a scan update. Encrypted lines are
/// decrypted when possible (key installed) and otherwise counted as skipped;
/// malformed JSON is counted, never fatal.
fn fold_jsonl_line(line: &str, scan: &mut SessionScan) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let decrypted;
    let text = if tack_session::crypto::is_encrypted_line(trimmed) {
        match tack_session::crypto::decrypt_line(trimmed) {
            Some(plain) => {
                decrypted = plain;
                decrypted.as_str()
            }
            None => {
                scan.encrypted_lines += 1;
                return;
            }
        }
    } else {
        trimmed
    };
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        scan.corrupt_lines += 1;
        return;
    };
    fold_entry_value(&v, scan);
}

/// Fold one parsed entry (shared shape of the session format v3) into the
/// scan. Unknown/future entry types are ignored by construction.
fn fold_entry_value(v: &Value, scan: &mut SessionScan) {
    let Some(entry_type) = v.get("type").and_then(Value::as_str) else {
        return;
    };
    match entry_type {
        "session" => scan.had_header = true,
        "message" => {
            let day = entry_day(v);
            scan.message_days.push(day);
            let Some(message) = v.get("message") else {
                return;
            };
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                return;
            }
            // Failed/aborted turns carry no billable usage (mirrors
            // SessionManager::session_totals).
            let stop = message.get("stopReason").and_then(Value::as_str);
            if matches!(stop, Some("error" | "aborted")) {
                return;
            }
            let Some(usage) = message.get("usage") else {
                return;
            };
            scan.usages.push(UsageRecord {
                day,
                provider: message
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("(unknown)")
                    .to_string(),
                model: message
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("(unknown)")
                    .to_string(),
                tokens: tokens_from_value(usage),
                stored_cost: stored_cost(usage),
                internal: false,
            });
        }
        // LLM calls without a model identity: group under "(internal)".
        "compaction" | "branch_summary" => {
            let Some(usage) = v.get("usage") else { return };
            scan.usages.push(UsageRecord {
                day: entry_day(v),
                provider: INTERNAL_PROVIDER.to_string(),
                model: if entry_type == "compaction" {
                    "compaction".to_string()
                } else {
                    "branch-summary".to_string()
                },
                tokens: tokens_from_value(usage),
                stored_cost: stored_cost(usage),
                internal: true,
            });
        }
        _ => {}
    }
}

/// Scan one JSONL session file.
pub fn scan_jsonl_file(path: &Path) -> std::io::Result<SessionScan> {
    // Format sniff (headers are always plaintext): format-v4 transaction
    // logs fold via the v4 scanner, legacy v3 streams line by line.
    let mut first = String::new();
    {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
        reader.read_line(&mut first)?;
    }
    let is_v4 = serde_json::from_str::<Value>(first.trim())
        .ok()
        .and_then(|v| v.get("kind").and_then(Value::as_str).map(str::to_string))
        .as_deref()
        == Some("header");
    if is_v4 {
        let content = std::fs::read_to_string(path)?;
        let mut scan = SessionScan::default();
        // Count undecryptable lines for the skipped report (the v4 scanner
        // skips them silently).
        for line in content.lines().skip(1) {
            let trimmed = line.trim();
            if tack_session::crypto::is_encrypted_line(trimmed)
                && tack_session::crypto::decrypt_line(trimmed).is_none()
            {
                scan.encrypted_lines += 1;
            }
        }
        if let Some(v4) = tack_session::scan_v4_file_content(&content) {
            fold_v4_scan(v4, &mut scan);
        }
        return Ok(scan);
    }
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut scan = SessionScan::default();
    for line in reader.lines() {
        let Ok(line) = line else {
            scan.corrupt_lines += 1;
            continue;
        };
        fold_jsonl_line(&line, &mut scan);
    }
    Ok(scan)
}

/// Fold a scanned format-v4 session into the scan. The usage ledger is
/// authoritative (messages also embed usage — counting both would double
/// charge); with an empty ledger, fall back to message-embedded usage.
fn fold_v4_scan(v4: tack_session::V4FileScan, scan: &mut SessionScan) {
    use tack_session::SessionEntry;
    scan.had_header = true;
    let header_day = parse_day_str(&tack_session::millis_to_iso(v4.header.created_at));
    // Attribution maps: entry id → (provider, model) for assistant turns,
    // entry id → day.
    let mut entry_models: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut entry_days: std::collections::HashMap<String, Option<NaiveDate>> =
        std::collections::HashMap::new();
    for entry in &v4.entries {
        if let SessionEntry::Message {
            id,
            timestamp,
            message,
            ..
        } = entry
        {
            let day = parse_day_str(timestamp);
            scan.message_days.push(day);
            entry_days.insert(id.clone(), day);
            if let tack_agent_core::AgentMessage::Assistant(a) = message {
                entry_models.insert(id.clone(), (a.provider.clone(), a.model.clone()));
            }
        }
    }
    if !v4.usage_rows.is_empty() {
        for row in &v4.usage_rows {
            let usage = serde_json::to_value(&row.usage).unwrap_or(Value::Null);
            let (provider, model, internal) =
                match row.entry_id.as_deref().and_then(|id| entry_models.get(id)) {
                    Some((provider, model)) => (provider.clone(), model.clone(), false),
                    None => {
                        let source = row
                            .details
                            .as_ref()
                            .and_then(|d| d.get("source"))
                            .and_then(Value::as_str)
                            .unwrap_or("session");
                        (INTERNAL_PROVIDER.to_string(), source.to_string(), true)
                    }
                };
            let day = row
                .entry_id
                .as_deref()
                .and_then(|id| entry_days.get(id).copied())
                .unwrap_or(header_day);
            scan.usages.push(UsageRecord {
                day,
                provider,
                model,
                tokens: tokens_from_value(&usage),
                stored_cost: stored_cost(&usage),
                internal,
            });
        }
        return;
    }
    // No ledger rows: aggregate from message-embedded usage (the entries
    // serialize back to the v3 shapes fold_entry_value understands).
    for entry in &v4.entries {
        if let Ok(v) = serde_json::to_value(entry) {
            fold_entry_value(&v, scan);
        }
    }
}

/// Fold a SQLite-loaded session (parsed `SessionLine`s) into a scan.
fn fold_sqlite_lines(lines: Vec<tack_session::SessionLine>, scan: &mut SessionScan) {
    use tack_session::SessionEntry;
    for line in lines {
        let tack_session::SessionLine::Entry(entry) = line else {
            continue;
        };
        match entry {
            SessionEntry::Message {
                timestamp, message, ..
            } => {
                let day = parse_day_str(&timestamp);
                scan.message_days.push(day);
                if let tack_agent_core::AgentMessage::Assistant(a) = message {
                    if matches!(
                        a.stop_reason,
                        tack_ai::StopReason::Error | tack_ai::StopReason::Aborted
                    ) {
                        continue;
                    }
                    scan.usages.push(UsageRecord {
                        day,
                        provider: a.provider,
                        model: a.model,
                        tokens: TokenTotals::from_usage(&a.usage),
                        stored_cost: Some(a.usage.cost.total),
                        internal: false,
                    });
                }
            }
            SessionEntry::Compaction {
                timestamp,
                usage: Some(u),
                ..
            } => {
                scan.usages.push(UsageRecord {
                    day: parse_day_str(&timestamp),
                    provider: INTERNAL_PROVIDER.to_string(),
                    model: "compaction".to_string(),
                    tokens: TokenTotals::from_usage(&u),
                    stored_cost: Some(u.cost.total),
                    internal: true,
                });
            }
            SessionEntry::BranchSummary {
                timestamp,
                usage: Some(u),
                ..
            } => {
                scan.usages.push(UsageRecord {
                    day: parse_day_str(&timestamp),
                    provider: INTERNAL_PROVIDER.to_string(),
                    model: "branch-summary".to_string(),
                    tokens: TokenTotals::from_usage(&u),
                    stored_cost: Some(u.cost.total),
                    internal: true,
                });
            }
            _ => {}
        }
    }
}

/// Scan the SQLite backend of one project dir (`sessions.db` exists).
/// Sessions that fail to load (encrypted rows without the key, IO errors)
/// are counted, never fatal.
fn scan_sqlite_dir(dir: &Path, out: &mut ScanOutcome) {
    let conn = match tack_session::sqlite_backend::open_db(dir) {
        Ok(conn) => conn,
        Err(_) => {
            out.skipped.sqlite_sessions_unreadable += 1;
            return;
        }
    };
    let sessions = match tack_session::sqlite_backend::list_sessions(&conn) {
        Ok(sessions) => sessions,
        Err(_) => {
            out.skipped.sqlite_sessions_unreadable += 1;
            return;
        }
    };
    for (id, _header) in sessions {
        match tack_session::sqlite_backend::load_session(&conn, &id) {
            Ok((_header, lines)) => {
                let mut scan = SessionScan {
                    had_header: true,
                    source: ScanSource::Sqlite,
                    ..Default::default()
                };
                fold_sqlite_lines(lines, &mut scan);
                out.skipped.sqlite_sessions_included += 1;
                out.scans.push(scan);
            }
            Err(tack_session::SessionError::Encrypted(_)) => {
                out.skipped.sqlite_sessions_encrypted += 1;
            }
            Err(_) => {
                out.skipped.sqlite_sessions_unreadable += 1;
            }
        }
    }
}

fn scan_project_dir(dir: &Path, out: &mut ScanOutcome) {
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                match scan_jsonl_file(&path) {
                    Ok(scan) => out.scans.push(scan),
                    Err(_) => out.skipped.unreadable_files += 1,
                }
            }
        }
    }
    if tack_session::sqlite_backend::db_path(dir).is_file() {
        scan_sqlite_dir(dir, out);
    }
}

/// Resolve `--dir` to a session directory: the encoded project session dir
/// when it exists, else the path itself when it is a directory (raw session
/// dir), else the encoded path (scanning it yields an empty report).
fn resolve_project_session_dir(project: &Path, agent_dir: &Path) -> PathBuf {
    let absolute = std::path::absolute(project).unwrap_or_else(|_| project.to_path_buf());
    let encoded = tack_session::default_session_dir(&absolute, agent_dir);
    if encoded.is_dir() {
        encoded
    } else if project.is_dir() {
        project.to_path_buf()
    } else {
        encoded
    }
}

/// Scan all project session dirs under the agent dir (or just one with
/// `--dir`). Pure I/O, no date filtering.
pub fn scan_agent_dir(agent_dir: &Path, project: Option<&Path>) -> ScanOutcome {
    let mut out = ScanOutcome::default();
    let dirs: Vec<PathBuf> = match project {
        Some(p) => vec![resolve_project_session_dir(p, agent_dir)],
        None => std::fs::read_dir(agent_dir.join("sessions"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
    };
    for dir in &dirs {
        if !dir.is_dir() {
            continue;
        }
        out.project_dirs += 1;
        scan_project_dir(dir, &mut out);
    }
    out
}

// ---------------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------------

/// Per-million-token catalog rates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Rates {
    /// All-zero rates mean "no pricing information" (local providers
    /// default to zeroed costs), not "free".
    fn is_priced(&self) -> bool {
        self.input != 0.0 || self.output != 0.0 || self.cache_read != 0.0 || self.cache_write != 0.0
    }

    fn cost(&self, tokens: &TokenTotals) -> f64 {
        (self.input * tokens.input as f64
            + self.output * tokens.output as f64
            + self.cache_read * tokens.cache_read as f64
            + self.cache_write * tokens.cache_write as f64)
            / 1_000_000.0
    }
}

/// (provider, model) → catalog rates.
pub type Pricing = HashMap<(String, String), Rates>;

/// Build the pricing table from the effective catalog (build-time snapshot,
/// already overridden by `<agentDir>/catalog.json` at startup) plus custom
/// providers from `models.json` (these win on key collisions).
pub fn build_pricing(agent_dir: &Path) -> Pricing {
    let mut map = HashMap::new();
    for def in tack_ai::providers::BUILTIN_PROVIDERS {
        for m in tack_ai::providers::builtin_models(def.id).iter() {
            map.insert(
                (def.id.to_string(), m.id.clone()),
                Rates {
                    input: m.cost.input,
                    output: m.cost.output,
                    cache_read: m.cost.cache_read,
                    cache_write: m.cost.cache_write,
                },
            );
        }
    }
    for cp in tack_ai::providers::load_custom_providers(agent_dir) {
        for m in &cp.models {
            map.insert(
                (cp.id.clone(), m.id.clone()),
                Rates {
                    input: m.cost.input,
                    output: m.cost.output,
                    cache_read: m.cost.cache_read,
                    cache_write: m.cost.cache_write,
                },
            );
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Aggregation (pure)
// ---------------------------------------------------------------------------

/// One provider×model breakdown row.
#[derive(Clone, Debug, Serialize)]
pub struct ModelRow {
    pub provider: String,
    pub model: String,
    pub calls: usize,
    pub tokens: TokenTotals,
    /// Estimated cost in USD; None when the model has no catalog pricing
    /// (internal rows: runtime-recorded cost when available).
    pub cost_usd: Option<f64>,
}

/// One day of the time series.
#[derive(Clone, Debug, Serialize)]
pub struct DailyRow {
    pub date: String,
    pub tokens: TokenTotals,
    pub cost_usd: Option<f64>,
}

/// The full report (also the `--json` schema, `version: 1`).
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub version: u32,
    /// Report generation day (UTC).
    pub generated_on: String,
    pub since: Option<String>,
    pub until: Option<String>,
    pub projects: usize,
    pub sessions: usize,
    pub messages: usize,
    pub assistant_calls: usize,
    pub tokens: TokenTotals,
    /// Sum of all priced rows; None when nothing could be priced.
    pub cost_usd: Option<f64>,
    /// "provider/model" entries with usage but no catalog pricing
    /// (excluded from cost totals, shown as `-`).
    pub unpriced_models: Vec<String>,
    pub models: Vec<ModelRow>,
    pub daily: Vec<DailyRow>,
    /// Days with usage before the daily table limit was applied.
    pub daily_total_days: usize,
    pub skipped: Skipped,
}

fn in_range(day: &Option<NaiveDate>, since: Option<NaiveDate>, until: Option<NaiveDate>) -> bool {
    match day {
        // No usable timestamp: keep the entry in the totals (cannot prove it
        // is out of range) but it never lands in the daily series.
        None => true,
        Some(d) => since.is_none_or(|s| *d >= s) && until.is_none_or(|u| *d <= u),
    }
}

fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

/// Aggregate scanned sessions into a report. Pure: pricing, range, daily
/// limit and "today" are all parameters.
pub fn aggregate(
    outcome: &ScanOutcome,
    pricing: &Pricing,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
    daily_limit: usize,
    today: NaiveDate,
) -> Report {
    let mut sessions = 0usize;
    let mut messages = 0usize;
    let mut assistant_calls = 0usize;
    let mut totals = TokenTotals::default();
    let mut total_cost = 0.0f64;
    let mut any_cost = false;
    let mut unpriced: HashSet<(String, String)> = HashSet::new();

    struct RowAcc {
        calls: usize,
        tokens: TokenTotals,
        cost: f64,
        cost_known: bool,
    }
    let mut rows: HashMap<(String, String), RowAcc> = HashMap::new();
    struct DayAcc {
        tokens: TokenTotals,
        cost: f64,
        cost_known: bool,
    }
    let mut days: BTreeMap<NaiveDate, DayAcc> = BTreeMap::new();

    let mut skipped = outcome.skipped.clone();

    // Borrow-key pricing index built ONCE: HashMap<(String, String), _> has
    // no Borrow<(&str, &str)>, so a direct lookup would clone both strings
    // for every usage record (F49a).
    let pricing_index: HashMap<(&str, &str), Rates> = pricing
        .iter()
        .map(|((provider, model), rates)| ((provider.as_str(), model.as_str()), *rates))
        .collect();

    for scan in &outcome.scans {
        skipped.encrypted_lines += scan.encrypted_lines;
        skipped.corrupt_lines += scan.corrupt_lines;
        let in_range_messages = scan
            .message_days
            .iter()
            .filter(|d| in_range(d, since, until))
            .count();
        let mut has_in_range_usage = false;
        for rec in &scan.usages {
            if !in_range(&rec.day, since, until) {
                continue;
            }
            has_in_range_usage = true;
            if !rec.internal {
                assistant_calls += 1;
            }
            totals.add(&rec.tokens);

            // Cost: catalog pricing by (provider, model); internal records
            // fall back to the cost recorded at runtime; unpriced models
            // are listed and excluded from totals.
            let record_cost = if rec.internal {
                rec.stored_cost.filter(|c| *c > 0.0)
            } else {
                match pricing_index.get(&(rec.provider.as_str(), rec.model.as_str())) {
                    Some(rates) if rates.is_priced() => Some(rates.cost(&rec.tokens)),
                    _ => {
                        unpriced.insert((rec.provider.clone(), rec.model.clone()));
                        None
                    }
                }
            };

            let key = (rec.provider.clone(), rec.model.clone());
            let row = rows.entry(key).or_insert(RowAcc {
                calls: 0,
                tokens: TokenTotals::default(),
                cost: 0.0,
                cost_known: false,
            });
            row.calls += 1;
            row.tokens.add(&rec.tokens);
            if let Some(c) = record_cost {
                row.cost += c;
                row.cost_known = true;
            }

            if let Some(day) = rec.day {
                let acc = days.entry(day).or_insert(DayAcc {
                    tokens: TokenTotals::default(),
                    cost: 0.0,
                    cost_known: false,
                });
                acc.tokens.add(&rec.tokens);
                if let Some(c) = record_cost {
                    acc.cost += c;
                    acc.cost_known = true;
                }
            }

            if let Some(c) = record_cost {
                total_cost += c;
                any_cost = true;
            }
        }
        // A session counts when it contributed at least one message or usage
        // record inside the selected range.
        if scan.had_header && (in_range_messages > 0 || has_in_range_usage) {
            sessions += 1;
        }
        messages += in_range_messages;
    }

    let mut models: Vec<ModelRow> = rows
        .into_iter()
        .map(|((provider, model), acc)| ModelRow {
            provider,
            model,
            calls: acc.calls,
            tokens: acc.tokens,
            cost_usd: acc.cost_known.then(|| round6(acc.cost)),
        })
        .collect();
    models.sort_by(|a, b| {
        b.tokens
            .total
            .cmp(&a.tokens.total)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.model.cmp(&b.model))
    });

    let daily_total_days = days.len();
    let daily: Vec<DailyRow> = days
        .into_iter()
        .skip(daily_total_days.saturating_sub(daily_limit.max(1)))
        .map(|(day, acc)| DailyRow {
            date: day.format("%Y-%m-%d").to_string(),
            tokens: acc.tokens,
            cost_usd: acc.cost_known.then(|| round6(acc.cost)),
        })
        .collect();

    let mut unpriced_models: Vec<String> = unpriced
        .into_iter()
        .map(|(p, m)| format!("{p}/{m}"))
        .collect();
    unpriced_models.sort();

    Report {
        version: 1,
        generated_on: today.format("%Y-%m-%d").to_string(),
        since: since.map(|d| d.format("%Y-%m-%d").to_string()),
        until: until.map(|d| d.format("%Y-%m-%d").to_string()),
        projects: outcome.project_dirs,
        sessions,
        messages,
        assistant_calls,
        tokens: totals,
        cost_usd: any_cost.then(|| round6(total_cost)),
        unpriced_models,
        models,
        daily,
        daily_total_days,
        skipped,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Thousands-separated integer.
fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_money(cost: Option<f64>) -> String {
    cost.map_or_else(|| "-".to_string(), |c| format!("${c:.4}"))
}

fn render_table(headers: &[&str], rows: &[Vec<String>], right_cols: usize) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let fmt_row = |cells: &[String], out: &mut String| {
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            if i >= cols - right_cols {
                out.push_str(&format!("{:>width$}", cell, width = widths[i]));
            } else {
                out.push_str(&format!("{:<width$}", cell, width = widths[i]));
            }
        }
        out.push('\n');
    };
    fmt_row(
        &headers.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
        &mut out,
    );
    for row in rows {
        fmt_row(row, &mut out);
    }
    out
}

/// Human-readable rendering: overview, provider×model table with a TOTAL
/// row, daily series, and skip/caveat footnotes.
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();
    out.push_str("tack stats — cross-session usage\n");
    match (&report.since, &report.until) {
        (None, None) => out.push_str("Range: all time\n"),
        (s, u) => out.push_str(&format!(
            "Range: {} .. {}\n",
            s.as_deref().unwrap_or("(beginning)"),
            u.as_deref().unwrap_or("(today)")
        )),
    }
    out.push('\n');
    out.push_str(&format!(
        "Projects: {}   Sessions: {}   Messages: {}   Assistant calls: {}\n\n",
        report.projects, report.sessions, report.messages, report.assistant_calls
    ));
    let t = &report.tokens;
    out.push_str("Tokens:\n");
    out.push_str(&format!("  input       {:>15}\n", fmt_int(t.input)));
    out.push_str(&format!("  output      {:>15}\n", fmt_int(t.output)));
    out.push_str(&format!("  cache read  {:>15}\n", fmt_int(t.cache_read)));
    out.push_str(&format!("  cache write {:>15}\n", fmt_int(t.cache_write)));
    out.push_str(&format!("  thinking    {:>15}\n", fmt_int(t.thinking)));
    out.push_str(&format!("  total       {:>15}\n", fmt_int(t.total)));
    out.push('\n');

    if !report.models.is_empty() {
        out.push_str("By provider / model:\n");
        let mut rows: Vec<Vec<String>> = report
            .models
            .iter()
            .map(|m| {
                vec![
                    m.provider.clone(),
                    m.model.clone(),
                    fmt_int(m.calls as u64),
                    fmt_int(m.tokens.input),
                    fmt_int(m.tokens.output),
                    fmt_int(m.tokens.cache_read),
                    fmt_int(m.tokens.cache_write),
                    fmt_int(m.tokens.thinking),
                    fmt_money(m.cost_usd),
                ]
            })
            .collect();
        rows.push(vec![
            "TOTAL".to_string(),
            String::new(),
            fmt_int(report.assistant_calls as u64),
            fmt_int(t.input),
            fmt_int(t.output),
            fmt_int(t.cache_read),
            fmt_int(t.cache_write),
            fmt_int(t.thinking),
            fmt_money(report.cost_usd),
        ]);
        out.push_str(&render_table(
            &[
                "PROVIDER", "MODEL", "CALLS", "INPUT", "OUTPUT", "CACHE RD", "CACHE WR",
                "THINKING", "COST",
            ],
            &rows,
            7,
        ));
        out.push('\n');
    }

    if !report.daily.is_empty() {
        if report.daily_total_days > report.daily.len() {
            out.push_str(&format!(
                "Daily (last {} of {} days with usage):\n",
                report.daily.len(),
                report.daily_total_days
            ));
        } else {
            out.push_str("Daily:\n");
        }
        let rows: Vec<Vec<String>> = report
            .daily
            .iter()
            .map(|d| {
                vec![
                    d.date.clone(),
                    fmt_int(d.tokens.input),
                    fmt_int(d.tokens.output),
                    fmt_int(d.tokens.cache_read),
                    fmt_int(d.tokens.cache_write),
                    fmt_int(d.tokens.thinking),
                    fmt_money(d.cost_usd),
                ]
            })
            .collect();
        out.push_str(&render_table(
            &[
                "DATE", "INPUT", "OUTPUT", "CACHE RD", "CACHE WR", "THINKING", "COST",
            ],
            &rows,
            6,
        ));
        out.push('\n');
    }

    // Footnotes: cost caveat + everything that was skipped.
    out.push_str(
        "Cost is an estimate from catalog pricing ($/1M tokens); actual billing may differ.\n",
    );
    if !report.unpriced_models.is_empty() {
        out.push_str(&format!(
            "{} model(s) without pricing shown as '-' and excluded from cost totals: {}\n",
            report.unpriced_models.len(),
            report.unpriced_models.join(", ")
        ));
    }
    if report
        .models
        .iter()
        .any(|m| m.provider == INTERNAL_PROVIDER)
    {
        out.push_str(
            "(internal) rows are compaction/branch-summary LLM calls; their cost uses the \
             value recorded at runtime.\n",
        );
    }
    let s = &report.skipped;
    if s.encrypted_lines > 0 {
        out.push_str(&format!(
            "{} encrypted entr{} skipped (tack-enc:v1, key unavailable)\n",
            s.encrypted_lines,
            if s.encrypted_lines == 1 { "y" } else { "ies" }
        ));
    }
    if s.corrupt_lines > 0 {
        out.push_str(&format!("{} corrupt line(s) skipped\n", s.corrupt_lines));
    }
    if s.unreadable_files > 0 {
        out.push_str(&format!(
            "{} unreadable session file(s) skipped\n",
            s.unreadable_files
        ));
    }
    if s.sqlite_sessions_included > 0
        || s.sqlite_sessions_encrypted > 0
        || s.sqlite_sessions_unreadable > 0
    {
        out.push_str(&format!(
            "SQLite backend: {} session(s) included, {} skipped ({} encrypted, {} unreadable)\n",
            s.sqlite_sessions_included,
            s.sqlite_sessions_encrypted + s.sqlite_sessions_unreadable,
            s.sqlite_sessions_encrypted,
            s.sqlite_sessions_unreadable
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run `tack stats`.
pub fn run(opts: &StatsOptions) -> Result<()> {
    let agent_dir = opts
        .agent_dir
        .clone()
        .unwrap_or_else(tack_session::default_agent_dir);
    let today = chrono::Utc::now().date_naive();
    let since = opts
        .since
        .as_deref()
        .map(|s| parse_since(s, today))
        .transpose()?;
    let until = opts.until.as_deref().map(parse_until).transpose()?;
    if let (Some(s), Some(u)) = (since, until) {
        anyhow::ensure!(s <= u, "--since ({s}) is after --until ({u})");
    }
    let outcome = scan_agent_dir(&agent_dir, opts.dir.as_deref());
    let pricing = build_pricing(&agent_dir);
    let daily_limit = if opts.daily_limit == 0 {
        DEFAULT_DAILY_LIMIT
    } else {
        opts.daily_limit
    };
    let report = aggregate(&outcome, &pricing, since, until, daily_limit, today);
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn tokens(
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        thinking: u64,
    ) -> TokenTotals {
        TokenTotals {
            input,
            output,
            cache_read,
            cache_write,
            thinking,
            total: input + output + cache_read + cache_write,
        }
    }

    fn rec(
        d: Option<&str>,
        provider: &str,
        model: &str,
        tokens: TokenTotals,
        internal: bool,
    ) -> UsageRecord {
        UsageRecord {
            day: d.map(day),
            provider: provider.to_string(),
            model: model.to_string(),
            tokens,
            stored_cost: None,
            internal,
        }
    }

    fn pricing() -> Pricing {
        HashMap::from([
            (
                ("anthropic".to_string(), "claude-test".to_string()),
                Rates {
                    input: 3.0,
                    output: 15.0,
                    cache_read: 0.3,
                    cache_write: 3.75,
                },
            ),
            // Local provider: all-zero rates == unpriced.
            (
                ("ollama".to_string(), "local-model".to_string()),
                Rates::default(),
            ),
        ])
    }

    // --- date parsing ---

    #[test]
    fn parse_since_accepts_dates_and_relative_days() {
        let today = day("2026-08-31");
        assert_eq!(parse_since("2026-08-01", today).unwrap(), day("2026-08-01"));
        assert_eq!(parse_since("7d", today).unwrap(), day("2026-08-24"));
        assert_eq!(parse_since("0d", today).unwrap(), today);
        assert!(parse_since("nonsense", today).is_err());
        assert!(parse_since("-3d", today).is_err());
        assert!(parse_since("7", today).is_err());
        assert!(parse_since("2026-13-01", today).is_err());
    }

    #[test]
    fn parse_until_accepts_only_dates() {
        assert_eq!(parse_until("2026-08-31").unwrap(), day("2026-08-31"));
        assert!(parse_until("7d").is_err());
    }

    #[test]
    fn parse_day_str_tolerates_iso_timestamps() {
        assert_eq!(
            parse_day_str("2026-08-30T07:40:30.183Z"),
            Some(day("2026-08-30"))
        );
        assert_eq!(
            parse_day_str("2026-08-30T00:00:00Z"),
            Some(day("2026-08-30"))
        );
        assert_eq!(parse_day_str("garbage"), None);
        assert_eq!(parse_day_str(""), None);
    }

    // --- aggregation ---

    fn fixture_outcome() -> ScanOutcome {
        let mut s1 = SessionScan {
            had_header: true,
            ..Default::default()
        };
        s1.message_days = vec![
            Some(day("2026-08-30")),
            Some(day("2026-08-30")),
            Some(day("2026-08-31")),
        ];
        s1.usages = vec![
            rec(
                Some("2026-08-30"),
                "anthropic",
                "claude-test",
                tokens(1_000_000, 100_000, 400_000, 10_000, 5_000),
                false,
            ),
            rec(
                Some("2026-08-31"),
                "anthropic",
                "claude-test",
                tokens(200_000, 20_000, 0, 0, 1_000),
                false,
            ),
            rec(
                Some("2026-08-31"),
                "ollama",
                "local-model",
                tokens(50_000, 5_000, 0, 0, 0),
                false,
            ),
        ];
        let mut s2 = SessionScan {
            had_header: true,
            ..Default::default()
        };
        s2.message_days = vec![Some(day("2026-07-01"))];
        s2.usages = vec![UsageRecord {
            stored_cost: Some(0.0123),
            ..rec(
                Some("2026-08-30"),
                INTERNAL_PROVIDER,
                "compaction",
                tokens(10_000, 2_000, 0, 0, 0),
                true,
            )
        }];
        ScanOutcome {
            scans: vec![s1, s2],
            project_dirs: 2,
            skipped: Skipped::default(),
        }
    }

    #[test]
    fn aggregate_totals_models_and_cost() {
        let report = aggregate(
            &fixture_outcome(),
            &pricing(),
            None,
            None,
            30,
            day("2026-08-31"),
        );
        assert_eq!(report.sessions, 2);
        assert_eq!(report.messages, 4);
        assert_eq!(report.assistant_calls, 3);
        assert_eq!(report.tokens.input, 1_260_000);
        assert_eq!(report.tokens.thinking, 6_000);

        // anthropic/claude-test priced: 1.2M in * 3 + 120k out * 15 + 400k cr * 0.3 + 10k cw * 3.75
        let expected = (1_200_000.0 * 3.0 + 120_000.0 * 15.0 + 400_000.0 * 0.3 + 10_000.0 * 3.75)
            / 1_000_000.0;
        let claude = report
            .models
            .iter()
            .find(|m| m.model == "claude-test")
            .unwrap();
        assert_eq!(claude.calls, 2);
        assert!((claude.cost_usd.unwrap() - expected).abs() < 1e-6);

        // Unpriced model: no cost, listed.
        let local = report
            .models
            .iter()
            .find(|m| m.model == "local-model")
            .unwrap();
        assert_eq!(local.cost_usd, None);
        assert_eq!(
            report.unpriced_models,
            vec!["ollama/local-model".to_string()]
        );

        // Internal compaction row: runtime-recorded cost.
        let internal = report
            .models
            .iter()
            .find(|m| m.provider == INTERNAL_PROVIDER)
            .unwrap();
        assert_eq!(internal.cost_usd, Some(0.0123));

        // Total cost = priced estimate + internal recorded cost.
        assert!((report.cost_usd.unwrap() - (expected + 0.0123)).abs() < 1e-6);

        // Rows sorted by total tokens desc: claude first.
        assert_eq!(report.models[0].model, "claude-test");
    }

    #[test]
    fn aggregate_filters_by_since_until() {
        let outcome = fixture_outcome();
        // --since 2026-08-31: only the two in-range assistant calls.
        let report = aggregate(
            &outcome,
            &pricing(),
            Some(day("2026-08-31")),
            None,
            30,
            day("2026-08-31"),
        );
        assert_eq!(report.assistant_calls, 2);
        assert_eq!(report.messages, 1);
        // Session 2 contributed no in-range message/usage → not counted.
        assert_eq!(report.sessions, 1);
        assert_eq!(report.daily.len(), 1);
        assert_eq!(report.daily[0].date, "2026-08-31");

        // --until 2026-08-30: the other day; compaction record is in range.
        let report = aggregate(
            &outcome,
            &pricing(),
            None,
            Some(day("2026-08-30")),
            30,
            day("2026-08-31"),
        );
        assert_eq!(report.assistant_calls, 1);
        // s1's two 08-30 messages plus s2's 07-01 message.
        assert_eq!(report.messages, 3);
        assert_eq!(report.sessions, 2);
        assert!(
            report
                .models
                .iter()
                .any(|m| m.provider == INTERNAL_PROVIDER)
        );

        // Empty range.
        let report = aggregate(
            &outcome,
            &pricing(),
            Some(day("2020-01-01")),
            Some(day("2020-01-02")),
            30,
            day("2026-08-31"),
        );
        assert_eq!(report.sessions, 0);
        assert_eq!(report.messages, 0);
        assert!(report.models.is_empty());
        assert_eq!(report.cost_usd, None);
    }

    #[test]
    fn aggregate_keeps_dayless_records_in_totals_only() {
        let mut scan = SessionScan {
            had_header: true,
            ..Default::default()
        };
        scan.message_days = vec![None];
        scan.usages = vec![rec(
            None,
            "anthropic",
            "claude-test",
            tokens(100, 10, 0, 0, 0),
            false,
        )];
        let outcome = ScanOutcome {
            scans: vec![scan],
            project_dirs: 1,
            skipped: Skipped::default(),
        };
        let report = aggregate(
            &outcome,
            &pricing(),
            Some(day("2026-08-01")),
            None,
            30,
            day("2026-08-31"),
        );
        assert_eq!(report.tokens.input, 100);
        assert_eq!(report.sessions, 1);
        assert!(report.daily.is_empty());
    }

    #[test]
    fn aggregate_limits_daily_series() {
        let mut scan = SessionScan {
            had_header: true,
            ..Default::default()
        };
        let base = day("2026-07-01");
        for i in 0..40 {
            let d = base + chrono::Duration::days(i);
            scan.message_days.push(Some(d));
            scan.usages.push(UsageRecord {
                day: Some(d),
                provider: "anthropic".to_string(),
                model: "claude-test".to_string(),
                tokens: tokens(1, 1, 0, 0, 0),
                stored_cost: None,
                internal: false,
            });
        }
        let outcome = ScanOutcome {
            scans: vec![scan],
            project_dirs: 1,
            skipped: Skipped::default(),
        };
        let report = aggregate(&outcome, &pricing(), None, None, 30, day("2026-08-31"));
        assert_eq!(report.daily_total_days, 40);
        assert_eq!(report.daily.len(), 30);
        // The 30 most recent days are kept.
        assert_eq!(report.daily[0].date, "2026-07-11");
        assert_eq!(report.daily[29].date, "2026-08-09");
        // Totals still cover everything.
        assert_eq!(report.tokens.input, 40);
    }

    // --- JSONL scanning ---

    #[test]
    fn scan_jsonl_fixture_counts_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let project_dir = agent_dir.join("sessions/--work-proj--");
        std::fs::create_dir_all(&project_dir).unwrap();
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-08-30T00:00:00Z\",\"cwd\":\"/work/proj\"}\n",
            "{\"type\":\"message\",\"id\":\"m0\",\"parentId\":null,\"timestamp\":\"2026-08-30T01:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
            "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":\"m0\",\"timestamp\":\"2026-08-30T01:00:05Z\",\"message\":{\"role\":\"assistant\",\"content\":[],\"api\":\"anthropic-messages\",\"provider\":\"anthropic\",\"model\":\"claude-test\",\"usage\":{\"input\":1000,\"output\":200,\"cacheRead\":300,\"cacheWrite\":50,\"reasoning\":25,\"totalTokens\":1550,\"cost\":{\"input\":0.003,\"output\":0.003,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":0.006}},\"stopReason\":\"stop\",\"timestamp\":2}}\n",
            // Aborted assistant turn: counts as a message, not as usage.
            "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"timestamp\":\"2026-08-30T01:01:00Z\",\"message\":{\"role\":\"assistant\",\"content\":[],\"api\":\"anthropic-messages\",\"provider\":\"anthropic\",\"model\":\"claude-test\",\"usage\":{\"input\":9,\"output\":9,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":18,\"cost\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"total\":0}},\"stopReason\":\"aborted\",\"timestamp\":3}}\n",
            "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":\"m2\",\"timestamp\":\"2026-08-30T02:00:00Z\",\"summary\":\"s\",\"firstKeptEntryId\":\"m1\",\"tokensBefore\":5000,\"usage\":{\"input\":500,\"output\":100,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":600,\"cost\":{\"input\":0.0015,\"output\":0.0015,\"cacheRead\":0,\"cacheWrite\":0,\"total\":0.003}}}\n",
            // Encrypted line without a key: skipped + counted.
            "tack-enc:v1:QUFBQUFBQUFBQUFBQUFBQUFBQUE=\n",
            // Corrupt lines: skipped + counted.
            "{not json\n",
            "{\"type\":\"message\",\"broken\":\n",
            // Unknown future entry type: ignored silently.
            "{\"type\":\"future_thing\",\"id\":\"f1\",\"timestamp\":\"2026-08-30T03:00:00Z\",\"data\":{}}\n",
        );
        std::fs::write(project_dir.join("a.jsonl"), content).unwrap();

        let outcome = scan_agent_dir(&agent_dir, None);
        assert_eq!(outcome.project_dirs, 1);
        assert_eq!(outcome.scans.len(), 1);
        let scan = &outcome.scans[0];
        assert!(scan.had_header);
        assert_eq!(scan.message_days.len(), 3);
        assert_eq!(scan.encrypted_lines, 1);
        assert_eq!(scan.corrupt_lines, 2);

        let report = aggregate(&outcome, &pricing(), None, None, 30, day("2026-08-31"));
        assert_eq!(report.sessions, 1);
        assert_eq!(report.messages, 3);
        assert_eq!(report.assistant_calls, 1, "aborted turn excluded");
        assert_eq!(report.tokens.input, 1500);
        assert_eq!(report.tokens.output, 300);
        assert_eq!(report.tokens.cache_read, 300);
        assert_eq!(report.tokens.cache_write, 50);
        assert_eq!(report.tokens.thinking, 25);
        assert_eq!(report.tokens.total, 1550 + 600);
        assert_eq!(report.skipped.encrypted_lines, 1);
        assert_eq!(report.skipped.corrupt_lines, 2);
        let internal = report
            .models
            .iter()
            .find(|m| m.provider == INTERNAL_PROVIDER)
            .unwrap();
        assert_eq!(internal.cost_usd, Some(0.003));
    }

    #[test]
    fn scan_dir_flag_targets_one_project() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let project = tmp.path().join("my project");
        std::fs::create_dir_all(&project).unwrap();
        let session_dir = tack_session::default_session_dir(&project, &agent_dir);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("s.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-08-30T00:00:00Z\",\"cwd\":\"/x\"}\n\
             {\"type\":\"message\",\"id\":\"m0\",\"parentId\":null,\"timestamp\":\"2026-08-30T01:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
        )
        .unwrap();
        // Another project that must NOT be picked up.
        let other = agent_dir.join("sessions/--other--");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("o.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"id\":\"s2\",\"timestamp\":\"2026-08-30T00:00:00Z\",\"cwd\":\"/other\"}\n",
        )
        .unwrap();

        let outcome = scan_agent_dir(&agent_dir, Some(&project));
        assert_eq!(outcome.project_dirs, 1);
        assert_eq!(outcome.scans.len(), 1);

        let outcome = scan_agent_dir(&agent_dir, None);
        assert_eq!(outcome.project_dirs, 2);
        assert_eq!(outcome.scans.len(), 2);
    }

    // --- SQLite backend ---

    #[test]
    fn sqlite_backend_sessions_are_included() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let session_dir = agent_dir.join("sessions/--proj--");
        std::fs::create_dir_all(&session_dir).unwrap();
        let manager = tack_session::SessionManager::create_with_backend(
            tmp.path(),
            Some(session_dir.clone()),
            tack_session::SessionBackend::Sqlite,
        )
        .unwrap();
        drop(manager);

        let outcome = scan_agent_dir(&agent_dir, None);
        assert_eq!(outcome.skipped.sqlite_sessions_included, 1);
        assert_eq!(outcome.scans.len(), 1);
        assert_eq!(outcome.scans[0].source, ScanSource::Sqlite);
        assert!(outcome.scans[0].had_header);
    }

    // --- JSON schema ---

    #[test]
    fn json_output_schema_is_stable() {
        let report = aggregate(
            &fixture_outcome(),
            &pricing(),
            Some(day("2026-08-01")),
            None,
            30,
            day("2026-08-31"),
        );
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["generated_on"], "2026-08-31");
        assert_eq!(v["since"], "2026-08-01");
        assert!(v["until"].is_null());
        assert_eq!(v["sessions"], 2);
        assert_eq!(v["messages"], 3);
        assert_eq!(v["assistant_calls"], 3);
        for key in [
            "input",
            "output",
            "cache_read",
            "cache_write",
            "thinking",
            "total",
        ] {
            assert!(v["tokens"][key].is_u64(), "tokens.{key}");
        }
        assert!(v["cost_usd"].is_f64());
        let model = &v["models"][0];
        for key in ["provider", "model", "calls", "tokens", "cost_usd"] {
            assert!(model.get(key).is_some(), "models[].{key}");
        }
        assert_eq!(v["unpriced_models"][0], "ollama/local-model");
        let day_row = &v["daily"][0];
        assert!(day_row["date"].is_string());
        assert!(day_row["tokens"]["input"].is_u64());
        assert!(v["daily_total_days"].is_u64());
        for key in [
            "encrypted_lines",
            "corrupt_lines",
            "unreadable_files",
            "sqlite_sessions_included",
            "sqlite_sessions_encrypted",
            "sqlite_sessions_unreadable",
        ] {
            assert!(v["skipped"][key].is_u64(), "skipped.{key}");
        }
    }

    #[test]
    fn render_text_shows_tables_and_footnotes() {
        let mut outcome = fixture_outcome();
        outcome.scans[0].encrypted_lines = 2;
        outcome.scans[0].corrupt_lines = 1;
        let report = aggregate(&outcome, &pricing(), None, None, 30, day("2026-08-31"));
        let text = render_text(&report);
        assert!(text.contains("tack stats"), "{text}");
        assert!(text.contains("Range: all time"), "{text}");
        assert!(text.contains("Sessions: 2"), "{text}");
        assert!(text.contains("claude-test"), "{text}");
        assert!(text.contains("TOTAL"), "{text}");
        assert!(text.contains('$'), "{text}");
        assert!(text.contains("estimate"), "{text}");
        assert!(text.contains("ollama/local-model"), "{text}");
        assert!(text.contains("2 encrypted entries skipped"), "{text}");
        assert!(text.contains("1 corrupt line(s) skipped"), "{text}");
    }
}

#[cfg(test)]
mod v4_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// A v4 fixture: header + one assistant message entry (with embedded
    /// usage) + one usage ledger row attributed to it.
    fn v4_fixture(with_ledger: bool) -> String {
        let mut content = concat!(
            "{\"v\":4,\"kind\":\"header\",\"id\":\"s1\",\"storageVersion\":1,\"createdAt\":1788103200000,\"cwd\":\"/work\"}\n",
            "{\"kind\":\"entry\",\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"seq\":1,\"timestamp\":1788103201000,",
            "\"message\":{\"role\":\"assistant\",\"content\":[],\"api\":\"anthropic-messages\",\"provider\":\"anthropic\",\"model\":\"claude-test\",",
            "\"usage\":{\"input\":1000,\"output\":200,\"cacheRead\":0,\"cacheWrite\":0,\"reasoning\":0,\"totalTokens\":1200,",
            "\"cost\":{\"input\":0.0,\"output\":0.0,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":0.0}},\"stopReason\":\"stop\",\"timestamp\":2}}\n",
        )
        .to_string();
        if with_ledger {
            content.push_str(concat!(
                "{\"kind\":\"usage\",\"id\":\"u1\",\"seq\":2,",
                "\"usage\":{\"input\":1000,\"output\":200,\"cacheRead\":0,\"cacheWrite\":0,\"reasoning\":0,\"totalTokens\":1200,",
                "\"cost\":{\"input\":0.0,\"output\":0.0,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":0.0}},",
                "\"entryId\":\"m1\",\"adjustment\":false}\n",
            ));
        }
        content
    }

    #[test]
    fn v4_ledger_is_authoritative_no_double_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v4.jsonl");
        std::fs::write(&path, v4_fixture(true)).unwrap();
        let scan = scan_jsonl_file(&path).unwrap();
        assert!(scan.had_header);
        // One record from the ledger — the message-embedded usage must NOT
        // be counted a second time.
        assert_eq!(scan.usages.len(), 1, "{:?}", scan.usages);
        let usage = &scan.usages[0];
        assert_eq!(usage.provider, "anthropic");
        assert_eq!(usage.model, "claude-test");
        assert!(!usage.internal);
        assert_eq!(usage.tokens.input, 1000);
        assert_eq!(usage.tokens.output, 200);
        assert_eq!(usage.day, Some(day("2026-08-30")));
        assert_eq!(scan.message_days.len(), 1);
    }

    #[test]
    fn v4_without_ledger_falls_back_to_message_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v4.jsonl");
        std::fs::write(&path, v4_fixture(false)).unwrap();
        let scan = scan_jsonl_file(&path).unwrap();
        assert_eq!(scan.usages.len(), 1, "{:?}", scan.usages);
        assert_eq!(scan.usages[0].tokens.input, 1000);
        assert_eq!(scan.usages[0].provider, "anthropic");
    }

    #[test]
    fn v4_unattributed_rows_are_internal_with_header_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v4.jsonl");
        // Migration-style import row: no entryId, details.source.
        let content = concat!(
            "{\"v\":4,\"kind\":\"header\",\"id\":\"s1\",\"storageVersion\":1,\"createdAt\":1788103200000,\"cwd\":\"/work\"}\n",
            "{\"kind\":\"usage\",\"id\":\"u1\",\"seq\":1,",
            "\"usage\":{\"input\":42,\"output\":7,\"cacheRead\":0,\"cacheWrite\":0,\"reasoning\":0,\"totalTokens\":49,",
            "\"cost\":{\"input\":0.0,\"output\":0.0,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":0.0}},",
            "\"adjustment\":true,\"details\":{\"source\":\"v3-import\"}}\n",
        );
        std::fs::write(&path, content).unwrap();
        let scan = scan_jsonl_file(&path).unwrap();
        assert_eq!(scan.usages.len(), 1);
        let usage = &scan.usages[0];
        assert!(usage.internal);
        assert_eq!(usage.provider, INTERNAL_PROVIDER);
        assert_eq!(usage.model, "v3-import");
        assert_eq!(usage.tokens.input, 42);
        assert_eq!(usage.day, Some(day("2026-08-30")));
    }
}
