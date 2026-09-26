//! Custom sub-agent definitions (`.pi/agents/*.md`, Claude-Code-style):
//! markdown files with a frontmatter header describing a named agent the
//! `subagent` tool can be pointed at.
//!
//! ```markdown
//! ---
//! name: reviewer
//! description: Code reviewer — use after implementing a feature
//! tools: read, grep, find, ls, diagnostics
//! model: anthropic/k3
//! ---
//!
//! You are a meticulous code reviewer. … (becomes the sub-agent's system prompt)
//! ```
//!
//! Discovery: global `<agent dir>/agents/*.md` + project `.pi/agents/*.md`
//! (project dir only when the project is trusted; project wins on name
//! collision).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    /// Tool whitelist (empty = full coding tool set).
    pub tools: Vec<String>,
    /// Optional model override as provider/id.
    pub model: Option<String>,
    /// Markdown body → the sub-agent's system prompt.
    pub system_prompt: String,
    pub source: PathBuf,
}

/// Parse one agent definition file. Returns None when the file has no
/// usable content.
pub fn parse_agent_file(path: &Path, content: &str) -> Option<AgentDefinition> {
    let (front, body) = split_frontmatter(content);
    let system_prompt = body.trim().to_string();
    if system_prompt.is_empty() {
        return None;
    }
    let get = |key: &str| -> Option<String> {
        front.lines().find_map(|line| {
            let line = line.trim();
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
    };
    let name = get("name").or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()))?;
    Some(AgentDefinition {
        name,
        description: get("description").unwrap_or_default(),
        tools: get("tools")
            .map(|t| {
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        model: get("model"),
        system_prompt,
        source: path.to_path_buf(),
    })
}

/// Split `---\nfrontmatter\n---\nbody`; without markers the whole file is body.
fn split_frontmatter(content: &str) -> (String, String) {
    let Some(rest) = content.strip_prefix("---") else {
        return (String::new(), content.to_string());
    };
    // Find the closing marker on its own line.
    for (idx, line) in rest.lines().enumerate() {
        if line.trim() == "---" {
            let front: String = rest.lines().take(idx).collect::<Vec<_>>().join("\n");
            let body: String = rest.lines().skip(idx + 1).collect::<Vec<_>>().join("\n");
            return (front, body);
        }
    }
    (String::new(), content.to_string())
}

fn load_dir(dir: &Path, out: &mut Vec<AgentDefinition>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "md")
            && let Ok(content) = std::fs::read_to_string(&path)
            && let Some(def) = parse_agent_file(&path, &content)
        {
            out.push(def);
        }
    }
}

/// Load all agent definitions (global first, project overrides by name).
pub fn load_agents(cwd: &Path, agent_dir: &Path, project_trusted: bool) -> Vec<AgentDefinition> {
    let mut defs: Vec<AgentDefinition> = Vec::new();
    load_dir(&agent_dir.join("agents"), &mut defs);
    if project_trusted {
        let mut project = Vec::new();
        load_dir(&cwd.join(".pi").join("agents"), &mut project);
        for def in project {
            defs.retain(|d| d.name != def.name);
            defs.push(def);
        }
    }
    defs
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parses_frontmatter_fields() {
        let def = parse_agent_file(
            Path::new("reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews code\ntools: read, grep\nmodel: anthropic/k3\n---\n\nYou review code.\n",
        )
        .unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Reviews code");
        assert_eq!(def.tools, vec!["read", "grep"]);
        assert_eq!(def.model.as_deref(), Some("anthropic/k3"));
        assert_eq!(def.system_prompt, "You review code.");
    }

    #[test]
    fn file_stem_is_default_name() {
        let def = parse_agent_file(Path::new("explorer.md"), "Just a prompt body.").unwrap();
        assert_eq!(def.name, "explorer");
        assert!(def.tools.is_empty());
        assert_eq!(def.system_prompt, "Just a prompt body.");
    }

    #[test]
    fn empty_body_is_rejected() {
        assert!(parse_agent_file(Path::new("x.md"), "---\nname: x\n---\n").is_none());
    }

    #[test]
    fn project_overrides_global_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(agent_dir.join("agents")).unwrap();
        std::fs::create_dir_all(cwd.join(".pi/agents")).unwrap();
        std::fs::write(agent_dir.join("agents/reviewer.md"), "global reviewer").unwrap();
        std::fs::write(cwd.join(".pi/agents/reviewer.md"), "project reviewer").unwrap();
        std::fs::write(cwd.join(".pi/agents/local.md"), "local only").unwrap();

        let untrusted = load_agents(&cwd, &agent_dir, false);
        assert_eq!(untrusted.len(), 1);
        assert_eq!(untrusted[0].system_prompt, "global reviewer");

        let trusted = load_agents(&cwd, &agent_dir, true);
        assert_eq!(trusted.len(), 2);
        let reviewer = trusted.iter().find(|d| d.name == "reviewer").unwrap();
        assert_eq!(reviewer.system_prompt, "project reviewer");
    }
}
