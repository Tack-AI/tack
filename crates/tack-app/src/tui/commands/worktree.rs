//! `/worktree` — list, apply, discard, or prune subagent worktrees.
//!
//! The subagent tool leaves isolated worktrees behind on purpose; these
//! commands are the human end of that loop. `apply` stages the worktree's
//! changes in the current repo (squash merge, no commit), `discard` removes
//! the checkout and its branch, `prune` forgets worktrees that vanished
//! outside tack.

use std::path::Path;

use super::super::chat::TranscriptItem;
use super::super::{ChatEntry, NoticeKind, TuiApp};
use crate::i18n;
use crate::worktrees::{self, ApplyOutcome, WorktreeInfo};
use tack_tools::background::TaskSummary;

impl TuiApp {
    pub(crate) async fn command_worktree(&mut self, args: &str) {
        let repo = match worktrees::repo_root(&self.cwd).await {
            Ok(repo) => repo,
            Err(_) => {
                self.notice(
                    i18n::t(
                        self.lang,
                        "msg.worktree_not_repo",
                        &[("cwd", &self.cwd.display().to_string())],
                    ),
                    NoticeKind::Warning,
                );
                return;
            }
        };
        let agent_dir = self.agent_dir.clone();
        let mut parts = args.split_whitespace();
        let action = parts.next().unwrap_or("list");
        // Applying/discarding while the agent edits files can race it.
        // Listing is read-only and stays available.
        if action != "list" && self.running {
            self.notice(
                i18n::t(self.lang, "msg.worktree_busy", &[]),
                NoticeKind::Warning,
            );
            return;
        }
        match action {
            "list" => {
                let infos = worktrees::list(&agent_dir, &repo, true).await;
                self.show_worktrees(&infos);
            }
            "apply" => {
                let Some(branch) = self.worktree_target(&agent_dir, &repo, parts.next()).await
                else {
                    return;
                };
                if let Some(task_id) = self.worktree_running_task(&agent_dir, &repo, &branch).await
                {
                    self.notice(
                        i18n::trf("msg.worktree_task_running", &[("id", &task_id)]),
                        NoticeKind::Warning,
                    );
                    return;
                }
                match worktrees::apply(&agent_dir, &repo, &branch).await {
                    Ok(ApplyOutcome::Applied { summary }) => self.notice(
                        i18n::trf(
                            "msg.worktree_applied",
                            &[
                                ("branch", &branch),
                                ("summary", summary.as_deref().unwrap_or("")),
                            ],
                        ),
                        NoticeKind::Info,
                    ),
                    Ok(ApplyOutcome::NoChanges) => self.notice(
                        i18n::trf("msg.worktree_no_changes", &[("branch", &branch)]),
                        NoticeKind::Info,
                    ),
                    Err(error) => self.notice(
                        i18n::trf("msg.worktree_failed", &[("error", &error)]),
                        NoticeKind::Error,
                    ),
                }
            }
            "discard" | "remove" | "rm" => {
                let Some(branch) = self.worktree_target(&agent_dir, &repo, parts.next()).await
                else {
                    return;
                };
                if let Some(task_id) = self.worktree_running_task(&agent_dir, &repo, &branch).await
                {
                    self.notice(
                        i18n::trf("msg.worktree_task_running", &[("id", &task_id)]),
                        NoticeKind::Warning,
                    );
                    return;
                }
                match worktrees::discard(&agent_dir, &repo, &branch).await {
                    Ok(()) => self.notice(
                        i18n::trf("msg.worktree_discarded", &[("branch", &branch)]),
                        NoticeKind::Info,
                    ),
                    Err(error) => self.notice(
                        i18n::trf("msg.worktree_failed", &[("error", &error)]),
                        NoticeKind::Error,
                    ),
                }
            }
            "prune" => {
                let removed = worktrees::prune(&agent_dir, &repo).await;
                self.notice(
                    i18n::trf("msg.worktree_pruned", &[("count", &removed.to_string())]),
                    NoticeKind::Info,
                );
            }
            _ => self.notice(
                i18n::t(self.lang, "msg.worktree_usage", &[]),
                NoticeKind::Warning,
            ),
        }
    }

    /// Explicit branch, or the only known one. With several candidates the
    /// user must name one — never guess which worktree to merge or delete.
    async fn worktree_target(
        &mut self,
        agent_dir: &Path,
        repo: &Path,
        explicit: Option<&str>,
    ) -> Option<String> {
        if let Some(branch) = explicit {
            return Some(branch.to_string());
        }
        let infos = worktrees::list(agent_dir, repo, false).await;
        match infos.as_slice() {
            [] => {
                self.notice(
                    i18n::t(self.lang, "msg.worktree_empty", &[]),
                    NoticeKind::Warning,
                );
                None
            }
            [only] => Some(only.branch.clone()),
            _ => {
                self.notice(
                    i18n::t(self.lang, "msg.worktree_usage", &[]),
                    NoticeKind::Warning,
                );
                None
            }
        }
    }

    /// The background task still writing this worktree, if any. Applying or
    /// discarding while the subagent edits it would race (or delete the
    /// checkout under a live child).
    async fn worktree_running_task(
        &self,
        agent_dir: &Path,
        repo: &Path,
        branch: &str,
    ) -> Option<String> {
        let info = worktrees::list(agent_dir, repo, false)
            .await
            .into_iter()
            .find(|info| info.branch == branch)?;
        running_task_for(&info, &self.background_tasks.tasks())
    }

    fn show_worktrees(&mut self, infos: &[WorktreeInfo]) {
        if infos.is_empty() {
            self.notice(
                i18n::t(self.lang, "msg.worktree_empty", &[]),
                NoticeKind::Info,
            );
            return;
        }
        let running: Vec<TaskSummary> = self
            .background_tasks
            .tasks()
            .into_iter()
            .filter(|task| task.running())
            .collect();
        let mut text = i18n::t(self.lang, "msg.worktree_header", &[]);
        for info in infos {
            let is_running = running_task_for(info, &running).is_some();
            let status = if is_running {
                i18n::t(self.lang, "msg.worktree_status_running", &[])
            } else if info.applied {
                i18n::t(self.lang, "msg.worktree_status_applied", &[])
            } else {
                i18n::t(self.lang, "msg.worktree_status_pending", &[])
            };
            let summary = match &info.summary {
                Some(summary) => {
                    let mut parts = Vec::new();
                    if let Some(shortstat) = &summary.shortstat {
                        parts.push(shortstat.clone());
                    }
                    if summary.untracked > 0 {
                        parts.push(i18n::trf(
                            "msg.worktree_untracked",
                            &[("count", &summary.untracked.to_string())],
                        ));
                    }
                    parts.join(" · ")
                }
                None if !info.present => i18n::t(self.lang, "msg.worktree_missing", &[]),
                None => String::new(),
            };
            let label = if info.label.is_empty() {
                String::new()
            } else {
                // Task text may be multi-line; keep the list item on one line.
                format!(" — {}", info.label.replace(['\n', '\r'], " "))
            };
            text.push_str(&format!(
                "- `{}` [{status}] {summary}{label}\n  `{}`\n",
                info.branch,
                info.path.display()
            ));
        }
        text.push('\n');
        text.push_str(&i18n::t(self.lang, "msg.worktree_hint", &[]));
        self.items
            .push(TranscriptItem::Chat(ChatEntry::Markdown { text }));
        self.line_cache.clear();
    }
}

/// The still-running task that owns `info`, if any. Task ids restart at bg1
/// every process and registry rows outlive the process, so the label must
/// still match too — a stale `bg1` must not block a different new task.
fn running_task_for(info: &WorktreeInfo, tasks: &[TaskSummary]) -> Option<String> {
    let task_id = info.task_id.as_ref()?;
    let label_prefix: String = info.label.chars().take(60).collect();
    tasks
        .iter()
        .find(|task| &task.id == task_id && task.running() && task.label.contains(&label_prefix))
        .map(|task| task.id.clone())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tack_tools::background::TaskStatus;

    fn info(task_id: Option<&str>) -> WorktreeInfo {
        WorktreeInfo {
            branch: "tack-subagent-ab12".to_string(),
            path: std::path::PathBuf::from("/tmp/wt"),
            repo: std::path::PathBuf::from("/tmp/repo"),
            label: "refactor auth".to_string(),
            applied: false,
            present: true,
            summary: None,
            task_id: task_id.map(str::to_string),
        }
    }

    fn task(id: &str, label: &str, status: TaskStatus) -> TaskSummary {
        TaskSummary {
            id: id.to_string(),
            label: label.to_string(),
            status,
            error: None,
            elapsed_seconds: 0,
            accepts_messages: true,
        }
    }

    #[test]
    fn running_task_requires_matching_id_label_and_status() {
        let worktree = info(Some("bg1"));
        assert_eq!(
            running_task_for(
                &worktree,
                &[task("bg1", "subagent: refactor auth", TaskStatus::Running)]
            )
            .as_deref(),
            Some("bg1")
        );
        // Stale id from a previous process: a different label must not block.
        assert!(
            running_task_for(
                &worktree,
                &[task("bg1", "subagent: unrelated", TaskStatus::Running)]
            )
            .is_none()
        );
        assert!(
            running_task_for(
                &worktree,
                &[task("bg2", "subagent: refactor auth", TaskStatus::Running)]
            )
            .is_none()
        );
        assert!(
            running_task_for(
                &worktree,
                &[task(
                    "bg1",
                    "subagent: refactor auth",
                    TaskStatus::Exited(0)
                )]
            )
            .is_none()
        );
        // Foreground worktrees have no task id.
        assert!(running_task_for(&info(None), &[]).is_none());
    }
}
