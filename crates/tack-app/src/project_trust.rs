//! Project trust (port of `core/trust-manager.ts` + `core/project-trust.ts`).
//!
//! Project-level `.pi` resources — settings, MCP servers, skills, prompts,
//! themes, system-prompt files, rules — are only loaded when the project is
//! trusted, so a malicious clone can't auto-start MCP servers or inject
//! settings. Decisions persist in `<agentDir>/trust.json` (`{path: bool}`);
//! the nearest ancestor entry wins. Session-only decisions live in a
//! process-global map. Non-interactive callers (print/ACP/RPC) treat "ask"
//! as untrusted, matching TS (`hasUI: false → false`).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

/// Resources in `<cwd>/.pi` that require trust (TS TRUST_REQUIRING_… plus
/// tack additions mcp.json and rules).
const TRUST_REQUIRING: &[&str] = &[
    "settings.json",
    "mcp.json",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
    "rules",
];

/// Session-only decisions, keyed by normalized cwd.
static SESSION_TRUST: Mutex<BTreeMap<String, bool>> = Mutex::new(BTreeMap::new());

/// Trust state for a project directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustState {
    Trusted,
    Untrusted,
    /// No stored/session decision and default is "ask" — UI should prompt.
    Ask,
}

fn normalize(cwd: &Path) -> String {
    dunce::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

fn read_store(agent_dir: &Path) -> BTreeMap<String, bool> {
    std::fs::read_to_string(agent_dir.join("trust.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<BTreeMap<String, bool>>(&c).ok())
        .unwrap_or_default()
}

fn write_store(agent_dir: &Path, store: &BTreeMap<String, bool>) -> std::io::Result<()> {
    let path = agent_dir.join("trust.json");
    crate::atomic_write::atomic_write(
        &path,
        &format!("{}\n", serde_json::to_string_pretty(store)?),
    )
}

/// Nearest-ancestor entry in a decision map.
fn nearest_decision(map: &BTreeMap<String, bool>, cwd_norm: &str) -> Option<bool> {
    let mut current = cwd_norm.to_string();
    loop {
        if let Some(&decision) = map.get(&current) {
            return Some(decision);
        }
        let pos = current.rfind('/')?;
        if pos == 0 {
            return None;
        }
        current.truncate(pos);
    }
}

/// TS hasTrustRequiringProjectResources: `.pi` entries in cwd, plus
/// `.agents/skills` walking ancestors ($HOME excluded from the walk —
/// `~/.agents/skills` is not a Tack skills source, and flagging it as a
/// project resource would gate every project under the home directory).
pub fn has_trust_requiring_resources(cwd: &Path) -> bool {
    let config = cwd.join(".pi");
    if TRUST_REQUIRING
        .iter()
        .any(|entry| config.join(entry).exists())
    {
        return true;
    }
    let home_skills = dirs::home_dir().map(|h| h.join(".agents").join("skills"));
    let mut current = Some(cwd);
    while let Some(dir) = current {
        let skills = dir.join(".agents").join("skills");
        if home_skills.as_ref() != Some(&skills) && skills.is_dir() {
            return true;
        }
        current = dir.parent();
    }
    false
}

fn default_project_trust(agent_dir: &Path) -> String {
    std::fs::read_to_string(agent_dir.join("settings.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|v| v.get("defaultProjectTrust")?.as_str().map(str::to_string))
        .unwrap_or_else(|| "ask".to_string())
}

/// Resolve the trust state (TS resolveProjectTrusted minus the UI prompt).
pub fn state(cwd: &Path, agent_dir: &Path) -> TrustState {
    if !has_trust_requiring_resources(cwd) {
        return TrustState::Trusted;
    }
    let norm = normalize(cwd);
    if let Some(decision) = nearest_decision(&SESSION_TRUST.lock().expect("session trust"), &norm) {
        return if decision {
            TrustState::Trusted
        } else {
            TrustState::Untrusted
        };
    }
    if let Some(decision) = nearest_decision(&read_store(agent_dir), &norm) {
        return if decision {
            TrustState::Trusted
        } else {
            TrustState::Untrusted
        };
    }
    match default_project_trust(agent_dir).as_str() {
        "always" => TrustState::Trusted,
        "never" => TrustState::Untrusted,
        _ => TrustState::Ask,
    }
}

/// Gate used by the resource loaders (non-interactive): Ask ⇒ untrusted.
pub fn is_trusted(cwd: &Path, agent_dir: &Path) -> bool {
    state(cwd, agent_dir) == TrustState::Trusted
}

/// Persist (or session-only set) a trust decision for `cwd`.
pub fn set_decision(agent_dir: &Path, cwd: &Path, trusted: bool, session_only: bool) {
    let norm = normalize(cwd);
    if session_only {
        SESSION_TRUST
            .lock()
            .expect("session trust")
            .insert(norm, trusted);
        return;
    }
    let mut store = read_store(agent_dir);
    store.insert(norm, trusted);
    if let Err(e) = write_store(agent_dir, &store) {
        tracing::warn!("failed to write trust store: {e}");
    }
}

/// "Trust parent folder": persist trust for the parent, clear the cwd entry.
pub fn set_parent_decision(agent_dir: &Path, cwd: &Path) {
    let norm = normalize(cwd);
    let Some(pos) = norm.rfind('/') else { return };
    if pos == 0 {
        return;
    }
    let parent = norm[..pos].to_string();
    let mut store = read_store(agent_dir);
    store.remove(&norm);
    store.insert(parent, true);
    if let Err(e) = write_store(agent_dir, &store) {
        tracing::warn!("failed to write trust store: {e}");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn nearest_ancestor_wins() {
        let mut map = BTreeMap::new();
        map.insert("D:/work".to_string(), true);
        map.insert("D:/work/repo/sub".to_string(), false);
        assert_eq!(nearest_decision(&map, "D:/work/repo"), Some(true));
        assert_eq!(nearest_decision(&map, "D:/work/repo/sub/deep"), Some(false));
        assert_eq!(nearest_decision(&map, "C:/elsewhere"), None);
    }

    #[test]
    fn detects_trust_requiring_resources() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_trust_requiring_resources(dir.path()));
        std::fs::create_dir_all(dir.path().join(".pi")).unwrap();
        assert!(!has_trust_requiring_resources(dir.path()));
        std::fs::write(dir.path().join(".pi").join("mcp.json"), "{}").unwrap();
        assert!(has_trust_requiring_resources(dir.path()));
    }

    #[test]
    fn store_roundtrip_and_gating() {
        let agent = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".pi")).unwrap();
        std::fs::write(project.path().join(".pi").join("settings.json"), "{}").unwrap();

        // No decision anywhere → Ask (non-interactive gate says untrusted).
        assert_eq!(state(project.path(), agent.path()), TrustState::Ask);
        assert!(!is_trusted(project.path(), agent.path()));

        set_decision(agent.path(), project.path(), true, false);
        assert_eq!(state(project.path(), agent.path()), TrustState::Trusted);
        assert!(is_trusted(project.path(), agent.path()));

        set_decision(agent.path(), project.path(), false, false);
        assert_eq!(state(project.path(), agent.path()), TrustState::Untrusted);
    }

    #[test]
    fn session_only_decision_not_persisted() {
        let agent = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".pi")).unwrap();
        std::fs::write(project.path().join(".pi").join("settings.json"), "{}").unwrap();

        set_decision(agent.path(), project.path(), true, true);
        assert!(is_trusted(project.path(), agent.path()));
        assert!(
            read_store(agent.path()).is_empty(),
            "session decision must not persist"
        );
        SESSION_TRUST.lock().unwrap().clear();
    }
}
