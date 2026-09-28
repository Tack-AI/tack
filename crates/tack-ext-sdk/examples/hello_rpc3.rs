//! A minimal tack-RPC v3 plugin: one echo tool plus a path-guard hook.
//! Run it under a host with an `extension.json` like:
//!
//! ```json
//! { "name": "hello-rpc3", "command": "hello-rpc3", "args": [] }
//! ```

use serde_json::json;
use tack_ext_sdk::{Plugin, ToolSpec, allow, deny, text_output};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    Plugin::builder("hello-rpc3")
        .version("0.1.0")
        .tool(
            ToolSpec {
                name: "hello.echo".to_string(),
                label: None,
                description: "Echo the arguments back to the model".to_string(),
                parameters: json!({"type": "object"}),
            },
            |params, _cx| async move { Ok(text_output(format!("echo: {}", params.arguments))) },
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
