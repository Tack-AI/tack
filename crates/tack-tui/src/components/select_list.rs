//! SelectList: a scrollable list of selectable options (port of
//! `select-list.ts`), used by every selector dialog.

use crate::component::Component;
use crate::input::InputEvent;
use crate::line::{Line, Span};
use crate::style::{Color, Style};

#[derive(Clone, Debug)]
pub struct SelectItem {
    pub label: String,
    pub description: Option<String>,
    /// Opaque value returned on selection.
    pub value: String,
}

impl SelectItem {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        SelectItem {
            label: label.into(),
            description: None,
            value: value.into(),
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

#[derive(Debug)]
pub struct SelectList {
    pub items: Vec<SelectItem>,
    /// Indices into `items`, after filtering.
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub max_visible: u16,
    pub selected_style: Style,
    pub description_style: Style,
    /// Optional fuzzy filter query.
    pub filter: String,
    /// Set by handle_input on Enter; cleared by the caller.
    pub on_confirm: Option<String>,
    /// Set on Escape.
    pub cancelled: bool,
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>) -> Self {
        let filtered: Vec<usize> = (0..items.len()).collect();
        SelectList {
            items,
            filtered,
            selected: 0,
            max_visible: 10,
            selected_style: Style::new().bg(Color::Rgb(60, 60, 90)),
            description_style: Style::new().dim(),
            filter: String::new(),
            on_confirm: None,
            cancelled: false,
        }
    }

    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items;
        self.apply_filter();
        self.selected = 0;
    }

    pub fn set_filter(&mut self, filter: impl Into<String>) {
        self.filter = filter.into();
        self.apply_filter();
        self.selected = 0;
    }

    fn apply_filter(&mut self) {
        let query = self.filter.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                query.is_empty()
                    || crate::components::select_list::fuzzy_match(
                        &query,
                        &item.label.to_lowercase(),
                    )
            })
            .map(|(i, _)| i)
            .collect();
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered.get(self.selected).map(|i| &self.items[*i])
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as i32;
        self.selected = ((self.selected as i32 + delta).rem_euclid(len)) as usize;
    }
}

/// Fast subsequence predicate (case-folded by the caller), retained for
/// selectors and path completion where ranking is not required.
pub fn fuzzy_match(query: &str, candidate: &str) -> bool {
    let mut chars = candidate.chars();
    query.chars().all(|q| chars.by_ref().any(|c| c == q))
}

/// Detailed result of the scored fuzzy matcher. Lower scores are better.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

/// Score one alignment. This is a direct port of upstream `scorePositions`:
/// consecutive characters and word boundaries are rewarded, while gaps and
/// later matches are penalized.
fn score_positions(text: &[char], positions: &[usize]) -> f64 {
    let mut score = 0.0;
    let mut last_match_index = None;
    let mut consecutive_matches = 0_u32;

    for &index in positions {
        if last_match_index == index.checked_sub(1) {
            consecutive_matches += 1;
            score -= f64::from(consecutive_matches) * 5.0;
        } else {
            consecutive_matches = 0;
            if let Some(last) = last_match_index {
                score += (index - last - 1) as f64 * 2.0;
            }
        }

        if index == 0
            || text[index - 1].is_whitespace()
            || matches!(text[index - 1], '-' | '_' | '.' | '/' | ':')
        {
            score -= 10.0;
        }
        score += index as f64 * 0.1;
        last_match_index = Some(index);
    }

    score
}

/// Match an already-lowercased query, considering both the greedy subsequence
/// alignment and every contiguous occurrence. The latter prevents an earlier,
/// unrelated provider/model prefix from outscoring the actual contiguous hit.
fn match_query(normalized_query: &str, text_lower: &str) -> FuzzyMatch {
    let query: Vec<char> = normalized_query.chars().collect();
    let text: Vec<char> = text_lower.chars().collect();

    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    let mut positions = Vec::with_capacity(query.len());
    let mut search_start = 0;
    for query_char in &query {
        let Some(relative_index) = text[search_start..]
            .iter()
            .position(|candidate| candidate == query_char)
        else {
            return FuzzyMatch {
                matches: false,
                score: 0.0,
            };
        };
        let index = search_start + relative_index;
        positions.push(index);
        search_start = index + 1;
    }

    let mut score = score_positions(&text, &positions);
    for start in 0..=text.len() - query.len() {
        if text[start..start + query.len()] != query {
            continue;
        }
        for (offset, position) in positions.iter_mut().enumerate() {
            *position = start + offset;
        }
        score = score.min(score_positions(&text, &positions));
    }

    if normalized_query == text_lower {
        score -= 100.0;
    }
    FuzzyMatch {
        matches: true,
        score,
    }
}

/// Upstream-compatible fallback for queries such as `codex52` against
/// `gpt-5.2-codex`. Only an entirely alphabetic + numeric (or numeric +
/// alphabetic) ASCII query is transposed, and only when the primary match
/// failed. The transposed match carries a five-point penalty.
fn swapped_alpha_numeric_query(query: &str) -> Option<String> {
    let bytes = query.as_bytes();
    let first = *bytes.first()?;
    let letters_first = first.is_ascii_alphabetic();
    if !letters_first && !first.is_ascii_digit() {
        return None;
    }

    let mut split = 0;
    while split < bytes.len()
        && if letters_first {
            bytes[split].is_ascii_alphabetic()
        } else {
            bytes[split].is_ascii_digit()
        }
    {
        split += 1;
    }
    if split == bytes.len()
        || !bytes[split..].iter().all(|byte| {
            if letters_first {
                byte.is_ascii_digit()
            } else {
                byte.is_ascii_alphabetic()
            }
        })
    {
        return None;
    }
    Some(format!("{}{}", &query[split..], &query[..split]))
}

/// Case-insensitive scored fuzzy match, ported from upstream `fuzzyMatch`.
pub fn fuzzy_match_scored(query: &str, text: &str) -> FuzzyMatch {
    let query_lower = query.to_lowercase();
    let text_lower = text.to_lowercase();
    let primary_match = match_query(&query_lower, &text_lower);
    if primary_match.matches {
        return primary_match;
    }

    let Some(swapped_query) = swapped_alpha_numeric_query(&query_lower) else {
        return primary_match;
    };
    let swapped_match = match_query(&swapped_query, &text_lower);
    if !swapped_match.matches {
        return primary_match;
    }
    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// Filter and stably sort items by fuzzy match quality (best matches first).
/// Whitespace- and slash-separated query tokens must all match. An empty query
/// (or one containing only separators) returns the input order unchanged.
pub fn fuzzy_filter<T, F>(items: Vec<T>, query: &str, get_text: F) -> Vec<T>
where
    F: Fn(&T) -> String,
{
    if query.trim().is_empty() {
        return items;
    }
    let tokens: Vec<&str> = query
        .trim()
        .split(|character: char| character.is_whitespace() || character == '/')
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return items;
    }

    let mut results = Vec::new();
    for item in items {
        let text = get_text(&item);
        let mut total_score = 0.0;
        let mut all_match = true;
        for token in &tokens {
            let result = fuzzy_match_scored(token, &text);
            if result.matches {
                total_score += result.score;
            } else {
                all_match = false;
                break;
            }
        }
        if all_match {
            results.push((item, total_score));
        }
    }

    // `sort_by` is stable, matching modern JavaScript's stable Array.sort.
    results.sort_by(|left, right| left.1.total_cmp(&right.1));
    results.into_iter().map(|(item, _)| item).collect()
}

impl Component for SelectList {
    fn render(&mut self, width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        if self.filtered.is_empty() {
            lines.push(Line::styled("  (no matches)", self.description_style));
            return lines;
        }
        // Scroll window around the selection.
        let max = self.max_visible.max(1) as usize;
        let start = if self.selected >= max {
            self.selected + 1 - max
        } else {
            0
        };
        for (pos, item_index) in self.filtered.iter().enumerate().skip(start).take(max) {
            let item = &self.items[*item_index];
            let is_selected = pos == self.selected;
            let mut line = Line::new();
            let marker = if is_selected { "→ " } else { "  " };
            line.push(Span::plain(marker));
            line.push(Span::styled(
                item.label.clone(),
                if is_selected {
                    Style::new().bold()
                } else {
                    Style::default()
                },
            ));
            if let Some(description) = &item.description {
                line.push(Span::styled(
                    format!("  {description}"),
                    self.description_style,
                ));
            }
            line.truncate(width as usize, false);
            line.pad_right(
                width as usize,
                if is_selected {
                    self.selected_style
                } else {
                    Style::default()
                },
            );
            if is_selected {
                for span in &mut line.spans {
                    span.style = span.style.merged_with(&self.selected_style);
                }
            }
            lines.push(line);
        }
        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        if key.matches("up") {
            self.move_selection(-1);
            return true;
        }
        if key.matches("down") {
            self.move_selection(1);
            return true;
        }
        if key.matches("pageup") {
            self.move_selection(-(self.max_visible as i32));
            return true;
        }
        if key.matches("pagedown") {
            self.move_selection(self.max_visible as i32);
            return true;
        }
        if key.matches("enter") {
            self.on_confirm = self.selected_item().map(|i| i.value.clone());
            return true;
        }
        if key.matches("escape") {
            self.cancelled = true;
            return true;
        }
        false
    }
}

impl Default for SelectList {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::{fuzzy_filter, fuzzy_match_scored};

    #[test]
    fn empty_query_matches_with_zero_score() {
        let result = fuzzy_match_scored("", "anything");
        assert!(result.matches);
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn query_must_be_an_in_order_subsequence() {
        assert!(!fuzzy_match_scored("longquery", "short").matches);
        assert!(fuzzy_match_scored("abc", "aXbXc").matches);
        assert!(!fuzzy_match_scored("abc", "cba").matches);
    }

    #[test]
    fn matching_is_case_insensitive_and_exact_match_has_bonus() {
        assert!(fuzzy_match_scored("ABC", "abc").matches);
        assert!(fuzzy_match_scored("abc", "ABC").matches);
        let exact = fuzzy_match_scored("test", "test");
        let prefix = fuzzy_match_scored("test", "testing");
        assert!(exact.score < 0.0);
        assert_eq!(exact.score, prefix.score - 100.0);
    }

    #[test]
    fn consecutive_and_boundary_matches_score_better() {
        let consecutive = fuzzy_match_scored("foo", "foobar");
        let scattered = fuzzy_match_scored("foo", "f_o_o_bar");
        assert!(consecutive.matches && scattered.matches);
        assert!(consecutive.score < scattered.score);

        let boundary = fuzzy_match_scored("fb", "foo-bar");
        let embedded = fuzzy_match_scored("fb", "afbx");
        assert!(boundary.matches && embedded.matches);
        assert!(boundary.score < embedded.score);
    }

    #[test]
    fn contiguous_occurrence_beats_earlier_scattered_alignment() {
        let opus = fuzzy_match_scored("opus", "openrouter openrouter/anthropic/claude-opus-4.1");
        let sonnet =
            fuzzy_match_scored("opus", "openrouter openrouter/anthropic/claude-sonnet-4.5");
        let gpt_oss = fuzzy_match_scored("opus", "openrouter openrouter/openai/gpt-oss-120b");
        assert!(opus.score < sonnet.score);
        assert!(opus.score < gpt_oss.score);
    }

    #[test]
    fn best_contiguous_occurrence_wins_not_the_first() {
        let embedded_first = fuzzy_match_scored("opus", "xxopus opus");
        let boundary_only = fuzzy_match_scored("opus", "xxxxxx opus");
        assert_eq!(embedded_first.score, boundary_only.score);
    }

    #[test]
    fn swapped_alpha_numeric_tokens_match_with_penalty() {
        let swapped = fuzzy_match_scored("codex52", "gpt-5.2-codex");
        let direct = fuzzy_match_scored("52codex", "gpt-5.2-codex");
        assert!(swapped.matches);
        assert_eq!(swapped.score, direct.score + 5.0);
        assert!(fuzzy_match_scored("52codex", "codex-5.2").matches);
        assert!(!fuzzy_match_scored("codex52x", "gpt-5.2-codex").matches);
    }

    #[test]
    fn empty_filter_preserves_input_order() {
        let items = vec!["apple", "banana", "cherry"];
        assert_eq!(
            fuzzy_filter(items.clone(), "", |item| (*item).to_string()),
            items
        );
        assert_eq!(
            fuzzy_filter(items.clone(), " / ", |item| (*item).to_string()),
            items
        );
    }

    #[test]
    fn filter_sorts_by_quality_and_prioritizes_exact_matches() {
        let items = vec!["a_p_p", "app", "application"];
        let filtered = fuzzy_filter(items, "app", |item| (*item).to_string());
        assert_eq!(filtered.first(), Some(&"app"));

        let filtered = fuzzy_filter(vec!["clone", "cl"], "cl", |item| (*item).to_string());
        assert_eq!(filtered, vec!["cl", "clone"]);
        let filtered = fuzzy_filter(vec!["apple", "banana", "cherry"], "an", |item| {
            (*item).to_string()
        });
        assert_eq!(filtered, vec!["banana"]);
    }

    #[test]
    fn filter_contiguous_hit_beats_shared_provider_prefix() {
        let items = vec![
            "radius radius/gpt-5.4 radius gpt-5.4 GPT 5.4",
            "radius radius/agentic radius agentic Agentic",
            "radius radius/gpt-6-sol radius gpt-6-sol GPT 6 Sol",
            "radius radius/gpt-6-astra radius gpt-6-astra GPT 6 Astra",
        ];
        let filtered = fuzzy_filter(items.clone(), "astra", |item| (*item).to_string());
        assert_eq!(filtered.first(), items.get(3));
    }

    #[test]
    fn slash_separated_tokens_can_match_reordered_text() {
        #[derive(PartialEq, Debug)]
        struct Model {
            id: &'static str,
            provider: &'static str,
        }
        let model = Model {
            id: "gpt-5.5",
            provider: "openai-codex",
        };
        // Use owned display text to exercise the same two-token scoring path
        // as upstream's `${id} ${provider}` fixture.
        let display = format!("{} {}", model.id, model.provider);
        let filtered = fuzzy_filter(vec![display], "openai-codex/gpt-5.5", |item| item.clone());
        assert_eq!(filtered, vec!["gpt-5.5 openai-codex"]);
    }
}
