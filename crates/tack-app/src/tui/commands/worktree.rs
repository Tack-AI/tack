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

    fn show_worktrees(&mut self, infos: &[WorktreeInfo]) {
        if infos.is_empty() {
            self.notice(
                i18n::t(self.lang, "msg.worktree_empty", &[]),
                NoticeKind::Info,
            );
            return;
        }
        let mut text = i18n::t(self.lang, "msg.worktree_header", &[]);
        for info in infos {
            let status = if info.applied {
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
                format!(" — {}", info.label)
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
