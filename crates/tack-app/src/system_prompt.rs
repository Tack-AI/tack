//! System prompt assembly. Port of `buildSystemPrompt` from
//! `packages/coding-agent/src/core/system-prompt.ts`. The pi-documentation
//! section is omitted (tack has no bundled docs); everything else follows
//! the TS structure so output is comparable for a fixed fixture.

use std::path::Path;

use crate::skills::{Skill, format_skills_for_prompt};

#[derive(Debug)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

/// Per-file context budget (settings `rulesMaxChars`, 0 = unlimited):
/// truncate oversized AGENTS.md/CLAUDE.md content with a pointer to the
/// on-disk file. The file itself is the offload target — the model can
/// read it in slices when it actually needs the full text.
pub fn apply_rules_budget(files: &mut [ContextFile], max_chars: usize) {
    if max_chars == 0 {
        return;
    }
    for file in files.iter_mut() {
        let total = file.content.chars().count();
        if total <= max_chars {
            continue;
        }
        let head: String = file.content.chars().take(max_chars).collect();
        file.content = format!(
            "{head}\n\n[truncated by tack context budget: {total} chars total — full file: {} \
             — read it with the read tool if you need more]",
            file.path
        );
    }
}

#[cfg(test)]
mod budget_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn oversized_rules_file_is_truncated_with_pointer() {
        let mut files = vec![ContextFile {
            path: "/repo/AGENTS.md".into(),
            content: "x".repeat(5_000),
        }];
        apply_rules_budget(&mut files, 1_000);
        assert!(
            files[0].content.chars().count() < 1_200,
            "{}",
            files[0].content.len()
        );
        assert!(
            files[0]
                .content
                .contains("truncated by tack context budget")
        );
        assert!(files[0].content.contains("/repo/AGENTS.md"));
    }

    #[test]
    fn small_files_and_zero_budget_untouched() {
        let mut files = vec![ContextFile {
            path: "/repo/AGENTS.md".into(),
            content: "short".into(),
        }];
        apply_rules_budget(&mut files, 1_000);
        assert_eq!(files[0].content, "short");
        let mut big = vec![ContextFile {
            path: "/repo/BIG.md".into(),
            content: "x".repeat(5_000),
        }];
        apply_rules_budget(&mut big, 0);
        assert_eq!(big[0].content.len(), 5_000);
    }
}

#[derive(Debug)]
pub struct SystemPromptOptions<'a> {
    pub custom_prompt: Option<&'a str>,
    /// Default: read/bash/edit/write.
    pub selected_tools: &'a [String],
    /// One-line snippets keyed by tool name.
    pub tool_snippets: &'a [(&'a str, &'a str)],
    pub prompt_guidelines: &'a [String],
    pub append_system_prompt: Option<&'a str>,
    pub cwd: &'a Path,
    pub context_files: &'a [ContextFile],
    pub skills: &'a [Skill],
}

pub fn build_system_prompt(options: &SystemPromptOptions) -> String {
    let prompt_cwd = options.cwd.to_string_lossy().replace('\\', "/");
    let append_section = options
        .append_system_prompt
        .map(|s| format!("\n\n{s}"))
        .unwrap_or_default();

    let context_files = options.context_files;
    let skills = options.skills;

    if let Some(custom) = options.custom_prompt {
        let mut prompt = custom.to_string() + &append_section;

        if !context_files.is_empty() {
            prompt.push_str("\n\n<project_context>\n\n");
            prompt.push_str("Project-specific instructions and guidelines:\n\n");
            for file in context_files {
                prompt.push_str(&format!(
                    "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                    file.path, file.content
                ));
            }
            prompt.push_str("</project_context>\n");
        }

        let has_read =
            options.selected_tools.is_empty() || options.selected_tools.iter().any(|t| t == "read");
        if has_read && !skills.is_empty() {
            prompt.push_str(&format_skills_for_prompt(skills));
        }

        prompt.push_str(&format!("\nCurrent working directory: {prompt_cwd}\n"));
        return prompt;
    }

    // Tools list: only tools with a snippet are visible.
    let default_tools = ["read", "bash", "edit", "write"];
    let tools: Vec<String> = if options.selected_tools.is_empty() {
        default_tools.iter().map(|s| s.to_string()).collect()
    } else {
        options.selected_tools.to_vec()
    };
    let visible: Vec<&String> = tools
        .iter()
        .filter(|name| options.tool_snippets.iter().any(|(n, _)| n == name))
        .collect();
    let tools_list = if visible.is_empty() {
        "(none)".to_string()
    } else {
        visible
            .iter()
            .map(|name| {
                let snippet = options
                    .tool_snippets
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, s)| *s)
                    .unwrap_or("");
                format!("- {name}: {snippet}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Guidelines.
    let mut guidelines: Vec<String> = Vec::new();
    let add_guideline = |g: &str, guidelines: &mut Vec<String>| {
        if !guidelines.iter().any(|x| x == g) {
            guidelines.push(g.to_string());
        }
    };

    let has_bash = tools.iter().any(|t| t == "bash");
    let has_powershell = tools.iter().any(|t| t == "powershell");
    let has_grep = tools.iter().any(|t| t == "grep");
    let has_find = tools.iter().any(|t| t == "find");
    let has_ls = tools.iter().any(|t| t == "ls");
    let has_read = tools.iter().any(|t| t == "read");

    if (has_bash || has_powershell) && !has_grep && !has_find && !has_ls {
        if has_bash && has_powershell {
            add_guideline(
                "Use bash or PowerShell for file operations like listing, searching, and finding files",
                &mut guidelines,
            );
        } else if has_powershell {
            add_guideline(
                "Use PowerShell for file operations like listing, searching, and finding files",
                &mut guidelines,
            );
        } else {
            add_guideline(
                "Use bash for file operations like ls, rg, find",
                &mut guidelines,
            );
        }
    }
    if tools.iter().any(|t| t == "git") {
        add_guideline(
            "Use the git tool instead of bash for git operations — it is validated and permission-aware",
            &mut guidelines,
        );
    }
    for g in options.prompt_guidelines {
        let normalized = g.trim();
        if !normalized.is_empty() {
            add_guideline(normalized, &mut guidelines);
        }
    }
    add_guideline("Be concise in your responses", &mut guidelines);
    add_guideline(
        "Show file paths clearly when working with files",
        &mut guidelines,
    );

    let guidelines_text = guidelines
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut prompt = format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{tools_list}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.\n\nGuidelines:\n{guidelines_text}"
    );

    prompt.push_str(&append_section);

    if !context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for file in context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                file.path, file.content
            ));
        }
        prompt.push_str("</project_context>\n");
    }

    if has_read && !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(skills));
    }

    prompt.push_str(&format!("\nCurrent working directory: {prompt_cwd}"));
    prompt
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn prompt_for(tools: &[&str]) -> String {
        let selected: Vec<String> = tools.iter().map(|s| s.to_string()).collect();
        let snippets: Vec<(&str, &str)> = tools.iter().map(|t| (*t, "snippet")).collect();
        build_system_prompt(&SystemPromptOptions {
            custom_prompt: None,
            selected_tools: &selected,
            tool_snippets: &snippets,
            prompt_guidelines: &[],
            append_system_prompt: None,
            cwd: Path::new("/tmp"),
            context_files: &[],
            skills: &[],
        })
    }

    /// File-exploration guideline follows the selected shell tool(s)
    /// (TS system-prompt.ts hasBash/hasPowerShell).
    #[test]
    fn powershell_guideline_variants() {
        let p = prompt_for(&["read", "powershell", "edit", "write"]);
        assert!(
            p.contains(
                "Use PowerShell for file operations like listing, searching, and finding files"
            ),
            "{p}"
        );
        let p = prompt_for(&["read", "bash", "powershell", "edit", "write"]);
        assert!(
            p.contains("Use bash or PowerShell for file operations like listing, searching, and finding files"),
            "{p}"
        );
        let p = prompt_for(&["read", "bash", "edit", "write"]);
        assert!(
            p.contains("Use bash for file operations like ls, rg, find"),
            "{p}"
        );
        // grep/find/ls present ⇒ no shell file-ops guideline at all.
        let p = prompt_for(&["read", "powershell", "grep", "find", "ls"]);
        assert!(!p.contains("file operations like"), "{p}");
    }
}
