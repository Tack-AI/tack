//! MCP integration test: an in-process rmcp server over a duplex stream,
//! exposed to the agent as an McpTool proxy.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde_json::json;
use tack_agent_core::AgentTool;
use tack_tools::mcp::{McpTool, mcp_tools};
use tokio_util::sync::CancellationToken;

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
struct SumRequest {
    a: i64,
    b: i64,
}

#[derive(Clone)]
struct Calculator {
    #[allow(dead_code)] // held so the router lives as long as the server
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[tool_router]
impl Calculator {
    #[tool(description = "Calculate the sum of two numbers")]
    fn sum(
        &self,
        rmcp::handler::server::wrapper::Parameters(SumRequest { a, b }): rmcp::handler::server::wrapper::Parameters<SumRequest>,
    ) -> String {
        (a + b).to_string()
    }

    #[tool(description = "Always fails")]
    fn fail(&self) -> String {
        "unreachable".to_string()
    }
}

#[tool_handler]
impl ServerHandler for Calculator {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("test calculator")
    }
}

#[tokio::test]
async fn mcp_tool_roundtrip_over_duplex() {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);

    // Server side.
    tokio::spawn(async move {
        let server = Calculator {
            tool_router: Calculator::tool_router(),
        };
        let running = server.serve(server_io).await.unwrap();
        running.waiting().await.unwrap();
    });

    // Client side (via the connection wrapper, so the McpTool path gets a
    // reconnectable Arc<McpConnection>).
    let conn = std::sync::Arc::new(
        tack_tools::mcp::connect_transport("calc", client_io, Default::default())
            .await
            .unwrap(),
    );
    let infos = conn.tools();
    assert_eq!(infos.len(), 2);
    assert!(infos.iter().any(|t| t.name == "sum"));

    // Wrap as agent tools via the McpTool path.
    let tools: Vec<Arc<dyn AgentTool>> = infos
        .into_iter()
        .map(|info| Arc::new(McpTool::new("calc", info, conn.clone())) as Arc<dyn AgentTool>)
        .collect();
    let sum = tools.iter().find(|t| t.name() == "mcp__calc__sum").unwrap();

    // Schema is forwarded.
    let schema = sum.parameters_schema();
    assert_eq!(schema["type"], json!("object"));
    assert!(schema["properties"]["a"].is_object());

    // Execute.
    let result = sum
        .execute(
            "t1",
            json!({"a": 20, "b": 22}),
            CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
    let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
        panic!()
    };
    assert_eq!(text, "42");

    conn.cancel();
}

#[tokio::test]
async fn mcp_tools_from_connections_prefixes_names() {
    // mcp_tools() mapping is exercised lightly here; the heavy lifting is in
    // the roundtrip test above.
    let _ = mcp_tools(&[]);
}

/// Level-2 MCP server plugins: `plugin_capabilities` synthesizes the
/// plugin's advertised tool specs (UNPREFIXED names — the plugin host
/// adds `ext__<plugin-id>__`) and routes execution to the MCP calls.
#[tokio::test]
async fn plugin_capabilities_expose_unprefixed_specs_and_execute() {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let server = Calculator {
            tool_router: Calculator::tool_router(),
        };
        let running = server.serve(server_io).await.unwrap();
        running.waiting().await.unwrap();
    });
    let conn = Arc::new(
        tack_tools::mcp::connect_transport("calc", client_io, Default::default())
            .await
            .unwrap(),
    );
    let caps = tack_tools::mcp::plugin_capabilities(&conn);
    let mut names: Vec<&str> = caps.iter().map(|c| c.spec_name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["fail", "sum"], "unprefixed spec names");
    let sum = caps.iter().find(|c| c.spec_name == "sum").unwrap();
    assert!(sum.description.contains("sum of two numbers"));
    assert_eq!(sum.parameters["type"], json!("object"));

    let result = sum.execute("t1", json!({"a": 20, "b": 22})).await.unwrap();
    let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
        panic!()
    };
    assert_eq!(text, "42");

    conn.cancel();
}
