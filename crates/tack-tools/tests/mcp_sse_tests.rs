//! End-to-end MCP over the legacy SSE transport (spec 2024-11-05): a
//! hand-rolled axum server speaks GET /sse (endpoint + message events) and
//! POST /messages; the client is tack-tools' hand-rolled SseClientTransport.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::Sse;
use axum::response::sse::Event;
use tack_tools::mcp::{McpServerSpec, connect, mcp_tools};
use tokio::sync::{Mutex, mpsc};

/// Shared state: each connected SSE stream gets a sender; POSTs push their
/// JSON-RPC responses into it.
#[derive(Clone, Default)]
struct ServerState {
    streams: Arc<Mutex<Vec<mpsc::UnboundedSender<String>>>>,
}

async fn sse_handler(
    State(state): State<ServerState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    state.streams.lock().await.push(tx);
    // The endpoint event tells the client where to POST.
    let endpoint = futures_util::stream::once(async {
        Ok(Event::default().event("endpoint").data("/messages/"))
    });
    let messages = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv()
            .await
            .map(|message| (Ok(Event::default().event("message").data(message)), rx))
    });
    use futures_util::StreamExt;
    Sse::new(endpoint.chain(messages))
}

async fn post_handler(
    State(state): State<ServerState>,
    body: axum::Json<serde_json::Value>,
) -> axum::http::StatusCode {
    let request = body.0;
    // Notifications have no id → nothing to send back.
    let Some(id) = request.get("id").cloned() else {
        return axum::http::StatusCode::ACCEPTED;
    };
    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "legacy-sse-test", "version": "0.1.0" }
        }),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "echo_tool",
                "description": "Echoes the input",
                "inputSchema": {
                    "type": "object",
                    "properties": { "text": { "type": "string" } }
                }
            }]
        }),
        "tools/call" => {
            let text = request
                .pointer("/params/arguments/text")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            serde_json::json!({
                "content": [{ "type": "text", "text": format!("echo: {text}") }]
            })
        }
        _ => serde_json::json!({ "error": { "code": -32601, "message": "no such method" } }),
    };
    let response = if result.get("error").is_some() {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": result["error"] })
    } else {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
    };
    let text = serde_json::to_string(&response).unwrap();
    let streams = state.streams.lock().await;
    for stream in streams.iter() {
        let _ = stream.send(text.clone());
    }
    axum::http::StatusCode::ACCEPTED
}

async fn spawn_server() -> String {
    let app = Router::new()
        .route("/sse", axum::routing::get(sse_handler))
        .route("/messages/", axum::routing::post(post_handler))
        .with_state(ServerState::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/sse", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    url
}

#[tokio::test]
async fn legacy_sse_initialize_list_and_call() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rmcp=debug,tack_tools=debug")
        .with_test_writer()
        .try_init();
    let url = spawn_server().await;
    let spec = McpServerSpec::sse("legacy".into(), url, Vec::new());
    let connection = connect(&spec).await.expect("connect over legacy SSE");

    assert_eq!(connection.tools().len(), 1);
    assert_eq!(connection.tools()[0].name, "echo_tool");

    let tools = mcp_tools(&[std::sync::Arc::new(connection)]);
    let echo = tools
        .iter()
        .find(|t| t.name().contains("echo_tool"))
        .expect("echo tool");
    let result = echo
        .execute(
            "call-1",
            serde_json::json!({ "text": "hello-sse" }),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await
        .expect("tool call");
    let text = result
        .content
        .iter()
        .filter_map(|b| match b {
            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(text.contains("echo: hello-sse"), "{text}");
}
