# tack-plugin

Python SDK for **tack-RPC v3** plugins — build a
[tack](https://github.com/Tack-AI/tack) Level-3 plugin without ever
seeing a JSON-RPC envelope. Standard library only; protocol types
(`tack_plugin/types.py`, TypedDicts + enums) are generated from
`protocol/tack-rpc.openrpc.json`.

```python
from tack_plugin import Plugin, text_output, allow, deny

plugin = (
    Plugin("hello", version="0.1.0")
    .tool(
        {"name": "hello.echo", "description": "Echo the arguments",
         "parameters": {"type": "object"}},
        lambda params, cx: text_output(f"echo: {params['arguments']}"),
    )
    .before_tool_call(lambda params, cx: (
        deny("refusing to delete the world")
        if params["toolCall"]["toolName"] == "bash"
        and "rm -rf /" in (params["toolCall"]["arguments"] or {}).get("command", "")
        else allow()
    ))
)

plugin.run()  # serves over stdio (asyncio under the hood)
```

Handler surface: `tool`, `command`, `before_tool_call`,
`after_tool_call`, `transform_context`, `approval_review`, `events`,
`widget` / `on_widget_action`, `autocomplete`, `config_schema`,
`metrics`. Handlers may be sync or async. The handler context (`cx`)
exposes the negotiated host environment and the typed host client
(`cx.host.select/exec/session/snapshot/config/…`).

**stdout is the RPC bus** — never `print()` from a plugin; use
`cx.host.log()` / `cx.host.warn()`.

Tests: `python3 -m unittest discover -s tests` (no model, no network).
