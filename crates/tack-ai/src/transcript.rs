//! Transcript replay helpers. Port of `packages/ai/src/utils/transcript.ts`
//! and `packages/ai/src/utils/text.ts`: system messages carry the system
//! prompt and tool declarations inside the transcript; replaying every
//! system message in order yields the current prompt and tools.
//!
//! The replay helpers are generic over [`TranscriptMessage`] so both
//! `tack_ai::Message` and the agent loop's wider `AgentMessage` (a superset
//! with UI-only roles) can be replayed without conversion, mirroring the TS
//! `TranscriptMessages = readonly { role: string }[]` contract.
//!
//! Fidelity notes vs the TS implementation:
//! - Tool/section collections keep JS `Map` semantics (first-insertion
//!   position, replace-in-place) via vectors, so replayed tool order matches
//!   upstream byte-for-byte.
//! - `declarations_equal` compares semantic JSON equality (serde_json
//!   normalizes object key order) instead of TS's same-construction
//!   `JSON.stringify` comparison; results agree for structurally equal
//!   declarations.

use crate::types::{
    Context, InputContentBlock, Message, SystemMessage, ToolDefinition, ToolReference, UserContent,
};

/// Any message list entry with a system-role view (TS `TranscriptMessages`
/// item). Implemented by `Message` here and by `AgentMessage` in
/// tack-agent-core.
pub trait TranscriptMessage {
    /// The system message payload, or `None` for other roles.
    fn as_system(&self) -> Option<&SystemMessage>;
}

impl TranscriptMessage for Message {
    fn as_system(&self) -> Option<&SystemMessage> {
        Message::as_system(self)
    }
}

// ---------------------------------------------------------------------------
// text.ts
// ---------------------------------------------------------------------------

/// Extract and join text from message content (TS `contentText`).
pub fn content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                InputContentBlock::Text { text, .. } => Some(text.as_str()),
                InputContentBlock::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Render a system message as a complete prompt: its content followed by its
/// sections (TS `getSystemMessageText`).
pub fn get_system_message_text(message: &SystemMessage) -> String {
    let mut parts = vec![content_text(&message.content)];
    if let Some(sections) = &message.sections {
        parts.extend(sections.values().flatten().cloned());
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render a later system message for APIs that accept system messages
/// mid-conversation (TS `renderSystemMessageUpdate`). Section changes are
/// framed by name so the model can relate them to the leading prompt.
pub fn render_system_message_update(message: &SystemMessage) -> String {
    let mut parts: Vec<String> = Vec::new();
    let text = content_text(&message.content);
    if !text.is_empty() {
        parts.push(text);
    }
    if let Some(sections) = &message.sections {
        for (name, value) in sections {
            parts.push(match value {
                None => format!("Removed system prompt section \"{name}\"."),
                Some(value) => {
                    format!("Updated system prompt section \"{name}\":\n\n{value}")
                }
            });
        }
    }
    parts.join("\n\n")
}

// ---------------------------------------------------------------------------
// transcript.ts — construction
// ---------------------------------------------------------------------------

/// Build the leading system message for a prompt and tool set (TS
/// `createInitialSystemMessage`). Returns `None` when both are empty, so an
/// empty transcript stays empty.
pub fn create_initial_system_message(
    system_prompt: Option<&str>,
    tools: Option<&[ToolDefinition]>,
) -> Option<SystemMessage> {
    let has_system_prompt = system_prompt.is_some_and(|p| !p.is_empty());
    let has_tools = tools.is_some_and(|t| !t.is_empty());
    if !has_system_prompt && !has_tools {
        return None;
    }
    Some(SystemMessage {
        content: UserContent::Text(system_prompt.unwrap_or("").to_string()),
        sections: None,
        tools_added: has_tools.then(|| tools.expect("checked").to_vec()),
        tools_removed: None,
        timestamp: 0,
    })
}

/// Fold `Context.system_prompt` and `Context.tools` into a leading system
/// message (TS `normalizeContext`): returns the full transcript with the
/// leading message prepended (or the messages unchanged when both are
/// empty).
pub fn normalize_context(context: &Context) -> Vec<Message> {
    let initial = create_initial_system_message(
        context.system_prompt.as_deref(),
        if context.tools.is_empty() {
            None
        } else {
            Some(context.tools.as_slice())
        },
    );
    match initial {
        Some(initial) => {
            let mut messages = Vec::with_capacity(context.messages.len() + 1);
            messages.push(Message::System(initial));
            messages.extend(context.messages.iter().cloned());
            messages
        }
        None => context.messages.clone(),
    }
}

// ---------------------------------------------------------------------------
// transcript.ts — replay
// ---------------------------------------------------------------------------

/// Return the leading system message, if the transcript starts with one (TS
/// `getInitialSystemMessage`).
pub fn get_initial_system_message<T: TranscriptMessage>(messages: &[T]) -> Option<&SystemMessage> {
    messages.first().and_then(TranscriptMessage::as_system)
}

/// Drop the leading system message for APIs that carry the prompt outside
/// the message list (TS `withoutInitialSystemMessage`).
pub fn without_initial_system_message(messages: &[Message]) -> &[Message] {
    if get_initial_system_message(messages).is_some() {
        &messages[1..]
    } else {
        messages
    }
}

/// Resolve the tools available after applying every transcript delta in
/// order (TS `getCurrentTools`). JS `Map` semantics: additions replace
/// same-name declarations in place, keeping first-insertion order.
pub fn get_current_tools<T: TranscriptMessage>(messages: &[T]) -> Vec<ToolDefinition> {
    replay_tools(messages.iter().filter_map(TranscriptMessage::as_system))
}

/// Iterator form of [`get_current_tools`]: replays tool deltas over chained
/// sources without materializing a combined transcript.
pub fn replay_tools<'a>(systems: impl Iterator<Item = &'a SystemMessage>) -> Vec<ToolDefinition> {
    let mut tools: Vec<ToolDefinition> = Vec::new();
    for system in systems {
        if let Some(removed) = &system.tools_removed {
            tools.retain(|tool| !removed.iter().any(|r| r.name == tool.name));
        }
        if let Some(added) = &system.tools_added {
            for tool in added {
                match tools.iter_mut().find(|t| t.name == tool.name) {
                    Some(existing) => *existing = tool.clone(),
                    None => tools.push(tool.clone()),
                }
            }
        }
    }
    tools
}

/// Replay every system message into one leading system message holding the
/// current prompt and tools (TS `getCurrentSystemMessage`). Later `content`
/// is appended to the base prompt, `sections` are patched by name, and tools
/// are resolved with [`get_current_tools`].
pub fn get_current_system_message<T: TranscriptMessage>(messages: &[T]) -> Option<SystemMessage> {
    let mut content: Vec<String> = Vec::new();
    // (name, text) with JS Map semantics: set replaces in place, null deletes.
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut timestamp: Option<u64> = None;
    for message in messages {
        let Some(system) = message.as_system() else {
            continue;
        };
        timestamp.get_or_insert(system.timestamp);
        let text = content_text(&system.content);
        if !text.is_empty() {
            content.push(text);
        }
        if let Some(message_sections) = &system.sections {
            for (name, value) in message_sections {
                match value {
                    None => sections.retain(|(n, _)| n != name),
                    Some(value) => match sections.iter_mut().find(|(n, _)| n == name) {
                        Some(existing) => existing.1 = value.clone(),
                        None => sections.push((name.clone(), value.clone())),
                    },
                }
            }
        }
    }
    let tools = get_current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: UserContent::Text(content.join("\n\n")),
        sections: (!sections.is_empty())
            .then(|| sections.into_iter().map(|(k, v)| (k, Some(v))).collect()),
        tools_added: (!tools.is_empty()).then_some(tools),
        tools_removed: None,
        timestamp: timestamp.unwrap_or(0),
    })
}

/// Render the current system prompt text after replaying every system
/// message (TS `getCurrentSystemPrompt`).
pub fn get_current_system_prompt<T: TranscriptMessage>(messages: &[T]) -> String {
    match get_current_system_message(messages) {
        Some(message) => get_system_message_text(&message),
        None => String::new(),
    }
}

/// Rebuild the transcript for APIs without mid-conversation system
/// messages: the replayed system message leads, and every later system
/// message is dropped (TS `collapseSystemMessages`).
pub fn collapse_system_messages(messages: &[Message]) -> Vec<Message> {
    let head = get_current_system_message(messages);
    let rest = messages
        .iter()
        .filter(|message| message.as_system().is_none())
        .cloned();
    match head {
        Some(head) => std::iter::once(Message::System(head)).chain(rest).collect(),
        None => rest.collect(),
    }
}

/// Strip runtime-only fields from a tool before transcript comparison or
/// persistence (TS `toToolDeclaration`). Drops tack's `defer_loading`
/// marker; keeps the declaration surface (name/description/parameters/
/// constrained_sampling).
pub fn to_tool_declaration(tool: &ToolDefinition) -> ToolDefinition {
    ToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: tool.parameters.clone(),
        defer_loading: false,
        constrained_sampling: tool.constrained_sampling.clone(),
    }
}

/// Whether two tools declare the same interface to the model (TS
/// `declarationsEqual`). Semantic JSON equality after declaration
/// normalization (see module docs).
pub fn declarations_equal(left: &ToolDefinition, right: &ToolDefinition) -> bool {
    to_tool_declaration(left) == to_tool_declaration(right)
}

/// Tool loadout delta between two complete tool states (TS
/// `ToolStateChanges`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolStateChanges {
    pub tools_added: Vec<ToolDefinition>,
    pub tools_removed: Vec<ToolReference>,
}

/// Compare two complete tool states. A changed definition is a removal
/// followed by an addition (TS `getToolStateChanges`).
pub fn get_tool_state_changes(
    previous: &[ToolDefinition],
    current: &[ToolDefinition],
) -> ToolStateChanges {
    let tools_added = current
        .iter()
        .filter(|tool| {
            previous
                .iter()
                .find(|p| p.name == tool.name)
                .is_none_or(|p| !declarations_equal(p, tool))
        })
        .map(to_tool_declaration)
        .collect();
    let tools_removed = previous
        .iter()
        .filter(|tool| {
            current
                .iter()
                .find(|c| c.name == tool.name)
                .is_none_or(|c| !declarations_equal(tool, c))
        })
        .map(|tool| ToolReference {
            name: tool.name.clone(),
        })
        .collect();
    ToolStateChanges {
        tools_added,
        tools_removed,
    }
}

/// Copy a system message with its tool fields replaced by `changes`; empty
/// lists omit the field (TS `withToolChanges` in agent-loop.ts).
pub fn with_tool_changes(message: &SystemMessage, changes: &ToolStateChanges) -> SystemMessage {
    let mut next = message.clone();
    next.tools_added = (!changes.tools_added.is_empty()).then(|| changes.tools_added.clone());
    next.tools_removed = (!changes.tools_removed.is_empty()).then(|| changes.tools_removed.clone());
    next
}

/// Every definition referenced by transcript tool state, in
/// first-declaration order (TS `getDeclaredTools`).
pub fn get_declared_tools<T: TranscriptMessage>(messages: &[T]) -> Vec<ToolDefinition> {
    let mut definitions: Vec<ToolDefinition> = Vec::new();
    for message in messages {
        let Some(system) = message.as_system() else {
            continue;
        };
        if let Some(added) = &system.tools_added {
            for tool in added {
                match definitions.iter_mut().find(|d| d.name == tool.name) {
                    Some(existing) => *existing = tool.clone(),
                    None => definitions.push(tool.clone()),
                }
            }
        }
    }
    definitions
}

/// Whether a tool name was declared twice with different definitions (TS
/// `hasToolRedefinitions`). Transports that reference tools by name cannot
/// express that.
pub fn has_tool_redefinitions<T: TranscriptMessage>(messages: &[T]) -> bool {
    let mut declared: Vec<&ToolDefinition> = Vec::new();
    for message in messages {
        let Some(system) = message.as_system() else {
            continue;
        };
        if let Some(added) = &system.tools_added {
            for tool in added {
                match declared.iter().find(|d| d.name == tool.name) {
                    Some(previous) if !declarations_equal(previous, tool) => return true,
                    Some(_) => {}
                    None => declared.push(tool),
                }
            }
        }
    }
    false
}

/// Whether tool history contains a removal or same-name redeclaration that
/// an addition-only transport cannot replay (TS `hasNonAdditiveToolChanges`).
pub fn has_non_additive_tool_changes<T: TranscriptMessage>(messages: &[T]) -> bool {
    let mut declared: Vec<&str> = Vec::new();
    for message in messages {
        let Some(system) = message.as_system() else {
            continue;
        };
        if system.tools_removed.as_ref().is_some_and(|r| !r.is_empty()) {
            return true;
        }
        if let Some(added) = &system.tools_added {
            for tool in added {
                if declared.contains(&tool.name.as_str()) {
                    return true;
                }
                declared.push(tool.name.as_str());
            }
        }
    }
    false
}

/// Split of tool declarations between the top-level request field and
/// in-place additions (TS `TranscriptTools`).
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptTools {
    /// Tools sent in the top-level request field.
    pub request_tools: Vec<ToolDefinition>,
    /// Whether later system messages carry their own `tools_added` as
    /// in-place additions. When false, `request_tools` already holds the
    /// complete current tool set.
    pub anchors_additions: bool,
}

/// Split tool declarations between the top-level request field and in-place
/// additions (TS `resolveTranscriptTools`). Anchoring only works when no
/// tool was removed or redeclared; everything else sends the current list.
pub fn resolve_transcript_tools<T: TranscriptMessage>(
    messages: &[T],
    supports_tool_additions: bool,
) -> TranscriptTools {
    let anchors_additions = supports_tool_additions && !has_non_additive_tool_changes(messages);
    let request_tools = if anchors_additions {
        get_initial_system_message(messages)
            .and_then(|m| m.tools_added.clone())
            .unwrap_or_default()
    } else {
        get_current_tools(messages)
    };
    TranscriptTools {
        request_tools,
        anchors_additions,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: serde_json::json!({"type": "object"}),
            defer_loading: false,
            constrained_sampling: None,
        }
    }

    fn system(content: &str, timestamp: u64) -> SystemMessage {
        SystemMessage {
            content: UserContent::Text(content.to_string()),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp,
        }
    }

    /// TS createInitialSystemMessage: undefined unless prompt or tools are
    /// non-empty; timestamp is 0.
    #[test]
    fn initial_message_empty_when_no_prompt_and_no_tools() {
        assert!(create_initial_system_message(None, None).is_none());
        assert!(create_initial_system_message(Some(""), Some(&[])).is_none());
        let msg = create_initial_system_message(Some("You are helpful."), None).unwrap();
        assert_eq!(msg.timestamp, 0);
        assert!(msg.tools_added.is_none());
        let msg = create_initial_system_message(None, Some(&[tool("a")])).unwrap();
        assert_eq!(content_text(&msg.content), "");
        assert_eq!(msg.tools_added.as_ref().unwrap().len(), 1);
    }

    /// TS getCurrentTools: additions apply in order, removals delete,
    /// re-adds replace in place (JS Map semantics).
    #[test]
    fn current_tools_replays_deltas_in_order() {
        let mut m1 = system("", 1);
        m1.tools_added = Some(vec![tool("a"), tool("b")]);
        let mut m2 = system("", 2);
        m2.tools_removed = Some(vec![ToolReference { name: "a".into() }]);
        let mut m3 = system("", 3);
        let mut b2 = tool("b");
        b2.description = "b v2".to_string();
        m3.tools_added = Some(vec![b2.clone(), tool("c")]);
        let messages = vec![
            Message::System(m1),
            Message::user("hi"),
            Message::System(m2),
            Message::System(m3),
        ];
        let tools = get_current_tools(&messages);
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"],
            "b keeps first-insertion position with the new definition"
        );
        assert_eq!(tools[0].description, "b v2");
    }

    /// TS getCurrentSystemMessage: content joins with blank lines, sections
    /// patch by name (null deletes), timestamp is the first system
    /// message's.
    #[test]
    fn current_system_message_replays_content_and_sections() {
        let mut m1 = system("base prompt", 10);
        m1.sections = Some(
            [("s1".to_string(), Some("section one".to_string()))]
                .into_iter()
                .collect(),
        );
        let mut m2 = system("extra instructions", 20);
        m2.sections = Some(
            [
                ("s1".to_string(), None),
                ("s2".to_string(), Some("section two".to_string())),
            ]
            .into_iter()
            .collect(),
        );
        let messages = vec![
            Message::System(m1),
            Message::user("hi"),
            Message::System(m2),
        ];
        let current = get_current_system_message(&messages).unwrap();
        assert_eq!(current.timestamp, 10);
        assert_eq!(
            content_text(&current.content),
            "base prompt\n\nextra instructions"
        );
        let sections = current.sections.unwrap();
        assert_eq!(sections.get("s1"), None);
        assert_eq!(sections.get("s2"), Some(&Some("section two".to_string())));
        // No system messages at all -> None (unless tools exist).
        assert!(get_current_system_message(&[Message::user("x")]).is_none());
    }

    /// TS getCurrentSystemPrompt renders content followed by sections.
    #[test]
    fn current_prompt_renders_sections_after_content() {
        let mut m1 = system("base", 1);
        m1.sections = Some(
            [("a".to_string(), Some("alpha".to_string()))]
                .into_iter()
                .collect(),
        );
        let messages = vec![Message::System(m1)];
        assert_eq!(get_current_system_prompt(&messages), "base\n\nalpha");
    }

    /// TS collapseSystemMessages: one replayed head, no later system
    /// messages, non-system order preserved.
    #[test]
    fn collapse_rebuilds_single_head() {
        let mut m1 = system("base", 1);
        m1.tools_added = Some(vec![tool("a")]);
        let m2 = system("more", 2);
        let messages = vec![
            Message::System(m1),
            Message::user("one"),
            Message::System(m2),
            Message::user("two"),
        ];
        let collapsed = collapse_system_messages(&messages);
        assert_eq!(collapsed.len(), 3);
        let Message::System(head) = &collapsed[0] else {
            panic!("expected system head, got {:?}", collapsed[0]);
        };
        assert_eq!(content_text(&head.content), "base\n\nmore");
        assert!(matches!(collapsed[1], Message::User(_)));
        assert!(matches!(collapsed[2], Message::User(_)));
    }

    /// TS getToolStateChanges: changed definitions are remove+add.
    #[test]
    fn tool_state_changes_treats_redefinition_as_remove_and_add() {
        let a1 = tool("a");
        let mut a2 = tool("a");
        a2.description = "a v2".to_string();
        let changes = get_tool_state_changes(&[a1, tool("b")], &[a2, tool("c")]);
        assert_eq!(
            changes
                .tools_added
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert_eq!(
            changes
                .tools_removed
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        // No change -> empty.
        let same = get_tool_state_changes(&[tool("x")], &[tool("x")]);
        assert!(same.tools_added.is_empty() && same.tools_removed.is_empty());
    }

    /// TS toToolDeclaration strips the runtime-only defer_loading marker.
    #[test]
    fn declaration_strips_defer_loading() {
        let mut t = tool("d");
        t.defer_loading = true;
        let decl = to_tool_declaration(&t);
        assert!(!decl.defer_loading);
        assert!(declarations_equal(&t, &decl));
    }

    /// TS withToolChanges: empty lists omit the fields entirely.
    #[test]
    fn with_tool_changes_omits_empty_fields() {
        let mut m = system("", 5);
        m.tools_added = Some(vec![tool("a")]);
        let cleared = with_tool_changes(&m, &ToolStateChanges::default());
        assert!(cleared.tools_added.is_none());
        assert!(cleared.tools_removed.is_none());
        let changes = ToolStateChanges {
            tools_added: vec![tool("b")],
            tools_removed: vec![ToolReference { name: "a".into() }],
        };
        let updated = with_tool_changes(&m, &changes);
        assert_eq!(updated.tools_added.unwrap()[0].name, "b");
        assert_eq!(updated.tools_removed.unwrap()[0].name, "a");
    }

    /// TS hasNonAdditiveToolChanges / resolveTranscriptTools anchoring.
    #[test]
    fn transcript_tools_anchor_only_when_purely_additive() {
        let mut m1 = system("", 1);
        m1.tools_added = Some(vec![tool("a")]);
        let mut m2 = system("", 2);
        m2.tools_added = Some(vec![tool("b")]);
        let additive = vec![Message::System(m1.clone()), Message::System(m2)];
        let resolved = resolve_transcript_tools(&additive, true);
        assert!(resolved.anchors_additions);
        assert_eq!(resolved.request_tools.len(), 1, "only the initial set");
        let resolved_off = resolve_transcript_tools(&additive, false);
        assert!(!resolved_off.anchors_additions);
        assert_eq!(resolved_off.request_tools.len(), 2, "current full set");

        let mut m3 = system("", 3);
        m3.tools_removed = Some(vec![ToolReference { name: "a".into() }]);
        let with_removal = vec![Message::System(m1), Message::System(m3)];
        assert!(has_non_additive_tool_changes(&with_removal));
        let resolved = resolve_transcript_tools(&with_removal, true);
        assert!(!resolved.anchors_additions);
        assert!(resolved.request_tools.is_empty());
    }

    /// TS normalizeContext folds systemPrompt + tools into a leading system
    /// message.
    #[test]
    fn normalize_context_folds_prompt_and_tools() {
        let context = Context {
            system_prompt: Some("prompt".to_string()),
            messages: vec![Message::user("hi")],
            tools: vec![tool("a")],
        };
        let messages = normalize_context(&context);
        assert_eq!(messages.len(), 2);
        let Message::System(head) = &messages[0] else {
            panic!("expected leading system message");
        };
        assert_eq!(content_text(&head.content), "prompt");
        assert_eq!(head.tools_added.as_ref().unwrap()[0].name, "a");
        // Empty context stays empty.
        let empty = normalize_context(&Context::default());
        assert!(empty.is_empty());
    }
}
