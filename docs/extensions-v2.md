# tack-ext v2: WASM Sandbox Carrier + Declarative UI Component Protocol (Design Doc)

**English | [简体中文](extensions-v2.zh-CN.md)**

> Status: v2.0 **done** (WASM carrier wired into `ExtensionManager`, the
> `carrier/ module/limits` manifest fields in effect, example in
> `examples/extensions/hello-wasm/`). v2.1/v2.2 **done** (host/TUI-side
> integration of declarative widgets + autocomplete providers, see §3 and
> docs/extensions.md §8). For the v1 subprocess protocol see
> [extensions.md](extensions.md); this document covers only the v2 delta.

## 1. v2 Goals

v1 defined a plugin as "any executable speaking NDJSON over stdio", which
bought language independence and crash isolation, but process-level trust
granularity is too coarse: a plugin is a full process that can read files,
open the network, and run arbitrary commands, gated only by project trust
(`/trust`). v1 also explicitly excluded a class of capabilities — custom
widgets, autocomplete providers, and the like — because they require a
**declarative component protocol** rather than RPC calls.

v2's two goals map exactly onto these two gaps:

1. **WASM sandbox carrier (v2.0)**: a plugin is a WASI module running in
   wasmtime. **The message schema is identical to v1** (same envelope,
   handshake, and method names); on the host side, "spawn subprocess + pipe
   stdio" simply becomes "instantiate wasmtime module + WASI pipe".
   Sandboxed by default (no filesystem, no network, no environment
   variables), with hard resource limits (fuel / epoch / memory);
   capabilities like `exec` must be explicitly granted via host functions.
2. **Declarative UI component protocol (v2.1 / v2.2)**: the plugin
   **declares** UI components (status line segments, markdown/list panels)
   and autocomplete providers in `register`; the host TUI owns rendering
   and event routing. The plugin never paints pixels — it only produces
   structured data and update events.

The carrier and the component protocol are orthogonal: declarative UI works
with both the subprocess carrier and the WASM carrier; the WASM carrier
itself does not require the component protocol.

## 2. WASM Carrier Protocol (v2.0)

### 2.1 Wire protocol: zero changes

The module uses WASI preview1 stdin/stdout and speaks a NDJSON protocol
that is **byte-identical** to v1:

```
host → guest(stdin)   {"type":"event","event":"initialize","payload":{"protocol":2,...}}
guest → host(stdout)  {"type":"event","event":"register","payload":{...}}
host → guest(stdin)   {"type":"request","id":1,"method":"tool.execute","params":{...}}
guest → host(stdout)  {"type":"response","id":1,"result":{...}}
```

- Handshake: `initialize` → `register`, same as v1.
- `tool.execute` / `command.invoke` / `intercept.tool_call` / lifecycle
  events: same as v1.
- `ui.*` / `session.*` / `provider.register` / `log`: same as v1 (requests
  sent over stdout, handled by the host's `HostServices`).
- stderr: used only for logging; the host forwards it to tracing (same as
  v1).
- 16MB single-line cap, request timeouts, fail-fast on dead plugins:
  exactly the same as v1 — because the host side reuses the very same
  `PluginPeer`.

### 2.2 Host-side integration: the transport layer is already abstracted

When v1 was implemented, `PluginPeer` was made transport-agnostic: its
constructor accepts any `AsyncRead + AsyncWrite` (the subprocess carrier
passes child stdout/stdin; unit tests pass an in-memory duplex). So **no
new `PluginTransport` trait is needed**: the WASM carrier
(`WasmCarrier::spawn` in `crates/tack-ext-wasm`) only has to:

1. Create two pairs of tokio duplex pipes;
2. Wrap the guest ends as `wasmtime_wasi::cli::AsyncStdinStream /
   AsyncStdoutStream` and feed them into `WasiCtxBuilder` (WASI p1);
3. In a background task, instantiate the module and call `_start` (async
   mode, so the guest yields while blocked reading stdin);
4. Hand the host ends to `PluginPeer::new(stdout, stdin, services)`.

Handshake, request/response matching, timeouts, pending-call cleanup, and
fail-fast on dead plugins are all reused. Guest exit/trap → stdout closes
→ the peer's read pump sees EOF → pending calls fail immediately, with
semantics identical to a v1 process death. Shutdown is the same too:
`shutdown` event → 2s grace → abort the guest task (no OS process to
reap — cleaner than v1's force kill).

### 2.3 Resource limits and the sandbox boundary

The POC (`crates/tack-ext-wasm`) implements all three limit layers:

| Mechanism | wasmtime configuration | Defends against |
|---|---|---|
| **Fuel metering** | `Config::consume_fuel(true)` + `store.set_fuel(n)` | CPU exhaustion (instruction-level budget; trap on exhaustion) |
| **Epoch interruption** | `Config::epoch_interruption(true)` + engine-level ticker (10ms ticks) + `store.set_epoch_deadline(k)` | Wall-clock cap; checked at back-edges/call sites, so pure-computation infinite loops are caught |
| **Memory cap** | `store.limiter(..)` + `StoreLimitsBuilder::memory_size(bytes)` | Linear-memory bloat; both instantiation (min pages) and `memory.grow` are checked |

WASI capability surface (`WasiCtxBuilder` defaults; the POC opens up none
of them):

- No preopened directories → **no filesystem access**;
- No args, no environment variables;
- TCP/UDP/DNS resolution all denied by default → **no network**;
- The only capabilities: stdin/stdout (protocol pipes), stderr (logging),
  clock/randomness.

In other words, **fully sandboxed by default**: a WASM plugin cannot even
do the things a v1 plugin does "incidentally" (read cwd, peek at API keys
in the environment). Capability grants go through explicit paths:

| Capability | Grant mechanism | Status |
|---|---|---|
| `exec` (host commands) | Protocol method, trust-gated (same as v1); host decides per trust | Already supported by the protocol |
| Filesystem | `preopened_dir(host_path, guest_path, FsPerms::ReadOnly/ReadWrite)`, declared per manifest | v2.x design item |
| Network | `allow_tcp/allow_udp/socket_addr_check` allowlists | v2.x design item |
| Host services | Protocol `ui.*`/`session.*` (supported without touching the wasmtime linker) | Already supported by the protocol |

### 2.4 Mapping against the v1 carrier

| Dimension | v1 subprocess carrier | v2 WASM carrier |
|---|---|---|
| Plugin form | Any executable | WASI p1 core module (`.wasm`/`.wat`) |
| Wire protocol | NDJSON over stdio pipes | **Same schema**, NDJSON over WASI stdin/stdout |
| Handshake | initialize → register | Identical |
| Host core | `PluginPeer` (AsyncRead+AsyncWrite) | **The same `PluginPeer`** |
| Isolation | Process boundary (crashes don't hurt the agent) | wasmtime sandbox (traps don't hurt the agent), no OS process |
| CPU/memory limits | None (trust gating instead) | fuel + epoch + memory hard caps |
| Filesystem/network | Full process permissions | None by default; explicitly granted per declaration |
| `exec` | Trust-gated | Trust-gated (identical at the protocol layer) |
| Lifecycle | shutdown event → 2s → kill | shutdown event → 2s → abort task |
| Language ecosystem | Any language | Languages that compile to wasm32-wasip1 (Rust/C/Go(TinyGo)/JS(javy)/…) |

### 2.5 Plugin packaging (extension.json delta)

```json
{
  "name": "hello-wasm",
  "carrier": "wasm",
  "module": "plugin.wasm",
  "limits": { "maxFuel": 1000000000, "maxMemoryBytes": 268435456, "maxExecutionMs": null }
}
```

- `carrier: "process" | "wasm"`, defaulting to `"process"` (backward
  compatible).
- `module` resolves relative to the extension directory (following v1's
  path-like argument rules).
- `limits` defaults to `WasmLimits::default()`.

## 3. Declarative UI Component Protocol (v2.1)

v1's `ui.*` is **imperative RPC** ("pop a dialog and I'll wait for the
result"). The component protocol inverts this: the plugin **declares**
long-lived UI units, the host TUI owns rendering and layout, and the
plugin pushes state updates over an event stream. This is exactly the
capability class v1 explicitly excluded (custom widgets, status segments).

### 3.1 New register fields

```json
{"type":"event","event":"register","payload":{
  "name": "git-status",
  "tools": [],
  "widgets": [
    {"id": "branch", "type": "status_line_segment", "priority": 50,
     "initial": {"text": "main", "style": "dim"}},
    {"id": "diff-panel", "type": "markdown_panel", "title": "Pending diff",
     "visible": false},
    {"id": "files", "type": "list_panel", "title": "Changed files",
     "items": []}
  ]
}}
```

```jsonc
// WidgetSpec (camelCase, consistent with v1 payload style)
{
  "id": "branch",                    // unique within the plugin; host-side key is <plugin>:<id>
  "type": "status_line_segment"      // | "markdown_panel" | "list_panel"
          | "markdown_panel"
          | "list_panel",
  "priority": 50,                    // status_line_segment ordering, smaller first
  "title": "Pending diff",           // required for panel types
  "visible": true,                   // initial panel visibility
  "initial": { /* per-type state, see §3.2 */ }
}
```

### 3.2 Update event stream: `widget.update`

Plugin → host **event** (fire-and-forget, so UI hiccups can't
back-pressure the plugin):

```json
{"type":"event","event":"widget.update","payload":{
  "id": "branch",
  "state": {"text": "feature/wasm", "style": "info"},
  "visible": true
}}
```

Per-type state schemas (inputs to the TUI rendering contract):

| Type | state | TUI rendering contract |
|---|---|---|
| `status_line_segment` | `{text, style?, tooltip?}`; `style` ∈ `default/info/warning/error/dim` | One segment of the status line, ordered by `priority`; hidden when `text` is empty; truncated to a single line |
| `markdown_panel` | `{markdown}` | Toggleable panel rendered with the existing pulldown-cmark pipeline; the host owns scrolling/focus |
| `list_panel` | `{items: [{id, label, detail?, icon?}], selectedId?}` | List panel; the host renders the selection state; user selection emits `widget.action` |

Host → plugin user-interaction report (event):

```json
{"type":"event","event":"widget.action","payload":{
  "id": "files", "action": "select", "itemId": "src/main.rs"}}
```

Hard rules of the rendering contract:

- **The host owns the terminal**: a plugin never gets screen coordinates
  and cannot emit ANSI directly; outside widgets, output remains limited
  to v1 primitives like `ui.notify`.
- **Updates are idempotent state replacement** (full-state snapshots, not
  diffs); the TUI applies them on the next frame of its main loop. Dropped
  frames are harmless.
- **Dead plugin ⇒ vanished widgets**: on peer EOF the host unregisters all
  of that plugin's widgets, leaving no ghost UI (consistent with v1's
  "plugin tools disappear" semantics).
- **Headless modes** (print/rpc/acp): widget declarations are accepted but
  ignored, and `widget.action` never fires; plugins must not depend on
  widgets for correctness.

### 3.3 Autocomplete providers (v2.2)

```json
{"type":"event","event":"register","payload":{
  "autocompleteProviders": [
    {"id": "issues", "trigger": "#", "description": "GitHub issues"}
  ]
}}
```

When a token prefixed with `trigger` appears anywhere in the input line,
the host sends a request:

```json
{"type":"request","id":9,"method":"autocomplete.provide","params":{
  "providerId": "issues", "query": "wasm", "cursorOffset": 5}}
→ {"result":{"suggestions":[
     {"value": "#1234", "label": "#1234 WASM carrier", "detail": "open", "insertText": "#1234"}]}}
```

Contract:

- The request rides the normal host→plugin request channel (30s timeout
  unchanged; the TUI adds a 300ms UI-level cancellation on top; both
  timeout and cancellation degrade silently to no suggestions).
- An empty `suggestions` array = no suggestions (valid); `insertText`
  defaults to `value`.
- Multiple providers may match one input; the host merges and dedupes in
  register order.
- v1 extension slash-command completion is unaffected (it remains
  statically known at register time).

## 4. Version Negotiation (Backward Compatibility)

- `initialize.payload.protocol` is bumped to `2`. **Since v2.1 the host
  sends `2` to all carriers** (the original plan was `2` only for the WASM
  carrier and `1` for the subprocess carrier; but §3's component protocol
  is carrier-orthogonal, and subprocess plugins equally need to know the
  host supports widgets / autocomplete before declaring them. v1 plugins
  are indifferent to the higher version number — `register` parsing on
  both sides tolerates unknown fields, so the ecosystem is unaffected).
- A plugin must receive `protocol >= 2` before declaring component
  capabilities (`widgets` / `autocompleteProviders`); when a v1 host
  receives a `register` payload with unknown fields, it ignores them per
  serde convention (`RegisterPayload` already has `#[serde(default)]` and
  the new fields are all optional, so old hosts are unaffected).
- `widget.update` / `widget.action` / `autocomplete.provide` are unknown
  envelopes to a v1 peer; the protocol rule (tolerate unknown
  fields/methods) keeps interop safe: a v1 plugin receiving
  `autocomplete.provide` replies with an error response, and the host
  degrades to no suggestions.
- The envelope itself gains no version field: the version appears exactly
  once, in the handshake.

## 5. Phased Roadmap

| Phase | Contents | Status |
|---|---|---|
| **v2.0 WASM carrier** | `crates/tack-ext-wasm`: wasmtime + WASI p1, reusing `PluginPeer`; fuel/epoch/memory three-layer limits; fully sandboxed by default; `carrier: "wasm"` manifest; e2e handshake + tool.execute | **Done**: `ExtensionManager` selects the carrier per manifest (protocol 2 handshake), `limits` configurable, `hello-wasm` example + e2e tests |
| **v2.1 Declarative widgets** | `WidgetSpec` + `widget.update`/`widget.action`; TUI renders the three component types; protocol version 2 | **Done**: host-side `WidgetRegistry` (key `<plugin>:<id>`, full-state replacement, cleared on plugin death); `widget.update` routed into the TUI main loop via AppEvent; status_line_segment (priority ordering, hidden on empty text) / markdown_panel (pulldown-cmark pipeline, host owns scrolling) / list_panel (keyboard selection → `widget.action` reported to the owning plugin); keys `ctrl+b` (panel visibility) / `alt+p` (focus cycling); example in `hello-js` |
| **v2.2 Autocomplete providers** | `autocompleteProviders` + `autocomplete.provide`; TUI input-line integration | **Done**: trigger-prefixed token activates, reusing the existing completion popup; multiple providers merged and deduped in register order; 30s protocol timeout + 300ms UI-level timeout degrading silently; `insertText ?? value` insertion |
| v2.x capability grants | Manifest declarations and UI confirmation flows for preopened_dir / network allowlists | Design item |

Why v2.0 chose "WASM carrier first": the sandbox is the foundation of the
trust model, and the component protocol (v2.1/2.2) embeds plugins ever
deeper into the UI main loop — tighten the execution boundary for
untrusted code first, then expand its expressive surface.

## 6. POC Capability Boundary (current state of `crates/tack-ext-wasm`)

Verified (unit tests, macOS):

- WASI p1 modules (`.wat` text / `.wasm` binary) instantiated via
  wasmtime, stdin/stdout wired to tokio duplex pipes;
- v1 handshake (initialize → register) and `tool.execute` request/response
  work over the **unmodified `PluginPeer`**;
- fuel-exhaustion trap, epoch wall-clock deadline trap, memory-cap
  rejection of instantiation;
- stderr forwarded to host logs; guest exit/trap → peer death → pending
  calls fail fast.

Explicitly not done (future phases):

- Not yet wired into `ExtensionManager` / manifest (the POC is a
  standalone crate API);
- No capability-grant paths for fs/network/exec (`exec` is trust-gated at
  the protocol layer and theoretically usable, but the POC has no
  end-to-end `HostServices` demo);
- WASI p2/component model not adopted (p1 has the most mature tooling;
  the p1 ctx is built on the p2 implementation, so future migration cost
  is low);
- No hot reload (deferred in v1 as well).
