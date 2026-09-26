//! MCP server mode: handshake + tool listing + stats/reset roundtrip over a
//! duplex stream (no LLM involved).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use tack_ai::provider::{Provider, StreamOptions};
use tack_ai::stream::AssistantMessageEventStream;
use tack_ai::types::{Context, Model};
use tack_app::mcp_serve::TackMcpServer;

#[derive(Debug)]
struct NullProvider;

impl Provider for NullProvider {
    fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        panic!("no LLM calls in this test")
    }
}

fn test_model() -> Model {
    tack_app::model::resolve_model("anthropic", Some("k3"), std::path::Path::new(".")).unwrap()
}

#[tokio::test]
async fn mcp_server_lists_and_calls_tools() {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let server = TackMcpServer::for_test(
            test_model(),
            Arc::new(NullProvider),
            Arc::new(tack_ai::oauth::StaticAuth::from(None)),
        );
        let running = server.serve(server_io).await.unwrap();
        running.waiting().await.unwrap();
    });

    let client = ().serve(client_io).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    assert!(names.contains(&"prompt".to_string()), "{names:?}");
    assert!(
        names.contains(&"get_session_stats".to_string()),
        "{names:?}"
    );
    assert!(names.contains(&"reset_session".to_string()), "{names:?}");
    assert!(names.contains(&"read_context".to_string()), "{names:?}");
    assert!(
        names.contains(&"list_available_tools".to_string()),
        "{names:?}"
    );

    // read_context + list_available_tools work without an LLM.
    let context = client
        .call_tool(CallToolRequestParams::new("read_context"))
        .await
        .unwrap();
    assert!(format!("{:?}", context.content).contains("empty context"));
    let tool_list = client
        .call_tool(CallToolRequestParams::new("list_available_tools"))
        .await
        .unwrap();
    assert!(format!("{:?}", tool_list.content).contains("bash"));

    let stats = client
        .call_tool(CallToolRequestParams::new("get_session_stats"))
        .await
        .unwrap();
    let text = format!("{:?}", stats.content);
    assert!(text.contains("inputTokens"), "{text}");

    let reset = client
        .call_tool(CallToolRequestParams::new("reset_session"))
        .await
        .unwrap();
    assert!(format!("{:?}", reset.content).contains("session reset"));
}
