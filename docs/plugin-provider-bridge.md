# P7: First-Class Provider Bridges

**English | [简体中文](plugin-provider-bridge.zh-CN.md)**

> Status: **design — not landed**. This is the P7 milestone design for the
> plugin roadmap ([plugin-roadmap.md](plugin-roadmap.md) §11). Context:
> [plugin-system.md](plugin-system.md) §3.1 (the tack-RPC v3 capability
> surface), §3.1b (headless degradation), and §3.2 (identity/load outcome).
> When P7 lands, fold the landed behavior back into plugin-system.md and
> record the protocol additions in [compatibility.md](compatibility.md).

## 1. The gap

`host/registerProvider` today registers a **registry entry only**. The
payload is a `RuntimeProviderSpec` (`tack-ai/src/providers.rs`):

```rust
pub struct RuntimeProviderSpec {
    pub id: String,
    pub base_url: String,
    /// Wire protocol id (`openai-completions`, `anthropic-messages`, …).
    pub api: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<serde_json::Value>,
    pub models: Vec<CustomModel>,
}
```

`register_runtime_provider` builds a `CustomProviderModels` entry; the
host's built-in **HTTP** adapters then do the inference against `baseUrl`.
There is **no host→plugin inference callback** anywhere in the v3 surface
(the host→plugin methods are `tools/execute`, `commands/invoke`,
`hooks/*`, `approval/review`, `autocomplete/provide`, `events/lifecycle`,
`widgets/action` — no `provider/*`).

Two consequences:

1. **A provider that is not an HTTP API cannot be a plugin.** A local
   CLI/agent bridge (CodeBuddy-style: spawn a CLI, speak JSONL, bridge
   tools back), an in-process vendor SDK, or an auth-brokered gateway has
   exactly two options today: an in-crate `Provider` impl
   (`tack-ai/src/codebuddy.rs` — 3.8k lines behind `CODEBUDDY_API`), or a
   localhost HTTP shim translating a wire protocol the host already speaks
   — an extra streaming parser/serializer between the user and the model,
   with the host adapter's retries/timeouts/error mapping layered on top.
2. **Registration is TUI-only.** In print/rpc/acp modes
   `host/registerProvider` answers `ERR_METHOD_NOT_FOUND`
   (`tack-app/src/ext_headless.rs`, pinned by tests), so even the shim
   route degrades to TUI-only — while native providers work in all four
   run modes.

The upstream TS pi never had this gap: its extension API registers a
provider with an in-process inference **function** (`streamSimple`), which
is why pi-codebuddy-sdk could be a pure plugin with no HTTP proxy. The
tack `Provider` trait (`tack-ai/src/provider.rs`) is the exact analogue —
`stream(model, context, options) -> AssistantMessageEventStream` — but the
plugin protocol cannot reach it yet.

**Design freedom**: there are no released plugins and no v3 deployments.
P7 reshapes the v3 provider surface into its final form directly — no
aliases, no migration shims, no reserved-field archaeology. (TS-pi
wire/storage compatibility is untouched either way; it does not intersect
this surface. See §6.)

## 2. Goals and non-goals

**Goals**

- G1 — A plugin can **serve inference directly**: it receives
  `(model, context, options)` and streams `AssistantMessage` events back.
  No HTTP hop.
- G2 — Provider plugins work in **all four run modes** (TUI, print, rpc,
  acp), like native providers.
- G3 — **Native UX parity**: `/model` listing, thinking levels passed
  through, abort (Esc), rate-limit/warning surfacing, usage/cost
  reporting.
- G4 — Provider bridges live in the **same load-outcome, policy, and
  audit model** as every other capability (P3/P5/P6): policy-blocked or
  disabled plugins register nothing; everything auditable.
- G5 — **SDK parity** across Rust/TS/Python, and `ext dev` / `ext test`
  can drive a provider plugin against a scenario without a session.

**Non-goals**

- Inference on the **WIT component carrier** — structural: the world
  imports no WASI (no network, no fs), and its synchronous per-call limits
  (fuel/epoch/wall-clock) cannot host a minutes-long stream. Documented
  unsupported, same posture as `approval/review` on that carrier.
- Provider bridging on the **MCP carrier** — no MCP concept maps to
  "serve inference for the host" (sampling is the reverse direction).
- Migrating built-in providers onto the bridge. CodeBuddy stays native;
  the bridge is for third parties. (A future native provider *may* move
  out-of-tree as a dogfood, but that is a product decision, not P7.)
- Model-list push notifications. Re-registration via
  `host/registerProvider` (registry replace-by-id) already covers
  discovery refresh and metadata learning; it matches how the native
  codebuddy provider updates its cache.

## 3. P7a — headless `registerProvider` (the small unlock)

Wire `register_runtime_provider` into the headless host services
(`ext_headless.rs`) for print/rpc/acp, replacing the pinned
`ERR_METHOD_NOT_FOUND`. Mode gating stays for `session/*` (a headless
session has no interactive owner); provider registration is
mode-independent — it only writes the process-global runtime registry
that every mode's model resolution already reads.

Effect on its own: HTTP-shim provider plugins work headless. It is also
the registration path P7b's bridge providers use, so it lands first.

- Update the pinned headless tests: `host/registerProvider` succeeds and
  the provider appears in model resolution; `session/*` stays
  `ERR_METHOD_NOT_FOUND`.
- ACP note: acp loads plugins with headless degradation; once registered,
  bridged models are selectable by acp clients like any runtime provider.

## 4. P7b — the provider stream bridge

### 4.1 Protocol additions (OpenRPC is the single source)

New plugin-declared capability (in `InitializeResult.capabilities`):

```json
"provider": { "stream": true }
```

Undeclared ⇒ the host never calls `provider/stream` and rejects bridge
registrations with `ERR_CAPABILITY_NOT_GRANTED` — same rule as every
other capability.

New host→plugin methods:

| Method | Kind | Purpose |
|---|---|---|
| `provider/stream` | request → `null` ack | Start one inference stream. Synchronous validation only (capability, params shape); everything after the ack rides the event channel. |
| `provider/streamCancel` | notification | Abort an in-flight stream (host cancelled / user pressed Esc). |

New plugin→host notifications:

| Method | Purpose |
|---|---|
| `provider/streamEvent` | One `AssistantMessageEvent` per notification, demuxed by `streamId`. Exactly one terminal event (`Done`/`Error`) ends the stream. |
| `provider/event` | (P7c, §5) Provider-scoped out-of-band events: rate limits, warnings. |

`ProviderStreamParams`:

```json
{
  "streamId": "host-generated, unique per connection",
  "model":    { "...": "the resolved registry Model entry, as JSON" },
  "context":  { "...": "the full session Context: system prompt, messages, tools" },
  "options":  {
    "maxTokens": 32768, "temperature": 1.0,
    "reasoning": "high", "thinkingBudgets": { "...": "..." },
    "toolChoice": "auto", "cacheRetention": "short",
    "sessionId": "…", "headers": {}, "samplingParams": {}
  }
}
```

`options` is the **serializable subset** of tack-ai's `StreamOptions`.
Deliberately excluded:

- `apiKey` — a bridge provider manages its own credentials (the CLI-login
  model: "if the tool works in your terminal, it works here"). The host
  never brokers keys for bridges.
- `cancel` / `retryCancel` — transport-level concerns. Cancellation rides
  `provider/streamCancel`; retry policy belongs to the plugin (§4.2).

`RegisterProviderParams.provider` (the `RuntimeProviderSpec` shape) gains
one optional field:

```json
{ "id": "acme-agent", "bridge": true, "models": [ … ] }
```

With `bridge: true`:

- the host assigns every model the reserved api kind
  **`ext-provider-bridge`** (the `ext-` prefix matches the `ext__` tool
  naming convention); a conflicting explicit `api` is a registration
  error;
- `baseUrl`/`apiKey`/`headers` are ignored (no HTTP endpoint exists);
- one plugin may serve several provider ids (one `registerProvider` call
  each); the bridge registry maps provider id → serving connection.

Schema division of labor follows the `RegisterProviderParams.provider`
precedent: the OpenRPC document owns the method envelopes and the
capability shape; `model`/`context`/`event` are typed as
provider-shaped/event-shaped JSON (`{}`) and parsed by tack-ai's serde
types. To that end `AssistantMessageEvent` gains
`Serialize`/`Deserialize` derives (today it is `Clone + Debug` only);
`Model`, `Context`, `AssistantMessage` are already serde.

### 4.2 Streaming model: events ride notifications

The v3 peer enforces a 30s request timeout; inference runs for minutes.
So `provider/stream` is a fast ack, and the turn's events flow as
plugin→host `provider/streamEvent` notifications:

```
host                     plugin
 │── provider/stream ─────▶│  (ack: null, or capability/validation error)
 │◀─ provider/streamEvent ─│  Start
 │◀─ provider/streamEvent ─│  ThinkingDelta, TextDelta, ToolCallEnd, …
 │◀─ provider/streamEvent ─│  Done{message}        (terminal, exactly one)
 │── provider/streamCancel▶│  (only on abort; plugin ends Error{Aborted})
```

Terminal semantics reuse the existing contract
(`AssistantMessageEvent::is_terminal` / `final_message`): `Done` carries
the final message, `Error` carries the error message with
`stop_reason: Error | Aborted`. This preserves the `Provider` trait
contract — **stream errors are in-band**, never transport failures — so
the agent loop treats a bridged provider exactly like a native one.

Fail-open synthesis: the host synthesizes a terminal `Error` event when

- the carrier dies mid-stream (dead-peer fail-fast, a v3 invariant);
- a grace period elapses after `streamCancel` without the plugin's own
  terminal event (leaning 5s, tuned at implementation);
- the plugin violates the protocol (events after terminal, a second
  terminal, unknown `streamId`) — warn, audit, synthesize `Error` if the
  stream is still open.

There is **no host-side wall-clock cap** on a stream in v1: native
adapters own their retry/timeout policy, and so do bridge plugins (the
plugin knows its backend's limits; the host does not). See open question
§8.1 for the idle-watchdog discussion.

### 4.3 Host-side architecture

Dependency direction is preserved by **trait erasure** — the same pattern
as the approval chain (`ApprovalReviewer` in `tack-app::approval` erases
the tack-ext dependency):

**tack-ai** (no tack-ext dependency):

```rust
/// Reserved api kind for plugin-served providers.
pub const EXT_PROVIDER_BRIDGE_API: &str = "ext-provider-bridge";

/// One serving endpoint for a bridged provider id. Implemented in
/// tack-app over `PluginConnection`.
pub trait ProviderStreamBridge: Send + Sync + std::fmt::Debug {
    /// Start a stream; events are delivered to `sink` until a terminal
    /// event. Errors returned here are pre-ack (validation) errors.
    fn stream(
        &self,
        params: BridgeStreamParams,          // serde mirror of ProviderStreamParams
        sink: AssistantMessageEventSender,   // the stream's event channel
    ) -> Result<(), String>;
    /// Best-effort abort (host cancelled).
    fn cancel(&self, stream_id: &str);
}

pub fn register_provider_bridge(id: &str, bridge: Arc<dyn ProviderStreamBridge>);
pub fn unregister_provider_bridge(id: &str);
```

`provider_for(model)` gains a branch: `EXT_PROVIDER_BRIDGE_API` →
`BridgedProvider { provider_id }`, a `Provider` impl that looks the
bridge up in the registry at `stream()` time (so re-registration after
plugin reload picks up the new connection), wires the sink into an
`AssistantMessageEventStream`, and converts every failure into in-band
`Error` events.

**tack-app**:

- `ExtProviderBridge` implements `ProviderStreamBridge` over
  `Arc<dyn PluginConnection>`. `PluginConnection` gains
  `provider_stream` / `provider_stream_cancel`; carriers that cannot
  serve them return the existing `unsupported_capability()` (-32002).
- **Event routing**: host services (TUI `extension_host.rs` and
  `ext_headless.rs`) already receive all plugin→host traffic. A
  `StreamSinks` registry maps `(connection identity, streamId)` →
  `AssistantMessageEventSender`; it is populated before the ack returns
  and cleaned up on terminal/cancel/carrier-death.
- **Lifecycle**: `registerProvider(bridge: true)` registers the models
  and the bridge atomically. Plugin disable/unload/crash ⇒ unregister
  both (the runtime registry already replaces/removes by id); in-flight
  streams receive the synthesized in-band `Error`. A policy-blocked or
  disabled plugin registers nothing — load-outcome semantics (P3) apply
  unchanged.
- **Concurrency**: `streamId` demuxes; subagent loops share the parent's
  plugin connections in-process (`subagents.inheritPlugins` decision), so
  concurrent streams from parent and children multiplex over one
  connection naturally. The TS pi-codebuddy-sdk needed a `Symbol.for`
  global guard for exactly this shared-`streamFn` hazard; the bridge
  designs it away.

### 4.4 Carrier matrix

| Carrier | `provider/stream` | Why |
|---|---|---|
| process | ✓ | Full privileges: spawn CLIs, open sockets, hold vendor SDKs. |
| WASI-stdio (wasm) | protocol-level ✓ | The JSON-RPC methods exist identically; practical use awaits network capability grants (a documented v2.x item — today a sandboxed module cannot reach an LLM API). |
| WIT component | ✗ `unsupported_capability` | Structural: the world imports no WASI (no network), and synchronous per-call fuel/epoch/wall-clock limits cannot host a long stream. The world (`tack:plugin@0.3.0`) is **unchanged** by P7. |
| MCP (Level 2) | ✗ `unsupported_capability` | No MCP concept maps to serving inference (sampling is the reverse direction). |

A bridge registration from a plugin on a non-serving carrier is rejected
at registration with a clear error; the stream path therefore never sees
`unsupported_capability` at runtime.

### 4.5 SDK surface (sketches)

The SDK owns the plumbing: streamId scoping, ack/cancel wiring, and
**terminal-event enforcement** — exactly one terminal event, with an
automatic `Error` if the handler panics/raises or returns without one.

Rust (`tack-ext-sdk`):

```rust
Plugin::builder("acme-provider")
    .provider_stream(|params, events, cx| async move {
        let mut turn = acme::query(&params.context).await?;
        while let Some(delta) = turn.next().await {
            events.text_delta(delta)?;
        }
        events.done(assistant_message)?;   // or events.error(...)
        Ok(())
    })
```

TypeScript (`@tack/plugin`): `.providerStream(async (params, events, cx) => { … })`.
Python (`tack-plugin`): `@plugin.provider_stream` decorator.

Cancellation surfaces as a `cx.cancel` signal the handler can poll/await;
ignoring it is legal (the host synthesizes the terminal after the grace
period) but discouraged.

### 4.6 Policy, audit, telemetry

- **Trust/mode gating**: registration keeps the existing
  `host/registerProvider` gates. The bar is honest: a bridged provider
  sees the full conversation — it *is* the model.
- **P5 policy**: `capabilities.provider` is narrowed at load like tools
  (intersection at initialize; a managed deny turns the plugin
  policy-blocked with `audit_narrow`). Managed `enabled` pinning works
  unchanged.
- **Audit**: structured tracing target **`plugin_provider`** for
  registration, stream start/end/cancel, protocol violations, and
  synthesized terminals — pinned at INFO in the managed `auditSink`
  EnvFilter alongside `plugin_policy` / `plugin_approval` /
  `plugin_metrics` / `plugin_load`.
- **Telemetry**: load telemetry counts provider-bridging plugins;
  registration failures land in `last-load.json` (doctor reads it; doctor
  never spawns plugins).

### 4.7 The development loop

- `ext dev` scenario format gains a `providerStream` step: scripted
  `(model, context, options)` in, asserted event sequence out. The
  DevHost captures `provider/streamEvent` notifications and can script
  cancel races.
- `ext inspect` prints the provider capability and bridge registrations.
- The `tack-v3-demo-plugin` fixture grows a deterministic fake model
  (scriptable deltas, thinking, tool calls, errors, slow stream for
  cancel tests) — it becomes the e2e provider for the whole bridge test
  suite, and the reference implementation for plugin authors.

## 5. P7c — provider events and usage/cost

Native providers surface out-of-band conditions — the codebuddy
rate-limit path shows an inline TUI warning plus a desktop notification
(`set_rate_limit_notifier` in `tack-ai/src/codebuddy.rs`; headless logs).
P7c generalizes that single-provider hook into a bridge-wide channel:

```json
provider/event  (plugin → host, notification)
{ "provider": "acme-agent",
  "kind": "rateLimited" | "warning" | "info",
  "message": "…", "detail": { "…": "optional structured data" } }
```

- tack-ai: `set_rate_limit_notifier` generalizes to a provider-event
  notifier (kind + provider id + message); the codebuddy provider is
  re-pointed at it unchanged in behavior.
- TUI: inline warning + desktop notification (the `notifications`
  setting gates it), headless modes log — identical to the native path.
- Audit: `plugin_provider` target.

**Usage/cost**: the terminal `AssistantMessage` carries the standard
`Usage` (tokens plus `cost: UsageCost`). Bridged providers report what
their backend reports; a subscription-style backend reports zeroed cost,
token counts real — exactly the native codebuddy semantics. Leaning:
pass-through (the plugin is the source of truth for its own billing);
whether the host ever recomputes from the model's static `cost`
declaration when the plugin reports zeros is decided at implementation
and documented in the landing note.

## 6. Compatibility stance

- **v3 plugin protocol**: no released plugins, no deployments — P7
  reshapes this surface outright and freezes it under the normal rules
  once landed. No aliases, no migration shims.
- **TS-pi compatibility**: untouched. Session formats, RPC/ACP wire
  protocols, the provider-registry file shape, and CLI flags do not
  intersect this change. `RuntimeProviderSpec` gains one optional field;
  `models.json` is unchanged.
- **WIT world**: unchanged (`tack:plugin@0.3.0`); the component carrier
  does not serve inference (§4.4).
- **Sessions naming a bridged model** persist its provider/model ids and
  the `ext-provider-bridge` api kind; resuming without the plugin hits
  the existing model-not-found path — the same behavior as a native
  provider whose CLI disappeared.
- Landing includes a [compatibility.md](compatibility.md) note per
  convention.

## 7. Security and trust

- Registration is trust- and mode-gated (existing `registerProvider`
  gates) and capability-gated (`capabilities.provider.stream`), with
  managed-policy narrowing on top (§4.6).
- The bridge grants the plugin **no new host privileges**: the process
  carrier is already fully privileged; `exec/run` stays separately
  trust-gated; no new host→plugin or plugin→host surface exists beyond
  §4.1/§5.
- The untrusted-content defense is model-source-agnostic: tool calls
  from a bridged model are ordinary `AssistantMessage` content flowing
  through the same permission layer — declarative deny, the mode gate,
  the approval chain, and the prompt all apply unchanged, and web/MCP
  untrusted-context rules keep mutating calls on the human path.
- A malicious or buggy plugin can degrade its own provider (bad events,
  hangs) but not others: protocol violations are contained per-stream,
  audited, and fail-open into in-band `Error`s.

## 8. Milestones, tests, open questions

**Sequencing**: P7a (headless `registerProvider`) is small and lands
first. P7b is the bulk — schema/codegen → tack-ai dispatch →
`PluginConnection` + host routing → three SDKs → dev-loop. P7c follows
P7b and is small.

**Test plan** (repo gates: `clippy --all-targets` 0 warnings, `fmt`,
`rustdoc -D warnings`, `xtask codegen --check`, docs-audit):

- rpc3 codegen roundtrips for the new schemas; peer-level stream e2e on
  the established duplex + `HostClient` + scripted-peer pattern.
- `BridgedProvider`: terminal synthesis on carrier death, cancel grace,
  protocol violations; in-band error mapping; concurrent `streamId`
  demux; registry lookup across re-registration.
- Host: register/unregister across load/disable/crash; all four run
  modes; policy narrowing; audit events; headless registration tests
  replacing the pinned `ERR_METHOD_NOT_FOUND` ones.
- SDKs: e2e per language mirroring the existing suites (Rust 12 / TS 7 /
  Python 8), driven by the fake-model demo fixture; `ext dev`/`ext test`
  scenario steps including cancel races.
- No network in tests, per repo rules — the fake model is in-process.

**Open questions**

1. **Idle watchdog**: should the host synthesize an `Error` for a stream
   with no events for N minutes? Leaning **no for v1** — the plugin owns
   its backend's policy, and native adapters have no such host-side cap
   either. Revisit with field experience.
2. **Cost recompute**: pass-through vs host recompute when the plugin
   reports zeroed cost (§5). Leaning pass-through; settled in the landing
   note.
3. **WASI network grants** timeline: decides when the wasm carrier
   becomes practically useful for bridges (protocol-ready from day one).
4. **Model-list push**: a `provider/modelsChanged` notification so a
   plugin can refresh its model list without re-registering. Leaning
   unnecessary — registry replace-by-id already works and matches native
   learning flows.
