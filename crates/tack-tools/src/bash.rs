//! The bash tool. Port of `tools/bash.ts` execution semantics: streaming
//! output with throttled partial updates, tail truncation with temp-file
//! spillover, timeout, and abort handling. Raw execution is delegated to a
//! `BashExecutor` (local shell by default; ACP client terminals in ACP mode).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult, ToolExecutionMode};
use tokio_util::sync::CancellationToken;

use crate::accumulator::OutputAccumulator;
use crate::executor::LocalBashExecutor;
use crate::services::ToolServices;
use crate::shell::sanitize_binary_output;
use crate::truncate::{DEFAULT_MAX_BYTES, TruncatedBy, format_size};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BashParams {
    /// Bash command to execute
    command: String,
    /// Timeout in seconds (optional, no default timeout)
    timeout: Option<f64>,
    /// Run the command in the background and return immediately with a task
    /// id. Use bash_wait to block until it finishes, bash_output to poll its
    /// output, and kill_shell to stop it. Use for long-running commands
    /// (dev servers, builds, watch mode).
    run_in_background: Option<bool>,
}

const MAX_TIMEOUT_SECONDS: f64 = 2_147_483.647;

/// Validate the optional `timeout` param (seconds) shared by the shell tools.
pub(crate) fn validate_timeout(
    timeout: Option<f64>,
) -> Result<Option<std::time::Duration>, String> {
    match timeout {
        None => Ok(None),
        Some(t) if !t.is_finite() || t <= 0.0 => {
            Err("Invalid timeout: must be a finite number of seconds".to_string())
        }
        Some(t) if t > MAX_TIMEOUT_SECONDS => Err(format!(
            "Invalid timeout: maximum is {} seconds",
            MAX_TIMEOUT_SECONDS as u64
        )),
        Some(t) => Ok(Some(std::time::Duration::from_secs_f64(t))),
    }
}

pub struct BashTool {
    services: ToolServices,
    command_prefix: Option<String>,
}

impl BashTool {
    pub fn new(services: ToolServices) -> Self {
        BashTool {
            services,
            command_prefix: None,
        }
    }

    pub fn with_command_prefix(mut self, prefix: String) -> Self {
        self.command_prefix = Some(prefix);
        self
    }
}

impl std::fmt::Debug for BashTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashTool").finish()
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn label(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        if self.services.background_tasks_enabled {
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds. Set run_in_background=true for long-running commands (dev servers, builds, installs) and for anything you would otherwise wait on: you are notified automatically when a background task finishes, so never block in the foreground with `sleep` or polling loops — run the wait in the background and keep working. When the next step depends on a background task, wait for it with bash_wait (a foreground `sleep` cannot be interrupted by the completion notification; bash_wait returns the moment the task finishes). Poll with bash_output, stop with kill_shell."
        } else {
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds."
        }
    }
    fn parameters_schema(&self) -> Value {
        let mut schema = crate::schema_for::<BashParams>();
        if !self.services.background_tasks_enabled {
            // Hide the background option entirely: a disabled feature must
            // not be visible to the model.
            if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
                props.remove("run_in_background");
            }
        }
        schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn constrained_sampling(&self) -> Option<tack_ai::constrained_sampling::ConstrainedSampling> {
        crate::prefer_strict_sampling()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: BashParams =
            serde_json::from_value(params).map_err(|e| format!("invalid bash params: {e}"))?;

        // Arbitrary commands mutate files outside the edit/write tools, so
        // the turn's git baseline must be complete before we run anything.
        self.services.checkpoints.settle();

        let timeout = validate_timeout(params.timeout)?;

        let command = match &self.command_prefix {
            Some(prefix) => format!("{prefix}\n{}", params.command),
            None => params.command.clone(),
        };

        // Background execution: hand off to the task manager and return the
        // task id immediately (local shell only — custom executors like ACP
        // client terminals don't support detaching).
        if params.run_in_background == Some(true) {
            if !self.services.background_tasks_enabled {
                return Err("background tasks are disabled (features.backgroundTasks)".to_string());
            }
            if self.services.bash_executor.is_some() {
                return Err(
                    "run_in_background is not supported with this execution backend".to_string(),
                );
            }
            let task_id = self.services.background.spawn(command, &self.services)?;
            let first_lines = {
                // Give fast commands a moment to produce their first output.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                self.services.background.snapshot(&task_id).and_then(|s| {
                    let running = s["running"].as_bool().unwrap_or(false);
                    let output = s["output"].as_str().unwrap_or("").to_string();
                    (!running).then(|| {
                        format!(
                            "Task {task_id} already finished ({}).\nOutput:\n{output}",
                            s["status"].as_str().unwrap_or("")
                        )
                    })
                })
            };
            let text = match first_lines {
                Some(done) => done,
                None => format!(
                    "Started background task {task_id}.\nUse bash_wait with task_id=\"{task_id}\" to block until it finishes, bash_output to read its output, kill_shell to stop it. You will be notified when it finishes."
                ),
            };
            return Ok(AgentToolResult {
                content: vec![tack_ai::InputContentBlock::text(text)],
                details: serde_json::json!({ "taskId": task_id, "background": true }),
                usage: None,
                terminate: false,
                added_tool_names: None,
            });
        }

        // Executor: ACP client terminals when configured, else the local shell.
        let sandbox = self
            .services
            .sandbox
            .as_ref()
            .and_then(crate::sandbox::resolve);
        let executor = match &self.services.bash_executor {
            Some(custom) => custom.clone(),
            None => {
                let shell = self
                    .services
                    .shell
                    .clone()
                    .ok_or_else(|| "no shell configured for bash tool".to_string())?;
                std::sync::Arc::new(LocalBashExecutor {
                    shell,
                    sandbox: sandbox.clone(),
                    env: self.services.env.clone(),
                })
            }
        };

        run_shell_command(
            &executor,
            &command,
            &self.services.cwd,
            params.timeout,
            timeout,
            "tack-bash",
            cancel,
            on_update,
            sandbox.as_ref().map(|(_, spec)| spec),
        )
        .await
    }
}

/// Streaming shell-command pipeline shared by the bash and powershell tools
/// (TS `createShellToolDefinition.execute`): throttled partial updates into
/// an `OutputAccumulator`, tail truncation with temp-file spillover, and
/// abort/timeout/exit-code status suffixes. `timeout_seconds` is the
/// original `timeout` param value, used for the timed-out status message.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_shell_command(
    executor: &std::sync::Arc<dyn crate::executor::BashExecutor>,
    command: &str,
    cwd: &std::path::Path,
    timeout_seconds: Option<f64>,
    timeout: Option<std::time::Duration>,
    temp_prefix: &str,
    cancel: CancellationToken,
    on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    // Resolved sandbox policy of this execution, for the EPERM denial
    // hint appended to failed-command output (None when unsandboxed).
    sandbox_spec: Option<&crate::sandbox::SandboxSpec>,
) -> Result<AgentToolResult, String> {
    {
        // Stream output into the accumulator with throttled partial updates.
        let output =
            std::sync::Arc::new(std::sync::Mutex::new(OutputAccumulator::new(temp_prefix)));
        let last_update = std::sync::Arc::new(std::sync::Mutex::new(
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        ));
        let on_output = {
            let output = output.clone();
            let last_update = last_update.clone();
            move |data: &[u8]| {
                output
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append(data);
                let throttle_elapsed = {
                    let mut last = last_update.lock().unwrap_or_else(|e| e.into_inner());
                    if last.elapsed() >= std::time::Duration::from_millis(100) {
                        *last = std::time::Instant::now();
                        true
                    } else {
                        false
                    }
                };
                if throttle_elapsed {
                    let snapshot = output
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .snapshot(false);
                    let text = sanitize_binary_output(&snapshot.content);
                    let mut result = AgentToolResult::text(text);
                    result.details =
                        serde_json::json!({ "truncation": snapshot.truncation.truncated });
                    // Host callback runs outside every lock: this callback
                    // executes synchronously inside the executor's select
                    // loop, so a blocking one would stall the timeout and
                    // cancel arms with it.
                    on_update(result);
                }
            }
        };

        let outcome = executor
            .exec(command, cwd, timeout, cancel, &on_output)
            .await?;

        // --- final result ---
        let snapshot = output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot(true);
        let content = sanitize_binary_output(&snapshot.content);
        let truncation = &snapshot.truncation;

        let mut text = if content.is_empty() {
            "(no output)".to_string()
        } else {
            content
        };
        let mut details = serde_json::Map::new();
        if truncation.truncated {
            let start_line = truncation.total_lines - truncation.output_lines + 1;
            let end_line = truncation.total_lines;
            let full_path = snapshot
                .full_output_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let footer = if truncation.last_line_partial {
                format!(
                    "\n\n[Showing last {} of line {end_line}. Full output: {full_path}]",
                    format_size(truncation.output_bytes)
                )
            } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
                format!(
                    "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {full_path}]",
                    truncation.total_lines
                )
            } else {
                format!(
                    "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {full_path}]",
                    truncation.total_lines,
                    format_size(DEFAULT_MAX_BYTES)
                )
            };
            text.push_str(&footer);
            details.insert(
                "truncation".to_string(),
                serde_json::json!({
                    "truncatedBy": match truncation.truncated_by {
                        Some(TruncatedBy::Lines) => "lines",
                        Some(TruncatedBy::Bytes) => "bytes",
                        None => "lines",
                    },
                    "totalLines": truncation.total_lines,
                    "outputLines": truncation.output_lines,
                }),
            );
            if let Some(path) = &snapshot.full_output_path {
                details.insert(
                    "fullOutputPath".to_string(),
                    Value::String(path.display().to_string()),
                );
            }
        }

        let status_suffix = |text: &str, status: &str| {
            if text.is_empty() {
                status.to_string()
            } else {
                format!("{text}\n\n{status}")
            }
        };

        if outcome.cancelled {
            return Err(status_suffix(&text, "Command aborted"));
        }
        if outcome.timed_out {
            return Err(status_suffix(
                &text,
                &format!(
                    "Command timed out after {} seconds",
                    timeout_seconds.unwrap_or(0.0)
                ),
            ));
        }
        match outcome.exit_code {
            Some(141) => {
                // SIGPIPE death of an upstream stage (`cmd | head`): with
                // pipefail the pipeline reports 141 even though nothing is
                // wrong — but it can ALSO mask a real failure whose output
                // was cut off, so don't silently pass it either.
                return Err(status_suffix(
                    &text,
                    "Pipeline died by SIGPIPE (141): a downstream pipe closed early \
                     (often `| head` truncation — usually harmless; re-run without the \
                     pipe if you need the upstream exit status)",
                ));
            }
            Some(code) if code != 0 => {
                let mut text = text;
                if let Some(spec) = sandbox_spec
                    && let Some(hint) = crate::sandbox::denial_hint(spec, &text)
                {
                    text.push_str("\n\n");
                    text.push_str(&hint);
                }
                return Err(status_suffix(
                    &text,
                    &format!("Command exited with code {code}"),
                ));
            }
            // The shell itself died by signal (e.g. SIGKILL from the OOM
            // killer): there is no exit code. Reporting success here would
            // be a lie — the command did not run to completion. (TS pi
            // treats a null exit code as success; tack deliberately
            // diverges — silent false-success corrupts agent state.)
            None => {
                return Err(status_suffix(&text, "Command terminated by signal"));
            }
            _ => {}
        }

        Ok(AgentToolResult {
            content: vec![tack_ai::InputContentBlock::text(text)],
            details: Value::Object(details),
            usage: None,
            terminate: false,
            added_tool_names: None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn services() -> ToolServices {
        crate::default_services(std::env::current_dir().unwrap())
    }

    /// Regression: when the shell itself is killed by a signal there is no
    /// exit code; the tool must NOT report success.
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_killed_by_signal_is_an_error() {
        let services = services();
        if services.shell.is_none() {
            eprintln!("no shell available, skipping");
            return;
        }
        let tool = BashTool::new(services);
        let err = tool
            .execute(
                "1",
                serde_json::json!({ "command": "kill -9 $$" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(err.contains("terminated by signal"), "unexpected: {err}");
    }

    /// Sanity: a normal successful command still reports success.
    #[tokio::test]
    async fn successful_command_still_ok() {
        let services = services();
        if services.shell.is_none() {
            return;
        }
        let tool = BashTool::new(services);
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "command": "echo fine" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("fine"));
    }
}
