// ---------------------------------------------------------------------------
// Session tree dialog (/tree): jump + label editing
// ---------------------------------------------------------------------------

use tack_session::SessionManager;
use tack_tui::Component as _;
use tack_tui::components::select_list::{SelectItem, SelectList};
use tack_tui::{InputEvent, Key, Line, Span};

use crate::tui::theme::Theme;

/// `/tree` filter modes (TS tree-selector FilterMode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeFilter {
    /// Hide settings entries (custom/model_change/thinking_level_change/session_info).
    Default,
    /// Default minus tool results.
    NoTools,
    /// Only user messages.
    UserOnly,
    /// Only entries with a custom label.
    LabeledOnly,
    /// Everything.
    All,
}

impl TreeFilter {
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("no-tools") => TreeFilter::NoTools,
            Some("user-only") => TreeFilter::UserOnly,
            Some("labeled-only") => TreeFilter::LabeledOnly,
            Some("all") => TreeFilter::All,
            _ => TreeFilter::Default,
        }
    }

    fn cycle(self) -> Self {
        match self {
            TreeFilter::Default => TreeFilter::NoTools,
            TreeFilter::NoTools => TreeFilter::UserOnly,
            TreeFilter::UserOnly => TreeFilter::LabeledOnly,
            TreeFilter::LabeledOnly => TreeFilter::All,
            TreeFilter::All => TreeFilter::Default,
        }
    }

    fn label(self) -> &'static str {
        match self {
            TreeFilter::Default => "default",
            TreeFilter::NoTools => "no-tools",
            TreeFilter::UserOnly => "user-only",
            TreeFilter::LabeledOnly => "labeled-only",
            TreeFilter::All => "all",
        }
    }

    fn passes(self, entry: &tack_session::SessionEntry, has_label: bool) -> bool {
        use tack_session::SessionEntry as E;
        let is_settings = matches!(
            entry,
            E::Custom { .. }
                | E::ModelChange { .. }
                | E::ThinkingLevelChange { .. }
                | E::SessionInfo { .. }
        );
        match self {
            TreeFilter::Default => !is_settings,
            TreeFilter::NoTools => {
                !is_settings
                    && !matches!(
                        entry,
                        E::Message {
                            message: tack_agent_core::AgentMessage::ToolResult(_),
                            ..
                        }
                    )
            }
            TreeFilter::UserOnly => {
                matches!(
                    entry,
                    E::Message {
                        message: tack_agent_core::AgentMessage::User(_),
                        ..
                    }
                )
            }
            TreeFilter::LabeledOnly => has_label,
            TreeFilter::All => true,
        }
    }
}

/// Build the tree rows (depth-first, with 🏷 custom labels). Shared by
/// `command_tree` and TreeDialog::refresh.
pub fn build_tree_items(session: &SessionManager) -> Vec<SelectItem> {
    build_tree_items_filtered(session, TreeFilter::Default)
}

/// Filtered variant (TS tree-selector filter modes).
pub fn build_tree_items_filtered(session: &SessionManager, filter: TreeFilter) -> Vec<SelectItem> {
    let entries = session.entries();
    let mut children: std::collections::HashMap<Option<String>, Vec<&tack_session::SessionEntry>> =
        std::collections::HashMap::new();
    for entry in &entries {
        children
            .entry(entry.parent_id().map(str::to_string))
            .or_default()
            .push(entry);
    }
    let leaf = session.leaf_id().map(str::to_string);
    let mut items: Vec<SelectItem> = Vec::new();
    #[allow(clippy::too_many_arguments)]
    fn walk(
        parent: Option<&str>,
        children: &std::collections::HashMap<Option<String>, Vec<&tack_session::SessionEntry>>,
        depth: usize,
        leaf: &Option<String>,
        session: &SessionManager,
        filter: TreeFilter,
        items: &mut Vec<SelectItem>,
    ) {
        let Some(list) = children.get(&parent.map(str::to_string)) else {
            return;
        };
        for entry in list {
            let has_label = session.get_label(entry.id()).is_some();
            if filter.passes(entry, has_label)
                && let Some(mut label) = tree_label(entry, session)
            {
                if let Some(custom) = session.get_label(entry.id()) {
                    label = format!("🏷 {custom} — {label}");
                }
                let marker = if Some(entry.id().to_string()) == *leaf {
                    " ●"
                } else {
                    ""
                };
                items.push(SelectItem::new(
                    format!("{}{}{}", "  ".repeat(depth), label, marker),
                    entry.id().to_string(),
                ));
            }
            walk(
                Some(entry.id()),
                children,
                depth + 1,
                leaf,
                session,
                filter,
                items,
            );
        }
    }
    walk(None, &children, 0, &leaf, session, filter, &mut items);
    items
}

/// `/tree` dialog (TS tree-selector): enter jumps; ctrl+e edits the selected
/// node's label. Label edits are applied by the app (it owns the session)
/// via `pending_label`; the dialog stays open.
#[derive(Debug)]
pub struct TreeDialog {
    list: SelectList,
    labels: std::collections::HashMap<String, String>,
    filter: TreeFilter,
    editing: bool,
    edit_input: String,
    edit_target: Option<String>,
    pub pending_label: Option<(String, Option<String>)>,
    /// Set on ctrl+t; the app cycles the filter (it owns the session).
    pub pending_filter_cycle: bool,
    pub(super) done: bool,
    pub(super) cancelled: bool,
    pub(super) on_confirm: Option<String>,
    theme: Theme,
}

impl TreeDialog {
    pub fn new(session: &SessionManager, filter: TreeFilter, theme: Theme) -> Self {
        let mut dialog = TreeDialog {
            list: SelectList::new(Vec::new()),
            labels: std::collections::HashMap::new(),
            filter,
            editing: false,
            edit_input: String::new(),
            edit_target: None,
            pending_label: None,
            pending_filter_cycle: false,
            done: false,
            cancelled: false,
            on_confirm: None,
            theme,
        };
        dialog.refresh(session);
        dialog
    }

    pub fn refresh(&mut self, session: &SessionManager) {
        let filter = self.list.filter.clone();
        self.list = SelectList::new(build_tree_items_filtered(session, self.filter));
        self.list.set_filter(filter);
        self.labels = session
            .entries()
            .iter()
            .filter_map(|e| session.get_label(e.id()).map(|l| (e.id().to_string(), l)))
            .collect();
    }

    pub fn render(&mut self, width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut title = Line::new();
        title.push(Span::styled(
            crate::i18n::trf("td.title", &[("filter", self.filter.label())]),
            self.theme.accent.bold(),
        ));
        if !self.list.filter.is_empty() {
            title.push(Span::styled(
                format!("  {}{}", crate::i18n::tr("dialog.filter"), self.list.filter),
                self.theme.dim,
            ));
        }
        title.truncate(width as usize, true);
        lines.push(title);
        lines.extend(self.list.render(width));
        let footer = if self.editing {
            crate::i18n::trf("td.edit_label", &[("input", &self.edit_input)])
        } else {
            crate::i18n::tr("td.hint")
        };
        lines.push(Line::styled(footer, self.theme.dim));
        lines
    }

    /// Cycle the filter mode (needs the session to rebuild).
    pub fn cycle_filter(&mut self, session: &SessionManager) {
        self.filter = self.filter.cycle();
        self.refresh(session);
    }

    pub fn handle_input(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        if key.is_release {
            return true;
        }
        if self.editing {
            if key.matches("enter") {
                if let Some(target) = self.edit_target.take() {
                    let label = self.edit_input.trim().to_string();
                    self.pending_label =
                        Some((target, if label.is_empty() { None } else { Some(label) }));
                }
                self.editing = false;
            } else if key.matches("escape") {
                self.editing = false;
            } else if key.matches("backspace") {
                self.edit_input.pop();
            } else if let Key::Char(c) = key.key
                && !key.modifiers.ctrl
                && !key.modifiers.alt
            {
                self.edit_input.push(c);
            }
            return true;
        }
        if key.matches("ctrl+e") {
            if let Some(item) = self.list.selected_item() {
                self.edit_target = Some(item.value.clone());
                self.edit_input = self.labels.get(&item.value).cloned().unwrap_or_default();
                self.editing = true;
            }
            return true;
        }
        if key.matches("ctrl+t") {
            // The app rebuilds (it owns the session).
            self.pending_filter_cycle = true;
            return true;
        }
        if let Key::Char(c) = key.key
            && !key.modifiers.ctrl
            && !key.modifiers.alt
        {
            let mut filter = self.list.filter.clone();
            filter.push(c);
            self.list.set_filter(filter);
            return true;
        }
        if key.matches("backspace") && !self.list.filter.is_empty() {
            let mut filter = self.list.filter.clone();
            filter.pop();
            self.list.set_filter(filter);
            return true;
        }
        let handled = self.list.handle_input(event);
        if let Some(value) = self.list.on_confirm.take() {
            self.on_confirm = Some(value);
            self.done = true;
        }
        if self.list.cancelled {
            self.cancelled = true;
            self.done = true;
        }
        handled
    }
}

/// Label for a tree-selector row (None = not shown).
fn tree_label(entry: &tack_session::SessionEntry, _session: &SessionManager) -> Option<String> {
    use tack_agent_core::AgentMessage;
    use tack_session::SessionEntry;
    match entry {
        SessionEntry::Message { message, .. } => match message {
            AgentMessage::User(u) => {
                let text = match &u.content {
                    tack_ai::UserContent::Text(t) => t.clone(),
                    tack_ai::UserContent::Blocks(b) => b
                        .iter()
                        .filter_map(|b| match b {
                            tack_ai::InputContentBlock::Text { text, .. } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                Some(text.chars().take(60).collect())
            }
            AgentMessage::Assistant(a) => {
                let text = a.text();
                if text.is_empty() {
                    Some(crate::i18n::tr("tree.assistant_tools"))
                } else {
                    Some(format!("· {}", text.chars().take(60).collect::<String>()))
                }
            }
            AgentMessage::BashExecution(b) => Some(format!("$ {}", b.command)),
            AgentMessage::BranchSummary(b) => Some(format!("⎇ {}", b.summary)),
            AgentMessage::CompactionSummary(c) => Some(crate::i18n::trf(
                "tree.compacted",
                &[("tokens", &c.tokens_before.to_string())],
            )),
            _ => None,
        },
        SessionEntry::Compaction { tokens_before, .. } => Some(crate::i18n::trf(
            "tree.compacted",
            &[("tokens", &tokens_before.to_string())],
        )),
        SessionEntry::BranchSummary { summary, .. } => Some(format!("⎇ {summary}")),
        SessionEntry::Label { label, .. } => label.clone().map(|l| format!("🏷 {l}")),
        SessionEntry::SessionInfo { name, .. } => name.as_ref().map(|n| format!("📛 {n}")),
        SessionEntry::ModelChange {
            provider, model_id, ..
        } => Some(crate::i18n::trf(
            "tree.model_change",
            &[("ref", &format!("{provider}/{model_id}"))],
        )),
        _ => None,
    }
}
