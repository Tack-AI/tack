//! The write tool. Port of `tools/write.ts`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::path_utils::resolve_to_cwd;
use crate::services::ToolServices;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WriteParams {
    /// Path to the file to write (relative or absolute)
    path: String,
    /// Content to write to the file
    content: String,
}

pub struct WriteTool {
    services: ToolServices,
}

impl WriteTool {
    pub fn new(services: ToolServices) -> Self {
        WriteTool { services }
    }
}

impl std::fmt::Debug for WriteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTool").finish()
    }
}

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn label(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Creates parent directories as needed. Use this for new files or complete rewrites."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<WriteParams>()
    }

    fn constrained_sampling(&self) -> Option<tack_ai::constrained_sampling::ConstrainedSampling> {
        crate::prefer_strict_sampling()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: WriteParams =
            serde_json::from_value(params).map_err(|e| format!("invalid write params: {e}"))?;
        let absolute = resolve_to_cwd(&params.path, &self.services.cwd);

        // Hold the mutation lock for the whole write (checks observe aborts
        // while keeping the queue locked until the operation settles).
        let _guard = self.services.mutation_lock.lock().await;
        if cancel.is_cancelled() {
            return Err("Operation aborted".to_string());
        }

        if let Some(dir) = absolute.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|e| format!("Could not create directory {}: {e}", dir.display()))?;
        }
        if cancel.is_cancelled() {
            return Err("Operation aborted".to_string());
        }
        // Checkpoint the pre-write state (first touch per turn).
        self.services.checkpoints.record(&absolute);
        tokio::fs::write(&absolute, &params.content)
            .await
            .map_err(|e| format!("Could not write file: {}. {e}", params.path))?;

        // LSP feedback loop (see edit.rs): release the lock before waiting.
        drop(_guard);
        let mut message = format!(
            "Successfully wrote {} bytes to {}",
            params.content.len(),
            params.path
        );
        if let Some(summary) = crate::lsp::post_edit_summary(&self.services.lsp, &absolute).await {
            message.push_str("\n\n");
            message.push_str(&summary);
        }
        Ok(AgentToolResult::text(message))
    }
}
