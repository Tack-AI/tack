//! Built-in minimal tack-RPC v3 plugin: e2e fixture for the dev tooling
//! (`tack ext dev` / `tack ext test` / `tack ext inspect`) and a protocol
//! reference implementation, mirroring v1's hidden `tack ext-demo-plugin`.

use serde_json::json;
use tack_ext_sdk::{Plugin, ToolSpec, allow, deny, text_output};

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
        .before_tool_call(|params, _cx| async move {
            let command = params.tool_call.arguments["command"].as_str().unwrap_or("");
            if params.tool_call.tool_name == "bash" && command.contains("rm -rf /") {
                return Ok(deny("refusing to delete the world"));
            }
            Ok(allow())
        })
        .run()
        .await
}
