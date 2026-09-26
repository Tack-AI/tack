//! Editor autocomplete: slash commands at message start, `@path` file
//! completion (gitignore-aware via the `ignore` crate — TS pi shells out to
//! `fd`). Rendered between the editor and the footer; Tab/Enter completes,
//! Esc dismisses.

use std::path::Path;

use tack_tui::components::select_list::{SelectItem, SelectList, fuzzy_match};

use super::commands::SLASH_COMMANDS;

/// Active autocomplete popup state.
#[derive(Debug)]
pub struct Autocomplete {
    pub list: SelectList,
    /// The token being completed (e.g. "/mod" or "@src/ma").
    pub token: String,
    /// Byte offset where the token starts in the editor text.
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    SlashCommand,
    FilePath,
    /// tack-ext autocomplete provider (v2.2): suggestions came from plugins.
    ExtProvider,
}

/// Extension-provider trigger match (v2.2): does the last
/// whitespace-separated token start with one of these triggers? Returns
/// the token. Built-in '/' and '@' completion takes precedence — the
/// caller only consults this when neither built-in fired.
pub fn ext_trigger_token(text: &str, triggers: &[&str]) -> Option<String> {
    // Same tokenization as compute(): CJK punctuation separates prose from
    // the trigger token (upstream bfa686240 covers all trigger characters).
    let tail = text.rsplit(is_token_separator).next()?;
    triggers
        .iter()
        .any(|t| !t.is_empty() && tail.starts_with(t))
        .then(|| tail.to_string())
}

/// Compute completions for the current editor text + cursor.
/// `templates` are prompt-template command names; `skills` feed `skill:<name>`
/// command completions (already gated by enableSkillCommands).
pub fn compute(
    editor_text: &str,
    templates: &[String],
    skills: &[crate::skills::Skill],
    cwd: &Path,
) -> Option<Autocomplete> {
    // Slash command: text starts with '/', cursor in the first token.
    if let Some(rest) = editor_text.strip_prefix('/') {
        if !rest.contains(char::is_whitespace) {
            let query = rest.to_lowercase();
            let mut items: Vec<SelectItem> = SLASH_COMMANDS
                .iter()
                .filter(|(name, _)| fuzzy_match(&query, &name.to_lowercase()))
                .map(|(name, key)| {
                    SelectItem::new(*name, *name).with_description(crate::i18n::tr(key))
                })
                .collect();
            for template in templates {
                let label = format!("/{template}");
                if fuzzy_match(&query, &label.to_lowercase()) {
                    items.push(
                        SelectItem::new(label.clone(), label)
                            .with_description(crate::i18n::tr("auto.prompt_template")),
                    );
                }
            }
            for skill in skills {
                let label = format!("/skill:{}", skill.name);
                if fuzzy_match(&query, &label.to_lowercase()) {
                    items.push(
                        SelectItem::new(label.clone(), label)
                            .with_description(skill.description.clone()),
                    );
                }
            }
            if items.is_empty() {
                return None;
            }
            return Some(Autocomplete {
                list: SelectList::new(items),
                token: rest.to_string(),
                kind: Kind::SlashCommand,
            });
        }
        return None;
    }

    // File path: last token starts with '@'. CJK punctuation counts as a
    // token separator (upstream bfa686240): "看下，@src/ma" must complete.
    let tail = editor_text.rsplit(is_token_separator).next()?;
    let query = tail.strip_prefix('@')?;
    if query.is_empty() {
        return None;
    }
    let items = complete_paths(cwd, query);
    if items.is_empty() {
        return None;
    }
    Some(Autocomplete {
        list: SelectList::new(items),
        token: tail.to_string(),
        kind: Kind::FilePath,
    })
}

/// CJK punctuation separates prose from an @path token, so Chinese/Japanese
/// text like "看下，@src/ma" still triggers file completion (upstream
/// bfa686240). Mirrors the upstream regex
/// `(?=\p{Punctuation})<cjkBreak>|[，．：；！？（）［］｛｝“”‘’…—]`:
/// the explicit fullwidth forms plus the CJK Symbols and Punctuation block
/// (、。「」【】etc.). The block range is a slight over-approximation (a few
/// rare symbols are not punctuation) but those never appear in real prose or
/// paths, and CJK *letters* stay word/path characters throughout.
fn is_cjk_punctuation(c: char) -> bool {
    // Explicit fullwidth forms from the upstream regex (the CJK Symbols
    // and Punctuation block is covered by the range check below).
    const FULLWIDTH_PUNCT: &[char] = &[
        '\u{FF01}', '\u{FF08}', '\u{FF09}', '\u{FF0C}', '\u{FF0E}', '\u{FF1A}', '\u{FF1B}',
        '\u{FF1F}', '\u{FF3B}', '\u{FF3D}', '\u{FF5B}', '\u{FF5D}', '\u{2018}', '\u{2019}',
        '\u{201C}', '\u{201D}', '\u{2014}', '\u{2026}',
    ];
    ('\u{3001}'..='\u{303F}').contains(&c) || FULLWIDTH_PUNCT.contains(&c)
}

pub(crate) fn is_token_separator(c: char) -> bool {
    c.is_whitespace() || is_cjk_punctuation(c)
}

/// Gitignore-aware file completion (fd-equivalent via `ignore`).
///
/// The cwd tree walk is far too slow for the UI thread — a full recursive
/// walk on a large repo takes 100ms+ and would stutter on the keystroke
/// that hits an expired cache (Termux flash makes it worse). The walked
/// path list is cached for a short TTL and only the fuzzy filter runs per
/// keystroke; when the cache is stale the walk runs on a background thread
/// and its result is atomically swapped in (stale-while-revalidate). The
/// UI thread keeps serving the old data and never blocks on the walk. A
/// cold cache (None / different cwd) yields no items until the first
/// background refresh lands.
fn complete_paths(cwd: &Path, query: &str) -> Vec<SelectItem> {
    let lower = query.to_lowercase();
    let stale = {
        let guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        match &*guard {
            Some(cache) => cache.cwd != cwd || cache.at.elapsed() > PATH_CACHE_TTL,
            None => true,
        }
    };
    if stale {
        spawn_path_refresh(cwd);
    }
    let guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    // An expired same-cwd cache is fine to serve (stale-while-revalidate),
    // but never serve paths walked for a different cwd.
    if let Some(cache) = guard.as_ref().filter(|c| c.cwd == cwd) {
        for (lower_path, display) in &cache.paths {
            if fuzzy_match(&lower, lower_path) {
                out.push(SelectItem::new(format!("@{display}"), display.clone()));
                if out.len() >= 50 {
                    break;
                }
            }
        }
    }
    out.sort_by_key(|a| a.value.len());
    out
}

/// Walk `cwd` into the cached path list. Returns None when the walk can't
/// run at all (missing/unreadable cwd) so the caller keeps the old cache;
/// per-entry errors are skipped via `flatten`, as before.
fn walk_paths(cwd: &Path) -> Option<Vec<(String, String)>> {
    if !cwd.is_dir() {
        return None;
    }
    let mut paths: Vec<(String, String)> = Vec::new();
    let walker = ignore::WalkBuilder::new(cwd)
        .hidden(true)
        .git_ignore(true)
        .max_filesize(Some(8 * 1024 * 1024))
        .build();
    for entry in walker.flatten() {
        let Ok(relative) = entry.path().strip_prefix(cwd) else {
            continue;
        };
        let display = relative.display().to_string().replace('\\', "/");
        paths.push((display.to_lowercase(), display));
        // Cap the walk: past this many entries fuzzy results are noise
        // anyway, and the per-keystroke filter must stay bounded.
        if paths.len() >= PATH_CACHE_MAX {
            break;
        }
    }
    Some(paths)
}

/// True while a background refresh is walking; prevents duplicate walks
/// when a TTL-expired cache is hit on consecutive keystrokes.
static PATH_REFRESH_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Resets PATH_REFRESH_IN_FLIGHT on scope exit, walk panic included.
struct InFlightGuard;
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        PATH_REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Kick off a background walk of `cwd`; on success the result is swapped
/// into PATH_CACHE under a short lock. No-op if a refresh is already
/// running — the stale cache keeps being served in the meantime.
fn spawn_path_refresh(cwd: &Path) {
    use std::sync::atomic::Ordering;
    if PATH_REFRESH_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let cwd = cwd.to_path_buf();
    std::thread::spawn(move || {
        let _in_flight = InFlightGuard;
        // Walk off-thread; lock only for the swap.
        if let Some(paths) = walk_paths(&cwd) {
            let mut guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(PathCache {
                cwd,
                at: std::time::Instant::now(),
                paths,
            });
        }
        // Walk failed: keep the old cache.
    });
}

/// Cached cwd walk for '@' completion (see complete_paths).
struct PathCache {
    cwd: std::path::PathBuf,
    at: std::time::Instant,
    /// Walk-order relative paths: (lowercased, display) — lowercasing is
    /// prepaid once per walk instead of per keystroke.
    paths: Vec<(String, String)>,
}

static PATH_CACHE: std::sync::Mutex<Option<PathCache>> = std::sync::Mutex::new(None);
const PATH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);
const PATH_CACHE_MAX: usize = 20_000;

/// Apply a completion to the editor text: replaces the active token.
pub fn apply(editor_text: &str, auto: &Autocomplete, value: &str) -> String {
    match auto.kind {
        Kind::SlashCommand => {
            // Replace the whole first token.
            format!("/{} ", value.trim_start_matches('/'))
        }
        Kind::FilePath => {
            let token = &auto.token;
            match editor_text.rfind(token.as_str()) {
                Some(pos) => {
                    let mut out = editor_text[..pos].to_string();
                    out.push('@');
                    // Quote paths with spaces.
                    if value.contains(' ') {
                        out.push_str(&format!("\"{value}\""));
                    } else {
                        out.push_str(value);
                    }
                    out
                }
                None => editor_text.to_string(),
            }
        }
        Kind::ExtProvider => {
            // Replace the trigger token (e.g. "#wa") with the accepted
            // suggestion's insert text (insertText ?? value, precomputed
            // into the item value by the merge step).
            match editor_text.rfind(auto.token.as_str()) {
                Some(pos) => format!("{}{value}", &editor_text[..pos]),
                None => editor_text.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// Serializes tests that touch the global PATH_CACHE.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Synchronously refresh PATH_CACHE for `cwd` (tests only — production
    /// refreshes happen on the background thread via spawn_path_refresh).
    fn refresh_path_cache_sync(cwd: &Path) {
        let paths = walk_paths(cwd).expect("test cwd must be walkable");
        let mut guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(PathCache {
            cwd: cwd.to_path_buf(),
            at: Instant::now(),
            paths,
        });
    }

    /// Block until `cond` holds or the deadline passes (then panic).
    fn wait_until(cond: impl Fn() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn cache_has_path(path: &str) -> bool {
        let guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .is_some_and(|c| c.paths.iter().any(|(l, _)| l == path))
    }

    #[test]
    fn slash_command_completion() {
        let auto = compute("/mod", &[], &[], Path::new(".")).unwrap();
        assert!(auto.list.items.iter().any(|i| i.value == "/model"));
        assert!(compute("/model x", &[], &[], Path::new(".")).is_none());
        assert!(compute("hello", &[], &[], Path::new(".")).is_none());
        assert!(compute("/", &[], &[], Path::new(".")).is_some());
    }

    #[test]
    fn slash_apply() {
        let auto = compute("/mo", &[], &[], Path::new(".")).unwrap();
        assert_eq!(apply("/mo", &auto, "/model"), "/model ");
    }

    #[test]
    fn file_completion() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "").unwrap();
        // Cold cache is served empty until the background refresh lands, so
        // tests populate it synchronously first.
        refresh_path_cache_sync(dir.path());
        let auto = compute("check @src", &[], &[], dir.path()).unwrap();
        assert!(auto.list.items.iter().any(|i| i.value == "src/main.rs"));
        let applied = apply("check @src", &auto, "src/main.rs");
        assert_eq!(applied, "check @src/main.rs");
    }

    #[test]
    fn file_completion_after_cjk_punctuation() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "").unwrap();
        refresh_path_cache_sync(dir.path());
        // CJK punctuation (，。、！？：；「」【】 etc.) is a token separator:
        // the @path after it still completes, and apply replaces only the
        // @token, keeping the prose.
        for text in ["看下，@src", "总结。@src", "读一下、@src"] {
            let auto = compute(text, &[], &[], dir.path())
                .unwrap_or_else(|| panic!("no completion for {text:?}"));
            assert_eq!(auto.token, "@src");
            assert!(auto.list.items.iter().any(|i| i.value == "src/main.rs"));
            let applied = apply(text, &auto, "src/main.rs");
            assert!(applied.ends_with("@src/main.rs"), "applied: {applied}");
            assert!(applied.len() > "@src/main.rs".len());
        }
        // CJK letters are NOT separators: a bare CJK token is not a path.
        assert!(compute("看下", &[], &[], dir.path()).is_none());
    }

    #[test]
    fn ext_trigger_after_cjk_punctuation() {
        // Plugin triggers use the same tokenization: "翻译，#wa" fires #wa.
        assert_eq!(
            ext_trigger_token("翻译，#wa", &["#"]),
            Some("#wa".to_string())
        );
        assert_eq!(
            ext_trigger_token("总结。#note", &["#"]),
            Some("#note".to_string())
        );
        assert_eq!(ext_trigger_token("看下", &["#"]), None);
    }

    #[test]
    fn file_completion_quotes_spaces() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my file.txt"), "").unwrap();
        refresh_path_cache_sync(dir.path());
        let auto = compute("@my", &[], &[], dir.path()).unwrap();
        let applied = apply("@my", &auto, "my file.txt");
        assert_eq!(applied, "@\"my file.txt\"");
    }

    /// Stale-while-revalidate: with a refresh in flight, an expired cache
    /// is still served and no duplicate refresh is spawned.
    #[test]
    fn stale_cache_served_and_not_duplicated_while_refresh_in_flight() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        PATH_REFRESH_IN_FLIGHT.store(false, Ordering::Release);
        let dir = tempfile::tempdir().unwrap();
        {
            let mut guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(PathCache {
                cwd: dir.path().to_path_buf(),
                at: Instant::now() - PATH_CACHE_TTL - Duration::from_secs(1),
                paths: vec![("old.txt".into(), "old.txt".into())],
            });
        }
        // Simulate a refresh already running: complete_paths must serve the
        // stale data and must NOT spawn another refresh.
        PATH_REFRESH_IN_FLIGHT.store(true, Ordering::Release);
        let items = complete_paths(dir.path(), "old");
        assert!(items.iter().any(|i| i.value == "old.txt"));
        // Cache untouched (no synchronous rewalk on the UI thread).
        let guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.as_ref().unwrap();
        assert!(cache.at.elapsed() > PATH_CACHE_TTL);
        assert_eq!(cache.paths.len(), 1);
        drop(guard);
        PATH_REFRESH_IN_FLIGHT.store(false, Ordering::Release);
    }

    /// A TTL-expired cache triggers a background refresh that atomically
    /// swaps in freshly walked data.
    #[test]
    fn background_refresh_swaps_in_fresh_cache() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        PATH_REFRESH_IN_FLIGHT.store(false, Ordering::Release);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.rs"), "").unwrap();
        {
            let mut guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(PathCache {
                cwd: dir.path().to_path_buf(),
                at: Instant::now() - PATH_CACHE_TTL - Duration::from_secs(1),
                paths: vec![("old.txt".into(), "old.txt".into())],
            });
        }
        // Stale data is served immediately...
        let items = complete_paths(dir.path(), "old");
        assert!(items.iter().any(|i| i.value == "old.txt"));
        // ...while the background thread walks and swaps in fresh data.
        wait_until(
            || cache_has_path("alpha.rs") && !PATH_REFRESH_IN_FLIGHT.load(Ordering::Acquire),
            "background refresh",
        );
        let items = complete_paths(dir.path(), "alpha");
        assert!(items.iter().any(|i| i.value == "alpha.rs"));
        let items = complete_paths(dir.path(), "old");
        assert!(!items.iter().any(|i| i.value == "old.txt"));
    }

    /// A failed walk (missing cwd) must keep the old cache intact.
    #[test]
    fn failed_refresh_keeps_old_cache() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        PATH_REFRESH_IN_FLIGHT.store(false, Ordering::Release);
        {
            let mut guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(PathCache {
                cwd: Path::new("/old").to_path_buf(),
                at: Instant::now(),
                paths: vec![("keep.txt".into(), "keep.txt".into())],
            });
        }
        spawn_path_refresh(Path::new("/definitely-not-a-real-dir-xyz-123"));
        wait_until(
            || !PATH_REFRESH_IN_FLIGHT.load(Ordering::Acquire),
            "refresh to finish",
        );
        let guard = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.as_ref().unwrap();
        assert_eq!(cache.cwd, Path::new("/old"));
        assert_eq!(cache.paths.len(), 1);
        assert_eq!(cache.paths[0].1, "keep.txt");
    }

    /// v2.2: extension providers trigger on a trigger-prefixed last token
    /// (any position); the token can also be the bare trigger.
    #[test]
    fn ext_provider_trigger_detection() {
        let triggers = ["#", "!"];
        assert_eq!(
            ext_trigger_token("fix #wa", &triggers).as_deref(),
            Some("#wa")
        );
        assert_eq!(
            ext_trigger_token("/cmd #q", &triggers).as_deref(),
            Some("#q")
        );
        // Trigger alone (empty query) still fires.
        assert_eq!(ext_trigger_token("#", &triggers).as_deref(), Some("#"));
        // No trigger prefix → no match.
        assert!(ext_trigger_token("plain text", &triggers).is_none());
        assert!(ext_trigger_token("", &triggers).is_none());
        // Empty triggers never match.
        assert!(ext_trigger_token("fix #wa", &[""]).is_none());
    }

    /// v2.2: accepting a suggestion replaces the trigger token with the
    /// insert text.
    #[test]
    fn ext_provider_apply_replaces_token() {
        let auto = Autocomplete {
            list: SelectList::new(vec![SelectItem::new("#wasm WASM", "#wasm ")]),
            token: "#wa".to_string(),
            kind: Kind::ExtProvider,
        };
        assert_eq!(apply("fix #wa", &auto, "#wasm "), "fix #wasm ");
        assert_eq!(apply("#wa", &auto, "#wasm "), "#wasm ");
        // Token no longer present (stale popup): text unchanged.
        assert_eq!(apply("fix bug", &auto, "#wasm "), "fix bug");
    }
}
