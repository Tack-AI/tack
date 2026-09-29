//! Level-2 MCP server plugins (`docs/plugin-roadmap.md` §4): an extension
//! whose `extension.json` declares `carrier: "mcp"` plus one `mcpServer`
//! entry IS the plugin — the host connects to the server at load time and
//! adapts its tools (plus resource meta-tools and prompt tools) into the
//! plugin model with the plugin's identity: the capabilities list,
//! attribution (`ext__<plugin-id>__<tool>`), policy, and interception all
//! behave exactly as for tack-RPC plugins, and no plugin process speaking
//! tack-RPC is ever spawned.
//!
//! The connection implements [`PluginConnection`], the carrier-agnostic
//! host→plugin surface, so `ExtensionManager` consumes Level-2 plugins
//! through the identical code path as process/WASM plugins. Capability
//! namespaces MCP cannot express (hooks, widgets, session control, …)
//! return `capabilityNotGranted` — the manager never calls them because
//! the synthesized capability list only declares `tools`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tack_ext::PluginConnection;
use tack_ext::rpc3::{
    ContentBlock, ContentBlockKind, InitializeParams, InitializeResult, PluginCapabilities,
    PluginInfo, ToolExecuteParams, ToolOutput, ToolSpec,
};
use tack_ext::v3::{PeerError, unsupported_capability};
use tack_tools::mcp::{McpClientCallbacks, McpConnection, McpPluginTool, McpServerSpec};

/// How often the death-watch polls [`McpConnection::is_closed`] (rmcp
/// exposes no closed-future; widget cleanup is not latency-critical).
const DEATH_POLL: Duration = Duration::from_millis(250);

/// Bound on one host→plugin call: the process/WASI carriers' v3 peer
/// fails a wedged plugin at 30s (`tack_ext::v3::peer::REQUEST_TIMEOUT`),
/// but the shared MCP client machinery sets no request timeout on its
/// rmcp calls, so the MCP carrier applies the same bound here (shorter
/// under test).
#[cfg(not(test))]
const CALL_TIMEOUT: Duration = tack_ext::v3::peer::REQUEST_TIMEOUT;
#[cfg(test)]
const CALL_TIMEOUT: Duration = Duration::from_millis(250);

/// Bound on connect + initialize + the capability probe at load time:
/// the extension load loop is SEQUENTIAL, so one server that spawns but
/// never answers initialize must not hang session startup for every
/// other plugin. The process carrier allows 10s for initialize alone;
/// the MCP connect also covers the server spawn, hence 15s (shorter
/// under test).
#[cfg(not(test))]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(test)]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// Apply [`CONNECT_TIMEOUT`] to a connect future, mapping expiry to the
/// same kind of error a failed handshake produces. Dropping the connect
/// future drops the rmcp serve future and its transport; for stdio that
/// kills the server child (rmcp's `TokioChildProcess` Drop kills the
/// process), so no wedged child outlives a timed-out connect.
async fn with_connect_timeout(
    server_name: &str,
    connect: impl Future<Output = Result<McpConnection, String>>,
) -> Result<McpConnection, String> {
    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "MCP server {server_name} did not finish connect+initialize within {CONNECT_TIMEOUT:?}"
        )),
    }
}

/// The carrier spec for a Level-2 plugin: identical to the declared
/// server, but with credential stripping on the stdio spawn — a plugin
/// is third-party code, so the v3 process carrier's env rule applies
/// (see `tack_ext::process::env_vars_to_strip`). Only user-configured
/// MCP servers inherit the host's full environment.
fn plugin_carrier_spec(spec: &McpServerSpec) -> McpServerSpec {
    spec.clone().with_credential_stripping()
}

/// The host's view of one Level-2 MCP server plugin.
pub struct McpPluginConnection {
    plugin_name: String,
    plugin_version: String,
    conn: Arc<McpConnection>,
    /// Advertised (unprefixed) tool name → execution engine.
    engines: HashMap<String, McpPluginTool>,
    /// The synthesized capability list (same order as connection probe).
    specs: Vec<ToolSpec>,
}

impl std::fmt::Debug for McpPluginConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpPluginConnection")
            .field("plugin", &self.plugin_name)
            .field("server", &self.conn.name)
            .field("tools", &self.specs.len())
            .finish()
    }
}

impl McpPluginConnection {
    /// Connect to the plugin's MCP server and probe its capabilities.
    /// `callbacks` carries the mode's elicitation handler (sampling is not
    /// wired at load time — see `mcp_config::plugin_mcp_callbacks`).
    pub async fn connect(
        spec: &McpServerSpec,
        callbacks: McpClientCallbacks,
        plugin_name: String,
        plugin_version: String,
    ) -> Result<Self, String> {
        let spec = plugin_carrier_spec(spec);
        let conn =
            with_connect_timeout(&spec.name, tack_tools::mcp::connect_with(&spec, callbacks))
                .await?;
        Ok(Self::from_connection(
            Arc::new(conn),
            plugin_name,
            plugin_version,
        ))
    }

    /// Adapt an established connection (capability synthesis shared by the
    /// load path and tests).
    pub fn from_connection(
        conn: Arc<McpConnection>,
        plugin_name: String,
        plugin_version: String,
    ) -> Self {
        let mut engines = HashMap::new();
        let mut specs = Vec::new();
        for tool in tack_tools::mcp::plugin_capabilities(&conn) {
            specs.push(ToolSpec {
                name: tool.spec_name.clone(),
                label: None,
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            });
            engines.insert(tool.spec_name.clone(), tool);
        }
        McpPluginConnection {
            plugin_name,
            plugin_version,
            conn,
            engines,
            specs,
        }
    }
}

/// AgentToolResult → wire ToolOutput (content blocks map 1:1; an empty
/// details object stays absent, matching process-plugin replies).
fn to_tool_output(result: tack_agent_core::AgentToolResult) -> ToolOutput {
    let content = result
        .content
        .iter()
        .map(|block| match block {
            tack_ai::InputContentBlock::Text { text, .. } => ContentBlock {
                r#type: ContentBlockKind::Text,
                text: Some(text.clone()),
                data: None,
                mime_type: None,
            },
            tack_ai::InputContentBlock::Image { data, mime_type } => ContentBlock {
                r#type: ContentBlockKind::Image,
                text: None,
                data: Some(data.clone()),
                mime_type: Some(mime_type.clone()),
            },
        })
        .collect();
    ToolOutput {
        content,
        details: match result.details {
            Value::Null => None,
            details => Some(details),
        },
        is_error: None,
    }
}

#[async_trait::async_trait]
impl PluginConnection for McpPluginConnection {
    async fn initialize(&self, _params: &InitializeParams) -> Result<InitializeResult, PeerError> {
        // No wire handshake: the MCP initialize already happened at
        // connect; synthesize the v3 result from the probed capabilities.
        // A server that died between connect and here must not register
        // as loaded — the process carrier fails its handshake in that
        // case, so report Dead the same way.
        if self.conn.is_closed() {
            return Err(PeerError::Dead);
        }
        // The server speaks no tack-RPC version — report the host's own so
        // the version check is a no-op by construction.
        Ok(InitializeResult {
            protocol_version: tack_ext::v3::PROTOCOL_VERSION.to_string(),
            plugin: PluginInfo {
                name: self.plugin_name.clone(),
                version: Some(self.plugin_version.clone()),
                description: None,
            },
            capabilities: PluginCapabilities {
                tools: Some(self.specs.clone()),
                ..Default::default()
            },
        })
    }

    async fn tool_execute(&self, params: &ToolExecuteParams) -> Result<ToolOutput, PeerError> {
        if self.conn.is_closed() {
            return Err(PeerError::Dead);
        }
        let Some(engine) = self.engines.get(&params.name) else {
            return Err(unsupported_capability(&format!(
                "tools/execute for unknown tool {:?}",
                params.name
            )));
        };
        // The shared MCP machinery sets no request timeout on the rmcp
        // call; bound it to the process carrier's per-call timeout so a
        // wedged-but-connected server fails the turn at 30s instead of
        // hanging it forever. Dropping the execute future on expiry
        // aborts the await (McpPluginTool::execute documents structural
        // cancellation — the same drop McpTool's cancel path performs),
        // so no explicit cancellation token needs firing.
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            engine.execute(&params.tool_call_id, params.arguments.clone()),
        )
        .await
        .map_err(|_| PeerError::Timeout)?;
        match result {
            Ok(result) => Ok(to_tool_output(result)),
            // MCP isError / transport failures both surface as tool errors
            // (the model sees the message; the run continues).
            Err(message) => Ok(ToolOutput {
                content: vec![ContentBlock {
                    r#type: ContentBlockKind::Text,
                    text: Some(message),
                    data: None,
                    mime_type: None,
                }],
                details: None,
                is_error: Some(true),
            }),
        }
    }

    async fn command_invoke(
        &self,
        _params: &tack_ext::rpc3::CommandInvokeParams,
    ) -> Result<Value, PeerError> {
        Err(unsupported_capability("commands/invoke"))
    }

    async fn before_tool_call(
        &self,
        _params: &tack_ext::rpc3::BeforeToolCallParams,
    ) -> Result<tack_ext::rpc3::Verdict, PeerError> {
        Err(unsupported_capability("hooks/beforeToolCall"))
    }

    async fn transform_context(
        &self,
        _params: &tack_ext::rpc3::TransformContextParams,
    ) -> Result<Option<tack_ext::rpc3::TransformContextResult>, PeerError> {
        Err(unsupported_capability("hooks/transformContext"))
    }

    async fn after_tool_call(
        &self,
        _params: &tack_ext::rpc3::AfterToolCallParams,
    ) -> Result<Option<tack_ext::rpc3::AfterToolCallPatch>, PeerError> {
        Err(unsupported_capability("hooks/afterToolCall"))
    }

    async fn approval_review(
        &self,
        _params: &tack_ext::rpc3::ApprovalReviewParams,
    ) -> Result<Option<tack_ext::rpc3::ApprovalDecision>, PeerError> {
        Err(unsupported_capability("approval/review"))
    }

    async fn autocomplete_provide(
        &self,
        _params: &tack_ext::rpc3::AutocompleteProvideParams,
    ) -> Result<tack_ext::rpc3::AutocompleteProvideResult, PeerError> {
        Err(unsupported_capability("autocomplete/provide"))
    }

    async fn lifecycle_event(&self, _event: &str, _payload: Value) -> Result<(), PeerError> {
        // Notifications are fire-and-forget and the host's subscription
        // filter treats "no declared events" as the DEFAULT set, so every
        // lifecycle event fans out here: a Level-2 plugin simply has no
        // use for them. Swallow silently instead of erroring on every
        // event (the errors would be ignored upstream anyway).
        Ok(())
    }

    async fn widget_action(
        &self,
        _params: &tack_ext::rpc3::WidgetActionParams,
    ) -> Result<(), PeerError> {
        Err(unsupported_capability("widgets/action"))
    }

    async fn call_raw(&self, rpc_method: &str, _params: Value) -> Result<Value, PeerError> {
        Err(unsupported_capability(rpc_method))
    }

    async fn notify_raw(&self, rpc_method: &str, _params: Value) -> Result<(), PeerError> {
        Err(unsupported_capability(rpc_method))
    }

    async fn shutdown(&self) -> Result<(), PeerError> {
        self.conn.cancel();
        Ok(())
    }

    fn is_alive(&self) -> bool {
        !self.conn.is_closed()
    }

    async fn wait_dead(&self) {
        while !self.conn.is_closed() {
            tokio::time::sleep(DEATH_POLL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, ContentBlock, ErrorData, GetPromptRequestParams,
        GetPromptResult, ListPromptsResult, ListResourcesResult, ListToolsResult,
        PaginatedRequestParams, Prompt, PromptMessage, ReadResourceRequestParams,
        ReadResourceResult, Resource, ResourceContents, Role, ServerCapabilities, ServerConfig,
        Tool,
    };
    use rmcp::service::RequestContext;
    use rmcp::{RoleServer, ServerHandler, ServiceExt};
    use std::sync::Arc;
    use tack_ext::rpc3::{
        AutocompleteProvideParams, BeforeToolCallParams, ERR_CAPABILITY_NOT_GRANTED,
        InitializeParams, ToolExecuteParams,
    };

    /// In-process MCP fixture server: three tools (one dotted name, one
    /// always-erroring), one resource, one prompt.
    #[derive(Clone, Debug)]
    struct FixtureServer;

    fn object_schema() -> Arc<serde_json::Map<String, Value>> {
        Arc::new(serde_json::Map::from_iter([(
            "type".to_string(),
            Value::String("object".to_string()),
        )]))
    }

    impl ServerHandler for FixtureServer {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(
                ServerCapabilities::builder()
                    .enable_tools()
                    .enable_resources()
                    .enable_prompts()
                    .build(),
            )
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            Ok(ListToolsResult::with_all_items(vec![
                Tool::new(
                    "echo".to_string(),
                    "Echo back text".to_string(),
                    object_schema(),
                ),
                Tool::new(
                    "query.coupon".to_string(),
                    "Dotted name".to_string(),
                    object_schema(),
                ),
                Tool::new(
                    "fail".to_string(),
                    "Always errors".to_string(),
                    object_schema(),
                ),
            ]))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<rmcp::model::CallToolResponse, ErrorData> {
            match request.name.as_ref() {
                "echo" => {
                    let text = request
                        .arguments
                        .as_ref()
                        .and_then(|a| a.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    Ok(
                        CallToolResult::success(vec![ContentBlock::text(format!("echo: {text}"))])
                            .into(),
                    )
                }
                "query.coupon" => {
                    Ok(CallToolResult::success(vec![ContentBlock::text("coupon-42")]).into())
                }
                "fail" => Ok(CallToolResult::error(vec![ContentBlock::text(
                    "boom: everything failed",
                )])
                .into()),
                other => Err(ErrorData::invalid_params(
                    format!("unknown tool {other}"),
                    None,
                )),
            }
        }

        async fn list_resources(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListResourcesResult, ErrorData> {
            Ok(ListResourcesResult::with_all_items(vec![Resource::new(
                "mem://notes",
                "notes",
            )]))
        }

        async fn read_resource(
            &self,
            request: ReadResourceRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<rmcp::model::ReadResourceResponse, ErrorData> {
            Ok(
                ReadResourceResult::new(vec![ResourceContents::TextResourceContents {
                    uri: request.uri.clone(),
                    mime_type: Some("text/plain".to_string()),
                    text: format!("contents of {}", request.uri),
                    meta: None,
                }])
                .into(),
            )
        }

        async fn list_prompts(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListPromptsResult, ErrorData> {
            Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
                "review",
                Some("Review code"),
                None,
            )]))
        }

        async fn get_prompt(
            &self,
            _request: GetPromptRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<rmcp::model::GetPromptResponse, ErrorData> {
            Ok(GetPromptResult::new(vec![PromptMessage::new(
                Role::User,
                ContentBlock::text("please review this diff"),
            )])
            .into())
        }
    }

    /// A Level-2 plugin connected to the in-process fixture server.
    async fn fixture_plugin() -> McpPluginConnection {
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let running = FixtureServer.serve(server_io).await.unwrap();
            running.waiting().await.unwrap();
        });
        let conn = tack_tools::mcp::connect_transport("fixture", client_io, Default::default())
            .await
            .unwrap();
        McpPluginConnection::from_connection(
            Arc::new(conn),
            "fixture".to_string(),
            "local".to_string(),
        )
    }

    fn init_params() -> InitializeParams {
        serde_json::from_value(serde_json::json!({
            "protocolVersion": tack_ext::v3::PROTOCOL_VERSION,
            "host": {"name": "tack", "version": "0.0.0"},
            "mode": "print",
            "cwd": "/tmp",
            "trusted": false,
            "capabilities": {}
        }))
        .unwrap()
    }

    /// Plugin carriers opt in to credential stripping on stdio spawns
    /// (third-party code must not inherit the host's API keys); the
    /// declared spec — which user-configured servers also use — is not
    /// mutated.
    #[test]
    fn plugin_carrier_opts_into_credential_stripping() {
        let spec = McpServerSpec::stdio("srv".to_string(), "cmd".to_string(), vec![], vec![], None);
        assert!(!spec.strip_credentials);
        let carrier = super::plugin_carrier_spec(&spec);
        assert!(carrier.strip_credentials);
        assert!(!spec.strip_credentials, "the declared spec is untouched");
    }

    #[tokio::test]
    async fn initialize_synthesizes_capabilities_from_mcp_probe() {
        let plugin = fixture_plugin().await;
        let result = plugin.initialize(&init_params()).await.unwrap();
        assert_eq!(result.protocol_version, tack_ext::v3::PROTOCOL_VERSION);
        assert_eq!(result.plugin.name, "fixture");
        assert_eq!(result.plugin.version.as_deref(), Some("local"));
        let tools = result.capabilities.tools.unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"echo"), "{names:?}");
        // Dotted MCP names sanitize to provider-safe tool names.
        assert!(names.contains(&"query_coupon"), "{names:?}");
        // Resources and prompts join as meta-tools / prompt tools.
        assert!(names.contains(&"list_resources"), "{names:?}");
        assert!(names.contains(&"read_resource"), "{names:?}");
        assert!(names.contains(&"prompt__review"), "{names:?}");
        // No tack-RPC-only namespaces are declared.
        assert!(result.capabilities.hooks.is_none());
        assert!(result.capabilities.widgets.is_none());
        assert!(result.capabilities.events.is_none());
    }

    async fn execute_text(plugin: &McpPluginConnection, name: &str, args: Value) -> ToolOutput {
        plugin
            .tool_execute(&ToolExecuteParams {
                name: name.to_string(),
                tool_call_id: "call-1".to_string(),
                arguments: args,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn tool_execute_routes_to_mcp_call_tool() {
        let plugin = fixture_plugin().await;
        let output = execute_text(&plugin, "echo", serde_json::json!({"text": "hi"})).await;
        assert_eq!(output.is_error, None);
        assert_eq!(output.content[0].text.as_deref(), Some("echo: hi"));
        // The sanitized spec name routes back to the dotted MCP tool.
        let output = execute_text(&plugin, "query_coupon", serde_json::json!({})).await;
        assert_eq!(output.content[0].text.as_deref(), Some("coupon-42"));
    }

    #[tokio::test]
    async fn mcp_is_error_becomes_tool_output_error() {
        let plugin = fixture_plugin().await;
        let output = execute_text(&plugin, "fail", serde_json::json!({})).await;
        assert_eq!(output.is_error, Some(true));
        assert!(
            output.content[0]
                .text
                .as_deref()
                .unwrap_or("")
                .contains("boom"),
            "{output:?}"
        );
    }

    #[tokio::test]
    async fn unknown_tool_and_unsupported_namespaces_are_capability_errors() {
        let plugin = fixture_plugin().await;
        let err = plugin
            .tool_execute(&ToolExecuteParams {
                name: "nope".to_string(),
                tool_call_id: "call-1".to_string(),
                arguments: Value::Null,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), ERR_CAPABILITY_NOT_GRANTED);

        let err = plugin
            .before_tool_call(
                &serde_json::from_value::<BeforeToolCallParams>(serde_json::json!({"toolCall": {
                    "toolName": "bash",
                    "toolCallId": "call-1",
                    "arguments": {}
                }}))
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ERR_CAPABILITY_NOT_GRANTED);

        let err = plugin
            .autocomplete_provide(&AutocompleteProvideParams {
                provider_id: "p".to_string(),
                query: "q".to_string(),
                cursor_offset: 0,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), ERR_CAPABILITY_NOT_GRANTED);
    }

    #[tokio::test]
    async fn resource_and_prompt_tools_execute() {
        let plugin = fixture_plugin().await;
        let output = execute_text(&plugin, "list_resources", serde_json::json!({})).await;
        assert!(
            output.content[0]
                .text
                .as_deref()
                .unwrap_or("")
                .contains("mem://notes"),
            "{output:?}"
        );
        let output = execute_text(
            &plugin,
            "read_resource",
            serde_json::json!({"uri": "mem://notes"}),
        )
        .await;
        assert!(
            output.content[0]
                .text
                .as_deref()
                .unwrap_or("")
                .contains("contents of mem://notes"),
            "{output:?}"
        );
        let output = execute_text(&plugin, "prompt__review", serde_json::json!({})).await;
        assert!(
            output.content[0]
                .text
                .as_deref()
                .unwrap_or("")
                .contains("please review this diff"),
            "{output:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_kills_the_connection_and_wait_dead_returns() {
        let plugin = fixture_plugin().await;
        assert!(plugin.is_alive());
        PluginConnection::shutdown(&plugin).await.unwrap();
        // wait_dead must observe the cancellation promptly.
        tokio::time::timeout(std::time::Duration::from_secs(5), plugin.wait_dead())
            .await
            .expect("wait_dead hangs after shutdown");
        assert!(!plugin.is_alive());
        // Calls against a dead connection report Dead.
        let err = plugin
            .tool_execute(&ToolExecuteParams {
                name: "echo".to_string(),
                tool_call_id: "call-1".to_string(),
                arguments: Value::Null,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, PeerError::Dead));
    }

    /// A fixture server that answers initialize and tools/list but whose
    /// call_tool never responds — a wedged-but-connected server.
    #[derive(Clone, Debug)]
    struct HangingServer;

    impl ServerHandler for HangingServer {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            Ok(ListToolsResult::with_all_items(vec![Tool::new(
                "hang".to_string(),
                "Never answers".to_string(),
                object_schema(),
            )]))
        }

        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<rmcp::model::CallToolResponse, ErrorData> {
            std::future::pending::<()>().await;
            unreachable!("pending never resolves")
        }
    }

    async fn hanging_plugin() -> McpPluginConnection {
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let running = HangingServer.serve(server_io).await.unwrap();
            running.waiting().await.unwrap();
        });
        let conn = tack_tools::mcp::connect_transport("hanging", client_io, Default::default())
            .await
            .unwrap();
        McpPluginConnection::from_connection(
            Arc::new(conn),
            "hanging".to_string(),
            "local".to_string(),
        )
    }

    /// A wedged MCP server must fail the call with the same error the
    /// process carrier produces (PeerError::Timeout) instead of hanging
    /// the agent turn forever (CALL_TIMEOUT is 250ms under test).
    #[tokio::test]
    async fn wedged_tool_call_times_out_like_the_process_carrier() {
        let plugin = hanging_plugin().await;
        let err = plugin
            .tool_execute(&ToolExecuteParams {
                name: "hang".to_string(),
                tool_call_id: "call-1".to_string(),
                arguments: Value::Null,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, PeerError::Timeout), "{err:?}");
    }

    /// A server that died between connect and the handshake must not
    /// register as loaded (the process carrier fails the handshake).
    #[tokio::test]
    async fn initialize_reports_dead_after_the_server_dies() {
        let plugin = fixture_plugin().await;
        PluginConnection::shutdown(&plugin).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), plugin.wait_dead())
            .await
            .expect("wait_dead hangs after shutdown");
        let err = plugin.initialize(&init_params()).await.unwrap_err();
        assert!(matches!(err, PeerError::Dead), "{err:?}");
    }

    /// A connect that never completes (server spawned but never answers
    /// initialize) must fail within CONNECT_TIMEOUT instead of hanging
    /// the sequential extension load loop (250ms under test).
    #[tokio::test]
    async fn connect_timeout_fails_instead_of_hanging() {
        let err = with_connect_timeout(
            "wedged",
            std::future::pending::<Result<McpConnection, String>>(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("wedged"), "{err}");
        assert!(err.contains("connect+initialize"), "{err}");
    }
}
