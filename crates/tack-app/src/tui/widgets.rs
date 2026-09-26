//! tack-ext declarative widgets (v2.1): the host renders plugin-declared
//! status-line segments and markdown/list panels; plugins push full-state
//! snapshots via `widget.update` and never touch the terminal themselves
//! (rendering contract: extensions-v2.md §3.2). Inherent `impl TuiApp`
//! split out of `mod.rs`, same pattern as the other tui modules.

use tack_ext::{
    ListPanelItem, ListPanelState, MarkdownPanelState, StatusLineState, StatusStyle, WidgetKind,
};
use tack_tui::components::markdown::render_markdown;
use tack_tui::{Line, Span, Style};

use super::*;
use crate::extension_host::WidgetEntry;

/// Max body lines a panel occupies. The host owns layout: panels never
/// push the editor/footer off screen; overflow is scrolled (host-owned).
pub(crate) const PANEL_MAX_BODY: usize = 10;

/// Per-widget TUI interaction state (selection cursor, scroll position,
/// markdown render cache). Lives on the host side — the plugin only sees
/// the resulting `widget.action` events.
#[derive(Clone, Debug, Default)]
pub struct PanelUi {
    pub selected: usize,
    pub scroll: usize,
    /// Markdown render cache keyed by (widget rev, width).
    md_cache: Option<(u64, u16, Vec<Line>)>,
}

/// Map a plugin-declared status style onto theme colors.
pub fn status_style(style: Option<StatusStyle>, theme: &Theme) -> Style {
    match style {
        None | Some(StatusStyle::Default) => theme.text,
        Some(StatusStyle::Info) => theme.accent,
        Some(StatusStyle::Warning) => theme.warning,
        Some(StatusStyle::Error) => theme.error,
        Some(StatusStyle::Dim) => theme.dim,
    }
}

/// Visible status segments, priority-ascending (ties: register order).
/// Empty text hides the segment; unparseable state is skipped (contract:
/// the host never breaks the status bar over a bad plugin payload).
pub fn status_segments(widgets: &[WidgetEntry]) -> Vec<(String, Option<StatusStyle>)> {
    let mut segments: Vec<(i64, usize, String, Option<StatusStyle>)> = widgets
        .iter()
        .enumerate()
        .filter(|(_, w)| w.spec.kind == WidgetKind::StatusLineSegment)
        .filter_map(|(index, w)| {
            let state: StatusLineState = serde_json::from_value(w.state.clone()?).ok()?;
            if state.text.is_empty() {
                return None;
            }
            Some((w.spec.priority.unwrap_or(0), index, state.text, state.style))
        })
        .collect();
    segments.sort_by_key(|(priority, index, _, _)| (*priority, *index));
    segments
        .into_iter()
        .map(|(_, _, text, style)| (text, style))
        .collect()
}

/// The `widget.action` payload for the current list selection (contract:
/// action "select", plugin-local widget id, the picked item's id).
pub fn list_select_action(
    widget_id: &str,
    items: &[ListPanelItem],
    selected: usize,
) -> Option<tack_ext::WidgetActionPayload> {
    items
        .get(selected)
        .map(|item| tack_ext::WidgetActionPayload {
            id: widget_id.to_string(),
            action: "select".to_string(),
            item_id: Some(item.id.clone()),
        })
}

/// Parse a widget's current state; None on absent/malformed state (the
/// host degrades to an empty body instead of failing the frame).
fn parse_state<T: serde::de::DeserializeOwned>(widget: &WidgetEntry) -> Option<T> {
    widget
        .state
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
}

impl TuiApp {
    /// Render visible ext panels (markdown/list) into the tail section,
    /// above the editor — same slot as the todo panel.
    pub(crate) fn render_ext_panels(&mut self, width: u16) -> Vec<Line> {
        let mut out = Vec::new();
        // Field-level borrows: the widget registry is BORROWED per frame
        // (cloning every WidgetEntry per frame was measurable with big
        // panel states) while the host-side panel UI state is mutated.
        let TuiApp {
            extensions,
            ext_panel_ui,
            ext_panels_hidden,
            ext_panel_focus,
            theme,
            ..
        } = self;
        if *ext_panels_hidden {
            return out;
        }
        for widget in extensions.widgets() {
            if widget.spec.kind == WidgetKind::StatusLineSegment || !widget.visible {
                continue;
            }
            let focused = ext_panel_focus.as_deref() == Some(widget.key.as_str());
            let title = widget
                .spec
                .title
                .clone()
                .unwrap_or_else(|| widget.spec.id.clone());
            let mut header = Line::new();
            header.push(Span::styled(format!(" {title}"), theme.accent.bold()));
            header.push(Span::styled(format!("  ({})", widget.plugin), theme.dim));
            if focused {
                header.push(Span::styled(
                    crate::i18n::tr("panel.ext_focused"),
                    theme.muted,
                ));
            }
            out.push(header);
            match widget.spec.kind {
                WidgetKind::MarkdownPanel => {
                    render_markdown_panel(theme, ext_panel_ui, widget, &mut out, width)
                }
                WidgetKind::ListPanel => {
                    render_list_panel(theme, ext_panel_ui, widget, &mut out, width)
                }
                WidgetKind::StatusLineSegment => unreachable!("filtered above"),
            }
        }
        out
    }

    /// Visible panel keys in registry order (focus cycle order).
    pub(crate) fn visible_panel_keys(&self) -> Vec<String> {
        if self.ext_panels_hidden {
            return Vec::new();
        }
        self.extensions
            .widgets()
            .iter()
            .filter(|w| w.spec.kind != WidgetKind::StatusLineSegment && w.visible)
            .map(|w| w.key.clone())
            .collect()
    }

    /// alt+p: move keyboard focus to the next visible ext panel.
    pub(crate) fn focus_next_ext_panel(&mut self) {
        let keys = self.visible_panel_keys();
        if keys.is_empty() {
            self.ext_panel_focus = None;
            return;
        }
        let next = match &self.ext_panel_focus {
            Some(current) => {
                let pos = keys.iter().position(|k| k == current);
                keys.get(pos.map(|p| p + 1).unwrap_or(0) % keys.len())
                    .cloned()
            }
            None => keys.first().cloned(),
        };
        self.ext_panel_focus = next;
    }

    /// Focused-panel keyboard capture: list navigation / panel scrolling;
    /// enter on a list panel reports `widget.action` to the owning plugin
    /// (fire-and-forget). Returns true when the key was consumed.
    pub(crate) async fn handle_ext_panel_key(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        let Some(focus_key) = self.ext_panel_focus.clone() else {
            return false;
        };
        let Some(widget) = self
            .extensions
            .widgets()
            .iter()
            .find(|w| w.key == focus_key)
            .cloned()
        else {
            // The widget vanished (plugin death / visibility toggle).
            self.ext_panel_focus = None;
            return false;
        };
        if self.kb.matches("app.interrupt", key) {
            self.ext_panel_focus = None;
            return true;
        }
        match widget.spec.kind {
            WidgetKind::ListPanel => {
                let state = parse_state::<ListPanelState>(&widget).unwrap_or(ListPanelState {
                    items: Vec::new(),
                    selected_id: None,
                });
                let ui = self.ext_panel_ui.entry(focus_key).or_default();
                if key.matches("up") {
                    ui.selected = ui.selected.saturating_sub(1);
                    true
                } else if key.matches("down") {
                    if ui.selected + 1 < state.items.len() {
                        ui.selected += 1;
                    }
                    true
                } else if key.matches("enter") {
                    if let Some(action) =
                        list_select_action(&widget.spec.id, &state.items, ui.selected)
                    {
                        self.extensions
                            .notify_widget_action(&widget.plugin, action)
                            .await;
                    }
                    true
                } else {
                    false
                }
            }
            WidgetKind::MarkdownPanel => {
                let ui = self.ext_panel_ui.entry(focus_key).or_default();
                if key.matches("up") {
                    ui.scroll = ui.scroll.saturating_sub(1);
                    true
                } else if key.matches("down") {
                    ui.scroll += 1; // clamped against the body height at render
                    true
                } else if key.matches("pageup") {
                    ui.scroll = ui.scroll.saturating_sub(PANEL_MAX_BODY);
                    true
                } else if key.matches("pagedown") {
                    ui.scroll += PANEL_MAX_BODY;
                    true
                } else {
                    false
                }
            }
            WidgetKind::StatusLineSegment => false,
        }
    }
}

/// Markdown panel body: pulldown-cmark via the shared pipeline,
/// cached per (state rev, width); host-owned scroll window. Only the
/// visible slice (≤ PANEL_MAX_BODY lines) is cloned out of the cache —
/// previously a cache hit cloned the WHOLE rendered body per frame.
fn render_markdown_panel(
    theme: &Theme,
    panel_ui: &mut HashMap<String, PanelUi>,
    widget: &WidgetEntry,
    out: &mut Vec<Line>,
    width: u16,
) {
    let markdown = parse_state::<MarkdownPanelState>(widget)
        .map(|s| s.markdown)
        .unwrap_or_default();
    let ui = panel_ui.entry(widget.key.clone()).or_default();
    let stale = !matches!(&ui.md_cache, Some((rev, w, _)) if *rev == widget.rev && *w == width);
    if stale {
        let lines = render_markdown(&markdown, width as usize, &theme.markdown);
        ui.md_cache = Some((widget.rev, width, lines));
    }
    let total = ui.md_cache.as_ref().map(|(_, _, l)| l.len()).unwrap_or(0);
    ui.scroll = ui.scroll.min(total.saturating_sub(PANEL_MAX_BODY));
    let scroll = ui.scroll;
    if let Some((_, _, lines)) = &ui.md_cache {
        out.extend(lines.iter().skip(scroll).take(PANEL_MAX_BODY).cloned());
    }
    if total > PANEL_MAX_BODY {
        let end = (scroll + PANEL_MAX_BODY).min(total);
        out.push(Line::styled(
            format!("  … {}-{end}/{total}", scroll + 1),
            theme.dim,
        ));
    }
}

/// List panel body: items with a host-rendered selection cursor and a
/// scroll window around it (SelectList semantics, minus filtering).
fn render_list_panel(
    theme: &Theme,
    panel_ui: &mut HashMap<String, PanelUi>,
    widget: &WidgetEntry,
    out: &mut Vec<Line>,
    width: u16,
) {
    let state = parse_state::<ListPanelState>(widget).unwrap_or(ListPanelState {
        items: Vec::new(),
        selected_id: None,
    });
    let ui = panel_ui.entry(widget.key.clone()).or_default();
    if state.items.is_empty() {
        out.push(Line::styled("  (empty)", theme.dim));
        return;
    }
    ui.selected = ui.selected.min(state.items.len() - 1);
    let selected = ui.selected;
    let start = if selected >= PANEL_MAX_BODY {
        selected + 1 - PANEL_MAX_BODY
    } else {
        0
    };
    for (pos, item) in state
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(PANEL_MAX_BODY)
    {
        let is_selected = pos == selected;
        let mut line = Line::new();
        line.push(Span::plain(if is_selected { "→ " } else { "  " }));
        if let Some(icon) = &item.icon {
            line.push(Span::plain(format!("{icon} ")));
        }
        line.push(Span::styled(
            item.label.clone(),
            if is_selected {
                Style::new().bold()
            } else {
                Style::default()
            },
        ));
        if let Some(detail) = &item.detail {
            line.push(Span::styled(format!("  {detail}"), theme.dim));
        }
        line.truncate(width as usize, false);
        if is_selected {
            for span in &mut line.spans {
                span.style = span.style.merged_with(&theme.selected_bg);
            }
        }
        out.push(line);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::extension_host::WidgetRegistry;

    fn registry_with(plugin: &str, specs: Vec<serde_json::Value>) -> WidgetRegistry {
        let mut registry = WidgetRegistry::default();
        let specs: Vec<tack_ext::WidgetSpec> = specs
            .into_iter()
            .map(|v| serde_json::from_value(v).unwrap())
            .collect();
        registry.register_plugin(plugin, &specs);
        registry
    }

    /// Status segments sort priority-ascending (ties keep register order),
    /// empty text hides the segment, bad state is skipped.
    #[test]
    fn status_segments_sorted_and_hidden() {
        let registry = registry_with(
            "demo",
            vec![
                serde_json::json!({"id": "z", "type": "status_line_segment", "priority": 50,
                    "initial": {"text": "seg-z"}}),
                serde_json::json!({"id": "a", "type": "status_line_segment", "priority": 10,
                    "initial": {"text": "seg-a", "style": "info"}}),
                serde_json::json!({"id": "b", "type": "status_line_segment", "priority": 10,
                    "initial": {"text": "seg-b", "style": "dim"}}),
                serde_json::json!({"id": "h", "type": "status_line_segment", "priority": 1,
                    "initial": {"text": ""}}),
                serde_json::json!({"id": "bad", "type": "status_line_segment", "priority": 1,
                    "initial": {"markdown": 1}}),
                // Panels never appear in the status bar.
                serde_json::json!({"id": "p", "type": "markdown_panel", "title": "t",
                    "initial": {"markdown": "hi"}}),
            ],
        );
        let segments = status_segments(registry.entries());
        assert_eq!(
            segments,
            vec![
                ("seg-a".to_string(), Some(StatusStyle::Info)),
                ("seg-b".to_string(), Some(StatusStyle::Dim)),
                ("seg-z".to_string(), None),
            ]
        );
    }

    /// widget.update replaces the rendered segment text (full-state
    /// replacement read back through the same helper the renderer uses).
    #[test]
    fn status_segments_reflect_updates() {
        let mut registry = registry_with(
            "demo",
            vec![serde_json::json!({"id": "s", "type": "status_line_segment",
                "initial": {"text": "old"}})],
        );
        let update = tack_ext::WidgetUpdatePayload {
            id: "s".to_string(),
            state: serde_json::json!({"text": "new", "style": "error"}),
            visible: None,
        };
        assert!(registry.apply_update("demo", &update));
        assert_eq!(
            status_segments(registry.entries()),
            vec![("new".to_string(), Some(StatusStyle::Error))]
        );
        // Empty replacement text hides the segment.
        let update = tack_ext::WidgetUpdatePayload {
            id: "s".to_string(),
            state: serde_json::json!({"text": ""}),
            visible: None,
        };
        assert!(registry.apply_update("demo", &update));
        assert!(status_segments(registry.entries()).is_empty());
    }

    /// List selection → widget.action payload (plugin-local id, item id).
    #[test]
    fn list_select_action_builds_payload() {
        let items = vec![
            ListPanelItem {
                id: "a".to_string(),
                label: "Alpha".to_string(),
                detail: None,
                icon: None,
            },
            ListPanelItem {
                id: "b".to_string(),
                label: "Beta".to_string(),
                detail: Some("second".to_string()),
                icon: None,
            },
        ];
        let action = list_select_action("files", &items, 1).unwrap();
        assert_eq!(action.id, "files");
        assert_eq!(action.action, "select");
        assert_eq!(action.item_id.as_deref(), Some("b"));
        assert!(list_select_action("files", &items, 2).is_none());
        assert!(list_select_action("files", &[], 0).is_none());
    }
}
