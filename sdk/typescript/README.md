# @tack/plugin

TypeScript/JavaScript SDK for **tack-RPC v3** plugins — build a
[ tack](https://github.com/Tack-AI/tack) Level-3 plugin without ever
seeing a JSON-RPC envelope. Zero dependencies; protocol types
(`src/types.d.ts`) are generated from `protocol/tack-rpc.openrpc.json`.

```js
import { plugin, textOutput, allow, deny } from "@tack/plugin";

plugin({ name: "hello", version: "0.1.0" })
  .tool(
    { name: "hello.echo", description: "Echo the arguments",
      parameters: { type: "object" } },
    async (params, cx) => textOutput(`echo: ${JSON.stringify(params.arguments)}`),
  )
  .beforeToolCall(async (params) => {
    const command = params.toolCall.arguments?.command ?? "";
    return params.toolCall.toolName === "bash" && command.includes("rm -rf /")
      ? deny("refusing to delete the world")
      : allow();
  })
  .run(); // serves over stdio
```

Handler surface: `tool`, `command`, `beforeToolCall`, `afterToolCall`,
`transformContext`, `approvalReview`, `events`, `widget` /
`onWidgetAction`, `autocomplete`, `configSchema`, `metrics`. The handler
context (`cx`) exposes the negotiated host environment and the typed
host client (`cx.host.select/exec/session/snapshot/config/…`).

**stdout is the RPC bus** — never `console.log` from a plugin; use
`cx.host.log()` / `cx.host.warn()`.

Tests: `node --test test/` (no model, no network).
