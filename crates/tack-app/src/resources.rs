//! Project context files (AGENTS.md / CLAUDE.md discovery). Port of
//! `loadProjectContextFiles` from resource-loader.ts: global agent-dir file
//! first, then ancestors from the repo root down to cwd, including the
//! nested-worktree shadowing rule (`findShadowedContextFile`).

use std::path::{Path, PathBuf};

use crate::system_prompt::ContextFile;

const CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

fn load_context_file_from_dir(dir: &Path) -> Option<ContextFile> {
    for name in CANDIDATES {
        let path = dir.join(name);
        if path.is_file()
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            return Some(ContextFile {
                path: path.to_string_lossy().to_string(),
                content: content.trim_start_matches('\u{FEFF}').to_string(),
            });
        }
    }
    None
}

/// Load every `*.md` under a directory as context files (sorted by filename
/// for deterministic order). Used for the `agent/rules` convention.
fn load_rules_dir(dir: &Path) -> Vec<ContextFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<ContextFile> = entries
        .flatten()
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_file())
                && e.file_name().to_string_lossy().ends_with(".md")
        })
        .filter_map(|e| {
            let path = e.path();
            let content = std::fs::read_to_string(&path).ok()?;
            Some(ContextFile {
                path: path.to_string_lossy().to_string(),
                content: content.trim_start_matches('\u{FEFF}').to_string(),
            })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Where a repo's work tree and common git dir live (TS `GitPaths`).
#[derive(Debug)]
pub(crate) struct GitPaths {
    repo_dir: PathBuf,
    common_git_dir: PathBuf,
    /// The worktree's own HEAD file (per-worktree for linked worktrees).
    pub head: PathBuf,
}

fn resolve_gitdir_target(base: &Path, target: &str) -> PathBuf {
    let target = Path::new(target.trim());
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    }
}

/// Port of `findGitPaths` (utils/git.ts): walk up from `cwd` to the dir
/// holding `.git` — a directory for an ordinary repo, a `gitdir:` file for
/// a linked worktree or submodule.
pub(crate) fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        let git_path = dir.join(".git");
        if git_path.is_file() {
            let content = std::fs::read_to_string(&git_path).ok()?;
            let target = content.trim().strip_prefix("gitdir: ")?;
            let git_dir = resolve_gitdir_target(dir, target);
            let head = git_dir.join("HEAD");
            if !head.exists() {
                return None;
            }
            let common_dir_file = git_dir.join("commondir");
            let common_git_dir = if common_dir_file.is_file() {
                let rel = std::fs::read_to_string(&common_dir_file).ok()?;
                resolve_gitdir_target(&git_dir, &rel)
            } else {
                git_dir
            };
            return Some(GitPaths {
                repo_dir: dir.to_path_buf(),
                common_git_dir,
                head,
            });
        }
        if git_path.is_dir() {
            let head = git_path.join("HEAD");
            if !head.exists() {
                return None;
            }
            return Some(GitPaths {
                repo_dir: dir.to_path_buf(),
                common_git_dir: git_path,
                head,
            });
        }
        current = dir.parent();
    }
    None
}

fn canonicalize(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Port of `findShadowedContextFile` (resource-loader.ts): the main repo's
/// context file that a nested linked worktree's own copy shadows. Both
/// occupy the same logical repository scope, so loading both would apply
/// that context twice. Returns None when nothing is shadowed, leaving
/// normal ancestor inheritance alone.
///
/// Canonicalized (realpath), because `git worktree add` writes the `.git`
/// file's `gitdir:` target in realpath form while cwd may still be
/// symlinked (macOS `/tmp` -> `/private/tmp`).
fn find_shadowed_context_file(cwd: &Path) -> Option<PathBuf> {
    let git = find_git_paths(cwd)?;
    let common_git_dir = canonicalize(&git.common_git_dir);
    let worktree_root = canonicalize(&git.repo_dir);
    let main_repo_root = common_git_dir.parent()?;
    // False for an ordinary repo, where the two are the same dir, and for a
    // sibling worktree (`git worktree add ../feat`), whose main repo is not
    // an ancestor.
    if worktree_root == main_repo_root || !worktree_root.starts_with(main_repo_root) {
        return None;
    }
    // The parent of the common git dir is the main worktree root only when
    // that dir is itself checked out from the same repo. In a bare layout
    // (`proj/.bare` + `proj/main`) it just holds `.bare`; a submodule's
    // gitdir has no `commondir` and lands under `.git/modules`.
    if canonicalize(&main_repo_root.join(".git")) != common_git_dir {
        return None;
    }
    let worktree_context_file = load_context_file_from_dir(&worktree_root)?;
    let file_name = Path::new(&worktree_context_file.path).file_name()?;
    Some(main_repo_root.join(file_name))
}

pub fn load_project_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    load_project_context_files_with_extra(cwd, agent_dir, &[])
}

/// Same as load_project_context_files, plus AGENTS.md/CLAUDE.md from
/// additional working directories (`--add-dir` / settings additionalDirs).
/// Extra dirs load unconditionally (the user explicitly added them).
pub fn load_project_context_files_with_extra(
    cwd: &Path,
    agent_dir: &Path,
    extra_dirs: &[PathBuf],
) -> Vec<ContextFile> {
    let mut files = load_core(cwd, agent_dir);
    let mut seen: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    for dir in extra_dirs {
        if let Some(file) = load_context_file_from_dir(dir)
            && !seen.contains(&file.path)
        {
            seen.push(file.path.clone());
            files.push(file);
        }
    }
    files
}

fn load_core(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut files: Vec<ContextFile> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    if let Some(global) = load_context_file_from_dir(agent_dir) {
        seen.push(global.path.clone());
        files.push(global);
    }

    // `~/.tack/agent/rules/*.md`: every markdown file, sorted by name.
    for file in load_rules_dir(&agent_dir.join("rules")) {
        if !seen.contains(&file.path) {
            seen.push(file.path.clone());
            files.push(file);
        }
    }

    // Ancestors from root down to cwd (project rules — trust-gated).
    if crate::project_trust::is_trusted(cwd, agent_dir) {
        let shadowed = find_shadowed_context_file(cwd);
        let mut ancestors: Vec<ContextFile> = Vec::new();
        let mut current = Some(cwd);
        while let Some(dir) = current {
            if let Some(file) = load_context_file_from_dir(dir) {
                let is_shadowed = shadowed
                    .as_ref()
                    .is_some_and(|s| *s == canonicalize(Path::new(&file.path)));
                if !is_shadowed && !seen.contains(&file.path) {
                    seen.push(file.path.clone());
                    ancestors.push(file);
                }
            }
            current = dir.parent();
        }
        ancestors.reverse();
        files.extend(ancestors);
    }
    files
}

#[cfg(test)]
mod worktree_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Build a main repo (`main/.git/` directory) with a linked worktree
    /// nested inside it at `main/feat` (`.git` file + commondir), as
    /// `git worktree add feat` would write it.
    fn make_nested_worktree(root: &Path) -> (PathBuf, PathBuf) {
        let main = root.join("main");
        let git_dir = main.join(".git");
        let wt_git = git_dir.join("worktrees/feat");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/feat\n").unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let feat = main.join("feat");
        std::fs::create_dir_all(&feat).unwrap();
        std::fs::write(feat.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        (main, feat)
    }

    #[test]
    fn nested_worktree_shadows_main_repo_context_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (main, feat) = make_nested_worktree(tmp.path());
        std::fs::write(main.join("AGENTS.md"), "main rule").unwrap();
        std::fs::write(feat.join("AGENTS.md"), "worktree rule").unwrap();
        let agent = tempfile::tempdir().unwrap();
        let files = load_project_context_files(&feat, agent.path());
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["worktree rule"]);
    }

    #[test]
    fn ordinary_repo_ancestors_unaffected() {
        // Same nesting but the child is a plain directory (no `.git` file):
        // the repo root's context file loads normally.
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let sub = main.join("feat");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir(main.join(".git")).unwrap();
        std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(main.join("AGENTS.md"), "main rule").unwrap();
        let agent = tempfile::tempdir().unwrap();
        let files = load_project_context_files(&sub, agent.path());
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["main rule"]);
    }

    #[test]
    fn worktree_without_own_context_file_inherits_main() {
        // The worktree has no AGENTS.md of its own, so nothing shadows the
        // main repo's copy (TS: shadowing is keyed on the worktree's file).
        let tmp = tempfile::tempdir().unwrap();
        let (main, feat) = make_nested_worktree(tmp.path());
        std::fs::write(main.join("AGENTS.md"), "main rule").unwrap();
        let agent = tempfile::tempdir().unwrap();
        let files = load_project_context_files(&feat, agent.path());
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["main rule"]);
    }

    #[test]
    fn git_paths_detects_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let (main, feat) = make_nested_worktree(tmp.path());
        let paths = find_git_paths(&feat).unwrap();
        assert_eq!(canonicalize(&paths.repo_dir), canonicalize(&feat));
        assert_eq!(
            canonicalize(&paths.common_git_dir),
            canonicalize(&main.join(".git"))
        );
        // Ordinary repo: `.git` directory.
        let plain = tempfile::tempdir().unwrap();
        std::fs::create_dir(plain.path().join(".git")).unwrap();
        std::fs::write(plain.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let paths = find_git_paths(plain.path()).unwrap();
        assert_eq!(canonicalize(&paths.repo_dir), canonicalize(plain.path()));
        assert_eq!(
            canonicalize(&paths.common_git_dir),
            canonicalize(&plain.path().join(".git"))
        );
        // `.git` without HEAD is not a repo.
        let bogus = tempfile::tempdir().unwrap();
        std::fs::create_dir(bogus.path().join(".git")).unwrap();
        assert!(find_git_paths(bogus.path()).is_none());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn global_and_ancestors_in_order() {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("AGENTS.md"), "global rule").unwrap();
        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("sub/dir");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "project rule").unwrap();
        let files = load_project_context_files(&nested, &agent);
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["global rule", "project rule"]);
    }

    #[test]
    fn agent_rules_dir_loaded_sorted() {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        let rules = agent.join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("b-second.md"), "second rule").unwrap();
        std::fs::write(rules.join("a-first.md"), "first rule").unwrap();
        std::fs::write(rules.join("not-md.txt"), "ignored").unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let files = load_project_context_files(cwd.path(), &agent);
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert!(contents.contains(&"first rule"), "{contents:?}");
        assert!(contents.contains(&"second rule"), "{contents:?}");
        assert!(!contents.contains(&"ignored"), "{contents:?}");
        let first_pos = contents.iter().position(|c| *c == "first rule").unwrap();
        let second_pos = contents.iter().position(|c| *c == "second rule").unwrap();
        assert!(first_pos < second_pos);
    }
}
