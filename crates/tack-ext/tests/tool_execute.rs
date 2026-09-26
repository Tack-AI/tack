//! Request/response integration tests: host→plugin calls (tool.execute,
//! command.invoke, intercept.tool_call) over the NDJSON transport, plus
//! the ExtTool agent-facing wrapper end to end.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use serde_json::{Value, json};
use tack_agent_core::AgentTool;
use tack_ext::{Envelope, ExtTool, ToolCallVerdict, ToolSpec};

/// tool.execute round trip: the wire params use the documented camelCase
/// shape, and the plugin's result value comes back untouched.
#[tokio::test]
async fn tool_execute_round_trip() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let (id, params) = side.read_request("tool.execute").await;
        // Wire shape: {name, toolCallId, arguments}.
        assert_eq!(params["name"], "ping");
        assert_eq!(params["toolCallId"], "call-42");
        assert_eq!(params["arguments"]["text"], "hello");
        assert!(params.get("tool_call_id").is_none(), "must be camelCase");
        side.send(&Envelope::result(id, json!({"content": "pong"})))
            .await;
    });

    let result = peer
        .call(
            "tool.execute",
            json!({"name": "ping", "toolCallId": "call-42", "arguments": {"text": "hello"}}),
        )
        .await
        .unwrap();
    assert_eq!(result["content"], "pong");
    driver.await.unwrap();
}

/// A protocol error response (`{"error": "..."}`) surfaces as Err with
/// the plugin's message.
#[tokio::test]
async fn tool_execute_error_response_propagates() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let (id, _params) = side.read_request("tool.execute").await;
        side.send(&Envelope::error(id, "tool exploded")).await;
    });

    let err = peer
        .call("tool.execute", json!({"name": "ping"}))
        .await
        .unwrap_err();
    assert_eq!(err, "tool exploded");
    driver.await.unwrap();
}

/// A response carrying neither result nor error is malformed; the caller
/// gets an error, and the peer stays usable for the next request.
#[tokio::test]
async fn malformed_response_errors_but_peer_survives() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let (id, _params) = side.read_request("tool.execute").await;
        side.send_raw(&format!(r#"{{"type":"response","id":{id}}}"#))
            .await;
        let (id2, _params) = side.read_request("command.invoke").await;
        side.send(&Envelope::result(id2, Value::Null)).await;
    });

    let err = peer
        .call("tool.execute", json!({"name": "ping"}))
        .await
        .unwrap_err();
    assert!(err.contains("malformed response"), "got: {err}");
    assert!(peer.is_alive());
    let ok = peer
        .call("command.invoke", json!({"name": "hello", "args": ""}))
        .await
        .unwrap();
    assert_eq!(ok, Value::Null);
    driver.await.unwrap();
}

/// Responses with an unknown id are dropped (late answer to an already
/// timed-out request); the matched request still resolves.
#[tokio::test]
async fn unknown_response_id_is_dropped() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        side.send(&Envelope::result(9999, json!("stale"))).await;
        let (id, _params) = side.read_request("tool.execute").await;
        side.send(&Envelope::result(id, json!("fresh"))).await;
    });

    let result = peer
        .call("tool.execute", json!({"name": "ping"}))
        .await
        .unwrap();
    assert_eq!(result, "fresh");
    driver.await.unwrap();
}

/// A present-but-null result is a REAL result (cancelled ui.input), not a
/// malformed response: it must reach the caller as Value::Null.
#[tokio::test]
async fn explicit_null_result_reaches_caller() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let (id, _params) = side.read_request("ui.input").await;
        side.send_raw(&format!(r#"{{"type":"response","id":{id},"result":null}}"#))
            .await;
    });

    let result = peer.call("ui.input", json!({"title": "t"})).await.unwrap();
    assert_eq!(result, Value::Null);
    driver.await.unwrap();
}

/// intercept.tool_call: all three verdict shapes round-trip into the
/// host's ToolCallVerdict enum.
#[tokio::test]
async fn intercept_tool_call_verdicts_round_trip() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let (id, params) = side.read_request("intercept.tool_call").await;
        assert_eq!(params["toolName"], "bash");
        assert_eq!(params["toolCallId"], "tc-1");
        side.send(&Envelope::result(id, json!({"action": "allow"})))
            .await;

        let (id, _params) = side.read_request("intercept.tool_call").await;
        side.send(&Envelope::result(
            id,
            json!({"action": "deny", "reason": "nope"}),
        ))
        .await;

        let (id, _params) = side.read_request("intercept.tool_call").await;
        side.send(&Envelope::result(
            id,
            json!({"action": "rewrite", "arguments": {"command": "ls"}}),
        ))
        .await;
    });

    let params =
        || json!({"toolCallId": "tc-1", "toolName": "bash", "arguments": {"command": "rm -rf /"}});
    let allow = peer.call("intercept.tool_call", params()).await.unwrap();
    assert!(matches!(
        serde_json::from_value::<ToolCallVerdict>(allow).unwrap(),
        ToolCallVerdict::Allow
    ));
    let deny = peer.call("intercept.tool_call", params()).await.unwrap();
    let ToolCallVerdict::Deny { reason } = serde_json::from_value::<ToolCallVerdict>(deny).unwrap()
    else {
        panic!("expected deny")
    };
    assert_eq!(reason, "nope");
    let rewrite = peer.call("intercept.tool_call", params()).await.unwrap();
    let ToolCallVerdict::Rewrite { arguments } =
        serde_json::from_value::<ToolCallVerdict>(rewrite).unwrap()
    else {
        panic!("expected rewrite")
    };
    assert_eq!(arguments["command"], "ls");
    driver.await.unwrap();
}

/// Concurrent requests with interleaved responses resolve by id, not by
/// arrival order.
#[tokio::test]
async fn concurrent_requests_match_responses_by_id() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let (id_x, params_x) = side.read_request("tool.execute").await;
        let (id_y, params_y) = side.read_request("tool.execute").await;
        // Arrival order is racy; identify by params, then answer the
        // second-issued request first.
        let (first, second) = if params_x["n"] == 1 {
            (id_x, id_y)
        } else {
            assert_eq!(params_y["n"], 1);
            (id_y, id_x)
        };
        side.send(&Envelope::result(second, json!("second-issued")))
            .await;
        side.send(&Envelope::result(first, json!("first-issued")))
            .await;
    });

    let a = {
        let peer = peer.clone();
        tokio::spawn(async move { peer.call("tool.execute", json!({"n": 1})).await })
    };
    let b = peer.call("tool.execute", json!({"n": 2})).await;
    assert_eq!(b.unwrap(), "second-issued");
    assert_eq!(a.await.unwrap().unwrap(), "first-issued");
    driver.await.unwrap();
}

/// Plugin→host requests (ui.notify & co.) reach HostServices and the
/// host's response comes back to the plugin.
#[tokio::test]
async fn plugin_requests_reach_host_services_and_get_responses() {
    let (peer, mut side, services) = connect();
    side.send(&Envelope::request(
        77,
        "ui.notify",
        json!({"message": "heads up", "level": "warning"}),
    ))
    .await;

    let response = side.read_envelope().await;
    let Envelope::Response { id, result, error } = response else {
        panic!("expected response")
    };
    assert_eq!(id, 77);
    assert_eq!(result, Some(Value::Null));
    assert!(error.is_none());

    let requests = services.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "ui.notify");
    assert_eq!(requests[0].1["level"], "warning");
    drop(requests);
    drop(peer);
}

/// Host events are forwarded to the plugin as fire-and-forget envelopes.
#[tokio::test]
async fn host_events_reach_plugin() {
    let (peer, mut side, _services) = connect();
    peer.send_event("agent_start", json!({"turn": 1}))
        .await
        .unwrap();
    let Envelope::Event { event, payload } = side.read_envelope().await else {
        panic!("expected event")
    };
    assert_eq!(event, "agent_start");
    assert_eq!(payload["turn"], 1);
}

/// ExtTool end to end: handshake-registered spec → agent-facing tool →
/// plugin result mapping (content blocks, isError).
#[tokio::test]
async fn ext_tool_executes_against_fake_plugin() {
    let (peer, mut side, _services) = connect();
    let driver = tokio::spawn(async move {
        let (id, params) = side.read_request("tool.execute").await;
        assert_eq!(params["name"], "ping");
        side.send(&Envelope::result(
            id,
            json!({"content": [{"type": "text", "text": "pong"}]}),
        ))
        .await;
        // Second call: isError result becomes a tool error.
        let (id, _params) = side.read_request("tool.execute").await;
        side.send(&Envelope::result(
            id,
            json!({"content": "kaboom", "isError": true}),
        ))
        .await;
    });

    let spec = ToolSpec {
        name: "ping".to_string(),
        label: None,
        description: "answers pong".to_string(),
        parameters: json!({"type": "object"}),
    };
    let tool = ExtTool::new("fake", spec, peer);
    assert_eq!(tool.name(), "ext__fake__ping");
    assert_eq!(tool.label(), "ping");
    assert_eq!(tool.description(), "answers pong");

    let result = tool
        .execute(
            "call-1",
            json!({}),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
    let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
        panic!("expected text")
    };
    assert_eq!(text, "pong");

    let err = tool
        .execute(
            "call-2",
            json!({}),
            tokio_util::sync::CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(err.contains("kaboom"), "got: {err}");
    driver.await.unwrap();
}

/// ExtTool honours cancellation: a pre-cancelled token aborts the
/// execute instead of waiting on the plugin.
#[tokio::test]
async fn ext_tool_aborts_on_cancelled_token() {
    let (peer, _side, _services) = connect();

    let spec = ToolSpec {
        name: "ping".to_string(),
        label: None,
        description: String::new(),
        parameters: json!({}),
    };
    let tool = ExtTool::new("fake", spec, peer);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let err = tool
        .execute("call-1", json!({}), token, &|_| {})
        .await
        .unwrap_err();
    assert_eq!(err, "Operation aborted");
}
