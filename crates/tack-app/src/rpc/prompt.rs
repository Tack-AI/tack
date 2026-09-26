use std::sync::Arc;

use serde_json::{Value, json};
use tack_agent_core::{
    AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentMessage, HooksChain,
    ToolExecutionMode, agent_loop,
};
use tack_ai::Provider;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::settings::Settings;

use super::RpcState;
use super::events::event_to_json;

/// Spawn a prompt run; events are written to stdout as JSON lines by a
/// dedicated forwarder task.
pub(crate) async fn spawn_prompt(
    state: &Arc<Mutex<RpcState>>,
    provider: &Arc<dyn Provider>,
    settings: &Settings,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    prompts: Vec<String>,
    images: Vec<(String, String)>,
    _continue_run: bool,
) {
    let (
        model,
        thinking,
        cancel,
        retry_cancel,
        existing,
        cwd,
        session_id,
        background,
        lsp,
        generation,
    ) = {
        let mut state = state.lock().await;
        state.is_streaming = true;
        state.cancel = tokio_util::sync::CancellationToken::new();
        state.retry_cancel = tokio_util::sync::CancellationToken::new();
        state.generation += 1;
        (
            state.model.clone(),
            state.thinking,
            state.cancel.clone(),
            state.retry_cancel.clone(),
            state.session.build_session_context().messages,
            state.session.cwd().to_path_buf(),
            state.session.session_id().to_string(),
            state.background.clone(),
            state.lsp.clone(),
            state.generation,
        )
    };

    let agent_dir = tack_session::default_agent_dir();
    // tack-ext plugin contributions, collected once per prompt run.
    let (ext_tools, ext_hooks, ext_mcp_servers, ext_bundle_hooks) = {
        let state_guard = state.lock().await;
        let extensions = state_guard.extensions.lock().await;
        (
            extensions.tools(),
            extensions.hooks(),
            extensions.bundle_mcp_servers.clone(),
            extensions.bundle_hooks.clone(),
        )
    };
    let shell_config;
    let tools = {
        let checkpoints = tack_tools::checkpoint::CheckpointManager::new();
        if settings.features.checkpoints {
            checkpoints.enable(agent_dir.join("checkpoints").join(&session_id));
            checkpoints.set_workdir(cwd.clone());
            checkpoints.begin_turn();
        }
        let mut services = tack_tools::default_services(cwd.clone())
            .with_background(background)
            .with_lsp(lsp)
            .with_checkpoints(checkpoints);
        if let Some(spec) = settings.sandbox_spec(&cwd) {
            services = services.with_sandbox(spec);
        }
        services = services
            .with_web_render(settings.web_render_mode())
            .with_web_search(settings.web_search_config())
            .with_background_tasks_enabled(settings.features.background_tasks)
            .with_memory_dir(settings.memory_directory.clone());
        shell_config = services.shell.clone();
        let mut tools = tack_tools::create_coding_tools(&services);
        tools.push(Arc::new(
            crate::session_search_tool::SessionSearchTool::new(agent_dir.clone()),
        ));
        // tack-subagents: built-in parallel sub-agent tool (+ custom agents).
        {
            let deny_rules = crate::permissions::PermissionRules::load(settings, &agent_dir);
            let state_guard = state.lock().await;
            let mut subagent_tool = crate::subagent_tool::SubagentTool::new(
                provider.clone(),
                model.clone(),
                auth.clone(),
            )
            .with_agents(crate::agents::load_agents(
                &cwd,
                &agent_dir,
                crate::project_trust::is_trusted(&cwd, &agent_dir),
            ))
            .with_cwd(cwd.clone())
            .with_features(settings.features.clone())
            .with_deny_rules(deny_rules)
            .with_memory_dir(settings.memory_directory.clone())
            .with_cache_retention(settings.cache_retention_mode())
            .with_model_locks(
                settings.locked_provider.clone(),
                settings.locked_model.clone(),
            )
            .with_background(state_guard.background.clone())
            .with_shared_limits(state_guard.subagent_limits.clone());
            if settings.features.shell_hooks {
                let cfg = crate::shell_hooks::load_hooks_config(settings, &agent_dir);
                let start_groups = cfg.take_groups(crate::shell_hooks::HookEvent::SubagentStart);
                let stop_groups = cfg.take_groups(crate::shell_hooks::HookEvent::SubagentStop);
                if !start_groups.is_empty() || !stop_groups.is_empty() {
                    let engine =
                        crate::shell_hooks::HookEngine::new(shell_config.clone(), cwd.clone())
                            .with_evaluator(Arc::new(crate::shell_hooks::LlmEvaluator {
                                model: model.clone(),
                                auth: auth.clone(),
                                agent_dir: agent_dir.clone(),
                                cwd: cwd.clone(),
                            }));
                    subagent_tool = subagent_tool
                        .with_start_hooks(start_groups, engine.clone())
                        .with_stop_hooks(stop_groups, engine);
                }
            }
            tools.push(Arc::new(subagent_tool));
        }
        // MCP connection pool: shared with get_mcp_status via
        // rpc::mcp::ensure_mcp_pool (session-scoped specs from
        // set_mcp_servers are merged in there; fingerprint-keyed cache,
        // rebuild on config/model/token change, dead-connection eviction).
        let connections = super::mcp::ensure_mcp_pool(
            state,
            settings,
            &crate::mcp_config::SamplingLlm {
                provider: provider.clone(),
                model: model.clone(),
                auth: auth.clone(),
            },
            &cwd,
            &agent_dir,
            ext_mcp_servers,
        )
        .await
        .connections;
        // Tool SNAPSHOTS are rebuilt every prompt (cheap; they borrow the
        // reused connections) so tool-list/config changes take effect.
        tools.extend(tack_tools::mcp::mcp_tools_with(
            &connections,
            Some(services.untrusted_seen.clone()),
        ));
        // tack-ext plugin tools (ext__<plugin>__<tool>).
        tools.extend(ext_tools);
        crate::cli_flags::filter_feature_tools(tools, &settings.features)
    };
    let (tools, tool_pool) =
        crate::cli_flags::split_for_tool_search(tools, settings.mcp_defer_threshold);
    let selected_tools: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();

    // settings.json hooks + extension bundle hooks + host session hooks
    // (set_hooks; gated by the same shellHooks feature flag — a host must
    // not smuggle shell commands past a user who disabled shell hooks).
    let mut hooks_cfg = if settings.features.shell_hooks {
        let mut cfg = crate::shell_hooks::load_hooks_config(settings, &agent_dir);
        cfg.extend(state.lock().await.session_hooks.clone());
        cfg
    } else {
        crate::shell_hooks::HookConfig::default()
    };
    hooks_cfg.extend(ext_bundle_hooks);
    let session_start_groups = hooks_cfg.take_groups(crate::shell_hooks::HookEvent::SessionStart);
    let prompt_submit_groups =
        hooks_cfg.take_groups(crate::shell_hooks::HookEvent::UserPromptSubmit);
    let stop_groups = hooks_cfg.take_groups(crate::shell_hooks::HookEvent::Stop);
    let pre_groups = hooks_cfg.take_groups(crate::shell_hooks::HookEvent::PreToolUse);
    let post_groups = hooks_cfg.take_groups(crate::shell_hooks::HookEvent::PostToolUse);
    let post_failure_groups =
        hooks_cfg.take_groups(crate::shell_hooks::HookEvent::PostToolUseFailure);
    let any_hook_groups = [
        &session_start_groups,
        &prompt_submit_groups,
        &stop_groups,
        &pre_groups,
        &post_groups,
        &post_failure_groups,
    ]
    .iter()
    .any(|groups| !groups.is_empty());
    let hook_engine = any_hook_groups.then(|| {
        crate::shell_hooks::HookEngine::new(shell_config.clone(), cwd.clone()).with_evaluator(
            Arc::new(crate::shell_hooks::LlmEvaluator {
                model: model.clone(),
                auth: auth.clone(),
                agent_dir: agent_dir.clone(),
                cwd: cwd.clone(),
            }),
        )
    });

    // SessionStart hooks fire lazily at the first prompt of a session (see
    // RpcState::session_start_hook_pending): their additionalContext joins
    // the session-scoped context appended to every system prompt.
    if let (Some(engine), false) = (&hook_engine, session_start_groups.is_empty()) {
        let pending = {
            let st = state.lock().await;
            st.session_start_hook_pending
        };
        if pending {
            let (source, session_id_for_hook) = {
                let mut st = state.lock().await;
                st.session_start_hook_pending = false;
                (
                    st.session_start_hook_source.clone(),
                    st.session.session_id().to_string(),
                )
            };
            let verdict = engine
                .run(
                    &session_start_groups,
                    None,
                    &json!({
                        "session_id": session_id_for_hook,
                        "transcript_path": serde_json::Value::Null,
                        "cwd": cwd,
                        "hook_event_name": "SessionStart",
                        "model": model.id,
                        "permission_mode": "bypass",
                        "source": source,
                    }),
                )
                .await;
            if !verdict.additional_context.is_empty() {
                let mut st = state.lock().await;
                let context = st.session_hook_context.get_or_insert_with(String::new);
                for text in &verdict.additional_context {
                    context.push_str(text);
                    context.push('\n');
                }
            }
            emit_hook_notices("SessionStart", &verdict).await;
        }
    }

    let mut system_prompt = crate::print_mode::assemble_system_prompt(
        &cwd,
        &agent_dir,
        settings,
        None,
        &selected_tools,
        &crate::cli_flags::CliFlags::default(),
    );
    {
        let st = state.lock().await;
        if let Some(context) = &st.session_hook_context
            && !context.trim().is_empty()
        {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(context.trim_end());
        }
    }

    // UserPromptSubmit hooks: a block verdict drops the prompt (synthetic
    // agent_end unblocks the client); additionalContext is prepended.
    let mut prompts = prompts;
    if let (Some(engine), false, Some(first)) = (
        &hook_engine,
        prompt_submit_groups.is_empty(),
        prompts.first(),
    ) {
        let verdict = engine
            .run(
                &prompt_submit_groups,
                None,
                &json!({
                    "session_id": session_id,
                    "transcript_path": serde_json::Value::Null,
                    "cwd": cwd,
                    "hook_event_name": "UserPromptSubmit",
                    "model": model.id,
                    "permission_mode": "bypass",
                    "prompt": first,
                }),
            )
            .await;
        if let Some(reason) = &verdict.blocked {
            emit_hook_notice(&json!({
                "type": "hook_notice",
                "hookEvent": "UserPromptSubmit",
                "kind": "block",
                "message": reason,
            }))
            .await;
            // Mirror the panic path: a synthetic terminal event so clients
            // waiting on this run's agent_end do not hang.
            emit_hook_notice(&json!({ "type": "agent_end" })).await;
            state.lock().await.is_streaming = false;
            return;
        }
        if !verdict.additional_context.is_empty() {
            prompts[0] = format!(
                "<hook_additional_context>\n{}\n</hook_additional_context>\n\n{}",
                verdict.additional_context.join("\n"),
                prompts[0]
            );
        }
        emit_hook_notices("UserPromptSubmit", &verdict).await;
    }

    let hooks: Arc<dyn AgentHooks> = {
        // Compaction against the live session + steering/follow-up queues.
        let mut hook_list: Vec<Arc<dyn AgentHooks>> = vec![
            Arc::new(RpcCompactionHooks {
                state: state.clone(),
                model: model.clone(),
                provider: provider.clone(),
                auth: auth.clone(),
                reasoning: thinking,
                settings: settings.compaction,
                cancel: cancel.clone(),
                generation,
            }),
            Arc::new(RpcQueueHooks {
                state: state.clone(),
            }),
        ];
        // settings.json + extension bundle + host session hooks:
        // PreToolUse/PostToolUse/PostToolUseFailure shell commands inside
        // the agent loop's tool-call gates.
        if let Some(engine) = &hook_engine
            && (!pre_groups.is_empty()
                || !post_groups.is_empty()
                || !post_failure_groups.is_empty())
        {
            hook_list.push(Arc::new(
                crate::shell_hooks::ShellHooks::new(
                    engine.clone(),
                    pre_groups,
                    post_groups,
                    crate::shell_hooks::HookSessionInfo {
                        session_id: session_id.clone(),
                        model: model.id.clone(),
                        permission_mode: "bypass".to_string(),
                    },
                    crate::shell_hooks::HookDecisions::default(),
                )
                .with_post_failure(post_failure_groups),
            ));
        }
        // permissions.deny applies headless too (CI safety net).
        let deny_rules = crate::permissions::PermissionRules::load(settings, &agent_dir);
        if !deny_rules.deny.is_empty() {
            hook_list.push(Arc::new(crate::permissions::DenyRulesHooks {
                rules: deny_rules.clone(),
            }));
        }
        // Interactive permission prompts (set_mode ask|acceptEdits|plan): gate
        // non-read-only tools behind a client answer; bypass short-circuits.
        {
            let state_guard = state.lock().await;
            let permission_state = state_guard.permission.clone();
            let event_sink = state_guard.event_sink.clone();
            let session_id = session_id.clone();
            hook_list.push(super::permission::rpc_permission_hooks(
                session_id,
                permission_state,
                deny_rules,
                event_sink,
                cancel.clone(),
            ));
        }
        // tack-ext plugin hooks see the final arguments after every other
        // hook in the chain.
        hook_list.extend(ext_hooks);
        Arc::new(HooksChain::new(hook_list))
    };

    // Stop hooks run in the pump after the terminal event. MVP: verdicts
    // surface as hook_notice events; Claude's block-to-continue semantics
    // are not yet wired in RPC mode (would need a client-visible second
    // turn without a user prompt). Computed before AgentLoopConfig moves
    // session_id/model/cwd.
    let stop_plan = match (&hook_engine, stop_groups.is_empty()) {
        (Some(engine), false) => Some((
            engine.clone(),
            stop_groups,
            session_id.clone(),
            model.id.clone(),
            cwd.clone(),
        )),
        _ => None,
    };
    let config = AgentLoopConfig {
        fallback_models: crate::model::resolve_fallback_models(
            &settings.fallback_models,
            &model,
            &agent_dir,
        ),
        model,
        provider: provider.clone(),
        hooks,
        tool_execution: ToolExecutionMode::Parallel,
        reasoning: thinking,
        auth,
        max_tokens: None,
        temperature: None,
        session_id: Some(session_id),
        cache_retention: settings.cache_retention_mode(),
        tool_pool,
        retry_cancel: Some(retry_cancel),
    };
    let context = AgentContext {
        system_prompt: Some(system_prompt),
        messages: existing,
        tools,
    };
    let prompt_messages: Vec<AgentMessage> = prompts
        .into_iter()
        .enumerate()
        .map(|(index, prompt)| {
            if index == 0 && !images.is_empty() {
                // Attached images ride with the first text block (TS pi
                // image+text user message shape).
                let mut blocks: Vec<tack_ai::InputContentBlock> = images
                    .iter()
                    .map(|(data, mime_type)| tack_ai::InputContentBlock::Image {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    })
                    .collect();
                blocks.push(tack_ai::InputContentBlock::text(prompt));
                AgentMessage::user(tack_ai::UserContent::Blocks(blocks))
            } else {
                AgentMessage::user(prompt)
            }
        })
        .collect();

    let mut stream = agent_loop(prompt_messages, context, config, cancel);
    let state = state.clone();
    tokio::spawn(async move {
        use futures_util::FutureExt as _;

        let pump = async {
            let mut stdout = tokio::io::stdout();
            while let Some(event) = stream.next().await {
                // Persist completed messages — only while this run's session is
                // still current (new_session/clone/switch_session bump the
                // generation; a cancelled run must not append to the new file).
                if let AgentEvent::MessageEnd { message } = &event
                    && !matches!(message, AgentMessage::Custom(_))
                {
                    let mut state = state.lock().await;
                    if state.generation == generation
                        && let Err(e) = state.session.append_message(message.clone())
                    {
                        tracing::warn!("persist failed: {e}");
                    }
                }
                let line = event_to_json(&event);
                // One write per record (line + newline together): the command
                // loop writes responses concurrently and two write calls could
                // interleave mid-record, corrupting the JSONL framing.
                if stdout
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
                let _ = stdout.flush().await;
                if event.is_terminal() {
                    break;
                }
            }
            let _ = stream.result().await;
            if let Some((engine, groups, session_id, model_id, cwd)) = stop_plan {
                let (last_text, totals, stop_hook_active) = {
                    let st = state.lock().await;
                    let last_text = st
                        .session
                        .build_session_context()
                        .messages
                        .iter()
                        .rev()
                        .find_map(|m| match m {
                            AgentMessage::Assistant(a) => {
                                let text = a.text();
                                (!text.is_empty()).then_some(text)
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    (last_text, st.session.session_totals(), st.stop_hook_active)
                };
                let verdict = engine
                    .run(
                        &groups,
                        None,
                        &json!({
                            "session_id": session_id,
                            "transcript_path": serde_json::Value::Null,
                            "cwd": cwd,
                            "hook_event_name": "Stop",
                            "model": model_id,
                            "permission_mode": "bypass",
                            "stop_hook_active": stop_hook_active,
                            "last_assistant_message": last_text,
                            "totalTokens": totals.total_tokens,
                            "totalCost": totals.cost.total,
                        }),
                    )
                    .await;
                if let Some(reason) = &verdict.blocked {
                    emit_hook_notice(&json!({
                        "type": "hook_notice",
                        "hookEvent": "Stop",
                        "kind": "block",
                        "message": reason,
                    }))
                    .await;
                }
                emit_hook_notices("Stop", &verdict).await;
            }
        };
        if std::panic::AssertUnwindSafe(pump)
            .catch_unwind()
            .await
            .is_err()
        {
            // The panic hook logged details. Clients block on this run's
            // terminal event — emit a synthetic agent_end so they unblock
            // instead of hanging on the dead stream.
            tracing::error!("rpc agent pump panicked; details in crash.log");
            let mut stdout = tokio::io::stdout();
            let _ = stdout.write_all(b"{\"type\":\"agent_end\"}\n").await;
            let _ = stdout.flush().await;
        }
        let mut state = state.lock().await;
        if state.generation == generation {
            state.is_streaming = false;
        }
    });
}

/// Write one out-of-band event line to stdout (same one-write-per-record
/// discipline as the pump / command loop).
async fn emit_hook_notice(value: &Value) {
    let mut stdout = tokio::io::stdout();
    let _ = stdout.write_all(format!("{value}\n").as_bytes()).await;
    let _ = stdout.flush().await;
}

/// Surface hook systemMessages as hook_notice events (block verdicts are
/// handled by the caller with a dedicated kind).
async fn emit_hook_notices(event: &str, verdict: &crate::shell_hooks::HookVerdict) {
    for message in &verdict.system_messages {
        emit_hook_notice(&json!({
            "type": "hook_notice",
            "hookEvent": event,
            "kind": "message",
            "message": message,
        }))
        .await;
    }
}

struct RpcQueueHooks {
    state: Arc<Mutex<RpcState>>,
}

impl std::fmt::Debug for RpcQueueHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcQueueHooks").finish()
    }
}

#[async_trait::async_trait]
impl AgentHooks for RpcQueueHooks {
    async fn steering_messages(&self) -> Vec<AgentMessage> {
        let mut state = self.state.lock().await;
        if state.steering_mode.as_deref() == Some("one-at-a-time") {
            state
                .steering
                .pop_front()
                .into_iter()
                .map(AgentMessage::user)
                .collect()
        } else {
            state.steering.drain(..).map(AgentMessage::user).collect()
        }
    }
    async fn follow_up_messages(&self) -> Vec<AgentMessage> {
        let mut state = self.state.lock().await;
        if state.follow_up_mode.as_deref() == Some("one-at-a-time") {
            state
                .follow_up
                .pop_front()
                .into_iter()
                .map(AgentMessage::user)
                .collect()
        } else {
            state.follow_up.drain(..).map(AgentMessage::user).collect()
        }
    }
}

/// Compaction hooks operating on the shared RpcState session.
struct RpcCompactionHooks {
    state: Arc<Mutex<RpcState>>,
    model: tack_ai::Model,
    provider: Arc<dyn Provider>,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    reasoning: Option<tack_ai::ThinkingLevel>,
    settings: tack_session::CompactionSettings,
    cancel: tokio_util::sync::CancellationToken,
    /// Run generation (see RpcState::generation).
    generation: u64,
}

impl std::fmt::Debug for RpcCompactionHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcCompactionHooks").finish()
    }
}

impl RpcCompactionHooks {
    /// The compaction core shared by threshold (transform_context) and
    /// overflow (compact_for_overflow) triggers: snapshot → prepare →
    /// summarize → persist → rebuild the post-compaction context. `None`
    /// on any failure or when the session was swapped mid-flight.
    async fn run_compaction(&self) -> Option<Vec<AgentMessage>> {
        // Snapshot under the lock, then DROP it: compaction is a full LLM
        // call and holding the state lock across it would block every
        // command (abort included) for the duration.
        let path = {
            let state = self.state.lock().await;
            if state.generation != self.generation {
                // The session was swapped mid-run; never write a compaction
                // entry into the replacement session.
                return None;
            }
            state.session.build_session_path()
        };
        let preparation = tack_session::prepare_compaction(&path, &self.settings)?;
        let auth = match self.auth.resolve().await {
            Ok(auth) => auth,
            Err(e) => {
                tracing::warn!("compaction auth resolution failed: {e}");
                return None;
            }
        };
        let result = match tack_session::compact(
            &preparation,
            &self.model,
            &self.provider,
            &auth,
            None,
            self.reasoning,
            None,
            &self.cancel,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("compaction failed: {e}");
                return None;
            }
        };
        let kept = &path[path
            .iter()
            .position(|e| e.id() == result.first_kept_entry_id)
            .unwrap_or(path.len())..];
        let retained_tail: Vec<AgentMessage> = kept
            .iter()
            .flat_map(tack_session::session_entry_to_context_messages)
            .collect();
        let mut state = self.state.lock().await;
        if state.generation != self.generation {
            // Swapped while the compaction call was in flight; discard it.
            return None;
        }
        if let Err(e) = state.session.append_compaction(
            &result.summary,
            Some(result.first_kept_entry_id.clone()),
            result.tokens_before,
            Some(retained_tail),
            Some(result.details.clone()),
            Some(result.usage.clone()),
        ) {
            tracing::warn!("failed to persist compaction: {e}");
            return None;
        }
        let leaf = state.session.leaf_id().map(str::to_string);
        Some(
            tack_session::build_session_context(&state.session.entries(), leaf.as_deref()).messages,
        )
    }
}

#[async_trait::async_trait]
impl AgentHooks for RpcCompactionHooks {
    async fn transform_context(&self, messages: &[AgentMessage]) -> Option<Vec<AgentMessage>> {
        let estimate = tack_session::estimate_context_tokens(messages);
        if !tack_session::should_compact(
            estimate.tokens,
            self.model.context_window as u64,
            &self.settings,
        ) {
            return None;
        }
        self.run_compaction().await
    }

    /// Overflow compact-and-retry (upstream agent-session `_checkCompaction`
    /// case 1): the provider's overflow error is the trigger, so the
    /// token-threshold gate is skipped. Marks the session compacting (same
    /// guard as manual /compact, TS #9178) for the duration.
    async fn compact_for_overflow(&self) -> Option<Vec<AgentMessage>> {
        if !self.settings.enabled {
            return None;
        }
        {
            let mut state = self.state.lock().await;
            if state.generation != self.generation || state.is_compacting {
                return None;
            }
            state.is_compacting = true;
        }
        let rebuilt = self.run_compaction().await;
        self.state.lock().await.is_compacting = false;
        rebuilt
    }
}

pub(crate) async fn run_compact_command(
    state: &Arc<Mutex<RpcState>>,
    provider: &Arc<dyn Provider>,
    settings: &Settings,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    custom: Option<String>,
) -> Result<Value, String> {
    // Snapshot under the lock, compact WITHOUT it (a full LLM call — other
    // commands, abort included, must keep working), then re-lock to persist.
    // Mark the session as compacting so tree navigation (fork) is rejected
    // until the result lands or is discarded (TS pi #9178).
    let snapshot = {
        let mut state = state.lock().await;
        if state.is_compacting {
            return Err("a compaction is already in progress".to_string());
        }
        state.is_compacting = true;
        CompactSnapshot {
            path: state.session.build_session_path(),
            model: state.model.clone(),
            thinking: state.thinking,
            session_id: state.session.session_id().to_string(),
            generation: state.generation,
        }
    };
    // Every exit path below must clear the flag.
    let result = run_compact_inner(state, provider, settings, auth, custom, snapshot).await;
    state.lock().await.is_compacting = false;
    result
}

/// Everything snapshotted under the lock before the compaction LLM call.
struct CompactSnapshot {
    path: Vec<tack_session::SessionEntry>,
    model: tack_ai::Model,
    thinking: Option<tack_ai::ThinkingLevel>,
    session_id: String,
    generation: u64,
}

async fn run_compact_inner(
    state: &Arc<Mutex<RpcState>>,
    provider: &Arc<dyn Provider>,
    settings: &Settings,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    custom: Option<String>,
    snapshot: CompactSnapshot,
) -> Result<Value, String> {
    let CompactSnapshot {
        path,
        model,
        thinking,
        session_id,
        generation,
    } = snapshot;
    let Some(preparation) = tack_session::prepare_compaction(&path, &settings.compaction) else {
        return Err("nothing to compact".to_string());
    };
    let resolved = auth.resolve().await?;
    let result = tack_session::compact(
        &preparation,
        &model,
        provider,
        &resolved,
        custom.as_deref(),
        thinking,
        Some(&session_id),
        &tokio_util::sync::CancellationToken::new(),
    )
    .await?;
    let kept = &path[path
        .iter()
        .position(|e| e.id() == result.first_kept_entry_id)
        .unwrap_or(path.len())..];
    let retained_tail: Vec<AgentMessage> = kept
        .iter()
        .flat_map(tack_session::session_entry_to_context_messages)
        .collect();
    let mut state = state.lock().await;
    if state.generation != generation {
        return Err("session changed while compacting; discarding the result".to_string());
    }
    state
        .session
        .append_compaction(
            &result.summary,
            Some(result.first_kept_entry_id.clone()),
            result.tokens_before,
            Some(retained_tail),
            Some(result.details.clone()),
            Some(result.usage.clone()),
        )
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "summary": result.summary,
        "firstKeptEntryId": result.first_kept_entry_id,
        "tokensBefore": result.tokens_before,
    }))
}

pub(crate) async fn run_bash_command(
    state: &Arc<Mutex<RpcState>>,
    command: String,
    exclude_from_context: bool,
) -> Result<Value, String> {
    let (cwd, cancel) = {
        let mut state = state.lock().await;
        state.bash_cancel = tokio_util::sync::CancellationToken::new();
        (state.session.cwd().to_path_buf(), state.bash_cancel.clone())
    };
    let services = tack_tools::default_services(cwd);
    let bash = tack_tools::BashTool::new(services);
    use tack_agent_core::AgentTool;
    let result = bash
        .execute("rpc-bash", json!({ "command": command }), cancel, &|_| {})
        .await;
    let (output, is_error) = match result {
        Ok(r) => (
            r.content
                .iter()
                .filter_map(|b| match b {
                    tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            false,
        ),
        Err(e) => (e, true),
    };
    // Record as a bashExecution message (pi's ! command semantics).
    let mut state = state.lock().await;
    let message = AgentMessage::BashExecution(tack_agent_core::BashExecutionMessage {
        command,
        output: output.clone(),
        exit_code: if is_error { Some(1) } else { Some(0) },
        cancelled: false,
        truncated: false,
        full_output_path: None,
        exclude_from_context: Some(exclude_from_context),
        timestamp: tack_ai::now_millis(),
    });
    if let Err(e) = state.session.append_message(message) {
        tracing::warn!("bash message persist failed: {e}");
    }
    Ok(
        json!({ "output": output, "exitCode": if is_error { 1 } else { 0 }, "cancelled": false, "isError": is_error }),
    )
}
