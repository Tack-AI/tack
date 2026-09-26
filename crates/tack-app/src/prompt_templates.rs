//! Prompt templates: markdown files with frontmatter, discovered from
//! `~/.tack/agent/prompts/` and `<project>/.pi/prompts/`. Port of
//! `core/prompt-templates.ts` including the bash-style argument substitution.

use std::path::{Path, PathBuf};

use crate::skills;

#[derive(Clone, Debug)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    pub file_path: PathBuf,
}

/// Parse command arguments respecting quoted strings (bash-style).
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for c in args_string.chars() {
        match in_quote {
            Some(q) => {
                if c == q {
                    in_quote = None;
                } else {
                    current.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    in_quote = Some(c);
                } else if c.is_whitespace() {
                    if !current.is_empty() {
                        args.push(std::mem::take(&mut current));
                    }
                } else {
                    current.push(c);
                }
            }
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Substitute argument placeholders (port of substituteArgs):
/// `$1..$N`, `$@`/`$ARGUMENTS`, `${N:-default}`, `${@:-default}`,
/// `${@:N}` and `${@:N:L}` slicing. No recursive substitution.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");
    let chars: Vec<char> = content.chars().collect();
    let mut out = String::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        let rest: String = chars[i + 1..].iter().collect();

        // ${...} forms
        if rest.starts_with('{') {
            if let Some(close) = rest.find('}') {
                let inner: String = rest[1..close].to_string();
                let consumed = close + 1;
                if let Some(value) = substitute_braced(&inner, args, &all_args) {
                    out.push_str(&value);
                    i += consumed + 1;
                    continue;
                }
            }
            out.push(c);
            i += 1;
            continue;
        }

        // Simple forms: $@, $ARGUMENTS, $N
        if let Some(after) = rest.strip_prefix("ARGUMENTS")
            && !after
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            out.push_str(&all_args);
            i += 1 + "ARGUMENTS".len();
            continue;
        }
        if rest.starts_with('@') {
            out.push_str(&all_args);
            i += 2;
            continue;
        }
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let index: usize = digits.parse().unwrap_or(0);
            // $0 is not a positional argument (bash parity); it expands to
            // nothing rather than aliasing $1.
            let value = if index == 0 {
                ""
            } else {
                args.get(index - 1).map(String::as_str).unwrap_or("")
            };
            out.push_str(value);
            i += 1 + digits.len();
            continue;
        }
        out.push(c);
        i += 1;
    }

    out
}

/// Handle `${...}` contents; returns None if not a recognized form.
fn substitute_braced(inner: &str, args: &[String], all_args: &str) -> Option<String> {
    // ${N:-default}, ${@:-default}, ${ARGUMENTS:-default}
    if let Some((target, default)) = inner.split_once(":-") {
        let value = match target {
            "@" | "ARGUMENTS" => all_args.to_string(),
            n if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() => {
                let index: usize = n.parse().ok()?;
                if index == 0 {
                    String::new()
                } else {
                    args.get(index - 1).cloned().unwrap_or_default()
                }
            }
            _ => return None,
        };
        return Some(if value.is_empty() {
            default.to_string()
        } else {
            value
        });
    }
    // ${@:N} and ${@:N:L}
    if let Some(slice) = inner.strip_prefix("@:") {
        let mut parts = slice.splitn(3, ':');
        let start: usize = parts.next()?.parse().ok()?;
        let start = start.saturating_sub(1); // 1-indexed → 0-indexed; 0 treated as 1
        let length: Option<usize> = parts.next().and_then(|l| l.parse().ok());
        let _ = parts;
        let sliced: Vec<&String> = match length {
            Some(l) => args.iter().skip(start).take(l).collect(),
            None => args.iter().skip(start).collect(),
        };
        return Some(
            sliced
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    None
}

fn load_template_from_file(path: &Path) -> Option<PromptTemplate> {
    // Upstream b6419322e (#9830): a template that fails to load must not
    // vanish silently — log the path and reason instead of a bare `None`.
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!("failed to read prompt template {}: {e}", path.display());
            return None;
        }
    };
    let (frontmatter, body) = skills_frontmatter(&raw);
    let Some(name) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
        tracing::warn!(
            "failed to parse prompt template {}: no file name",
            path.display()
        );
        return None;
    };
    let description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let argument_hint = frontmatter
        .get("argument-hint")
        .or_else(|| frontmatter.get("argumentHint"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(PromptTemplate {
        name,
        description,
        argument_hint,
        content: body,
        file_path: path.to_path_buf(),
    })
}

// Reuse the skills frontmatter parser (flat YAML subset).
fn skills_frontmatter(content: &str) -> (serde_json::Map<String, serde_json::Value>, String) {
    // skills::parse_frontmatter is private; re-expose via a tiny shim.
    skills::parse_frontmatter_pub(content)
}

/// Load templates from the default directories: global agent prompts first,
/// then project prompts (project wins on name conflicts, matching pi's
/// later-source-wins dedupe).
pub fn load_prompt_templates(cwd: &Path, agent_dir: &Path) -> Vec<PromptTemplate> {
    let mut by_name: Vec<PromptTemplate> = Vec::new();
    let mut dirs = vec![agent_dir.join("prompts")];
    // Extra prompt dirs from settings `prompts` (TS getPromptTemplatePaths).
    for extra in crate::settings::Settings::extra_paths(agent_dir, "prompts") {
        dirs.push(PathBuf::from(extra));
    }
    // Project prompts are trust-gated.
    if crate::project_trust::is_trusted(cwd, agent_dir) {
        dirs.push(cwd.join(".pi").join("prompts"));
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            if let Some(template) = load_template_from_file(&path) {
                if let Some(existing) = by_name.iter_mut().find(|t| t.name == template.name) {
                    *existing = template;
                } else {
                    by_name.push(template);
                }
            }
        }
    }
    by_name
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn substitutes_positional_and_all_args() {
        let a = args(&["one", "two", "three"]);
        assert_eq!(substitute_args("$1 $2", &a), "one two");
        assert_eq!(substitute_args("$@!", &a), "one two three!");
        assert_eq!(substitute_args("$ARGUMENTS", &a), "one two three");
        assert_eq!(substitute_args("${2:-fallback}", &a), "two");
        assert_eq!(substitute_args("${9:-fallback}", &a), "fallback");
        assert_eq!(substitute_args("${@:2}", &a), "two three");
        assert_eq!(substitute_args("${@:1:2}", &a), "one two");
    }

    #[test]
    fn dollar_zero_is_not_the_first_argument() {
        // Regression: $0 / ${0:-d} aliased args[0]; $0 is not a positional
        // argument and must expand to nothing (→ default).
        let a = args(&["one", "two"]);
        assert_eq!(substitute_args("[$0]", &a), "[]");
        assert_eq!(substitute_args("${0:-fallback}", &a), "fallback");
    }

    #[test]
    fn parse_args_quotes() {
        assert_eq!(parse_command_args("a b  c"), vec!["a", "b", "c"]);
        assert_eq!(
            parse_command_args("a \"b c\" 'd e'"),
            vec!["a", "b c", "d e"]
        );
        assert_eq!(parse_command_args(""), Vec::<String>::new());
    }

    /// Regression for pi #9830: an unreadable template must be skipped
    /// (with a warning) while valid siblings still load — not silently
    /// swallow the whole directory or panic.
    #[test]
    fn unreadable_template_is_skipped_but_siblings_load() {
        let cwd = tempfile::tempdir().unwrap();
        let agent = tempfile::tempdir().unwrap();
        let prompts = agent.path().join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        // Invalid UTF-8: read_to_string fails.
        std::fs::write(prompts.join("broken.md"), [0xff, 0xfe, 0xfd]).unwrap();
        std::fs::write(prompts.join("valid.md"), "Valid prompt content.").unwrap();

        let templates = load_prompt_templates(cwd.path(), agent.path());
        let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["valid"]);
    }
}
