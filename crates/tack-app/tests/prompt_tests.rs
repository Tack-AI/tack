//! M5 tests: skills loading, settings merge, system prompt assembly.
#![allow(clippy::unwrap_used)]

use tack_app::prompt_templates::load_prompt_templates;
use tack_app::settings::Settings;
use tack_app::skills::{format_skills_for_prompt, load_skills_from_dir};
use tack_app::system_prompt::{ContextFile, SystemPromptOptions, build_system_prompt};

fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name} body\n"),
    )
    .unwrap();
}

#[test]
fn loads_skill_md_with_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(
        &dir.path().join("my-skill"),
        "my-skill",
        "Does useful things",
    );

    let (skills, diagnostics) = load_skills_from_dir(dir.path());
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "my-skill");
    assert_eq!(skills[0].description, "Does useful things");
    assert_eq!(skills[0].body, "# my-skill body");
    assert!(diagnostics.is_empty());
}

#[test]
fn root_md_files_need_description() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("plain.md"), "no frontmatter here").unwrap();
    std::fs::write(
        dir.path().join("documented.md"),
        "---\ndescription: Has docs\n---\nbody",
    )
    .unwrap();

    let (skills, _) = load_skills_from_dir(dir.path());
    assert_eq!(skills.len(), 1);
    // Name falls back to the file's parent directory.
    assert_eq!(skills[0].description, "Has docs");
}

#[test]
fn invalid_skill_names_warn_but_load() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(
        &dir.path().join("bad"),
        "Bad--Name",
        "Invalid name still loads",
    );

    let (skills, diagnostics) = load_skills_from_dir(dir.path());
    assert_eq!(skills.len(), 1);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("consecutive hyphens"))
    );
}

#[test]
fn disable_model_invocation_hidden_from_prompt() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(&dir.path().join("visible"), "visible-skill", "Shown");
    std::fs::create_dir_all(dir.path().join("hidden")).unwrap();
    std::fs::write(
        dir.path().join("hidden").join("SKILL.md"),
        "---\nname: hidden-skill\ndescription: Not shown\ndisable-model-invocation: true\n---\nbody",
    )
    .unwrap();

    let (skills, _) = load_skills_from_dir(dir.path());
    assert_eq!(skills.len(), 2);
    let prompt = format_skills_for_prompt(&skills);
    assert!(prompt.contains("visible-skill"));
    assert!(!prompt.contains("hidden-skill"));
}

#[test]
fn settings_deep_merge_project_wins() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(project.join(".pi")).unwrap();
    tack_app::project_trust::set_decision(&agent_dir, &project, true, false);

    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"defaultModel": "global-model", "compaction": {"reserveTokens": 8192}}"#,
    )
    .unwrap();
    std::fs::write(
        project.join(".pi").join("settings.json"),
        r#"{"defaultModel": "project-model"}"#,
    )
    .unwrap();

    let settings = Settings::load(&project, &agent_dir);
    assert_eq!(settings.default_model.as_deref(), Some("project-model"));
    // Global compaction override survives (project didn't touch it).
    assert_eq!(settings.compaction.reserve_tokens, 8192);
    // Defaults elsewhere.
    assert_eq!(settings.compaction.keep_recent_tokens, 20000);
}

#[test]
fn system_prompt_structure_matches_pi() {
    let cwd = std::path::Path::new("C:\\work\\proj");
    let tools: Vec<String> = ["read", "bash", "edit", "write"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let guidelines: Vec<String> = tack_app::print_mode::TOOL_GUIDELINES
        .iter()
        .map(|s| s.to_string())
        .collect();
    let context_files = vec![ContextFile {
        path: "C:\\work\\proj\\AGENTS.md".to_string(),
        content: "Always run tests.".to_string(),
    }];

    let prompt = build_system_prompt(&SystemPromptOptions {
        custom_prompt: None,
        selected_tools: &tools,
        tool_snippets: tack_app::print_mode::TOOL_SNIPPETS,
        prompt_guidelines: &guidelines,
        append_system_prompt: Some("Extra instructions."),
        cwd,
        context_files: &context_files,
        skills: &[],
    });

    // Structure: intro → tools → guidelines → append → project_context → cwd.
    let intro = prompt.find("You are an expert coding assistant").unwrap();
    let tools_idx = prompt
        .find("Available tools:\n- read: Read file contents")
        .unwrap();
    let guidelines_idx = prompt
        .find("Guidelines:\n- Use bash for file operations")
        .unwrap();
    let append_idx = prompt.find("Extra instructions.").unwrap();
    let context_idx = prompt.find("<project_context>").unwrap();
    let instructions_idx = prompt
        .find("<project_instructions path=\"C:\\work\\proj\\AGENTS.md\">\nAlways run tests.\n</project_instructions>")
        .unwrap();
    let cwd_idx = prompt
        .find("Current working directory: C:/work/proj")
        .unwrap();

    assert!(intro < tools_idx);
    assert!(tools_idx < guidelines_idx);
    assert!(guidelines_idx < append_idx);
    assert!(append_idx < context_idx);
    assert!(context_idx < instructions_idx);
    assert!(instructions_idx < cwd_idx);
    assert!(prompt.contains("Be concise in your responses"));
}

#[test]
fn custom_prompt_still_appends_context_and_cwd() {
    let cwd = std::path::Path::new("/tmp/x");
    let prompt = build_system_prompt(&SystemPromptOptions {
        custom_prompt: Some("You are a pirate."),
        selected_tools: &[],
        tool_snippets: &[],
        prompt_guidelines: &[],
        append_system_prompt: None,
        cwd,
        context_files: &[],
        skills: &[],
    });
    assert!(prompt.starts_with("You are a pirate."));
    assert!(prompt.contains("Current working directory: /tmp/x"));
}

#[test]
fn template_arg_substitution() {
    use tack_app::prompt_templates::*;
    let args = vec!["a".to_string(), "b c".to_string(), "d".to_string()];
    assert_eq!(substitute_args("run $1 then $2", &args), "run a then b c");
    assert_eq!(substitute_args("all: $@", &args), "all: a b c d");
    assert_eq!(substitute_args("all: $ARGUMENTS", &args), "all: a b c d");
    assert_eq!(substitute_args("x${3:-fallback}", &args), "xd");
    assert_eq!(substitute_args("x${4:-fallback}", &args), "xfallback");
    assert_eq!(substitute_args("${@:2}", &args), "b c d");
    assert_eq!(substitute_args("${@:2:1}", &args), "b c");
    assert_eq!(
        substitute_args("keep $1x literal", &args),
        "keep ax literal"
    ); // TS substitutes $1 here too

    assert_eq!(
        parse_command_args("one \"two three\" 'four five'"),
        vec!["one", "two three", "four five"]
    );
}

#[test]
fn load_templates_and_project_wins() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    let project = dir.path().join("proj");
    std::fs::create_dir_all(agent.join("prompts")).unwrap();
    std::fs::create_dir_all(project.join(".pi/prompts")).unwrap();
    tack_app::project_trust::set_decision(&agent, &project, true, false);
    std::fs::write(
        agent.join("prompts/review.md"),
        "---\ndescription: global review\n---\nReview $1 globally",
    )
    .unwrap();
    std::fs::write(agent.join("prompts/explain.md"), "Explain $1").unwrap();
    std::fs::write(project.join(".pi/prompts/review.md"), "Review $1 locally").unwrap();

    let templates = load_prompt_templates(&project, &agent);
    assert_eq!(templates.len(), 2);
    let review = templates.iter().find(|t| t.name == "review").unwrap();
    assert_eq!(review.content, "Review $1 locally"); // project replaces global entirely
}
