//! The agent loop. Direct port of `runLoop` in
//! `packages/agent/src/agent-loop.ts`: outer follow-up loop + inner
//! tool-call/steering loop, with pi's exact event ordering and error
//! semantics.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use serde_json::Value;
use tack_ai::{
    AssistantMessage, CacheRetention, Context, EventStream, Model, Provider, StopReason,
    StreamOptions, SystemMessage, ThinkingLevel, ToolResultMessage, now_millis,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::event::AgentEvent;
use crate::hooks::{
    AfterToolCallContext, AgentHooks, BeforeToolCallContext, BeforeToolCallOutcome, TurnContext,
};
use crate::message::AgentMessage;
use crate::tool::{AgentTool, AgentToolResult, ToolExecutionMode, tool_definition};

/// tack's system-prompt section name in transcript system messages
/// (upstream #9548 stores structured sections: preamble/tools/rules/...).
/// tack renders one monolithic prompt and stores it under this name so
/// prompt *replacement* replays cleanly (system-message `content` appends
/// across messages; `sections` replace by name). Foreign sections — e.g. an
/// upstream pi session resumed in tack — are nulled by the next update.
pub const SYSTEM_PROMPT_SECTION: &str = "system-prompt";

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub provider: Arc<dyn Provider>,
    pub hooks: Arc<dyn AgentHooks>,
    pub tool_execution: ToolExecutionMode,
    pub reasoning: Option<ThinkingLevel>,
    /// Request-time auth source; resolved before every LLM call so OAuth
    /// tokens refresh proactively in long-running processes.
    pub auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    /// Session identifier forwarded to providers that support caching.
    pub session_id: Option<String>,
    /// Prompt-cache retention hint (settings `cacheRetention`). None defers
    /// to the provider's own resolution: `TACK_CACHE_RETENTION` env, then
    /// short (5-minute) writes.
    pub cache_retention: Option<CacheRetention>,
    /// Ordered fallback chain: when the active model fails with a retryable
    /// error (429/overloaded/5xx/timeout) after provider-level retries are
    /// exhausted, the loop switches to the next entry and retries the turn.
    /// Entries are consumed as they are used.
    pub fallback_models: Vec<Model>,
    /// Deferred tools (client-side tool search): NOT in the context until a
    /// tool result names them via the tack-internal activation channel
    /// (set by tool_search); the loadout change is then recorded as a
    /// transcript system message.
    pub tool_pool: Vec<Arc<dyn AgentTool>>,
    /// Retry-scope cancellation: aborts an in-progress retry backoff without
    /// aborting the run (TS abortRetry). None = no separate retry scope.
    pub retry_cancel: Option<tokio_util::sync::CancellationToken>,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("model", &self.model.id)
            .field("tool_execution", &self.tool_execution)
            .field("reasoning", &self.reasoning)
            .finish_non_exhaustive()
    }
}

pub struct AgentContext {
    pub system_prompt: Option<String>,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

impl std::fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentContext")
            .field("messages", &self.messages.len())
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

/// Start an agent loop with new prompt messages. Events stream back; the
/// final result is the list of messages produced by this run.
pub fn agent_loop(
    prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    config: AgentLoopConfig,
    cancel: CancellationToken,
) -> EventStream<AgentEvent, Vec<AgentMessage>> {
    let (tx, result_tx, stream) = agent_channel();
    tokio::spawn(async move {
        // Per-run transcript state (upstream #9548): the prompt-section
        // diff and tool-loadout declarations become system messages at the
        // head of the run, recorded in the transcript like any message.
        let mut prompts = prompts;
        if let Some(update) = system_state_update(&context) {
            prompts.insert(0, update);
        }
        let prompts = declare_tool_changes(&context, prompts);
        let new_messages: Vec<AgentMessage> = prompts.clone();
        context.messages.extend(prompts.iter().cloned());

        emit(&tx, AgentEvent::AgentStart).await;
        emit(&tx, AgentEvent::TurnStart).await;
        for prompt in &prompts {
            emit(
                &tx,
                AgentEvent::MessageStart {
                    message: prompt.clone(),
                },
            )
            .await;
            emit(
                &tx,
                AgentEvent::MessageEnd {
                    message: prompt.clone(),
                },
            )
            .await;
        }

        let messages = run_loop(context, new_messages, config, cancel, &tx).await;
        finish(&tx, result_tx, messages).await;
    });
    stream
}

/// Continue from the current context without adding a new message (retry).
/// The last message must not be an assistant message.
pub fn agent_loop_continue(
    mut context: AgentContext,
    config: AgentLoopConfig,
    cancel: CancellationToken,
) -> Result<EventStream<AgentEvent, Vec<AgentMessage>>, String> {
    if context.messages.is_empty() {
        return Err("Cannot continue: no messages in context".to_string());
    }
    if matches!(context.messages.last(), Some(AgentMessage::Assistant(_))) {
        return Err("Cannot continue from message role: assistant".to_string());
    }

    let (tx, result_tx, stream) = agent_channel();
    tokio::spawn(async move {
        // Continue still declares transcript state changes (tool loadout or
        // prompt may have changed between runs).
        let mut initial: Vec<AgentMessage> = Vec::new();
        if let Some(update) = system_state_update(&context) {
            initial.push(update);
        }
        let initial = declare_tool_changes(&context, initial);
        context.messages.extend(initial.iter().cloned());
        emit(&tx, AgentEvent::AgentStart).await;
        emit(&tx, AgentEvent::TurnStart).await;
        for message in &initial {
            emit(
                &tx,
                AgentEvent::MessageStart {
                    message: message.clone(),
                },
            )
            .await;
            emit(
                &tx,
                AgentEvent::MessageEnd {
                    message: message.clone(),
                },
            )
            .await;
        }
        let messages = run_loop(context, initial, config, cancel, &tx).await;
        finish(&tx, result_tx, messages).await;
    });
    Ok(stream)
}

/// Sending half of the agent-event channel: a BOUNDED queue with
/// producer-side coalescing for overwritable streaming updates.
///
/// Why: consumers (TUI `handle_agent_event` awaits per event) can stall
/// while every `MessageUpdate`/`ToolExecutionUpdate` carries a full
/// partial-message clone. With an unbounded channel the backlog — and the
/// cloned partials inside it — grew without bound for the duration of a
/// long stream (quadratic-feeling memory). Now:
/// - Must-deliver events (MessageStart/End, Turn*, ToolExecutionStart/End,
///   AgentStart/End, ModelFallback) go through bounded `send().await`:
///   backpressure instead of growth, and they are never dropped.
/// - Overwritable updates (`MessageUpdate`, `ToolExecutionUpdate` — each a
///   cumulative snapshot superseded by the next one) never block the
///   agent: when the channel is full they merge into a small overflow
///   mailbox (same-block deltas concatenate, so delta-accumulating
///   consumers still see complete text; newest partial wins), drained in
///   order before the next must-deliver event.
#[derive(Clone, Debug)]
struct AgentEventTx {
    inner: mpsc::Sender<AgentEvent>,
    overflow: Arc<std::sync::Mutex<UpdateOverflow>>,
    /// Serializes concurrent [`emit`] calls: two tool tasks ending
    /// together must not interleave one's drained updates past the
    /// other's must-deliver event. Async mutex because the drain awaits
    /// channel sends. [`emit_update`] never touches it (lock-free path).
    drain_lock: Arc<tokio::sync::Mutex<()>>,
}

/// The update overflow mailbox plus the drain flag, under ONE mutex: the
/// flag and the "mailbox is empty" observation must change atomically, or
/// an update could slip between them and be reordered.
#[derive(Debug, Default)]
struct UpdateOverflow {
    updates: Vec<AgentEvent>,
    /// Set while [`emit`] drains the mailbox. While draining,
    /// [`emit_update`] must merge into the mailbox instead of
    /// `try_send`-ing directly into the channel: the drain holds older
    /// updates it is still awaiting on, and a direct send would let a
    /// NEWER update jump ahead of them (parallel tool tasks streaming
    /// updates inside the drain window) (F53).
    draining: bool,
}

/// Queued-event bound before backpressure / update coalescing kicks in.
const EVENT_CHANNEL_CAPACITY: usize = 256;
/// Bound for the update overflow mailbox. Same-block updates merge into a
/// single entry, so this only fills via block transitions during a stall;
/// past it, the stalest update is dropped for the newest ("丢旧的留新的").
const UPDATE_OVERFLOW_CAPACITY: usize = 64;

/// Error classes where switching models can help: rate limits, overload,
/// server-side failures, timeouts. (Auth errors, bad requests, context
/// overflow etc. would fail identically on the next model — no fallback.)
pub fn is_fallback_worthy(error_message: Option<&str>) -> bool {
    let Some(error) = error_message else {
        return false;
    };
    let e = error.to_lowercase();
    const PATTERNS: &[&str] = &[
        "429",
        "rate limit",
        "rate_limit",
        "ratelimit",
        "overloaded",
        "529",
        "503",
        "502",
        "quota",
        "capacity",
        "timeout",
        "timed out",
        "deadline exceeded",
        "temporarily unavailable",
        "service unavailable",
    ];
    if PATTERNS.iter().any(|p| e.contains(p)) {
        return true;
    }
    // "500" needs a digit-boundary check: the old `"500 "` pattern missed a
    // trailing "HTTP 500", while a bare "500" would false-positive on
    // "1500" / "5000 tokens".
    e.match_indices("500").any(|(i, _)| {
        let before_ok = i == 0
            || !e[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_digit());
        let after_ok = !e[i + 3..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit());
        before_ok && after_ok
    })
}

fn agent_channel() -> (
    AgentEventTx,
    oneshot::Sender<Vec<AgentMessage>>,
    EventStream<AgentEvent, Vec<AgentMessage>>,
) {
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let tx = AgentEventTx {
        inner: tx,
        overflow: Arc::new(std::sync::Mutex::new(UpdateOverflow::default())),
        drain_lock: Arc::new(tokio::sync::Mutex::new(())),
    };
    let (result_tx, result_rx) = oneshot::channel();
    (
        tx,
        result_tx,
        EventStream::from_parts_bounded(rx, result_rx),
    )
}

/// Send a must-deliver event: bounded backpressure, never dropped while a
/// consumer exists. Overflowed updates are flushed FIRST — they precede
/// this event in producer order and must not arrive after the events that
/// supersede them (a stranded delta after `MessageEnd` would corrupt
/// delta-accumulating consumers like print mode / JSON transcripts).
async fn emit(tx: &AgentEventTx, event: AgentEvent) {
    // Exclusive for the whole drain + final send: a concurrent emit must
    // wait, otherwise it can observe the mailbox empty mid-drain and push
    // its must-deliver event ahead of updates that precede it.
    let _drain_guard = tx.drain_lock.lock().await;
    tx.overflow
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .draining = true;
    loop {
        let pending: Vec<AgentEvent> = {
            let mut overflow = tx.overflow.lock().unwrap_or_else(|e| e.into_inner());
            if overflow.updates.is_empty() {
                // Flag and empty-observation change under the same lock:
                // from here on an update sees draining == false and may
                // try_send directly — nothing older is queued ahead of it.
                overflow.draining = false;
                break;
            }
            std::mem::take(&mut overflow.updates)
        };
        for update in pending {
            if tx.inner.send(update).await.is_err() {
                // Consumer gone: nothing left to deliver to. Clear the
                // flag so surviving senders don't pile into the mailbox.
                tx.overflow
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .draining = false;
                return;
            }
        }
    }
    let _ = tx.inner.send(event).await;
}

/// Push an overwritable streaming update. Never blocks: full channel →
/// merge into the overflow mailbox.
fn emit_update(tx: &AgentEventTx, event: AgentEvent) {
    debug_assert!(matches!(
        event,
        AgentEvent::MessageUpdate { .. } | AgentEvent::ToolExecutionUpdate { .. }
    ));
    let mut overflow = tx.overflow.lock().unwrap_or_else(|e| e.into_inner());
    if !overflow.draining && overflow.updates.is_empty() {
        match tx.inner.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(event)) => {
                merge_update(&mut overflow.updates, event);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    } else {
        merge_update(&mut overflow.updates, event);
    }
}

/// Merge an update into the overflow mailbox, or queue it. Newest
/// supersedes: same-block message deltas concatenate (complete text for
/// delta-accumulating consumers, one fresh partial for partial-state
/// consumers); same-tool execution updates replace wholesale (their
/// partials are cumulative snapshots).
fn merge_update(overflow: &mut Vec<AgentEvent>, event: AgentEvent) {
    enum Action {
        Coalesced,
        ReplaceLast,
        Push,
    }
    let action = match overflow.last_mut() {
        Some(last) => match (&mut *last, &event) {
            (old @ AgentEvent::MessageUpdate { .. }, AgentEvent::MessageUpdate { .. }) => {
                if coalesce_message_updates(old, &event) {
                    Action::Coalesced
                } else {
                    Action::Push
                }
            }
            (
                AgentEvent::ToolExecutionUpdate {
                    tool_call_id: old_id,
                    ..
                },
                AgentEvent::ToolExecutionUpdate {
                    tool_call_id: new_id,
                    ..
                },
            ) if old_id == new_id => Action::ReplaceLast,
            _ => Action::Push,
        },
        None => Action::Push,
    };
    match action {
        Action::Coalesced => {}
        Action::ReplaceLast => {
            *overflow.last_mut().expect("last checked above") = event;
        }
        Action::Push => {
            if overflow.len() < UPDATE_OVERFLOW_CAPACITY {
                overflow.push(event);
            } else {
                // Saturated with unmergeable updates (many block transitions while
                // the consumer is stalled): drop the stalest, keep the newest.
                overflow.remove(0);
                overflow.push(event);
            }
        }
    }
}

/// Fold `new` into `old` when both are same-block streaming deltas:
/// concatenated delta text + the newest cumulative partial/message.
fn coalesce_message_updates(old: &mut AgentEvent, new: &AgentEvent) -> bool {
    let (
        AgentEvent::MessageUpdate {
            assistant_message_event: old_event,
            message: old_message,
        },
        AgentEvent::MessageUpdate {
            assistant_message_event: new_event,
            message: new_message,
        },
    ) = (old, new)
    else {
        return false;
    };
    let coalesced = match (old_event, new_event) {
        (
            tack_ai::AssistantMessageEvent::TextDelta {
                content_index: old_index,
                delta: old_delta,
                partial: old_partial,
            },
            tack_ai::AssistantMessageEvent::TextDelta {
                content_index: new_index,
                delta: new_delta,
                partial: new_partial,
            },
        )
        | (
            tack_ai::AssistantMessageEvent::ThinkingDelta {
                content_index: old_index,
                delta: old_delta,
                partial: old_partial,
            },
            tack_ai::AssistantMessageEvent::ThinkingDelta {
                content_index: new_index,
                delta: new_delta,
                partial: new_partial,
            },
        )
        | (
            tack_ai::AssistantMessageEvent::ToolCallDelta {
                content_index: old_index,
                delta: old_delta,
                partial: old_partial,
            },
            tack_ai::AssistantMessageEvent::ToolCallDelta {
                content_index: new_index,
                delta: new_delta,
                partial: new_partial,
            },
        ) if old_index == new_index => {
            old_delta.push_str(new_delta);
            *old_partial = new_partial.clone();
            true
        }
        _ => false,
    };
    if coalesced {
        *old_message = new_message.clone();
    }
    coalesced
}

/// Minimum interval between streamed `MessageUpdate` events. Providers emit
/// text/thinking/tool-arg deltas far faster than consumers can process them
/// (every event carries a full partial-message clone), which backlogs the
/// unbounded event channel: the model finishes but the UI keeps draining
/// stale updates long after. Coalescing keeps live output at ~20 updates/s.
const STREAM_UPDATE_INTERVAL: Duration = Duration::from_millis(50);

/// Which content block kind a coalesced delta window belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeltaKind {
    Text,
    Thinking,
    ToolCall,
}

/// A streaming delta window: consecutive deltas of one block kind are
/// concatenated into a single equivalent delta, so delta-accumulating
/// consumers (print mode, remote transcripts, JSON mode) see complete text —
/// just coarser chunks — while partial-state consumers (the TUI) get one
/// full `message` per window instead of one per provider chunk.
struct PendingDelta {
    kind: DeltaKind,
    content_index: usize,
    delta: String,
    partial: AssistantMessage,
}

/// Emit the coalesced `MessageUpdate` for the pending delta window, if
/// any. `tail` (the live partial at the end of `context.messages`) is
/// synced to the flushed partial: per-chunk tail updates were dropped
/// (each cost a full message clone), so the tail tracks the last partial
/// the world has seen rather than every provider chunk.
fn flush_pending_update(
    tx: &AgentEventTx,
    pending: &mut Option<PendingDelta>,
    tail: Option<&mut AgentMessage>,
) {
    let Some(p) = pending.take() else {
        return;
    };
    if let Some(tail) = tail {
        *tail = AgentMessage::Assistant(p.partial.clone());
    }
    let message = AgentMessage::Assistant(p.partial.clone());
    let kind_label = match p.kind {
        DeltaKind::Text => "text",
        DeltaKind::Thinking => "thinking",
        DeltaKind::ToolCall => "tool_call",
    };
    // Perf probe (observability.level=debug): correlating these emits with
    // tack_tui::perf "frame" entries localizes streaming-latency complaints —
    // big time gaps HERE = provider/proxy buffering; big gaps in the TUI
    // frame log = terminal/render slowness.
    tracing::debug!(target: "tack_agent_core::stream", kind = kind_label, bytes = p.delta.len(), "coalesced flush");
    let event = match p.kind {
        DeltaKind::Text => tack_ai::AssistantMessageEvent::TextDelta {
            content_index: p.content_index,
            delta: p.delta,
            partial: p.partial,
        },
        DeltaKind::Thinking => tack_ai::AssistantMessageEvent::ThinkingDelta {
            content_index: p.content_index,
            delta: p.delta,
            partial: p.partial,
        },
        DeltaKind::ToolCall => tack_ai::AssistantMessageEvent::ToolCallDelta {
            content_index: p.content_index,
            delta: p.delta,
            partial: p.partial,
        },
    };
    emit_update(
        tx,
        AgentEvent::MessageUpdate {
            assistant_message_event: event,
            message,
        },
    );
}

async fn finish(
    tx: &AgentEventTx,
    result_tx: oneshot::Sender<Vec<AgentMessage>>,
    messages: Vec<AgentMessage>,
) {
    emit(
        tx,
        AgentEvent::AgentEnd {
            messages: messages.clone(),
        },
    )
    .await;
    let _ = result_tx.send(messages);
}

/// Compute the system-message update that brings the transcript's replayed
/// prompt state in line with this run's desired prompt, or `None` when
/// already in sync. Port of upstream's per-run loadout diff
/// (`_preparePromptAndToolLoadout` + `diffSystemPromptSections`): tack
/// stores its rendered prompt in the [`SYSTEM_PROMPT_SECTION`] section and
/// nulls foreign sections. Tool deltas are NOT computed here —
/// [`declare_tool_changes`] folds them into this update before the request.
///
/// `context.system_prompt == None` means "no opinion": replayed sections
/// are left untouched.
pub fn system_state_update(context: &AgentContext) -> Option<AgentMessage> {
    let replayed = tack_ai::transcript::get_current_system_message(&context.messages);
    let replayed_sections = replayed.as_ref().and_then(|m| m.sections.as_ref());
    let desired_prompt = context.system_prompt.as_deref().filter(|p| !p.is_empty());

    let mut patch: indexmap::IndexMap<String, Option<String>> = indexmap::IndexMap::new();
    if let Some(desired) = desired_prompt {
        let current = replayed_sections.and_then(|s| s.get(SYSTEM_PROMPT_SECTION));
        if current != Some(&Some(desired.to_string())) {
            patch.insert(SYSTEM_PROMPT_SECTION.to_string(), Some(desired.to_string()));
        }
        if let Some(sections) = replayed_sections {
            for name in sections.keys() {
                if name != SYSTEM_PROMPT_SECTION {
                    patch.insert(name.clone(), None);
                }
            }
        }
    }
    if patch.is_empty() {
        return None;
    }
    let mut update = SystemMessage::update(now_millis());
    update.sections = Some(patch);
    Some(AgentMessage::System(update))
}

/// Declare tool loadout changes to the model (port of upstream
/// `declareToolChanges` in agent-loop.ts).
///
/// `context.tools` is what the runtime can execute; the transcript's system
/// messages declare what the model may call. Before each request the
/// difference becomes `toolsAdded`/`toolsRemoved` on a system message. When
/// a pending system message exists, its tool fields are treated as intent
/// and replaced with the delta between the committed transcript and the
/// executable set, so replay always yields exactly `context.tools`.
/// Otherwise a new system message is inserted before the first non-system
/// pending message.
pub fn declare_tool_changes(
    context: &AgentContext,
    pending_messages: Vec<AgentMessage>,
) -> Vec<AgentMessage> {
    let system_index = pending_messages
        .iter()
        .rposition(|m| matches!(m, AgentMessage::System(_)));
    // Baseline: pending messages with the system message's tool fields
    // cleared (they are intent, replaced by the exact delta below).
    let cleared: Option<SystemMessage> = system_index.map(|i| {
        let AgentMessage::System(s) = &pending_messages[i] else {
            unreachable!("rposition matched a system message")
        };
        let mut cleared = s.clone();
        cleared.tools_added = None;
        cleared.tools_removed = None;
        cleared
    });
    let current = {
        let committed = context.messages.iter().filter_map(|m| m.as_system());
        let pending_systems = pending_messages.iter().enumerate().filter_map(|(idx, m)| {
            m.as_system()?;
            // The pending system message replays with its tool fields
            // cleared (they are intent, replaced by the exact delta below).
            match (&cleared, system_index == Some(idx)) {
                (Some(cleared), true) => Some(cleared),
                _ => m.as_system(),
            }
        });
        tack_ai::transcript::replay_tools(committed.chain(pending_systems))
    };
    let executable: Vec<tack_ai::ToolDefinition> = context
        .tools
        .iter()
        .map(|t| tack_ai::transcript::to_tool_declaration(&tool_definition(t.as_ref())))
        .collect();
    let changes = tack_ai::transcript::get_tool_state_changes(&current, &executable);
    let unchanged = changes.tools_added.is_empty() && changes.tools_removed.is_empty();

    match system_index {
        Some(i) => {
            let AgentMessage::System(pending) = &pending_messages[i] else {
                unreachable!("rposition matched a system message")
            };
            let pending_declares_tools =
                pending.tools_added.as_ref().is_some_and(|v| !v.is_empty())
                    || pending
                        .tools_removed
                        .as_ref()
                        .is_some_and(|v| !v.is_empty());
            // Keep the caller's message object when it already declares no
            // tool changes.
            if unchanged && !pending_declares_tools {
                return pending_messages;
            }
            let mut out = pending_messages;
            if let AgentMessage::System(s) = &mut out[i] {
                *s = tack_ai::transcript::with_tool_changes(s, &changes);
            }
            out
        }
        None => {
            if unchanged {
                return pending_messages;
            }
            let update = AgentMessage::System(tack_ai::transcript::with_tool_changes(
                &SystemMessage::update(now_millis()),
                &changes,
            ));
            let insert_index = pending_messages
                .iter()
                .position(|m| !matches!(m, AgentMessage::System(_)))
                .unwrap_or(pending_messages.len());
            let mut out = pending_messages;
            out.insert(insert_index, update);
            out
        }
    }
}

/// Restore the active tool loadout declared by the session transcript
/// (upstream `_restoreToolsFromTranscript`, scoped to tack's deferred
/// pool): pool tools named by the replayed state move into the active set,
/// so a resumed session keeps the tools it had activated (e.g. via
/// tool_search). The freshly built active set stays authoritative for
/// everything else — deltas are recorded by [`declare_tool_changes`].
pub fn restore_tools_from_transcript(
    messages: &[AgentMessage],
    active: &mut Vec<Arc<dyn AgentTool>>,
    pool: &mut Vec<Arc<dyn AgentTool>>,
) {
    if pool.is_empty() {
        return;
    }
    let declared = tack_ai::transcript::get_current_tools(messages);
    if declared.is_empty() {
        return;
    }
    let mut i = 0;
    while i < pool.len() {
        let name = pool[i].name();
        if declared.iter().any(|t| t.name == name) && !active.iter().any(|t| t.name() == name) {
            let tool = pool.remove(i);
            tracing::debug!("tool restored from transcript: {name}");
            active.push(tool);
        } else {
            i += 1;
        }
    }
}

/// Main loop. Returns the new messages produced by this run.
async fn run_loop(
    mut context: AgentContext,
    mut new_messages: Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    cancel: CancellationToken,
    tx: &AgentEventTx,
) -> Vec<AgentMessage> {
    let mut first_turn = true;
    // The last completed turn (pi's `lastCompletedTurn`): set only after a
    // turn that produced a usable assistant message. Gates
    // `prepare_next_turn`, which pi applies at the START of the next turn —
    // never after the final turn of a run.
    let mut last_turn: Option<(AssistantMessage, Vec<ToolResultMessage>)> = None;
    // Overflow compact-and-retry budget (upstream
    // `_overflowRecoveryAttempted`): one attempt per failure streak — reset
    // on a new user message or a successful assistant response.
    let mut overflow_recovery_attempted = false;
    // Check for steering messages at start (user may have typed while waiting).
    let mut pending_messages = config.hooks.steering_messages().await;

    // Outer loop: continues when queued follow-up messages arrive after the
    // agent would stop.
    loop {
        let mut has_more_tool_calls = true;

        // Inner loop: process tool calls and steering messages.
        while has_more_tool_calls || !pending_messages.is_empty() {
            if first_turn {
                first_turn = false;
            } else {
                // pi's prepareNextTurn: applied at the start of the next
                // turn, and only when a previous turn actually completed.
                if let Some((prev_message, prev_results)) = &last_turn {
                    let turn_ctx = TurnContext {
                        message: prev_message,
                        tool_results: prev_results,
                        new_messages: &new_messages,
                    };
                    if let Some(update) = config.hooks.prepare_next_turn(&turn_ctx).await {
                        if let Some(model) = update.model {
                            config.model = model;
                        }
                        if let Some(level) = update.thinking_level {
                            config.reasoning = level;
                        }
                    }
                    // Preparation can be long-running (e.g. compaction). Pick
                    // up steering queued while it ran — but only when the
                    // earlier poll returned nothing (pi semantics).
                    if pending_messages.is_empty() {
                        pending_messages = config.hooks.steering_messages().await;
                    }
                }
                emit(tx, AgentEvent::TurnStart).await;
            }

            // Inject pending messages before the next assistant response,
            // declaring prompt/tool state changes (upstream
            // declareToolChanges): a system update may be injected even
            // with no queued messages (e.g. a tool_search activation in the
            // previous batch).
            for message in declare_tool_changes(&context, std::mem::take(&mut pending_messages)) {
                // A new user message re-arms the overflow recovery budget
                // (upstream resets on user message_start).
                if matches!(message, AgentMessage::User(_)) {
                    overflow_recovery_attempted = false;
                }
                emit(
                    tx,
                    AgentEvent::MessageStart {
                        message: message.clone(),
                    },
                )
                .await;
                emit(
                    tx,
                    AgentEvent::MessageEnd {
                        message: message.clone(),
                    },
                )
                .await;
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            // Stream assistant response.
            let message = stream_assistant_response(&mut context, &config, &cancel, tx).await;

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                // Context overflow recovery (upstream agent-session
                // `_checkCompaction` case 1): the provider rejected the
                // request as too large. Compact once and retry the turn
                // against the rebuilt post-compaction context; a second
                // overflow falls through to the normal error path (and
                // the fallback chain below).
                if message.stop_reason == StopReason::Error
                    && !overflow_recovery_attempted
                    && tack_ai::overflow::is_context_overflow(
                        &message,
                        Some(u64::from(config.model.context_window)),
                    )
                    && let Some(rebuilt) = config.hooks.compact_for_overflow().await
                {
                    overflow_recovery_attempted = true;
                    // Retry against the session-rebuilt post-compaction
                    // context. As upstream, the failed message stays in
                    // session history and may sit in the retained tail of
                    // the rebuilt context — it is small; what matters is
                    // that the bulk of history got summarized.
                    context.messages = rebuilt;
                    new_messages.push(AgentMessage::Assistant(message.clone()));
                    emit(
                        tx,
                        AgentEvent::TurnEnd {
                            message,
                            tool_results: Vec::new(),
                        },
                    )
                    .await;
                    tracing::warn!("context overflow detected; compacted and retrying the turn");
                    // Same retry invariants as the fallback path below:
                    // consume last_turn and force another iteration.
                    last_turn = None;
                    has_more_tool_calls = true;
                    continue;
                }
                // Model fallback chain: retry the turn with the next model
                // when the failure is a retryable provider-side condition.
                if message.stop_reason == StopReason::Error
                    && is_fallback_worthy(message.error_message.as_deref())
                    && !config.fallback_models.is_empty()
                {
                    let from = config.model.clone();
                    let to = config.fallback_models.remove(0);
                    if let Some(provider) = tack_ai::provider_for(&to) {
                        config.provider = provider;
                    }
                    let reason = message.error_message.clone().unwrap_or_default();
                    new_messages.push(AgentMessage::Assistant(message.clone()));
                    emit(
                        tx,
                        AgentEvent::TurnEnd {
                            message,
                            tool_results: Vec::new(),
                        },
                    )
                    .await;
                    tracing::warn!(
                        "model fallback: {}/{} -> {}/{} ({reason})",
                        from.provider,
                        from.id,
                        to.provider,
                        to.id
                    );
                    emit(tx, AgentEvent::ModelFallback { from, to, reason }).await;
                    // The retried turn must not re-trigger prepare_next_turn
                    // for the SAME completed turn: consume last_turn (it is
                    // re-set once the retried turn actually completes).
                    last_turn = None;
                    // Force another inner iteration: the previous turn may
                    // have left `has_more_tool_calls` false (no tool calls),
                    // which would silently swallow the retry.
                    has_more_tool_calls = true;
                    continue;
                }
                new_messages.push(AgentMessage::Assistant(message.clone()));
                emit(
                    tx,
                    AgentEvent::TurnEnd {
                        message,
                        tool_results: Vec::new(),
                    },
                )
                .await;
                return new_messages;
            }
            // Recoverable length-stop (upstream `_checkCompaction` case 2):
            // the response was cut off below the model's intended output
            // limit, which points to context pressure or provider-side
            // truncation rather than a genuine max_tokens hit. Compact once
            // and retry the turn; a second length-stop (budget spent) or an
            // unrecoverable one (output reached the limit) falls through to
            // the fail-tool-calls path below.
            if message.stop_reason == StopReason::Length
                && !overflow_recovery_attempted
                && tack_ai::overflow::is_recoverable_length(
                    &message,
                    // The intended output limit is what the request actually
                    // sends: the caller's override wins over the model
                    // default. A user-capped max_tokens hit is a genuine
                    // limit, not context pressure — don't compact for it.
                    u64::from(config.max_tokens.unwrap_or(config.model.max_tokens)),
                )
                && let Some(rebuilt) = config.hooks.compact_for_overflow().await
            {
                overflow_recovery_attempted = true;
                context.messages = rebuilt;
                new_messages.push(AgentMessage::Assistant(message.clone()));
                emit(
                    tx,
                    AgentEvent::TurnEnd {
                        message,
                        tool_results: Vec::new(),
                    },
                )
                .await;
                tracing::warn!("recoverable length-stop detected; compacted and retrying the turn");
                // Same retry invariants as the overflow path above.
                last_turn = None;
                has_more_tool_calls = true;
                continue;
            }
            // A successful (non-error/length) response re-arms the overflow
            // recovery budget (upstream resets on assistant message_end).
            if !matches!(message.stop_reason, StopReason::Length) {
                overflow_recovery_attempted = false;
            }
            new_messages.push(AgentMessage::Assistant(message.clone()));

            // Check for tool calls.
            let tool_calls: Vec<(String, String, Value)> = message
                .tool_calls()
                .map(|(id, name, args)| (id.to_string(), name.to_string(), args.clone()))
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                // A "length" stop means the output was cut off by the token
                // limit, so every tool call may carry truncated arguments.
                // Fail them all instead of executing potentially borked calls.
                let batch = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&tool_calls, tx).await
                } else {
                    execute_tool_calls(&context, &message, &tool_calls, &config, &cancel, tx).await
                };
                has_more_tool_calls = !batch.terminate;

                // Client-side tool search: results may activate deferred pool
                // tools (tack-internal channel; see AgentToolResult).
                // Activated tools join the context for subsequent LLM calls,
                // and the next declare_tool_changes records the delta as a
                // transcript system message (upstream #9548 semantics).
                if !config.tool_pool.is_empty() {
                    for name in &batch.activated_tool_names {
                        if context.tools.iter().any(|t| t.name() == name) {
                            continue;
                        }
                        if let Some(pos) = config.tool_pool.iter().position(|t| t.name() == name) {
                            context.tools.push(config.tool_pool.remove(pos));
                            tracing::debug!("tool activated via tool_search: {name}");
                        }
                    }
                }

                tool_results = batch.messages;
                for result in &tool_results {
                    context
                        .messages
                        .push(AgentMessage::ToolResult(result.clone()));
                    new_messages.push(AgentMessage::ToolResult(result.clone()));
                }
            }

            emit(
                tx,
                AgentEvent::TurnEnd {
                    message: message.clone(),
                    tool_results: tool_results.clone(),
                },
            )
            .await;

            {
                let turn_ctx = TurnContext {
                    message: &message,
                    tool_results: &tool_results,
                    new_messages: &new_messages,
                };
                if config.hooks.should_stop_after_turn(&turn_ctx).await {
                    return new_messages;
                }
            }
            last_turn = Some((message, tool_results));

            pending_messages = config.hooks.steering_messages().await;
        }

        // Agent would stop here. Check for follow-up messages.
        let follow_up = config.hooks.follow_up_messages().await;
        if !follow_up.is_empty() {
            pending_messages = follow_up;
            continue;
        }
        break;
    }

    new_messages
}

/// Stream an assistant response from the LLM, emitting message events and
/// maintaining the partial message at the tail of `context.messages`.
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    tx: &AgentEventTx,
) -> AssistantMessage {
    // AgentMessage[] -> AgentMessage[] transform (e.g. compaction). COW:
    // hooks borrow the context and only materialize a rewritten copy when
    // they actually change it, so the common no-op path no longer
    // deep-clones the full history (images included) before every LLM call.
    let transformed = config.hooks.transform_context(&context.messages).await;
    let messages: &[AgentMessage] = transformed.as_deref().unwrap_or(&context.messages);
    // AgentMessage[] -> Message[] at the LLM boundary.
    let llm_messages = config.hooks.convert_to_llm(messages);

    // Transcript system messages carry the prompt/tool declarations
    // (upstream #9548). tack providers take the prompt as a separate
    // Context field, so replay the transcript here: the replayed prompt
    // wins; system roles never reach provider converters (collapse
    // semantics for APIs without mid-conversation system messages). When
    // the transcript declares no system state (old sessions, ad-hoc
    // callers), the loop's configured system_prompt applies unchanged.
    let has_system = llm_messages
        .iter()
        .any(|m| matches!(m, tack_ai::Message::System(_)));
    let (system_prompt, request_messages) = if has_system {
        let prompt = tack_ai::transcript::get_current_system_prompt(&llm_messages);
        let messages = llm_messages
            .into_iter()
            .filter(|m| !matches!(m, tack_ai::Message::System(_)))
            .collect();
        (
            if prompt.is_empty() {
                None
            } else {
                Some(prompt)
            },
            messages,
        )
    } else {
        (context.system_prompt.clone(), llm_messages)
    };

    let llm_context = Context {
        system_prompt,
        messages: request_messages,
        tools: context
            .tools
            .iter()
            // Provider-bound tools (e.g. ask_codebuddy) are only advertised
            // to their provider's models.
            .filter(|t| t.available_for_provider(&config.model.provider))
            .map(|t| tool_definition(t.as_ref()))
            .collect(),
    };

    let resolved = match config.auth.resolve().await {
        Ok(auth) => auth,
        Err(e) => {
            // Auth resolution failure (e.g. OAuth refresh rejected): surface
            // as an in-band error message, same as a provider error.
            let mut message = AssistantMessage::pending(&config.model);
            message.stop_reason = StopReason::Error;
            message.error_message = Some(e);
            context
                .messages
                .push(AgentMessage::Assistant(message.clone()));
            emit(
                tx,
                AgentEvent::MessageStart {
                    message: AgentMessage::Assistant(message.clone()),
                },
            )
            .await;
            emit(
                tx,
                AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(message.clone()),
                },
            )
            .await;
            return message;
        }
    };

    // Per-credential base URL override (e.g. copilot proxy-ep).
    let mut model = config.model.clone();
    if let Some(base_url) = &resolved.base_url {
        model.base_url = base_url.clone();
    }

    let options = StreamOptions {
        api_key: resolved.api_key,
        headers: resolved.headers,
        max_tokens: config.max_tokens,
        temperature: config.temperature,
        reasoning: config.reasoning,
        session_id: config.session_id.clone(),
        cache_retention: config.cache_retention,
        cancel: cancel.clone(),
        retry_cancel: config.retry_cancel.clone(),
        ..Default::default()
    };

    let mut response = config.provider.stream(&model, &llm_context, options);

    let mut added_partial = false;
    let mut pending_update: Option<PendingDelta> = None;
    let mut last_update = Instant::now();

    loop {
        // A pending coalesced window must flush on the timer even when the
        // provider goes quiet mid-block; otherwise already-generated text
        // stays invisible to consumers until the next provider event (which
        // may be seconds away). `recv` is cancellation-safe, so the timeout
        // drops no events.
        let next = if pending_update.is_some() {
            let remaining = STREAM_UPDATE_INTERVAL.saturating_sub(last_update.elapsed());
            match tokio::time::timeout(remaining, response.next()).await {
                Ok(event) => event,
                Err(_) => {
                    flush_pending_update(tx, &mut pending_update, context.messages.last_mut());
                    last_update = Instant::now();
                    continue;
                }
            }
        } else {
            response.next().await
        };
        let Some(event) = next else {
            // Producer dropped without a terminal event — synthesize an error
            // message rather than hanging (defensive; providers shouldn't).
            flush_pending_update(tx, &mut pending_update, None);
            let mut message = AssistantMessage::pending(&config.model);
            message.stop_reason = StopReason::Error;
            message.error_message = Some("provider stream ended without a terminal event".into());
            if added_partial {
                let last = context.messages.len() - 1;
                context.messages[last] = AgentMessage::Assistant(message.clone());
            } else {
                context
                    .messages
                    .push(AgentMessage::Assistant(message.clone()));
                emit(
                    tx,
                    AgentEvent::MessageStart {
                        message: AgentMessage::Assistant(message.clone()),
                    },
                )
                .await;
            }
            emit(
                tx,
                AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(message.clone()),
                },
            )
            .await;
            return message;
        };

        // The event is consumed by value so the accumulated `partial` can
        // MOVE into the coalescing window / emitted events instead of being
        // deep-cloned per provider chunk (O(stream length) per chunk →
        // O(stream length) per flush window).
        match event {
            tack_ai::AssistantMessageEvent::Start { partial } => {
                context
                    .messages
                    .push(AgentMessage::Assistant(partial.clone()));
                added_partial = true;
                emit(
                    tx,
                    AgentEvent::MessageStart {
                        message: AgentMessage::Assistant(partial),
                    },
                )
                .await;
            }
            delta @ (tack_ai::AssistantMessageEvent::TextDelta { .. }
            | tack_ai::AssistantMessageEvent::ThinkingDelta { .. }
            | tack_ai::AssistantMessageEvent::ToolCallDelta { .. }) => {
                if added_partial {
                    // Deltas are coalesced into a window (see
                    // STREAM_UPDATE_INTERVAL); boundary events flush the
                    // window first so block transitions keep their order.
                    let (kind, content_index, delta_text, partial) = match delta {
                        tack_ai::AssistantMessageEvent::TextDelta {
                            content_index,
                            delta,
                            partial,
                        } => (DeltaKind::Text, content_index, delta, partial),
                        tack_ai::AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta,
                            partial,
                        } => (DeltaKind::Thinking, content_index, delta, partial),
                        tack_ai::AssistantMessageEvent::ToolCallDelta {
                            content_index,
                            delta,
                            partial,
                        } => (DeltaKind::ToolCall, content_index, delta, partial),
                        _ => unreachable!("delta arm matched above"),
                    };
                    let continues = matches!(
                        &pending_update,
                        Some(p) if p.kind == kind && p.content_index == content_index
                    );
                    if !continues {
                        flush_pending_update(tx, &mut pending_update, context.messages.last_mut());
                    }
                    match &mut pending_update {
                        Some(p) => {
                            p.delta.push_str(&delta_text);
                            p.partial = partial;
                        }
                        None => {
                            pending_update = Some(PendingDelta {
                                kind,
                                content_index,
                                delta: delta_text,
                                partial,
                            });
                        }
                    }
                    if last_update.elapsed() >= STREAM_UPDATE_INTERVAL {
                        flush_pending_update(tx, &mut pending_update, context.messages.last_mut());
                        last_update = Instant::now();
                    }
                }
            }
            boundary @ (tack_ai::AssistantMessageEvent::TextStart { .. }
            | tack_ai::AssistantMessageEvent::TextEnd { .. }
            | tack_ai::AssistantMessageEvent::ThinkingStart { .. }
            | tack_ai::AssistantMessageEvent::ThinkingEnd { .. }
            | tack_ai::AssistantMessageEvent::ToolCallStart { .. }
            | tack_ai::AssistantMessageEvent::ToolCallEnd { .. }) => {
                if added_partial {
                    flush_pending_update(tx, &mut pending_update, context.messages.last_mut());
                    let partial = boundary
                        .partial()
                        .expect("boundary events carry a partial")
                        .clone();
                    let last = context.messages.len() - 1;
                    context.messages[last] = AgentMessage::Assistant(partial.clone());
                    emit_update(
                        tx,
                        AgentEvent::MessageUpdate {
                            assistant_message_event: boundary,
                            message: AgentMessage::Assistant(partial),
                        },
                    );
                }
            }
            tack_ai::AssistantMessageEvent::Done { message, .. }
            | tack_ai::AssistantMessageEvent::Error { error: message, .. } => {
                // Flush the window so delta-accumulating consumers (print
                // mode, remote transcripts) see the complete text even if
                // the adapter skipped TextEnd; MessageEnd below then carries
                // the exact final message.
                flush_pending_update(tx, &mut pending_update, None);
                let final_message = message;
                if added_partial {
                    let last = context.messages.len() - 1;
                    context.messages[last] = AgentMessage::Assistant(final_message.clone());
                } else {
                    context
                        .messages
                        .push(AgentMessage::Assistant(final_message.clone()));
                    emit(
                        tx,
                        AgentEvent::MessageStart {
                            message: AgentMessage::Assistant(final_message.clone()),
                        },
                    )
                    .await;
                }
                emit(
                    tx,
                    AgentEvent::MessageEnd {
                        message: AgentMessage::Assistant(final_message.clone()),
                    },
                )
                .await;
                return final_message;
            }
        }
    }
}

struct ExecutedToolCallBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
    /// Deferred pool tools activated by this batch (tack-internal channel
    /// from AgentToolResult.added_tool_names; never serialized).
    activated_tool_names: Vec<String>,
}

/// Collect the deferred-pool activations from a finalized batch.
fn activated_names(finalized_calls: &[FinalizedToolCall]) -> Vec<String> {
    finalized_calls
        .iter()
        .flat_map(|f| f.result.added_tool_names.iter().flatten().cloned())
        .collect()
}

struct FinalizedToolCall {
    tool_call_id: String,
    tool_name: String,
    result: AgentToolResult,
    is_error: bool,
}

fn error_tool_result(message: impl Into<String>) -> AgentToolResult {
    AgentToolResult::error(message)
}

/// Extract a displayable message from a caught panic payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn should_terminate_batch(finalized: &[FinalizedToolCall]) -> bool {
    !finalized.is_empty() && finalized.iter().all(|f| f.result.terminate)
}

fn create_tool_result_message(finalized: &FinalizedToolCall) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call_id.clone(),
        tool_name: finalized.tool_name.clone(),
        content: finalized.result.content.clone(),
        details: Some(finalized.result.details.clone()),
        usage: finalized.result.usage.clone(),
        is_error: finalized.is_error,
        timestamp: now_millis(),
    }
}

async fn emit_tool_execution_end(tx: &AgentEventTx, finalized: &FinalizedToolCall) {
    emit(
        tx,
        AgentEvent::ToolExecutionEnd {
            tool_call_id: finalized.tool_call_id.clone(),
            tool_name: finalized.tool_name.clone(),
            result: finalized.result.clone(),
            is_error: finalized.is_error,
        },
    )
    .await;
}

async fn emit_tool_result_message(tx: &AgentEventTx, message: &ToolResultMessage) {
    let message = AgentMessage::ToolResult(message.clone());
    emit(
        tx,
        AgentEvent::MessageStart {
            message: message.clone(),
        },
    )
    .await;
    emit(tx, AgentEvent::MessageEnd { message }).await;
}

/// Fail all tool calls from an assistant message truncated by the output
/// token limit. Streamed arguments are salvaged with a best-effort parser, so
/// truncated calls can validate yet be silently incomplete — none are safe to
/// execute.
async fn fail_tool_calls_from_truncated_message(
    tool_calls: &[(String, String, Value)],
    tx: &AgentEventTx,
) -> ExecutedToolCallBatch {
    let mut messages = Vec::new();
    for (id, name, args) in tool_calls {
        emit(
            tx,
            AgentEvent::ToolExecutionStart {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                args: args.clone(),
            },
        )
        .await;
        let finalized = FinalizedToolCall {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            result: error_tool_result(format!(
                "Tool call \"{name}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments."
            )),
            is_error: true,
        };
        emit_tool_execution_end(tx, &finalized).await;
        let message = create_tool_result_message(&finalized);
        emit_tool_result_message(tx, &message).await;
        messages.push(message);
    }
    ExecutedToolCallBatch {
        messages,
        terminate: false,
        activated_tool_names: Vec::new(),
    }
}

/// Outcome of the preflight phase for one tool call.
enum Preparation {
    /// Ready to execute.
    Prepared {
        tool: Arc<dyn AgentTool>,
        args: Value,
    },
    /// Resolved without execution (validation failure, blocked, aborted).
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
}

/// Preflight for one tool call. Panics anywhere in preparation
/// (prepare_arguments / validate_arguments / the before_tool_call hook)
/// degrade to an in-band error result — pi wraps `prepareToolCall` in a
/// try/catch that does the same with JS exceptions.
async fn prepare_tool_call(
    context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call_id: &str,
    tool_name: &str,
    raw_args: &Value,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
) -> Preparation {
    let inner = prepare_tool_call_inner(
        context,
        assistant_message,
        tool_call_id,
        tool_name,
        raw_args,
        config,
        cancel,
    );
    match std::panic::AssertUnwindSafe(inner).catch_unwind().await {
        Ok(preparation) => preparation,
        Err(payload) => Preparation::Immediate {
            result: error_tool_result(format!(
                "tool preparation panicked: {}",
                panic_message(&payload)
            )),
            is_error: true,
        },
    }
}

async fn prepare_tool_call_inner(
    context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call_id: &str,
    tool_name: &str,
    raw_args: &Value,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
) -> Preparation {
    let Some(tool) = context.tools.iter().find(|t| t.name() == tool_name) else {
        return Preparation::Immediate {
            result: error_tool_result(format!("Tool {tool_name} not found")),
            is_error: true,
        };
    };

    let args = tool.prepare_arguments(raw_args.clone());
    if let Err(e) = tool.validate_arguments(&args) {
        return Preparation::Immediate {
            result: error_tool_result(e),
            is_error: true,
        };
    }

    let before_ctx = BeforeToolCallContext {
        assistant_message,
        tool_call_id,
        tool_name,
        args: &args,
        context: &context.messages,
    };
    if cancel.is_cancelled() {
        return Preparation::Immediate {
            result: error_tool_result("Operation aborted"),
            is_error: true,
        };
    }
    match config.hooks.before_tool_call(&before_ctx).await {
        BeforeToolCallOutcome::Allow => {}
        BeforeToolCallOutcome::Block { reason, terminate } => {
            let mut result =
                error_tool_result(reason.unwrap_or_else(|| "Tool execution was blocked".into()));
            result.terminate = terminate;
            return Preparation::Immediate {
                result,
                is_error: true,
            };
        }
        BeforeToolCallOutcome::Rewrite { args: rewritten } => {
            // Hooks rewrote the arguments (Claude updatedInput semantics):
            // re-validate before execution so a hook can't smuggle in a
            // schema-invalid call the tool would have rejected up front.
            let args = tool.prepare_arguments(rewritten);
            if let Err(e) = tool.validate_arguments(&args) {
                return Preparation::Immediate {
                    result: error_tool_result(format!("hook-rewritten arguments invalid: {e}")),
                    is_error: true,
                };
            }
            if cancel.is_cancelled() {
                return Preparation::Immediate {
                    result: error_tool_result("Operation aborted"),
                    is_error: true,
                };
            }
            return Preparation::Prepared {
                tool: tool.clone(),
                args,
            };
        }
    }
    if cancel.is_cancelled() {
        return Preparation::Immediate {
            result: error_tool_result("Operation aborted"),
            is_error: true,
        };
    }

    Preparation::Prepared {
        tool: tool.clone(),
        args,
    }
}

/// Execute + finalize one prepared tool call (shared by sequential and
/// parallel paths). Emits `tool_execution_update` and `tool_execution_end`.
/// `context_messages` is a snapshot for the `after_tool_call` hook (tools
/// only read it), shared via `Arc` so a batch of tool calls clones the
/// full message history ONCE instead of once per tool call.
#[allow(clippy::too_many_arguments)]
async fn execute_and_finalize(
    context_messages: Arc<Vec<AgentMessage>>,
    assistant_message: AssistantMessage,
    tool_call_id: String,
    tool_name: String,
    tool_call_args: Value,
    tool: Arc<dyn AgentTool>,
    args: Value,
    config: AgentLoopConfig,
    cancel: CancellationToken,
    tx: AgentEventTx,
) -> FinalizedToolCall {
    let update_tx = tx.clone();
    let update_id = tool_call_id.clone();
    let update_name = tool_name.clone();
    let update_args = tool_call_args.clone();
    let on_update = move |partial: AgentToolResult| {
        // Streaming tool partials are overwritable updates: never block the
        // tool on a stalled consumer — merge into the overflow instead.
        emit_update(
            &update_tx,
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: update_id.clone(),
                tool_name: update_name.clone(),
                args: update_args.clone(),
                partial_result: partial,
            },
        );
    };

    let executed = match std::panic::AssertUnwindSafe(tool.execute(
        &tool_call_id,
        args.clone(),
        cancel,
        &on_update,
    ))
    .catch_unwind()
    .await
    {
        Ok(Ok(result)) => (result, false),
        Ok(Err(e)) => (error_tool_result(e), true),
        // pi catches tool exceptions and turns them into error results.
        Err(payload) => (
            error_tool_result(format!("tool panicked: {}", panic_message(&payload))),
            true,
        ),
    };

    let (mut result, mut is_error) = executed;
    let after_ctx = AfterToolCallContext {
        assistant_message: &assistant_message,
        tool_call_id: &tool_call_id,
        tool_name: &tool_name,
        args: &args,
        context: &context_messages,
    };
    // pi wraps afterToolCall in try/catch: a throwing hook replaces the
    // result with an error.
    match std::panic::AssertUnwindSafe(config.hooks.after_tool_call(&after_ctx, &result, is_error))
        .catch_unwind()
        .await
    {
        Ok(Some(patch)) => {
            if let Some(content) = patch.content {
                result.content = content;
            }
            if let Some(details) = patch.details {
                result.details = details;
            }
            if let Some(usage) = patch.usage {
                result.usage = Some(usage);
            }
            if let Some(terminate) = patch.terminate {
                result.terminate = terminate;
            }
            if let Some(err) = patch.is_error {
                is_error = err;
            }
        }
        Ok(None) => {}
        Err(payload) => {
            result = error_tool_result(format!(
                "after_tool_call hook panicked: {}",
                panic_message(&payload)
            ));
            is_error = true;
        }
    }

    let finalized = FinalizedToolCall {
        tool_call_id,
        tool_name,
        result,
        is_error,
    };
    emit_tool_execution_end(&tx, &finalized).await;
    finalized
}

async fn execute_tool_calls(
    context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[(String, String, Value)],
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    tx: &AgentEventTx,
) -> ExecutedToolCallBatch {
    let has_sequential_tool = tool_calls.iter().any(|(_, name, _)| {
        context
            .tools
            .iter()
            .find(|t| t.name() == name)
            .is_some_and(|t| t.execution_mode() == ToolExecutionMode::Sequential)
    });
    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential_tool {
        execute_tool_calls_sequential(context, assistant_message, tool_calls, config, cancel, tx)
            .await
    } else {
        execute_tool_calls_parallel(context, assistant_message, tool_calls, config, cancel, tx)
            .await
    }
}

async fn execute_tool_calls_sequential(
    context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[(String, String, Value)],
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    tx: &AgentEventTx,
) -> ExecutedToolCallBatch {
    let mut finalized_calls: Vec<FinalizedToolCall> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    let mut processed = 0;
    // One shared context snapshot for the whole batch's after_tool_call
    // hooks (Arc: the history is cloned once, not once per tool call).
    let mut context_snapshot: Option<Arc<Vec<AgentMessage>>> = None;
    for (id, name, args) in tool_calls {
        emit(
            tx,
            AgentEvent::ToolExecutionStart {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                args: args.clone(),
            },
        )
        .await;

        let preparation =
            prepare_tool_call(context, assistant_message, id, name, args, config, cancel).await;
        let finalized = match preparation {
            Preparation::Immediate { result, is_error } => {
                let f = FinalizedToolCall {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(tx, &f).await;
                f
            }
            Preparation::Prepared { tool, args } => {
                let snapshot = context_snapshot
                    .get_or_insert_with(|| Arc::new(context.messages.clone()))
                    .clone();
                execute_and_finalize(
                    snapshot,
                    assistant_message.clone(),
                    id.clone(),
                    name.clone(),
                    args.clone(),
                    tool,
                    args,
                    config.clone(),
                    cancel.clone(),
                    tx.clone(),
                )
                .await
            }
        };

        let message = create_tool_result_message(&finalized);
        emit_tool_result_message(tx, &message).await;
        messages.push(message);
        finalized_calls.push(finalized);
        processed += 1;

        if cancel.is_cancelled() {
            break;
        }
    }

    // Cancel mid-batch: the remaining tool calls still need results.
    // Leaving them dangling means the assistant message in the resumed
    // context has tool calls with no matching result, which providers
    // reject (HTTP 400 on the next request).
    for (id, name, args) in &tool_calls[processed..] {
        emit(
            tx,
            AgentEvent::ToolExecutionStart {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                args: args.clone(),
            },
        )
        .await;
        let finalized = FinalizedToolCall {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            result: error_tool_result("Operation aborted"),
            is_error: true,
        };
        emit_tool_execution_end(tx, &finalized).await;
        let message = create_tool_result_message(&finalized);
        emit_tool_result_message(tx, &message).await;
        messages.push(message);
        finalized_calls.push(finalized);
    }

    ExecutedToolCallBatch {
        activated_tool_names: activated_names(&finalized_calls),
        messages,
        terminate: should_terminate_batch(&finalized_calls),
    }
}

async fn execute_tool_calls_parallel(
    context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[(String, String, Value)],
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    tx: &AgentEventTx,
) -> ExecutedToolCallBatch {
    // Preflight is sequential and in source order (matches TS).
    #[allow(clippy::large_enum_variant)]
    enum Pending {
        Done(FinalizedToolCall),
        Running {
            tool_call_id: String,
            tool_name: String,
            handle: tokio::task::JoinHandle<FinalizedToolCall>,
        },
    }
    let mut pending: Vec<Pending> = Vec::new();
    // One shared context snapshot for the whole batch's after_tool_call
    // hooks (Arc: the history is cloned once, not once per tool call).
    let mut context_snapshot: Option<Arc<Vec<AgentMessage>>> = None;

    let mut started = 0;
    for (id, name, args) in tool_calls {
        emit(
            tx,
            AgentEvent::ToolExecutionStart {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                args: args.clone(),
            },
        )
        .await;

        let preparation =
            prepare_tool_call(context, assistant_message, id, name, args, config, cancel).await;
        match preparation {
            Preparation::Immediate { result, is_error } => {
                let f = FinalizedToolCall {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(tx, &f).await;
                pending.push(Pending::Done(f));
            }
            Preparation::Prepared {
                tool,
                args: prepared_args,
            } => {
                // Spawn: tool_execution_end fires in completion order.
                let snapshot = context_snapshot
                    .get_or_insert_with(|| Arc::new(context.messages.clone()))
                    .clone();
                pending.push(Pending::Running {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    handle: tokio::spawn(execute_and_finalize(
                        // Shared snapshot for the after_tool_call hook
                        // context; parallel tools only read it.
                        snapshot,
                        assistant_message.clone(),
                        id.clone(),
                        name.clone(),
                        args.clone(),
                        tool,
                        prepared_args,
                        config.clone(),
                        cancel.clone(),
                        tx.clone(),
                    )),
                });
            }
        }
        started += 1;
        if cancel.is_cancelled() {
            break;
        }
    }

    // Cancel mid-preflight: the tool calls never started still need
    // results — a resumed context with unanswered tool calls is rejected
    // by providers (HTTP 400).
    for (id, name, args) in &tool_calls[started..] {
        emit(
            tx,
            AgentEvent::ToolExecutionStart {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                args: args.clone(),
            },
        )
        .await;
        let f = FinalizedToolCall {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            result: error_tool_result("Operation aborted"),
            is_error: true,
        };
        emit_tool_execution_end(tx, &f).await;
        pending.push(Pending::Done(f));
    }

    // Results join in source order regardless of completion order.
    let mut finalized_calls: Vec<FinalizedToolCall> = Vec::new();
    for entry in pending {
        match entry {
            Pending::Done(f) => finalized_calls.push(f),
            Pending::Running {
                tool_call_id,
                tool_name,
                handle,
            } => match handle.await {
                Ok(f) => finalized_calls.push(f),
                Err(e) => {
                    // The task died before finalizing (tool/hook panics are
                    // caught inside execute_and_finalize, so this is a last
                    // line of defense). Keep the real tool call id/name so
                    // the result still matches the assistant's tool call,
                    // and emit tool_execution_end so observers don't hang.
                    let f = FinalizedToolCall {
                        tool_call_id,
                        tool_name,
                        result: error_tool_result(format!("tool task failed: {e}")),
                        is_error: true,
                    };
                    emit_tool_execution_end(tx, &f).await;
                    finalized_calls.push(f);
                }
            },
        }
    }

    let mut messages = Vec::new();
    for finalized in &finalized_calls {
        let message = create_tool_result_message(finalized);
        emit_tool_result_message(tx, &message).await;
        messages.push(message);
    }

    ExecutedToolCallBatch {
        activated_tool_names: activated_names(&finalized_calls),
        messages,
        terminate: should_terminate_batch(&finalized_calls),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod channel_tests {
    //! Bounded event channel: backpressure for must-deliver events,
    //! merge/replace coalescing for overwritable streaming updates.
    use super::*;

    fn test_model() -> Model {
        Model {
            id: "mock".into(),
            name: "Mock".into(),
            api: "mock".into(),
            provider: "mock".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![tack_ai::InputKind::Text],
            cost: tack_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn text_update(index: usize, delta: &str, tag: &str) -> AgentEvent {
        let mut partial = AssistantMessage::pending(&test_model());
        partial.content = vec![tack_ai::ContentBlock::text(tag)];
        AgentEvent::MessageUpdate {
            assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta {
                content_index: index,
                delta: delta.to_string(),
                partial: partial.clone(),
            },
            message: AgentMessage::Assistant(partial),
        }
    }

    fn boundary_update(index: usize, tag: &str) -> AgentEvent {
        let mut partial = AssistantMessage::pending(&test_model());
        partial.content = vec![tack_ai::ContentBlock::text(tag)];
        AgentEvent::MessageUpdate {
            assistant_message_event: tack_ai::AssistantMessageEvent::TextStart {
                content_index: index,
                partial: partial.clone(),
            },
            message: AgentMessage::Assistant(partial),
        }
    }

    fn tool_update(tool_call_id: &str, text: &str) -> AgentEvent {
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "mock".to_string(),
            args: Value::Null,
            partial_result: AgentToolResult::text(text),
        }
    }

    fn must_deliver(tag: &str) -> AgentEvent {
        AgentEvent::MessageStart {
            message: AgentMessage::user(tag),
        }
    }

    /// Same-block streaming deltas merge into ONE overflow entry with
    /// concatenated delta text and the newest partial, so a stalled
    /// consumer still sees complete text and memory stays bounded.
    #[tokio::test]
    async fn same_block_updates_coalesce_in_overflow() {
        let (tx, _result_tx, mut stream) = agent_channel();
        // Fill the bounded channel; nobody is draining.
        for i in 0..EVENT_CHANNEL_CAPACITY {
            emit_update(&tx, text_update(0, &format!("d{i} "), &format!("p{i}")));
        }
        assert_eq!(tx.inner.capacity(), 0);
        // These overflow and must coalesce into a single entry.
        emit_update(&tx, text_update(0, "x", "px"));
        emit_update(&tx, text_update(0, "y", "py"));
        emit_update(&tx, text_update(0, "z", "pz"));
        assert_eq!(tx.overflow.lock().unwrap().updates.len(), 1);

        // A must-deliver event drains the overflow first, then itself —
        // blocking on channel space as the consumer reads.
        let tx2 = tx.clone();
        let sender = tokio::spawn(async move {
            emit(&tx2, must_deliver("end")).await;
        });
        let mut drained = Vec::new();
        for _ in 0..EVENT_CHANNEL_CAPACITY + 2 {
            drained.push(stream.next().await.unwrap());
        }
        sender.await.unwrap();
        // The merged update sits right before the must-deliver event.
        let AgentEvent::MessageUpdate {
            assistant_message_event:
                tack_ai::AssistantMessageEvent::TextDelta { delta, partial, .. },
            ..
        } = &drained[EVENT_CHANNEL_CAPACITY]
        else {
            panic!("expected a merged TextDelta update");
        };
        assert_eq!(delta, "xyz");
        assert_eq!(partial.text(), "pz");
        assert!(matches!(
            drained[EVENT_CHANNEL_CAPACITY + 1],
            AgentEvent::MessageStart { .. }
        ));
    }

    /// Different-block updates cannot concatenate deltas: they queue
    /// separately (bounded), dropping the stalest past the overflow cap.
    #[tokio::test]
    async fn unmergeable_updates_drop_oldest_when_overflow_saturated() {
        let (tx, _result_tx, _stream) = agent_channel();
        for i in 0..EVENT_CHANNEL_CAPACITY {
            emit_update(&tx, boundary_update(i, &format!("fill{i}")));
        }
        assert_eq!(tx.inner.capacity(), 0);
        for i in 0..UPDATE_OVERFLOW_CAPACITY + 10 {
            emit_update(&tx, boundary_update(i, &format!("b{i}")));
        }
        let overflow = tx.overflow.lock().unwrap();
        assert_eq!(overflow.updates.len(), UPDATE_OVERFLOW_CAPACITY);
        // The 10 stalest were dropped; the newest survived.
        let AgentEvent::MessageUpdate { message, .. } = overflow.updates.last().unwrap() else {
            panic!("expected message update");
        };
        let AgentMessage::Assistant(a) = message else {
            panic!("expected assistant");
        };
        assert_eq!(a.text(), format!("b{}", UPDATE_OVERFLOW_CAPACITY + 9));
    }

    /// Tool execution updates are cumulative snapshots: same tool call
    /// replaces wholesale in the overflow.
    #[tokio::test]
    async fn tool_updates_replace_in_overflow() {
        let (tx, _result_tx, _stream) = agent_channel();
        for i in 0..EVENT_CHANNEL_CAPACITY {
            emit_update(&tx, boundary_update(i, &format!("fill{i}")));
        }
        emit_update(&tx, tool_update("t1", "old"));
        emit_update(&tx, tool_update("t1", "new"));
        emit_update(&tx, tool_update("t2", "other"));
        let overflow = tx.overflow.lock().unwrap();
        assert_eq!(overflow.updates.len(), 2);
        let AgentEvent::ToolExecutionUpdate { partial_result, .. } = &overflow.updates[0] else {
            panic!("expected tool update");
        };
        let Some(tack_ai::InputContentBlock::Text { text, .. }) = partial_result.content.first()
        else {
            panic!("expected text partial result");
        };
        assert_eq!(text, "new");
    }

    /// Must-deliver events never drop: a full channel applies backpressure
    /// (the send pends) until the consumer drains a slot.
    #[tokio::test]
    async fn must_deliver_backpressures_instead_of_dropping() {
        let (tx, _result_tx, mut stream) = agent_channel();
        for i in 0..EVENT_CHANNEL_CAPACITY {
            emit(&tx, must_deliver(&format!("m{i}"))).await;
        }
        let tx2 = tx.clone();
        let blocked = tokio::spawn(async move {
            emit(&tx2, must_deliver("last")).await;
        });
        // No slot: the send must still be pending.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!blocked.is_finished());
        // Drain one event; the pending send completes and preserves order.
        let first = stream.next().await.unwrap();
        assert!(matches!(first, AgentEvent::MessageStart { .. }));
        blocked.await.unwrap();
        let mut last = None;
        for _ in 0..EVENT_CHANNEL_CAPACITY {
            last = Some(stream.next().await.unwrap());
        }
        let Some(AgentEvent::MessageStart {
            message: AgentMessage::User(u),
        }) = last
        else {
            panic!("expected the final user message start");
        };
        let tack_ai::UserContent::Text(text) = &u.content else {
            panic!("expected text user content");
        };
        assert_eq!(text, "last");
    }

    /// A dropped receiver fails sends quietly (producer keeps working),
    /// same as the old unbounded behavior.
    #[tokio::test]
    async fn closed_channel_drops_quietly() {
        let (tx, _result_tx, stream) = agent_channel();
        drop(stream);
        emit_update(&tx, text_update(0, "d", "p"));
        emit(&tx, must_deliver("m")).await;
        assert!(tx.overflow.lock().unwrap().updates.is_empty());
    }

    /// Regression (F53): an update emitted WHILE `emit` drains the
    /// overflow must join the mailbox instead of try_send-ing into a
    /// freshly freed channel slot — a direct send would land it ahead of
    /// the older updates the drain is still awaiting on (parallel tool
    /// tasks stream updates inside the drain window).
    #[tokio::test(flavor = "current_thread")]
    async fn updates_emitted_during_drain_keep_producer_order() {
        let (inner, mut rx) = mpsc::channel(1);
        let tx = AgentEventTx {
            inner,
            overflow: Arc::new(std::sync::Mutex::new(UpdateOverflow::default())),
            drain_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let tag = |e: &AgentEvent| -> String {
            match e {
                AgentEvent::MessageUpdate {
                    message: AgentMessage::Assistant(a),
                    ..
                } => a.text(),
                AgentEvent::MessageStart {
                    message: AgentMessage::User(u),
                    ..
                } => match &u.content {
                    tack_ai::UserContent::Text(t) => t.clone(),
                    _ => panic!("unexpected user content"),
                },
                other => panic!("unexpected event: {other:?}"),
            }
        };

        // Fill the single channel slot; two updates overflow.
        emit_update(&tx, boundary_update(90, "filler"));
        emit_update(&tx, boundary_update(0, "old1"));
        emit_update(&tx, boundary_update(1, "old2"));
        assert_eq!(tx.overflow.lock().unwrap().updates.len(), 2);

        // Start a must-deliver send: it claims the drain, takes the
        // mailbox, and pends on the full channel.
        let tx2 = tx.clone();
        let sender = tokio::spawn(async move {
            emit(&tx2, must_deliver("end")).await;
        });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(tx.overflow.lock().unwrap().draining);
        assert!(tx.overflow.lock().unwrap().updates.is_empty());

        // Free the channel slot but do NOT yield: the drain has not sent
        // old1 yet. A new update must join the mailbox (with the bug it
        // try_send-ed into the free slot, landing BEFORE old1).
        assert_eq!(tag(&rx.recv().await.unwrap()), "filler");
        emit_update(&tx, boundary_update(2, "new"));
        assert_eq!(
            tx.overflow.lock().unwrap().updates.len(),
            1,
            "update during drain must not bypass the mailbox"
        );

        // Producer order preserved: old1, old2, then the drain-window
        // update, then the must-deliver event.
        assert_eq!(tag(&rx.recv().await.unwrap()), "old1");
        assert_eq!(tag(&rx.recv().await.unwrap()), "old2");
        assert_eq!(tag(&rx.recv().await.unwrap()), "new");
        assert_eq!(tag(&rx.recv().await.unwrap()), "end");
        sender.await.unwrap();
    }

    /// Two concurrent must-deliver emits (parallel tool tasks ending
    /// together): the second must wait for the first's drain, not observe
    /// the mailbox empty mid-drain and overtake the pending updates.
    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_emits_do_not_interleave_drained_updates() {
        let (inner, mut rx) = mpsc::channel(1);
        let tx = AgentEventTx {
            inner,
            overflow: Arc::new(std::sync::Mutex::new(UpdateOverflow::default())),
            drain_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let tag = |e: &AgentEvent| -> String {
            match e {
                AgentEvent::MessageUpdate {
                    message: AgentMessage::Assistant(a),
                    ..
                } => a.text(),
                AgentEvent::MessageStart {
                    message: AgentMessage::User(u),
                    ..
                } => match &u.content {
                    tack_ai::UserContent::Text(t) => t.clone(),
                    _ => panic!("unexpected user content"),
                },
                other => panic!("unexpected event: {other:?}"),
            }
        };

        // Channel full; two updates sit in the mailbox.
        emit_update(&tx, boundary_update(90, "filler"));
        emit_update(&tx, boundary_update(0, "u1"));
        emit_update(&tx, boundary_update(1, "u2"));

        // Emit A claims the drain lock and pends on the full channel.
        let tx_a = tx.clone();
        let emit_a = tokio::spawn(async move {
            emit(&tx_a, must_deliver("end1")).await;
        });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        // Emit B must block on the drain lock (not overtake u1/u2).
        let tx_b = tx.clone();
        let emit_b = tokio::spawn(async move {
            emit(&tx_b, must_deliver("end2")).await;
        });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Producer order: filler, u1, u2, end1, end2 — end2 never lands
        // between the drained updates and end1.
        assert_eq!(tag(&rx.recv().await.unwrap()), "filler");
        assert_eq!(tag(&rx.recv().await.unwrap()), "u1");
        assert_eq!(tag(&rx.recv().await.unwrap()), "u2");
        assert_eq!(tag(&rx.recv().await.unwrap()), "end1");
        assert_eq!(tag(&rx.recv().await.unwrap()), "end2");
        emit_a.await.unwrap();
        emit_b.await.unwrap();
    }
}
