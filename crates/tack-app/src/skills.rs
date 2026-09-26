//! Skill loading. Port of `packages/coding-agent/src/core/skills.ts`:
//! SKILL.md discovery rules, frontmatter contract, name validation, and the
//! `<available_skills>` prompt section. Ignore-file handling is simplified to
//! .gitignore via the `ignore` crate.

use std::path::{Path, PathBuf};

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub disable_model_invocation: bool,
    /// Skill body (markdown after frontmatter), used by /skill expansion.
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct SkillDiagnostic {
    pub message: String,
    pub path: PathBuf,
}

/// `name` per the Agent Skills spec: lowercase a-z, 0-9, hyphens; no
/// leading/trailing/double hyphens; max 64 chars.
fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name.chars().count() > MAX_NAME_LENGTH {
        errors.push(format!("name exceeds {MAX_NAME_LENGTH} characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".into());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".into());
    }
    errors
}

/// Parse `---\nyaml\n---\nbody` frontmatter. Flat `key: value` pairs plus
/// `|`/`>` block scalars; nested maps/sequences are not supported (the
/// skills contract only uses flat scalars).
fn parse_frontmatter(content: &str) -> (serde_json::Map<String, serde_json::Value>, String) {
    parse_frontmatter_impl(content)
}

fn parse_frontmatter_impl(content: &str) -> (serde_json::Map<String, serde_json::Value>, String) {
    let normalized = content
        .trim_start_matches('\u{FEFF}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if !normalized.starts_with("---") {
        return (serde_json::Map::new(), normalized);
    }
    let Some(end) = normalized[3..].find("\n---").map(|i| i + 3) else {
        return (serde_json::Map::new(), normalized);
    };
    // `end` points at the '\n' before the closing ---; an empty frontmatter
    // block ("---\n---") gives end < 4 — the yaml section is empty.
    let yaml = normalized.get(4..end).unwrap_or("");
    let body = normalized[end + 4..].trim().to_string();

    let mut map = serde_json::Map::new();
    // Flat `key: value` pairs plus `|`/`>` block scalars; enough for the
    // skills contract.
    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || key.starts_with('-')
            || key.chars().next().is_some_and(|c| c.is_whitespace())
        {
            continue;
        }
        let key_indent = line.len() - line.trim_start().len();
        let value = value.trim();
        if let Some(block) = parse_block_scalar(value, &lines, &mut i, key_indent) {
            map.insert(key.to_string(), block);
            continue;
        }
        let parsed: serde_json::Value = match value {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            _ => {
                // Strip surrounding quotes.
                let unquoted = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                    .unwrap_or(value);
                serde_json::Value::String(unquoted.to_string())
            }
        };
        map.insert(key.to_string(), parsed);
    }
    (map, body)
}

/// Parse a YAML block scalar after its header (`|` literal / `>` folded,
/// with optional `-`/`+` chomping). Consumes the indented block lines from
/// `lines`, advancing `i` past them. Returns None when `header` is not a
/// block scalar header.
fn parse_block_scalar(
    header: &str,
    lines: &[&str],
    i: &mut usize,
    key_indent: usize,
) -> Option<serde_json::Value> {
    let (style, chomp) = match header {
        "|" => ('|', ' '),
        "|-" => ('|', '-'),
        "|+" => ('|', '+'),
        ">" => ('>', ' '),
        ">-" => ('>', '-'),
        ">+" => ('>', '+'),
        _ => return None,
    };
    // Block lines: blank lines tentatively; stop at the first non-empty
    // line indented at or below the key.
    let mut raw: Vec<Option<&str>> = Vec::new();
    while *i < lines.len() {
        let l = lines[*i];
        if l.trim().is_empty() {
            raw.push(None);
            *i += 1;
            continue;
        }
        let indent = l.len() - l.trim_start().len();
        if indent <= key_indent {
            break;
        }
        raw.push(Some(l));
        *i += 1;
    }
    // Block indent comes from the first content line; a block with no
    // content is the empty string.
    let block_indent = raw
        .iter()
        .find_map(|l| l.map(|l| l.len() - l.trim_start().len()));
    let Some(block_indent) = block_indent else {
        return Some(serde_json::Value::String(String::new()));
    };
    let stripped: Vec<Option<&str>> = raw
        .iter()
        .map(|l| {
            l.map(|l| {
                let indent = l.len() - l.trim_start().len();
                &l[block_indent.min(indent)..]
            })
        })
        .collect();

    let mut out = String::new();
    match style {
        // Literal: every line keeps its line break.
        '|' => {
            for l in &stripped {
                out.push_str(l.unwrap_or(""));
                out.push('\n');
            }
        }
        // Folded: consecutive content lines join with a space; a blank
        // line folds to a line break.
        _ => {
            let mut join_with_space = false;
            for l in &stripped {
                match l {
                    Some(text) => {
                        if join_with_space {
                            out.push(' ');
                        }
                        out.push_str(text);
                        join_with_space = true;
                    }
                    None => {
                        out.push('\n');
                        join_with_space = false;
                    }
                }
            }
            out.push('\n');
        }
    }

    // Chomping: `-` strips the final break, default clips to one, `+`
    // keeps everything (including trailing blank lines).
    let chomped = match chomp {
        '-' => out.trim_end_matches('\n').to_string(),
        '+' => out,
        _ => format!("{}\n", out.trim_end_matches('\n')),
    };
    Some(serde_json::Value::String(chomped))
}

/// Shared with prompt_templates.
pub(crate) fn parse_frontmatter_pub(
    content: &str,
) -> (serde_json::Map<String, serde_json::Value>, String) {
    parse_frontmatter_impl(content)
}

fn load_skill_from_file(file_path: &Path) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let mut diagnostics = Vec::new();
    let is_declared = file_path.file_name().is_some_and(|n| n == "SKILL.md");

    let Ok(raw) = std::fs::read_to_string(file_path) else {
        diagnostics.push(SkillDiagnostic {
            message: "failed to read skill file".into(),
            path: file_path.to_path_buf(),
        });
        return (None, diagnostics);
    };

    let (frontmatter, body) = parse_frontmatter(&raw);
    let description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let has_description = !description.trim().is_empty();

    // Non-SKILL.md files need a description to count as skills.
    if !is_declared && !has_description {
        return (None, diagnostics);
    }

    for error in validate_description(description) {
        diagnostics.push(SkillDiagnostic {
            message: error,
            path: file_path.to_path_buf(),
        });
    }

    let base_dir = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = frontmatter
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            base_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_default();
    for error in validate_name(&name) {
        diagnostics.push(SkillDiagnostic {
            message: error,
            path: file_path.to_path_buf(),
        });
    }

    if !has_description {
        return (None, diagnostics);
    }

    let disable_model_invocation = frontmatter
        .get("disable-model-invocation")
        .and_then(|v| v.as_bool())
        == Some(true);

    (
        Some(Skill {
            name,
            description: description.to_string(),
            file_path: file_path.to_path_buf(),
            base_dir,
            disable_model_invocation,
            body,
        }),
        diagnostics,
    )
}

fn validate_description(description: &str) -> Vec<String> {
    if description.trim().is_empty() {
        return vec!["description is required".into()];
    }
    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        return vec![format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters"
        )];
    }
    Vec::new()
}

/// Load skills from a directory (pi's discovery rules):
/// - a directory containing SKILL.md is a skill root; don't recurse further
/// - otherwise load direct .md children at the root, and recurse into subdirs
pub fn load_skills_from_dir(dir: &Path) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    load_skills_from_dir_internal(dir, true)
}

fn load_skills_from_dir_internal(
    dir: &Path,
    include_root_files: bool,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();

    let Ok(entries) = std::fs::read_dir(dir) else {
        return (skills, diagnostics);
    };
    let entries: Vec<_> = entries.flatten().collect();

    // SKILL.md at this level → skill root.
    for entry in &entries {
        if entry.file_name() == "SKILL.md" && entry.file_type().is_ok_and(|t| t.is_file()) {
            let (skill, diag) = load_skill_from_file(&entry.path());
            if let Some(skill) = skill {
                skills.push(skill);
            }
            diagnostics.extend(diag);
            return (skills, diagnostics);
        }
    }

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type().ok();
        let is_dir = file_type.is_some_and(|t| t.is_dir());
        let is_file = file_type.is_some_and(|t| t.is_file());

        if is_dir {
            let (sub_skills, sub_diag) = load_skills_from_dir_internal(&path, false);
            skills.extend(sub_skills);
            diagnostics.extend(sub_diag);
        } else if is_file && include_root_files && name.ends_with(".md") {
            let (skill, diag) = load_skill_from_file(&path);
            if let Some(skill) = skill {
                skills.push(skill);
            }
            diagnostics.extend(diag);
        }
    }

    (skills, diagnostics)
}

/// Load skills from the default locations (TS pi precedence order):
/// 1. project `<cwd>/.pi/skills` and `.agents/skills` (cwd + ancestors up to
///    the git root),
/// 2. user `<agentDir>/skills`.
///
/// (`~/.agents/skills`, the cross-tool Agent Skills home location, is
/// deliberately not loaded — dropped in the pi→Tack cut.)
///
/// Name collisions: first loaded wins (project before user), matching pi's
/// resourcePrecedenceRank. Identical real files are deduped silently.
pub fn load_skills(cwd: &Path, agent_dir: &Path) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills: Vec<Skill> = Vec::new();
    let mut diagnostics = Vec::new();

    // Discovery order = precedence order (first wins on name collision).
    // Project-level dirs are trust-gated (a malicious clone must not inject
    // skills); user dirs always load.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let home = dirs::home_dir();
    if crate::project_trust::is_trusted(cwd, agent_dir) {
        dirs.push(cwd.join(".pi").join("skills"));
        // Project .agents/skills: cwd and ancestors up to (and including) the
        // git root. Home is excluded (it's the user dir below).
        let mut current = Some(cwd);
        while let Some(dir) = current {
            let candidate = dir.join(".agents").join("skills");
            if home.as_deref() != Some(dir) {
                dirs.push(candidate);
            }
            if dir.join(".git").exists() {
                break;
            }
            current = dir.parent();
        }
    }
    dirs.push(agent_dir.join("skills"));
    // NOTE: `~/.agents/skills` is deliberately NOT a Tack skills source (the
    // cross-tool Agent Skills home location was dropped in the pi→Tack cut).
    // The project-level ancestor walk above still skips $HOME so a stray
    // `~/.agents/skills` is never picked up as a project dir either.
    // Extra skill dirs from settings `skills` (TS getSkillPaths).
    for extra in crate::settings::Settings::extra_paths(agent_dir, "skills") {
        dirs.push(PathBuf::from(extra));
    }

    let mut seen_files: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let (found, diag) = load_skills_from_dir(&dir);
        for skill in found {
            // Silent dedup of the same real file.
            let canonical =
                dunce::canonicalize(&skill.file_path).unwrap_or_else(|_| skill.file_path.clone());
            if seen_files.contains(&canonical) {
                continue;
            }
            seen_files.push(canonical);
            if skills.iter().any(|s: &Skill| s.name == skill.name) {
                diagnostics.push(SkillDiagnostic {
                    message: format!("name \"{}\" collision", skill.name),
                    path: skill.file_path.clone(),
                });
            } else {
                skills.push(skill);
            }
        }
        diagnostics.extend(diag);
    }

    (skills, diagnostics)
}

/// Expand a `/skill:<name> [args]` invocation into the user message sent to
/// the model (TS `_expandSkillCommand`). Returns None when the text is not a
/// skill command or the skill is unknown (caller sends the text unchanged).
pub fn expand_skill_command(text: &str, skills: &[Skill]) -> Option<String> {
    let rest = text.strip_prefix("/skill:")?;
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(pos) => (&rest[..pos], rest[pos..].trim()),
        None => (rest, ""),
    };
    let skill = skills.iter().find(|s| s.name == name)?;
    let mut expanded = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path.display(),
        skill.base_dir.display(),
        skill.body.trim(),
    );
    if !args.is_empty() {
        expanded.push_str(&format!("\n\n{args}"));
    }
    Some(expanded)
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Format skills for the system prompt (`formatSkillsForPrompt` in pi).
/// Skills with disable_model_invocation are excluded.
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.to_string_lossy())
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn empty_frontmatter_does_not_panic() {
        // Regression: "---\n---" (empty frontmatter) sliced [4..3] and
        // panicked.
        let (fm, body) = parse_frontmatter("---\n---\nbody text");
        assert!(fm.is_empty());
        assert_eq!(body, "body text");
        let (fm, body) = parse_frontmatter("---\n---");
        assert!(fm.is_empty());
        assert_eq!(body, "");
    }

    #[test]
    fn frontmatter_literal_block_scalar() {
        let (fm, body) = parse_frontmatter(
            "---\nname: t\ndescription: |\n  line one\n  line two\n---\nbody text",
        );
        assert_eq!(
            fm.get("description").and_then(|v| v.as_str()),
            Some("line one\nline two\n")
        );
        assert_eq!(body, "body text");
        // A following top-level key is not swallowed by the block.
        let (fm, _) = parse_frontmatter("---\ndescription: |\n  multi\nname: foo\n---\n");
        assert_eq!(
            fm.get("description").and_then(|v| v.as_str()),
            Some("multi\n")
        );
        assert_eq!(fm.get("name").and_then(|v| v.as_str()), Some("foo"));
    }

    #[test]
    fn frontmatter_folded_block_scalar_and_chomping() {
        let (fm, _) = parse_frontmatter("---\ndescription: >\n  folded\n  text\n---\n");
        assert_eq!(
            fm.get("description").and_then(|v| v.as_str()),
            Some("folded text\n")
        );
        // A blank line folds to a paragraph break.
        let (fm, _) = parse_frontmatter("---\ndescription: >\n  para one\n\n  para two\n---\n");
        assert_eq!(
            fm.get("description").and_then(|v| v.as_str()),
            Some("para one\npara two\n")
        );
        // `-` strips the final line break, `+` keeps trailing blanks.
        let (fm, _) = parse_frontmatter("---\ndescription: |-\n  no trailing\n---\n");
        assert_eq!(
            fm.get("description").and_then(|v| v.as_str()),
            Some("no trailing")
        );
        // `+` keeps the blank line between content lines; a blank line
        // immediately before the closing `---` is consumed by the
        // frontmatter delimiter itself (same as TS's extractFrontmatter).
        let (fm, _) = parse_frontmatter("---\ndescription: |+\n  keep\n\n  end\n---\n");
        assert_eq!(
            fm.get("description").and_then(|v| v.as_str()),
            Some("keep\n\nend\n")
        );
    }

    #[test]
    fn project_wins_name_collision_over_user() {
        let cwd = tempfile::tempdir().unwrap();
        let agent = tempfile::tempdir().unwrap();
        crate::project_trust::set_decision(agent.path(), cwd.path(), true, false);
        write_skill(
            &cwd.path().join(".pi/skills/dup"),
            "dup",
            "project version",
            "project body",
        );
        write_skill(
            &agent.path().join("skills/dup"),
            "dup",
            "user version",
            "user body",
        );
        let (skills, _) = load_skills(cwd.path(), agent.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "project version");
    }

    #[test]
    fn agents_skills_dirs_discovered() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let agent = tempfile::tempdir().unwrap();
        crate::project_trust::set_decision(agent.path(), &nested, true, false);
        write_skill(
            &repo.path().join(".agents/skills/rs"),
            "rs",
            "repo skill",
            "body",
        );
        let (skills, _) = load_skills(&nested, agent.path());
        assert!(skills.iter().any(|s| s.name == "rs"), "{skills:?}");
    }

    #[test]
    fn expand_skill_command_wraps_body_and_args() {
        let dir = tempfile::tempdir().unwrap();
        crate::project_trust::set_decision(dir.path(), dir.path(), true, false);
        write_skill(
            &dir.path().join(".pi/skills/git-commit"),
            "git-commit",
            "Commit helper",
            "Write a good commit.",
        );
        let (skills, _) = load_skills(dir.path(), dir.path());
        let expanded = expand_skill_command("/skill:git-commit fix the tests", &skills).unwrap();
        assert!(
            expanded.starts_with("<skill name=\"git-commit\""),
            "{expanded}"
        );
        assert!(
            expanded.contains("References are relative to"),
            "{expanded}"
        );
        assert!(expanded.contains("Write a good commit."), "{expanded}");
        assert!(expanded.ends_with("fix the tests"), "{expanded}");
        // Unknown skill and non-skill text pass through as None.
        assert!(expand_skill_command("/skill:nope", &skills).is_none());
        assert!(expand_skill_command("hello", &skills).is_none());
    }

    #[test]
    fn disable_model_invocation_hides_from_prompt_but_not_expansion() {
        let dir = tempfile::tempdir().unwrap();
        crate::project_trust::set_decision(dir.path(), dir.path(), true, false);
        std::fs::create_dir_all(dir.path().join(".pi/skills/hidden")).unwrap();
        std::fs::write(
            dir.path().join(".pi/skills/hidden/SKILL.md"),
            "---\nname: hidden\ndescription: hidden skill\ndisable-model-invocation: true\n---\nbody",
        )
        .unwrap();
        let (skills, _) = load_skills(dir.path(), dir.path());
        assert!(format_skills_for_prompt(&skills).is_empty());
        assert!(expand_skill_command("/skill:hidden", &skills).is_some());
    }
}
