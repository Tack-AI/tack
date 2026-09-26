//! The optional PowerShell tool (Windows). Port of
//! `packages/coding-agent/src/core/tools/powershell.ts` (TS 0.84.3 parity).
//!
//! TS semantics:
//! - Optional tool: not part of the default coding tool set; the host
//!   registers it only when `powershell` is listed in `defaultTools`.
//! - Registration is platform-independent — the tool definition is created
//!   on any OS; shell resolution happens lazily at execution time and fails
//!   with "The powershell tool is only available on Windows." off-Windows
//!   (TS `getPowerShellConfig`).
//! - Execution prefers PowerShell 7 (`pwsh.exe`) over Windows PowerShell
//!   (`powershell.exe`), always with `-NoProfile -NonInteractive
//!   -ExecutionPolicy Bypass -Command`, and prefixes the command with a
//!   best-effort UTF-8 console-output shim.
//!
//! Execution reuses the bash tool's executor pipeline (sandbox, timeout,
//! cancellation, output truncation) via `bash::run_shell_command`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult, ToolExecutionMode};
use tokio_util::sync::CancellationToken;

use crate::executor::LocalBashExecutor;
use crate::services::ToolServices;

/// Best-effort UTF-8 console output shim prepended to every command
/// (TS `UTF8_OUTPUT_PREFIX`).
pub const UTF8_OUTPUT_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

/// TS `createLocalPowerShellOperations`: force UTF-8 output, then the command.
pub fn prefixed_command(command: &str) -> String {
    format!("{UTF8_OUTPUT_PREFIX}{command}")
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PowerShellParams {
    /// PowerShell command to execute
    command: String,
    /// Timeout in seconds (optional, no default timeout)
    timeout: Option<f64>,
}

pub struct PowerShellTool {
    services: ToolServices,
}

impl PowerShellTool {
    pub fn new(services: ToolServices) -> Self {
        PowerShellTool { services }
    }
}

impl std::fmt::Debug for PowerShellTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PowerShellTool").finish()
    }
}

#[async_trait]
impl AgentTool for PowerShellTool {
    fn name(&self) -> &'static str {
        "powershell"
    }
    fn label(&self) -> &str {
        "powershell"
    }
    fn description(&self) -> &str {
        "Execute a PowerShell command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<PowerShellParams>()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: PowerShellParams = serde_json::from_value(params)
            .map_err(|e| format!("invalid powershell params: {e}"))?;

        // Arbitrary commands mutate files outside the edit/write tools, so
        // the turn's git baseline must be complete before we run anything.
        self.services.checkpoints.settle();

        let timeout = crate::bash::validate_timeout(params.timeout)?;

        // Lazy shell resolution (TS getPowerShellConfig): errors off-Windows
        // or when neither pwsh.exe nor powershell.exe is on PATH.
        let shell = crate::shell::resolve_powershell().map_err(|e| e.to_string())?;
        let sandbox = self
            .services
            .sandbox
            .as_ref()
            .and_then(crate::sandbox::resolve);
        let executor: std::sync::Arc<dyn crate::executor::BashExecutor> =
            std::sync::Arc::new(LocalBashExecutor {
                shell: std::sync::Arc::new(shell),
                sandbox: sandbox.clone(),
                env: self.services.env.clone(),
            });

        let command = prefixed_command(&params.command);
        crate::bash::run_shell_command(
            &executor,
            &command,
            &self.services.cwd,
            params.timeout,
            timeout,
            "tack-powershell",
            cancel,
            on_update,
            sandbox.as_ref().map(|(_, spec)| spec),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tool() -> PowerShellTool {
        PowerShellTool::new(crate::default_services(std::env::current_dir().unwrap()))
    }

    /// The UTF-8 shim is prepended verbatim, exactly once (TS
    /// createLocalPowerShellOperations wraps at the operations layer, so the
    /// model's command is never double-prefixed).
    #[test]
    fn command_gets_utf8_prefix() {
        let cmd = prefixed_command("Get-ChildItem");
        assert!(cmd.starts_with(UTF8_OUTPUT_PREFIX));
        assert!(cmd.ends_with("Get-ChildItem"));
        assert_eq!(cmd.matches("OutputEncoding").count(), 1);
    }

    /// Metadata parity with TS powershellToolConfig: name/label
    /// "powershell", description names PowerShell + the truncation budget.
    #[test]
    fn tool_metadata() {
        let tool = tool();
        assert_eq!(tool.name(), "powershell");
        assert_eq!(tool.label(), "powershell");
        assert!(tool.description().contains("PowerShell command"));
        assert!(tool.description().contains("truncated"));
    }

    /// Schema: required `command`, optional `timeout`, and — unlike the
    /// tack bash extension — no `run_in_background` (TS bashSchema shape).
    #[test]
    fn parameters_schema_shape() {
        let schema = tool().parameters_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("command"));
        assert!(props.contains_key("timeout"));
        assert!(!props.contains_key("run_in_background"));
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required, &vec![Value::String("command".to_string())]);
    }

    /// Timeout validation is shared with the bash tool.
    #[test]
    fn timeout_validation() {
        assert!(crate::bash::validate_timeout(None).unwrap().is_none());
        assert!(crate::bash::validate_timeout(Some(1.5)).unwrap().is_some());
        assert!(crate::bash::validate_timeout(Some(0.0)).is_err());
        assert!(crate::bash::validate_timeout(Some(-3.0)).is_err());
        assert!(crate::bash::validate_timeout(Some(f64::NAN)).is_err());
        assert!(crate::bash::validate_timeout(Some(f64::INFINITY)).is_err());
        assert!(crate::bash::validate_timeout(Some(3_000_000.0)).is_err());
    }

    /// Off-Windows the tool registers fine but execution fails with the TS
    /// getPowerShellConfig error (lazy shell resolution).
    #[cfg(not(windows))]
    #[tokio::test]
    async fn execute_errors_off_windows() {
        let err = tool()
            .execute(
                "1",
                serde_json::json!({ "command": "Get-ChildItem" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("only available on Windows"),
            "unexpected: {err}"
        );
    }
}
