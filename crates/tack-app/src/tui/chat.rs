//! Chat transcript components: user/assistant/tool/notice entries rendered
//! to styled lines (port of the components in
//! `modes/interactive/components/`).

use tack_ai::{AssistantMessage, ContentBlock, StopReason};
use tack_tui::components::markdown::render_markdown;
use tack_tui::{Line, Span, Style};

use super::stream_md::StreamMarkdownCache;
use super::theme::Theme;

/// Rendering capabilities for media embedded in chat entries (mermaid
/// diagrams → inline images / half-block fallback).
#[derive(Clone, Copy, Debug, Default)]
pub struct Media {
    pub image_protocol: Option<tack_tui::image::ImageProtocol>,
    pub mermaid: bool,
    /// hideThinkingBlock: never render thinking content (streaming or done).
    pub hide_thinking: bool,
    /// ctrl+o expand-all: render completed thinking blocks in full instead
    /// of the "Thought for a while" collapsed label.
    pub expand_thinking: bool,
    /// terminal.imageWidthCells: cap for rendered image width.
    pub image_width_cells: Option<u16>,
}

/// A line in the unified display list: chat entry or tool card (rendered
/// interleaved in event order).
/// ChatEntry dominates the size; tool cards are just an id.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum TranscriptItem {
    Chat(ChatEntry),
    /// Index references the app's tool registry by tool call id.
    Tool(String),
}

/// One entry in the chat transcript.
// Transcript entries are few (one per message); boxing AssistantMessage
// would churn every construction site for no real gain.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum ChatEntry {
    User {
        text: String,
    },
    Assistant {
        message: AssistantMessage,
        streaming: bool,
    },
    Notice {
        text: String,
        kind: NoticeKind,
    },
    Compaction {
        tokens_before: u64,
    },
    /// Auto/manual compaction completed: summary is available.
    CompactionSummary {
        summary: String,
        tokens_before: u64,
    },
    /// A `/skill:<name>` invocation (TS skill-invocation-message).
    SkillInvocation {
        name: String,
        args: String,
    },
    /// Free-form markdown block (changelog, help pages, etc.).
    Markdown {
        text: String,
    },
    /// A user message sitting in the steering/follow-up queue (not yet
    /// delivered to the model). Replaced by `ChatEntry::User` on delivery.
    Queued {
        text: String,
        /// true = follow-up queue (sent when the agent stops); false =
        /// steering (sent at the next turn boundary).
        follow_up: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

impl ChatEntry {
    pub fn notice(text: impl Into<String>, kind: NoticeKind) -> Self {
        ChatEntry::Notice {
            text: text.into(),
            kind,
        }
    }

    /// Render to lines (finalized entries only — streaming entries use
    /// `render_streaming`).
    pub fn render(&self, width: u16, theme: &Theme, media: Media) -> Vec<Line> {
        match self {
            ChatEntry::User { text } => render_user(text, width, theme),
            ChatEntry::Assistant { message, streaming } => {
                render_assistant(message, *streaming, width, theme, media)
            }
            ChatEntry::Notice { text, kind } => {
                let style = match kind {
                    NoticeKind::Info => theme.muted,
                    NoticeKind::Warning => theme.warning,
                    NoticeKind::Error => theme.error,
                };
                let mut lines: Vec<Line> = Line::styled(text.clone(), style).wrap(width as usize);
                lines.push(Line::new());
                lines
            }
            ChatEntry::Compaction { tokens_before } => {
                let mut lines = vec![Line::styled(
                    crate::i18n::trf("chat.compacted", &[("tokens", &tokens_before.to_string())]),
                    theme.muted,
                )];
                lines.push(Line::new());
                lines
            }
            ChatEntry::SkillInvocation { name, args } => {
                let mut line = Line::new();
                line.push(Span::styled(
                    crate::i18n::trf("chat.skill", &[("name", name)]),
                    theme.accent,
                ));
                if !args.is_empty() {
                    line.push(Span::styled(format!("  {args}"), theme.dim));
                }
                vec![line, Line::new()]
            }
            ChatEntry::Markdown { text } => {
                let mut lines = render_markdown(text, width as usize, &theme.markdown);
                lines.push(Line::new());
                lines
            }
            ChatEntry::Queued { text, follow_up } => {
                let when = if *follow_up {
                    crate::i18n::tr("chat.queued_followup")
                } else {
                    crate::i18n::tr("chat.queued_steering")
                };
                let mut lines = vec![Line::styled(
                    crate::i18n::trf("chat.queued", &[("when", &when)]),
                    theme.warning,
                )];
                for wrapped in Line::plain(text.clone()).wrap((width as usize).max(1)) {
                    lines.push(Line::styled(format!("  {}", wrapped.text()), theme.muted));
                }
                lines.push(Line::new());
                lines
            }
            ChatEntry::CompactionSummary {
                summary,
                tokens_before,
            } => {
                let mut lines = vec![Line::styled(
                    crate::i18n::trf(
                        "chat.compacted_summary",
                        &[("tokens", &tokens_before.to_string())],
                    ),
                    theme.accent,
                )];
                lines.extend(
                    render_markdown(summary, width as usize, &theme.markdown)
                        .into_iter()
                        .map(|mut l| {
                            l.spans.insert(0, Span::styled("  ", Style::default()));
                            for span in &mut l.spans {
                                span.style = span.style.merged_with(&theme.muted);
                            }
                            l
                        }),
                );
                lines.push(Line::new());
                lines
            }
        }
    }
}

fn render_user(text: &str, width: u16, theme: &Theme) -> Vec<Line> {
    let inner = (width as usize).saturating_sub(4);
    let mut lines = vec![Line::new()];
    for wrapped in Line::plain(text.to_string()).wrap(inner.max(1)) {
        let mut line = Line::new();
        line.push(Span::styled("  ", theme.user_message_bg));
        line.push(Span::styled(
            wrapped.text(),
            theme.text.merged_with(&theme.user_message_bg),
        ));
        // Fill the row with the background.
        let w = line.width();
        line.push(Span::styled(
            " ".repeat((width as usize).saturating_sub(w)),
            theme.user_message_bg,
        ));
        lines.push(line);
    }
    lines.push(Line::new());
    lines
}

/// Render an assistant message: thinking blocks (collapsed label when done),
/// markdown text, error/abort notices. Tool calls are rendered separately by
/// the app as tool entries.
pub fn render_assistant(
    message: &AssistantMessage,
    streaming: bool,
    width: u16,
    theme: &Theme,
    media: Media,
) -> Vec<Line> {
    render_assistant_inner(message, streaming, width, theme, media, None)
}

/// Streaming variant of [`render_assistant`]: markdown blocks render
/// through the incremental cache (closed prefix reused, only the growing
/// tail re-parses/re-highlights — O(tail) per pass instead of O(total)).
pub fn render_assistant_streaming(
    message: &AssistantMessage,
    width: u16,
    theme: &Theme,
    media: Media,
    cache: &mut StreamMarkdownCache,
) -> Vec<Line> {
    render_assistant_inner(message, true, width, theme, media, Some(cache))
}

fn render_assistant_inner(
    message: &AssistantMessage,
    streaming: bool,
    width: u16,
    theme: &Theme,
    media: Media,
    mut cache: Option<&mut StreamMarkdownCache>,
) -> Vec<Line> {
    let mut mermaid_hook = |source: &str, w: usize| -> Option<Vec<Line>> {
        let width = media
            .image_width_cells
            .map_or(w as u16, |c| c.min(w as u16));
        super::mermaid::mermaid_lines(source, width, media.image_protocol)
    };
    let mut lines = Vec::new();
    for (block_index, block) in message.content.iter().enumerate() {
        match block {
            ContentBlock::Thinking { thinking, .. } => {
                if thinking.trim().is_empty() || media.hide_thinking {
                    continue;
                }
                if streaming || media.expand_thinking {
                    let rendered = match &mut cache {
                        Some(c) => {
                            c.render(block_index, thinking, width as usize, &theme.markdown, None)
                        }
                        None => render_markdown(thinking, width as usize, &theme.markdown),
                    };
                    lines.extend(rendered.into_iter().map(|mut l| {
                        for span in &mut l.spans {
                            span.style = span.style.merged_with(&theme.thinking_text);
                        }
                        l
                    }));
                } else {
                    let chars = thinking.chars().count();
                    lines.push(Line::styled(
                        crate::i18n::trf("chat.thought", &[("chars", &chars.to_string())]),
                        theme.thinking_text,
                    ));
                }
            }
            ContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    continue;
                }
                let rendered = match &mut cache {
                    Some(c) => c.render(
                        block_index,
                        text,
                        width as usize,
                        &theme.markdown,
                        if media.mermaid {
                            Some(&mut mermaid_hook)
                        } else {
                            None
                        },
                    ),
                    None => tack_tui::components::markdown::render_markdown_with(
                        text,
                        width as usize,
                        &theme.markdown,
                        if media.mermaid {
                            Some(&mut mermaid_hook)
                        } else {
                            None
                        },
                    ),
                };
                lines.extend(rendered);
            }
            ContentBlock::ToolCall { .. } => {} // tool entries render separately
            ContentBlock::Image { .. } => {}
        }
    }
    match message.stop_reason {
        StopReason::Error => {
            lines.push(Line::styled(
                format!("✗ {}", message.error_message.as_deref().unwrap_or("error")),
                theme.error,
            ));
        }
        StopReason::Aborted => {
            lines.push(Line::styled(crate::i18n::tr("chat.aborted"), theme.muted));
        }
        StopReason::Length => {
            lines.push(Line::styled(
                crate::i18n::tr("chat.truncated"),
                theme.warning,
            ));
        }
        _ => {}
    }
    if !lines.is_empty() {
        lines.push(Line::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn user_message_newlines_become_separate_rows() {
        // Multi-line editor text (paste / shift+enter) must not emit a raw
        // '\n' into the frame — the renderers assume one Line = one row.
        let lines = render_user("first\nsecond", 80, &Theme::dark());
        let texts: Vec<String> = lines.iter().map(|l| l.text()).collect();
        assert!(texts.iter().all(|t| !t.contains('\n')), "{texts:?}");
        let first = texts.iter().position(|t| t.contains("first")).unwrap();
        let second = texts.iter().position(|t| t.contains("second")).unwrap();
        assert!(second > first, "{texts:?}");
    }

    #[test]
    fn notice_newlines_become_separate_rows() {
        let entry = ChatEntry::notice("one\ntwo", NoticeKind::Info);
        let lines = entry.render(80, &Theme::dark(), Media::default());
        let texts: Vec<String> = lines.iter().map(|l| l.text()).collect();
        assert!(texts.iter().all(|t| !t.contains('\n')), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("one")));
        assert!(texts.iter().any(|t| t.contains("two")));
    }
}
