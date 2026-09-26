//! Eval / regression harness: `tack eval <dir>` runs agent tasks headlessly
//! and scores them, so behavior changes (LSP, sandbox, prompts, models) can
//! be measured instead of vibes. SWE-bench-inspired but local and light:
//!
//! ```text
//! evals/
//!   fix-typo/
//!     task.json   { "prompt": "fix the typo in a.txt",
//!                   "setup": "printf 'teh' > a.txt",
//!                   "verify": "grep -q the a.txt", "timeoutSecs": 300 }
//! ```
//!
//! Each task: run `setup` (optional) → run the agent with `prompt` (fresh
//! in-memory context, full coding tools) → run `verify` (exit 0 = pass).
//! `--runs N` repeats tasks for a pass rate; `--baseline report.json` diffs
//! pass rates against a previous run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tack_agent_core::{
    AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentMessage, ToolExecutionMode,
    agent_loop,
};
use tack_ai::Provider;

/// One eval task (task.json).
#[derive(Clone, Debug, Deserialize)]
pub struct EvalTask {
    /// Display name (defaults to the directory name).
    pub name: Option<String>,
    pub prompt: String,
    /// Shell command preparing the fixture (runs in a throwaway copy of
    /// the task dir — the on-disk fixtures stay pristine).
    pub setup: Option<String>,
    /// Shell command scoring the run: exit 0 = pass.
    pub verify: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    300
}

/// Bounded stderr retention for run_shell (tail kept for error context).
const STDERR_TAIL_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvalRun {
    pub pass: bool,
    pub duration_secs: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
    pub tool_calls: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Tail of the agent's final message, captured on failure only — the
    /// difference between "verify failed" and knowing WHY (permission
    /// denial, wrong tool, gave up). Truncated to keep reports small.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvalTaskResult {
    pub name: String,
    pub runs: Vec<EvalRun>,
    pub pass_rate: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvalReport {
    pub model: String,
    pub started_unix_ms: u64,
    pub tasks: Vec<EvalTaskResult>,
    pub total_pass_rate: f64,
    pub total_cost: f64,
}

/// Load all task dirs (any subdirectory containing task.json), sorted.
pub fn load_tasks(dir: &Path, filter: Option<&str>) -> Vec<(String, PathBuf, EvalTask)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let task_dir = entry.path();
        let task_file = task_dir.join("task.json");
        if !task_file.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&task_file) else {
            continue;
        };
        let Ok(task) = serde_json::from_str::<EvalTask>(&content) else {
            tracing::warn!("skipping malformed {}", task_file.display());
            continue;
        };
        let name = task
            .name
            .clone()
            .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
        if let Some(filter) = filter
            && !name.contains(filter)
        {
            continue;
        }
        out.push((name, task_dir, task));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Run a shell command in `cwd`; Ok(exit_code).
async fn run_shell(
    shell: &tack_tools::shell::ShellConfig,
    command: &str,
    cwd: &Path,
    timeout: std::time::Duration,
) -> Result<i32, String> {
    use tokio::io::AsyncReadExt as _;
    let mut cmd = tokio::process::Command::new(&shell.shell);
    cmd.args(&shell.args)
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let pid = child.id().unwrap_or(0);
    // Both pipes must be drained: a child that fills the stdout pipe buffer
    // (>64 KiB) blocks on write and would otherwise hang until the timeout.
    let mut stdout = child.stdout.take().expect("stdout piped");
    tokio::spawn(async move {
        let mut sink = tokio::io::sink();
        let _ = tokio::io::copy(&mut stdout, &mut sink).await;
    });
    // stderr is diagnostic context, not payload: drain it too (same
    // backpressure reason) but keep only a bounded TAIL for error messages
    // — an unbounded read_to_string lets a spammy command grow memory
    // without limit, and the collected string was never even received.
    let mut stderr = child.stderr.take().expect("stderr piped");
    let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    {
        let stderr_tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut tail = stderr_tail.lock().unwrap_or_else(|e| e.into_inner());
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > STDERR_TAIL_BYTES {
                            let excess = tail.len() - STDERR_TAIL_BYTES;
                            tail.drain(..excess);
                        }
                    }
                }
            }
        });
    }
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => Ok(status.code().unwrap_or(-1)),
        Ok(Err(e)) => Err(format!("wait failed: {e}")),
        Err(_) => {
            tack_tools::shell::kill_process_tree(pid);
            let mut message = format!("command timed out ({}s)", timeout.as_secs());
            let tail = std::mem::take(&mut *stderr_tail.lock().unwrap_or_else(|e| e.into_inner()));
            let tail = String::from_utf8_lossy(&tail);
            let tail = tail.trim();
            if !tail.is_empty() {
                // Keep the message short: last ~2 KiB of the tail.
                let keep = tail.len().saturating_sub(2_048);
                let start = tail
                    .char_indices()
                    .find_map(|(i, _)| (i >= keep).then_some(i))
                    .unwrap_or(0);
                message.push_str(&format!("; stderr tail: {}", &tail[start..]));
            }
            Err(message)
        }
    }
}

/// A no-op hooks impl (headless, no UI, no compaction).
#[derive(Debug)]
struct NoopHooks;

#[async_trait::async_trait]
impl AgentHooks for NoopHooks {}

/// Run the agent once with a fresh in-memory context. Returns
/// (tool_calls, usage, final_text, error, tool_errors).
async fn run_agent_once(
    prompt: &str,
    cwd: &Path,
    model: &tack_ai::Model,
    provider: Arc<dyn Provider>,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    settings: &crate::settings::Settings,
    timeout: std::time::Duration,
) -> (usize, tack_ai::Usage, String, Option<String>, Vec<String>) {
    let mut services = tack_tools::default_services(cwd.to_path_buf())
        .with_lsp(settings.lsp_manager(cwd))
        .with_web_render(settings.web_render_mode())
        .with_web_search(settings.web_search_config())
        .with_memory_dir(settings.memory_directory.clone());
    if let Some(spec) = settings.sandbox_spec(cwd) {
        services = services.with_sandbox(spec);
    }
    let tools = tack_tools::create_coding_tools(&services);
    let config = AgentLoopConfig {
        model: model.clone(),
        provider,
        hooks: Arc::new(NoopHooks),
        tool_execution: ToolExecutionMode::Parallel,
        reasoning: None,
        auth,
        max_tokens: None,
        temperature: None,
        session_id: None,
        cache_retention: settings.cache_retention_mode(),
        fallback_models: crate::model::resolve_fallback_models(
            &settings.fallback_models,
            model,
            &tack_session::default_agent_dir(),
        ),
        tool_pool: Vec::new(),
        retry_cancel: None,
    };
    let context = AgentContext {
        system_prompt: None,
        messages: Vec::new(),
        tools,
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let stream = agent_loop(
        vec![AgentMessage::user(prompt.to_string())],
        context,
        config,
        cancel.clone(),
    );

    let mut stream = std::pin::pin!(stream);
    let mut tool_calls = 0usize;
    let mut usage = tack_ai::Usage::zero();
    let mut final_text = String::new();
    let mut tool_errors: Vec<String> = Vec::new();
    let mut error = None;
    let collect = async {
        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::ToolExecutionStart { .. } => tool_calls += 1,
                AgentEvent::ToolExecutionEnd {
                    tool_name,
                    result,
                    is_error: true,
                    ..
                } => {
                    // Keep the first few tool errors: on a failed run they
                    // are usually THE explanation (permission denied,
                    // sandbox refusal, missing binary).
                    if tool_errors.len() < 3 {
                        let text = result
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                tack_ai::InputContentBlock::Text { text, .. } => {
                                    Some(text.as_str())
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        let short: String = text.chars().take(200).collect();
                        tool_errors.push(format!("{tool_name}: {short}"));
                    }
                }
                AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(a),
                } => {
                    usage.input += a.usage.input;
                    usage.output += a.usage.output;
                    usage.cache_read += a.usage.cache_read;
                    usage.cache_write += a.usage.cache_write;
                    usage.total_tokens += a.usage.total_tokens;
                    usage.cost.total += a.usage.cost.total;
                    if a.stop_reason == tack_ai::StopReason::Error {
                        error = a.error_message.clone();
                    }
                    let text = a.text();
                    if !text.trim().is_empty() {
                        final_text = text;
                    }
                }
                AgentEvent::MessageEnd { .. } => {}
                _ => {}
            }
        }
    };
    if tokio::time::timeout(timeout, collect).await.is_err() {
        cancel.cancel();
        error = Some(format!("agent run timed out ({}s)", timeout.as_secs()));
    }
    (tool_calls, usage, final_text, error, tool_errors)
}

/// Sandbox nesting probe: apply the resolved sandbox to `true`. Fails
/// when the outer environment is itself sandboxed (seatbelt cannot nest,
/// bwrap may lack user namespaces inside containers).
async fn sandbox_usable(
    settings: &crate::settings::Settings,
    shell: &tack_tools::shell::ShellConfig,
    cwd: &Path,
) -> bool {
    let Some(spec) = settings.sandbox_spec(cwd) else {
        return true; // sandbox disabled in settings: nothing to probe
    };
    let Some((backend, spec)) = tack_tools::sandbox::resolve(&spec) else {
        return true; // no backend available: commands run unsandboxed anyway
    };
    let (program, args) =
        tack_tools::sandbox::plan(Some(&backend), Some(&spec), shell, "true", cwd);
    tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Copy a task dir's contents into a fresh work dir (fixtures are small;
/// a full copy keeps every run hermetic and leaves the on-disk task dir
/// byte-identical after the eval).
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir_all(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
        // Symlinks are not expected in fixtures; skip rather than follow.
    }
    Ok(())
}

/// Run one task `runs` times.
#[allow(clippy::too_many_arguments)]
pub async fn run_task(
    name: &str,
    task_dir: &Path,
    task: &EvalTask,
    runs: usize,
    model: &tack_ai::Model,
    provider: Arc<dyn Provider>,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    settings: &crate::settings::Settings,
    shell: &tack_tools::shell::ShellConfig,
) -> EvalTaskResult {
    let mut results = Vec::new();
    for _ in 0..runs.max(1) {
        let started = Instant::now();
        let timeout = std::time::Duration::from_secs(task.timeout_secs);
        let mut error = None;
        let mut usage = tack_ai::Usage::zero();
        let mut tool_calls = 0;

        // Hermetic workdir per run: setup, agent and verify all execute in
        // a throwaway copy of the task dir. Running in-place would pollute
        // the checked-out fixtures (stale files skew the NEXT run's setup,
        // and `git status` fills up with generated files).
        let workdir = match tempfile::tempdir()
            .map_err(|e| e.to_string())
            .and_then(|w| {
                copy_dir_all(task_dir, w.path()).map_err(|e| e.to_string())?;
                Ok(w)
            }) {
            Ok(w) => Some(w),
            Err(e) => {
                error = Some(format!("workdir setup failed: {e}"));
                None
            }
        };
        let cwd = workdir.as_ref().map_or(task_dir, |w| w.path());

        // setup
        if error.is_none()
            && let Some(setup) = &task.setup
            && let Err(e) = run_shell(shell, setup, cwd, timeout).await
        {
            error = Some(format!("setup failed: {e}"));
        }

        // agent
        let mut run_agent_text = String::new();
        let mut run_tool_errors: Vec<String> = Vec::new();
        let pass = if error.is_none() {
            let (calls, u, text, agent_error, tool_errors) = run_agent_once(
                &task.prompt,
                cwd,
                model,
                provider.clone(),
                auth.clone(),
                settings,
                timeout,
            )
            .await;
            tool_calls = calls;
            usage = u;
            run_agent_text = text;
            run_tool_errors = tool_errors;
            if let Some(e) = agent_error {
                error = Some(e);
                false
            } else {
                // verify
                match run_shell(shell, &task.verify, cwd, timeout).await {
                    Ok(0) => true,
                    Ok(code) => {
                        error = Some(format!("verify exited {code}"));
                        false
                    }
                    Err(e) => {
                        error = Some(e);
                        false
                    }
                }
            }
        } else {
            false
        };

        // On failure, keep the tool errors and the tail of the agent's
        // final message so the report shows what happened (permission
        // denial, missing binary, gave up) instead of a bare exit code.
        let agent_note = if pass {
            None
        } else {
            let mut note = String::new();
            if !run_tool_errors.is_empty() {
                note.push_str(&run_tool_errors.join(" | "));
            }
            let text = run_agent_text.trim();
            if !text.is_empty() {
                if !note.is_empty() {
                    note.push_str(" ;; ");
                }
                note.push_str(
                    &text
                        .chars()
                        .skip(text.chars().count().saturating_sub(500))
                        .collect::<String>(),
                );
            }
            (!note.is_empty()).then_some(note)
        };

        results.push(EvalRun {
            pass,
            duration_secs: started.elapsed().as_secs_f64(),
            input_tokens: usage.input,
            output_tokens: usage.output,
            cost: usage.cost.total,
            tool_calls,
            error,
            agent_note,
        });
    }
    let pass_rate = results.iter().filter(|r| r.pass).count() as f64 / results.len() as f64;
    EvalTaskResult {
        name: name.to_string(),
        runs: results,
        pass_rate,
    }
}

/// Run the whole eval dir and build the report.
pub async fn run_eval(
    dir: &Path,
    runs: usize,
    filter: Option<&str>,
    model: &tack_ai::Model,
    provider: Arc<dyn Provider>,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    settings: &crate::settings::Settings,
) -> Result<EvalReport, String> {
    let tasks = load_tasks(dir, filter);
    if tasks.is_empty() {
        return Err(format!(
            "no eval tasks (task.json) found under {}",
            dir.display()
        ));
    }
    let shell = tack_tools::shell::resolve_shell(None)
        .map_err(|e| format!("no shell for setup/verify commands: {e}"))?;

    // One-time sandbox nesting probe: seatbelt refuses to apply a profile
    // from inside an already-sandboxed process (sandbox_apply: EPERM) —
    // e.g. when the eval itself runs inside another agent or a sandboxed
    // CI job. Without this, every sandboxed tool call fails opaquely and
    // bash-needing tasks score 0 for environmental reasons.
    let mut unsandboxed;
    let settings = if sandbox_usable(settings, &shell, dir).await {
        settings
    } else {
        eprintln!(
            "eval: sandbox probe failed (running inside another sandbox?) — \
             agent tool calls will run WITHOUT the OS sandbox this time"
        );
        unsandboxed = settings.clone();
        unsandboxed.sandbox = false;
        &unsandboxed
    };

    let mut results = Vec::new();
    for (name, task_dir, task) in &tasks {
        eprintln!("eval: running {name} ({} run(s))…", runs.max(1));
        let result = run_task(
            name,
            task_dir,
            task,
            runs,
            model,
            provider.clone(),
            auth.clone(),
            settings,
            &shell,
        )
        .await;
        eprintln!("eval: {name} → pass rate {:.0}%", result.pass_rate * 100.0);
        results.push(result);
    }

    let total_runs: usize = results.iter().map(|t| t.runs.len()).sum();
    let total_pass: usize = results
        .iter()
        .flat_map(|t| &t.runs)
        .filter(|r| r.pass)
        .count();
    let total_cost: f64 = results.iter().flat_map(|t| &t.runs).map(|r| r.cost).sum();
    Ok(EvalReport {
        model: format!("{}/{}", model.provider, model.id),
        started_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        tasks: results,
        total_pass_rate: if total_runs == 0 {
            0.0
        } else {
            total_pass as f64 / total_runs as f64
        },
        total_cost,
    })
}

/// Diff pass rates against a previous report (matched by task name).
pub fn diff_baseline(current: &EvalReport, baseline: &EvalReport) -> String {
    let mut out = String::from("Baseline comparison (current vs baseline):\n");
    for task in &current.tasks {
        let base = baseline.tasks.iter().find(|t| t.name == task.name);
        match base {
            Some(base) => {
                let delta = (task.pass_rate - base.pass_rate) * 100.0;
                let marker = if delta > 1.0 {
                    "▲"
                } else if delta < -1.0 {
                    "▼"
                } else {
                    "="
                };
                out.push_str(&format!(
                    "  {marker} {}: {:.0}% → {:.0}% ({delta:+.0}%)\n",
                    task.name,
                    base.pass_rate * 100.0,
                    task.pass_rate * 100.0
                ));
            }
            None => {
                out.push_str(&format!(
                    "  + {}: new task ({:.0}%)\n",
                    task.name,
                    task.pass_rate * 100.0
                ));
            }
        }
    }
    for base in &baseline.tasks {
        if !current.tasks.iter().any(|t| t.name == base.name) {
            out.push_str(&format!(
                "  - {}: removed (was {:.0}%)\n",
                base.name,
                base.pass_rate * 100.0
            ));
        }
    }
    out
}

/// Serialize a report (pretty JSON).
pub fn report_json(report: &EvalReport) -> String {
    serde_json::to_string_pretty(&json!(report)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tack_ai::provider::StreamOptions;
    use tack_ai::stream::AssistantMessageEventStream;
    use tack_ai::types::Context;

    /// Provider that returns a fixed "done" answer without tool calls.
    #[derive(Debug)]
    struct ScriptedDoneProvider;

    impl Provider for ScriptedDoneProvider {
        fn stream(
            &self,
            model: &tack_ai::Model,
            _context: &Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let (sender, stream) = tack_ai::event_stream();
            let mut message = tack_ai::AssistantMessage::pending(model);
            message.stop_reason = tack_ai::StopReason::Stop;
            message.content = vec![tack_ai::ContentBlock::text("done")];
            message.usage.input = 10;
            message.usage.output = 5;
            message.usage.total_tokens = 15;
            sender.push(tack_ai::AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            sender.finish(tack_ai::AssistantMessageEvent::Done {
                reason: tack_ai::StopReason::Stop,
                message,
            });
            stream
        }
    }

    fn write_task(dir: &Path, verify: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("task.json"),
            serde_json::json!({
                "prompt": "do the thing",
                "verify": verify,
                "timeoutSecs": 30,
            })
            .to_string(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn eval_scores_pass_and_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let evals = tmp.path().join("evals");
        write_task(&evals.join("passing"), "exit 0");
        write_task(&evals.join("failing"), "exit 1");

        let model = crate::model::resolve_model("anthropic", Some("k3"), tmp.path()).unwrap();
        let settings = crate::settings::Settings::default();
        let report = run_eval(
            &evals,
            1,
            None,
            &model,
            Arc::new(ScriptedDoneProvider),
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
            &settings,
        )
        .await
        .unwrap();

        assert_eq!(report.tasks.len(), 2);
        let passing = report.tasks.iter().find(|t| t.name == "passing").unwrap();
        let failing = report.tasks.iter().find(|t| t.name == "failing").unwrap();
        assert_eq!(passing.pass_rate, 1.0);
        assert_eq!(failing.pass_rate, 0.0);
        assert!(failing.runs[0].error.as_deref().unwrap().contains("verify"));
        assert_eq!(report.total_pass_rate, 0.5);
        assert_eq!(passing.runs[0].input_tokens, 10);
    }

    #[tokio::test]
    async fn eval_leaves_task_dir_pristine() {
        // Regression: the harness used to run setup/agent/verify in the
        // task dir itself, leaving generated fixtures behind (and skewing
        // the next run's setup). Runs must be hermetic.
        let tmp = tempfile::tempdir().unwrap();
        let evals = tmp.path().join("evals");
        let task_dir = evals.join("polluting");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(
            task_dir.join("task.json"),
            serde_json::json!({
                "prompt": "do the thing",
                "setup": "echo data > fixture.txt",
                // passes only if setup ran (in whatever cwd it gets)
                "verify": "test -f fixture.txt",
                "timeoutSecs": 30,
            })
            .to_string(),
        )
        .unwrap();

        let model = crate::model::resolve_model("anthropic", Some("k3"), tmp.path()).unwrap();
        let settings = crate::settings::Settings::default();
        let report = run_eval(
            &evals,
            1,
            None,
            &model,
            Arc::new(ScriptedDoneProvider),
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
            &settings,
        )
        .await
        .unwrap();

        let task = report.tasks.iter().find(|t| t.name == "polluting").unwrap();
        assert_eq!(task.pass_rate, 1.0, "setup+verify must run (in the copy)");
        let on_disk: Vec<String> = std::fs::read_dir(&task_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(on_disk, ["task.json"], "task dir must stay pristine");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_drains_large_stdout() {
        // > pipe buffer (64 KiB) of stdout: must not deadlock the child.
        let shell = tack_tools::shell::resolve_shell(None).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let code = run_shell(
            &shell,
            "head -c 300000 /dev/zero",
            tmp.path(),
            std::time::Duration::from_secs(15),
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "large stdout must be drained, not awaited to timeout"
        );
    }

    #[test]
    fn baseline_diff_marks_regressions() {
        let mk = |name: &str, rate: f64| EvalTaskResult {
            name: name.to_string(),
            runs: vec![],
            pass_rate: rate,
        };
        let current = EvalReport {
            model: "m".into(),
            started_unix_ms: 0,
            tasks: vec![mk("a", 0.5), mk("b", 1.0)],
            total_pass_rate: 0.75,
            total_cost: 0.0,
        };
        let baseline = EvalReport {
            model: "m".into(),
            started_unix_ms: 0,
            tasks: vec![mk("a", 1.0), mk("c", 0.5)],
            total_pass_rate: 0.75,
            total_cost: 0.0,
        };
        let diff = diff_baseline(&current, &baseline);
        assert!(diff.contains("▼ a: 100% → 50%"), "{diff}");
        assert!(diff.contains("+ b: new task"), "{diff}");
        assert!(diff.contains("- c: removed"), "{diff}");
    }
}
