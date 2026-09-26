//! Client-side tool search: when many MCP servers are connected, their tools
//! start DEFERRED (not in the model's tool list — no schema cost). The
//! `tool_search` tool searches the deferred pool by name/description and
//! activates matches: the result's `added_tool_names` (a tack-internal
//! channel, never serialized) moves them into the context for the next LLM
//! call, and the loop records the loadout change as a transcript system
//! message (upstream #9548: system messages carry tool declarations).
//!
//! Enabled via settings `mcpDeferThreshold` (defer MCP tools when the total
//! tool count exceeds it; 0 = off).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

/// One deferred tool's search metadata.
#[derive(Clone, Debug)]
pub struct PoolEntry {
    pub name: String,
    pub description: String,
}

pub struct ToolSearchTool {
    pool: Vec<PoolEntry>,
}

impl ToolSearchTool {
    pub fn new(pool: Vec<PoolEntry>) -> Self {
        ToolSearchTool { pool }
    }
}

impl std::fmt::Debug for ToolSearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSearchTool")
            .field("pool", &self.pool.len())
            .finish()
    }
}

/// Score an entry against the query: name matches count double; every query
/// token must hit somewhere (name or description).
fn score(entry: &PoolEntry, tokens: &[&str]) -> usize {
    let name = entry.name.to_lowercase();
    let desc = entry.description.to_lowercase();
    let mut total = 0;
    for token in tokens {
        let in_name = name.contains(token);
        let in_desc = desc.contains(token);
        if !in_name && !in_desc {
            return 0;
        }
        total += if in_name { 2 } else { 1 };
    }
    total
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// What you want to do, e.g. "create an issue" or "search code"
    query: String,
}

#[async_trait]
impl AgentTool for ToolSearchTool {
    fn name(&self) -> &'static str {
        "tool_search"
    }
    fn label(&self) -> &str {
        "tool_search"
    }
    fn description(&self) -> &str {
        "Search for additional tools by capability and activate them. Use when the currently \
         available tools cannot perform the task — matching tools become callable immediately \
         after this returns."
    }
    fn parameters_schema(&self) -> Value {
        crate::schema_for::<SearchParams>()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: SearchParams = serde_json::from_value(params)
            .map_err(|e| format!("invalid tool_search params: {e}"))?;
        let query_lower = params.query.to_lowercase();
        let tokens: Vec<&str> = query_lower.split_whitespace().collect();
        if tokens.is_empty() {
            return Err("query must not be empty".to_string());
        }

        let mut scored: Vec<(&PoolEntry, usize)> = self
            .pool
            .iter()
            .filter_map(|e| {
                let s = score(e, &tokens);
                (s > 0).then_some((e, s))
            })
            .collect();
        scored.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
        scored.truncate(5);

        if scored.is_empty() {
            return Ok(AgentToolResult::text(format!(
                "No tools match {:?} ({} tools in the deferred pool).",
                params.query,
                self.pool.len()
            )));
        }

        let mut text = String::from("Activated tools (callable from the next step):\n");
        for (entry, _) in &scored {
            text.push_str(&format!("- {}: {}\n", entry.name, entry.description));
        }
        let mut result = AgentToolResult::text(text);
        result.added_tool_names = Some(scored.iter().map(|(e, _)| e.name.clone()).collect());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn search_activates_matches() {
        let pool = vec![
            PoolEntry {
                name: "mcp__github__create_issue".into(),
                description: "Create a GitHub issue".into(),
            },
            PoolEntry {
                name: "mcp__linear__list_teams".into(),
                description: "List Linear teams".into(),
            },
        ];
        let tool = ToolSearchTool::new(pool);
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "query": "github issue" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert_eq!(
            result.added_tool_names.as_deref(),
            Some(&["mcp__github__create_issue".to_string()][..])
        );

        // No match → empty activation.
        let result = tool
            .execute(
                "2",
                serde_json::json!({ "query": "nonexistent capability" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(result.added_tool_names.is_none());
    }

    /// Name hits outrank description-only hits; ties keep pool order; the
    /// activation list is capped at 5.
    #[tokio::test]
    async fn ranking_prefers_name_matches_and_caps_at_five() {
        let mut pool: Vec<PoolEntry> = (0..8)
            .map(|i| PoolEntry {
                name: format!("mcp__srv__tool_{i}"),
                description: format!("deploy helper {i}"),
            })
            .collect();
        // This one matches "deploy" only in the description → lower rank
        // than the name hit below.
        pool.insert(
            0,
            PoolEntry {
                name: "mcp__srv__deploy_now".into(),
                description: "release it".into(),
            },
        );
        let tool = ToolSearchTool::new(pool);
        let result = tool
            .execute(
                "1",
                serde_json::json!({ "query": "deploy" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let names = result.added_tool_names.unwrap();
        assert_eq!(names.len(), 5, "capped at 5: {names:?}");
        assert_eq!(names[0], "mcp__srv__deploy_now", "name hit ranks first");

        // Every query token must hit somewhere: a two-token query where one
        // token matches nothing returns zero results.
        let result = tool
            .execute(
                "2",
                serde_json::json!({ "query": "deploy zzz" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(result.added_tool_names.is_none());
    }
}
