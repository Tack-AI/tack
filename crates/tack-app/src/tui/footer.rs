//! Footer: cwd + git branch + session name; token/cost/context stats; model
//! + thinking level (port of `footer.ts`).

use std::path::{Path, PathBuf};

use tack_ai::{Model, ThinkingLevel};
use tack_tui::{Line, Span, Style};

use super::theme::Theme;

/// Session stats for the footer.
#[derive(Clone, Debug, Default)]
pub struct FooterStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub context_tokens: u64,
}

/// Current git branch: worktree-aware (via find_git_paths) and mtime-cached
/// so the per-frame render costs one stat instead of a full walk + read
/// (TS footer-data-provider watches the HEAD file instead).
fn git_branch(cwd: &Path) -> Option<String> {
    use std::sync::Mutex;
    struct Entry {
        cwd: PathBuf,
        head: PathBuf,
        mtime: Option<std::time::SystemTime>,
        branch: Option<String>,
    }
    static CACHE: Mutex<Option<Entry>> = Mutex::new(None);

    fn read_branch(head: &Path) -> Option<String> {
        let content = std::fs::read_to_string(head).ok()?;
        content
            .trim()
            .strip_prefix("ref: refs/heads/")
            .map(str::to_string)
    }

    let mut cache = CACHE.lock().ok()?;
    if let Some(entry) = cache.as_mut()
        && entry.cwd == cwd
    {
        let mtime = std::fs::metadata(&entry.head)
            .and_then(|m| m.modified())
            .ok();
        if mtime == entry.mtime {
            return entry.branch.clone();
        }
        entry.mtime = mtime;
        entry.branch = mtime.and_then(|_| read_branch(&entry.head));
        return entry.branch.clone();
    }
    let head = crate::resources::find_git_paths(cwd)?.head;
    let mtime = std::fs::metadata(&head).and_then(|m| m.modified()).ok();
    let branch = read_branch(&head);
    *cache = Some(Entry {
        cwd: cwd.to_path_buf(),
        head,
        mtime,
        branch: branch.clone(),
    });
    branch
}

fn short_home(path: &Path) -> String {
    let display = path.display().to_string();
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    display
}

#[allow(clippy::too_many_arguments)]
pub fn render_footer(
    cwd: &Path,
    model: &Model,
    thinking: Option<ThinkingLevel>,
    stats: &FooterStats,
    session_name: Option<&str>,
    update_hint: Option<&str>,
    mode: &str,
    ext_segments: &[(String, Style)],
    width: u16,
    theme: &Theme,
) -> Vec<Line> {
    let w = width as usize;

    // Line 1: cwd (branch) • session name • update hint
    let mut line1 = Line::new();
    line1.push(Span::styled(short_home(cwd), theme.muted));
    if let Some(branch) = git_branch(cwd) {
        line1.push(Span::styled(format!(" ({branch})"), theme.dim));
    }
    if let Some(name) = session_name
        && !name.is_empty()
    {
        line1.push(Span::styled(format!(" • {name}"), theme.accent));
    }
    // Startup update check found a newer release: persistent hint.
    if let Some(version) = update_hint {
        line1.push(Span::styled(
            format!(
                "  {}",
                crate::i18n::trf("footer.update_available", &[("version", version)])
            ),
            theme.warning,
        ));
    }
    line1.truncate(w, true);

    // Line 2: stats left, model right.
    let pct = if model.context_window > 0 {
        stats.context_tokens as f64 * 100.0 / model.context_window as f64
    } else {
        0.0
    };
    let pct_style = if pct > 90.0 {
        theme.error
    } else if pct > 70.0 {
        theme.warning
    } else {
        theme.dim
    };
    let mut left = Line::new();
    // Cache segments are shown only when nonzero: some providers (e.g.
    // Moonshot's automatic context cache) report cache READS but never
    // cache creation, so a permanent `W0` is noise, not information.
    let mut stats_text = format!("↑{} ↓{}", stats.input, stats.output);
    if stats.cache_read > 0 {
        stats_text.push_str(&format!(" R{}", stats.cache_read));
    }
    if stats.cache_write > 0 {
        stats_text.push_str(&format!(" W{}", stats.cache_write));
    }
    if stats.cache_read > 0 || stats.cache_write > 0 {
        // Cache hit rate: cache_read / (input + cache_read) as a percentage.
        let cache_hit = if stats.input + stats.cache_read > 0 {
            stats.cache_read as f64 * 100.0 / (stats.input + stats.cache_read) as f64
        } else {
            0.0
        };
        stats_text.push_str(&format!(" CH{cache_hit:.0}%"));
    }
    stats_text.push_str(&format!(" ${:.4} ", stats.cost));
    left.push(Span::styled(stats_text, theme.dim));
    left.push(Span::styled(
        format!("{pct:.0}%/{}k", model.context_window / 1000),
        pct_style,
    ));
    left.push(Span::styled(format!("  [{mode}]"), theme.muted));
    // tack-ext status-line segments (v2.1): priority-sorted by the caller,
    // theme-mapped styles, single-line truncation below.
    for (text, style) in ext_segments {
        left.push(Span::styled(format!("  {text}"), *style));
    }

    let thinking_label = thinking
        .map(|t| t.as_str().to_string())
        .unwrap_or_else(|| crate::i18n::tr("footer.thinking_off"));
    let mut right = Line::new();
    right.push(Span::styled(format!("({}) ", model.provider), theme.muted));
    right.push(Span::styled(model.id.clone(), theme.text));
    right.push(Span::styled(format!(" • {thinking_label}"), theme.dim));

    let gap = w.saturating_sub(left.width() + right.width());
    left.push(Span::plain(" ".repeat(gap)));
    for span in right.spans {
        left.push(span);
    }
    // Long stats on narrow terminals can exceed the width; never wrap.
    left.truncate(w, false);
    vec![line1, left]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tack_ai::Model;

    fn model() -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: "x".into(),
            provider: "x".into(),
            base_url: "http://x".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: Default::default(),
            context_window: 1_048_576,
            max_tokens: 100,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn stats_line(stats: &FooterStats) -> String {
        let lines = render_footer(
            Path::new("/tmp"),
            &model(),
            None,
            stats,
            None,
            None,
            "ask",
            &[],
            200,
            &Theme::dark(),
        );
        lines[1].text()
    }

    #[test]
    fn update_hint_appears_on_line_one() {
        let lines = render_footer(
            Path::new("/tmp"),
            &model(),
            None,
            &FooterStats::default(),
            None,
            Some("1.2.3"),
            "ask",
            &[],
            200,
            &Theme::dark(),
        );
        let line1 = lines[0].text();
        assert!(line1.contains("↑ v1.2.3"), "{line1}");
        // No hint: nothing rendered.
        let lines = render_footer(
            Path::new("/tmp"),
            &model(),
            None,
            &FooterStats::default(),
            None,
            None,
            "ask",
            &[],
            200,
            &Theme::dark(),
        );
        assert!(!lines[0].text().contains('↑'), "{}", lines[0].text());
    }

    #[test]
    fn zero_cache_segments_are_hidden() {
        // Moonshot-style: reads reported, creation never reported — the
        // permanent W0 was noise.
        let moonshot = FooterStats {
            input: 726_528,
            output: 155_107,
            cache_read: 45_832_448,
            cache_write: 0,
            cost: 18.2559,
            context_tokens: 270_000,
        };
        let line = stats_line(&moonshot);
        assert!(line.contains("R45832448"), "{line}");
        assert!(!line.contains("W0"), "{line}");
        assert!(line.contains("CH98%"), "{line}");

        // No caching at all: R/W/CH all hidden.
        let plain = FooterStats {
            input: 100,
            output: 20,
            ..Default::default()
        };
        let line = stats_line(&plain);
        assert!(!line.contains('R'), "{line}");
        assert!(!line.contains("CH"), "{line}");

        // Anthropic-style: both read and write shown.
        let anthropic = FooterStats {
            input: 10,
            output: 5,
            cache_read: 90,
            cache_write: 40,
            ..Default::default()
        };
        let line = stats_line(&anthropic);
        assert!(line.contains("R90"), "{line}");
        assert!(line.contains("W40"), "{line}");
    }
}
