//! Persistent cross-session memory (Claude-Code-style auto memory). The agent
//! proactively records durable facts into two scopes:
//!
//! - **user** — cross-project preferences (the user, how they work). Root:
//!   `TACK_MEMORY_DIR` > settings `memoryDirectory` > `<agent dir>/memory`.
//! - **project** (default) — conventions/decisions/environment quirks of THIS
//!   repository. Lives at `<user root>/projects/<encoded repo>/`, keyed by the
//!   main working-tree root so all worktrees of a repo share it.
//!
//! Each scope's MEMORY.md index is injected into the system prompt of every
//! future session. Indexes are capped (200 lines / 25KB of entries) like
//! Claude Code: reads truncate with an explicit warning, writes that push the
//! index over the cap fail with an error telling the agent to compact.
//!
//! Layout (compatible with Claude-Code-style auto-memory):
//!   memory/MEMORY.md   index, one line per memory: `- [name](name.md) — description`
//!   `memory/<name>.md`   frontmatter (name/description/modified) + markdown body

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::services::ToolServices;

/// Read caps for a MEMORY.md index (Claude Code loads the first 200 lines or
/// 25KB). Counted over entry lines (`- […](…) — …`), not the header.
pub const INDEX_MAX_LINES: usize = 200;
pub const INDEX_MAX_BYTES: usize = 25 * 1024;
/// Past this fraction of a cap, the system prompt nudges the agent to
/// compact the index (merge/prune entries) before writes start failing.
const INDEX_NEAR_LIMIT: f64 = 0.8;

fn default_agent_dir() -> PathBuf {
    std::env::var_os("TACK_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".tack")
                .join("agent")
        })
}

/// User-scope memory root. `TACK_MEMORY_DIR` env wins over the settings
/// override (matches the rest of tack: env > settings > default).
pub fn user_memory_dir(agent_dir: &Path, override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = std::env::var_os("TACK_MEMORY_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    override_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| agent_dir.join("memory"))
}

/// Resolved memory locations for a working directory.
#[derive(Clone, Debug)]
pub struct MemoryDirs {
    /// Cross-project (user) scope root.
    pub user: PathBuf,
    /// Per-repository scope (default for new memories); shared across
    /// worktrees of the same repository.
    pub project: PathBuf,
}

pub fn resolve_dirs(agent_dir: &Path, cwd: &Path, override_dir: Option<&Path>) -> MemoryDirs {
    let user = user_memory_dir(agent_dir, override_dir);
    let project = user.join("projects").join(project_key(cwd));
    MemoryDirs { user, project }
}

/// Stable key for a project: the repository's main working-tree root when
/// inside git (worktree-shared), else the cwd itself. Encoded like
/// `tack_session::default_session_dir` (strip leading slash, replace / \ :).
fn project_key(cwd: &Path) -> String {
    let root = repo_main_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    encode_path(&root)
}

fn encode_path(path: &Path) -> String {
    let mut encoded = path
        .to_string_lossy()
        .replace('\\', "/")
        .replace(['/', ':'], "-");
    if let Some(stripped) = encoded.strip_prefix('-') {
        encoded = stripped.to_string();
    }
    encoded
}

/// Main working-tree root of the repository containing `cwd` (worktrees of
/// the same repo all resolve to the main root). None outside a repository.
fn repo_main_root(cwd: &Path) -> Option<PathBuf> {
    // Fast path: ask git. --git-common-dir points at the MAIN worktree's
    // .git even from a linked worktree; its parent is the main root.
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(cwd)
        .output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            let gitdir = PathBuf::from(&s);
            if let Some(root) = gitdir.parent() {
                return Some(root.to_path_buf());
            }
        }
    }
    // Fallback: walk up for a `.git` entry (git may be unavailable).
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let dotgit = d.join(".git");
        if dotgit.is_dir() {
            return Some(d.to_path_buf());
        }
        if dotgit.is_file()
            && let Ok(content) = std::fs::read_to_string(&dotgit)
            && let Some(target) = content.trim().strip_prefix("gitdir:")
        {
            let target = target.trim();
            let gitdir = if Path::new(target).is_absolute() {
                PathBuf::from(target)
            } else {
                d.join(target)
            };
            // Linked worktrees and submodules point into the main repo's
            // `<main>/.git/…`; the part before `/.git/` is the main root.
            let s = gitdir.to_string_lossy().replace('\\', "/");
            if let Some(idx) = s.find("/.git/") {
                return Some(PathBuf::from(&s[..idx]));
            }
            return gitdir
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf);
        }
        dir = d.parent();
    }
    None
}

/// A MEMORY.md index capped for prompt injection.
#[derive(Clone, Debug)]
pub struct MemoryIndex {
    /// Entry lines (truncated at the caps) + optional truncation warning.
    pub text: String,
    /// Total entry lines / bytes in the full index (pre-truncation).
    pub total_lines: usize,
    pub total_bytes: usize,
    pub truncated: bool,
}

impl MemoryIndex {
    /// ≥80% of a cap: nudge the agent to compact before writes fail.
    pub fn near_limit(&self) -> bool {
        self.total_lines as f64 >= INDEX_MAX_LINES as f64 * INDEX_NEAR_LIMIT
            || self.total_bytes as f64 >= INDEX_MAX_BYTES as f64 * INDEX_NEAR_LIMIT
    }

    /// Over the read caps: writes must fail loudly instead of silently
    /// truncating what future sessions see.
    pub fn over_limit(&self) -> bool {
        self.total_lines > INDEX_MAX_LINES || self.total_bytes > INDEX_MAX_BYTES
    }
}

/// Read the MEMORY.md index of one scope. Returns None when empty/missing.
pub fn read_index(dir: &Path) -> Option<MemoryIndex> {
    let content = std::fs::read_to_string(dir.join("MEMORY.md")).ok()?;
    let entries: Vec<&str> = content.lines().filter(|l| l.starts_with("- [")).collect();
    if entries.is_empty() {
        return None;
    }
    let total_lines = entries.len();
    let total_bytes: usize = entries.iter().map(|l| l.len() + 1).sum();
    let mut shown: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in &entries {
        if shown.len() >= INDEX_MAX_LINES || bytes + line.len() + 1 > INDEX_MAX_BYTES {
            break;
        }
        shown.push(line);
        bytes += line.len() + 1;
    }
    let truncated = shown.len() < total_lines;
    let mut text = shown.join("\n");
    if truncated {
        text.push_str(&format!(
            "\n(… truncated: {} more entries from entry {} — read {}/MEMORY.md)",
            total_lines - shown.len(),
            shown.len() + 1,
            dir.display()
        ));
    }
    Some(MemoryIndex {
        text,
        total_lines,
        total_bytes,
        truncated,
    })
}

/// (lines, bytes) of index entries, for post-write limit enforcement.
fn index_stats(dir: &Path) -> (usize, usize) {
    let Ok(content) = std::fs::read_to_string(dir.join("MEMORY.md")) else {
        return (0, 0);
    };
    let entries = content.lines().filter(|l| l.starts_with("- ["));
    entries.fold((0, 0), |(n, b), l| (n + 1, b + l.len() + 1))
}

fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim().trim_end_matches(".md").to_string();
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "invalid memory name {name:?}: use lowercase kebab-case (a-z, 0-9, -), max 64 chars"
        ));
    }
    // `memory.md` collides with the `MEMORY.md` index on case-insensitive
    // filesystems (macOS/Windows): saving it would clobber the index.
    if name.eq_ignore_ascii_case("memory") {
        return Err("invalid memory name \"memory\": reserved for the MEMORY.md index".to_string());
    }
    Ok(name)
}

fn memory_file(name: &str, description: &str, content: &str) -> String {
    let modified = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!(
        "---\nname: {name}\ndescription: {description}\nmodified: {modified}\n---\n\n{}\n",
        content.trim()
    )
}

/// Rewrite the MEMORY.md index from the current set of memory files.
pub fn rebuild_index(dir: &Path) -> Result<(), String> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let read_dir = std::fs::read_dir(dir).map_err(|e| format!("cannot read memory dir: {e}"))?;
    for entry in read_dir {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") || name == "MEMORY.md" {
            continue;
        }
        let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let description = parse_frontmatter(&content, "description")
            .unwrap_or_else(|| "(no description)".to_string());
        let slug = name.trim_end_matches(".md").to_string();
        entries.push((slug, description));
    }
    entries.sort();
    let mut index = String::from("# Memory Index\n\n");
    for (slug, description) in entries {
        index.push_str(&format!("- [{slug}]({slug}.md) — {description}\n"));
    }
    std::fs::write(dir.join("MEMORY.md"), index).map_err(|e| format!("cannot write index: {e}"))
}

fn parse_frontmatter(content: &str, key: &str) -> Option<String> {
    let content = content.strip_prefix("---")?;
    let front = content.split("---").next()?;
    for line in front.lines() {
        if let Some(value) = line.trim_start().strip_prefix(&format!("{key}:")) {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Write succeeded but the index is over the read caps: future sessions
/// would silently lose the tail, so fail loudly with compaction guidance.
fn enforce_index_limit(dir: &Path) -> Result<(), String> {
    let (lines, bytes) = index_stats(dir);
    if lines > INDEX_MAX_LINES || bytes > INDEX_MAX_BYTES {
        return Err(format!(
            "memory index at {} is over its limit ({lines}/{INDEX_MAX_LINES} entries, \
             {bytes}/{INDEX_MAX_BYTES} bytes) — future sessions would not see all of it. \
             Compact first: merge related entries into fewer files, delete obsolete ones, \
             then retry.",
            dir.display()
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryParams {
    /// save: create/overwrite a memory. delete: remove one. list: show all.
    action: String,
    /// Kebab-case memory name (required for save/delete), e.g. "user-prefers-pnpm"
    name: Option<String>,
    /// One-line summary shown in the index (required for save)
    description: Option<String>,
    /// Markdown body (required for save)
    content: Option<String>,
    /// "project" (default): this repository's memory, shared across its
    /// worktrees. "user": cross-project preferences about the user.
    scope: Option<String>,
}

pub struct MemoryTool {
    #[allow(dead_code)]
    services: ToolServices,
    dirs: MemoryDirs,
}

impl MemoryTool {
    pub fn new(services: ToolServices) -> Self {
        let dirs = resolve_dirs(
            &default_agent_dir(),
            &services.cwd,
            services.memory_dir_override.as_deref(),
        );
        MemoryTool { services, dirs }
    }

    #[cfg(test)]
    pub fn with_dirs(services: ToolServices, dirs: MemoryDirs) -> Self {
        MemoryTool { services, dirs }
    }

    fn scope_dir(&self, scope: Option<&str>) -> Result<&Path, String> {
        match scope.map(str::trim) {
            None | Some("") | Some("project") => Ok(&self.dirs.project),
            Some("user") => Ok(&self.dirs.user),
            Some(other) => Err(format!(
                "unknown scope {other:?} (\"project\" default | \"user\")"
            )),
        }
    }
}

impl std::fmt::Debug for MemoryTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTool").finish()
    }
}

#[async_trait]
impl AgentTool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }
    fn label(&self) -> &str {
        "memory"
    }
    fn description(&self) -> &str {
        "Read and write persistent memory across sessions. Two scopes: \"project\" (default) \
         for conventions, decisions, and environment quirks of THIS repository (shared across \
         its worktrees), and \"user\" for cross-project facts about the user (preferences, \
         role, how they want you to work). Save proactively when you learn something durable; \
         the indexes of both scopes are already in your system prompt — read a memory file \
         with the read tool for details. Do NOT save things already recorded in the repo \
         (code structure, CLAUDE.md content, git history) or session-specific state."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<MemoryParams>()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: MemoryParams =
            serde_json::from_value(params).map_err(|e| format!("invalid memory params: {e}"))?;

        match params.action.as_str() {
            "list" => {
                let mut out = String::new();
                for (label, dir) in [("project", &self.dirs.project), ("user", &self.dirs.user)] {
                    let index = read_index(dir)
                        .map(|i| i.text)
                        .unwrap_or_else(|| "(empty)".to_string());
                    out.push_str(&format!("## {label} ({})\n{index}\n\n", dir.display()));
                }
                Ok(AgentToolResult::text(out.trim_end().to_string()))
            }
            "save" => {
                let dir = self.scope_dir(params.scope.as_deref())?;
                let name = validate_name(params.name.as_deref().unwrap_or(""))?;
                let description = params
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .ok_or("save requires a one-line description")?;
                // The description is embedded in YAML-ish frontmatter and in
                // the one-line-per-entry MEMORY.md index — newlines or a
                // frontmatter delimiter would corrupt both.
                if description.contains(['\n', '\r']) || description.contains("---") {
                    return Err(
                        "description must be a single line and must not contain \"---\""
                            .to_string(),
                    );
                }
                let content = params
                    .content
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .ok_or("save requires content")?;
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("cannot create memory dir: {e}"))?;
                let path = dir.join(format!("{name}.md"));
                let existed = path.exists();
                std::fs::write(&path, memory_file(&name, description, content))
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                rebuild_index(dir)?;
                enforce_index_limit(dir)?;
                Ok(AgentToolResult::text(format!(
                    "Memory {name} {} ({}).",
                    if existed { "updated" } else { "saved" },
                    path.display()
                )))
            }
            "delete" => {
                let name = validate_name(params.name.as_deref().unwrap_or(""))?;
                // Explicit scope wins; otherwise project first, then user.
                let dir = match params.scope.as_deref().map(str::trim) {
                    Some("") | None => {
                        if self.dirs.project.join(format!("{name}.md")).exists() {
                            &self.dirs.project
                        } else {
                            &self.dirs.user
                        }
                    }
                    Some("project") => &self.dirs.project,
                    Some("user") => &self.dirs.user,
                    Some(other) => {
                        return Err(format!(
                            "unknown scope {other:?} (\"project\" default | \"user\")"
                        ));
                    }
                };
                let path = dir.join(format!("{name}.md"));
                if !path.exists() {
                    return Err(format!("no memory named {name}"));
                }
                std::fs::remove_file(&path)
                    .map_err(|e| format!("cannot delete {}: {e}", path.display()))?;
                rebuild_index(dir)?;
                Ok(AgentToolResult::text(format!("Memory {name} deleted.")))
            }
            other => Err(format!("unknown action {other:?} (save|delete|list)")),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tool(user: PathBuf, project: PathBuf) -> MemoryTool {
        MemoryTool::with_dirs(
            crate::default_services(std::env::current_dir().unwrap()),
            MemoryDirs { user, project },
        )
    }

    fn dirs(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        (
            tmp.path().join("memory"),
            tmp.path().join("memory").join("projects").join("repo"),
        )
    }

    #[tokio::test]
    async fn save_list_delete_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let (user, project) = dirs(&tmp);
        let tool = tool(user.clone(), project.clone());

        tool.execute(
            "1",
            serde_json::json!({
                "action": "save",
                "name": "user-prefers-pnpm",
                "description": "User prefers pnpm over npm",
                "content": "All package installs should use pnpm."
            }),
            CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();

        // Default scope is project.
        let index = read_index(&project).unwrap();
        assert!(
            index
                .text
                .contains("[user-prefers-pnpm](user-prefers-pnpm.md)")
        );
        assert!(index.text.contains("pnpm over npm"));
        assert!(!index.truncated);
        assert!(read_index(&user).is_none());

        // Frontmatter carries a modified timestamp.
        let file = std::fs::read_to_string(project.join("user-prefers-pnpm.md")).unwrap();
        assert!(parse_frontmatter(&file, "modified").is_some());

        // Invalid names are rejected.
        assert!(
            tool.execute(
                "2",
                serde_json::json!({ "action": "save", "name": "../evil", "description": "x", "content": "y" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .is_err()
        );

        tool.execute(
            "3",
            serde_json::json!({ "action": "delete", "name": "user-prefers-pnpm" }),
            CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
        assert!(read_index(&project).is_none());
    }

    #[tokio::test]
    async fn user_scope_roundtrip_and_scope_fallback_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let (user, project) = dirs(&tmp);
        let tool = tool(user.clone(), project.clone());

        tool.execute(
            "1",
            serde_json::json!({
                "action": "save",
                "scope": "user",
                "name": "editor-pref",
                "description": "Uses vim bindings",
                "content": "Everywhere, all editors."
            }),
            CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
        assert!(read_index(&user).is_some());
        assert!(read_index(&project).is_none());

        // Unscoped delete falls back to the user scope.
        tool.execute(
            "2",
            serde_json::json!({ "action": "delete", "name": "editor-pref" }),
            CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
        assert!(read_index(&user).is_none());

        // Unknown scopes are rejected.
        assert!(
            tool.execute(
                "3",
                serde_json::json!({ "action": "save", "scope": "global", "name": "x", "description": "d", "content": "c" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn index_truncates_with_warning_at_caps() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("memory");
        std::fs::create_dir_all(&dir).unwrap();
        let mut index = String::from("# Memory Index\n\n");
        for i in 0..250 {
            index.push_str(&format!(
                "- [entry-{i:03}](entry-{i:03}.md) — description {i}\n"
            ));
        }
        std::fs::write(dir.join("MEMORY.md"), index).unwrap();

        let index = read_index(&dir).unwrap();
        assert!(index.truncated);
        assert_eq!(index.total_lines, 250);
        assert!(index.text.contains("truncated"));
        assert!(index.text.contains("50 more entries"));
        assert!(index.near_limit());
        assert!(index.over_limit());
    }

    #[tokio::test]
    async fn save_fails_loudly_when_index_over_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let (user, project) = dirs(&tmp);
        std::fs::create_dir_all(&project).unwrap();
        let mut index = String::from("# Memory Index\n\n");
        for i in 0..=INDEX_MAX_LINES {
            index.push_str(&format!(
                "- [seed-{i:03}](seed-{i:03}.md) — seeded entry {i}\n"
            ));
        }
        std::fs::write(project.join("MEMORY.md"), &index).unwrap();
        // rebuild_index would regenerate from files, so seed real files too.
        for i in 0..=INDEX_MAX_LINES {
            std::fs::write(
                project.join(format!("seed-{i:03}.md")),
                memory_file(&format!("seed-{i:03}"), &format!("seeded entry {i}"), "x"),
            )
            .unwrap();
        }

        let tool = tool(user, project);
        let err = tool
            .execute(
                "1",
                serde_json::json!({
                    "action": "save",
                    "name": "one-more",
                    "description": "pushes the index over",
                    "content": "body"
                }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(err.contains("over its limit"), "unexpected error: {err}");
        assert!(err.contains("Compact first"));
    }

    /// Regression: a memory named "memory" becomes `memory.md`, which is
    /// the same file as the `MEMORY.md` index on case-insensitive
    /// filesystems (macOS/Windows) — saving it would clobber the index.
    #[tokio::test]
    async fn name_colliding_with_index_file_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (user, project) = dirs(&tmp);
        let tool = tool(user, project);
        for bad in ["memory", "MEMORY", "Memory", "memory.md"] {
            let result = tool
                .execute(
                    "1",
                    serde_json::json!({
                        "action": "save",
                        "name": bad,
                        "description": "x",
                        "content": "y",
                    }),
                    CancellationToken::new(),
                    &|_| {},
                )
                .await;
            assert!(result.is_err(), "name {bad:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn description_with_newline_or_delimiter_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (user, project) = dirs(&tmp);
        let tool = tool(user, project.clone());
        for bad in [
            "first line\nsecond line",
            "first line\n---\nevil: injected",
            "a --- b",
        ] {
            let result = tool
                .execute(
                    "1",
                    serde_json::json!({
                        "action": "save",
                        "name": "bad-desc",
                        "description": bad,
                        "content": "body",
                    }),
                    CancellationToken::new(),
                    &|_| {},
                )
                .await;
            assert!(result.is_err(), "description {bad:?} should be rejected");
        }
        assert!(!project.join("bad-desc.md").exists());
    }

    /// Descriptions may contain '#': the frontmatter parser must not treat
    /// it as a comment starter (Claude Code had this bug).
    #[test]
    fn frontmatter_values_keep_hash_characters() {
        let file = memory_file("hash-test", "fix #42 regression", "body");
        assert_eq!(
            parse_frontmatter(&file, "description").as_deref(),
            Some("fix #42 regression")
        );
    }

    #[test]
    fn encode_path_matches_session_dir_encoding() {
        assert_eq!(
            encode_path(Path::new("/data/github/tack")),
            "data-github-tack"
        );
    }

    #[test]
    fn repo_main_root_walks_up_for_dotgit_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        // The git CLI fast path may also answer; both must agree on `repo`.
        assert_eq!(repo_main_root(&nested), Some(repo));
    }

    #[test]
    fn repo_main_root_resolves_worktree_gitdir_file() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("wt")).unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let gitdir = main.join(".git").join("worktrees").join("wt");
        std::fs::write(wt.join(".git"), format!("gitdir: {}", gitdir.display())).unwrap();
        assert_eq!(repo_main_root(&wt), Some(main));
    }
}
