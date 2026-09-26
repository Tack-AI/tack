//! Background bash tasks. Port of the Claude-Code-style background task
//! pattern: `bash` with `run_in_background` returns immediately with a task
//! id; `bash_output` polls/reads the (still growing) output; `bash_wait`
//! blocks until a task finishes; `kill_shell` terminates the task's process
//! tree. Completion can optionally push a notification through a channel
//! the host app drains into the conversation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::accumulator::OutputAccumulator;
use crate::executor::BashExecutor;
use crate::executor::LocalBashExecutor;
use crate::services::ToolServices;
use crate::shell::sanitize_binary_output;

/// Terminal state of a background task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Exited(i32),
    Killed,
    /// Spawn or wait failed; message kept in `BackgroundTask::error`.
    Failed,
}

impl TaskStatus {
    pub fn describe(self) -> String {
        match self {
            TaskStatus::Running => "running".to_string(),
            TaskStatus::Exited(code) => format!("exited with code {code}"),
            TaskStatus::Killed => "killed".to_string(),
            TaskStatus::Failed => "failed".to_string(),
        }
    }
}

struct BackgroundTask {
    command: String,
    started: Instant,
    status: TaskStatus,
    error: Option<String>,
    output: Arc<Mutex<OutputAccumulator>>,
    cancel: CancellationToken,
}

/// Notification emitted when a task reaches a terminal state.
#[derive(Clone, Debug)]
pub struct TaskNotification {
    pub task_id: String,
    pub command: String,
    pub status: String,
}

struct ManagerInner {
    tasks: HashMap<String, BackgroundTask>,
    next_id: u64,
    notify: Option<tokio::sync::mpsc::UnboundedSender<TaskNotification>>,
    /// Generation counter bumped on every terminal transition. `bash_wait`
    /// subscribes and awaits `changed()` — unlike a one-shot `Notify`, a
    /// watch receiver can't miss a wakeup that lands between the status
    /// check and the await.
    completion_tx: tokio::sync::watch::Sender<u64>,
}

/// Shared registry of background tasks. Cheap to clone (Arc inside); one
/// instance should live for the whole app session so tasks survive across
/// agent runs.
#[derive(Clone, Default)]
pub struct BackgroundTaskManager {
    inner: Arc<Mutex<ManagerInner>>,
}

impl std::fmt::Debug for BackgroundTaskManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.inner.lock().map(|i| i.tasks.len()).unwrap_or(0);
        f.debug_struct("BackgroundTaskManager")
            .field("tasks", &count)
            .finish()
    }
}

impl Default for ManagerInner {
    fn default() -> Self {
        ManagerInner {
            tasks: HashMap::new(),
            next_id: 1,
            notify: None,
            completion_tx: tokio::sync::watch::channel(0).0,
        }
    }
}

/// Handle to a registered background task: finish it (with notification)
/// when the underlying work completes.
pub struct TaskHandle {
    pub id: String,
    pub output: Arc<Mutex<OutputAccumulator>>,
    pub cancel: CancellationToken,
    manager: BackgroundTaskManager,
    finished: bool,
}

impl std::fmt::Debug for TaskHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskHandle").field("id", &self.id).finish()
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        // A handle dropped without finish() (driver bug, panicked or
        // aborted spawner) must not leave a phantom "running" task in the
        // registry forever.
        if !self.finished {
            self.finish_inner(
                TaskStatus::Failed,
                Some("task handle dropped without completion".to_string()),
            );
        }
    }
}

impl TaskHandle {
    /// Append a progress line to the task's output buffer.
    pub fn log(&self, line: &str) {
        let mut out = self.output.lock().unwrap_or_else(|e| e.into_inner());
        out.append(line.as_bytes());
        out.append(b"\n");
    }

    /// Mark the task finished and emit the completion notification.
    pub fn finish(mut self, status: TaskStatus, error: Option<String>) {
        self.finished = true;
        self.finish_inner(status, error);
    }

    fn finish_inner(&mut self, status: TaskStatus, error: Option<String>) {
        let (command, notify) = {
            let mut inner = self.manager.inner.lock().unwrap_or_else(|e| e.into_inner());
            match inner.tasks.get_mut(&self.id) {
                Some(task) => {
                    // Never resurrect a task that already reached a
                    // terminal state.
                    if task.status == TaskStatus::Running {
                        task.status = status;
                        task.error = error.clone();
                    }
                    let command = task.command.clone();
                    // Wake bash_wait waiters (same lock, so a waiter can
                    // never observe the finished status with a stale
                    // generation).
                    inner.completion_tx.send_modify(|seq| *seq += 1);
                    (command, inner.notify.clone())
                }
                None => (String::new(), None),
            }
        };
        if let Some(tx) = notify {
            let mut status_text = status.describe();
            if let Some(error) = error {
                status_text = format!("{status_text}: {error}");
            }
            let _ = tx.send(TaskNotification {
                task_id: self.id.clone(),
                command,
                status: status_text,
            });
        }
    }
}

/// Result of a kill request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillOutcome {
    /// No task with that id.
    Unknown,
    /// Task was running; the kill signal was sent.
    Signalled,
    /// Task had already reached a terminal state; nothing was killed.
    AlreadyFinished(TaskStatus),
}

impl KillOutcome {
    pub fn unknown(self) -> bool {
        matches!(self, KillOutcome::Unknown)
    }
    pub fn signalled(self) -> bool {
        matches!(self, KillOutcome::Signalled)
    }
    pub fn already_finished(self) -> bool {
        matches!(self, KillOutcome::AlreadyFinished(_))
    }
}

impl BackgroundTaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the completion-notification channel (host app drains it).
    pub fn set_notify(&self, tx: tokio::sync::mpsc::UnboundedSender<TaskNotification>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).notify = Some(tx);
    }

    /// Register a task without a shell command (e.g. a background
    /// sub-agent). The caller drives progress via the returned handle.
    pub fn register(&self, label: String) -> TaskHandle {
        let cancel = CancellationToken::new();
        let output = Arc::new(Mutex::new(OutputAccumulator::new("tack-task")));
        // Evicted tasks' output handles, cleaned up AFTER the registry lock
        // is released: cleanup takes the task's `output` lock, and `inner`
        // must never be held while an `output` lock is taken (lock order).
        let mut evicted: Vec<Arc<Mutex<OutputAccumulator>>> = Vec::new();
        let id = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // The registry is session-scoped and otherwise only grows:
            // evict the oldest FINISHED tasks past a cap so a long session
            // of completed tasks doesn't accumulate unboundedly. Running
            // tasks are never evicted.
            const MAX_FINISHED_TASKS: usize = 100;
            let finished = inner
                .tasks
                .values()
                .filter(|t| t.status != TaskStatus::Running)
                .count();
            if finished >= MAX_FINISHED_TASKS {
                let mut finished_ids: Vec<(String, std::time::Instant)> = inner
                    .tasks
                    .iter()
                    .filter(|(_, t)| t.status != TaskStatus::Running)
                    .map(|(id, t)| (id.clone(), t.started))
                    .collect();
                finished_ids.sort_by_key(|(_, started)| *started);
                let evict = finished - MAX_FINISHED_TASKS + 1;
                for (id, _) in finished_ids.into_iter().take(evict) {
                    // Evicted tasks can no longer be snapshotted, so the
                    // persisted full-output file is unreachable: queue it
                    // for removal once `inner` is released.
                    if let Some(task) = inner.tasks.remove(&id) {
                        evicted.push(task.output);
                    }
                }
            }
            let id = format!("bg{}", inner.next_id);
            inner.next_id += 1;
            inner.tasks.insert(
                id.clone(),
                BackgroundTask {
                    command: label.clone(),
                    started: Instant::now(),
                    status: TaskStatus::Running,
                    error: None,
                    output: output.clone(),
                    cancel: cancel.clone(),
                },
            );
            id
        };
        for output in evicted {
            output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cleanup_persisted();
        }
        TaskHandle {
            id,
            output,
            cancel,
            manager: self.clone(),
            finished: false,
        }
    }

    /// Spawn a command as a background task. Returns the task id.
    pub fn spawn(&self, command: String, services: &ToolServices) -> Result<String, String> {
        let shell = services
            .shell
            .clone()
            .ok_or_else(|| "no shell configured for background task".to_string())?;
        let handle = self.register(command.clone());
        let id = handle.id.clone();
        let cancel = handle.cancel.clone();
        let output = handle.output.clone();

        let cwd = services.cwd.clone();
        let sandbox = services.sandbox.as_ref().and_then(crate::sandbox::resolve);
        let env = services.env.clone();
        tokio::spawn(async move {
            let executor = LocalBashExecutor {
                shell,
                sandbox: sandbox.clone(),
                env,
            };
            let on_output = {
                let output = output.clone();
                move |data: &[u8]| {
                    output
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .append(data);
                }
            };
            let result = executor
                .exec(&command, &cwd, None, cancel, &on_output)
                .await;

            let (status, error) = match result {
                Ok(outcome) if outcome.cancelled => (TaskStatus::Killed, None),
                Ok(outcome) => match outcome.exit_code {
                    Some(code) => {
                        // A failed sandboxed command whose output smells
                        // like EPERM gets the policy note appended (see
                        // bash's foreground path) — background output is
                        // read just the same.
                        append_sandbox_denial_hint(&sandbox, &output, code);
                        (TaskStatus::Exited(code), None)
                    }
                    // The shell died by signal without a cancel from us
                    // (e.g. OOM kill) — there is no exit code to report.
                    None => (TaskStatus::Killed, Some("terminated by signal".to_string())),
                },
                Err(e) => (TaskStatus::Failed, Some(e)),
            };
            handle.finish(status, error);
        });

        Ok(id)
    }

    /// Kill a task's process tree.
    pub fn kill(&self, task_id: &str) -> KillOutcome {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.tasks.get(task_id) {
            Some(task) if task.status == TaskStatus::Running => {
                task.cancel.cancel();
                KillOutcome::Signalled
            }
            Some(task) => KillOutcome::AlreadyFinished(task.status),
            None => KillOutcome::Unknown,
        }
    }

    /// Snapshot of one task: status, elapsed seconds, tail-truncated output.
    pub fn snapshot(&self, task_id: &str) -> Option<Value> {
        // Lock order: `inner` is never held while a task's `output` lock is
        // taken (the pump's completion path takes `output` on its own), so
        // no AB-BA cycle between the registry and an accumulator can form.
        // Clone the shared handle + copy the cheap fields under `inner`,
        // release it, then read the output. Status and output may race a
        // concurrent finish by a few nanoseconds — harmless for a
        // diagnostic snapshot; wait() re-checks via the completion watch.
        let (command, task_status, error, started, output) = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let task = inner.tasks.get(task_id)?;
            (
                task.command.clone(),
                task.status,
                task.error.clone(),
                task.started,
                task.output.clone(),
            )
        };
        let snapshot = output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot(true);
        let output = sanitize_binary_output(&snapshot.content);
        let mut status = task_status.describe();
        if let Some(error) = &error {
            status = format!("{status}: {error}");
        }
        Some(json!({
            "taskId": task_id,
            "command": command,
            "status": status,
            "running": task_status == TaskStatus::Running,
            "elapsedSeconds": started.elapsed().as_secs(),
            "output": output,
            "truncated": snapshot.truncation.truncated,
            "fullOutputPath": snapshot.full_output_path.map(|p| p.display().to_string()),
        }))
    }

    /// Wait for a task to reach a terminal state, or for `timeout` to
    /// elapse. Returns the task snapshot (`running: true` on timeout), or
    /// None for an unknown task id. Cancellation is the caller's job:
    /// dropping this future has no side effects.
    pub async fn wait(&self, task_id: &str, timeout: Duration) -> Option<Value> {
        let mut rx = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completion_tx
            .subscribe();
        // Unknown id: no point waiting (the watch would never fire for it).
        self.snapshot(task_id)?;
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.snapshot(task_id)?;
            if snap["running"].as_bool() == Some(false) {
                return Some(snap);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, rx.changed()).await.is_err() {
                // Timed out (the sender outlives the manager, so `changed`
                // only errors via the elapsed timeout).
                return self.snapshot(task_id);
            }
        }
    }

    /// List all tasks (running first), without output bodies.
    pub fn list(&self) -> Value {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut tasks: Vec<Value> = inner
            .tasks
            .iter()
            .map(|(id, t)| {
                json!({
                    "taskId": id,
                    "command": t.command,
                    "status": t.status.describe(),
                    "running": t.status == TaskStatus::Running,
                    "elapsedSeconds": t.started.elapsed().as_secs(),
                })
            })
            .collect();
        tasks.sort_by_key(|t| t["running"].as_bool().map(|r| !r));
        json!(tasks)
    }

    /// Drop finished tasks (keep currently running ones).
    pub fn prune_finished(&self) {
        // Detach the finished tasks' output handles under `inner`, then do
        // the actual cleanup after releasing it: cleanup takes the `output`
        // lock, which must never nest inside `inner` (lock order).
        let mut pruned: Vec<Arc<Mutex<OutputAccumulator>>> = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.tasks.retain(|_, t| {
                let keep = t.status == TaskStatus::Running;
                if !keep {
                    pruned.push(t.output.clone());
                }
                keep
            });
        }
        for output in pruned {
            // Pruned tasks can no longer be snapshotted: remove the
            // persisted full-output file along with the task.
            output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cleanup_persisted();
        }
    }
}

/// Append the sandbox-denial policy note to a failed task's output when
/// the output smells like an EPERM the sandbox policy caused.
///
/// The snapshot guard is a statement temporary on purpose: as a let-chain
/// scrutinee it lives through the whole `if` body, so an append inside
/// that body would re-lock the same non-reentrant mutex — a same-thread
/// self-deadlock that wedges the pump task forever (observed in
/// production: the task stuck "running" and every snapshot/bash_wait on
/// it blocked another worker thread).
fn append_sandbox_denial_hint(
    sandbox: &Option<(crate::sandbox::SandboxBackend, crate::sandbox::SandboxSpec)>,
    output: &Mutex<OutputAccumulator>,
    exit_code: i32,
) {
    if exit_code == 0 {
        return;
    }
    let Some((_, spec)) = sandbox else { return };
    let content = output
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .snapshot(true)
        .content;
    if let Some(hint) = crate::sandbox::denial_hint(spec, &content) {
        output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append(format!("\n\n{hint}").as_bytes());
    }
}

// ---------------------------------------------------------------------
// bash_output tool
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BashOutputParams {
    /// Task id returned by bash with run_in_background (e.g. "bg1"). Omit to list all tasks.
    task_id: Option<String>,
}

pub struct BashOutputTool {
    services: ToolServices,
}

impl BashOutputTool {
    pub fn new(services: ToolServices) -> Self {
        BashOutputTool { services }
    }
}

impl std::fmt::Debug for BashOutputTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashOutputTool").finish()
    }
}

#[async_trait]
impl AgentTool for BashOutputTool {
    fn name(&self) -> &'static str {
        "bash_output"
    }
    fn label(&self) -> &str {
        "bash_output"
    }
    fn description(&self) -> &str {
        "Read output from a background task started with bash run_in_background. \
         Pass the task_id to get its status and output (tail-truncated), or omit \
         task_id to list all background tasks."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<BashOutputParams>()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: BashOutputParams = serde_json::from_value(params)
            .map_err(|e| format!("invalid bash_output params: {e}"))?;
        let manager = &self.services.background;

        match params.task_id {
            None => {
                let list = manager.list();
                Ok(AgentToolResult::text(format!(
                    "Background tasks:\n{}",
                    serde_json::to_string_pretty(&list).unwrap_or_default()
                )))
            }
            Some(id) => match manager.snapshot(&id) {
                Some(snapshot) => Ok(AgentToolResult::text(
                    serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
                )),
                None => Err(format!(
                    "Unknown background task: {id}. Omit task_id to list all tasks."
                )),
            },
        }
    }
}

// ---------------------------------------------------------------------
// bash_wait tool
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BashWaitParams {
    /// Task id returned by bash with run_in_background (e.g. "bg1")
    task_id: String,
    /// Max seconds to wait before giving up (default 600, max 3600). The
    /// call returns the moment the task finishes, whichever comes first.
    timeout: Option<u64>,
}

pub struct BashWaitTool {
    services: ToolServices,
}

impl BashWaitTool {
    pub fn new(services: ToolServices) -> Self {
        BashWaitTool { services }
    }
}

impl std::fmt::Debug for BashWaitTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashWaitTool").finish()
    }
}

#[async_trait]
impl AgentTool for BashWaitTool {
    fn name(&self) -> &'static str {
        "bash_wait"
    }
    fn label(&self) -> &str {
        "bash_wait"
    }
    fn description(&self) -> &str {
        "Block until a background task started with bash run_in_background \
         finishes, then return its status and output. Returns immediately \
         if the task already finished; gives up after `timeout` seconds \
         (default 600). Use this instead of a foreground `sleep` or repeated \
         bash_output polling when the next step depends on the task — a \
         foreground sleep cannot be interrupted by the task-completion \
         notification, bash_wait can."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<BashWaitParams>()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: BashWaitParams =
            serde_json::from_value(params).map_err(|e| format!("invalid bash_wait params: {e}"))?;
        let manager = &self.services.background;

        if manager.snapshot(&params.task_id).is_none() {
            return Err(format!(
                "Unknown background task: {}. Use bash_output without task_id to list all tasks.",
                params.task_id
            ));
        }
        let timeout = Duration::from_secs(params.timeout.unwrap_or(600).clamp(1, 3600));

        // Distinguish WHY the wait returned: the message must name the
        // real cause and the actual wait time — a wait cut short by an
        // abort reported as "still running after {timeout}s" once sent an
        // agent re-waiting in confusion (the task's own elapsedSeconds
        // exposed the lie).
        let wait_started = std::time::Instant::now();
        enum Outcome {
            Done(Value),
            TimedOut(Value),
            Interrupted(Option<Value>),
        }
        // Bind first, classify after: a nested match inside select!
        // trips the macro's `if`-guard parsing.
        let (snapshot, interrupted) = tokio::select! {
            snapshot = manager.wait(&params.task_id, timeout) => (snapshot, false),
            // Aborted run: report the current state rather than hanging.
            // (`_ =`, not `() =>` — a unit literal before the arrow is
            // unparseable in select!'s `pat = fut` grammar.)
            _ = cancel.cancelled() => (manager.snapshot(&params.task_id), true),
        };
        let outcome = if interrupted {
            Outcome::Interrupted(snapshot)
        } else {
            match snapshot {
                Some(s) if s["running"].as_bool() == Some(true) => Outcome::TimedOut(s),
                Some(s) => Outcome::Done(s),
                None => Outcome::Interrupted(None),
            }
        };
        let waited = wait_started.elapsed().as_secs();

        match outcome {
            Outcome::TimedOut(snapshot) => Ok(AgentToolResult::text(format!(
                "Task {} is still running after {}s (wait limit {}s). Call bash_wait again to keep waiting, or bash_output to inspect partial output.\n{}",
                params.task_id,
                waited,
                timeout.as_secs(),
                serde_json::to_string_pretty(&snapshot).unwrap_or_default()
            ))),
            Outcome::Interrupted(Some(snapshot)) if snapshot["running"].as_bool() == Some(true) => {
                Ok(AgentToolResult::text(format!(
                    "bash_wait was interrupted after {}s (the run aborted the wait, NOT the task — it keeps running). Inspect with bash_output, re-wait with bash_wait, stop with kill_shell.\n{}",
                    waited,
                    serde_json::to_string_pretty(&snapshot).unwrap_or_default()
                )))
            }
            Outcome::Interrupted(Some(snapshot)) => Ok(AgentToolResult::text(format!(
                "bash_wait was interrupted after {}s; the task had already finished.\n{}",
                waited,
                serde_json::to_string_pretty(&snapshot).unwrap_or_default()
            ))),
            Outcome::Interrupted(None) => Ok(AgentToolResult::text(format!(
                "bash_wait was interrupted after {}s and the task state is unavailable.",
                waited
            ))),
            Outcome::Done(snapshot) => Ok(AgentToolResult::text(format!(
                "Task {} finished.\n{}",
                params.task_id,
                serde_json::to_string_pretty(&snapshot).unwrap_or_default()
            ))),
        }
    }
}

// ---------------------------------------------------------------------
// kill_shell tool
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KillShellParams {
    /// Task id of the background task to terminate (e.g. "bg1")
    task_id: String,
}

pub struct KillShellTool {
    services: ToolServices,
}

impl KillShellTool {
    pub fn new(services: ToolServices) -> Self {
        KillShellTool { services }
    }
}

impl std::fmt::Debug for KillShellTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KillShellTool").finish()
    }
}

#[async_trait]
impl AgentTool for KillShellTool {
    fn name(&self) -> &'static str {
        "kill_shell"
    }
    fn label(&self) -> &str {
        "kill_shell"
    }
    fn description(&self) -> &str {
        "Terminate a background task started with bash run_in_background. \
         Kills the whole process tree."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<KillShellParams>()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: KillShellParams = serde_json::from_value(params)
            .map_err(|e| format!("invalid kill_shell params: {e}"))?;
        match self.services.background.kill(&params.task_id) {
            crate::background::KillOutcome::Signalled => Ok(AgentToolResult::text(format!(
                "Sent kill signal to background task {}",
                params.task_id
            ))),
            crate::background::KillOutcome::AlreadyFinished(status) => {
                Ok(AgentToolResult::text(format!(
                    "Background task {} already finished ({}).",
                    params.task_id,
                    status.describe()
                )))
            }
            crate::background::KillOutcome::Unknown => {
                Err(format!("Unknown background task: {}", params.task_id))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn services_with_shell() -> ToolServices {
        crate::default_services(std::env::current_dir().unwrap())
    }

    /// Lock-order regression: the registry `inner` mutex must never be held
    /// while a task's `output` mutex is taken (snapshot/eviction/prune used
    /// to nest them). Any path acquiring the two in the opposite order
    /// deadlocks both waiters — and because `std::sync::Mutex::lock`
    /// blocks the async worker thread synchronously, a wedged `bash_wait`
    /// never polls its timeout or the ESC cancellation again (observed in
    /// the wild: run frozen mid-tool-call, UI alive). Hammer the three
    /// paths from OS threads; an inversion shows up as this test hanging.
    #[test]
    fn registry_snapshot_register_prune_do_not_deadlock() {
        let manager = BackgroundTaskManager::new();
        // Pre-fill past the eviction cap so churn keeps hitting eviction.
        for _ in 0..150 {
            manager
                .register("prefill".to_string())
                .finish(TaskStatus::Exited(0), None);
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut threads = Vec::new();
        // Churn: register + finish feeds both eviction and prune.
        threads.push({
            let manager = manager.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    manager
                        .register("churn".to_string())
                        .finish(TaskStatus::Exited(0), None);
                }
            })
        });
        // Snapshot whatever is registered at the moment (known or not).
        threads.push({
            let manager = manager.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut n = 0u32;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = manager.snapshot(&format!("bg{n}"));
                    n = (n + 1) % 256;
                }
            })
        });
        // Prune finished tasks continuously (throttled so eviction also
        // gets windows above the cap).
        threads.push({
            let manager = manager.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    manager.prune_finished();
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            })
        });
        std::thread::sleep(std::time::Duration::from_secs(2));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }
    }

    /// Regression for the production wedge behind the 1.0.11 audit: with
    /// the snapshot guard held by a let-chain scrutinee, appending the
    /// denial hint re-locked the same `std::sync::Mutex` and the pump
    /// thread deadlocked on itself. The helper must return (and append).
    #[test]
    fn sandbox_denial_hint_append_does_not_self_deadlock() {
        let sandbox = Some((
            crate::sandbox::SandboxBackend::WindowsJob,
            crate::sandbox::SandboxSpec::default(),
        ));
        let output = Mutex::new(OutputAccumulator::new("test"));
        output
            .lock()
            .unwrap()
            .append(b"cp: /etc/passwd: Operation not permitted");
        append_sandbox_denial_hint(&sandbox, &output, 1);
        assert!(
            output
                .lock()
                .unwrap()
                .snapshot(false)
                .content
                .contains("ran sandboxed")
        );
        // No hint for successful exits, missing sandbox, or clean output.
        let clean = Mutex::new(OutputAccumulator::new("test"));
        clean.lock().unwrap().append(b"Operation not permitted");
        append_sandbox_denial_hint(&sandbox, &clean, 0);
        append_sandbox_denial_hint(&None, &clean, 1);
        assert!(
            !clean
                .lock()
                .unwrap()
                .snapshot(false)
                .content
                .contains("ran sandboxed")
        );
    }

    #[tokio::test]
    async fn background_task_runs_to_completion() {
        let services = services_with_shell();
        let manager = services.background.clone();
        let id = manager
            .spawn("echo hello-bg".to_string(), &services)
            .unwrap();
        for _ in 0..100 {
            let snap = manager.snapshot(&id).unwrap();
            if !snap["running"].as_bool().unwrap() {
                assert!(snap["output"].as_str().unwrap().contains("hello-bg"));
                assert_eq!(snap["status"].as_str().unwrap(), "exited with code 0");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("task did not finish in 10s");
    }

    #[tokio::test]
    async fn kill_stops_long_running_task() {
        let services = services_with_shell();
        let manager = services.background.clone();
        let id = manager.spawn("sleep 60".to_string(), &services).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(manager.kill(&id).signalled());
        for _ in 0..100 {
            let snap = manager.snapshot(&id).unwrap();
            if !snap["running"].as_bool().unwrap() {
                assert_eq!(snap["status"].as_str().unwrap(), "killed");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("task was not killed in 10s");
    }

    #[tokio::test]
    async fn completion_notification_is_sent() {
        let services = services_with_shell();
        let manager = services.background.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        manager.set_notify(tx);
        let id = manager.spawn("echo done".to_string(), &services).unwrap();
        let note = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.task_id, id);
        assert_eq!(note.status, "exited with code 0");
    }

    #[tokio::test]
    async fn unknown_task_snapshot_is_none() {
        let manager = BackgroundTaskManager::new();
        assert!(manager.snapshot("nope").is_none());
        assert!(!manager.kill("nope").signalled());
    }

    /// kill_shell on an already-finished task must not claim it sent a kill
    /// signal — the task is done, there is nothing to kill.
    #[tokio::test]
    async fn kill_on_finished_task_reports_finished() {
        let manager = BackgroundTaskManager::new();
        let handle = manager.register("t".to_string());
        let id = handle.id.clone();
        handle.finish(TaskStatus::Exited(0), None);
        let outcome = manager.kill(&id);
        assert!(!outcome.signalled(), "finished task must not be signalled");
        assert!(outcome.already_finished(), "{outcome:?}");

        let unknown = manager.kill("nope");
        assert!(unknown.unknown());
    }

    /// A TaskHandle dropped without finish() (driver bug / aborted spawner)
    /// must not leave a phantom "running" task forever.
    #[tokio::test]
    async fn dropped_handle_does_not_leave_phantom_running_task() {
        let manager = BackgroundTaskManager::new();
        let handle = manager.register("t".to_string());
        let id = handle.id.clone();
        drop(handle);
        let snap = manager.snapshot(&id).unwrap();
        assert!(!snap["running"].as_bool().unwrap(), "{snap}");
        assert!(
            snap["status"].as_str().unwrap().starts_with("failed"),
            "{snap}"
        );
    }

    /// Regression: a shell that dies by signal (no exit code) must not be
    /// reported as "exited with code 0".
    #[cfg(unix)]
    #[tokio::test]
    async fn signal_death_is_not_reported_as_exit_zero() {
        let services = services_with_shell();
        if services.shell.is_none() {
            return;
        }
        let manager = services.background.clone();
        let id = manager.spawn("kill -9 $$".to_string(), &services).unwrap();
        for _ in 0..100 {
            let snap = manager.snapshot(&id).unwrap();
            if !snap["running"].as_bool().unwrap() {
                let status = snap["status"].as_str().unwrap();
                assert!(status.starts_with("killed"), "unexpected status: {status}");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("task did not finish in 10s");
    }

    /// bash_wait returns the moment the task finishes — well before the
    /// timeout — with the final snapshot.
    #[tokio::test]
    async fn wait_returns_on_completion() {
        let services = services_with_shell();
        let manager = services.background.clone();
        let id = manager
            .spawn("echo waited-output".to_string(), &services)
            .unwrap();
        let started = std::time::Instant::now();
        let snap = manager
            .wait(&id, std::time::Duration::from_secs(30))
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(!snap["running"].as_bool().unwrap(), "{snap}");
        assert_eq!(snap["status"].as_str().unwrap(), "exited with code 0");
        assert!(snap["output"].as_str().unwrap().contains("waited-output"));
    }

    /// A waiter that subscribed BEFORE the task finished must still wake —
    /// no missed-notification race between status check and await.
    #[tokio::test]
    async fn wait_does_not_miss_concurrent_finish() {
        let manager = BackgroundTaskManager::new();
        let handle = manager.register("t".to_string());
        let id = handle.id.clone();
        let waiter = {
            let manager = manager.clone();
            let id = id.clone();
            tokio::spawn(async move { manager.wait(&id, std::time::Duration::from_secs(10)).await })
        };
        // Let the waiter subscribe, then finish out from under it.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        handle.finish(TaskStatus::Exited(0), None);
        let snap = waiter.await.unwrap().unwrap();
        assert!(!snap["running"].as_bool().unwrap(), "{snap}");
    }

    /// Timeout on a still-running task returns the running snapshot.
    #[tokio::test]
    async fn wait_times_out_on_running_task() {
        let manager = BackgroundTaskManager::new();
        let handle = manager.register("t".to_string());
        let started = std::time::Instant::now();
        let snap = manager
            .wait(&handle.id, std::time::Duration::from_millis(200))
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(snap["running"].as_bool().unwrap(), "{snap}");
        handle.finish(TaskStatus::Exited(0), None);
    }

    /// Finished tasks are evicted past a cap so the session-scoped
    /// registry doesn't grow unboundedly; running tasks are never evicted.
    #[tokio::test]
    async fn finished_tasks_are_evicted_past_cap() {
        let manager = BackgroundTaskManager::new();
        let mut keep_running = Vec::new();
        for i in 0..150 {
            let handle = manager.register(format!("task-{i}"));
            if i < 3 {
                keep_running.push(handle); // stays running
            } else {
                handle.finish(TaskStatus::Exited(0), None);
            }
        }
        let list = manager.list();
        let tasks = list.as_array().unwrap();
        let running = tasks
            .iter()
            .filter(|t| t["running"].as_bool() == Some(true))
            .count();
        assert_eq!(running, 3, "running tasks must never be evicted: {tasks:?}");
        assert!(
            tasks.len() <= 103,
            "finished tasks must be evicted past the cap: {}",
            tasks.len()
        );
        for handle in keep_running {
            handle.finish(TaskStatus::Exited(0), None);
        }
    }

    /// Already-finished task: wait returns immediately. Unknown id: None.
    #[tokio::test]
    async fn wait_on_finished_and_unknown_tasks() {
        let manager = BackgroundTaskManager::new();
        let handle = manager.register("t".to_string());
        let id = handle.id.clone();
        handle.finish(TaskStatus::Exited(0), None);
        let snap = manager
            .wait(&id, std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!snap["running"].as_bool().unwrap());
        assert!(
            manager
                .wait("nope", std::time::Duration::from_millis(10))
                .await
                .is_none()
        );
    }

    fn result_text(result: &AgentToolResult) -> String {
        match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text content"),
        }
    }

    /// A wait that hits its timeout must say how long it ACTUALLY waited
    /// and what the limit was — the historic bug reported the requested
    /// timeout as if it had elapsed, misreading an early return.
    #[tokio::test]
    async fn bash_wait_timeout_reports_actual_wait_and_limit() {
        let services = services_with_shell();
        let tool = BashWaitTool::new(services.clone());
        let id = services
            .background
            .spawn("sleep 30".to_string(), &services)
            .unwrap();
        let started = std::time::Instant::now();
        let result = tool
            .execute(
                "call",
                serde_json::json!({ "task_id": id, "timeout": 2 }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        let text = result_text(&result);
        assert!(text.contains("wait limit 2s"), "{text}");
        assert!(text.contains("still running after"), "{text}");
        assert!(
            elapsed >= std::time::Duration::from_secs(2),
            "returned early: {elapsed:?}"
        );
        services.background.kill(&id);
    }

    /// An aborted run interrupts the wait — the message must say so
    /// (and that the TASK keeps running), not masquerade as a timeout.
    #[tokio::test]
    async fn bash_wait_interrupted_names_the_real_cause() {
        let services = services_with_shell();
        let tool = BashWaitTool::new(services.clone());
        let id = services
            .background
            .spawn("sleep 30".to_string(), &services)
            .unwrap();
        let cancel = CancellationToken::new();
        let cancel_later = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            cancel_later.cancel();
        });
        let result = tool
            .execute(
                "call",
                serde_json::json!({ "task_id": id, "timeout": 60 }),
                cancel,
                &|_| {},
            )
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(text.contains("interrupted"), "{text}");
        assert!(text.contains("keeps running"), "{text}");
        assert!(!text.contains("wait limit 60s"), "{text}");
        services.background.kill(&id);
    }

    /// Finished task under an aborted wait: report the finish, not a lie.
    #[tokio::test]
    async fn bash_wait_interrupted_after_finish_reports_finish() {
        let services = services_with_shell();
        let tool = BashWaitTool::new(services.clone());
        let id = services
            .background
            .spawn("echo done".to_string(), &services)
            .unwrap();
        // Let it finish first.
        let _ = services
            .background
            .wait(&id, std::time::Duration::from_secs(10))
            .await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tool
            .execute(
                "call",
                serde_json::json!({ "task_id": id, "timeout": 60 }),
                cancel,
                &|_| {},
            )
            .await
            .unwrap();
        let text = result_text(&result);
        // Both select arms were ready (task done AND cancel fired), so
        // either the Done arm ("finished") or the Interrupted arm ("had
        // already finished") can win — both are honest. What must never
        // appear is a false "still running" claim.
        assert!(text.contains("finished"), "{text}");
        assert!(!text.contains("still running"), "{text}");
    }
}
