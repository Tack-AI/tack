//! Server-initiated requests over an in-process duplex stream: the MCP
//! server calls back into the client (`sampling/createMessage`,
//! `elicitation/create`) while handling a tool call, and `TackClientHandler`
//! routes those to the configured `McpClientCallbacks`.
#![allow(clippy::unwrap_used)]
// Sampling types are deprecated upstream (SEP-2577) but remain the wire
// mechanism for server-initiated LLM requests.
#![allow(deprecated)]

use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};
use tack_tools::mcp::{ElicitationHandler, McpClientCallbacks, SamplingHandler, TackClientHandler};

// ---------------------------------------------------------------------------
// Test server: tools that call BACK into the client.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ReverseServer;

impl ServerHandler for ReverseServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tool = |name: &str| {
            Tool::new(
                name.to_string(),
                "reverse".to_string(),
                Arc::new(serde_json::Map::from_iter([(
                    "type".to_string(),
                    serde_json::Value::String("object".to_string()),
                )])),
            )
        };
        Ok(ListToolsResult::with_all_items(vec![
            tool("sample"),
            tool("elicit"),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            "sample" => {
                let params = CreateMessageRequestParams::new(
                    vec![SamplingMessage::user_text("hello from server")],
                    64,
                )
                .with_system_prompt("server system prompt")
                .with_temperature(0.3);
                let result = context
                    .peer
                    .create_message(params)
                    .await
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                let text = result
                    .message
                    .content
                    .first()
                    .and_then(|b| b.as_text())
                    .map(|t| t.text.clone())
                    .unwrap_or_default();
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "model={} stop={:?} text={text}",
                    result.model, result.stop_reason
                ))])
                .into())
            }
            "elicit" => {
                let schema = ElicitationSchema::builder()
                    .required_string("name")
                    .build()
                    .map_err(|e| ErrorData::internal_error(e, None))?;
                let result = context
                    .peer
                    .create_elicitation(ElicitRequestParams::FormElicitationParams {
                        meta: None,
                        message: "who are you?".to_string(),
                        requested_schema: schema,
                    })
                    .await
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "action={:?} content={}",
                    result.action,
                    result.content.unwrap_or(serde_json::Value::Null)
                ))])
                .into())
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool {other}"),
                None,
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Test callbacks
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ScriptedSampling {
    reply: String,
}

#[async_trait::async_trait]
impl SamplingHandler for ScriptedSampling {
    async fn create_message(
        &self,
        server: &str,
        params: CreateMessageRequestParams,
    ) -> Result<CreateMessageResult, String> {
        assert_eq!(server, "reverse");
        assert_eq!(
            params.system_prompt.as_deref(),
            Some("server system prompt")
        );
        assert_eq!(params.max_tokens, 64);
        Ok(CreateMessageResult::new(
            SamplingMessage::assistant_text(self.reply.clone()),
            "m".into(),
        )
        .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN))
    }
}

#[derive(Debug)]
struct AcceptElicitation;

#[async_trait::async_trait]
impl ElicitationHandler for AcceptElicitation {
    async fn elicit(
        &self,
        server: &str,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, String> {
        assert_eq!(server, "reverse");
        match params {
            ElicitRequestParams::FormElicitationParams { message, .. } => {
                assert_eq!(message, "who are you?");
                Ok(ElicitResult::new(ElicitationAction::Accept)
                    .with_content(serde_json::json!({ "name": "pi" })))
            }
            _ => Ok(ElicitResult::new(ElicitationAction::Decline)),
        }
    }
}

async fn spawn_pair(
    callbacks: McpClientCallbacks,
) -> rmcp::service::RunningService<rmcp::RoleClient, TackClientHandler> {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let running = ReverseServer.serve(server_io).await.unwrap();
        running.waiting().await.unwrap();
    });
    TackClientHandler::new("reverse".to_string(), callbacks)
        .serve(client_io)
        .await
        .unwrap()
}

async fn call_text(
    client: &rmcp::service::RunningService<rmcp::RoleClient, TackClientHandler>,
    tool: &str,
) -> String {
    let result = client
        .peer()
        .call_tool(CallToolRequestParams::new(tool.to_string()))
        .await
        .unwrap();
    result.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn sampling_request_reaches_client_callback() {
    let client = spawn_pair(McpClientCallbacks::default().with_sampling(Arc::new(
        ScriptedSampling {
            reply: "sampled reply".to_string(),
        },
    )))
    .await;

    let text = call_text(&client, "sample").await;
    assert_eq!(text, "model=m stop=Some(\"endTurn\") text=sampled reply");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn elicitation_request_reaches_client_callback() {
    let client =
        spawn_pair(McpClientCallbacks::default().with_elicitation(Arc::new(AcceptElicitation)))
            .await;
    let text = call_text(&client, "elicit").await;
    assert_eq!(text, r#"action=Accept content={"name":"pi"}"#);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn elicitation_without_handler_declines() {
    // No elicitation callback (headless mode): the server gets a graceful
    // Decline, not a protocol error.
    let client = spawn_pair(McpClientCallbacks::default()).await;
    let text = call_text(&client, "elicit").await;
    assert_eq!(text, "action=Decline content=null");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn sampling_without_handler_is_method_not_found() {
    let client = spawn_pair(McpClientCallbacks::default()).await;
    let err = client
        .peer()
        .call_tool(CallToolRequestParams::new("sample"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("-32601"), "{err}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn capabilities_advertised_only_with_callbacks() {
    use rmcp::ClientHandler;
    let bare = TackClientHandler::new("s".to_string(), McpClientCallbacks::default());
    let info = bare.get_info();
    assert!(info.capabilities.sampling.is_none());
    assert!(info.capabilities.elicitation.is_none());

    let full = TackClientHandler::new(
        "s".to_string(),
        McpClientCallbacks::default()
            .with_sampling(Arc::new(ScriptedSampling {
                reply: String::new(),
            }))
            .with_elicitation(Arc::new(AcceptElicitation)),
    );
    let info = full.get_info();
    assert!(info.capabilities.sampling.is_some());
    let elicitation = info.capabilities.elicitation.expect("elicitation cap");
    assert!(elicitation.form.is_some());
    assert!(elicitation.url.is_none());
}
