# tack-ext: Dynamic Extensions (Subprocess Plugins)

Tack runs dynamic extensions as **subprocess plugins**: any executable that
speaks newline-delimited JSON (NDJSON) over stdio. One process per plugin,
crash-isolated from the agent loop. Language-agnostic — plugins can be written
in JavaScript, Python, Rust, Go, or anything that can read stdin and write
stdout.

This document covers installation, the wire protocol, the plugin API, and the
security model. Reference implementations:

- [`examples/extensions/hello-js/plugin.js`](../examples/extensions/hello-js/plugin.js) — Node.js
- `tack ext-demo-plugin` — built-in minimal implementation (also the e2e fixture)
- [`crates/tack-ext/src/protocol.rs`](../crates/tack-ext/src/protocol.rs) — canonical type definitions

---

## 1. Architecture

```
┌──────────── tack (host) ────────────┐         ┌──── plugin process ────┐
│  ExtensionManager                    │  NDJSON │                        │
│   ├─ PluginProcess (stdin/stdout) ───┼─────────┼─► your code            │
│   ├─ ExtTool ──► agent loop tools    │◄────────┼─  tool.execute results │
│   ├─ ExtHooks ──► tool_call intercept│         │  ui.* requests         │
│   └─ TuiExtServices ──► dialogs      │         │  exec / log            │
└──────────────────────────────────────┘         └────────────────────────┘
```

- **One process per plugin**, spawned at startup, `kill_on_drop`, stderr
  forwarded to the host log.
- **Both sides issue requests concurrently**; responses match by `id`
  (per-sender counter).
- **Timeouts**: host→plugin calls 30s (intercepts, tool executes), plugin
  failures are *fail-open* — a dead or slow plugin never stalls the agent.
- **Lifecycle**: `shutdown` event on exit → 2s grace → force kill.

The same message schema is designed to also serve a future WASM carrier
(sandboxed, capability-restricted) without protocol changes.

## 1b. Marketplaces

A marketplace is a named JSON catalog (`{"name": ..., "plugins": {"<name>":
{"source": "<git-url|path>", "description": ..., "rev": "<ref>"}}}`) registered under
`~/.tack/agent/marketplaces/`:

```sh
tack ext marketplace add acme ./acme-marketplace.json   # or an https URL
tack ext marketplace list                               # registered catalogs
tack ext marketplace list acme                          # plugins in a catalog
tack ext install code-review@acme                       # install by spec
tack ext marketplace remove acme
```

`tack ext install <git-url|dir>` still works directly; the
`<plugin>@<marketplace>` form resolves the source through the catalog and
installs under the marketplace key. A catalog entry's optional `"rev"` pins
the plugin to a git ref (tag/branch/commit sha), exactly like an explicit
`#<ref>` suffix (below); an explicit `#<ref>` on the command line wins.

### Marketplace signatures (ed25519, TOFU key pinning)

A catalog may carry a top-level signature:

```json
{
  "name": "acme",
  "plugins": { "...": {} },
  "signature": { "algorithm": "ed25519", "value": "<128 hex chars>" }
}
```

The signed payload is the **canonical catalog**: the catalog JSON with the
top-level `signature` key removed, re-serialized with `serde_json::to_string`
— that is the entire canonicalization rule. Because Tack builds serde_json
without `preserve_order`, object keys are BTreeMap-ordered
(lexicographic), so the canonical form is deterministic and independent of
the original file's key order and whitespace.

Registering a signed catalog requires the signer's public key:

```sh
tack ext marketplace add acme https://acme.com/marketplace.json --public-key <64 hex chars>
```

The first key that verifies is **pinned** to `~/.tack/agent/marketplaces/acme.key`
(trust on first use). Every later `tack ext install <plugin>@acme`
re-verifies the registered catalog against the pinned key; a tampered or
re-signed catalog is rejected. Re-registering the same marketplace uses the
pinned key when `--public-key` is omitted. Unsigned catalogs register as
before, with a warning that installs are not integrity-protected.

## 1c. Ref pinning, lockfile, and verify

Git installs accept an optional ref suffix:

```sh
tack ext install https://github.com/acme/tack-ext-review.git#v1.2.0   # tag/branch/sha
```

The ref may be a tag, branch, or full/short commit sha; Tack clones and
runs `git checkout <ref>` (a pinned install is a full clone, unpinned stays
`--depth 1`). Local-directory installs do not support refs.

Every user-dir install is recorded in `~/.tack/agent/extensions-lock.json`:

```json
{
  "version": 1,
  "plugins": {
    "tack-ext-review": {
      "source": "https://github.com/acme/tack-ext-review.git",
      "rev": "v1.2.0",
      "resolvedCommit": "<40-char sha HEAD resolved to>",
      "installedAt": 1735689600,
      "marketplace": "acme"
    }
  }
}
```

`rev`/`marketplace` are `null` when absent; local-directory installs record
`resolvedCommit: null`. `ext remove` deletes the entry. Project-local
(`--local`) installs are trust-gated already and stay out of the lockfile.
Git installs keep their `.git` directory so the checkout can be audited.

`tack ext verify` compares every locked plugin's `git rev-parse HEAD`
against `resolvedCommit` and prints `ok` / `changed` / `not-a-git-repo` /
`missing` per plugin, exiting non-zero when any plugin changed or is
missing. The same check runs at startup: a user-dir plugin whose checkout
drifted from its lock entry is **skipped** (with a warning) by default —
set `"extensionLockRequired": false` in settings to downgrade the mismatch
to a warning and load anyway. Plugins without a lock entry (manually copied
directories, local-directory installs) are never gated. Managed settings
may force `extensionLockRequired` in either direction.


## 2. Installation & discovery

An extension is a **directory containing `extension.json`**:

```json
{
  "name": "hello-js",
  "command": "node",
  "args": ["plugin.js"],
  "env": { "MY_VAR": "1" }
}
```

| Field | Required | Notes |
|---|---|---|
| `name` | yes | Used in tool names (`ext__<name>__<tool>`) and logs. |
| `command` | process carrier | Executable to spawn (must be on PATH or absolute). |
| `args` | no | Arguments. **Path-like args** (containing `/`, `\`, or starting with `.`) resolve against the extension directory; plain words (subcommands, flags) pass through unchanged. |
| `env` | no | Extra environment variables for the plugin process. |
| `carrier` | no | `"process"` (default) \| `"wasm"` — run the plugin as a WASI module in the wasmtime sandbox (same NDJSON protocol, `protocol: 2` handshake). |
| `module` | wasm carrier | `.wasm`/`.wat` file, resolved against the extension directory. |
| `limits` | no | WASM sandbox limits: `{maxFuel, maxMemoryBytes, maxExecutionMs}` (defaults 1e9 fuel, 256MB, no wall-clock cap). |
| `capabilities` | no | WASM capability grants (default: full sandbox): `{fs: [{host, guest, access: "read-only"\|"read-write"}], env: {K:V} or [K,...] (host pass-through), args: [...], network: {tcp, udp, dns}}`. Relative `host` resolves against the extension dir. Every grant is audit-logged at load. Network flags are inert for WASI p1 guests (no socket ABI in wasmtime-wasi p1). |
| `hooks` | no | **Bundle**: path(s) to Claude-format `hooks.json` — contributed to the session hook config (see [hooks.md](hooks.md)). |
| `mcpServers` | no | **Bundle**: a file path or an inline server map — merged into the session's MCP servers. |
| `skills` | no | **Bundle**: skill directories — merged into skill discovery. |

A manifest with only bundle fields (no `command`/`module`) is valid: it
contributes declarative resources without running a plugin. See
`examples/extensions/hello-wasm/` for a WASM-carrier plugin.

Discovery roots (all scanned at startup):

| Root | Trust |
|---|---|
| `~/.tack/agent/extensions/*/` | always loaded |
| `extensionPaths` setting (array of dirs, global settings.json) | always loaded |
| `<project>/.pi/extensions/*/` | **requires project trust** (`/trust`) — plugins execute code |

A broken plugin (bad manifest, spawn failure, handshake timeout) is logged
and skipped — it never prevents session creation.

## 3. Wire protocol

One JSON object per line. Three envelope kinds:

```json
{"type":"request","id":1,"method":"tool.execute","params":{...}}
{"type":"response","id":1,"result":{...}}
{"type":"response","id":1,"error":"something failed"}
{"type":"event","event":"agent_start","payload":{...}}
```

### 3.1 Handshake

1. Host spawns the plugin and sends:
   ```json
   {"type":"event","event":"initialize","payload":{
     "protocol": 1, "mode": "tui", "cwd": "D:/work/repo",
     "trusted": true, "host": "tack/0.1.0"}}
   ```
2. Plugin answers with its registration:
   ```json
   {"type":"event","event":"register","payload":{
     "name": "hello-js",
     "tools": [{"name":"echo","description":"Echo","parameters":{...}}],
     "commands": [{"name":"hello-js","description":"Greet"}],
     "shortcuts": [],
     "subscriptions": ["agent_start","tool_call"]}}
   ```

`subscriptions` filters which lifecycle events the plugin receives. Empty =
the default set (everything except `message_update`, which is opt-in due to
frequency). `tool_call` in subscriptions enables interception (§4.3).

### 3.2 Defaults & robustness rules for plugin authors

- Read **whole lines**, tolerate blank lines and unknown envelope fields.
- If you crash, the host fails pending calls and keeps running — but your
  tools disappear mid-session, so catch your own errors.
- `shutdown` event → exit promptly (host force-kills after 2s).

## 4. Plugin API

### 4.1 Host → plugin: lifecycle events

| Event | Payload | When |
|---|---|---|
| `session_start` | `{sessionId, resumed, cwd}` | App start |
| `session_shutdown` | `{sessionId}` | App exit |
| `agent_start` / `agent_end` | agent event JSON | A run starts/ends |
| `turn_start` / `turn_end` | agent event JSON | Each turn |
| `message_start` | `{message}` | Assistant message begins streaming |
| `message_end` | `{message}` | Assistant message final (includes usage) |
| `message_update` | `{message}` | Streaming deltas — **opt-in only** |
| `tool_execution_start` | `{toolCallId, toolName, args}` | Tool begins |
| `tool_execution_end` | `{toolCallId, toolName, result, isError}` | Tool finishes |
| `before_provider_request` | `{provider, model, messageCount, toolCount, hasSystemPrompt}` | Before every LLM call (sanitized — no message bodies) |
| `after_provider_response` | `{provider, model, stopReason, durationMs, usage, errorMessage}` | After every LLM call |
| `model_select` | `{provider, modelId, source}` | Model changed |
| `thinking_level_select` | `{level}` | Thinking level changed |

### 4.2 Host → plugin: `tool.execute`

Runs a plugin-registered tool.

```json
{"type":"request","id":7,"method":"tool.execute","params":{
  "name":"echo","toolCallId":"call_1","arguments":{"text":"hi"}}}
```

Result convention (any of):
```json
{"result":{"content":"plain text"}}
{"result":{"content":[{"type":"text","text":"block text"}]}}
{"result":"plain text"}
```

Plugin tools appear to the model as `ext__<plugin>__<tool>` and flow through
the standard pipeline: permission modes, session persistence, TUI tool cards.

### 4.3 Host → plugin: `intercept.tool_call`

Requires `"tool_call"` in `subscriptions`. Fired before every built-in tool
call; the plugin returns a verdict:

```json
{"result":{"action":"allow"}}
{"result":{"action":"deny","reason":"blocked by policy"}}
{"result":{"action":"rewrite","arguments":{ /* full replacement args */ }}}
```

`deny` turns the tool call into an error result with `reason`; `rewrite`
re-runs validation on the new arguments and executes with them. Intercept
failures/timeouts are **fail-open** (the call proceeds).

### 4.3b Host → plugin: `intercept.context` (subscription-gated)

Plugins subscribing `"context"` receive the full message list before every
LLM call and may return a replacement (the `context` mutation point):

```json
{"type":"request","id":9,"method":"intercept.context","params":{
  "messages": [ /* AgentMessage... */ ]}}
→ {"result":{"messages":[ /* replacement AgentMessage... */ ]}}
```

Opt-in only (the payload is the whole context); missing/invalid answers and
timeouts are fail-open — the original context is used.

### 4.4 Host → plugin: `command.invoke`

Runs a plugin-registered slash command (`/<name>` typed by the user, args as
a string). Extension commands autocomplete like built-ins and take precedence
over prompt templates.

### 4.5 Plugin → host: UI methods

| Method | Params | Result |
|---|---|---|
| `ui.notify` | `{message, level?}` (`info`/`warning`/`error`) | `null` |
| `ui.select` | `{title, options: [...]}` | selected option string, `null` on cancel |
| `ui.confirm` | `{title, message}` | `true`/`false` |
| `ui.input` | `{title, placeholder?}` | entered text, `null` on cancel |
| `ui.set_status` | `{text}` (`null` clears) | `null` |

Dialogs render on the TUI main loop; the plugin's request simply awaits the
user. In headless run modes (print/rpc/acp) plugins load with degraded
services: `ui.notify`/`ui.set_status` are accepted (log only),
`ui.select`/`ui.confirm`/`ui.input` return an error (no user to ask), and
`session.*`/`provider.register` are unavailable. The `initialize` payload's
`mode` field ("tui"/"print"/"rpc"/"acp") tells the plugin which mode hosts
it — interactive requests must never be load-bearing for correctness.

### 4.6 Plugin → host: `exec`

```json
{"type":"request","id":3,"method":"exec","params":{"command":"git status","timeout_ms":120000}}
→ {"result":{"stdout":"...","stderr":"...","code":0}}
```

**Trust-gated**: only honored when the project is trusted (user-dir plugins
are always trusted; project plugins inherit project trust).

### 4.7 Plugin → host: session control (`session.*`)

Drive the session from a plugin (TS ExtensionCommandContext equivalents):

| Method | Params | Effect |
|---|---|---|
| `session.get_info` | — | `{sessionId, cwd, provider, modelId, thinking, messageCount, running}` |
| `session.new` | — | Start a fresh session |
| `session.switch` | `{session: "path-or-id-prefix"}` | Resume another session file |
| `session.branch` | `{entryId}` | Jump the tree (summarizes the abandoned branch first) |
| `session.set_model` | `{provider, modelId}` | Switch model |
| `session.set_thinking` | `{level}` | Switch thinking level |
| `session.set_name` | `{name}` | Name the session |
| `session.send_user_message` | `{text}` | Inject a user message (queues while running, starts a run when idle) |

### 4.8 Plugin → host: `provider.register`

Register a custom LLM provider (the dynamic equivalent of models.json):

```json
{"type":"request","id":5,"method":"provider.register","params":{
  "id": "corp-gateway",
  "baseUrl": "https://llm.corp.internal/v1",
  "api": "openai-completions",
  "apiKeyEnv": "CORP_LLM_KEY",
  "headers": {"x-team": "search"},
  "models": [{"id": "corp-1", "contextWindow": 128000, "maxTokens": 8192}]
}}
```

`api` is any Tack wire protocol (`anthropic-messages`, `openai-completions`,
`openai-responses`, `google-generative-ai`, `mistral-conversations`,
`bedrock-converse-stream`, `tack-messages`, …; legacy alias `pi-messages`
accepted). Registered models immediately
appear in `/model` and resolve through the matching adapter. Registering the
same `id` twice replaces the entry.

### 4.9 Plugin → host: `log` event

```json
{"type":"event","event":"log","payload":{"level":"info","message":"..."}}
```

Forwarded to the host's tracing log (stderr / `--verbose`).

## 5. Security model

- **Project trust** (`/trust`, `~/.tack/agent/trust.json`): project-local
  `.pi/extensions/` only load when trusted — a malicious clone can't auto-run
  code. `defaultProjectTrust: "never"` blocks them globally.
- **Process isolation**: plugin crashes can't take down the agent; a dead
  plugin fails its pending calls and drops out.
- **exec gating**: arbitrary host command execution requires trust.
- **No ambient access**: plugins only get what the protocol offers — session
  data arrives via events; there is no direct filesystem/memory access to the
  host.
- **Supply chain**: installs are pinned in `extensions-lock.json` (resolved
  commit per plugin; §1c), `ext verify` audits drift, the startup check skips
  drifted plugins (`extensionLockRequired`, default on), and marketplace
  catalogs can be ed25519-signed with TOFU-pinned keys (§1b).
- Sandbox hardening (WASI capabilities, memory/fuel limits) is the WASM v2
  variant's job; subprocess plugins are full processes and should be treated
  with the same trust as any installed program.

## 6. Writing a plugin (Node.js quick start)

```js
import readline from "node:readline";
const rl = readline.createInterface({ input: process.stdin, terminal: false });
const send = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");

rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.type === "event" && msg.event === "initialize") {
    send({ type: "event", event: "register", payload: {
      name: "my-ext",
      tools: [{ name: "ping", description: "Ping", parameters: { type: "object", properties: {} } }],
      commands: [{ name: "my-cmd", description: "My command" }],
      subscriptions: ["agent_start"],
    }});
    return;
  }
  if (msg.type === "request" && msg.method === "tool.execute") {
    send({ type: "response", id: msg.id, result: { content: "pong" } });
    return;
  }
  if (msg.type === "request" && msg.method === "command.invoke") {
    send({ type: "response", id: msg.id, result: { ok: true } });
    return;
  }
  if (msg.type === "event" && msg.event === "shutdown") process.exit(0);
});
```

With `extension.json`:
```json
{ "name": "my-ext", "command": "node", "args": ["plugin.js"] }
```

Install to `~/.tack/agent/extensions/my-ext/`, restart Tack, then
`/my-cmd` works and the model can call `ext__my-ext__ping`.

**Debugging tips**: run `tack --verbose` to see plugin stderr and handshake
logs; test your plugin standalone by piping an `initialize` line into it;
`tack ext-demo-plugin` shows a minimal correct implementation.

## 7. Scope

**Supported today**: lifecycle events (including provider boundaries),
plugin tools, slash commands, tool_call interception (allow/deny/**rewrite**),
context transformation (`intercept.context`, subscription-gated), UI dialogs
(notify/select/confirm/input/status), trust-gated exec, session control
(`session.*`), runtime provider registration (`provider.register`), logging,
**WASM sandboxed carrier** (`carrier: "wasm"`, protocol 2 handshake — see
`examples/extensions/hello-wasm/`), **declarative widgets and autocomplete
providers** (v2.1/v2.2 — see §8), and **bundle resources** (declarative
hooks/MCP servers/skills via manifest fields; hooks engine documented in
[hooks.md](hooks.md)).

**Deliberately out of scope (v2.x)** — these need a declarative component
protocol rather than RPC calls:

- Tool render components and custom message renderers
- Markdown transformers
- Request mutation (`before_provider_request` headers/payload rewrite —
  events are sanitized notifications)
- Hot reload

See [extensions-v2.md](extensions-v2.md) for the v2 design.

## 8. Declarative widgets & autocomplete providers (v2.1 / v2.2)

The `ui.*` methods above are imperative RPC ("open a dialog, I wait"). The
component protocol is the inverse: the plugin **declares** long-lived UI
units at register time, the host TUI owns rendering and layout, and the
plugin pushes state updates over the event stream. Full design:
[extensions-v2.md](extensions-v2.md) §3. Both subprocess and WASM carriers
are supported; a v2.1+ host negotiates `protocol: 2` with every carrier
(v1 plugins simply ignore the higher number).

### 8.1 Declaring widgets (`register.widgets`)

```json
{"type":"event","event":"register","payload":{
  "name": "git-status",
  "widgets": [
    {"id": "branch", "type": "status_line_segment", "priority": 50,
     "initial": {"text": "main", "style": "dim"}},
    {"id": "diff-panel", "type": "markdown_panel", "title": "Pending diff",
     "visible": false},
    {"id": "files", "type": "list_panel", "title": "Changed files",
     "initial": {"items": []}}
  ]
}}
```

`id` is unique per plugin; the host keys widgets as `<plugin>:<id>`.
Widget kinds and their state schemas (the shape of `initial` and of every
later `widget.update.state`):

| `type` | state | host rendering |
|---|---|---|
| `status_line_segment` | `{text, style?, tooltip?}`; `style` ∈ `default/info/warning/error/dim` | one status-bar segment, sorted by `priority` ascending; empty `text` hides it; truncated to one line |
| `markdown_panel` | `{markdown}` | toggleable panel rendered with the host markdown pipeline; the host owns scroll/focus and caps the panel height |
| `list_panel` | `{items: [{id, label, detail?, icon?}], selectedId?}` | item list with host-rendered selection; a user pick is reported back as `widget.action` |

### 8.2 Events: `widget.update` (plugin → host), `widget.action` (host → plugin)

```json
{"type":"event","event":"widget.update","payload":{
  "id": "branch", "state": {"text": "feature/wasm", "style": "info"},
  "visible": true}}
```

Updates are **idempotent full-state replacements** (not diffs), applied on
the host's next frame; dropped frames are harmless. Unknown widget ids are
warned about and ignored.

```json
{"type":"event","event":"widget.action","payload":{
  "id": "files", "action": "select", "itemId": "src/main.rs"}}
```

`widget.action` is sent only to the widget's **owning** plugin (not
subscription-gated, never broadcast).

Rendering contract (hard rules):

- **The host owns the terminal.** Plugins never get screen coordinates and
  cannot emit ANSI; outside widgets, `ui.*` remains the only UI channel.
- **Plugin death = widget removal.** When the plugin's peer hits EOF the
  host drops all of its widgets — no residue.
- **Headless modes** (print/rpc/acp): widget declarations are accepted but
  ignored; `widget.action` is never produced. Plugins must not depend on
  widgets for correctness.

TUI keybindings (rebindable via `keybindings.json`):

| action | default | effect |
|---|---|---|
| `app.ext.panels.toggle` | `ctrl+b` | hide/show all ext panels |
| `app.ext.panel.focusNext` | `alt+p` | cycle keyboard focus over visible panels; focused list panels: `↑↓` move, `enter` select (sends `widget.action`), `esc` unfocus; focused markdown panels: `↑↓`/`pgup`/`pgdn` scroll |

### 8.3 Autocomplete providers (`register.autocompleteProviders`, v2.2)

```json
{"type":"event","event":"register","payload":{
  "autocompleteProviders": [
    {"id": "issues", "trigger": "#", "description": "GitHub issues"}
  ]
}}
```

When the input line's current token carries a provider's `trigger` prefix,
the host requests suggestions:

```json
{"type":"request","id":9,"method":"autocomplete.provide","params":{
  "providerId": "issues", "query": "wasm", "cursorOffset": 5}}
→ {"result":{"suggestions":[
     {"value": "#1234", "label": "#1234 WASM carrier",
      "detail": "open", "insertText": "#1234"}]}}
```

Contract:

- Requests use the normal host→plugin channel (30s protocol timeout); the
  TUI adds a **300ms UI-level timeout** — timeout, cancellation, error
  responses and dead plugins all silently degrade to no suggestions.
- An empty `suggestions` array is a legal "no suggestions" answer;
  `insertText` defaults to `value` on accept.
- Several providers may share a trigger; the host queries them all and
  merges in register order, deduplicating by `value`.
- Built-in `/command` and `@file` completion takes precedence over
  extension triggers (a provider reusing `/` or `@` never fires on those
  tokens).

See `examples/extensions/hello-js/` for a working demo (status segment +
list panel + `#` provider).
