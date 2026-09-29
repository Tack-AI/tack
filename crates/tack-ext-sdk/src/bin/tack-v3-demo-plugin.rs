//! Built-in feature-rich tack-RPC v3 demo plugin: e2e fixture for the
//! extension host tests and a protocol reference implementation, covering
//! tools, commands, hooks, events, widgets, autocomplete, host services
//! (ui dialogs, notifications), and the P7 provider bridge.
//!
//! Provider fixture (env-gated): when `TACK_DEMO_PROVIDER` carries a JSON
//! spec (`{"id": "demo-provider", "models": […CustomModel…]}`), the plugin
//! declares `provider.stream`, registers the provider with `bridge: true`,
//! and serves a deterministic fake model driven by the last user message:
//!
//! - `<text>`: thinking + text deltas echoing the text, `done`;
//! - `tool:<name>`: one tool call, `done` (stopReason `toolUse`);
//! - `error`: a terminal `error` event;
//! - `no-terminal`: returns without a terminal event (SDK auto-error);
//! - `rate-limit`: a `provider/event` (rateLimited), then the echo.
//!
//! `TACK_DEMO_PROVIDER_DELAY_MS` slows the stream (cancel tests poll the
//! stream's cancellation between deltas and answer with an `aborted`
//! terminal).

use serde_json::{Value, json};
use tack_ext_sdk::{
    AutocompleteProvideResult, AutocompleteProviderSpec, AutocompleteSuggestion, MetricOperation,
    MetricsDeclaration, MetricsRecorder, Plugin, ProviderEventKind, ProviderStreamParams, ToolSpec,
    WidgetKind, WidgetSpec, allow, deny, text_output,
};

/// The metrics sidecar declaration (the host validates every drained
/// line against this schema — see plugin_metrics.rs).
fn metrics_declaration() -> MetricsDeclaration {
    MetricsDeclaration {
        operations: [(
            "demo.metric".to_string(),
            MetricOperation {
                description: Some("one demo measurement".to_string()),
                dimensions: Some(
                    [(
                        "outcome".to_string(),
                        vec!["ok".to_string(), "error".to_string()],
                    )]
                    .into_iter()
                    .collect(),
                ),
            },
        )]
        .into_iter()
        .collect(),
    }
}

// ---------------------------------------------------------------------------
// Fake provider model (P7 fixture)
// ---------------------------------------------------------------------------

fn zero_usage() -> Value {
    json!({
        "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
    })
}

/// A fresh pending assistant message for the served model (the streaming
/// accumulator the `partial` events carry).
fn pending_partial(model: &Value) -> Value {
    json!({
        "content": [],
        "api": model["api"].as_str().unwrap_or(""),
        "provider": model["provider"].as_str().unwrap_or(""),
        "model": model["id"].as_str().unwrap_or(""),
        "usage": zero_usage(),
        "stopReason": "pending",
        "timestamp": 0,
    })
}

/// The last user message's text (user content is a string or text blocks).
fn last_user_text(context: &Value) -> String {
    let Some(messages) = context["messages"].as_array() else {
        return String::new();
    };
    for message in messages.iter().rev() {
        if message["role"].as_str() != Some("user") {
            continue;
        }
        match &message["content"] {
            Value::String(text) => return text.clone(),
            Value::Array(blocks) => {
                return blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("");
            }
            _ => {}
        }
    }
    String::new()
}

fn event_delay() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("TACK_DEMO_PROVIDER_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    )
}

/// Pause between scripted events; resolves `true` early on cancellation.
async fn pause_cancelable(cx: &tack_ext_sdk::ProviderStreamCx) -> bool {
    let delay = event_delay();
    if delay.is_zero() {
        return cx.is_cancelled();
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => cx.is_cancelled(),
        _ = cx.cancelled() => true,
    }
}

/// Emit the terminal aborted event (the plugin honored
/// `provider/streamCancel` in time).
async fn emit_aborted(events: &tack_ext_sdk::ProviderEvents, partial: Value) {
    let mut error = partial;
    error["stopReason"] = json!("aborted");
    error["errorMessage"] = json!("demo stream aborted");
    let _ = events
        .send(json!({"type": "error", "reason": "aborted", "error": error}))
        .await;
}

/// The deterministic fake model: one scripted turn per `provider/stream`.
async fn run_fake_model(
    params: ProviderStreamParams,
    events: tack_ext_sdk::ProviderEvents,
    cx: tack_ext_sdk::ProviderStreamCx,
) -> Result<(), tack_ext_sdk::Error> {
    let text = last_user_text(&params.context);
    let mut partial = pending_partial(&params.model);
    if text == "rate-limit" {
        let _ = cx
            .host()
            .provider_event(
                params.model["provider"].as_str().unwrap_or("demo-provider"),
                ProviderEventKind::RateLimited,
                "demo rate limit: 90% used",
                None,
            )
            .await;
    }
    events
        .send(json!({"type": "start", "partial": partial}))
        .await?;
    macro_rules! step {
        ($event:expr) => {
            if pause_cancelable(&cx).await {
                emit_aborted(&events, partial).await;
                return Ok(());
            }
            events.send($event).await?;
        };
    }
    if text == "error" {
        step!(json!({"type": "error", "reason": "error", "error":
        pending_partial(&params.model).tap_mut(|p| {
            p["stopReason"] = json!("error");
            p["errorMessage"] = json!("demo provider error");
        })}));
        return Ok(());
    }
    if text == "no-terminal" {
        // SDK terminal enforcement fires the automatic error event.
        return Ok(());
    }
    if let Some(tool) = text.strip_prefix("tool:") {
        let call =
            json!({"type": "toolCall", "id": "demo-call-1", "name": tool, "arguments": {"x": 1}});
        step!(json!({"type": "toolCallStart", "contentIndex": 0, "partial": partial}));
        step!(
            json!({"type": "toolCallDelta", "contentIndex": 0, "delta": "{\"x\":1}", "partial": partial})
        );
        partial["content"] = json!([call]);
        step!(
            json!({"type": "toolCallEnd", "contentIndex": 0, "toolCall": call, "partial": partial})
        );
        partial["stopReason"] = json!("toolUse");
        events.done(partial).await?;
        return Ok(());
    }
    let thinking = format!("thinking: {text}");
    step!(json!({"type": "thinkingStart", "contentIndex": 0, "partial": partial}));
    step!(
        json!({"type": "thinkingDelta", "contentIndex": 0, "delta": thinking, "partial": partial})
    );
    partial["content"] = json!([{"type": "thinking", "thinking": thinking}]);
    step!(
        json!({"type": "thinkingEnd", "contentIndex": 0, "content": thinking, "partial": partial})
    );
    let answer = format!("echo: {text}");
    step!(json!({"type": "textStart", "contentIndex": 1, "partial": partial}));
    step!(json!({"type": "textDelta", "contentIndex": 1, "delta": "echo: ", "partial": partial}));
    step!(json!({"type": "textDelta", "contentIndex": 1, "delta": text, "partial": partial}));
    partial["content"] = json!([
        {"type": "thinking", "thinking": thinking},
        {"type": "text", "text": answer}
    ]);
    step!(json!({"type": "textEnd", "contentIndex": 1, "content": answer, "partial": partial}));
    partial["stopReason"] = json!("stop");
    partial["usage"]["input"] = json!(10);
    partial["usage"]["output"] = json!(5);
    partial["usage"]["totalTokens"] = json!(15);
    events.done(partial).await?;
    Ok(())
}

/// `ProviderEvents`-independent mutable tap helper (keeps the scripted
/// events terse).
trait TapMut {
    fn tap_mut(&mut self, f: impl FnOnce(&mut Self)) -> Self;
}
impl TapMut for Value {
    fn tap_mut(&mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(self);
        self.clone()
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut builder = Plugin::builder("tack-v3-demo")
        .version("0.1.0")
        .metrics(metrics_declaration())
        .tool(
            ToolSpec {
                name: "hello.metric".to_string(),
                label: None,
                description:
                    "Append one demo.metric measurement to the metrics sidecar scratch file"
                        .to_string(),
                parameters: json!({"type": "object"}),
            },
            |_params, cx| async move {
                let Some(metrics) = cx.capabilities().metrics.clone() else {
                    return Err(tack_ext_sdk::Error::from("host offered no metrics sidecar"));
                };
                let mut recorder = MetricsRecorder::new(&metrics.scratch_file)
                    .map_err(|e| tack_ext_sdk::Error::from(e.to_string()))?;
                recorder
                    .record("demo.metric", 1.0, &[("outcome", "ok")])
                    .map_err(|e| tack_ext_sdk::Error::from(e.to_string()))?;
                Ok(text_output("recorded demo.metric"))
            },
        )
        .tool(
            ToolSpec {
                name: "hello.echo".to_string(),
                label: None,
                description: "Echo the arguments back".to_string(),
                parameters: json!({"type": "object"}),
            },
            |params, _cx| async move { Ok(text_output(format!("echo: {}", params.arguments))) },
        )
        .tool(
            ToolSpec {
                name: "hello.select".to_string(),
                label: None,
                description: "Ask the user to pick an option (ui/select)".to_string(),
                parameters: json!({"type": "object"}),
            },
            |_params, cx| async move {
                let picked = cx
                    .host()
                    .select(
                        "pick one".to_string(),
                        vec!["a".to_string(), "b".to_string()],
                    )
                    .await
                    .map_err(tack_ext_sdk::Error::from)?;
                Ok(text_output(format!(
                    "picked: {}",
                    picked.unwrap_or_default()
                )))
            },
        )
        .command(
            "hello",
            Some("Say hello".to_string()),
            |_params, cx| async move {
                cx.host()
                    .notify("hello from the demo plugin!".to_string(), None)
                    .await
                    .map_err(tack_ext_sdk::Error::from)?;
                Ok(json!({"ok": true}))
            },
        )
        .before_tool_call(|params, _cx| async move {
            let command = params.tool_call.arguments["command"].as_str().unwrap_or("");
            if params.tool_call.tool_name == "bash" && command.contains("rm -rf /") {
                return Ok(deny("refusing to delete the world"));
            }
            Ok(allow())
        })
        .events(&["agentStart"], |params, cx| async move {
            let _ = params;
            let _ = cx
                .host()
                .notify("demo plugin saw agentStart".to_string(), None)
                .await;
        })
        .widget(WidgetSpec {
            id: "demo-status".to_string(),
            r#type: WidgetKind::StatusLineSegment,
            priority: Some(10),
            title: None,
            visible: None,
            initial: Some(json!({"text": "demo:ok", "style": "info"})),
        })
        .widget(WidgetSpec {
            id: "demo-list".to_string(),
            r#type: WidgetKind::ListPanel,
            priority: None,
            title: Some("Demo items".to_string()),
            visible: Some(true),
            initial: Some(json!({"items": [
                {"id": "a", "label": "Alpha", "detail": "first"},
                {"id": "b", "label": "Beta"}
            ]})),
        })
        .on_widget_action(|params, cx| async move {
            let _ = cx
                .host()
                .notify(
                    format!(
                        "demo plugin saw widget.action {} {}",
                        params.action,
                        params.item_id.unwrap_or_default()
                    ),
                    None,
                )
                .await;
        })
        .autocomplete(
            AutocompleteProviderSpec {
                id: "hash".to_string(),
                trigger: "#".to_string(),
                description: Some("Demo tags".to_string()),
            },
            |params, _cx| async move {
                let tags = ["#alpha", "#beta", "#wasm"];
                let query = params.query.to_lowercase();
                let suggestions = tags
                    .iter()
                    .filter(|t| query.is_empty() || t.contains(&query))
                    .map(|t| AutocompleteSuggestion {
                        value: (*t).to_string(),
                        label: format!("{t} demo tag"),
                        detail: Some("demo".to_string()),
                        insert_text: None,
                    })
                    .collect();
                Ok(AutocompleteProvideResult { suggestions })
            },
        );
    // Provider fixture: only when TACK_DEMO_PROVIDER carries the spec —
    // the capability is declared (and the provider registered) only then,
    // so the plain demo scenarios see an unchanged handshake.
    if let Ok(spec) = std::env::var("TACK_DEMO_PROVIDER")
        && let Ok(spec) = serde_json::from_str::<Value>(&spec)
    {
        builder = builder
            .provider_stream(|params, events, cx| async move {
                run_fake_model(params, events, cx).await
            })
            .on_ready(move |cx| {
                let spec = spec.clone();
                async move {
                    let provider = json!({
                        "id": spec["id"].as_str().unwrap_or("demo-provider"),
                        "bridge": true,
                        "models": spec["models"].clone(),
                    });
                    if let Err(e) = cx.host().register_provider(provider).await {
                        let _ = cx
                            .host()
                            .log(
                                tack_ext_sdk::LogLevel::Error,
                                format!("demo provider registration failed: {e}"),
                            )
                            .await;
                    }
                }
            });
    }
    builder.run().await
}
