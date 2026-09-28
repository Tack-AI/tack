//! Built-in feature-rich tack-RPC v3 demo plugin: e2e fixture for the
//! extension host tests and a protocol reference implementation, covering
//! tools, commands, hooks, events, widgets, autocomplete, and host
//! services (ui dialogs, notifications).

use serde_json::json;
use tack_ext_sdk::{
    AutocompleteProvideResult, AutocompleteProviderSpec, AutocompleteSuggestion, Plugin, ToolSpec,
    WidgetKind, WidgetSpec, allow, deny, text_output,
};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    Plugin::builder("tack-v3-demo")
        .version("0.1.0")
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
        )
        .run()
        .await
}
