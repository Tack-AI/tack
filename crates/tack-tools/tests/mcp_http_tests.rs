//! End-to-end MCP over Streamable HTTP: an in-process rmcp server (axum +
//! StreamableHttpService) with a tool, a resource, and a prompt, exercised
//! through tack-tools' McpConnection + AgentTool proxies.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, ListPromptsResult, ListResourcesResult, ListToolsResult,
    Prompt, PromptArgument, PromptMessage, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, Role, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tack_tools::mcp::{McpServerSpec, connect, mcp_tools};

#[derive(Clone, Debug)]
struct TestServer;

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        );
        info.server_info.name = "test-server".into();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let schema: rmcp::model::JsonObject = serde_json::Map::from_iter([(
            "text".to_string(),
            serde_json::json!({ "type": "string" }),
        )]);
        Ok(ListToolsResult::with_all_items(vec![
            rmcp::model::Tool::new("echo_tool", "Echoes the input", schema),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let text = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(format!("echo: {text}")),
        ])))
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        let mut resource = Resource::new("test://hello", "hello");
        resource.description = Some("A test resource".into());
        resource.mime_type = Some("text/plain".into());
        Ok(ListResourcesResult::with_all_items(vec![resource]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ReadResourceResponse, rmcp::ErrorData> {
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::TextResourceContents {
                uri: request.uri.clone(),
                mime_type: Some("text/plain".into()),
                text: "resource body here".into(),
                meta: None,
            }],
        )))
    }

    async fn list_prompts(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListPromptsResult, rmcp::ErrorData> {
        let mut arg = PromptArgument::new("name");
        arg.description = Some("who to greet".into());
        arg.required = Some(true);
        let prompt = Prompt::new("greet", Some("Greeting prompt"), Some(vec![arg]));
        Ok(ListPromptsResult::with_all_items(vec![prompt]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<GetPromptResponse, rmcp::ErrorData> {
        let name = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("world")
            .to_string();
        Ok(GetPromptResponse::Complete(GetPromptResult::new(vec![
            PromptMessage::new_text(Role::User, format!("Say hi to {name}")),
        ])))
    }
}

/// Start the test server on an ephemeral port; return its URL.
async fn start_server() -> String {
    let service = StreamableHttpService::new(
        || Ok(TestServer),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/mcp")
}

async fn start_auth_server() -> (
    String,
    Arc<std::sync::Mutex<Vec<(axum::http::Method, Option<String>)>>>,
) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let service = StreamableHttpService::new(
        || Ok(TestServer),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let observed = requests.clone();
    let app = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let observed = observed.clone();
                async move {
                    let authorization = request
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    observed
                        .lock()
                        .unwrap()
                        .push((request.method().clone(), authorization));
                    next.run(request).await
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/mcp"), requests)
}

/// mcp.json `exposure` / `toolExposure` reach the built AgentTools:
/// deferred tools carry the pool marker (server-level for meta/prompt
/// tools), hidden tools are never built.
#[tokio::test]
async fn exposure_marks_deferred_and_drops_hidden() {
    use tack_tools::mcp::McpExposure;

    let spec = McpServerSpec::http("test".into(), start_server().await, vec![])
        .with_exposure(McpExposure::Deferred)
        .with_tool_exposure(vec![("echo_tool".to_string(), McpExposure::Hidden)]);
    let conn = connect(&spec).await.unwrap();
    let tools = mcp_tools(&[Arc::new(conn)]);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(
        !names.contains(&"mcp__test__echo_tool"),
        "hidden tool is never built: {names:?}"
    );
    assert!(names.contains(&"mcp__test__list_resources"));
    assert!(names.contains(&"mcp__test__prompt__greet"));
    assert!(
        tools.iter().all(|t| t.starts_deferred()),
        "server-level deferred marks everything built"
    );

    // A fully hidden server exposes nothing.
    let spec = McpServerSpec::http("test2".into(), start_server().await, vec![])
        .with_exposure(McpExposure::Hidden);
    let conn = connect(&spec).await.unwrap();
    assert!(mcp_tools(&[Arc::new(conn)]).is_empty());
}

#[tokio::test]
async fn http_connect_discovers_capabilities() {
    let url = start_server().await;
    let conn = connect(&McpServerSpec::http("test".into(), url, vec![]))
        .await
        .unwrap();
    assert_eq!(conn.tools().len(), 1);
    assert!(conn.has_resources());
    assert_eq!(conn.prompts().len(), 1);

    let tools = mcp_tools(&[Arc::new(conn)]);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(names.contains(&"mcp__test__echo_tool"), "{names:?}");
    assert!(names.contains(&"mcp__test__list_resources"), "{names:?}");
    assert!(names.contains(&"mcp__test__read_resource"), "{names:?}");
    assert!(names.contains(&"mcp__test__prompt__greet"), "{names:?}");

    // Call the proxied tool.
    let echo = tools
        .iter()
        .find(|t| t.name() == "mcp__test__echo_tool")
        .unwrap();
    let result = echo
        .execute(
            "call-1",
            serde_json::json!({ "text": "hi" }),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
    let text = text_of(&result);
    assert_eq!(text, "echo: hi");

    // Read the resource.
    let read = tools
        .iter()
        .find(|t| t.name() == "mcp__test__read_resource")
        .unwrap();
    let result = read
        .execute(
            "call-2",
            serde_json::json!({ "uri": "test://hello" }),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(text.contains("resource body here"), "{text}");

    // Get the prompt.
    let prompt = tools
        .iter()
        .find(|t| t.name() == "mcp__test__prompt__greet")
        .unwrap();
    let result = prompt
        .execute(
            "call-3",
            serde_json::json!({ "name": "pi" }),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(text.contains("Say hi to pi"), "{text}");
}

/// Provider credentials are request-scoped, not a connect-time header. This
/// exercises two calls through one live rmcp session and also checks the
/// documented static-header fallback when provider auth becomes unavailable.
#[tokio::test]
async fn provider_token_changes_on_one_live_http_connection() {
    let (url, requests) = start_auth_server().await;
    let token = Arc::new(std::sync::Mutex::new(Some("tok1".to_string())));
    let resolver = tack_tools::mcp::McpProviderTokenResolver::new({
        let token = token.clone();
        move || {
            let token = token.clone();
            Box::pin(async move { token.lock().unwrap().clone() })
        }
    });
    let spec = McpServerSpec::http(
        "test".into(),
        url,
        vec![("Authorization".into(), "Bearer static".into())],
    )
    .with_auth_provider("test-provider".into())
    .with_provider_token_resolver(resolver);
    let conn = Arc::new(connect(&spec).await.unwrap());
    let tools = mcp_tools(std::slice::from_ref(&conn));
    let echo = tools
        .iter()
        .find(|tool| tool.name() == "mcp__test__echo_tool")
        .unwrap();

    echo.execute(
        "first",
        serde_json::json!({"text": "first"}),
        tokio_util::sync::CancellationToken::new(),
        &|_| {},
    )
    .await
    .unwrap();
    let first_count = requests.lock().unwrap().len();
    assert!(first_count > 0);
    {
        let observed = requests.lock().unwrap();
        assert!(
            observed
                .iter()
                .all(|(_, auth)| auth.as_deref() == Some("Bearer tok1")),
            "provider auth must override the static header on every initial request"
        );
        assert!(
            observed
                .iter()
                .any(|(method, _)| *method == axum::http::Method::POST)
        );
        assert!(
            observed
                .iter()
                .any(|(method, _)| *method == axum::http::Method::GET),
            "the fixture also exercises rmcp's streaming GET path: {observed:?}"
        );
    }

    *token.lock().unwrap() = Some("tok2".into());
    echo.execute(
        "second",
        serde_json::json!({"text": "second"}),
        tokio_util::sync::CancellationToken::new(),
        &|_| {},
    )
    .await
    .unwrap();
    assert!(!conn.is_closed(), "the second call reused the MCP session");
    {
        let observed = requests.lock().unwrap();
        assert!(observed.len() > first_count);
        assert!(
            observed[first_count..]
                .iter()
                .all(|(_, auth)| auth.as_deref() == Some("Bearer tok2")),
            "requests after the token change: {:?}",
            &observed[first_count..]
        );
    }

    *token.lock().unwrap() = None;
    echo.execute(
        "third",
        serde_json::json!({"text": "third"}),
        tokio_util::sync::CancellationToken::new(),
        &|_| {},
    )
    .await
    .unwrap();
    {
        let observed = requests.lock().unwrap();
        assert_eq!(
            observed.last().unwrap().1.as_deref(),
            Some("Bearer static"),
            "a missing provider token leaves the static Authorization header intact"
        );
    }
    conn.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some((_, auth)) = requests
                .lock()
                .unwrap()
                .iter()
                .find(|(method, _)| *method == axum::http::Method::DELETE)
                .cloned()
            {
                assert_eq!(auth.as_deref(), Some("Bearer static"));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("provider-authenticated DELETE session cleanup");
}

fn text_of(result: &tack_agent_core::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Regression: resource/prompt/list meta-tool outputs must be wrapped as
/// untrusted content (like McpTool results) and the shared flag must be set —
/// previously only McpTool::execute did this.
#[tokio::test]
async fn meta_tool_outputs_are_wrapped_untrusted() {
    use tack_tools::mcp::mcp_tools_with;
    let url = start_server().await;
    let conn = connect(&McpServerSpec::http("test".into(), url, vec![]))
        .await
        .unwrap();
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tools = mcp_tools_with(&[Arc::new(conn)], Some(flag.clone()));

    for name in [
        "mcp__test__echo_tool",
        "mcp__test__list_resources",
        "mcp__test__read_resource",
        "mcp__test__prompt__greet",
    ] {
        let tool = tools.iter().find(|t| t.name() == name).unwrap();
        let params = if name.contains("read_resource") {
            serde_json::json!({ "uri": "test://hello" })
        } else if name.contains("prompt") {
            serde_json::json!({ "name": "pi" })
        } else if name.contains("echo_tool") {
            serde_json::json!({ "text": "hi" })
        } else {
            serde_json::json!({})
        };
        let result = tool
            .execute(
                "c",
                params,
                tokio_util::sync::CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(
            text.starts_with("<untrusted_content source=\"mcp://test/"),
            "{name}: {text}"
        );
        assert!(text.ends_with("</untrusted_content>"), "{name}: {text}");
    }
    assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
}

/// Regression: MCP tool results must respect the same truncation budget as
/// bash/read output (an unbounded result could flood the context).
#[tokio::test]
async fn oversized_tool_result_is_truncated() {
    use tack_tools::mcp::mcp_tools_with;
    let url = start_server().await;
    let conn = connect(&McpServerSpec::http("test".into(), url, vec![]))
        .await
        .unwrap();
    let tools = mcp_tools_with(&[Arc::new(conn)], None);
    let echo = tools
        .iter()
        .find(|t| t.name() == "mcp__test__echo_tool")
        .unwrap();
    let big = "y\n".repeat(10_000);
    let result = echo
        .execute(
            "c",
            serde_json::json!({ "text": big }),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(
        text.contains("[MCP output truncated:"),
        "len {}: {text}",
        text.len()
    );
    assert!(text.len() < big.len(), "len {}", text.len());
}
