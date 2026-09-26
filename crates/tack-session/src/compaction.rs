//! Context compaction. Port of
//! `packages/coding-agent/src/core/compaction/compaction.ts` (+ `utils.ts`),
//! including the summarization prompts verbatim.

use std::collections::HashSet;
use std::sync::Arc;

use tack_agent_core::AgentMessage;
use tack_ai::{
    ContentBlock, Context, Message, Provider, StopReason, StreamOptions, Usage, UserContent,
    now_millis,
};
use tokio_util::sync::CancellationToken;

use crate::context::{build_session_context, session_entry_to_context_messages};
use crate::entry::SessionEntry;

// ============================================================================
// Settings
// ============================================================================

#[derive(Clone, Copy, Debug)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    /// Goal recitation: after a compaction, append the summary's
    /// "## Goal" / "## Next Steps" sections as a trailing user message
    /// on the LLM-bound context copy, keeping the objective in the
    /// recency-biased attention zone. Append-only, so prompt-cache
    /// friendly. Session files are never touched.
    pub goal_recitation: bool,
    /// Never compact away all but fewer than this many turn-start
    /// entries: a token-dense tail (huge tool results) could otherwise
    /// leave the model with near-zero usable history after compaction.
    pub min_kept_turns: usize,
}

pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16384,
    keep_recent_tokens: 20000,
    goal_recitation: true,
    min_kept_turns: 2,
};

// ============================================================================
// Token estimation
// ============================================================================

const ESTIMATED_IMAGE_CHARS: usize = 4800;

pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn estimate_user_content_chars(content: &UserContent) -> usize {
    match content {
        UserContent::Text(s) => s.chars().count(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                tack_ai::InputContentBlock::Text { text, .. } => text.chars().count(),
                tack_ai::InputContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
    }
}

/// chars/4 heuristic (conservative overestimate), matching TS estimateTokens.
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    let chars = match message {
        AgentMessage::User(m) => estimate_user_content_chars(&m.content),
        AgentMessage::Assistant(a) => a
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text, .. } => text.chars().count(),
                ContentBlock::Thinking { thinking, .. } => thinking.chars().count(),
                ContentBlock::ToolCall {
                    name, arguments, ..
                } => name.chars().count() + arguments.to_string().chars().count(),
                ContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
        AgentMessage::ToolResult(t) => t
            .content
            .iter()
            .map(|b| match b {
                tack_ai::InputContentBlock::Text { text, .. } => text.chars().count(),
                tack_ai::InputContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
        AgentMessage::Custom(c) => estimate_user_content_chars(&c.content),
        AgentMessage::BashExecution(b) => b.command.chars().count() + b.output.chars().count(),
        AgentMessage::BranchSummary(b) => b.summary.chars().count(),
        // Upstream estimateTokens has no system case: 0.
        AgentMessage::System(_) => 0,
        AgentMessage::CompactionSummary(c) => c.summary.chars().count(),
    };
    chars.div_ceil(4) as u64
}

fn assistant_usage(message: &AgentMessage) -> Option<&Usage> {
    let AgentMessage::Assistant(a) = message else {
        return None;
    };
    if matches!(a.stop_reason, StopReason::Aborted | StopReason::Error) {
        return None;
    }
    if calculate_context_tokens(&a.usage) == 0 {
        return None;
    }
    Some(&a.usage)
}

#[derive(Debug)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

/// Estimate context tokens from messages, using the last assistant usage when
/// available (pi's estimateContextTokens).
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let usage_info = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, m)| assistant_usage(m).map(|u| (i, u)));

    match usage_info {
        None => {
            let estimated = messages.iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: estimated,
                usage_tokens: 0,
                trailing_tokens: estimated,
                last_usage_index: None,
            }
        }
        Some((index, usage)) => {
            let usage_tokens = calculate_context_tokens(usage);
            let trailing_tokens: u64 = messages[index + 1..].iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: usage_tokens + trailing_tokens,
                usage_tokens,
                trailing_tokens,
                last_usage_index: Some(index),
            }
        }
    }
}

pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

// ============================================================================
// Cut point detection
// ============================================================================

fn is_cut_point_message(message: &AgentMessage) -> bool {
    // Upstream findValidCutPoints: user/assistant/custom/bashExecution/
    // branchSummary/compactionSummary are cut points; toolResult and
    // system (#9548) never are.
    !matches!(
        message,
        AgentMessage::ToolResult(_) | AgentMessage::System(_)
    )
}

fn is_turn_start_message(message: &AgentMessage) -> bool {
    matches!(
        message,
        AgentMessage::User(_)
            | AgentMessage::BashExecution(_)
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary(_)
            | AgentMessage::CompactionSummary(_)
    )
}

fn is_turn_start_entry(entry: &SessionEntry) -> bool {
    if matches!(entry, SessionEntry::Compaction { .. }) {
        return false;
    }
    session_entry_to_context_messages(entry)
        .iter()
        .any(is_turn_start_message)
}

fn find_valid_cut_points(entries: &[SessionEntry], start: usize, end: usize) -> Vec<usize> {
    (start..end)
        .filter(|&i| {
            !matches!(entries[i], SessionEntry::Compaction { .. })
                && session_entry_to_context_messages(&entries[i])
                    .iter()
                    .any(is_cut_point_message)
        })
        .collect()
}

pub fn find_turn_start_index(entries: &[SessionEntry], entry_index: usize, start: usize) -> i64 {
    for i in (start..=entry_index).rev() {
        if is_turn_start_entry(&entries[i]) {
            return i as i64;
        }
    }
    -1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutPoint {
    pub first_kept_entry_index: usize,
    pub turn_start_index: i64,
    pub is_split_turn: bool,
}

/// Find the cut point keeping approximately `keep_recent_tokens` (pi's
/// findCutPoint). Never cuts at tool results. Guarantees at least
/// `min_kept_turns` turn-start entries stay in the kept tail (0 = no
/// guarantee, pi's original behavior).
pub fn find_cut_point(
    entries: &[SessionEntry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
    min_kept_turns: usize,
) -> CutPoint {
    let cut_points = find_valid_cut_points(entries, start_index, end_index);
    if cut_points.is_empty() {
        return CutPoint {
            first_kept_entry_index: start_index,
            turn_start_index: -1,
            is_split_turn: false,
        };
    }

    let mut accumulated = 0u64;
    let mut cut_index = cut_points[0];

    for i in (start_index..end_index).rev() {
        let tokens: u64 = session_entry_to_context_messages(&entries[i])
            .iter()
            .map(estimate_tokens)
            .sum();
        if tokens == 0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_recent_tokens {
            // Prefer the closest valid cut point at or after this entry. If
            // trailing tool results exceed the budget by themselves, fall
            // back to the LAST cut point (their preceding assistant tool
            // call) instead of the first message, so older history still
            // compacts before the next provider request (pi #9740).
            cut_index = cut_points
                .iter()
                .copied()
                .find(|&c| c >= i)
                .unwrap_or(cut_points[cut_points.len() - 1]);
            break;
        }
    }

    // Pull in adjacent metadata entries that don't affect context.
    while cut_index > start_index {
        let prev = &entries[cut_index - 1];
        if matches!(prev, SessionEntry::Compaction { .. })
            || !session_entry_to_context_messages(prev).is_empty()
        {
            break;
        }
        cut_index -= 1;
    }

    // Minimum kept turns: walk the cut point earlier until the kept tail
    // holds at least `min_kept_turns` turn-start entries.
    if min_kept_turns > 0 {
        while entries[cut_index..end_index]
            .iter()
            .filter(|e| is_turn_start_entry(e))
            .count()
            < min_kept_turns
        {
            let Some(&earlier) = cut_points.iter().rev().find(|&&c| c < cut_index) else {
                break;
            };
            cut_index = earlier;
        }
    }

    let starts_turn = is_turn_start_entry(&entries[cut_index]);
    let turn_start_index = if starts_turn {
        -1
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };

    CutPoint {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index != -1,
    }
}

// ============================================================================
// File operation tracking (utils.ts)
// ============================================================================

#[derive(Clone, Debug, Default)]
pub struct FileOperations {
    pub read: HashSet<String>,
    pub written: HashSet<String>,
    pub edited: HashSet<String>,
}

pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    let AgentMessage::Assistant(a) = message else {
        return;
    };
    for (.., name, args) in a.tool_calls() {
        let Some(path) = args.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        match name {
            "read" => {
                file_ops.read.insert(path.to_string());
            }
            "write" => {
                file_ops.written.insert(path.to_string());
            }
            "edit" => {
                file_ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: HashSet<&String> = file_ops.edited.union(&file_ops.written).collect();
    let mut read_only: Vec<String> = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(f))
        .cloned()
        .collect();
    read_only.sort();
    let mut modified_files: Vec<String> = modified.into_iter().cloned().collect();
    modified_files.sort();
    (read_only, modified_files)
}

pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

// ============================================================================
// Serialization for summarization (utils.ts)
// ============================================================================

const TOOL_RESULT_MAX_CHARS: usize = 2000;

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    let remaining = text.chars().count() - max_chars;
    format!("{truncated}\n\n[... {remaining} more characters truncated]")
}

fn content_text(blocks: &[tack_ai::InputContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Serialize LLM messages to text for summarization (prevents the model from
/// treating it as a conversation to continue).
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        match msg {
            Message::User(u) => {
                let content = match &u.content {
                    UserContent::Text(t) => t.clone(),
                    UserContent::Blocks(b) => content_text(b),
                };
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Message::Assistant(a) => {
                let thinking: Vec<&str> = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect();
                let tool_calls: Vec<String> = a
                    .tool_calls()
                    .map(|(_, name, args)| {
                        let args_str = args
                            .as_object()
                            .map(|obj| {
                                obj.iter()
                                    .map(|(k, v)| format!("{k}={v}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        format!("{name}({args_str})")
                    })
                    .collect();

                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                let text = a.text();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(t) => {
                let content = content_text(&t.content);
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            // Upstream serializeConversation has no system branch: system
            // messages are skipped in the summarization input.
            Message::System(_) => {}
        }
    }

    parts.join("\n\n")
}

// ============================================================================
// Summarization prompts (verbatim from compaction.ts)
// ============================================================================

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix."#;

// ============================================================================
// Summarization calls
// ============================================================================

fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write_1h: match (first.cache_write_1h, second.cache_write_1h) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        },
        reasoning: match (first.reasoning, second.reasoning) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        },
        total_tokens: first.total_tokens + second.total_tokens,
        cost: tack_ai::UsageCost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
        },
    }
}

pub(crate) fn build_summarization_context(prompt_text: &str) -> Context {
    Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::user(UserContent::Blocks(vec![
            tack_ai::InputContentBlock::text(prompt_text),
        ]))],
        tools: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn complete_summarization(
    model: &tack_ai::Model,
    provider: &Arc<dyn Provider>,
    context: &Context,
    max_tokens: u64,
    auth: &tack_ai::oauth::ResolvedAuth,
    reasoning: Option<tack_ai::ThinkingLevel>,
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<tack_ai::AssistantMessage, String> {
    let options = StreamOptions {
        max_tokens: Some(max_tokens.min(u32::MAX as u64) as u32),
        api_key: auth.api_key.clone(),
        headers: auth.headers.clone(),
        // Avoid cache writes for one-off summaries.
        cache_retention: Some(tack_ai::CacheRetention::None),
        session_id: session_id.map(str::to_string),
        tool_choice: Some(tack_ai::ToolChoice::None),
        reasoning,
        cancel: cancel.clone(),
        ..Default::default()
    };
    // Summaries are critical path for compaction; retry transient failures
    // with the default policy (pi's retryAssistantCall semantics).
    let provider = provider.clone();
    let model = model.clone();
    let context = context.clone();
    let response = tack_ai::retry::retry_assistant_call(
        move || {
            let provider = provider.clone();
            let model = model.clone();
            let context = context.clone();
            let options = options.clone();
            async move { provider.complete(&model, &context, options).await }
        },
        tack_ai::retry::RetryPolicy::default(),
        cancel.clone(),
        Default::default(),
    )
    .await;
    if response.stop_reason == StopReason::Error {
        return Err(format!(
            "Summarization failed: {}",
            response.error_message.as_deref().unwrap_or("Unknown error")
        ));
    }
    if response.has_tool_calls() {
        return Err("Summarization attempted to call a tool".to_string());
    }
    Ok(response)
}

struct SummaryWithUsage {
    text: String,
    usage: Usage,
}

#[allow(clippy::too_many_arguments)]
async fn generate_summary_with_usage(
    current_messages: &[AgentMessage],
    model: &tack_ai::Model,
    provider: &Arc<dyn Provider>,
    reserve_tokens: u64,
    auth: &tack_ai::oauth::ResolvedAuth,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    reasoning: Option<tack_ai::ThinkingLevel>,
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<SummaryWithUsage, String> {
    let max_tokens = (0.8 * reserve_tokens as f64).floor() as u64;
    let max_tokens = if model.max_tokens > 0 {
        max_tokens.min(model.max_tokens as u64)
    } else {
        max_tokens
    };

    let mut base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    }
    .to_string();
    if let Some(custom) = custom_instructions {
        base_prompt = format!("{base_prompt}\n\nAdditional focus: {custom}");
    }

    let llm_messages = AgentMessage::default_convert_to_llm(current_messages);
    let conversation_text = serialize_conversation(&llm_messages);

    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(prev) = previous_summary {
        prompt_text.push_str(&format!(
            "<previous-summary>\n{prev}\n</previous-summary>\n\n"
        ));
    }
    prompt_text.push_str(&base_prompt);

    let response = complete_summarization(
        model,
        provider,
        &build_summarization_context(&prompt_text),
        max_tokens,
        auth,
        reasoning,
        session_id,
        cancel,
    )
    .await?;

    Ok(SummaryWithUsage {
        text: response.text(),
        usage: response.usage,
    })
}

#[allow(clippy::too_many_arguments)]
async fn generate_turn_prefix_summary(
    messages: &[AgentMessage],
    model: &tack_ai::Model,
    provider: &Arc<dyn Provider>,
    reserve_tokens: u64,
    auth: &tack_ai::oauth::ResolvedAuth,
    reasoning: Option<tack_ai::ThinkingLevel>,
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<SummaryWithUsage, String> {
    let max_tokens = (0.5 * reserve_tokens as f64).floor() as u64;
    let max_tokens = if model.max_tokens > 0 {
        max_tokens.min(model.max_tokens as u64)
    } else {
        max_tokens
    };

    let llm_messages = AgentMessage::default_convert_to_llm(messages);
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );

    let response = complete_summarization(
        model,
        provider,
        &build_summarization_context(&prompt_text),
        max_tokens,
        auth,
        reasoning,
        session_id,
        cancel,
    )
    .await?;

    Ok(SummaryWithUsage {
        text: response.text(),
        usage: response.usage,
    })
}

// ============================================================================
// Preparation + main entry
// ============================================================================

#[derive(Debug)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<AgentMessage>,
    pub turn_prefix_messages: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

fn message_from_entry_for_compaction(entry: &SessionEntry) -> Option<AgentMessage> {
    if matches!(entry, SessionEntry::Compaction { .. }) {
        return None;
    }
    let message = session_entry_to_context_messages(entry)
        .into_iter()
        .next()?;
    // Upstream #9548: system messages are prompt state, not conversation —
    // the compaction entry's systemMessage carries their replay, so they
    // are never summarized.
    if matches!(message, AgentMessage::System(_)) {
        return None;
    }
    Some(message)
}

/// Materialize a compaction retained tail from the kept entries (tack
/// checkpoint form). Transcript system messages are excluded — the
/// compaction entry's `systemMessage` carries their replay (upstream
/// #9548), and duplicating them in the tail would double-apply updates.
pub fn retained_tail_from_kept_entries(kept: &[SessionEntry]) -> Vec<AgentMessage> {
    kept.iter()
        .flat_map(session_entry_to_context_messages)
        .filter(|m| !matches!(m, AgentMessage::System(_)))
        .collect()
}

/// pi's prepareCompaction: analyze the current path and decide what to
/// summarize. Returns None when compaction is not applicable.
pub fn prepare_compaction(
    path_entries: &[SessionEntry],
    settings: &CompactionSettings,
) -> Option<CompactionPreparation> {
    if path_entries.is_empty() {
        return None;
    }
    if path_entries
        .last()
        .is_some_and(|e| matches!(e, SessionEntry::Compaction { .. }))
    {
        return None;
    }

    let prev_compaction_index = path_entries
        .iter()
        .rposition(|e| matches!(e, SessionEntry::Compaction { .. }));

    let mut previous_summary: Option<String> = None;
    let mut boundary_start = 0usize;
    if let Some(index) = prev_compaction_index {
        let SessionEntry::Compaction {
            summary,
            first_kept_entry_id,
            details,
            from_hook,
            ..
        } = &path_entries[index]
        else {
            unreachable!()
        };
        previous_summary = Some(summary.clone());
        boundary_start = first_kept_entry_id
            .as_ref()
            .and_then(|id| path_entries.iter().position(|e| e.id() == id))
            .unwrap_or(index + 1);
        // Pull forward file ops from the previous pi-generated compaction.
        let _ = (details, from_hook);
    }
    let boundary_end = path_entries.len();

    let tokens_before =
        estimate_context_tokens(&build_session_context(path_entries, None).messages).tokens;

    // Everything derived from a cut point, so the min_kept_turns
    // fallback below can recompute it in one call.
    let collect = |cut_point: &CutPoint| {
        let first_kept_entry_id = path_entries[cut_point.first_kept_entry_index]
            .id()
            .to_string();
        let history_end = if cut_point.is_split_turn {
            cut_point.turn_start_index as usize
        } else {
            cut_point.first_kept_entry_index
        };
        let mut messages_to_summarize: Vec<AgentMessage> = Vec::new();
        for entry in &path_entries[boundary_start..history_end] {
            if let Some(msg) = message_from_entry_for_compaction(entry) {
                messages_to_summarize.push(msg);
            }
        }
        let mut turn_prefix_messages: Vec<AgentMessage> = Vec::new();
        if cut_point.is_split_turn {
            for entry in
                &path_entries[cut_point.turn_start_index as usize..cut_point.first_kept_entry_index]
            {
                if let Some(msg) = message_from_entry_for_compaction(entry) {
                    turn_prefix_messages.push(msg);
                }
            }
        }
        (
            first_kept_entry_id,
            messages_to_summarize,
            turn_prefix_messages,
        )
    };

    let mut cut_point = find_cut_point(
        path_entries,
        boundary_start,
        boundary_end,
        settings.keep_recent_tokens,
        settings.min_kept_turns,
    );
    let (mut first_kept_entry_id, mut messages_to_summarize, mut turn_prefix_messages) =
        collect(&cut_point);
    if messages_to_summarize.is_empty()
        && turn_prefix_messages.is_empty()
        && settings.min_kept_turns > 0
    {
        // min_kept_turns suppressed compaction entirely: when the window
        // is full, a thin kept tail beats no compaction at all — the
        // summary preserves history semantically. The floor is a
        // preference, not a hard constraint: retry without it.
        cut_point = find_cut_point(
            path_entries,
            boundary_start,
            boundary_end,
            settings.keep_recent_tokens,
            0,
        );
        (
            first_kept_entry_id,
            messages_to_summarize,
            turn_prefix_messages,
        ) = collect(&cut_point);
    }

    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    // File operations: previous compaction details + extracted from messages.
    let mut file_ops = FileOperations::default();
    if let Some(index) = prev_compaction_index
        && let SessionEntry::Compaction {
            details, from_hook, ..
        } = &path_entries[index]
        && from_hook != &Some(true)
        && let Some(details) = details
    {
        if let Some(read) = details.get("readFiles").and_then(|v| v.as_array()) {
            for f in read.iter().filter_map(|v| v.as_str()) {
                file_ops.read.insert(f.to_string());
            }
        }
        if let Some(modified) = details.get("modifiedFiles").and_then(|v| v.as_array()) {
            for f in modified.iter().filter_map(|v| v.as_str()) {
                file_ops.edited.insert(f.to_string());
            }
        }
    }
    for msg in &messages_to_summarize {
        extract_file_ops_from_message(msg, &mut file_ops);
    }
    if cut_point.is_split_turn {
        for msg in &turn_prefix_messages {
            extract_file_ops_from_message(msg, &mut file_ops);
        }
    }

    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings: *settings,
    })
}

#[derive(Debug)]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub usage: Usage,
    /// { readFiles, modifiedFiles } for the compaction entry details.
    pub details: serde_json::Value,
}

/// pi's compact(): generate the summary (split-turn aware) and the result
/// payload for `SessionManager::append_compaction`.
#[allow(clippy::too_many_arguments)]
pub async fn compact(
    preparation: &CompactionPreparation,
    model: &tack_ai::Model,
    provider: &Arc<dyn Provider>,
    auth: &tack_ai::oauth::ResolvedAuth,
    custom_instructions: Option<&str>,
    reasoning: Option<tack_ai::ThinkingLevel>,
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<CompactionResult, String> {
    let (mut summary, summary_usage) =
        if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
            let mut history_text = "No prior history.".to_string();
            let mut history_usage: Option<Usage> = None;
            if !preparation.messages_to_summarize.is_empty() {
                let history = generate_summary_with_usage(
                    &preparation.messages_to_summarize,
                    model,
                    provider,
                    preparation.settings.reserve_tokens,
                    auth,
                    custom_instructions,
                    preparation.previous_summary.as_deref(),
                    reasoning,
                    session_id,
                    cancel,
                )
                .await?;
                history_text = history.text;
                history_usage = Some(history.usage);
            }
            let turn_prefix = generate_turn_prefix_summary(
                &preparation.turn_prefix_messages,
                model,
                provider,
                preparation.settings.reserve_tokens,
                auth,
                reasoning,
                session_id,
                cancel,
            )
            .await?;
            let merged = format!(
                "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
                turn_prefix.text
            );
            let usage = match history_usage {
                Some(h) => combine_usage(&h, &turn_prefix.usage),
                None => turn_prefix.usage,
            };
            (merged, usage)
        } else {
            let result = generate_summary_with_usage(
                &preparation.messages_to_summarize,
                model,
                provider,
                preparation.settings.reserve_tokens,
                auth,
                custom_instructions,
                preparation.previous_summary.as_deref(),
                reasoning,
                session_id,
                cancel,
            )
            .await?;
            (result.text, result.usage)
        };

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    Ok(CompactionResult {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id.clone(),
        tokens_before: preparation.tokens_before,
        usage: summary_usage,
        details: serde_json::json!({ "readFiles": read_files, "modifiedFiles": modified_files }),
    })
}

/// Timestamp helper for callers constructing retained tails.
pub fn now_millis_u64() -> u64 {
    now_millis()
}

// ============================================================================
// Goal recitation
// ============================================================================

/// Extract a level-2 markdown section ("## <name>") from a summary.
/// Line-anchored so "### In Progress" never matches as "## Goal".
fn extract_section<'a>(markdown: &'a str, name: &str) -> Option<&'a str> {
    let heading = format!("## {name}");
    for (offset, line) in markdown
        .match_indices(&heading)
        .map(|(i, _)| (i, &markdown[i..]))
    {
        let line_start = offset == 0 || markdown.as_bytes()[offset - 1] == b'\n';
        let after = &line[heading.len()..];
        let heading_ends_line = after.is_empty() || after.starts_with('\n');
        if line_start && heading_ends_line {
            let body = after.strip_prefix('\n').unwrap_or(after);
            let end = body.find("\n## ").unwrap_or(body.len());
            return Some(body[..end].trim());
        }
    }
    None
}

/// Goal recitation: after compaction the summary sits far from the tail
/// of the context, where attention is weakest. Build a trailing user
/// message restating the summary's "## Goal" / "## Next Steps" sections
/// so the objective stays in focus. Appended to the LLM-bound context
/// copy only (append-only, so prompt-cache friendly); session files are
/// never touched. None when the summary has no usable Goal section.
pub fn goal_recitation_message(summary: &str) -> Option<AgentMessage> {
    let goal = extract_section(summary, "Goal")?;
    if goal.is_empty() {
        return None;
    }
    let mut text = String::from(
        "[Context was compacted earlier in this session. Restating the active objective \
         from the checkpoint summary so it stays in focus — keep working toward it:]\n\n## Goal\n",
    );
    text.push_str(goal);
    if let Some(steps) = extract_section(summary, "Next Steps")
        && !steps.is_empty()
    {
        text.push_str("\n\n## Next Steps\n");
        text.push_str(steps);
    }
    Some(AgentMessage::User(tack_ai::UserMessage {
        content: tack_ai::UserContent::Text(text),
        timestamp: now_millis(),
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn msg_entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
        SessionEntry::Message {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            message: AgentMessage::user(text),
        }
    }

    /// Regression: an empty path used to panic indexing
    /// `path_entries[cut_point.first_kept_entry_index]`.
    #[test]
    fn empty_path_returns_none_instead_of_panicking() {
        assert!(prepare_compaction(&[], &DEFAULT_COMPACTION_SETTINGS).is_none());
    }

    #[test]
    fn cut_point_splits_history_from_kept_tail() {
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 1,
            goal_recitation: true,
            min_kept_turns: 0,
        };
        let entries = vec![
            msg_entry("a", None, "hello"),
            msg_entry("b", Some("a"), "world"),
        ];
        let prep = prepare_compaction(&entries, &settings).unwrap();
        assert_eq!(prep.first_kept_entry_id, "b");
        assert!(!prep.is_split_turn);
        assert_eq!(prep.messages_to_summarize.len(), 1);
    }

    #[test]
    fn cut_point_respects_min_kept_turns() {
        // keep_recent_tokens tiny: the token rule alone would keep only
        // the last entry; min_kept_turns must pull the cut point back.
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 1,
            goal_recitation: true,
            min_kept_turns: 3,
        };
        let entries = vec![
            msg_entry("a", None, "one"),
            msg_entry("b", Some("a"), "two"),
            msg_entry("c", Some("b"), "three"),
            msg_entry("d", Some("c"), "four"),
        ];
        let prep = prepare_compaction(&entries, &settings).unwrap();
        // 3 turn-start entries must be kept: b, c, d.
        assert_eq!(prep.first_kept_entry_id, "b");
        assert_eq!(prep.messages_to_summarize.len(), 1);
    }

    #[test]
    fn min_kept_turns_falls_back_when_it_would_suppress_compaction() {
        // Asking for more turns than exist: the floor alone would leave
        // nothing to summarize, so it must yield (a thin kept tail beats
        // an overflowing window) rather than suppress compaction forever.
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 1,
            goal_recitation: true,
            min_kept_turns: 50,
        };
        let entries = vec![
            msg_entry("a", None, "one"),
            msg_entry("b", Some("a"), "two"),
        ];
        let prep = prepare_compaction(&entries, &settings).unwrap();
        assert_eq!(prep.first_kept_entry_id, "b");
        assert_eq!(prep.messages_to_summarize.len(), 1);
    }

    fn assistant_entry(
        id: &str,
        parent: Option<&str>,
        content: Vec<ContentBlock>,
        stop_reason: StopReason,
    ) -> SessionEntry {
        SessionEntry::Message {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            message: AgentMessage::Assistant(tack_ai::AssistantMessage {
                content,
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test".to_string(),
                response_model: None,
                response_id: None,
                provider_thinking_level: None,
                diagnostics: None,
                usage: Usage::zero(),
                stop_reason,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 0,
            }),
        }
    }

    fn tool_result_entry(id: &str, parent: Option<&str>, text: String) -> SessionEntry {
        SessionEntry::Message {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            message: AgentMessage::ToolResult(tack_ai::ToolResultMessage {
                tool_call_id: "call-1".to_string(),
                tool_name: "read".to_string(),
                content: vec![tack_ai::InputContentBlock::text(text)],
                details: None,
                usage: None,
                is_error: false,
                timestamp: 0,
            }),
        }
    }

    /// Regression for pi #9740: a trailing tool result that exceeds
    /// `keep_recent_tokens` on its own must NOT collapse the cut point to
    /// the first message; fall back to the last valid cut point (the
    /// preceding assistant tool call) so older history still compacts.
    #[test]
    fn oversized_trailing_tool_result_falls_back_to_last_cut_point() {
        let entries = vec![
            msg_entry("a", None, "old history"),
            assistant_entry(
                "b",
                Some("a"),
                vec![ContentBlock::text("old answer")],
                StopReason::Stop,
            ),
            msg_entry("c", Some("b"), "read the large file"),
            assistant_entry(
                "d",
                Some("c"),
                vec![ContentBlock::ToolCall {
                    id: "call-1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({"path": "big.txt"}),
                    thought_signature: None,
                    namespace: None,
                }],
                StopReason::ToolUse,
            ),
            tool_result_entry("e", Some("d"), "x".repeat(8000)),
        ];

        let cut = find_cut_point(&entries, 0, entries.len(), 1000, 0);
        assert_eq!(cut.first_kept_entry_index, 3);
        assert_eq!(cut.turn_start_index, 2);
        assert!(cut.is_split_turn);

        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 1000,
            goal_recitation: true,
            min_kept_turns: 0,
        };
        let prep = prepare_compaction(&entries, &settings).unwrap();
        assert_eq!(prep.first_kept_entry_id, "d");
        assert!(prep.is_split_turn);
        assert_eq!(prep.messages_to_summarize.len(), 2);
        assert_eq!(prep.turn_prefix_messages.len(), 1);
    }

    #[test]
    fn everything_kept_returns_none() {
        // Nothing crosses keep_recent_tokens: no history to summarize.
        let entries = vec![msg_entry("a", None, "hi")];
        assert!(prepare_compaction(&entries, &DEFAULT_COMPACTION_SETTINGS).is_none());
    }

    #[test]
    fn goal_recitation_extracts_goal_and_next_steps() {
        let summary = "## Goal\nFix the flaky compaction test\n\n## Constraints & Preferences\n\
            - keep it simple\n\n## Next Steps\n1. reproduce\n2. fix\n\n## Key Decisions\n- none";
        let Some(AgentMessage::User(msg)) = goal_recitation_message(summary) else {
            panic!("expected a recitation user message")
        };
        let tack_ai::UserContent::Text(text) = msg.content else {
            panic!("expected text content")
        };
        assert!(
            text.contains("## Goal\nFix the flaky compaction test"),
            "{text}"
        );
        assert!(
            text.contains("## Next Steps\n1. reproduce\n2. fix"),
            "{text}"
        );
        assert!(!text.contains("Key Decisions"), "{text}");
        assert!(!text.contains("Constraints"), "{text}");
    }

    #[test]
    fn goal_recitation_ignores_level3_headings_and_empty_goals() {
        // "### Goalkeepers" must not be parsed as the Goal section.
        assert!(goal_recitation_message("### Goalkeepers\nx").is_none());
        assert!(goal_recitation_message("## Goal\n\n## Next Steps\n1. x").is_none());
        assert!(goal_recitation_message("no sections at all").is_none());
    }
}
