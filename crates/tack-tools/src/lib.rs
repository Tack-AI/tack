//! Built-in coding tools: read, bash, edit, write, grep, find, ls.
//!
//! Rust port of `packages/coding-agent/src/core/tools/`. Output text formats
//! and truncation budgets match the TypeScript tools.

pub mod accumulator;
pub mod ask_codebuddy;
pub mod background;
pub mod bash;
pub mod browser;
pub mod checkpoint;
pub mod edit;
pub mod edit_diff;
pub mod edit_fuzzy;
pub mod executor;
pub mod git_tool;
pub mod lsp;
pub mod mcp;
pub mod mcp_sse;
pub mod memory;
pub mod path_utils;
pub mod powershell;
pub mod read;
pub mod sandbox;
pub mod search;
pub mod services;
pub mod shell;
pub mod todo;
pub mod tool_search;
pub mod truncate;
pub mod web;
#[cfg(windows)]
pub mod windows_job;
pub mod write;

use std::path::PathBuf;
use std::sync::Arc;

use tack_agent_core::AgentTool;

pub use bash::BashTool;
pub use edit::EditTool;
pub use git_tool::GitTool;
pub use powershell::PowerShellTool;
pub use read::ReadTool;
pub use search::{FindTool, GrepTool, LsTool};
pub use services::ToolServices;
pub use write::WriteTool;

/// JSON schema for a tool's params type.
pub(crate) fn schema_for<T: schemars::JsonSchema>() -> serde_json::Value {
    let schema = schemars::schema_for!(T);
    serde_json::to_value(&schema).expect("schemars schema is always serializable")
}

/// Strict JSON-schema constrained sampling in "prefer" mode (TS pi
/// fcff255b0: built-in tools default to `{ type: "json_schema", strict:
/// "prefer" }`). Degrades gracefully when the provider or schema does not
/// support strict mode.
pub(crate) fn prefer_strict_sampling() -> Option<tack_ai::constrained_sampling::ConstrainedSampling>
{
    Some(
        tack_ai::constrained_sampling::ConstrainedSampling::JsonSchema {
            strict: Some("prefer".to_string()),
        },
    )
}

/// Build shared services for a working directory, resolving the shell.
pub fn default_services(cwd: PathBuf) -> ToolServices {
    let services = ToolServices::new(cwd);
    match shell::resolve_shell(None) {
        Ok(config) => services.with_shell(config),
        Err(e) => {
            tracing::warn!("no shell resolved, bash tool will fail: {e}");
            services
        }
    }
}

/// The coding tool set (read/bash/edit/write + web access) — mirrors pi's
/// createCodingTools plus tack web tools.
pub fn create_coding_tools(services: &ToolServices) -> Vec<Arc<dyn AgentTool>> {
    let mut tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(ReadTool::new(services.clone())),
        Arc::new(BashTool::new(services.clone())),
        Arc::new(GitTool::new(services.clone())),
        Arc::new(background::BashOutputTool::new(services.clone())),
        Arc::new(background::BashWaitTool::new(services.clone())),
        Arc::new(background::KillShellTool::new(services.clone())),
        Arc::new(lsp::LspTool::new(services.clone())),
        Arc::new(memory::MemoryTool::new(services.clone())),
        Arc::new(EditTool::new(services.clone())),
        Arc::new(WriteTool::new(services.clone())),
        Arc::new(crate::web::WebFetchTool::new(services.clone())),
        Arc::new(crate::web::WebSearchTool::new(services.clone())),
    ];
    // AskCodebuddy parity: only when the codebuddy CLI is installed.
    if tack_ai::codebuddy::cli_available() {
        tools.push(Arc::new(ask_codebuddy::AskCodebuddyTool::new()));
    }
    tools
}

/// Read-only tools (read/grep/find/ls) — mirrors pi's createReadOnlyTools.
pub fn create_read_only_tools(services: &ToolServices) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(ReadTool::new(services.clone())),
        Arc::new(GrepTool::new(services.clone())),
        Arc::new(FindTool::new(services.clone())),
        Arc::new(LsTool::new(services.clone())),
    ]
}

/// All built-in tools — mirrors pi's createAllTools. Unlike the coding tool
/// set this includes the optional PowerShell tool (registered on any
/// platform; execution resolves PowerShell lazily and errors off-Windows).
pub fn create_all_tools(services: &ToolServices) -> Vec<Arc<dyn AgentTool>> {
    let mut tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(ReadTool::new(services.clone())),
        Arc::new(BashTool::new(services.clone())),
        Arc::new(PowerShellTool::new(services.clone())),
        Arc::new(GitTool::new(services.clone())),
        Arc::new(background::BashOutputTool::new(services.clone())),
        Arc::new(background::BashWaitTool::new(services.clone())),
        Arc::new(background::KillShellTool::new(services.clone())),
        Arc::new(lsp::LspTool::new(services.clone())),
        Arc::new(memory::MemoryTool::new(services.clone())),
        Arc::new(EditTool::new(services.clone())),
        Arc::new(WriteTool::new(services.clone())),
        Arc::new(GrepTool::new(services.clone())),
        Arc::new(FindTool::new(services.clone())),
        Arc::new(LsTool::new(services.clone())),
        Arc::new(crate::web::WebFetchTool::new(services.clone())),
        Arc::new(crate::web::WebSearchTool::new(services.clone())),
    ];
    // AskCodebuddy parity: only when the codebuddy CLI is installed.
    if tack_ai::codebuddy::cli_available() {
        tools.push(Arc::new(ask_codebuddy::AskCodebuddyTool::new()));
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TS pi fcff255b0: the core built-in tools (read/bash/edit/write)
    /// default to strict JSON-schema sampling in "prefer" mode.
    #[test]
    fn core_tools_prefer_strict_sampling() {
        let services = crate::default_services(std::path::PathBuf::from("/tmp"));
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(ReadTool::new(services.clone())),
            Arc::new(BashTool::new(services.clone())),
            Arc::new(EditTool::new(services.clone())),
            Arc::new(WriteTool::new(services.clone())),
        ];
        for tool in &tools {
            let def = tack_agent_core::tool_definition(tool.as_ref());
            assert_eq!(
                def.constrained_sampling,
                Some(
                    tack_ai::constrained_sampling::ConstrainedSampling::JsonSchema {
                        strict: Some("prefer".to_string()),
                    }
                ),
                "tool {} must prefer strict sampling",
                def.name
            );
        }
    }
}
