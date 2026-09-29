//! Sub-agent tool: spawn isolated child agent loops in parallel (tack-subagents
//! as a built-in). The child gets a fresh context (task-only system prompt)
//! and the standard coding tool set (minus itself, preventing recursion);
//! the parent's final assistant text becomes the tool result.
//!
//! Extensions over the hard-coded original:
//! - `agent` param: run a custom agent definition from `.pi/agents/*.md`
//!   (own system prompt, tool whitelist, default model) — tack-app/agents.rs.
//! - `isolation: "worktree"`: the child works in a fresh git worktree on a
//!   temporary branch, so parallel sub-agents never step on each other's
//!   files. The result reports the branch/worktree path + diff stat; the
//!   worktree is left in place for the parent to inspect or merge.
//! - SubagentStart hooks (settings `hooks.SubagentStart`) gate delegation:
//!   a block verdict refuses the run with the hook's reason (in-band tool
//!   error), `additionalContext` is folded into the child's task. Matchers
//!   match the sub-agent name (custom agent name, `subagent` for the
//!   default) — same as SubagentStop.

use std::sync::Arc;

use serde_json::{Value, json};
use tack_agent_core::{
    AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentTool, AgentToolResult,
    ToolExecutionMode, agent_loop,
};
use tack_ai::{Model, Provider};
use tokio_util::sync::CancellationToken;

use crate::agents::AgentDefinition;

/// A no-op hooks impl for the child loop.
#[derive(Debug)]
struct NoopHooks;

#[async_trait::async_trait]
impl AgentHooks for NoopHooks {}

/// Cross-run coordination for sub-agents, shared by every SubagentTool clone
/// in a session (foreground, parallel batches, and fire-and-forget children):
/// a concurrency cap (semaphore) so N parallel calls don't stampede the
/// provider, and a shared token budget so background children can't bypass
/// the session-level tokenBudget (children don't persist into the session
/// file, so budget hooks never saw their usage).
///
/// Session-wide: the host creates ONE `Arc<SubagentLimits>` per session via
/// [`SubagentLimits::shared`] and hands it to every SubagentTool it builds
/// (tools are rebuilt per prompt — a fresh limits object per tool would
/// silently reset the budget every turn and strand background children's
/// usage in a stale counters object).
#[derive(Debug)]
pub(crate) struct SubagentLimits {
    semaphore: Arc<tokio::sync::Semaphore>,
    budget_tokens: Option<u64>,
    used_tokens: std::sync::atomic::AtomicU64,
}

impl SubagentLimits {
    fn new(max_concurrent: Option<usize>, budget_tokens: Option<u64>) -> Self {
        SubagentLimits {
            semaphore: Arc::new(tokio::sync::Semaphore::new(
                // tokio's MAX_PERMITS is far below usize::MAX; a cap of 1024
                // concurrent children is indistinguishable from unlimited.
                max_concurrent.unwrap_or(1024).max(1),
            )),
            budget_tokens,
            used_tokens: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Session-owned shared limits (one per session; see struct docs).
    pub(crate) fn shared(
        max_concurrent: Option<usize>,
        budget_tokens: Option<u64>,
    ) -> Arc<SubagentLimits> {
        Arc::new(SubagentLimits::new(max_concurrent, budget_tokens))
    }

    /// Over budget already?
    fn exhausted(&self) -> Option<(u64, u64)> {
        let budget = self.budget_tokens?;
        let used = self.used_tokens.load(std::sync::atomic::Ordering::Relaxed);
        (used >= budget).then_some((used, budget))
    }

    fn exhaustion_error(&self) -> Option<String> {
        self.exhausted().map(|(used, budget)| {
            format!(
                "subagent token budget exhausted: sub-agents used {used} / {budget} tokens. \
                 Finish some background tasks or raise subagents.budgetTokens."
            )
        })
    }

    fn record(&self, tokens: u64) {
        self.used_tokens
            .fetch_add(tokens, std::sync::atomic::Ordering::Relaxed);
    }
}

pub struct SubagentTool {
    provider: Arc<dyn Provider>,
    model: Model,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    agents: Vec<AgentDefinition>,
    features: crate::settings::FeatureFlags,
    /// Declarative `permissions.deny` rules enforced in the CHILD loop.
    /// Children are non-interactive (no permission prompts), so without
    /// this a spawned sub-agent would bypass the session's deny list
    /// entirely — deny rules are the child's only permission layer.
    deny_rules: crate::permissions::PermissionRules,
    /// Managed lockedProvider/lockedModel (settings.rs): delegated work
    /// must not route around a provider/model lock via the `model` param.
    locked_provider: Option<String>,
    locked_model: Option<String>,
    /// SubagentStart hook groups, fired (and enforced) before a child
    /// loop starts: a block verdict refuses the delegation.
    start_hook_groups: Vec<crate::shell_hooks::HookGroup>,
    /// SubagentStop hook groups, fired on completion.
    stop_hook_groups: Vec<crate::shell_hooks::HookGroup>,
    /// Engine shared by the start/stop hook groups.
    hook_engine: Option<crate::shell_hooks::HookEngine>,
    /// Background task registry for run_in_background sub-agents.
    background: Option<tack_tools::background::BackgroundTaskManager>,
    /// Shared concurrency/budget coordination (clones share the same state).
    limits: Arc<SubagentLimits>,
    /// User-scope memory root override propagated to the child tool set
    /// (settings `memoryDirectory`; TACK_MEMORY_DIR env wins regardless).
    memory_dir_override: Option<std::path::PathBuf>,
    /// Working directory for the child loop and the base repo for worktree
    /// isolation. None = the process's current directory (legacy default;
    /// hosts should prefer `.with_cwd(...)` so children don't depend on
    /// global process state).
    cwd: Option<std::path::PathBuf>,
    /// settings `cacheRetention` propagated to the child agent loop.
    cache_retention: Option<tack_ai::CacheRetention>,
    /// settings `subagents.inheritPlugins`: which parts of the parent's
    /// active plugin set follow the child (default Hooks — without hook
    /// inheritance a spawned sub-agent would bypass guardrail plugins'
    /// beforeToolCall verdicts entirely).
    inherit_plugins: crate::settings::SubagentInheritance,
    /// Plugin hook bridges shared from the parent's loaded extensions
    /// (consumed when inherit_plugins is Hooks|Full). The underlying
    /// plugin connections are Arc-shared with the parent session; the
    /// JSON-RPC peer multiplexes concurrent parent/child calls.
    extension_hooks: Vec<Arc<dyn AgentHooks>>,
    /// Plugin tools shared from the parent's loaded extensions (consumed
    /// when inherit_plugins is Full).
    extension_tools: Vec<Arc<dyn AgentTool>>,
}

/// Progress callback for background sub-agents (tool activity lines).
type ProgressFn = Arc<dyn Fn(&str) + Send + Sync>;

impl Clone for SubagentTool {
    fn clone(&self) -> Self {
        SubagentTool {
            provider: self.provider.clone(),
            model: self.model.clone(),
            auth: self.auth.clone(),
            agents: self.agents.clone(),
            features: self.features.clone(),
            deny_rules: self.deny_rules.clone(),
            locked_provider: self.locked_provider.clone(),
            locked_model: self.locked_model.clone(),
            start_hook_groups: self.start_hook_groups.clone(),
            stop_hook_groups: self.stop_hook_groups.clone(),
            hook_engine: self.hook_engine.clone(),
            background: self.background.clone(),
            limits: self.limits.clone(),
            memory_dir_override: self.memory_dir_override.clone(),
            cwd: self.cwd.clone(),
            cache_retention: self.cache_retention,
            inherit_plugins: self.inherit_plugins,
            extension_hooks: self.extension_hooks.clone(),
            extension_tools: self.extension_tools.clone(),
        }
    }
}

impl SubagentTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: Model,
        auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    ) -> Self {
        SubagentTool {
            provider,
            model,
            auth,
            agents: Vec::new(),
            features: crate::settings::FeatureFlags::default(),
            deny_rules: crate::permissions::PermissionRules::default(),
            locked_provider: None,
            locked_model: None,
            start_hook_groups: Vec::new(),
            stop_hook_groups: Vec::new(),
            hook_engine: None,
            background: None,
            limits: Arc::new(SubagentLimits::new(None, None)),
            memory_dir_override: None,
            cwd: None,
            cache_retention: None,
            inherit_plugins: crate::settings::SubagentInheritance::default(),
            extension_hooks: Vec::new(),
            extension_tools: Vec::new(),
        }
    }

    pub fn with_agents(mut self, agents: Vec<AgentDefinition>) -> Self {
        self.agents = agents;
        self
    }

    /// Feature flags the CHILD agent loop inherits (tool filtering).
    pub fn with_features(mut self, features: crate::settings::FeatureFlags) -> Self {
        self.features = features;
        self
    }

    /// `permissions.deny` rules the CHILD loop enforces. Deny enforcement
    /// is non-interactive by nature, so it is safe (and required) for
    /// children; interactive prompts are NOT inherited.
    pub fn with_deny_rules(mut self, rules: crate::permissions::PermissionRules) -> Self {
        self.deny_rules = rules;
        self
    }

    /// Managed provider/model locks (managed settings lockedProvider /
    /// lockedModel): the child's resolved model is validated against them.
    pub fn with_model_locks(
        mut self,
        locked_provider: Option<String>,
        locked_model: Option<String>,
    ) -> Self {
        self.locked_provider = locked_provider;
        self.locked_model = locked_model;
        self
    }

    /// SubagentStart hooks fired (and enforced) before a child loop
    /// starts: a `block` verdict refuses the delegation with the hook's
    /// reason (an in-band tool error); `additionalContext` fragments are
    /// folded into the child's task. Matchers match the sub-agent name
    /// (custom agent name, `subagent` for the default).
    pub fn with_start_hooks(
        mut self,
        groups: Vec<crate::shell_hooks::HookGroup>,
        engine: crate::shell_hooks::HookEngine,
    ) -> Self {
        self.start_hook_groups = groups;
        self.hook_engine = Some(engine);
        self
    }

    /// SubagentStop hooks fired when a child loop finishes.
    pub fn with_stop_hooks(
        mut self,
        groups: Vec<crate::shell_hooks::HookGroup>,
        engine: crate::shell_hooks::HookEngine,
    ) -> Self {
        self.stop_hook_groups = groups;
        self.hook_engine = Some(engine);
        self
    }

    /// Background task registry (run_in_background support).
    pub fn with_background(
        mut self,
        manager: tack_tools::background::BackgroundTaskManager,
    ) -> Self {
        self.background = Some(manager);
        self
    }

    /// settings `memoryDirectory` propagated to the child memory tool.
    pub fn with_memory_dir(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.memory_dir_override = dir;
        self
    }

    /// settings `cacheRetention` propagated to the child agent loop (None =
    /// provider default resolution: TACK_CACHE_RETENTION env, then short).
    pub fn with_cache_retention(mut self, retention: Option<tack_ai::CacheRetention>) -> Self {
        self.cache_retention = retention;
        self
    }

    /// settings `subagents.inheritPlugins`: which plugin surfaces follow
    /// the child loop (see `SubagentInheritance`).
    pub fn with_plugin_inheritance(mut self, mode: crate::settings::SubagentInheritance) -> Self {
        self.inherit_plugins = mode;
        self
    }

    /// Plugin hook bridges (`ExtensionManager::hooks()`) shared from the
    /// parent session; enforced in the child when inheritance is Hooks|Full.
    pub fn with_extension_hooks(mut self, hooks: Vec<Arc<dyn AgentHooks>>) -> Self {
        self.extension_hooks = hooks;
        self
    }

    /// Plugin tools (`ExtensionManager::tools_with_untrusted(..)`) shared
    /// from the parent session; added to the child tool set when
    /// inheritance is Full.
    pub fn with_extension_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        self.extension_tools = tools;
        self
    }

    /// Explicit working directory for the child loop (and the base repo a
    /// worktree is created from). Without it the tool falls back to the
    /// process's current directory — fine for the CLI, fragile for
    /// long-lived hosts (TUI/rpc) whose cwd can change under them.
    pub fn with_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    /// The effective working directory (explicit `with_cwd` wins; falls
    /// back to the process cwd for backwards compatibility).
    fn cwd(&self) -> Result<std::path::PathBuf, String> {
        match &self.cwd {
            Some(cwd) => Ok(cwd.clone()),
            None => std::env::current_dir().map_err(|e| e.to_string()),
        }
    }

    /// Concurrency cap + shared token budget across all sub-agent runs of
    /// this session (settings subagents.maxConcurrent / subagents.budgetTokens).
    /// Prefer `SubagentTool::with_shared_limits` with a session-owned
    /// limits object; this constructor resets usage to zero and is only
    /// correct for one-shot (print-mode) sessions.
    pub fn with_limits(
        mut self,
        max_concurrent: Option<usize>,
        budget_tokens: Option<u64>,
    ) -> Self {
        self.limits = Arc::new(SubagentLimits::new(max_concurrent, budget_tokens));
        self
    }

    /// Attach a session-owned limits object so the budget/concurrency cap is
    /// shared across every prompt's tool set and every background child.
    pub(crate) fn with_shared_limits(mut self, limits: Arc<SubagentLimits>) -> Self {
        self.limits = limits;
        self
    }
}

impl std::fmt::Debug for SubagentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentTool").finish()
    }
}

const SUBAGENT_SYSTEM_PROMPT: &str = "You are a sub-agent working on a specific delegated task. \
    Work autonomously using the available tools, then report back concisely: what you did, \
    the outcome, and anything the parent agent must know. Do not ask questions — \
    make reasonable assumptions and note them in the report.";

const MAX_RESULT_CHARS: usize = 30_000;

/// Validated start parameters plus the SubagentStart verdict's
/// additionalContext, threaded into the child run.
struct StartGate {
    task: String,
    agent_def: Option<AgentDefinition>,
    hook_context: Vec<String>,
}

/// Hook-facing name of the child (Claude Code matches Subagent hooks on
/// the sub-agent type): the custom agent name, `subagent` for the default.
fn agent_type(agent_def: Option<&AgentDefinition>) -> String {
    agent_def
        .map(|a| a.name.clone())
        .unwrap_or_else(|| "subagent".to_string())
}

/// A git worktree created for one isolated sub-agent run.
struct Worktree {
    path: std::path::PathBuf,
    branch: String,
    repo_root: std::path::PathBuf,
}

fn git(args: &[&str], repo: &std::path::Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null());
    cmd
}

async fn git_output(repo: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = git(args, repo)
        .output()
        .await
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Create a worktree on a new branch at HEAD. The branch name carries a
/// random suffix so parallel sub-agents never collide.
async fn create_worktree(cwd: &std::path::Path) -> Result<Worktree, String> {
    let repo_root =
        std::path::PathBuf::from(git_output(cwd, &["rev-parse", "--show-toplevel"]).await?);
    let suffix: String = rand::random::<[u8; 4]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let branch = format!("tack-subagent-{suffix}");
    let path = std::env::temp_dir().join(format!("tack-worktree-{suffix}"));
    git_output(
        &repo_root,
        &[
            "worktree",
            "add",
            &path.display().to_string(),
            "-b",
            &branch,
            "HEAD",
        ],
    )
    .await?;
    Ok(Worktree {
        path,
        branch,
        repo_root,
    })
}

/// Diff stat of the worktree vs. its base (uncommitted changes included).
async fn worktree_diff_stat(worktree: &Worktree) -> String {
    // Stage nothing; diff working tree against HEAD of the temp branch.
    let mut stat = git_output(&worktree.path, &["diff", "--stat", "HEAD"])
        .await
        .unwrap_or_default();
    let untracked = git_output(
        &worktree.path,
        &["ls-files", "--others", "--exclude-standard"],
    )
    .await
    .unwrap_or_default();
    if !untracked.is_empty() {
        if !stat.is_empty() {
            stat.push('\n');
        }
        stat.push_str(&format!("untracked files:\n{untracked}"));
    }
    stat
}

#[async_trait::async_trait]
impl AgentTool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn label(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Delegate a self-contained task to a sub-agent with an isolated context window. \
         Multiple subagent calls in one message run in parallel. Use for independent \
         research/exploration/implementation subtasks that would bloat the main context. \
         Pass agent to use a custom agent definition from .pi/agents/*.md, \
         isolation=worktree when parallel sub-agents would edit overlapping files, and \
         run_in_background=true for fire-and-forget long-running tasks (results arrive via \
         background task notification)."
    }

    fn parameters_schema(&self) -> Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "The complete task description for the sub-agent (it sees no prior conversation)" },
                "description": { "type": "string", "description": "Short label for the task (shown in the UI)" },
                "model": { "type": "string", "description": "Optional model override as provider/id" },
                "isolation": { "type": "string", "enum": ["worktree"], "description": "Run in a throwaway git worktree (safe parallel file edits; result includes the branch + diff)" },
                "run_in_background": { "type": "boolean", "description": "Run the sub-agent in the background: returns a task id immediately; wait for completion with bash_wait, poll progress with bash_output, notified on completion. Use for long-running delegated work you don't need to block on." }
            },
            "required": ["task"]
        });
        if !self.agents.is_empty() {
            let list = self
                .agents
                .iter()
                .map(|a| format!("{}: {}", a.name, a.description))
                .collect::<Vec<_>>()
                .join("; ");
            schema["properties"]["agent"] = json!({
                "type": "string",
                "description": format!("Custom agent to run. Available: {list}")
            });
        }
        schema
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        // Fire-and-forget: register in the shared task manager, return the
        // id immediately, deliver the result via the completion notification.
        if params.get("run_in_background").and_then(Value::as_bool) == Some(true) {
            let Some(manager) = self.background.clone() else {
                return Err("background sub-agents are not available in this mode".to_string());
            };
            // Refuse synchronously when the budget is already spent — the
            // model must see the refusal as a tool error, not as a
            // background task that immediately fails out of band.
            if let Some(e) = self.limits.exhaustion_error() {
                return Err(e);
            }
            // SubagentStart gate runs synchronously too: a blocking hook
            // must surface as a tool error, not as a background task that
            // immediately fails out of band.
            let gate = self.subagent_start_gate(&params).await?;
            let task_preview: String = params
                .get("task")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            let handle = manager.register(format!("subagent: {task_preview}"));
            let id = handle.id.clone();
            let this = self.clone();
            tokio::spawn(async move {
                use futures_util::FutureExt as _;

                let progress = {
                    let output = handle.output.clone();
                    move |line: &str| {
                        let mut out = output.lock().unwrap_or_else(|e| e.into_inner());
                        out.append(line.as_bytes());
                        out.append(b"\n");
                    }
                };
                let progress: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(progress);
                let cancel = handle.cancel.clone();
                let run = this.run_gated(params, cancel, Some(progress), gate);
                match std::panic::AssertUnwindSafe(run).catch_unwind().await {
                    Ok(Ok((text, _))) => {
                        handle.log("\n=== final result ===");
                        handle.log(&text);
                        handle.finish(tack_tools::background::TaskStatus::Exited(0), None);
                    }
                    Ok(Err(e)) => {
                        handle.finish(tack_tools::background::TaskStatus::Failed, Some(e));
                    }
                    Err(_) => {
                        // Panic: the hook logged details; never leave the
                        // background task "Running" forever.
                        handle.finish(
                            tack_tools::background::TaskStatus::Failed,
                            Some("subagent panicked (details in crash.log)".to_string()),
                        );
                    }
                }
            });
            return Ok(AgentToolResult {
                content: vec![tack_ai::InputContentBlock::text(format!(
                    "Started background sub-agent task {id}. Wait for it with bash_wait \
                     (task_id=\"{id}\"), poll progress with bash_output; \
                     you will be notified when it finishes."
                ))],
                details: json!({ "taskId": id, "background": true }),
                usage: None,
                terminate: false,
                added_tool_names: None,
            });
        }

        let (result, details) = self.run(params, cancel, None).await?;
        Ok(AgentToolResult {
            content: vec![tack_ai::InputContentBlock::text(result)],
            details,
            usage: None,
            terminate: false,
            added_tool_names: None,
        })
    }
}

impl SubagentTool {
    /// Resolve the child model: an explicit `provider/id` override is fully
    /// resolved (api kind, base_url, limits) instead of patching the id onto
    /// the parent's model — a cross-provider override would otherwise keep
    /// the parent's api kind and send the override id to the wrong API.
    fn resolve_child_model(
        &self,
        override_model: Option<&str>,
        agent_dir: &std::path::Path,
    ) -> Result<Model, String> {
        let model = match override_model {
            Some(override_model) => {
                let (provider, id) = override_model
                    .split_once('/')
                    .ok_or_else(|| "model must be provider/id".to_string())?;
                crate::model::resolve_model(provider, Some(id), agent_dir)
                    .map_err(|e| format!("invalid model override {override_model:?}: {e}"))?
            }
            None => self.model.clone(),
        };
        // Managed locks bind delegated work too: a sub-agent must not route
        // around lockedProvider/lockedModel via the `model` param. Reject
        // (not clamp), matching the TUI /model picker.
        crate::model::enforce_locked_values(
            self.locked_provider.as_deref(),
            self.locked_model.as_deref(),
            &model,
        )?;
        Ok(model)
    }

    /// Custom agent definition named by the `agent` param, if any.
    fn resolve_agent_def(&self, params: &Value) -> Result<Option<AgentDefinition>, String> {
        match params.get("agent").and_then(Value::as_str) {
            Some(name) => Ok(Some(
                self.agents
                    .iter()
                    .find(|a| a.name == name)
                    .ok_or_else(|| {
                        let available = self
                            .agents
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>();
                        format!(
                            "unknown agent {name:?} (available: {})",
                            available.join(", ")
                        )
                    })?
                    .clone(),
            )),
            None => Ok(None),
        }
    }

    /// SubagentStart hook gate: validates the task/agent params, fires the
    /// configured handlers (matcher = sub-agent name, Claude Code
    /// semantics), and enforces the merged verdict. A block verdict
    /// refuses the delegation with the hook's reason (surfaced as an
    /// in-band tool error); `additionalContext` fragments are folded into
    /// the child's task. Runs BEFORE any side effect (worktree creation,
    /// token spend), so a blocked start leaves nothing behind.
    async fn subagent_start_gate(&self, params: &Value) -> Result<StartGate, String> {
        let task = params
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required parameter: task".to_string())?;
        if task.trim().is_empty() {
            return Err("task must not be empty".to_string());
        }
        // Custom agent definition (system prompt / tool whitelist / model).
        let agent_def = self.resolve_agent_def(params)?;
        let agent_type = agent_type(agent_def.as_ref());
        let allow = || StartGate {
            task: task.to_string(),
            agent_def: agent_def.clone(),
            hook_context: Vec::new(),
        };
        if self.start_hook_groups.is_empty() {
            return Ok(allow());
        }
        let Some(engine) = self.hook_engine.clone() else {
            return Ok(allow());
        };
        let cwd = self.cwd()?;
        let payload = json!({
            "session_id": "",
            "transcript_path": Value::Null,
            "cwd": cwd,
            "hook_event_name": crate::shell_hooks::HookEvent::SubagentStart.as_str(),
            "agent_id": params.get("agent").and_then(Value::as_str),
            "agent_type": agent_type.clone(),
            "prompt": task.chars().take(2_000).collect::<String>(),
            "description": params.get("description").and_then(Value::as_str),
            "isolation": params.get("isolation").and_then(Value::as_str),
            "background": params
                .get("run_in_background")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
        let verdict = engine
            .run(&self.start_hook_groups, Some(&agent_type), &payload)
            .await;
        if let Some(reason) = verdict.blocked {
            return Err(format!("blocked by SubagentStart hook: {reason}"));
        }
        Ok(StartGate {
            task: task.to_string(),
            agent_def,
            hook_context: verdict.additional_context,
        })
    }

    /// The full child-loop run. Returns (result_text, details).
    async fn run(
        &self,
        params: Value,
        cancel: CancellationToken,
        progress: Option<ProgressFn>,
    ) -> Result<(String, Value), String> {
        let gate = self.subagent_start_gate(&params).await?;
        self.run_gated(params, cancel, progress, gate).await
    }

    /// Child-loop run behind an already-evaluated SubagentStart gate
    /// (background callers gate synchronously in `execute` so a block
    /// surfaces as a tool error, not an out-of-band task failure).
    async fn run_gated(
        &self,
        params: Value,
        cancel: CancellationToken,
        progress: Option<ProgressFn>,
        gate: StartGate,
    ) -> Result<(String, Value), String> {
        // Shared budget: refuse new children once sub-agents collectively
        // burned the allowance (background children would otherwise be
        // invisible to the session-level budget hooks).
        if let Some(e) = self.limits.exhaustion_error() {
            return Err(e);
        }
        // Concurrency cap: queue behind the permit (cancellation-aware).
        let _permit = tokio::select! {
            permit = self.limits.semaphore.acquire() => {
                permit.map_err(|_| "subagent semaphore closed".to_string())?
            }
            _ = cancel.cancelled() => return Err("subagent aborted".to_string()),
        };
        // Re-check after queueing: the budget may have been exhausted by
        // other children while this call waited for a permit.
        if let Some(e) = self.limits.exhaustion_error() {
            return Err(e);
        }

        let StartGate {
            mut task,
            agent_def,
            hook_context,
        } = gate;
        if !hook_context.is_empty() {
            task.push_str("\n\n[SubagentStart hook context]\n");
            task.push_str(&hook_context.join("\n\n"));
        }

        let model_override = params
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| agent_def.as_ref().and_then(|a| a.model.clone()));
        let model = self.resolve_child_model(
            model_override.as_deref(),
            &tack_session::default_agent_dir(),
        )?;
        let provider: Arc<dyn Provider> = match tack_ai::provider_for(&model) {
            Some(p) => p,
            None => Arc::clone(&self.provider),
        };

        // Worktree isolation: the child edits a throwaway checkout.
        let worktree = match params.get("isolation").and_then(Value::as_str) {
            Some("worktree") => {
                let cwd = self.cwd()?;
                Some(
                    create_worktree(&cwd)
                        .await
                        .map_err(|e| format!("worktree setup failed: {e}"))?,
                )
            }
            Some(other) => return Err(format!("unknown isolation {other:?} (only \"worktree\")")),
            None => None,
        };

        let child_cwd = match &worktree {
            Some(w) => w.path.clone(),
            None => self.cwd()?,
        };

        // Child tool set: standard coding tools (subagent excluded by
        // construction), narrowed to the agent's whitelist when defined and
        // to the parent's feature flags.
        let services = tack_tools::default_services(child_cwd.clone())
            .with_background_tasks_enabled(self.features.background_tasks)
            .with_memory_dir(self.memory_dir_override.clone());
        // A worktree sharing the parent's CARGO_TARGET_DIR races on build
        // fingerprints: two worktrees building different versions of a
        // common crate poison each other's rmeta (observed as phantom
        // stale-rmeta errors in parallel sub-agent sessions). Isolate the
        // target dir inside the throwaway checkout — it dies with the
        // worktree. Explicit CARGO_TARGET_DIR wins: the operator asked
        // for a specific layout.
        let services = match &worktree {
            Some(w)
                if std::env::var_os("CARGO_TARGET_DIR").is_none()
                    && w.path.join("Cargo.toml").exists() =>
            {
                services.with_env(vec![(
                    "CARGO_TARGET_DIR".to_string(),
                    w.path.join("target").into_os_string(),
                )])
            }
            _ => services,
        };
        let tools = tack_tools::create_coding_tools(&services);
        let mut tools = crate::cli_flags::filter_feature_tools(tools, &self.features);
        // Plugin inheritance (subagents.inheritPlugins "full"): plugin
        // tools join BEFORE the agent-definition whitelist so `tools`
        // narrows the combined set uniformly (ext__<plugin>__<tool> names).
        if self.inherit_plugins.inherits_tools() {
            tools.extend(self.extension_tools.iter().cloned());
        }
        let tools = match &agent_def {
            Some(def) if !def.tools.is_empty() => tools
                .into_iter()
                .filter(|t| def.tools.iter().any(|w| w == t.name()))
                .collect::<Vec<_>>(),
            _ => tools,
        };

        let system_prompt = match &agent_def {
            Some(def) => format!("{SUBAGENT_SYSTEM_PROMPT}\n\n{}", def.system_prompt),
            None => SUBAGENT_SYSTEM_PROMPT.to_string(),
        };

        // Child hooks: non-interactive, so no permission prompts — but the
        // session's `permissions.deny` rules MUST still apply, or a spawned
        // sub-agent would bypass the deny list entirely (headless CI safety
        // net). Plugin hook bridges follow the deny rules when inheritance
        // allows (default "hooks"): hard blocks first, then plugin verdicts
        // — the same order as the parent surfaces' chains. NoopHooks when
        // neither is configured.
        let hooks: Arc<dyn AgentHooks> = {
            let mut chain: Vec<Arc<dyn AgentHooks>> = Vec::new();
            if !self.deny_rules.deny.is_empty() {
                chain.push(Arc::new(crate::permissions::DenyRulesHooks {
                    rules: self.deny_rules.clone(),
                }));
            }
            if self.inherit_plugins.inherits_hooks() {
                chain.extend(self.extension_hooks.iter().cloned());
            }
            match chain.len() {
                0 => Arc::new(NoopHooks),
                1 => chain.pop().expect("len checked"),
                _ => Arc::new(tack_agent_core::HooksChain::new(chain)),
            }
        };
        let config = AgentLoopConfig {
            model,
            provider,
            hooks,
            tool_execution: ToolExecutionMode::Parallel,
            reasoning: None,
            auth: self.auth.clone(),
            max_tokens: None,
            temperature: None,
            session_id: None,
            cache_retention: self.cache_retention,
            // Sub-agents run their explicit model choice; the fallback chain
            // is a session-level concern of the parent loop.
            fallback_models: Vec::new(),
            tool_pool: Vec::new(),
            retry_cancel: None,
        };
        let context = AgentContext {
            system_prompt: Some(system_prompt),
            messages: Vec::new(),
            tools,
        };

        // Persist the child transcript to its own session file (grouped with
        // the parent's sessions dir) so hosts can offer transcript drill-down.
        let session_cwd = self.cwd()?;
        let mut child_session = match tack_session::SessionManager::create(&session_cwd, None) {
            Ok(session) => Some(session),
            Err(e) => {
                tracing::warn!("subagent child session create failed: {e}");
                None
            }
        };
        if let Some(session) = child_session.as_mut() {
            let _ = session.append_message(tack_agent_core::AgentMessage::user(task.clone()));
            let title: String = task.chars().take(60).collect();
            let _ = session.append_session_info(Some(format!("↳ {title}")));
        }

        let mut stream = agent_loop(
            vec![tack_agent_core::AgentMessage::user(task)],
            context,
            config,
            cancel.clone(),
        );

        let mut final_text = String::new();
        let mut tool_calls = 0usize;
        let mut stop_reason = String::from("stop");
        let mut error: Option<String> = None;
        let mut usage_tokens = 0u64;
        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::ToolExecutionStart { tool_name, .. } => {
                    tool_calls += 1;
                    if let Some(progress) = &progress {
                        progress(&format!("[tool] {tool_name}"));
                    }
                }
                AgentEvent::MessageEnd { message, .. } => {
                    if let Some(session) = child_session.as_mut()
                        && let Err(e) = session.append_message(message.clone())
                    {
                        tracing::warn!("subagent session persist failed: {e}");
                    }
                    if let tack_agent_core::AgentMessage::Assistant(a) = message {
                        if a.stop_reason == tack_ai::StopReason::Error {
                            stop_reason = "error".to_string();
                            error = a.error_message.clone();
                        }
                        let text = a.text();
                        if !text.trim().is_empty() {
                            final_text = text;
                        }
                        usage_tokens += a.usage.total_tokens;
                    }
                }
                _ => {}
            }
        }

        if let Some(error) = error {
            // Failed runs still burned tokens: count them against the shared
            // budget BEFORE returning, or a sequence of failing sub-agents
            // burns unbounded tokens without ever tripping the cap.
            self.limits.record(usage_tokens);
            let child_suffix = child_session
                .as_ref()
                .map(|session| format!("\n[childSessionId: {}]", session.session_id()))
                .unwrap_or_default();
            return Err(format!("sub-agent failed: {error}{child_suffix}"));
        }
        let mut result = final_text;
        if result.chars().count() > MAX_RESULT_CHARS {
            result = result.chars().take(MAX_RESULT_CHARS).collect();
            result.push_str("\n\n[truncated]");
        }
        if result.trim().is_empty() {
            result = "(sub-agent produced no text output)".to_string();
        }

        let mut details = json!({ "toolCalls": tool_calls, "stopReason": stop_reason });
        if let Some(session) = child_session.as_ref() {
            details["childSessionId"] = json!(session.session_id());
        }
        // Sub-agent usage lands in the shared budget + the tool result so the
        // parent can account for delegation cost.
        self.limits.record(usage_tokens);
        details["usageTokens"] = json!(usage_tokens);
        if let Some(budget) = self.limits.budget_tokens {
            let used = self
                .limits
                .used_tokens
                .load(std::sync::atomic::Ordering::Relaxed);
            details["budgetTokensUsed"] = json!(used);
            details["budgetTokens"] = json!(budget);
        }
        // SubagentStop hooks (fire-and-forget). Symmetric with
        // SubagentStart: matchers match the sub-agent name.
        if !self.stop_hook_groups.is_empty()
            && let Some(engine) = self.hook_engine.clone()
        {
            let groups = self.stop_hook_groups.clone();
            let agent_name = agent_type(agent_def.as_ref());
            let payload = json!({
                "session_id": "",
                "transcript_path": child_session
                    .as_ref()
                    .and_then(|session| session.session_file().map(|p| p.display().to_string()))
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
                "cwd": child_cwd,
                "hook_event_name": "SubagentStop",
                "agent_id": params.get("agent").and_then(Value::as_str),
                "agent_type": agent_name.clone(),
                "stop_hook_active": false,
                "tool_calls": tool_calls,
                "stop_reason": stop_reason,
            });
            crate::task::spawn_guarded("subagent-stop-hook", async move {
                engine.run(&groups, Some(&agent_name), &payload).await;
            });
        }
        if let Some(worktree) = &worktree {
            let stat = worktree_diff_stat(worktree).await;
            details["worktree"] = json!({
                "path": worktree.path.display().to_string(),
                "branch": worktree.branch,
                "repoRoot": worktree.repo_root.display().to_string(),
            });
            result.push_str(&format!(
                "\n\n---\nWorktree isolation: changes are on branch `{}` at `{}` (NOT in your working tree). \
                 Merge with `git merge {}` from `{}`, inspect with `git -C {} diff HEAD`.",
                worktree.branch,
                worktree.path.display(),
                worktree.branch,
                worktree.repo_root.display(),
                worktree.path.display(),
            ));
            if !stat.trim().is_empty() {
                result.push_str(&format!("\n\nDiff stat:\n{stat}"));
            } else {
                result.push_str("\n\n(no file changes in the worktree)");
            }
        }

        Ok((result, details))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Arc;

    use serde_json::json;
    use tack_tools::shell::shell_quote;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn limits_track_shared_budget_and_concurrency() {
        let limits = super::SubagentLimits::new(Some(2), Some(1000));
        assert!(
            limits.exhausted().is_none(),
            "fresh limits are not exhausted"
        );
        limits.record(600);
        limits.record(500);
        let (used, budget) = limits.exhausted().unwrap();
        assert_eq!((used, budget), (1100, 1000));

        // Semaphore caps concurrent children at 2; the third permit is only
        // available after one is dropped.
        let p1 = limits.semaphore.clone().try_acquire_owned().unwrap();
        let p2 = limits.semaphore.clone().try_acquire_owned().unwrap();
        let _ = p2;
        assert!(limits.semaphore.try_acquire().is_err());
        drop(p1);
        assert!(limits.semaphore.try_acquire().is_ok());
    }

    /// Session-scoped limits: usage recorded through one tool's limits is
    /// visible (and enforced) through another tool sharing the same object —
    /// this is what keeps the budget intact across per-prompt tool rebuilds
    /// and background children.
    #[test]
    fn shared_limits_accumulate_across_tool_instances() {
        let shared = super::SubagentLimits::shared(None, Some(1000));
        shared.record(700);
        let tool = make_tool(super::subagent_tool_test_support::done_event("ok", 0))
            .with_shared_limits(shared.clone());
        // The second tool instance sees the 700 already used: only 300 left,
        // so recording another 400 through it must exhaust the budget.
        tool.limits.record(400);
        let (used, budget) = shared.exhausted().unwrap();
        assert_eq!((used, budget), (1100, 1000));
        assert!(tool.limits.exhaustion_error().is_some());
    }

    fn make_tool(event: tack_ai::AssistantMessageEvent) -> super::SubagentTool {
        super::SubagentTool::new(
            Arc::new(super::subagent_tool_test_support::ScriptProvider { event }),
            super::subagent_tool_test_support::test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        )
    }

    /// A successful run reports usageTokens and charges the shared budget.
    #[tokio::test]
    async fn run_reports_usage_and_charges_budget() {
        let tool = make_tool(super::subagent_tool_test_support::done_event("done!", 700))
            .with_limits(None, Some(1000));
        let (text, details) = tool
            .run(
                json!({ "task": "do the thing" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(text.contains("done!"), "{text}");
        assert_eq!(details["usageTokens"], 700);
        assert_eq!(details["budgetTokensUsed"], 700);
        assert_eq!(details["budgetTokens"], 1000);
        // A second run (another 700) is still allowed — the budget is not
        // predictive — and pushes usage to 1400: over budget.
        let _ = tool
            .run(
                json!({ "task": "more work" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        // The next call is refused with a proper error.
        let err = tool
            .run(
                json!({ "task": "once more" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.contains("budget exhausted"), "{err}");
    }

    /// Regression: a FAILED sub-agent run must still charge its tokens to
    /// the shared budget — previously `record()` ran only on the success
    /// path, so repeatedly-failing children burned unbounded tokens without
    /// ever tripping budgetTokens.
    #[tokio::test]
    async fn failed_run_still_charges_budget() {
        let tool = make_tool(super::subagent_tool_test_support::error_event(
            "provider exploded",
            600,
        ))
        .with_limits(None, Some(1000));
        let err = tool
            .run(
                json!({ "task": "do the thing" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.contains("provider exploded"), "{err}");
        assert_eq!(
            tool.limits
                .used_tokens
                .load(std::sync::atomic::Ordering::Relaxed),
            600,
            "the failed run's tokens must be charged to the shared budget"
        );
        // A second failure burns past the budget; the third call is refused.
        let _ = tool
            .run(json!({ "task": "again" }), CancellationToken::new(), None)
            .await;
        let err = tool
            .run(
                json!({ "task": "once more" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.contains("budget exhausted"), "{err}");
    }

    /// Regression: a call queued behind the concurrency semaphore must
    /// re-check the budget after acquiring its permit — children finishing
    /// while it waits may have exhausted the allowance.
    #[tokio::test]
    async fn queued_call_is_refused_when_budget_exhausts_while_waiting() {
        let tool = make_tool(super::subagent_tool_test_support::done_event("ok", 0))
            .with_limits(Some(1), Some(100));
        // Hold the only permit so the run below queues on the semaphore.
        let held_permit = tool.limits.semaphore.clone().try_acquire_owned().unwrap();
        let queued = tool.clone();
        let handle = tokio::spawn(async move {
            queued
                .run(json!({ "task": "x" }), CancellationToken::new(), None)
                .await
        });
        // Let the spawned call reach the semaphore wait, then exhaust the
        // budget "from another child" before releasing the permit.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tool.limits.record(200);
        drop(held_permit);
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("budget exhausted"), "{err}");
    }

    /// Regression: an over-budget BACKGROUND call must fail synchronously
    /// as a tool error — previously it registered a task, returned
    /// "Started background sub-agent task …" (success!), and only failed
    /// out of band.
    #[tokio::test]
    async fn background_call_is_refused_synchronously_when_budget_exhausted() {
        use tack_agent_core::AgentTool as _;
        let tool = make_tool(super::subagent_tool_test_support::done_event("ok", 0))
            .with_limits(None, Some(100))
            .with_background(tack_tools::background::BackgroundTaskManager::new());
        tool.limits.record(150);
        let result = tool
            .execute(
                "call-1",
                json!({ "task": "x", "run_in_background": true }),
                CancellationToken::new(),
                &|_| {},
            )
            .await;
        let err = result.unwrap_err();
        assert!(err.contains("budget exhausted"), "{err}");
    }

    /// A background call within budget still starts normally (returns a
    /// task id immediately) and its usage lands in the shared budget.
    #[tokio::test]
    async fn background_call_within_budget_starts_and_charges() {
        use tack_agent_core::AgentTool as _;
        let tool = make_tool(super::subagent_tool_test_support::done_event(
            "bg done", 250,
        ))
        .with_limits(None, Some(1000))
        .with_background(tack_tools::background::BackgroundTaskManager::new());
        let result = tool
            .execute(
                "call-1",
                json!({ "task": "x", "run_in_background": true }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("Started background sub-agent task"), "{text}");
        // The spawned child finishes promptly (scripted provider); wait for
        // its usage to land in the shared budget.
        for _ in 0..100 {
            if tool
                .limits
                .used_tokens
                .load(std::sync::atomic::Ordering::Relaxed)
                == 250
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("background child's usage never reached the shared budget");
    }

    /// Regression (HIGH): the child loop used to run with bare NoopHooks,
    /// so `permissions.deny` rules never applied to sub-agents — a parent
    /// blocked by `Bash(curl *)` could simply delegate the same command to
    /// a sub-agent. The probe provider issues a denied bash call, then
    /// reports from the follow-up context whether the tool result carries
    /// the deny verdict (i.e. the command never executed).
    #[tokio::test]
    async fn child_loop_enforces_deny_rules() {
        let tool = super::SubagentTool::new(
            Arc::new(super::subagent_tool_test_support::DenyProbeProvider::default()),
            super::subagent_tool_test_support::test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        )
        .with_deny_rules(crate::permissions::PermissionRules {
            allow: vec![],
            deny: vec![crate::permissions::Rule::parse("Bash(echo probe-bypass*)").unwrap()],
        });
        let (text, details) = tool
            .run(
                json!({ "task": "run the probe command" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(
            text.contains("deny-enforced"),
            "child loop must block the denied command, got: {text}"
        );
        assert_eq!(details["toolCalls"], 1);
    }

    /// No deny rules configured: the child loop keeps its no-op hooks and
    /// the probe command runs (reported back as executed).
    #[tokio::test]
    async fn child_loop_without_deny_rules_runs_tools() {
        let tool = super::SubagentTool::new(
            Arc::new(super::subagent_tool_test_support::DenyProbeProvider::default()),
            super::subagent_tool_test_support::test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        );
        let (text, _) = tool
            .run(
                json!({ "task": "run the probe command" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(text.contains("deny-MISSING"), "{text}");
    }

    // ---- plugin inheritance (subagents.inheritPlugins) ----

    /// Guardrail-plugin stand-in: blocks every bash call with a
    /// distinctive verdict (the needle the probe looks for).
    #[derive(Debug)]
    struct GuardrailHooks;

    #[async_trait::async_trait]
    impl tack_agent_core::AgentHooks for GuardrailHooks {
        async fn before_tool_call(
            &self,
            ctx: &tack_agent_core::hooks::BeforeToolCallContext<'_>,
        ) -> tack_agent_core::hooks::BeforeToolCallOutcome {
            if ctx.tool_name == "bash" {
                return tack_agent_core::hooks::BeforeToolCallOutcome::Block {
                    reason: Some("plugin-guardrail-blocked".into()),
                    terminate: false,
                };
            }
            tack_agent_core::hooks::BeforeToolCallOutcome::Allow
        }
    }

    /// Plugin-tool stand-in (`ext__fake__probe`): answers with a
    /// distinctive output text.
    #[derive(Debug)]
    struct FakeExtTool;

    #[async_trait::async_trait]
    impl tack_agent_core::AgentTool for FakeExtTool {
        fn name(&self) -> &'static str {
            "ext__fake__probe"
        }
        fn label(&self) -> &str {
            "fake"
        }
        fn description(&self) -> &str {
            "fake plugin tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _cancel: CancellationToken,
            _on_update: &(dyn Fn(tack_agent_core::AgentToolResult) + Send + Sync),
        ) -> Result<tack_agent_core::AgentToolResult, String> {
            Ok(tack_agent_core::AgentToolResult::text("fake-tool-output"))
        }
    }

    fn hook_probe_tool(mode: crate::settings::SubagentInheritance) -> super::SubagentTool {
        super::SubagentTool::new(
            Arc::new(
                super::subagent_tool_test_support::ToolResultProbeProvider::new(
                    "bash",
                    json!({ "command": "echo probe-hook" }),
                    "plugin-guardrail-blocked",
                ),
            ),
            super::subagent_tool_test_support::test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        )
        .with_plugin_inheritance(mode)
        .with_extension_hooks(vec![Arc::new(GuardrailHooks)])
    }

    /// Default ("hooks"): a guardrail plugin's beforeToolCall verdict
    /// applies inside the child loop — without inheritance a sub-agent
    /// would run straight past the plugin's deny list.
    #[tokio::test]
    async fn hooks_inheritance_enforces_plugin_hooks_in_child() {
        let tool = hook_probe_tool(crate::settings::SubagentInheritance::Hooks);
        let (text, _) = tool
            .run(
                json!({ "task": "run the probe command" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(text.contains("needle-found"), "{text}");
    }

    /// "none" restores the legacy behavior: plugin hooks never see child
    /// tool calls (the harmless echo actually executes).
    #[tokio::test]
    async fn none_inheritance_skips_plugin_hooks() {
        let tool = hook_probe_tool(crate::settings::SubagentInheritance::None);
        let (text, _) = tool
            .run(
                json!({ "task": "run the probe command" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(text.contains("needle-missing"), "{text}");
    }

    fn tool_probe_tool(mode: crate::settings::SubagentInheritance) -> super::SubagentTool {
        super::SubagentTool::new(
            Arc::new(
                super::subagent_tool_test_support::ToolResultProbeProvider::new(
                    "ext__fake__probe",
                    json!({}),
                    "fake-tool-output",
                ),
            ),
            super::subagent_tool_test_support::test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        )
        .with_plugin_inheritance(mode)
        .with_extension_tools(vec![Arc::new(FakeExtTool)])
    }

    /// "full": plugin tools join the child tool set and execute.
    #[tokio::test]
    async fn full_inheritance_adds_plugin_tools_to_child() {
        let tool = tool_probe_tool(crate::settings::SubagentInheritance::Full);
        let (text, _) = tool
            .run(
                json!({ "task": "call the probe tool" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(text.contains("needle-found"), "{text}");
    }

    /// "hooks": plugin tools are NOT in the child set — the same call
    /// ends in an unknown-tool error result, never the tool's output.
    #[tokio::test]
    async fn hooks_inheritance_excludes_plugin_tools() {
        let tool = tool_probe_tool(crate::settings::SubagentInheritance::Hooks);
        let (text, _) = tool
            .run(
                json!({ "task": "call the probe tool" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(text.contains("needle-missing"), "{text}");
    }

    /// Regression: managed lockedProvider/lockedModel must bind delegated
    /// work too — the subagent `model` param used to resolve any provider,
    /// routing around the org's approved-provider policy.
    #[test]
    fn child_model_lock_rejects_unlocked_override() {
        let agent_dir = tempfile::tempdir().unwrap();
        let parent =
            crate::model::resolve_model("anthropic", Some("claude-haiku-4-5"), agent_dir.path())
                .unwrap();
        let tool = super::SubagentTool::new(
            Arc::new(super::subagent_tool_test_support::NullProvider),
            parent,
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        )
        .with_model_locks(
            Some("anthropic".to_string()),
            Some("claude-haiku-4-5".to_string()),
        );
        // Cross-provider override is rejected, naming the lock.
        let err = tool
            .resolve_child_model(Some("openai/gpt-4.1"), agent_dir.path())
            .unwrap_err();
        assert!(err.contains("locked to anthropic"), "{err}");
        // Same-provider but non-locked model is rejected too.
        let err = tool
            .resolve_child_model(Some("anthropic/claude-sonnet-4-5"), agent_dir.path())
            .unwrap_err();
        assert!(err.contains("locked to claude-haiku-4-5"), "{err}");
        // The exact locked model and the inherited parent model both pass.
        assert!(
            tool.resolve_child_model(Some("anthropic/claude-haiku-4-5"), agent_dir.path())
                .is_ok()
        );
        assert!(tool.resolve_child_model(None, agent_dir.path()).is_ok());
    }

    // ---- SubagentStart / SubagentStop hooks --------------------------

    fn reviewer_agent() -> crate::agents::AgentDefinition {
        crate::agents::AgentDefinition {
            name: "reviewer".into(),
            description: "Reviews code".into(),
            tools: vec![],
            model: None,
            system_prompt: "You review.".into(),
            source: std::path::PathBuf::from("reviewer.md"),
        }
    }

    fn command_group(matcher: Option<&str>, command: &str) -> crate::shell_hooks::HookGroup {
        crate::shell_hooks::HookGroup {
            matcher: matcher.map(str::to_string),
            hooks: vec![crate::shell_hooks::HookHandler::Command {
                command: command.to_string(),
                timeout_sec: Some(10),
                run_async: false,
                status_message: None,
            }],
        }
    }

    fn hook_engine() -> crate::shell_hooks::HookEngine {
        let shell = tack_tools::shell::resolve_shell(None).ok().map(Arc::new);
        crate::shell_hooks::HookEngine::new(shell, std::env::current_dir().unwrap())
    }

    fn start_hook_tool(
        event: tack_ai::AssistantMessageEvent,
        matcher: Option<&str>,
        command: &str,
    ) -> super::SubagentTool {
        make_tool(event).with_start_hooks(vec![command_group(matcher, command)], hook_engine())
    }

    /// A command handler receives the Claude-wire SubagentStart payload on
    /// stdin: event name, session/cwd, agent type/id, task summary.
    #[tokio::test]
    async fn subagent_start_hook_receives_claude_payload() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("input.json");
        let tool = start_hook_tool(
            super::subagent_tool_test_support::done_event("ok", 0),
            None,
            &format!("cat > {}", shell_quote(&capture)),
        );
        let (text, _) = tool
            .run(
                json!({ "task": "inspect the hooks", "description": "hook probe" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(text.contains("ok"), "{text}");
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();
        assert_eq!(payload["hook_event_name"], "SubagentStart");
        assert!(payload["session_id"].is_string());
        assert!(payload["cwd"].is_string());
        assert_eq!(payload["agent_type"], "subagent");
        assert!(payload["agent_id"].is_null());
        assert_eq!(payload["prompt"], "inspect the hooks");
        assert_eq!(payload["description"], "hook probe");
        assert_eq!(payload["background"], false);
    }

    /// Matchers match the sub-agent name: a `reviewer` matcher fires for
    /// `agent: "reviewer"` runs (payload names the custom agent) and is
    /// skipped for default runs.
    #[tokio::test]
    async fn subagent_start_matcher_matches_custom_agent_name() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("input.json");
        let tool = start_hook_tool(
            super::subagent_tool_test_support::done_event("ok", 0),
            Some("reviewer"),
            &format!("cat > {}", shell_quote(&capture)),
        )
        .with_agents(vec![reviewer_agent()]);
        // Default sub-agent: matcher does not hit, hook never runs.
        let _ = tool
            .run(json!({ "task": "plain" }), CancellationToken::new(), None)
            .await
            .unwrap();
        assert!(!capture.exists(), "matcher must skip the default subagent");
        // Custom agent: matcher hits, payload identifies the agent.
        let _ = tool
            .run(
                json!({ "task": "review this", "agent": "reviewer" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();
        assert_eq!(payload["agent_type"], "reviewer");
        assert_eq!(payload["agent_id"], "reviewer");
    }

    /// A block verdict (exit 2) refuses the delegation as an in-band tool
    /// error BEFORE the child loop starts: no tokens are burned.
    #[tokio::test]
    async fn subagent_start_block_refuses_before_start() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let tool = start_hook_tool(
            super::subagent_tool_test_support::done_event("unreachable", 0),
            None,
            "echo policy-violation 1>&2; exit 2",
        );
        let err = tool
            .run(json!({ "task": "x" }), CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.contains("policy-violation"), "{err}");
        assert_eq!(
            tool.limits
                .used_tokens
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a blocked start must not burn any tokens"
        );
    }

    /// A JSON `decision: "block"` verdict refuses the run too (Claude
    /// output-schema path).
    #[tokio::test]
    async fn subagent_start_json_block_refuses_run() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let tool = start_hook_tool(
            super::subagent_tool_test_support::done_event("unreachable", 0),
            None,
            r#"echo '{"decision":"block","reason":"json-nope"}'"#,
        );
        let err = tool
            .run(json!({ "task": "x" }), CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.contains("json-nope"), "{err}");
    }

    /// `additionalContext` from the verdict is folded into the child's task.
    #[tokio::test]
    async fn subagent_start_additional_context_is_folded_into_task() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let provider = Arc::new(super::subagent_tool_test_support::TaskCaptureProvider::default());
        let tool = super::SubagentTool::new(
            provider.clone(),
            super::subagent_tool_test_support::test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        )
        .with_start_hooks(
            vec![command_group(
                None,
                r#"echo '{"hookSpecificOutput":{"additionalContext":"always use serde"}}'"#,
            )],
            hook_engine(),
        );
        let _ = tool
            .run(
                json!({ "task": "build it" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let seen = provider.seen.lock().unwrap().clone().unwrap();
        assert!(seen.contains("build it"), "{seen}");
        assert!(seen.contains("always use serde"), "{seen}");
    }

    /// Background sub-agents gate SYNCHRONOUSLY: a blocking SubagentStart
    /// hook fails the tool call instead of registering a doomed task.
    #[tokio::test]
    async fn background_subagent_start_block_fails_synchronously() {
        use tack_agent_core::AgentTool as _;
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let tool = start_hook_tool(
            super::subagent_tool_test_support::done_event("unreachable", 0),
            None,
            "echo bg-blocked 1>&2; exit 2",
        )
        .with_background(tack_tools::background::BackgroundTaskManager::new());
        let result = tool
            .execute(
                "call-1",
                json!({ "task": "x", "run_in_background": true }),
                CancellationToken::new(),
                &|_| {},
            )
            .await;
        let err = result.unwrap_err();
        assert!(err.contains("bg-blocked"), "{err}");
    }

    /// Allowed background runs still fire the hook (before the tool call
    /// returns) with `background: true` in the payload.
    #[tokio::test]
    async fn background_subagent_fires_start_hook_before_returning() {
        use tack_agent_core::AgentTool as _;
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("input.json");
        let tool = start_hook_tool(
            super::subagent_tool_test_support::done_event("bg done", 0),
            None,
            &format!("cat > {}", shell_quote(&capture)),
        )
        .with_background(tack_tools::background::BackgroundTaskManager::new());
        let result = tool
            .execute(
                "call-1",
                json!({ "task": "x", "run_in_background": true }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = match &result.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("Started background sub-agent task"), "{text}");
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();
        assert_eq!(payload["hook_event_name"], "SubagentStart");
        assert_eq!(payload["background"], true);
    }

    /// SubagentStop matchers are symmetric with SubagentStart: they match
    /// the sub-agent name (previously matchers never fired — the matcher
    /// input was always None).
    #[tokio::test]
    async fn subagent_stop_matcher_matches_agent_name() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("stop.json");
        let tool = make_tool(super::subagent_tool_test_support::done_event("ok", 0))
            .with_stop_hooks(
                vec![command_group(
                    Some("reviewer"),
                    &format!("cat > {}", shell_quote(&capture)),
                )],
                hook_engine(),
            )
            .with_agents(vec![reviewer_agent()]);
        // Default run: matcher misses (fire-and-forget — give the spawned
        // hook a moment to (not) run).
        let _ = tool
            .run(json!({ "task": "plain" }), CancellationToken::new(), None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!capture.exists(), "matcher must skip the default subagent");
        // Custom agent run: matcher hits (poll — the hook is spawned).
        let _ = tool
            .run(
                json!({ "task": "review this", "agent": "reviewer" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        // Poll for the hook payload. `cat > file` creates the file empty
        // before cat writes, so a mere exists() check races the writer
        // (this fails on Windows): keep polling until the contents parse.
        let mut last_raw = String::new();
        for _ in 0..100 {
            if let Ok(raw) = std::fs::read_to_string(&capture) {
                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&raw) {
                    assert_eq!(payload["hook_event_name"], "SubagentStop");
                    assert_eq!(payload["agent_type"], "reviewer");
                    return;
                }
                if !raw.is_empty() {
                    last_raw = raw;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!(
            "SubagentStop hook never produced a valid payload for a matching agent name (last contents: {last_raw:?})"
        );
    }

    #[test]
    fn schema_requires_task() {
        use tack_agent_core::AgentTool as _;
        let agent_dir = tempfile::tempdir().unwrap();
        let model = crate::model::resolve_model("anthropic", Some("k3"), agent_dir.path()).unwrap();
        let tool = super::SubagentTool::new(
            Arc::new(super::subagent_tool_test_support::NullProvider),
            model,
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        );
        let schema = tool.parameters_schema();
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("task"))
        );
    }

    #[test]
    fn schema_lists_custom_agents() {
        use tack_agent_core::AgentTool as _;
        let agent_dir = tempfile::tempdir().unwrap();
        let model = crate::model::resolve_model("anthropic", Some("k3"), agent_dir.path()).unwrap();
        let tool = super::SubagentTool::new(
            Arc::new(super::subagent_tool_test_support::NullProvider),
            model,
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        )
        .with_agents(vec![crate::agents::AgentDefinition {
            name: "reviewer".into(),
            description: "Reviews code".into(),
            tools: vec![],
            model: None,
            system_prompt: "You review.".into(),
            source: std::path::PathBuf::from("reviewer.md"),
        }]);
        let schema = tool.parameters_schema();
        assert!(
            schema["properties"]["agent"]["description"]
                .as_str()
                .unwrap()
                .contains("reviewer: Reviews code")
        );
    }

    #[test]
    fn cross_provider_model_override_resolves_target_api() {
        // Regression: the override used to patch provider/id onto the parent
        // model, keeping the parent's api kind/base_url — the child would
        // call the parent's API with the override's model id.
        let agent_dir = tempfile::tempdir().unwrap();
        let parent =
            crate::model::resolve_model("anthropic", Some("claude-haiku-4-5"), agent_dir.path())
                .unwrap();
        let tool = super::SubagentTool::new(
            Arc::new(super::subagent_tool_test_support::NullProvider),
            parent,
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        );
        let child = tool
            .resolve_child_model(Some("openai/gpt-4.1"), agent_dir.path())
            .unwrap();
        assert_eq!(child.provider, "openai");
        assert_eq!(child.id, "gpt-4.1");
        assert_eq!(
            child.api, "openai-responses",
            "api kind must come from the override"
        );
    }

    #[test]
    fn model_override_unknown_provider_errors() {
        let agent_dir = tempfile::tempdir().unwrap();
        let parent =
            crate::model::resolve_model("anthropic", Some("claude-haiku-4-5"), agent_dir.path())
                .unwrap();
        let tool = super::SubagentTool::new(
            Arc::new(super::subagent_tool_test_support::NullProvider),
            parent,
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        );
        let err = tool
            .resolve_child_model(Some("no-such-provider/x"), agent_dir.path())
            .unwrap_err();
        assert!(err.contains("invalid model override"), "{err}");
        // Malformed override (no provider/id split) also errors.
        assert!(
            tool.resolve_child_model(Some("gpt-4.1"), agent_dir.path())
                .is_err()
        );
    }

    #[tokio::test]
    async fn worktree_create_and_diff() {
        // Set up a repo with one commit.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {:?}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);

        let worktree = super::create_worktree(repo).await.unwrap();
        assert!(worktree.path.join("a.txt").exists());
        // Change a file + add an untracked one, then check the diff stat.
        std::fs::write(worktree.path.join("a.txt"), "two").unwrap();
        std::fs::write(worktree.path.join("new.txt"), "new").unwrap();
        let stat = super::worktree_diff_stat(&worktree).await;
        assert!(stat.contains("a.txt"), "stat: {stat}");
        assert!(stat.contains("new.txt"), "stat: {stat}");

        // Cleanup (not part of the tool contract — the tool keeps worktrees).
        super::git_output(
            repo,
            &[
                "worktree",
                "remove",
                "--force",
                &worktree.path.display().to_string(),
            ],
        )
        .await
        .unwrap();
    }
}

#[cfg(test)]
pub(crate) mod subagent_tool_test_support {
    #![allow(clippy::unwrap_used)]
    use tack_ai::provider::{Provider, StreamOptions};
    use tack_ai::stream::AssistantMessageEventStream;
    use tack_ai::types::{Context, Model};

    /// Provider that immediately finishes (for schema tests only).
    #[derive(Debug)]
    pub struct NullProvider;

    impl Provider for NullProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: &Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            panic!("not used in schema tests")
        }
    }

    /// Provider that answers every request with one fixed terminal event
    /// (Done or Error) carrying a canned assistant message.
    #[derive(Debug)]
    pub struct ScriptProvider {
        pub event: tack_ai::AssistantMessageEvent,
    }

    impl Provider for ScriptProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: &Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let (sender, stream) = tack_ai::stream::event_stream();
            sender.finish(self.event.clone());
            stream
        }
    }

    /// Provider that records the text of the first user message it
    /// receives (the child task), then answers with a fixed Done event.
    #[derive(Debug, Default)]
    pub struct TaskCaptureProvider {
        pub seen: std::sync::Mutex<Option<String>>,
    }

    impl Provider for TaskCaptureProvider {
        fn stream(
            &self,
            _model: &Model,
            context: &Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let (sender, stream) = tack_ai::stream::event_stream();
            let text = context.messages.iter().find_map(|m| match m {
                tack_ai::Message::User(u) => Some(match &u.content {
                    tack_ai::UserContent::Text(text) => text.clone(),
                    tack_ai::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                }),
                _ => None,
            });
            *self.seen.lock().unwrap() = text;
            sender.finish(done_event("captured", 0));
            stream
        }
    }

    /// Deny-rule probe: the first request gets an assistant message with a
    /// `bash` tool call whose command matches the test's deny rule (and is
    /// harmless if it ever ran); the second request inspects the context
    /// for the tool result and reports whether the call was blocked by
    /// `permissions.deny` ("deny-enforced") or executed ("deny-MISSING").
    #[derive(Debug, Default)]
    pub struct DenyProbeProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Provider for DenyProbeProvider {
        fn stream(
            &self,
            _model: &Model,
            context: &Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let (sender, stream) = tack_ai::stream::event_stream();
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let event = if call == 0 {
                let mut message = tack_ai::AssistantMessage::pending(&test_model());
                message.stop_reason = tack_ai::StopReason::ToolUse;
                message.content = vec![tack_ai::ContentBlock::ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": "echo probe-bypass" }),
                    thought_signature: None,
                    namespace: None,
                }];
                tack_ai::AssistantMessageEvent::Done {
                    reason: tack_ai::StopReason::ToolUse,
                    message,
                }
            } else {
                let denied = context.messages.iter().any(|m| match m {
                    tack_ai::Message::ToolResult(tr) => {
                        tr.is_error
                            && tr.content.iter().any(|c| match c {
                                tack_ai::InputContentBlock::Text { text, .. } => {
                                    text.contains("denied by permissions.deny rule")
                                }
                                _ => false,
                            })
                    }
                    _ => false,
                });
                done_event(
                    if denied {
                        "deny-enforced"
                    } else {
                        "deny-MISSING"
                    },
                    0,
                )
            };
            sender.finish(event);
            stream
        }
    }

    /// Hook/tool-inheritance probe: the first request issues a fixed
    /// `tool_name` call; the second request inspects the context for the
    /// tool result and reports "needle-found" when any result text
    /// contains `needle`, else "needle-missing". Generic successor of
    /// `DenyProbeProvider` (a block verdict's reason and a fake tool's
    /// output are both just needles in the tool result).
    #[derive(Debug)]
    pub struct ToolResultProbeProvider {
        pub tool_name: &'static str,
        pub arguments: serde_json::Value,
        pub needle: &'static str,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ToolResultProbeProvider {
        pub fn new(
            tool_name: &'static str,
            arguments: serde_json::Value,
            needle: &'static str,
        ) -> Self {
            ToolResultProbeProvider {
                tool_name,
                arguments,
                needle,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl Provider for ToolResultProbeProvider {
        fn stream(
            &self,
            _model: &Model,
            context: &Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let (sender, stream) = tack_ai::stream::event_stream();
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let event = if call == 0 {
                let mut message = tack_ai::AssistantMessage::pending(&test_model());
                message.stop_reason = tack_ai::StopReason::ToolUse;
                message.content = vec![tack_ai::ContentBlock::ToolCall {
                    id: "call-1".into(),
                    name: self.tool_name.into(),
                    arguments: self.arguments.clone(),
                    thought_signature: None,
                    namespace: None,
                }];
                tack_ai::AssistantMessageEvent::Done {
                    reason: tack_ai::StopReason::ToolUse,
                    message,
                }
            } else {
                let found = context.messages.iter().any(|m| match m {
                    tack_ai::Message::ToolResult(tr) => tr.content.iter().any(|c| match c {
                        tack_ai::InputContentBlock::Text { text, .. } => text.contains(self.needle),
                        _ => false,
                    }),
                    _ => false,
                });
                done_event(
                    if found {
                        "needle-found"
                    } else {
                        "needle-missing"
                    },
                    0,
                )
            };
            sender.finish(event);
            stream
        }
    }

    /// A model whose `api` matches no built-in provider, so
    /// `tack_ai::provider_for` returns None and the sub-agent falls back to
    /// the tool's own (scripted) provider instead of a real API client.
    pub fn test_model() -> Model {
        Model {
            id: "test-model".into(),
            name: "test".into(),
            api: "test-unknown-api".into(),
            provider: "test".into(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![tack_ai::InputKind::Text],
            cost: tack_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 1024,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    /// Terminal event: a successful text answer with `total_tokens` usage.
    pub fn done_event(text: &str, total_tokens: u64) -> tack_ai::AssistantMessageEvent {
        let mut message = tack_ai::AssistantMessage::pending(&test_model());
        message.stop_reason = tack_ai::StopReason::Stop;
        message.usage.total_tokens = total_tokens;
        message.content = vec![tack_ai::ContentBlock::Text {
            text: text.to_string(),
            text_signature: None,
        }];
        tack_ai::AssistantMessageEvent::Done {
            reason: tack_ai::StopReason::Stop,
            message,
        }
    }

    /// Terminal event: a provider error after burning `total_tokens`.
    pub fn error_event(error: &str, total_tokens: u64) -> tack_ai::AssistantMessageEvent {
        let mut message = tack_ai::AssistantMessage::pending(&test_model());
        message.stop_reason = tack_ai::StopReason::Error;
        message.error_message = Some(error.to_string());
        message.usage.total_tokens = total_tokens;
        tack_ai::AssistantMessageEvent::Error {
            reason: tack_ai::StopReason::Error,
            error: message,
        }
    }
}
