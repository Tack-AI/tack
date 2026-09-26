//! Session-aware agent hooks: auto-compaction before LLM calls (port of the
//! harness compaction trigger) and message persistence is handled by the app
//! event loop.

use std::sync::Arc;

use async_trait::async_trait;
use tack_agent_core::{AgentHooks, AgentMessage};
use tack_ai::{Model, Provider, ThinkingLevel};
use tack_session::{
    CompactionSettings, SessionManager, build_session_context, estimate_context_tokens,
    prepare_compaction, should_compact,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Hooks that run auto-compaction when the context approaches the window.
pub struct SessionHooks {
    pub session: Arc<Mutex<SessionManager>>,
    pub model: Model,
    pub provider: Arc<dyn Provider>,
    pub auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    pub reasoning: Option<ThinkingLevel>,
    pub settings: CompactionSettings,
    pub cancel: CancellationToken,
    /// UI notification after a successful auto-compaction
    /// (summary, tokens_before); None in non-interactive modes.
    pub on_compaction: Option<tokio::sync::mpsc::UnboundedSender<(String, u64)>>,
    /// History optimization (microcompaction, duplicate-read masking,
    /// fresh tool-result cap) + spill dir. Rewritten content lands in the
    /// spill dir so the model can re-read it on demand; the session file
    /// always keeps the full content.
    pub history: Option<(HistorySettings, std::path::PathBuf)>,
    /// PreCompact/PostCompact hook groups + engine (empty = not configured).
    pub hook_engine: crate::shell_hooks::HookEngine,
    pub pre_compact: Vec<crate::shell_hooks::HookGroup>,
    pub post_compact: Vec<crate::shell_hooks::HookGroup>,
    pub hook_session_id: String,
}

/// Microcompaction tuning (settings `microcompact.*`).
///
/// Budgets are BYTE counts, not char counts (counting chars requires an
/// O(n) scan per tool result; byte length is free). The defaults are 4×
/// the historical char budgets, which approximates the same threshold for
/// multi-byte-heavy output while being exact for ASCII.
#[derive(Clone, Copy, Debug)]
pub struct MicrocompactSettings {
    /// Tool results larger than this are eligible (default 80_000 bytes ≈
    /// the old 20_000-char budget ×4).
    pub max_chars: usize,
    /// This many most-recent tool results are always kept full (default 3).
    pub keep_recent: usize,
    /// Only rewrite history when at least this many bytes are reclaimable
    /// (default 32_000 ≈ the old 8_000-char budget ×4). Rewriting old
    /// messages invalidates the provider prompt cache from the first
    /// rewritten message on, so shrinking a single small result is a net
    /// loss: the gate makes microcompaction pay for the cache miss it
    /// causes.
    pub min_savings_chars: usize,
}

impl Default for MicrocompactSettings {
    fn default() -> Self {
        MicrocompactSettings {
            max_chars: 80_000,
            keep_recent: 3,
            min_savings_chars: 32_000,
        }
    }
}

/// History-optimization tuning beyond microcompaction.
#[derive(Clone, Copy, Debug)]
pub struct HistorySettings {
    pub micro: MicrocompactSettings,
    /// Mask a `read` tool result when the same file was re-read later
    /// with byte-identical content (settings `maskDuplicateReads`).
    pub mask_duplicate_reads: bool,
    /// Hard cap applied to ANY tool result, fresh ones included, with the
    /// full text spilled to disk (settings `toolResultMaxChars`, 0 = off).
    /// Byte budget (see MicrocompactSettings docs; default 240_000 ≈ the
    /// old 60_000-char budget ×4).
    pub tool_result_max_chars: usize,
}

impl Default for HistorySettings {
    fn default() -> Self {
        HistorySettings {
            micro: MicrocompactSettings::default(),
            mask_duplicate_reads: true,
            tool_result_max_chars: 240_000,
        }
    }
}

/// Byte length of a tool result's text content. Threshold budgets are in
/// bytes (free to compute) rather than chars (O(n) per result per pass) —
/// the budgets are approximate anyway, and the defaults were scaled 4× to
/// compensate for multi-byte text.
fn tool_result_text_len(result: &tack_ai::ToolResultMessage) -> usize {
    result
        .content
        .iter()
        .map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => text.len(),
            _ => 0,
        })
        .sum()
}

fn tool_result_full_text(result: &tack_ai::ToolResultMessage) -> String {
    result
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Index (in tool-result order) below which a result counts as "old":
/// the most recent `keep_recent` results are never rewritten.
fn old_result_cutoff(messages: &[AgentMessage], keep_recent: usize) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::ToolResult(_)))
        .count()
        .saturating_sub(keep_recent)
}

/// Total bytes microcompaction could reclaim right now (qualifying rule:
/// a tool result older than `keep_recent` with over `max_chars` of text).
fn microcompact_savings(messages: &[AgentMessage], settings: &MicrocompactSettings) -> usize {
    let cutoff = old_result_cutoff(messages, settings.keep_recent);
    let mut seen = 0usize;
    let mut savings = 0usize;
    for message in messages {
        let AgentMessage::ToolResult(result) = message else {
            continue;
        };
        seen += 1;
        if seen > cutoff {
            continue;
        }
        let len = tool_result_text_len(result);
        if len > settings.max_chars {
            savings += len;
        }
    }
    savings
}

/// One pass over the history collecting everything
/// `history_optimize_needed` needs from tool results: reclaimable
/// microcompaction savings and whether ANY result exceeds the hard cap.
/// (Previously this was two full traversals; duplicate-read masking keeps
/// its own pass since it needs assistant messages too.)
fn scan_tool_results(messages: &[AgentMessage], settings: &HistorySettings) -> (usize, bool) {
    let cutoff = old_result_cutoff(messages, settings.micro.keep_recent);
    let mut seen = 0usize;
    let mut savings = 0usize;
    let mut over_cap = false;
    for message in messages {
        let AgentMessage::ToolResult(result) = message else {
            continue;
        };
        seen += 1;
        let len = tool_result_text_len(result);
        if settings.tool_result_max_chars > 0 && len > settings.tool_result_max_chars {
            over_cap = true;
        }
        if seen <= cutoff && len > settings.micro.max_chars {
            savings += len;
        }
    }
    (savings, over_cap)
}

/// Read-only pre-scan for `history_optimize` so `transform_context`
/// stays zero-copy when there is nothing to do.
pub fn history_optimize_needed(messages: &[AgentMessage], settings: &HistorySettings) -> bool {
    let (savings, over_cap) = scan_tool_results(messages, settings);
    if over_cap || savings >= settings.micro.min_savings_chars {
        return true;
    }
    settings.mask_duplicate_reads
        && !duplicate_read_masks(messages, settings.micro.keep_recent).is_empty()
}

/// Spill-dir retention: at most this many most-recent spill files survive
/// (microcompaction's `<id>.log` + the hard cap's `<id>.full.log`). Spill
/// files are write-only context overflow; without a cap the directory grew
/// forever across sessions.
const SPILL_KEEP_FILES: usize = 100;

/// Delete the oldest spill files beyond SPILL_KEEP_FILES (by modification
/// time). Best effort: pruning failure must never block history rewriting.
fn prune_spill_dir(spill_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(spill_dir) else {
        return; // no dir yet (or unreadable): nothing to prune
    };
    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .map(|p| {
            let mtime = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            (p, mtime)
        })
        .collect();
    if files.len() <= SPILL_KEEP_FILES {
        return;
    }
    // Oldest first; delete everything beyond the retention window.
    files.sort_by_key(|(_, mtime)| *mtime);
    for (path, _) in &files[..files.len() - SPILL_KEEP_FILES] {
        if let Err(e) = std::fs::remove_file(path) {
            tracing::warn!("microcompact: cannot prune spill {}: {e}", path.display());
        }
    }
}

/// Apply every enabled history optimization in one pass over an owned
/// copy. Returns how many tool results were rewritten.
pub fn history_optimize(
    messages: &mut [AgentMessage],
    settings: &HistorySettings,
    spill_dir: &std::path::Path,
) -> usize {
    let mut changed = 0;
    // Retention sweep before writing new spills: the dir is append-only
    // otherwise (F21). Best effort — failures only warn.
    prune_spill_dir(spill_dir);
    // Fresh-result hard cap first: nothing monstrous enters the LLM
    // context even in the most recent turns.
    if settings.tool_result_max_chars > 0 {
        changed += cap_oversized_results(messages, settings.tool_result_max_chars, spill_dir);
    }
    if microcompact_savings(messages, &settings.micro) >= settings.micro.min_savings_chars {
        changed += microcompact_messages(messages, &settings.micro, spill_dir);
    }
    if settings.mask_duplicate_reads {
        changed += mask_duplicate_read_results(messages, settings.micro.keep_recent);
    }
    changed
}

/// In-place shrink of old oversized tool results. Returns how many were
/// compacted. Runs on the LLM-bound copy only — the session file keeps the
/// full content.
pub fn microcompact_messages(
    messages: &mut [AgentMessage],
    settings: &MicrocompactSettings,
    spill_dir: &std::path::Path,
) -> usize {
    let total_results = messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::ToolResult(_)))
        .count();
    let cutoff = total_results.saturating_sub(settings.keep_recent);
    let mut seen = 0usize;
    let mut compacted = 0usize;
    for message in messages.iter_mut() {
        let AgentMessage::ToolResult(result) = message else {
            continue;
        };
        seen += 1;
        if seen > cutoff {
            continue;
        }
        let total_bytes = tool_result_text_len(result);
        if total_bytes <= settings.max_chars {
            continue;
        }
        let full = tool_result_full_text(result);
        let spill = spill_dir.join(format!("{}.log", spill_file_stem(&result.tool_call_id)));
        let _ = std::fs::create_dir_all(spill_dir);
        if let Err(e) = std::fs::write(&spill, &full) {
            tracing::warn!("microcompact: cannot spill {}: {e}", spill.display());
            continue;
        }
        result.content = vec![tack_ai::InputContentBlock::text(format!(
            "[output microcompacted: {total_bytes} bytes from `{}`. \
             Full output: {} — read it with the read tool if needed]",
            result.tool_name,
            spill.display()
        ))];
        compacted += 1;
    }
    compacted
}

/// Duplicate-read detection (context engineering: observation masking).
/// Returns `(tool_call_id, path)` pairs whose read result may be masked
/// because the SAME file was re-read later with identical content — the
/// full text is still in context at the later position, so masking
/// loses nothing. Only results older than the `keep_recent` window are
/// maskable (the fresh tail is never rewritten: prompt-cache friendly).
///
/// Hot-path discipline: this runs as a read-only pre-scan before EVERY
/// LLM call, so it borrows text blocks instead of cloning result texts;
/// allocation only happens for pairs that actually mask (rare).
fn duplicate_read_masks<'m>(
    messages: &'m [AgentMessage],
    keep_recent: usize,
) -> Vec<(String, String)> {
    // call id → read path, from the assistant tool calls.
    let mut call_paths: std::collections::HashMap<&'m str, &'m str> =
        std::collections::HashMap::new();
    for message in messages {
        let AgentMessage::Assistant(assistant) = message else {
            continue;
        };
        for block in &assistant.content {
            if let tack_ai::ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } = block
                && name == "read"
                && let Some(path) = arguments.get("path").and_then(|v| v.as_str())
            {
                call_paths.insert(id.as_str(), path);
            }
        }
    }
    if call_paths.is_empty() {
        return Vec::new();
    }
    let cutoff = old_result_cutoff(messages, keep_recent);
    // path → (result index, call id, borrowed text blocks) of the latest
    // full copy. Block-wise comparison is stricter than comparing joined
    // text (different block splits never mask) and always lossless.
    let mut last_read: std::collections::HashMap<&'m str, (usize, &'m str, Vec<&'m str>)> =
        std::collections::HashMap::new();
    let mut masks = Vec::new();
    let mut seen = 0usize;
    for message in messages {
        let AgentMessage::ToolResult(result) = message else {
            continue;
        };
        let index = seen;
        seen += 1;
        if result.tool_name != "read" {
            continue;
        }
        let Some(path) = call_paths.get(result.tool_call_id.as_str()) else {
            continue;
        };
        // Results with non-text blocks (e.g. images) are never masked.
        let texts: Option<Vec<&str>> = result
            .content
            .iter()
            .map(|b| match b {
                tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let Some(texts) = texts else {
            continue;
        };
        if let Some((prev_index, prev_id, prev_texts)) = last_read.get(path)
            && *prev_index < cutoff
            && prev_texts == &texts
        {
            masks.push(((*prev_id).to_string(), (*path).to_string()));
        }
        last_read.insert(path, (index, result.tool_call_id.as_str(), texts));
    }
    masks
}

fn mask_duplicate_read_results(messages: &mut [AgentMessage], keep_recent: usize) -> usize {
    let masks = duplicate_read_masks(messages, keep_recent);
    if masks.is_empty() {
        return 0;
    }
    let by_id: std::collections::HashMap<String, String> = masks.into_iter().collect();
    let mut masked = 0;
    for message in messages.iter_mut() {
        let AgentMessage::ToolResult(result) = message else {
            continue;
        };
        let Some(path) = by_id.get(&result.tool_call_id) else {
            continue;
        };
        result.content = vec![tack_ai::InputContentBlock::text(format!(
            "[duplicate read masked: `{path}` was re-read later in this conversation with \
             identical content — refer to the most recent read of this file above]"
        ))];
        masked += 1;
    }
    masked
}

/// Hard cap on ANY tool result, fresh ones included (microcompaction
/// only handles old results). The full text spills to `<id>.full.log`
/// — a different name than microcompaction's `<id>.log`, so a later
/// microcompaction pass can never overwrite the pre-cap original.
fn cap_oversized_results(
    messages: &mut [AgentMessage],
    max_chars: usize,
    spill_dir: &std::path::Path,
) -> usize {
    let mut capped = 0;
    for message in messages.iter_mut() {
        let AgentMessage::ToolResult(result) = message else {
            continue;
        };
        let total_bytes = tool_result_text_len(result);
        if total_bytes <= max_chars {
            continue;
        }
        let full = tool_result_full_text(result);
        let spill = spill_dir.join(format!(
            "{}.full.log",
            spill_file_stem(&result.tool_call_id)
        ));
        let _ = std::fs::create_dir_all(spill_dir);
        if let Err(e) = std::fs::write(&spill, &full) {
            tracing::warn!("tool-result cap: cannot spill {}: {e}", spill.display());
            continue;
        }
        let head: String = first_bytes(&full, max_chars).to_string();
        result.content = vec![tack_ai::InputContentBlock::text(format!(
            "{head}\n\n[output capped at {max_chars} of {total_bytes} bytes by tack. \
             Full output: {} — read it with the read tool if needed]",
            spill.display()
        ))];
        capped += 1;
    }
    capped
}

/// The longest prefix of `s` that fits in `max` bytes without splitting a
/// multi-byte char.
fn first_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// tool_call_id is model-controlled text: reduce it to a safe file-name
/// stem so the spill file can never escape the spill dir.
fn spill_file_stem(tool_call_id: &str) -> String {
    let sanitized: String = tool_call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = sanitized.trim_start_matches('.');
    if stem.is_empty() {
        "spill".to_string()
    } else {
        stem.chars().take(120).collect()
    }
}

impl std::fmt::Debug for SessionHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHooks").finish_non_exhaustive()
    }
}

/// Latest compaction summary in the context, if any.
fn last_compaction_summary(messages: &[AgentMessage]) -> Option<&str> {
    messages.iter().rev().find_map(|m| match m {
        AgentMessage::CompactionSummary(c) => Some(c.summary.as_str()),
        _ => None,
    })
}

/// Sessions with an auto-compaction currently in flight, keyed by the
/// session Arc's pointer identity. `transform_inner` deliberately holds the
/// session lock only for short snapshots (never across the network
/// compaction), so this is the re-entrancy guard: a concurrent
/// `transform_context` on the SAME session while a compaction is running
/// skips compacting — the next turn's estimate triggers it again.
static COMPACTING_SESSIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// RAII guard removing the session key on drop (covers every exit path,
/// including early returns and panics).
struct CompactionInFlight(usize);

impl CompactionInFlight {
    fn try_acquire(session: &Arc<Mutex<SessionManager>>) -> Option<Self> {
        let key = Arc::as_ptr(session) as usize;
        let mut set = COMPACTING_SESSIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if set.contains(&key) {
            None
        } else {
            set.insert(key);
            Some(CompactionInFlight(key))
        }
    }
}

impl Drop for CompactionInFlight {
    fn drop(&mut self) {
        let mut set = COMPACTING_SESSIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set.remove(&self.0);
    }
}

impl SessionHooks {
    async fn transform_inner(&self, messages: &[AgentMessage]) -> Option<Vec<AgentMessage>> {
        // History optimization first (cheap): cap/shrink/mask tool results
        // regardless of where we are relative to the compaction threshold.
        // COW: pre-scan read-only and only materialize a copy when some
        // tool result actually qualifies, so the common path stays
        // zero-copy.
        let optimized: Option<Vec<AgentMessage>> = match &self.history {
            Some((settings, spill_dir)) if history_optimize_needed(messages, settings) => {
                let mut owned = messages.to_vec();
                history_optimize(&mut owned, settings, spill_dir);
                Some(owned)
            }
            _ => None,
        };
        let messages: &[AgentMessage] = optimized.as_deref().unwrap_or(messages);
        let estimate = estimate_context_tokens(messages);
        if !should_compact(
            estimate.tokens,
            self.model.context_window as u64,
            &self.settings,
        ) {
            return optimized;
        }

        // Re-entrancy guard: the session lock is NOT held across the
        // compaction below, so a second transform on this session must not
        // start a parallel compaction against the same baseline. Return the
        // already-computed history optimization — `?` would drop it and
        // send un-capped tool results to the provider precisely when the
        // context is fullest.
        let Some(_in_flight) = CompactionInFlight::try_acquire(&self.session) else {
            return optimized;
        };

        self.run_pre_compact_hooks().await;
        // Compaction succeeded → the rebuilt context; any failure → fall
        // back to the history-optimized messages.
        match self.run_compaction().await {
            Some(rebuilt) => Some(rebuilt),
            None => optimized,
        }
    }

    /// PreCompact hooks (fire-and-forget; verdicts are advisory here).
    async fn run_pre_compact_hooks(&self) {
        if !self.pre_compact.is_empty() {
            let payload = serde_json::json!({
                "session_id": self.hook_session_id,
                "transcript_path": serde_json::Value::Null,
                "cwd": self.hook_engine.cwd(),
                "hook_event_name": "PreCompact",
                "model": self.model.id,
                "trigger": "auto",
            });
            self.hook_engine
                .run(&self.pre_compact, None, &payload)
                .await;
        }
    }

    /// The compaction core shared by threshold (transform_inner) and
    /// overflow (compact_for_overflow) triggers: snapshot → prepare →
    /// summarize → persist → rebuild the post-compaction context.
    /// Caller holds the `CompactionInFlight` guard. `None` on any failure
    /// (nothing to compact, auth/compaction/persist error, lineage moved).
    async fn run_compaction(&self) -> Option<Vec<AgentMessage>> {
        // Snapshot under the lock, then RELEASE it: the compaction is a
        // network call and must not block every other session user (token
        // totals, UI stats, budget hooks) for its duration. The snapshot
        // stays valid because the session log is append-only: entry ids
        // referenced by the preparation cannot move or vanish, and a
        // message appended mid-compaction simply lands after the
        // compaction entry (same as a message arriving one turn later).
        let (path, session_id) = {
            let session = self.session.lock().await;
            (
                session.build_session_path(),
                session.session_id().to_string(),
            )
        };
        // Lineage marker: the snapshot's leaf. New appends keep it
        // reachable; a fork/resume/undo abandons it.
        let snapshot_leaf = path.last().map(|e| e.id().to_string());
        let preparation = prepare_compaction(&path, &self.settings)?;
        let tokens_before = preparation.tokens_before;
        tracing::info!(tokens_before, "auto-compacting context");

        let auth = match self.auth.resolve().await {
            Ok(auth) => auth,
            Err(e) => {
                tracing::warn!("compaction auth resolution failed: {e}");
                return None;
            }
        };
        let result = match tack_session::compact(
            &preparation,
            &self.model,
            &self.provider,
            &auth,
            None,
            self.reasoning,
            Some(session_id.as_str()),
            &self.cancel,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("compaction failed: {e}");
                return None;
            }
        };

        // Materialize the retained tail (kept messages after the cut point)
        // so the compaction entry is a self-contained checkpoint.
        let kept_entries = &path[path
            .iter()
            .position(|e| e.id() == result.first_kept_entry_id)
            .unwrap_or(path.len())..];
        let retained_tail: Vec<AgentMessage> =
            tack_session::retained_tail_from_kept_entries(kept_entries);

        // Re-acquire the lock briefly to persist the compaction and build
        // the post-compaction context from the CURRENT session state.
        let mut session = self.session.lock().await;
        // The lock was not held across the network call: if the session
        // lineage moved while compacting (fork/resume/undo — the snapshot
        // leaf is no longer reachable), persisting here would inject the
        // abandoned branch's summary + retained tail into the NEW branch.
        // Skip; the abandoned branch's compaction is useless anyway, and
        // the new branch re-triggers compaction on its own estimate.
        if snapshot_leaf
            .as_ref()
            .is_some_and(|leaf| !session.build_session_path().iter().any(|e| e.id() == *leaf))
        {
            tracing::warn!("session lineage changed during auto-compaction; discarding the result");
            return None;
        }
        if let Err(e) = session.append_compaction(
            &result.summary,
            Some(result.first_kept_entry_id.clone()),
            result.tokens_before,
            Some(retained_tail),
            Some(result.details.clone()),
            Some(result.usage.clone()),
        ) {
            tracing::warn!("failed to persist compaction: {e}");
            return None;
        }
        let compacted_context =
            build_session_context(&session.entries(), session.leaf_id()).messages;
        drop(session);

        if let Some(tx) = &self.on_compaction {
            let _ = tx.send((result.summary.clone(), tokens_before));
        }

        // PostCompact hooks (fire-and-forget).
        if !self.post_compact.is_empty() {
            let payload = serde_json::json!({
                "session_id": self.hook_session_id,
                "transcript_path": serde_json::Value::Null,
                "cwd": self.hook_engine.cwd(),
                "hook_event_name": "PostCompact",
                "model": self.model.id,
                "trigger": "auto",
                "tokens_before": tokens_before,
            });
            self.hook_engine
                .run(&self.post_compact, None, &payload)
                .await;
        }

        Some(compacted_context)
    }
}

#[async_trait]
impl AgentHooks for SessionHooks {
    async fn transform_context(&self, messages: &[AgentMessage]) -> Option<Vec<AgentMessage>> {
        let base = self.transform_inner(messages).await;
        if !self.settings.goal_recitation {
            return base;
        }
        // Goal recitation: restate the latest compaction summary's
        // Goal/Next Steps as a trailing user message so the objective
        // stays in the recency-biased attention zone. Append-only
        // (prompt-cache friendly) and LLM-bound only — the session file
        // never sees the synthetic message.
        match base {
            Some(mut rewritten) => {
                if let Some(summary) = last_compaction_summary(&rewritten)
                    && let Some(recitation) = tack_session::goal_recitation_message(summary)
                {
                    rewritten.push(recitation);
                }
                Some(rewritten)
            }
            None => {
                // Stay zero-copy unless a recitation actually applies.
                let summary = last_compaction_summary(messages)?;
                let recitation = tack_session::goal_recitation_message(summary)?;
                let mut owned = messages.to_vec();
                owned.push(recitation);
                Some(owned)
            }
        }
    }

    /// Overflow compact-and-retry (upstream agent-session `_checkCompaction`
    /// case 1): the provider's overflow error is the trigger, so the
    /// token-threshold gate is skipped. Compacts and returns the rebuilt
    /// context; `None` when compaction is disabled, already in flight, or
    /// failed — the agent loop then ends the turn with the original error.
    async fn compact_for_overflow(&self) -> Option<Vec<AgentMessage>> {
        if !self.settings.enabled {
            return None;
        }
        let _in_flight = CompactionInFlight::try_acquire(&self.session)?;
        self.run_pre_compact_hooks().await;
        self.run_compaction().await
    }
}

// ---------------------------------------------------------------------
// Token budget enforcement
// ---------------------------------------------------------------------

/// Hard budget actions: `pause` stops the run after the current turn,
/// `downgrade` switches to a cheaper model for subsequent turns. (`warn`
/// stays a UI-side concern and isn't handled here.)
pub struct BudgetHooks {
    pub session: Arc<Mutex<SessionManager>>,
    pub budget: u64,
    /// "pause" | "downgrade"
    pub action: String,
    pub downgrade_model: Option<Model>,
    /// One-shot flags (warned/paused/downgraded already fired).
    pub fired: std::sync::Arc<std::sync::Mutex<bool>>,
    /// UI notification (message); None in non-interactive modes.
    pub on_trigger: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl std::fmt::Debug for BudgetHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BudgetHooks").finish_non_exhaustive()
    }
}

#[async_trait]
impl AgentHooks for BudgetHooks {
    async fn prepare_next_turn(
        &self,
        _ctx: &tack_agent_core::TurnContext<'_>,
    ) -> Option<tack_agent_core::NextTurnUpdate> {
        if self.action != "downgrade" {
            return None;
        }
        let Some(model) = &self.downgrade_model else {
            return None;
        };
        let totals = self.session.lock().await.session_totals();
        if totals.total_tokens <= self.budget {
            return None;
        }
        {
            let mut fired = self.fired.lock().expect("budget mutex");
            if *fired {
                return None;
            }
            *fired = true;
        }
        if let Some(tx) = &self.on_trigger {
            let _ = tx.send(crate::i18n::trf(
                "notice.budget_downgrade",
                &[
                    ("used", &totals.total_tokens.to_string()),
                    ("budget", &self.budget.to_string()),
                    ("model", &format!("{}/{}", model.provider, model.id)),
                ],
            ));
        }
        Some(tack_agent_core::NextTurnUpdate {
            model: Some(model.clone()),
            thinking_level: None,
        })
    }

    async fn should_stop_after_turn(&self, _ctx: &tack_agent_core::TurnContext<'_>) -> bool {
        if self.action != "pause" {
            return false;
        }
        let totals = self.session.lock().await.session_totals();
        if totals.total_tokens <= self.budget {
            return false;
        }
        {
            let mut fired = self.fired.lock().expect("budget mutex");
            if *fired {
                return false;
            }
            *fired = true;
        }
        if let Some(tx) = &self.on_trigger {
            let _ = tx.send(crate::i18n::trf(
                "notice.budget_pause",
                &[
                    ("used", &totals.total_tokens.to_string()),
                    ("budget", &self.budget.to_string()),
                ],
            ));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn assistant_with_tokens(tokens: u64) -> tack_ai::AssistantMessage {
        let model = crate::model::resolve_model("anthropic", Some("k3"), std::path::Path::new("."))
            .unwrap();
        let mut message = tack_ai::AssistantMessage::pending(&model);
        message.stop_reason = tack_ai::StopReason::Stop;
        message.usage.input = tokens;
        message.usage.output = 0;
        message.usage.total_tokens = tokens;
        message
    }

    #[tokio::test]
    // The lock guard is deliberately held across the awaits (whole-test
    // mutual exclusion); the other lock holder is a sync test that never
    // awaits while holding it, so no deadlock. Statement-level allow does
    // not suppress this lint (emitted against the fn), hence fn-level.
    #[allow(clippy::await_holding_lock)]
    async fn pause_action_stops_once_over_budget() {
        // The note is localized via the process-global language; serialize
        // against i18n's global-language test and pin En so the assertion
        // below is deterministic.
        let _lang_guard = crate::i18n::TEST_LANG_LOCK.lock().unwrap();
        crate::i18n::set_current(crate::i18n::Lang::En);
        let mut session = SessionManager::in_memory(std::path::Path::new("."));
        session
            .append_message(AgentMessage::Assistant(assistant_with_tokens(10_000)))
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hooks = BudgetHooks {
            session: Arc::new(Mutex::new(session)),
            budget: 5_000,
            action: "pause".into(),
            downgrade_model: None,
            fired: std::sync::Arc::new(std::sync::Mutex::new(false)),
            on_trigger: Some(tx),
        };
        let turn = tack_agent_core::TurnContext {
            message: &assistant_with_tokens(0),
            tool_results: &[],
            new_messages: &[],
        };
        assert!(
            hooks.should_stop_after_turn(&turn).await,
            "over budget → stop"
        );
        assert!(
            !hooks.should_stop_after_turn(&turn).await,
            "one-shot: don't stop twice"
        );
        let note = rx.recv().await.unwrap();
        assert!(note.contains("paused"), "{note}");
    }

    #[tokio::test]
    async fn pause_action_ignores_under_budget() {
        let session = SessionManager::in_memory(std::path::Path::new("."));
        let hooks = BudgetHooks {
            session: Arc::new(Mutex::new(session)),
            budget: 5_000,
            action: "pause".into(),
            downgrade_model: None,
            fired: std::sync::Arc::new(std::sync::Mutex::new(false)),
            on_trigger: None,
        };
        let turn = tack_agent_core::TurnContext {
            message: &assistant_with_tokens(0),
            tool_results: &[],
            new_messages: &[],
        };
        assert!(!hooks.should_stop_after_turn(&turn).await);
    }

    #[test]
    fn microcompact_shrinks_old_oversized_results() {
        let big = "x".repeat(30_000);
        let mk = |id: &str, content: &str| {
            AgentMessage::ToolResult(tack_ai::ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: "bash".into(),
                content: vec![tack_ai::InputContentBlock::text(content)],
                details: None,
                usage: None,
                is_error: false,
                timestamp: 0,
            })
        };
        let mut messages = vec![
            mk("old-big", &big),   // compacted
            mk("old-small", "ok"), // under threshold: untouched
            mk("r1", &big),        // recent: kept full
            mk("r2", &big),
            mk("r3", &big),
        ];
        let tmp = tempfile::tempdir().unwrap();
        let settings = MicrocompactSettings {
            max_chars: 1_000,
            keep_recent: 3,
            min_savings_chars: 0,
        };
        let compacted = microcompact_messages(&mut messages, &settings, tmp.path());
        assert_eq!(compacted, 1);

        let AgentMessage::ToolResult(old) = &messages[0] else {
            panic!()
        };
        let tack_ai::InputContentBlock::Text { text, .. } = &old.content[0] else {
            panic!()
        };
        assert!(text.contains("microcompacted"), "{text}");
        assert!(text.contains("old-big.log"), "{text}");
        // Full content was spilled and is recoverable.
        let spilled = std::fs::read_to_string(tmp.path().join("old-big.log")).unwrap();
        assert_eq!(spilled.len(), 30_000);
        // Recent results untouched.
        let AgentMessage::ToolResult(recent) = &messages[4] else {
            panic!()
        };
        let tack_ai::InputContentBlock::Text {
            text: recent_text, ..
        } = &recent.content[0]
        else {
            panic!()
        };
        assert_eq!(recent_text.len(), 30_000);
    }

    #[test]
    fn spill_dir_pruned_to_retention_window() {
        // F21: the spill dir is append-only overflow; files beyond the
        // retention window (oldest by mtime) are pruned on the next write.
        let tmp = tempfile::tempdir().unwrap();
        let spill_dir = tmp.path().join("spill");
        std::fs::create_dir_all(&spill_dir).unwrap();
        for i in 0..(super::SPILL_KEEP_FILES + 5) {
            std::fs::write(spill_dir.join(format!("s{i:04}.log")), "x").unwrap();
        }
        // A non-log file is never pruned.
        std::fs::write(spill_dir.join("keep.txt"), "x").unwrap();
        super::prune_spill_dir(&spill_dir);
        let logs = std::fs::read_dir(&spill_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
            .count();
        assert_eq!(logs, super::SPILL_KEEP_FILES);
        assert!(spill_dir.join("keep.txt").exists());
    }

    #[test]
    fn microcompact_spill_path_is_sanitized() {
        // tool_call_id is model-controlled: a path-traversal id must not
        // write the spill file outside the spill dir.
        let tmp = tempfile::tempdir().unwrap();
        let spill_dir = tmp.path().join("spill");
        let big = "y".repeat(5_000);
        let mut messages = vec![AgentMessage::ToolResult(tack_ai::ToolResultMessage {
            tool_call_id: "../escaped".into(),
            tool_name: "bash".into(),
            content: vec![tack_ai::InputContentBlock::text(&big)],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        })];
        let settings = MicrocompactSettings {
            max_chars: 100,
            keep_recent: 0,
            min_savings_chars: 0,
        };
        let compacted = microcompact_messages(&mut messages, &settings, &spill_dir);
        assert_eq!(compacted, 1);
        assert!(
            !tmp.path().join("escaped.log").exists(),
            "spill file escaped the spill dir"
        );
        let spilled: Vec<_> = std::fs::read_dir(&spill_dir).unwrap().collect();
        assert_eq!(spilled.len(), 1, "spill file must stay inside spill dir");
    }

    // ------------------------------------------------------------
    // History optimization (min-savings gate, duplicate-read
    // masking, fresh-result cap) and goal recitation.
    // ------------------------------------------------------------

    fn tool_result(id: &str, tool: &str, content: &str) -> AgentMessage {
        AgentMessage::ToolResult(tack_ai::ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: tool.into(),
            content: vec![tack_ai::InputContentBlock::text(content)],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        })
    }

    fn read_call(id: &str, path: &str) -> AgentMessage {
        let model = crate::model::resolve_model("anthropic", Some("k3"), std::path::Path::new("."))
            .unwrap();
        let mut message = tack_ai::AssistantMessage::pending(&model);
        message.content.push(tack_ai::ContentBlock::ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": path }),
            thought_signature: None,
            namespace: None,
        });
        AgentMessage::Assistant(message)
    }

    fn result_text(message: &AgentMessage) -> String {
        let AgentMessage::ToolResult(result) = message else {
            panic!("expected tool result")
        };
        let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
            panic!("expected text block")
        };
        text.clone()
    }

    fn history_settings() -> HistorySettings {
        HistorySettings {
            micro: MicrocompactSettings {
                max_chars: 1_000,
                keep_recent: 1,
                min_savings_chars: 8_000,
            },
            mask_duplicate_reads: true,
            tool_result_max_chars: 0,
        }
    }

    #[test]
    fn min_savings_gate_blocks_cheap_rewrites() {
        // One old 5k result: below the 8k gate, so the rewrite (and the
        // prompt-cache invalidation it causes) must not happen.
        let big = "x".repeat(5_000);
        let messages = vec![
            tool_result("old", "bash", &big),
            tool_result("recent", "bash", "ok"),
        ];
        let settings = history_settings();
        assert!(!history_optimize_needed(&messages, &settings));
        // Lower the gate below the savings and the pass activates.
        let mut eager = settings;
        eager.micro.min_savings_chars = 1_000;
        assert!(history_optimize_needed(&messages, &eager));
    }

    #[test]
    fn duplicate_reads_masked_when_identical() {
        let content = "file contents".repeat(10);
        let mut messages = vec![
            read_call("r1", "/a.rs"),
            tool_result("r1", "read", &content),
            read_call("r2", "/a.rs"),
            tool_result("r2", "read", &content),
            read_call("r3", "/a.rs"),
            tool_result("r3", "read", &content),
        ];
        let tmp = tempfile::tempdir().unwrap();
        let settings = history_settings(); // keep_recent 1 → only r3 protected
        assert!(history_optimize_needed(&messages, &settings));
        let changed = history_optimize(&mut messages, &settings, tmp.path());
        assert_eq!(changed, 2, "r1 and r2 both have a later identical copy");
        assert!(result_text(&messages[1]).contains("duplicate read masked"));
        assert!(result_text(&messages[1]).contains("/a.rs"));
        assert!(result_text(&messages[3]).contains("duplicate read masked"));
        assert_eq!(result_text(&messages[5]), content, "latest copy stays full");
    }

    #[test]
    fn duplicate_reads_kept_when_content_differs() {
        let messages = vec![
            read_call("r1", "/a.rs"),
            tool_result("r1", "read", "version one"),
            read_call("r2", "/a.rs"),
            tool_result("r2", "read", "version two"),
        ];
        let settings = history_settings();
        assert!(!history_optimize_needed(&messages, &settings));
    }

    #[test]
    fn duplicate_reads_recent_window_protected() {
        // keep_recent covers every result: nothing is old enough to mask.
        let content = "same".repeat(10);
        let messages = vec![
            read_call("r1", "/a.rs"),
            tool_result("r1", "read", &content),
            read_call("r2", "/a.rs"),
            tool_result("r2", "read", &content),
        ];
        let mut settings = history_settings();
        settings.micro.keep_recent = 5;
        assert!(!history_optimize_needed(&messages, &settings));
    }

    #[test]
    fn fresh_result_capped_and_spilled() {
        // A monster result in the PROTECTED recent window: microcompaction
        // must not touch it, but the hard cap still applies (to a distinct
        // spill file, so a later microcompaction can't overwrite it).
        let big = "y".repeat(5_000);
        let mut messages = vec![tool_result("fresh-big", "bash", &big)];
        let tmp = tempfile::tempdir().unwrap();
        let settings = HistorySettings {
            micro: MicrocompactSettings {
                max_chars: 1_000,
                keep_recent: 3,
                min_savings_chars: 0,
            },
            mask_duplicate_reads: false,
            tool_result_max_chars: 2_000,
        };
        assert!(history_optimize_needed(&messages, &settings));
        let changed = history_optimize(&mut messages, &settings, tmp.path());
        assert_eq!(changed, 1);
        let text = result_text(&messages[0]);
        assert!(text.contains("capped at 2000 of 5000 bytes"), "{text}");
        assert!(text.contains("fresh-big.full.log"), "{text}");
        let spilled = std::fs::read_to_string(tmp.path().join("fresh-big.full.log")).unwrap();
        assert_eq!(spilled.len(), 5_000);
        assert!(
            !tmp.path().join("fresh-big.log").exists(),
            "microcompaction must not touch the protected recent result"
        );
    }

    #[derive(Debug)]
    struct PanicProvider;

    impl tack_ai::Provider for PanicProvider {
        fn stream(
            &self,
            _model: &tack_ai::Model,
            _context: &tack_ai::Context,
            _options: tack_ai::StreamOptions,
        ) -> tack_ai::AssistantMessageEventStream {
            panic!("not used by these tests")
        }
    }

    fn test_session_hooks(goal_recitation: bool) -> SessionHooks {
        SessionHooks {
            session: Arc::new(Mutex::new(SessionManager::in_memory(std::path::Path::new(
                ".",
            )))),
            model: crate::model::resolve_model("anthropic", Some("k3"), std::path::Path::new("."))
                .unwrap(),
            provider: Arc::new(PanicProvider),
            auth: Arc::new(tack_ai::oauth::StaticAuth::from(None)),
            reasoning: None,
            settings: tack_session::CompactionSettings {
                goal_recitation,
                ..tack_session::DEFAULT_COMPACTION_SETTINGS
            },
            cancel: CancellationToken::new(),
            on_compaction: None,
            history: None,
            hook_engine: crate::shell_hooks::HookEngine::new(
                None,
                std::path::Path::new(".").to_path_buf(),
            ),
            pre_compact: Vec::new(),
            post_compact: Vec::new(),
            hook_session_id: "test".into(),
        }
    }

    fn compaction_summary_message() -> AgentMessage {
        AgentMessage::CompactionSummary(tack_agent_core::CompactionSummaryMessage {
            summary:
                "## Goal\nShip context engineering\n\n## Next Steps\n1. write tests\n2. commit"
                    .into(),
            tokens_before: 1_000,
            timestamp: 0,
        })
    }

    #[tokio::test]
    async fn recitation_appended_after_compaction_summary() {
        let hooks = test_session_hooks(true);
        let messages = vec![compaction_summary_message(), AgentMessage::user("continue")];
        let out = AgentHooks::transform_context(&hooks, &messages)
            .await
            .expect("recitation should materialize the context");
        assert_eq!(out.len(), 3);
        let AgentMessage::User(recitation) = out.last().unwrap() else {
            panic!("recitation must be a trailing user message")
        };
        let tack_ai::UserContent::Text(text) = &recitation.content else {
            panic!("text content")
        };
        assert!(text.contains("## Goal\nShip context engineering"), "{text}");
        assert!(text.contains("1. write tests"), "{text}");
        // Original messages are untouched (the copy is LLM-bound only).
        assert_eq!(messages.len(), 2);
    }

    #[tokio::test]
    async fn recitation_disabled_stays_zero_copy() {
        let hooks = test_session_hooks(false);
        let messages = vec![compaction_summary_message(), AgentMessage::user("continue")];
        assert!(
            AgentHooks::transform_context(&hooks, &messages)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn no_recitation_without_summary() {
        let hooks = test_session_hooks(true);
        let messages = vec![AgentMessage::user("hello")];
        assert!(
            AgentHooks::transform_context(&hooks, &messages)
                .await
                .is_none()
        );
    }
}
