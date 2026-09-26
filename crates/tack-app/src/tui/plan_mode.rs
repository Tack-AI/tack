//! Structured plan mode: while the permission mode is `plan`, the agent gets
//! an `exit_plan_mode` tool. Calling it saves the plan to
//! `<agent dir>/plans/plan-<ts>.md` and asks the user to approve:
//!   Yes        → approve, switch to acceptEdits (implement with free edits)
//!   Yes, always→ approve, switch to bypass
//!   No         → reject; the agent stays in plan mode and revises
//!
//! This turns /mode plan from a bare permission level into a closed loop:
//! plan → user review → implementation.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use super::lock_recover;
use super::permission::{PermissionChoice, PermissionMode, PermissionQuery};

/// System-prompt addition while in plan mode.
pub const PLAN_MODE_PROMPT: &str = "\n\n# Plan mode\nYou are in plan mode: \
read-only (no edits, no commands). Research the codebase and design an \
implementation plan. When the plan is ready, call exit_plan_mode with the \
full plan text (numbered steps, files to change, risks). The user reviews \
it: on approval you leave plan mode and implement; on rejection you revise.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExitPlanParams {
    /// The complete implementation plan (markdown)
    plan: String,
}

pub struct ExitPlanModeTool {
    mode: Arc<std::sync::Mutex<PermissionMode>>,
    queries: tokio::sync::mpsc::UnboundedSender<PermissionQuery>,
    plans_dir: PathBuf,
}

impl ExitPlanModeTool {
    pub fn new(
        mode: Arc<std::sync::Mutex<PermissionMode>>,
        queries: tokio::sync::mpsc::UnboundedSender<PermissionQuery>,
        plans_dir: PathBuf,
    ) -> Self {
        ExitPlanModeTool {
            mode,
            queries,
            plans_dir,
        }
    }
}

impl std::fmt::Debug for ExitPlanModeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExitPlanModeTool").finish()
    }
}

#[async_trait]
impl AgentTool for ExitPlanModeTool {
    fn name(&self) -> &'static str {
        "exit_plan_mode"
    }
    fn label(&self) -> &str {
        "exit_plan_mode"
    }
    fn description(&self) -> &str {
        "Call when your implementation plan is ready for user review. Saves the plan to disk \
         and asks the user to approve or reject it. On approval, plan mode ends and you may \
         implement. On rejection, revise the plan based on the feedback."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "The complete implementation plan (markdown)" }
            },
            "required": ["plan"]
        })
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: ExitPlanParams = serde_json::from_value(params)
            .map_err(|e| format!("invalid exit_plan_mode params: {e}"))?;
        if params.plan.trim().is_empty() {
            return Err("plan must not be empty".to_string());
        }

        // Persist the plan.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = std::fs::create_dir_all(&self.plans_dir);
        // Two exit_plan_mode calls in the same second must not overwrite
        // each other's plan.
        let mut plan_path = self.plans_dir.join(format!("plan-{ts}.md"));
        for n in 1.. {
            if !plan_path.exists() {
                break;
            }
            plan_path = self.plans_dir.join(format!("plan-{ts}-{n}.md"));
        }
        std::fs::write(&plan_path, &params.plan).map_err(|e| format!("cannot save plan: {e}"))?;

        // Ask the user.
        let preview: String = params.plan.lines().take(3).collect::<Vec<_>>().join(" / ");
        let preview: String = preview.chars().take(80).collect();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let query = PermissionQuery {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "exit_plan_mode".to_string(),
            title: crate::i18n::trf(
                "permission.plan",
                &[
                    ("preview", &preview),
                    ("path", &plan_path.display().to_string()),
                ],
            ),
            raw_input: serde_json::json!({ "plan": params.plan }),
            respond: tx,
        };
        if self.queries.send(query).is_err() {
            return Err("UI closed".to_string());
        }
        match rx.await {
            Ok(PermissionChoice::AllowOnce) => {
                *lock_recover(&self.mode) = PermissionMode::AcceptEdits;
                Ok(AgentToolResult::text(format!(
                    "Plan approved (saved to {}). Plan mode is now acceptEdits — implement the plan. \
                     Edits run freely; commands still ask.",
                    plan_path.display()
                )))
            }
            Ok(PermissionChoice::AllowAlways) => {
                *lock_recover(&self.mode) = PermissionMode::Bypass;
                Ok(AgentToolResult::text(format!(
                    "Plan approved (saved to {}). Plan mode is now bypass — implement the plan; \
                     no further prompts.",
                    plan_path.display()
                )))
            }
            Ok(PermissionChoice::Deny) | Err(_) => Err(
                "Plan rejected. Stay in plan mode: gather more context or revise the plan, \
                 then call exit_plan_mode again."
                    .to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn approve_switches_to_accept_edits_and_saves_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let mode = Arc::new(std::sync::Mutex::new(PermissionMode::Plan));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PermissionQuery>();
        let tool = ExitPlanModeTool::new(mode.clone(), tx, tmp.path().join("plans"));

        let handle = tokio::spawn(async move {
            tool.execute(
                "1",
                serde_json::json!({ "plan": "1. do x\n2. do y" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
        });
        let query = rx.recv().await.unwrap();
        assert!(query.title.contains("Approve plan?"));
        query.respond.send(PermissionChoice::AllowOnce).unwrap();
        let result = handle.await.unwrap().unwrap();
        let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
            panic!()
        };
        assert!(text.contains("approved"), "{text}");
        assert_eq!(*mode.lock().unwrap(), PermissionMode::AcceptEdits);
        // Plan was persisted.
        let plans: Vec<_> = std::fs::read_dir(tmp.path().join("plans"))
            .unwrap()
            .collect();
        assert_eq!(plans.len(), 1);
    }

    /// Two approvals in the same second must produce two plan files
    /// (previously plan-<secs>.md collided and the second overwrote the first).
    #[tokio::test]
    async fn same_second_plans_do_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let mode = Arc::new(std::sync::Mutex::new(PermissionMode::Plan));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PermissionQuery>();
        for plan in ["plan A", "plan B"] {
            let tool = ExitPlanModeTool::new(mode.clone(), tx.clone(), tmp.path().join("plans"));
            let handle = tokio::spawn(async move {
                tool.execute(
                    "1",
                    serde_json::json!({ "plan": plan }),
                    CancellationToken::new(),
                    &|_| {},
                )
                .await
            });
            let query = rx.recv().await.unwrap();
            query.respond.send(PermissionChoice::AllowOnce).unwrap();
            handle.await.unwrap().unwrap();
        }
        let plans: Vec<_> = std::fs::read_dir(tmp.path().join("plans"))
            .unwrap()
            .collect();
        assert_eq!(plans.len(), 2, "both plans must survive");
        let contents: Vec<String> = plans
            .iter()
            .map(|p| std::fs::read_to_string(p.as_ref().unwrap().path()).unwrap())
            .collect();
        assert!(
            contents.contains(&"plan A".to_string()) && contents.contains(&"plan B".to_string())
        );
    }

    #[tokio::test]
    async fn reject_keeps_plan_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let mode = Arc::new(std::sync::Mutex::new(PermissionMode::Plan));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PermissionQuery>();
        let tool = ExitPlanModeTool::new(mode.clone(), tx, tmp.path().join("plans"));

        let handle = tokio::spawn(async move {
            tool.execute(
                "1",
                serde_json::json!({ "plan": "some plan" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
        });
        let query = rx.recv().await.unwrap();
        query.respond.send(PermissionChoice::Deny).unwrap();
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("rejected"), "{err}");
        assert_eq!(*mode.lock().unwrap(), PermissionMode::Plan);
    }
}
