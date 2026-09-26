//! MCP server mode: expose the tack agent to OTHER MCP clients (agents,
//! IDEs, orchestrators) — the inverse of the built-in MCP client.
//! `tack mcp-serve` speaks MCP over stdio. Tools:
//!   prompt(text)            — run the full coding agent headlessly, returns
//!                             the final assistant text (context persists
//!                             across calls for the server's lifetime)
//!   get_session_stats()     — accumulated token/cost totals
//!   reset_session()         — drop the accumulated context

use std::sync::Arc;

use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use tack_agent_core::{
    AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentMessage, ToolExecutionMode,
    agent_loop,
};
use tack_ai::Provider;
use tokio::sync::Mutex;

/// A no-op hooks impl (headless child loop, no UI).
#[derive(Debug)]
struct NoopHooks;

#[async_trait::async_trait]
impl AgentHooks for NoopHooks {}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct PromptRequest {
    /// The prompt to run (the agent has the full coding tool set)
    text: String,
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ReadContextRequest {
    /// How many recent messages to include (default 10, max 100)
    max_messages: Option<usize>,
}

struct SharedState {
    messages: Vec<AgentMessage>,
    /// Accumulated usage across prompt calls (input/output/cache/cost).
    usage: tack_ai::Usage,
}

/// Hard cap on the retained conversation history. Beyond it the oldest
/// messages are dropped (with a log line) instead of running a full LLM
/// compaction: this server is a thin headless embedding and the truncation
/// is a deliberate memory bound, not a summarization feature — callers
/// wanting compaction should use reset_session() between tasks.
const MAX_HISTORY_MESSAGES: usize = 200;

/// Drop the oldest history beyond MAX_HISTORY_MESSAGES. Cheap truncation
/// (not LLM compaction — see the constant's docs).
fn truncate_history(messages: &mut Vec<AgentMessage>) {
    if messages.len() <= MAX_HISTORY_MESSAGES {
        return;
    }
    let mut dropped = messages.len() - MAX_HISTORY_MESSAGES;
    // Advance to a User-message boundary. Cutting anywhere else leaves an
    // orphaned tool_result behind (providers reject it — "unexpected
    // tool_use_id") and makes the first retained message an
    // assistant/tool message (also rejected); user boundaries split
    // between exchanges, so tool_use/tool_result pairs stay whole.
    while dropped < messages.len() && !matches!(messages[dropped], AgentMessage::User(_)) {
        dropped += 1;
    }
    // One exchange bigger than the whole cap: drop everything rather
    // than send a guaranteed-invalid history — the next prompt starts
    // fresh instead of failing until the orphans wash out.
    let dropped = dropped.min(messages.len());
    messages.drain(..dropped);
    tracing::info!(
        dropped,
        "mcp-serve: history over the {MAX_HISTORY_MESSAGES}-message cap; oldest dropped"
    );
}

#[derive(Clone)]
pub struct TackMcpServer {
    #[allow(dead_code)] // held so the router lives as long as the server
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    model: tack_ai::Model,
    provider: Arc<dyn Provider>,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    settings: Arc<crate::settings::Settings>,
    state: Arc<Mutex<SharedState>>,
    /// Serializes prompt runs: without it, concurrent prompt() calls each
    /// clone the same baseline history and append disjoint results,
    /// silently forking the conversation. The second caller WAITS here
    /// instead of running against a stale baseline.
    run_lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for TackMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TackMcpServer").finish_non_exhaustive()
    }
}

#[tool_router]
impl TackMcpServer {
    #[tool(
        description = "Run a prompt through the tack coding agent (full tool set: read/edit/write/bash/lsp/etc). Context persists across calls."
    )]
    async fn prompt(
        &self,
        rmcp::handler::server::wrapper::Parameters(request): rmcp::handler::server::wrapper::Parameters<PromptRequest>,
    ) -> String {
        self.run_prompt(request.text).await
    }

    #[tool(description = "Token usage and cost totals accumulated by this MCP server's session")]
    async fn get_session_stats(&self) -> String {
        let state = self.state.lock().await;
        serde_json::json!({
            "inputTokens": state.usage.input,
            "outputTokens": state.usage.output,
            "cacheReadTokens": state.usage.cache_read,
            "cacheWriteTokens": state.usage.cache_write,
            "totalCost": state.usage.cost.total,
            "messages": state.messages.len(),
        })
        .to_string()
    }

    #[tool(description = "Reset the accumulated conversation context")]
    async fn reset_session(&self) -> String {
        let mut state = self.state.lock().await;
        state.messages.clear();
        state.usage = tack_ai::Usage::zero();
        "session reset".to_string()
    }

    #[tool(description = "Read the recent conversation context (last N messages, summarized)")]
    async fn read_context(
        &self,
        rmcp::handler::server::wrapper::Parameters(request): rmcp::handler::server::wrapper::Parameters<ReadContextRequest>,
    ) -> String {
        let state = self.state.lock().await;
        let max = request.max_messages.unwrap_or(10).clamp(1, 100);
        let mut out = String::new();
        let start = state.messages.len().saturating_sub(max);
        for message in &state.messages[start..] {
            match message {
                AgentMessage::User(u) => {
                    let text = match &u.content {
                        tack_ai::UserContent::Text(t) => t.clone(),
                        tack_ai::UserContent::Blocks(_) => "[blocks]".to_string(),
                    };
                    let preview: String = text.chars().take(500).collect();
                    out.push_str(&format!("[user] {preview}\n"));
                }
                AgentMessage::Assistant(a) => {
                    let text: String = a.text().chars().take(500).collect();
                    let tools: Vec<String> = a
                        .tool_calls()
                        .map(|(_, name, _)| name.to_string())
                        .collect();
                    out.push_str(&format!(
                        "[assistant] {} tools={:?} ({} tokens)\n",
                        if text.is_empty() {
                            "(tool calls)".to_string()
                        } else {
                            text
                        },
                        tools,
                        a.usage.total_tokens
                    ));
                }
                AgentMessage::ToolResult(t) => {
                    out.push_str(&format!(
                        "[tool:{}] {} chars{}\n",
                        t.tool_name,
                        t.content
                            .iter()
                            .map(|b| match b {
                                tack_ai::InputContentBlock::Text { text, .. } => text.len(),
                                _ => 0,
                            })
                            .sum::<usize>(),
                        if t.is_error { " (error)" } else { "" }
                    ));
                }
                _ => {}
            }
        }
        if out.is_empty() {
            "(empty context)".to_string()
        } else {
            out
        }
    }

    #[tool(description = "List the agent's available tools (built-ins + MCP)")]
    async fn list_available_tools(&self) -> String {
        let cwd = std::env::current_dir().unwrap_or_default();
        let services = tack_tools::default_services(cwd);
        let tools = tack_tools::create_all_tools(&services);
        tools
            .iter()
            .map(|t| format!("{}: {}", t.name(), t.description()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[tool_handler]
impl ServerHandler for TackMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("tack coding agent (MCP server mode)")
    }
}

impl TackMcpServer {
    /// Test/embedding constructor: default settings, no session persistence.
    #[doc(hidden)]
    pub fn for_test(
        model: tack_ai::Model,
        provider: Arc<dyn Provider>,
        auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    ) -> Self {
        TackMcpServer {
            tool_router: TackMcpServer::tool_router(),
            model,
            provider,
            auth,
            settings: Arc::new(crate::settings::Settings::default()),
            state: Arc::new(Mutex::new(SharedState {
                messages: Vec::new(),
                usage: tack_ai::Usage::zero(),
            })),
            run_lock: Arc::new(Mutex::new(())),
        }
    }

    async fn run_prompt(&self, text: String) -> String {
        // One prompt at a time (see run_lock's docs): history is a shared
        // linear conversation, so concurrent runs must not fork it.
        let _run = self.run_lock.lock().await;
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut services = tack_tools::default_services(cwd.clone())
            .with_lsp(self.settings.lsp_manager(&cwd))
            .with_web_render(self.settings.web_render_mode())
            .with_web_search(self.settings.web_search_config())
            .with_background_tasks_enabled(self.settings.features.background_tasks)
            .with_memory_dir(self.settings.memory_directory.clone());
        if let Some(spec) = self.settings.sandbox_spec(&cwd) {
            services = services.with_sandbox(spec);
        }
        let tools = tack_tools::create_coding_tools(&services);
        let tools = crate::cli_flags::filter_feature_tools(tools, &self.settings.features);

        let messages = {
            let state = self.state.lock().await;
            state.messages.clone()
        };
        let config = AgentLoopConfig {
            model: self.model.clone(),
            provider: self.provider.clone(),
            hooks: Arc::new(NoopHooks),
            tool_execution: ToolExecutionMode::Parallel,
            reasoning: None,
            auth: self.auth.clone(),
            max_tokens: None,
            temperature: None,
            session_id: None,
            cache_retention: self.settings.cache_retention_mode(),
            fallback_models: crate::model::resolve_fallback_models(
                &self.settings.fallback_models,
                &self.model,
                &tack_session::default_agent_dir(),
            ),
            tool_pool: Vec::new(),
            retry_cancel: None,
        };
        let context = AgentContext {
            system_prompt: None,
            messages,
            tools,
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stream = agent_loop(vec![AgentMessage::user(text)], context, config, cancel);

        let mut final_text = String::new();
        let mut new_messages = Vec::new();
        let mut turn_usage = tack_ai::Usage::zero();
        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::MessageEnd { message } => {
                    if let AgentMessage::Assistant(a) = &message {
                        turn_usage.input += a.usage.input;
                        turn_usage.output += a.usage.output;
                        turn_usage.cache_read += a.usage.cache_read;
                        turn_usage.cache_write += a.usage.cache_write;
                        turn_usage.total_tokens += a.usage.total_tokens;
                        turn_usage.cost.total += a.usage.cost.total;
                        let text = a.text();
                        if !text.trim().is_empty() {
                            final_text = text;
                        }
                    }
                    new_messages.push(message);
                }
                AgentEvent::ModelFallback { from, to, .. } => {
                    tracing::info!("mcp-serve: model fallback {} → {}", from.id, to.id);
                }
                _ => {}
            }
        }

        let mut state = self.state.lock().await;
        state.messages.extend(new_messages);
        truncate_history(&mut state.messages);
        state.usage.input += turn_usage.input;
        state.usage.output += turn_usage.output;
        state.usage.cache_read += turn_usage.cache_read;
        state.usage.cache_write += turn_usage.cache_write;
        state.usage.total_tokens += turn_usage.total_tokens;
        state.usage.cost.total += turn_usage.cost.total;

        if final_text.trim().is_empty() {
            "(no text output)".to_string()
        } else {
            final_text
        }
    }
}

/// `tack mcp-serve`: speak MCP over stdio (run by the host agent/IDE).
pub async fn run(
    model: tack_ai::Model,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let agent_dir = tack_session::default_agent_dir();
    let settings = Arc::new(crate::settings::Settings::load(&cwd, &agent_dir));
    let provider: Arc<dyn Provider> = tack_ai::provider_for(&model)
        .ok_or_else(|| anyhow::anyhow!("no adapter for api kind {}", model.api))?;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::retry::RetryingProvider {
        inner: provider,
        policy: settings.retry.policy(),
        on_retry_scheduled: None,
    });

    let server = TackMcpServer {
        tool_router: TackMcpServer::tool_router(),
        model,
        provider,
        auth,
        settings,
        state: Arc::new(Mutex::new(SharedState {
            messages: Vec::new(),
            usage: tack_ai::Usage::zero(),
        })),
        run_lock: Arc::new(Mutex::new(())),
    };

    let transport = rmcp::transport::io::stdio();
    let running = server.serve(transport).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn non_user(tag: &str) -> AgentMessage {
        // Stand-in for assistant/tool_result messages: the truncation
        // boundary logic only discriminates User vs non-User.
        AgentMessage::custom(tag, tag, false, None)
    }

    #[test]
    fn truncation_lands_on_a_user_boundary() {
        // user → assistant → tool_result exchanges, over the cap.
        let mut messages = Vec::new();
        for i in 0..150 {
            messages.push(AgentMessage::user(format!("q{i}")));
            messages.push(non_user("assistant(tool_call)"));
            messages.push(non_user("tool_result"));
        }
        assert_eq!(messages.len(), 450);
        truncate_history(&mut messages);
        assert!(messages.len() <= MAX_HISTORY_MESSAGES);
        // The first retained message is a User (providers reject both
        // orphaned tool_results and a non-user first message).
        assert!(
            matches!(messages.first(), Some(AgentMessage::User(_))),
            "truncation must not split a tool exchange"
        );
    }

    #[test]
    fn truncation_without_user_boundary_drops_everything() {
        // One exchange bigger than the cap: no valid cut exists.
        let mut messages: Vec<AgentMessage> = (0..(MAX_HISTORY_MESSAGES + 50))
            .map(|_| non_user("tool_result"))
            .collect();
        truncate_history(&mut messages);
        assert!(messages.is_empty());
    }

    #[test]
    fn truncation_below_cap_is_a_noop() {
        let mut messages = vec![non_user("assistant"), AgentMessage::user("hi")];
        truncate_history(&mut messages);
        assert_eq!(messages.len(), 2);
    }
}
