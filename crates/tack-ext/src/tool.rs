//! AgentTool proxy for plugin-registered tools (mirrors McpTool's pattern):
//! execution crosses to the plugin process over NDJSON.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::process::PluginPeer;
use crate::protocol::{ToolExecuteParams, ToolSpec};

/// Intern a fully-qualified tool name. `AgentTool::name` must return
/// `&'static str`, and the naive `Box::leak` on every `ExtTool::new`
/// leaked a fresh allocation on EVERY plugin reload. Interning leaks each
/// DISTINCT name exactly once; reloading a plugin reuses the existing one.
fn intern_tool_name(name: &str) -> &'static str {
    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let pool = POOL.get_or_init(|| Mutex::new(HashSet::new()));
    let mut pool = pool.lock().expect("tool name pool poisoned");
    if let Some(&existing) = pool.get(name) {
        return existing;
    }
    let interned: &'static str = Box::leak(name.to_string().into_boxed_str());
    pool.insert(interned);
    interned
}

/// A plugin tool exposed to the agent loop.
pub struct ExtTool {
    spec: ToolSpec,
    peer: Arc<PluginPeer>,
    full_name: &'static str,
    label: String,
}

impl std::fmt::Debug for ExtTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtTool")
            .field("name", &self.full_name)
            .finish()
    }
}

impl ExtTool {
    pub fn new(plugin_name: &str, spec: ToolSpec, peer: Arc<PluginPeer>) -> Self {
        let full_name = intern_tool_name(&format!("ext__{plugin_name}__{}", spec.name));
        let label = spec.label.clone().unwrap_or_else(|| spec.name.clone());
        ExtTool {
            spec,
            peer,
            full_name,
            label,
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for ExtTool {
    fn name(&self) -> &'static str {
        self.full_name
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn parameters_schema(&self) -> Value {
        self.spec.parameters.clone()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let call = self.peer.call(
            "tool.execute",
            serde_json::to_value(ToolExecuteParams {
                name: self.spec.name.clone(),
                tool_call_id: tool_call_id.to_string(),
                arguments: params,
            })
            .map_err(|e| e.to_string())?,
        );
        let result = tokio::select! {
            _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
            r = call => r,
        };
        let result =
            result.map_err(|e| format!("extension tool {} failed: {e}", self.spec.name))?;
        // Result convention: {content: "..."} or {content: [{type,text}...], isError}
        let is_error = result
            .get("isError")
            .or_else(|| result.get("is_error"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = match &result {
            Value::String(s) => s.clone(),
            Value::Object(_) => result
                .get("content")
                .map(|c| match c {
                    Value::String(s) => s.clone(),
                    Value::Array(blocks) => blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    other => other.to_string(),
                })
                .unwrap_or_default(),
            other => other.to_string(),
        };
        let text = if text.is_empty() {
            "(no output)".to_string()
        } else {
            text
        };
        if is_error {
            return Err(text);
        }
        Ok(AgentToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::process::HostServices;
    use crate::protocol::Envelope;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct NoopServices;

    #[async_trait::async_trait]
    impl HostServices for NoopServices {
        async fn handle_request(&self, _method: &str, _params: Value) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn handle_event(&self, _event: &str, _payload: Value) {}
    }

    /// Interning: reloading a plugin must reuse the same &'static str for
    /// the same tool name (a fresh Box::leak per reload was a permanent
    /// leak).
    #[test]
    fn tool_names_are_interned_across_reloads() {
        let a = intern_tool_name("ext__plug__probe");
        let b = intern_tool_name("ext__plug__probe");
        assert!(
            std::ptr::eq(a, b),
            "same name must resolve to one allocation"
        );
        let c = intern_tool_name("ext__plug__other");
        assert!(!std::ptr::eq(a, c));
    }

    /// Run ExtTool::execute against a fake plugin answering with `result`.
    async fn execute_with_result(result: Value) -> Result<AgentToolResult, String> {
        let (host_reader, mut plugin_writer) = tokio::io::duplex(8192);
        let (mut plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(NoopServices));
        tokio::spawn(async move {
            let mut lines = BufReader::new(&mut plugin_reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(Envelope::Request { id, .. }) = serde_json::from_str(&line) {
                    let response = Envelope::result(id, result.clone());
                    let mut text = serde_json::to_string(&response).unwrap();
                    text.push('\n');
                    if plugin_writer.write_all(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });
        let tool = ExtTool::new(
            "fake",
            ToolSpec {
                name: "probe".to_string(),
                label: None,
                description: "probe".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
            peer,
        );
        tool.execute(
            "call-1",
            serde_json::json!({}),
            CancellationToken::new(),
            &|_| {},
        )
        .await
    }

    /// The documented result convention includes isError: a plugin tool
    /// reporting failure must surface as a tool ERROR, not a successful
    /// result whose text merely looks like an error message.
    #[tokio::test]
    async fn is_error_result_becomes_tool_error() {
        let result = execute_with_result(serde_json::json!({
            "content": "boom: everything failed",
            "isError": true,
        }))
        .await;
        let err = result.expect_err("isError must become a tool error");
        assert!(err.contains("boom"), "{err}");
    }

    #[tokio::test]
    async fn content_shapes_map_to_text() {
        // String result.
        let r = execute_with_result(serde_json::json!("plain"))
            .await
            .unwrap();
        // Object with content blocks.
        let r2 = execute_with_result(serde_json::json!({
            "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
        }))
        .await
        .unwrap();
        let texts: Vec<String> = [r, r2]
            .iter()
            .map(|r| match &r.content[0] {
                tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
                _ => panic!("expected text"),
            })
            .collect();
        assert_eq!(texts[0], "plain");
        assert_eq!(texts[1], "a\nb");
    }
}
