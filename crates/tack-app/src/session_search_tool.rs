//! `session_search` agent tool (context engineering: just-in-time
//! retrieval). Wraps tack-session's cross-session full-text search so the
//! model can ask "how did we solve this last time?" on demand, instead
//! of past context being carried into every session up front. Results
//! point at session files on disk — the model reads/greps them for
//! detail, so nothing bulky enters the context unprompted.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

/// Total output cap: search results are a directory, not a data dump.
const MAX_OUTPUT_CHARS: usize = 20_000;

#[derive(Debug)]
pub struct SessionSearchTool {
    agent_dir: PathBuf,
}

impl SessionSearchTool {
    pub fn new(agent_dir: PathBuf) -> Self {
        SessionSearchTool { agent_dir }
    }
}

fn format_hits(hits: &[tack_session::SessionSearchHit], query: &str) -> String {
    if hits.is_empty() {
        return format!("No past sessions match {query:?}.");
    }
    let mut out = format!(
        "{} session(s) match {query:?}. Read a session file with read/grep for full detail \
         (JSONL, one entry per line; the current session may be among the matches):\n",
        hits.len()
    );
    for hit in hits {
        let title = hit.name.as_deref().unwrap_or(hit.session_id.as_str());
        out.push_str(&format!(
            "\n## {title} ({})\ncwd: {}\nfile: {}\nmatches: {}\n",
            hit.timestamp,
            hit.cwd,
            hit.path.display(),
            hit.match_count,
        ));
        for (role, snippet) in &hit.snippets {
            out.push_str(&format!("- [{role}] {}\n", snippet.replace('\n', " ")));
        }
        if out.chars().count() > MAX_OUTPUT_CHARS {
            out.push_str("\n[truncated: too many matches — narrow the query]\n");
            break;
        }
    }
    out
}

#[async_trait]
impl AgentTool for SessionSearchTool {
    fn name(&self) -> &'static str {
        "session_search"
    }

    fn label(&self) -> &str {
        "session search"
    }

    fn description(&self) -> &str {
        "Search all past tack sessions (full-text over user and assistant messages) to find \
         how a similar problem was solved before. Returns matching sessions with snippets and \
         file paths; use read/grep on the reported session file for full context."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Case-insensitive substring to search for in past sessions"
                },
                "maxResults": {
                    "type": "integer",
                    "description": "Maximum number of sessions to return (default 5, capped at 20)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| "missing non-empty \"query\" argument".to_string())?
            .to_string();
        let max_results = params
            .get("maxResults")
            .and_then(|v| v.as_u64())
            .map(|v| usize::try_from(v.clamp(1, 20)).unwrap_or(5))
            .unwrap_or(5);
        let agent_dir = self.agent_dir.clone();
        let query_for_search = query.clone();
        let hits = tokio::task::spawn_blocking(move || {
            tack_session::search_sessions(&agent_dir, &query_for_search, max_results)
        })
        .await
        .map_err(|e| format!("session search failed: {e}"))?;
        Ok(AgentToolResult::text(format_hits(&hits, &query)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn empty_query_is_rejected() {
        let tool = SessionSearchTool::new(PathBuf::from("."));
        let result = tool
            .execute(
                "t1",
                json!({ "query": "   " }),
                CancellationToken::new(),
                &|_| {},
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn no_sessions_dir_gives_empty_answer_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SessionSearchTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(
                "t2",
                json!({ "query": "anything" }),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
            panic!("text result")
        };
        assert!(text.contains("No past sessions match"), "{text}");
    }

    #[test]
    fn schema_requires_query() {
        let tool = SessionSearchTool::new(PathBuf::from("."));
        let schema = tool.parameters_schema();
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("query"))
        );
        tool.validate_arguments(&json!({ "query": "x" })).unwrap();
        assert!(tool.validate_arguments(&json!({})).is_err());
    }
}
