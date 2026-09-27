//! Shared CLI flag bundle (TS `cli/args.ts` parity) and the helpers that
//! apply them: session resolution, tool filtering, resource toggles.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use tack_agent_core::AgentTool;
use tack_session::SessionManager;

/// The built-in coding tools (for --no-builtin-tools / --tools filtering).
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "bash",
    "powershell",
    "bash_output",
    "bash_wait",
    "kill_shell",
    "edit",
    "write",
    "grep",
    "find",
    "ls",
    "web_fetch",
    "web_search",
    "subagent",
    "todo",
    "memory",
    "lsp",
    "ask_codebuddy",
    "ask_user",
    "session_search",
];

/// CLI flags shared by the print and TUI paths.
#[derive(Clone, Debug, Default)]
pub struct CliFlags {
    /// Open the session picker (TUI).
    pub resume: bool,
    /// Session file path or partial session id.
    pub session: Option<String>,
    /// Exact session id (created if missing).
    pub session_id: Option<String>,
    /// Fork a session file/partial id into a new session.
    pub fork: Option<String>,
    /// Session display name.
    pub name: Option<String>,
    /// Don't persist the session.
    pub no_session: bool,
    /// Scoped-models list for ctrl+p cycling.
    pub models: Vec<String>,
    /// Tool allowlist (empty = all).
    pub tools: Vec<String>,
    pub exclude_tools: Vec<String>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub no_skills: bool,
    pub no_context_files: bool,
    pub no_prompt_templates: bool,
    pub no_themes: bool,
    /// Print output as JSONL events.
    pub mode_json: bool,
    pub tui_mode: Option<String>,
    pub use_theme: Option<String>,
    /// Appended to the system prompt (file contents when a path exists).
    pub append_system_prompt: Vec<String>,
    /// Extra skill directories to load.
    pub skill_paths: Vec<PathBuf>,
    /// Additional working directories (multi-root sessions).
    pub add_dirs: Vec<PathBuf>,
    /// Copy the session file here when the run/session ends.
    pub export: Option<PathBuf>,
    /// Block non-LLM network access (gist share, OAuth login flows).
    pub offline: bool,
    /// Project trust override (approve/no-approve).
    pub approve: Option<bool>,
}

/// Open (or create) the session described by the flags. Precedence:
/// no-session > --session > --session-id > --fork > continue > create.
/// `backend` comes from settings.sessionBackend (jsonl default).
///
/// Sessions are opened FOR CONTINUATION here (TUI, print mode), so any
/// tool calls left dangling by an interrupted previous run are repaired
/// (`SessionManager::repair_dangling_tool_calls`).
pub fn open_session_for_flags(
    cwd: &std::path::Path,
    session_dir: Option<PathBuf>,
    continue_session: bool,
    flags: &CliFlags,
    backend: tack_session::SessionBackend,
) -> Result<SessionManager> {
    let mut session =
        open_session_for_flags_inner(cwd, session_dir, continue_session, flags, backend)?;
    match session.repair_dangling_tool_calls() {
        Ok(0) => {}
        Ok(n) => tracing::info!("repaired {n} dangling tool call(s) from an interrupted run"),
        Err(e) => tracing::warn!("failed to repair dangling tool calls: {e}"),
    }
    Ok(session)
}

fn open_session_for_flags_inner(
    cwd: &std::path::Path,
    session_dir: Option<PathBuf>,
    continue_session: bool,
    flags: &CliFlags,
    backend: tack_session::SessionBackend,
) -> Result<SessionManager> {
    if flags.no_session {
        return Ok(SessionManager::in_memory(cwd));
    }
    let dir = session_dir.unwrap_or_else(|| {
        tack_session::default_session_dir(cwd, &tack_session::default_agent_dir())
    });
    if let Some(arg) = &flags.session {
        if backend == tack_session::SessionBackend::Sqlite {
            return SessionManager::open_sqlite(arg, &dir).map_err(anyhow::Error::from);
        }
        let path = tack_session::resolve_session_arg(arg, &dir)
            .with_context(|| format!("no session file/unique id prefix matching {arg:?}"))?;
        return SessionManager::open(&path, Some(dir)).map_err(anyhow::Error::from);
    }
    if let Some(id) = &flags.session_id {
        if backend == tack_session::SessionBackend::Sqlite {
            return match SessionManager::open_sqlite(id, &dir) {
                Ok(session) => Ok(session),
                Err(_) => {
                    SessionManager::create_with_id_and_backend(cwd, Some(dir), id.clone(), backend)
                        .map_err(anyhow::Error::from)
                }
            };
        }
        // Exact-ID lookup reads only each session file's header line
        // instead of deserializing every transcript body (pi #9440).
        let existing = tack_session::find_session_by_id(&dir, id);
        if let Some(path) = existing {
            return SessionManager::open(&path, Some(dir)).map_err(anyhow::Error::from);
        }
        return SessionManager::create_with_id(cwd, Some(dir), id.clone())
            .map_err(anyhow::Error::from);
    }
    if let Some(arg) = &flags.fork {
        if backend == tack_session::SessionBackend::Sqlite {
            return SessionManager::fork_from_sqlite(arg, cwd, &dir).map_err(anyhow::Error::from);
        }
        let path = tack_session::resolve_session_arg(arg, &dir)
            .with_context(|| format!("no session file/unique id prefix matching {arg:?}"))?;
        return SessionManager::fork_from(&path, cwd).map_err(anyhow::Error::from);
    }
    if continue_session {
        if backend == tack_session::SessionBackend::Sqlite
            && let Some(session) = SessionManager::continue_recent_sqlite(&dir)?
        {
            return Ok(session);
        }
        return SessionManager::continue_recent(cwd, Some(dir)).map_err(anyhow::Error::from);
    }
    match backend {
        tack_session::SessionBackend::Sqlite | tack_session::SessionBackend::JsonlV4 => {
            SessionManager::create_with_backend(cwd, Some(dir), backend)
                .map_err(anyhow::Error::from)
        }
        tack_session::SessionBackend::Jsonl => {
            SessionManager::create_with_backend(cwd, Some(dir), tack_session::SessionBackend::Jsonl)
                .map_err(anyhow::Error::from)
        }
    }
}

/// (active, deferred-pool) pair from split_for_tool_search.
pub type ToolSplit = (Vec<Arc<dyn AgentTool>>, Vec<Arc<dyn AgentTool>>);

/// Split tools for client-side tool search (settings `mcpDeferThreshold`):
/// when the total tool count exceeds the threshold, MCP tools defer to a
/// pool that is invisible to the model until `tool_search` activates them
/// (the loadout change is recorded as a transcript system message).
/// Returns (active_tools, deferred_pool).
pub fn split_for_tool_search(tools: Vec<Arc<dyn AgentTool>>, threshold: usize) -> ToolSplit {
    if threshold == 0 || tools.len() <= threshold {
        return (tools, Vec::new());
    }
    let (deferred, mut active): (Vec<_>, Vec<_>) = tools
        .into_iter()
        .partition(|t| t.name().starts_with("mcp__"));
    if deferred.is_empty() {
        return (active, Vec::new());
    }
    let pool_entries: Vec<tack_tools::tool_search::PoolEntry> = deferred
        .iter()
        .map(|t| tack_tools::tool_search::PoolEntry {
            name: t.name().to_string(),
            description: t.description().to_string(),
        })
        .collect();
    active.push(Arc::new(tack_tools::tool_search::ToolSearchTool::new(
        pool_entries,
    )));
    (active, deferred)
}

/// Remove tools owned by disabled features (settings `features.*`). A
/// disabled feature's tools are never registered — the model cannot see
/// them (no schema, no snippet in the system prompt).
pub fn filter_feature_tools(
    tools: Vec<Arc<dyn AgentTool>>,
    features: &crate::settings::FeatureFlags,
) -> Vec<Arc<dyn AgentTool>> {
    tools
        .into_iter()
        .filter(|t| match t.name() {
            "lsp" => features.lsp,
            "memory" => features.memory,
            "bash_output" | "bash_wait" | "kill_shell" => features.background_tasks,
            _ => true,
        })
        .collect()
}

/// Apply --no-tools/--no-builtin-tools/--tools/--exclude-tools to a tool set.
pub fn filter_tools(tools: Vec<Arc<dyn AgentTool>>, flags: &CliFlags) -> Vec<Arc<dyn AgentTool>> {
    if flags.no_tools {
        return Vec::new();
    }
    tools
        .into_iter()
        .filter(|tool| {
            let name = tool.name();
            if flags.no_builtin_tools && BUILTIN_TOOL_NAMES.contains(&name) {
                return false;
            }
            if !flags.tools.is_empty() && !flags.tools.iter().any(|t| t == name) {
                return false;
            }
            !flags.exclude_tools.iter().any(|t| t == name)
        })
        .collect()
}

/// settings.defaultTools: built-in tool allowlist (MCP/extension tools
/// unaffected) plus opt-in registration of the optional `powershell` tool.
///
/// TS parity (sdk.ts): the active built-ins are exactly `defaultTools` when
/// set, `read/bash/edit/write` otherwise — so the PowerShell tool is OFF by
/// default and appears only when explicitly listed. Like TS, registration is
/// platform-independent: off-Windows the tool is created but its execution
/// fails with "only available on Windows".
pub fn apply_default_tools(
    tools: Vec<Arc<dyn AgentTool>>,
    default_tools: &[String],
    services: &tack_tools::ToolServices,
) -> Vec<Arc<dyn AgentTool>> {
    if default_tools.is_empty() {
        return tools;
    }
    let mut tools: Vec<Arc<dyn AgentTool>> = tools
        .into_iter()
        .filter(|t| {
            !BUILTIN_TOOL_NAMES.contains(&t.name()) || default_tools.iter().any(|d| d == t.name())
        })
        .collect();
    if default_tools.iter().any(|d| d == "powershell")
        && !tools.iter().any(|t| t.name() == "powershell")
    {
        tools.push(Arc::new(tack_tools::powershell::PowerShellTool::new(
            services.clone(),
        )));
    }
    tools
}

/// Copy the persisted session file to the --export path (no-op for in-memory
/// sessions or when already at that path).
pub fn export_session_file(session: &SessionManager, target: &std::path::Path) -> Result<()> {
    let Some(source) = session.session_file() else {
        anyhow::bail!("session is not persisted (--no-session); nothing to export");
    };
    if source != target {
        std::fs::copy(source, target).with_context(|| format!("export to {}", target.display()))?;
    }
    Ok(())
}

/// Apply the session name (--name) after opening/creating.
pub fn apply_session_name(session: &mut SessionManager, name: &Option<String>) {
    if let Some(name) = name
        && !name.trim().is_empty()
        && let Err(e) = session.append_session_info(Some(name.trim().to_string()))
    {
        tracing::warn!("failed to set session name: {e}");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn names(tools: &[Arc<dyn AgentTool>]) -> Vec<&str> {
        tools.iter().map(|t| t.name()).collect()
    }

    /// TS sdk.ts: active built-ins are exactly `defaultTools` when set; the
    /// optional powershell tool is off by default and registered (on any
    /// platform) only when explicitly listed.
    #[test]
    fn default_tools_gates_powershell() {
        let services = tack_tools::default_services(std::env::current_dir().unwrap());
        let base = tack_tools::create_coding_tools(&services);
        assert!(!names(&base).contains(&"powershell"));

        // Empty defaultTools: unchanged, no powershell.
        let out = apply_default_tools(tack_tools::create_coding_tools(&services), &[], &services);
        assert!(!names(&out).contains(&"powershell"));
        assert_eq!(names(&out), names(&base));

        // defaultTools replacing bash with powershell (TS windows.md recipe).
        let selection: Vec<String> = ["read", "powershell", "edit", "write"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = apply_default_tools(
            tack_tools::create_coding_tools(&services),
            &selection,
            &services,
        );
        let selected = names(&out);
        assert!(selected.contains(&"powershell"), "{selected:?}");
        assert!(!selected.contains(&"bash"), "{selected:?}");
        assert!(selected.contains(&"read") && selected.contains(&"edit"));
        // Unlisted built-ins are filtered out.
        assert!(!selected.contains(&"web_fetch"), "{selected:?}");

        // Both shells listed: both present, powershell not duplicated.
        let selection: Vec<String> = ["read", "bash", "powershell", "edit", "write"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = apply_default_tools(
            tack_tools::create_coding_tools(&services),
            &selection,
            &services,
        );
        let selected = names(&out);
        assert!(selected.contains(&"bash") && selected.contains(&"powershell"));
        assert_eq!(
            selected.iter().filter(|n| **n == "powershell").count(),
            1,
            "{selected:?}"
        );
    }

    #[test]
    fn session_arg_resolves_partial_id() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let session = SessionManager::create(cwd.path(), Some(dir.path().to_path_buf())).unwrap();
        let id = session.session_id().to_string();
        drop(session);
        let found = tack_session::resolve_session_arg(&id[..8], dir.path());
        assert!(found.is_some());
        assert!(tack_session::resolve_session_arg("nonexistent", dir.path()).is_none());
    }

    #[test]
    fn session_id_created_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let flags = CliFlags {
            session_id: Some("my-exact-id".to_string()),
            ..Default::default()
        };
        let session = open_session_for_flags(
            cwd.path(),
            Some(dir.path().to_path_buf()),
            false,
            &flags,
            tack_session::SessionBackend::Jsonl,
        )
        .unwrap();
        assert_eq!(session.session_id(), "my-exact-id");
    }

    /// Regression for pi #9440: an exact --session-id match is found by
    /// scanning session headers, so a session file renamed after creation
    /// (filename no longer encodes the id) still reopens instead of a new
    /// session being created.
    #[test]
    fn session_id_reopens_renamed_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let session = SessionManager::create(cwd.path(), Some(dir.path().to_path_buf())).unwrap();
        let id = session.session_id().to_string();
        let original = session.session_file().unwrap().to_path_buf();
        drop(session);
        let renamed = dir.path().join("imported-session.jsonl");
        std::fs::rename(&original, &renamed).unwrap();

        let flags = CliFlags {
            session_id: Some(id.clone()),
            ..Default::default()
        };
        let reopened = open_session_for_flags(
            cwd.path(),
            Some(dir.path().to_path_buf()),
            false,
            &flags,
            tack_session::SessionBackend::Jsonl,
        )
        .unwrap();
        assert_eq!(reopened.session_id(), id);
        assert_eq!(reopened.session_file().unwrap(), renamed.as_path());
    }

    #[test]
    fn no_session_is_in_memory() {
        let cwd = tempfile::tempdir().unwrap();
        let flags = CliFlags {
            no_session: true,
            ..Default::default()
        };
        let session = open_session_for_flags(
            cwd.path(),
            None,
            false,
            &flags,
            tack_session::SessionBackend::Jsonl,
        )
        .unwrap();
        assert!(session.session_file().is_none());
    }
}
