//! AgentTool proxy for v3 plugin-registered tools: execution crosses to
//! the plugin over `tools/execute`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::rpc3::{ContentBlockKind, ToolExecuteParams, ToolSpec};
use crate::v3::PluginConnection;

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
    client: Arc<dyn PluginConnection>,
    full_name: &'static str,
    label: String,
    /// Prompt-injection defense for tools of MCP-carrier plugins (the
    /// output is untrusted server data): wrap result text in
    /// `<untrusted_content>` and flag the run for the permission layer.
    /// None for process/WASM plugins (host-trusted code).
    untrusted: Option<(Arc<std::sync::atomic::AtomicBool>, String)>,
}

impl std::fmt::Debug for ExtTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtTool")
            .field("name", &self.full_name)
            .finish()
    }
}

impl ExtTool {
    pub fn new(plugin_id: &str, spec: ToolSpec, client: Arc<dyn PluginConnection>) -> Self {
        // Provider APIs restrict tool names to [A-Za-z0-9_-]; the plugin
        // id (name@source, dots allowed) is sanitized into that charset.
        let sanitized = |s: &str| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        };
        let full_name = intern_tool_name(&format!(
            "ext__{}__{}",
            sanitized(plugin_id),
            sanitized(&spec.name)
        ));
        let label = spec.label.clone().unwrap_or_else(|| spec.name.clone());
        ExtTool {
            spec,
            client,
            full_name,
            label,
            untrusted: None,
        }
    }

    /// Mark this tool's output as untrusted external content (MCP-carrier
    /// plugins): text results are wrapped in `<untrusted_content>` tagged
    /// with `source`, and `flag` is set so the permission layer re-prompts
    /// for mutating follow-ups (same defense as config-file MCP tools).
    pub fn with_untrusted(
        mut self,
        flag: Arc<std::sync::atomic::AtomicBool>,
        source: String,
    ) -> Self {
        self.untrusted = Some((flag, source));
        self
    }

    /// Wrap result text when the untrusted defense is wired (success AND
    /// error results: a hostile server's error text must not enter the
    /// conversation unwrapped either).
    fn wrap_text(&self, text: String) -> String {
        match &self.untrusted {
            Some((flag, source)) => {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                format!("<untrusted_content source=\"{source}\">\n{text}\n</untrusted_content>")
            }
            None => text,
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
        let execute_params = ToolExecuteParams {
            name: self.spec.name.clone(),
            tool_call_id: tool_call_id.to_string(),
            arguments: params,
        };
        let call = self.client.tool_execute(&execute_params);
        let output = tokio::select! {
            _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
            r = call => r,
        };
        let output =
            output.map_err(|e| format!("extension tool {} failed: {e}", self.spec.name))?;
        let text = output
            .content
            .iter()
            .filter_map(|block| match block.r#type {
                ContentBlockKind::Text => block.text.clone(),
                ContentBlockKind::Image => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.is_empty() {
            "(no output)".to_string()
        } else {
            text
        };
        if output.is_error.unwrap_or(false) {
            return Err(self.wrap_text(text));
        }
        // Image blocks flow through to the model (an MCP tool returning a
        // screenshot must not silently degrade to "(no output)").
        let images: Vec<tack_ai::InputContentBlock> = output
            .content
            .iter()
            .filter_map(|block| match block.r#type {
                ContentBlockKind::Image => match (&block.data, &block.mime_type) {
                    (Some(data), Some(mime_type)) => Some(tack_ai::InputContentBlock::Image {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    }),
                    _ => None,
                },
                ContentBlockKind::Text => None,
            })
            .collect();
        if images.is_empty() {
            return Ok(AgentToolResult::text(self.wrap_text(text)));
        }
        let mut content = vec![tack_ai::InputContentBlock::text(self.wrap_text(text))];
        content.extend(images);
        Ok(AgentToolResult {
            content,
            details: output
                .details
                .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
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
    use crate::rpc3::{ErrorObject, ToolOutput};
    use crate::v3::{HostClient, JsonRpcPeer, PeerHandler};
    use std::sync::Arc;

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

    struct Noop;

    #[async_trait::async_trait]
    impl PeerHandler for Noop {}

    /// Fake plugin answering tools/execute with a fixed ToolOutput.
    struct Scripted(ToolOutput);

    #[async_trait::async_trait]
    impl PeerHandler for Scripted {
        async fn handle_request(
            &self,
            _method: &str,
            _params: Value,
        ) -> Result<Value, ErrorObject> {
            Ok(serde_json::to_value(&self.0).unwrap())
        }
    }

    async fn execute_with_output(output: ToolOutput) -> Result<AgentToolResult, String> {
        let (s1, s2) = tokio::io::duplex(8192);
        let (r1, w1) = tokio::io::split(s1);
        let (r2, w2) = tokio::io::split(s2);
        let client = HostClient::new(JsonRpcPeer::new(r1, w1, Arc::new(Noop)));
        let _plugin = JsonRpcPeer::new(r2, w2, Arc::new(Scripted(output)));
        let tool = ExtTool::new(
            "fake@user",
            ToolSpec {
                name: "probe".to_string(),
                label: None,
                description: "probe".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
            Arc::new(client),
        );
        tool.execute(
            "call-1",
            serde_json::json!({}),
            CancellationToken::new(),
            &|_| {},
        )
        .await
    }

    /// isError must surface as a tool ERROR, not a successful result
    /// whose text merely looks like an error message.
    #[tokio::test]
    async fn is_error_result_becomes_tool_error() {
        let result = execute_with_output(ToolOutput {
            content: vec![crate::rpc3::ContentBlock {
                r#type: ContentBlockKind::Text,
                text: Some("boom: everything failed".to_string()),
                mime_type: None,
                data: None,
            }],
            details: None,
            is_error: Some(true),
        })
        .await;
        let err = result.expect_err("isError must become a tool error");
        assert!(err.contains("boom"), "{err}");
    }

    #[tokio::test]
    async fn text_blocks_join_with_newlines() {
        let result = execute_with_output(ToolOutput {
            content: vec![
                crate::rpc3::ContentBlock {
                    r#type: ContentBlockKind::Text,
                    text: Some("a".to_string()),
                    mime_type: None,
                    data: None,
                },
                crate::rpc3::ContentBlock {
                    r#type: ContentBlockKind::Text,
                    text: Some("b".to_string()),
                    mime_type: None,
                    data: None,
                },
            ],
            details: None,
            is_error: None,
        })
        .await
        .unwrap();
        let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
            panic!("expected text")
        };
        assert_eq!(text, "a\nb");
    }
}
