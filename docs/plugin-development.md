# Plugin Development Guide

**English | [简体中文](plugin-development.zh-CN.md)**

> The hands-on tutorial for writing tack plugins: **three carriers**
> (process / WASM / MCP) × **three SDK languages** (Rust / TypeScript /
> Python). This page is the "build one yourself" path. Once you are
> oriented, the reference manual is [extensions.md](extensions.md)
> (manifest schema, identity/store, marketplace, enterprise policy,
> observability), and the architecture panorama is
> [plugin-system.md](plugin-system.md).

Plugins speak **tack-RPC v3** — JSON-RPC 2.0 over NDJSON stdio — whose
single source of truth is
[`protocol/tack-rpc.openrpc.json`](../protocol/tack-rpc.openrpc.json).
With an SDK you never see an envelope; without one, any language that can
read stdin and write stdout can be a plugin.

## 1. Choose your carrier

The carrier is how the host runs your code — declared in `extension.json`.

| Carrier | Your code | Isolation | Best for |
|---|---|---|---|
| **`process`** (default) | Any executable speaking tack-RPC v3; first-class SDKs for Rust / TS / Python | OS process (crash-isolated, env-scrubbed) | Most plugins: tools, guardrail hooks, approval, dialogs, provider bridges |
| **`wasm`** | A WASI-stdio core module (debug carrier) or a WIT component (distribution carrier) | wasmtime sandbox — no fs / env / network by default; WIT components are capability-free *by construction* | Third-party or untrusted code; marketplace distribution |
| **`mcp`** | None of your own — an existing MCP server (stdio / Streamable HTTP / SSE, any language) *is* the plugin | The server runs as its own process | Reusing the MCP ecosystem with full plugin identity |

What each carrier can do:

| Capability | `process` | WASI-stdio | WIT component | `mcp` |
|---|---|---|---|---|
| Tools (`ext__<plugin>__<tool>`) | ✓ | ✓ | ✓ | ✓ (plus resources & prompts) |
| Hooks (`beforeToolCall` rewrite/deny, …) | ✓ | ✓ | ✓ (`before-tool-call` only) | — |
| Approval chain (`approval/review`) | ✓ | ✓ | — | — |
| UI dialogs / widgets / autocomplete | ✓ | ✓ | — | — (elicitation follows run mode) |
| Provider registration & bridges | ✓ | ✓ | — | — (plain registration only) |
| Commands / lifecycle events / metrics | ✓ | ✓ | — | — |

Languages per carrier:

- **`process`**: SDKs for Rust (`tack-ext-sdk`), TypeScript (`@tack/plugin`),
  Python (`tack-plugin`) — or any language, speaking the protocol raw.
- **`wasm`**: WASI-stdio — compile anything that speaks the stdio protocol
  to `wasm32-wasip1`; WIT component — any
  [wit-bindgen](https://github.com/bytecodealliance/wit-bindgen) toolchain
  (Rust, C, Go, JS, Python), payloads are rpc3 JSON strings.
- **`mcp`**: the server can be written in anything; there is no plugin-side
  tack-RPC code at all.

## 2. The shared skeleton

Every plugin is a **directory containing `extension.json`**:

```jsonc
{
  "name": "my-plugin",                 // required; becomes part of the id
  "version": "1.0.0",                  // optional semver → store version
  "command": "node",                   // process carrier
  "args": ["plugin.js"],
  // "carrier": "process" | "wasm" | "mcp"   (default: process)
  // "module": "plugin.wasm",              // wasm carrier
  // "mcpServer": { … },                   // mcp carrier (one server)
  // "failMode": "block",                  // hook failures block (default: fail-open)
}
```

The full manifest reference (env, WASM `limits`, bundle fields for
hooks/MCP/skills, …) is [extensions.md §2](extensions.md). The dev loop is
the same for every carrier:

```sh
tack ext inspect <dir>     # handshake + dump declared capabilities
tack ext dev <dir>         # run, stream plugin logs (Ctrl-C to stop)
tack ext test <dir>        # run plugin.scenario.json assertions (CI-friendly)
```

## 3. Process carrier: one plugin, three languages

We will build the same tiny plugin in each SDK — one tool (`hello.echo`)
plus a guardrail hook that refuses `rm -rf /`:

### 3.1 Scaffold

```sh
tack ext new my-plugin rust     # or: ts | python
```

| Language | Generated files | Manifest command |
|---|---|---|
| `rust` | `Cargo.toml`, `src/main.rs` | `cargo run --quiet --manifest-path ./Cargo.toml` |
| `ts` | `package.json`, `plugin.js` (ESM) | `node plugin.js` |
| `python` | `plugin.py` | `python3 plugin.py` |

Every scaffold also gets `extension.json`, a ready-to-run
`plugin.scenario.json`, and a `README.md`. The SDKs are **not yet
published** to crates.io / npm / PyPI, so point the dependency at your tack
checkout (the scaffold says so in a comment): Rust —
`tack-ext-sdk = { path = "…/crates/tack-ext-sdk" }`, TS —
`"@tack/plugin": "file:…/sdk/typescript"`, Python —
`pip install …/sdk/python`.

### 3.2 Rust

```rust
use serde_json::json;
use tack_ext_sdk::{Plugin, ToolSpec, allow, deny, text_output};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    Plugin::builder(env!("CARGO_PKG_NAME"))
        .version("0.1.0")
        .tool(
            ToolSpec {
                name: "hello.echo".to_string(),
                label: None,
                description: "Echo the arguments back".to_string(),
                parameters: json!({"type": "object"}),
            },
            |params, _cx| async move {
                Ok(text_output(format!("echo: {}", params.arguments)))
            },
        )
        .before_tool_call(|params, _cx| async move {
            let command = params.tool_call.arguments
                .get("command").and_then(|v| v.as_str()).unwrap_or("");
            if params.tool_call.tool_name == "bash" && command.contains("rm -rf /") {
                Ok(deny("refusing to delete the world"))
            } else {
                Ok(allow())
            }
        })
        .run()
        .await
}
```

Verdicts are `allow()`, `deny(reason)` (the reason becomes the error tool
result), and `rewrite(arguments)` (full argument replacement — chained
plugins observe the previous plugin's rewrite; the first `deny`
short-circuits).

### 3.3 TypeScript

```js
import { plugin, textOutput, allow, deny } from "@tack/plugin";

plugin({ name: "my-plugin", version: "0.1.0" })
  .tool(
    { name: "hello.echo", description: "Echo the arguments back",
      parameters: { type: "object" } },
    async (params, cx) =>
      textOutput(`echo: ${JSON.stringify(params.arguments)}`),
  )
  .beforeToolCall(async (params) => {
    const command = params.toolCall.arguments?.command ?? "";
    return params.toolCall.toolName === "bash" && command.includes("rm -rf /")
      ? deny("refusing to delete the world")
      : allow();
  })
  .run(); // serves over stdio
```

### 3.4 Python

```python
from tack_plugin import Plugin, text_output, allow, deny

plugin = (
    Plugin("my-plugin", version="0.1.0")
    .tool(
        {"name": "hello.echo", "description": "Echo the arguments back",
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

Python handlers may be sync or async.

> **stdout is the RPC bus.** Never `println!` / `console.log` / `print`
> from a plugin — a stray line corrupts the protocol. Use
> `cx.host.log(...)` / `cx.host.warn(...)`, which land in the host's
> tracing.

### 3.5 The handler surface, side by side

| Rust (`PluginBuilder`) | TypeScript | Python | What it declares |
|---|---|---|---|
| `.tool(spec, h)` | `.tool(spec, h)` | `.tool(spec, h)` | A model-callable tool |
| `.command(spec, h)` | `.command(spec, h)` | `.command(spec, h)` | A `/slash` command |
| `.before_tool_call(h)` | `.beforeToolCall(h)` | `.before_tool_call(h)` | allow / deny / **rewrite** |
| `.after_tool_call(h)` | `.afterToolCall(h)` | `.after_tool_call(h)` | Per-field result patch |
| `.transform_context(h)` | `.transformContext(h)` | `.transform_context(h)` | Full-context replacement |
| `.approval_review(h)` | `.approvalReview(h)` | `.approval_review(h)` | Joins the approval chain |
| `.events(&[…], h)` | `.events([…], h)` | `.events([…], h)` | Lifecycle events (camelCase: `agentStart`, `turnEnd`, …) |
| `.widget(spec)` + `.on_widget_action(h)` | `.widget(spec)` + `.onWidgetAction(h)` | `.widget(spec)` + `.on_widget_action(h)` | TUI widget |
| `.autocomplete(spec, h)` | `.autocomplete(spec, h)` | `.autocomplete(spec, h)` | Argument autocomplete |
| `.config_schema(v)` | `.configSchema(v)` | `.config_schema(v)` | Per-plugin config schema |
| `.metrics(decl)` | `.metrics(decl)` | `.metrics(decl)` | Metrics sidecar schema |
| `.provider_register(bool)` | `.providerRegister(bool)` | `.provider_register(bool)` | May register HTTP-shim providers |
| `.provider_stream(h)` + `.on_ready(h)` | `.providerStream(h)` + `.onReady(h)` | `.provider_stream(h)` + `.on_ready(h)` | Provider bridge (serve inference directly) |

Undeclared capabilities are never called — the host answers
`ERR_CAPABILITY_NOT_GRANTED`.

### 3.6 Talking back to the host

Every handler gets a context (`cx`): the negotiated environment
(`cx.mode()` / `cx.trusted()` / `cx.cwd()` / `cx.capabilities()` /
`cx.config()` in Rust; equivalents in the other SDKs) plus a typed host
client:

| Host service | Rust (`cx.host()`) | Notes |
|---|---|---|
| `ui/notify` | `.notify(...)` | TUI toast; headless modes log it |
| `ui/select` / `ui/confirm` / `ui/input` | `.select(...)` / `.confirm(...)` / `.input(...)` | TUI dialogs; headless → `ERR_CAPABILITY_NOT_GRANTED` |
| `exec/run` | `.exec(...)` | Trust-gated (untrusted sessions are refused) |
| `session/get` · `session/sendUserMessage` | `.session()` · `.send_user_message(...)` | Session state; inject a user message |
| `snapshot/get` | `.snapshot()` | Read-only session digest |
| `config/get` | `.config()` | This plugin's merged config |
| `logs/emit` · `warnings/emit` | `.log(level, msg)` · `.warn(msg)` | Land in host tracing |

Headless degradation is deterministic (see
[extensions.md §7](extensions.md)): print/rpc/acp keep tools, hooks,
events, and `exec/run`; dialog calls answer `ERR_CAPABILITY_NOT_GRANTED`;
`ui/notify` goes to the log.

## 4. WASM carrier

`carrier: "wasm"` + `"module": "plugin.wasm"` (+ optional `limits`) runs
the plugin in a wasmtime sandbox. The module format is auto-detected:

### 4.1 WASI-stdio core module (the debug carrier)

The plugin speaks the same tack-RPC v3 NDJSON protocol over WASI
stdin/stdout — host-side it shares the JSON-RPC peer with the process
carrier, so the capability surface is the same (including approval and
provider bridges). Manifest `capabilities` (fs preopens, env, args) are
honored here; fuel / wall-clock / memory are hard-capped by the host.
Build any language to `wasm32-wasip1` and speak the protocol (or an SDK,
if yours supports the target). Reference:
[`examples/extensions/hello-wasm/`](../examples/extensions/hello-wasm/)
(hand-written WAT).

```jsonc
{ "name": "hello-wasm", "carrier": "wasm", "module": "hello.wasm",
  "capabilities": { "fs": ["./data"], "env": ["MY_VAR"] },
  "limits": { "fuel": 1000000000, "memoryMb": 64, "timeoutMs": 30000 } }
```

### 4.2 WIT component (the distribution carrier)

A component targeting [`tack:plugin@0.3.0`](../protocol/wit/tack-plugin.wit)
exports typed interfaces with **rpc3 JSON-string payloads** (the OpenRPC
schema stays the single source of truth; wit-bindgen handles the string
framing in Rust, C, Go, JS, or Python):

- `tack:plugin/tools` — `list: func() -> string` (rpc3 `ToolSpec[]` JSON),
  `execute: func(call: string) -> result<string, string>`
- `tack:plugin/hooks` — `before-tool-call: func(call: string) ->
  result<string, string>` (in `BeforeToolCallParams`, out `Verdict`)
- imports only `tack:plugin/host` — `log(level, message)`

The world imports **no WASI interfaces**, so the sandbox (no fs, no env,
no network) is structural rather than configured — this is why components
are the marketplace distribution format. Interface **subsets are valid**:
a hooks-only plugin exports just `tack:plugin/hooks`. Calls are
synchronous with per-call fuel/wall-clock/memory limits; a trapping call
kills only the plugin. Real toolchains emit version-qualified names
(`tack:plugin/tools@0.3.0`) — the host resolves those, and falls back to
the bare names for hand-written guests. Reference:
[`examples/extensions/hello-component/`](../examples/extensions/hello-component/)
(hand-written component WAT).

## 5. MCP carrier

A Level-2 plugin declares **one MCP server** — the server *is* the plugin;
no tack-RPC process is spawned:

```jsonc
{ "name": "filesystem-tools", "carrier": "mcp",
  "mcpServer": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] } }
```

Any existing MCP server qualifies (stdio, Streamable HTTP, or legacy SSE —
same entry shape as `mcp.json`, including `url`/`headers`/`oauth`). The
host connects at load time and adapts the probe into the plugin model:

- tools become plugin tools named `ext__<plugin>__<tool>`; resources add
  `list_resources` / `read_resource` meta-tools; prompts join as
  `prompt__<name>` tools;
- full plugin identity: attribution, `ext list`, policy, and hook
  interception (another plugin's `beforeToolCall` sees these calls);
- the untrusted-content defense applies exactly as for config-file MCP
  servers (`<untrusted_content>` wrapping, permission elevation);
- stdio servers run with the extension directory as cwd; path-like
  relative `command`s resolve against it.

Limits: capabilities MCP cannot express (hooks, approval, widgets, session
control) are never declared; elicitation follows the run mode; **sampling
is not wired** for plugin connections (documented Level-2 limitation).

## 6. Debug and test

```sh
tack ext inspect my-plugin      # handshake; dumps declared capabilities
tack ext dev my-plugin          # serve + stream logs until Ctrl-C
tack ext dev my-plugin s.json   # run a scenario interactively
tack ext test my-plugin         # assertions; non-zero exit on failure
```

`tack ext test` defaults to `<dir>/plugin.scenario.json`. Scenarios run
against the **real protocol** (the plugin is spawned and handshaken
first):

```jsonc
{
  "initialize": { "trusted": true, "config": {} },   // optional overrides
  "steps": [
    { "call": "tools/execute", "params": { "name": "hello.echo", "toolCallId": "c-1",
        "arguments": {"text": "hi"} },
      "expect": { "content": [ { "type": "text" } ] } },        // recursive subset match
    { "call": "hooks/beforeToolCall", "params": { "toolCall": { "toolCallId": "c-2",
        "toolName": "bash", "arguments": {"command": "rm -rf /"} } },
      "expectError": -32001 },                                   // JSON-RPC error code
    { "notify": "events/lifecycle", "params": { "event": "turnStart", "payload": {} } },
    { "expectHostRequest": "ui/select", "respond": "b" },        // script plugin→host answers
    { "providerStream": { "model": {…}, "context": {…}, "options": {} },
      "expectEvents": [ {"type": "start"}, {"type": "done"} ],
      "cancelAfterMs": 50 },                                     // provider-bridge plugins
    { "sleepMs": 50 }
  ]
}
```

When a session misbehaves, `tack doctor` reads
`extensions/last-load.json` and reports load failures with causes
(manifest / handshake / register / policy / store), plus policy-filtered
entries.

## 7. Install, iterate, distribute

**Live dev loop** — add your checkout to settings `extensionPaths`
(a single extension dir or a parent dir; project-layer entries are
trust-gated). Plugins loaded this way get source `local` and are picked up
straight from the working directory — no reinstall per edit:

```jsonc
// ~/.tack/agent/settings.json
{ "extensionPaths": ["~/work/my-plugin"] }
```

**Store installs** — `tack ext install <dir|git-url[#ref]|name@marketplace|file.tgz>`
copies into the versioned store
(`~/.tack/agent/extensions/store/<source>/<name>/<version>/`, source
`user`; `--local` installs into the project's `.pi/extensions`, source
`project`). Identity is `name@source`; the active version is the highest
semver. Then:

```sh
tack ext list                    # id, state, version, layout — incl. failed/policy-blocked
tack ext disable my-plugin@user  # persisted as plugins."<id>".enabled
tack ext upgrade [my-plugin]     # fingerprint-idempotent; no-op when unchanged
tack ext verify                  # check installs against the lockfile
```

**Distribute**:

- `tack ext bundle pack <dir>` writes a deterministic
  `<name>-<version>.tgz` — the air-gapped unit; `tack ext install
  <file.tgz>` extracts it under hostile-input rules.
- Marketplaces: `tack ext marketplace add <name> <file|url>` (signed
  catalogs pin an ed25519 key, TOFU), then `tack ext install
  <plugin>@<marketplace>`. See [extensions.md §5](extensions.md) for
  catalog v2 (`installation`, inline `manifest`) and the curated startup
  sync (`pluginMarketplaces`).

## 8. Where to go next

| Topic | Document |
|---|---|
| Manifest schema, store, install/upgrade, marketplace, policy, observability | [extensions.md](extensions.md) |
| Approval chain semantics (claim/pass/fail-open, prompt-injection guard) | [plugin-system.md §3.1c](plugin-system.md) |
| Provider bridges (serve inference from a plugin) | [plugin-provider-bridge.md](plugin-provider-bridge.md) |
| Architecture panorama, hooks engine, run modes | [plugin-system.md](plugin-system.md) |
| The wire protocol itself | [`protocol/tack-rpc.openrpc.json`](../protocol/tack-rpc.openrpc.json), [`protocol/wit/tack-plugin.wit`](../protocol/wit/tack-plugin.wit) |
| Design rationale (three-level model, enterprise plane) | [plugin-roadmap.md](plugin-roadmap.md) |
