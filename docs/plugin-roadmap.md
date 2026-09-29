# Plugin System Redesign & Enterprise Roadmap

**English | [简体中文](plugin-roadmap.zh-CN.md)**

> This document is the **clean-slate design** for tack's plugin system.
> Premise (2026-05): tack has **no installed base and no plugin
> ecosystem** — there is no legacy to protect. The v1/v2 NDJSON protocol
> and the flat install layout are therefore explicitly **disposable**:
> this design replaces them where a better answer exists, instead of
> accreting compatibility layers. What survives is the *architecture
> skeleton* (dual carriers, declarative-first, hooks chain, fail-open
> semantics) plus six management-plane patterns borrowed from Codex
> (`codex-rs/core-plugins`, `codex-rs/ext/extension-api`).
>
> Current state lives in [plugin-system.md](plugin-system.md). This
> document is the target; phases P0–P6 each land shippable and are
> recorded in [compatibility.md](compatibility.md) as they do.

## 1. Goals and non-goals

**Goals, in priority order**

1. **Developer experience (DX)**: a hello-world plugin is ≤15 lines in
   Rust, TypeScript, or Python, scaffolded by one command, debuggable
   without a TUI, testable in CI without a model.
2. **Type safety from one source**: the protocol is defined once as a
   machine-readable schema; all host and SDK types are generated.
3. **Sandboxed distribution**: the WASM/component carrier is the form
   plugins are *shared* in; the process carrier is the *development*
   form.
4. **Enterprise-grade management**: governable (managed policy),
   auditable (structured trail), observable (metrics, doctor), reliable
   (atomic installs).

**Non-goals**

- **Hot reload in production sessions.** Plugin capabilities are woven
  into the agent loop at session start. (The *development* loop restarts
  the plugin process freely — that is a new session by definition.)
- In-process dynamic loading (dylib/stable ABI).
- **Backward compatibility with tack-ext protocol v1/v2.** No users
  exist; carrying a translation shim costs more than it saves. The old
  protocol is removed when v3 lands (§10).

## 2. Design principles

1. **Declarative first, code only when needed.** Guardrails, context
   injection, MCP tools, and skills need no plugin process at all.
2. **Standards over bespoke.** JSON-RPC 2.0 envelopes (free libraries
   and validators everywhere), MCP for tool-only plugins (free
   ecosystem), WIT for the sandboxed carrier (free language bindings).
   Custom surface exists only where standards genuinely can't express
   the semantics (interception, widgets, session control).
3. **Narrow capabilities over one fat protocol.** Codex's key lesson
   from `extension-api`: fifteen contributor traits beat one plugin
   trait with forty methods. tack-RPC is split into independent
   capability namespaces; a plugin opts into each explicitly and pays
   nothing for the rest.
4. **Errors are data.** A failed plugin is `LoadedPlugin{enabled,
   error}` in a load outcome, not a log line. Every consumer filters on
   `is_active()`.
5. **Host keeps sovereignty.** Plugins *contribute* and *request*; the
   host decides rendering, admission, policy, and persistence.

## 3. What Codex teaches us (management plane)

Six patterns imported unchanged in spirit; see §8/§9 for where each
lands:

1. `LoadedPlugin{enabled, error}` + `is_active()` — failure as
   first-class state (`plugin/src/load_outcome.rs`).
2. Two-segment identity `name@source`, character-whitelisted so IDs are
   safe path segments (`core-plugin-common/src/plugin_id.rs`).
3. Versioned immutable store with derived activation (`local` wins, else
   highest semver) and staging → re-verify → rename-swap → rollback
   installs (`core-plugins/src/store.rs`).
4. Policy as a load-time filter over the effective config, not scattered
   call-site checks; user policy may only narrow
   (`core-plugins/src/marketplace_policy.rs`).
5. Declared-schema telemetry sidecar for untrusted processes: declare
   operations/dimensions, hand over a sandbox-authorized file, validate
   the drain strictly (`core-plugins/src/plugin_metrics_sidecar.rs`).
6. Fingerprint-idempotent multi-transport sync with scrubbed git
   environment (`core-plugins/src/startup_sync.rs`, `git_policy.rs`).

## 4. The three-level plugin model

A plugin author pays for exactly as much machinery as their idea needs:

```
Level 1   Declarative bundle        extension.json + hooks/MCP/skills files
          (no code)                 guardrails, context, tool wiring

Level 2   MCP server plugin         extension.json declaring an MCP server
          (standard ecosystem)      tool contribution with plugin identity,
                                    policy, attribution — zero tack-specific
                                    code; any existing MCP server qualifies

Level 3   tack-RPC plugin           process (dev) or WASM component
          (full capability surface) (distribution) speaking tack-RPC v3:
                                    interception, lifecycle, widgets,
                                    session control, approval, config,
                                    metrics — via an official SDK
```

- **Level 1** is today's bundle, kept as-is.
- **Level 2** makes MCP a *plugin form*, not just a config feature: the
  host spawns the server, adapts its tools into the plugin model with
  the plugin's identity (attribution, policy, interception applies
  uniformly), and treats its resources/prompts as bundle contributions.
  Authors who only need "give the agent tools" never touch tack-RPC.
- **Level 3** is where tack-specific semantics live — the things MCP has
  no vocabulary for: blocking/rewriting a tool call before it runs,
  observing the agent loop, declarative UI, session control, approval
  chains.

## 5. tack-RPC v3

The v1/v2 protocol (bespoke envelopes, hand-written peers, docs-as-spec)
is replaced by a schema-first protocol designed for SDK generation.

### 5.1 Transport and framing

- Process carrier: newline-delimited JSON over stdio (unchanged — every
  language frames NDJSON trivially, and stdout stays the single bus).
- WASM carrier: WIT/component-model calls (§6.3); NDJSON-over-WASI-stdio
  remains only as a debug carrier.
- Either side may issue requests concurrently; ids are per-sender
  (JSON-RPC semantics).

### 5.2 Envelopes: JSON-RPC 2.0

```json
{"jsonrpc":"2.0","id":7,"method":"tools/execute","params":{...}}
{"jsonrpc":"2.0","id":7,"result":{...}}
{"jsonrpc":"2.0","id":7,"error":{"code":-32001,"message":"denied by policy","data":{...}}}
{"jsonrpc":"2.0","method":"events/turnStart","params":{...}}
```

- Standard codes plus a reserved domain range: `-32001` policyDenied,
  `-32002` capabilityNotGranted, `-32003` pluginUnavailable,
  `-32004` requestTimeout. SDKs map these to typed errors.
- Cancellation: `$/cancelRequest`; long-running work reports
  `$/progress` with a work-done token (LSP conventions — free mental
  model for anyone who has written an LSP/MCP peer).
- Method naming follows MCP: `<namespace>/<verb>`, camelCase verbs.

### 5.3 Initialize and capability negotiation

Modeled on MCP's initialize:

```json
→ {"method":"initialize","params":{
     "protocolVersion":"3.0.0",
     "host":{"name":"tack","version":"x.y.z"},
     "mode":"tui | print | rpc | acp",
     "cwd":"…","trusted":true,
     "capabilities":{"widgets":true,"sessionControl":true,
                     "snapshot":true,"metrics":{"scratchFile":"/…"}},
     "config":{ /* validated against the plugin's config schema */ }}}
← {"result":{
     "protocolVersion":"3.0.0",
     "plugin":{"name":"acme-review","version":"1.4.2"},
     "capabilities":{
       "tools":[{…}],"commands":[{…}],
       "hooks":{"beforeToolCall":true,"transformContext":false},
       "events":["turnStart","turnEnd"],
       "widgets":[{…}],"autocompleteProviders":[{…}],
       "config":{"schema":{…}},
       "metrics":{"operations":{…}} }}}
```

- Version is a semver string; each side declares the range it speaks;
  mismatch is a clean handshake error, not a silent degradation.
- The host's `capabilities` block is how a plugin learns mode-dependent
  availability (no widgets in print mode) instead of discovering it by
  failing requests.
- Plugin capabilities are **all optional and independent** — a metrics-
  only plugin declares only `metrics`.

### 5.4 Method namespaces

| Namespace | Direction | Purpose |
|---|---|---|
| `initialize`, `shutdown` | both | lifecycle handshake |
| `tools/execute` | host → plugin | run a contributed tool |
| `commands/invoke` | host → plugin | run a slash command |
| `hooks/beforeToolCall` | host → plugin | allow / deny / **rewrite** (structured verdict) |
| `hooks/transformContext` | host → plugin | COW context pipeline (opt-in) |
| `hooks/afterToolCall` | host → plugin | per-field result patch (opt-in) |
| `events/*` | host → plugin | lifecycle notifications (subscription-gated) |
| `widgets/update`, `widgets/action` | both | declarative UI state push / interaction |
| `autocomplete/provide` | host → plugin | input-line suggestions |
| `approval/review` | host → plugin | approval chain participant, first-claim-wins |
| `session/get`, `session/sendUserMessage`, … | plugin → host | session control (trust/mode gated) |
| `snapshot/get` | plugin → host | versioned read-only session digest |
| `config/get` | plugin → host | effective per-plugin config |
| `ui/notify`, `ui/select`, `ui/confirm`, `ui/input` | plugin → host | interactive dialogs (mode-gated) |
| `exec/run` | plugin → host | shell on host (trust-gated) |
| `warnings/emit`, `logs/emit` | plugin → host | structured user-facing / diagnostic channels |
| `host/registerProvider` | plugin → host | dynamic LLM provider bridge |

New namespaces join by the same rule Codex uses for its contributor
list: each is a small, precisely-contracted surface with its own
versioning, never a method dump on a god-object.

## 6. One schema, three SDKs

### 6.1 Single source of truth

`protocol/tack-rpc.openrpc.json` — an [OpenRPC](https://open-rpc.org)
document describing every method, notification, and type. CI enforces:

- generated Rust types in `tack-ext` are fresh (an `xtask codegen`
  freshness check),
- generated TS/Python SDK types are fresh,
- the Markdown protocol reference is generated, not hand-written.

Hand-maintained protocol docs (the current docs-as-spec drift risk)
disappear.

### 6.2 SDKs

- **Rust** (`tack-ext-sdk`): a builder API over the generated types
  (proc-macro sugar on top is future polish) —

  ```rust
  Plugin::builder("acme-review")
      .tool(ToolSpec { /* name, description, parameters */ ..spec() },
            |params, cx| async move { Ok(text_output("…")) })
      .before_tool_call(|params, _cx| async move { Ok(allow()) })
      .run()
      .await
  ```

- **TypeScript** (`@tack/plugin`): builder API, zod-flavored schemas,

  ```ts
  export default plugin({
    name: "acme-review",
    tools: { review: tool({ description: "…", schema: … },
                          async (args, cx) => ({ text: "…" })) },
    hooks: { beforeToolCall: async (call) => allow() },
  });
  ```

- **Python** (`tack_plugin`): decorator API, pydantic schemas.

Each SDK owns framing, id correlation, cancellation, timeouts, and
version negotiation — the plugin author never sees an envelope.

### 6.3 WASM carrier: WIT, not handwritten WAT

The sandboxed carrier moves from "NDJSON over WASI stdio with a
handwritten-WAT reference plugin" to a **component-model world**:

```wit
package tack:plugin@0.3.0;

interface tools { execute: func(call: tool-call) -> result<tool-output, string>; }
interface hooks { before-tool-call: func(call: tool-call) -> verdict; }
interface host  { /* config/get, snapshot/get, logs/emit, metrics … */ }

world plugin {
    import host;
    export tools;
    export hooks;   // every interface optional in practice via world variants
}
```

- Guests use `wit-bindgen` (Rust, C, Go, JS, Python) — the SDK problem
  for sandboxed plugins is solved by the toolchain, not by us.
- Host side: wasmtime component linker; the same capability grants,
  fuel/memory/wall-clock limits, and audit logging as today.
- The handwritten-WAT era ends; `examples/` gets wit-bindgen guests.

## 7. The development loop

Tooling is part of the protocol's job:

| Command | Purpose |
|---|---|
| `tack ext new <dir> <rust|ts|python>` | scaffold: manifest, SDK dep, hello tool, CI test |
| `tack ext dev` | mock host: scripted event scenarios, REPL to invoke tools/hooks, watches files and restarts the plugin process (a dev-loop restart, not session hot reload) |
| `tack ext test` | runs the plugin against a fixture host with an assertion API (`expect_tool_call`, `feed_event`, `assert_widget`) — CI-friendly, no model, no network |
| `tack ext inspect` | performs the handshake and dumps declared capabilities, config schema, metrics schema as JSON |

The fixture host is the same `PluginPeer` with scripted inputs, so plugin
tests exercise the real protocol end to end.

## 8. Management plane (from the Codex patterns)

Condensed here; the patterns are §3. All land directly in final form —
no migration shims.

### 8.1 Identity and load outcome

```text
plugin-id = name "@" source      name: [a-z0-9][a-z0-9.-]{0,63} (no "..")
                                 source: marketplace name | "user" | "project" | "local" | "mcp"
```

- `PluginLoadOutcome { plugins: Vec<LoadedPlugin>, warnings }` with
  `LoadedPlugin { id, enabled, error, capabilities }` and
  `is_active() = enabled && error.is_none()`.
- Capability-level problems (one bad hooks file, one malformed MCP
  entry) are warnings, not errors: the plugin loads minus that
  capability.
- Enable/disable lives in settings `plugins."<id>".enabled`; the CLI is
  `tack ext enable|disable <id>`. Disabled plugins keep metadata, never
  spawn.
- `tack ext list` shows `ID / STATE / SOURCE / VERSION / CAPABILITIES`;
  failures and policy blocks are rows, not absences.

### 8.2 Versioned store, atomic installs

```text
~/.tack/agent/extensions/
├── store/<source>/<name>/<version>/   # active = "local" else highest semver
└── data/<source>/<name>/              # writable per-plugin root (WASM: preopened at /data)
```

Every mutation: stage → parse + re-read manifest byte-check (TOCTOU) →
policy check → rename / backup-swap with rollback → lockfile record →
prune superseded versions. `tack ext upgrade [id]` is fingerprint-
idempotent. Git runs scrubbed (`GIT_TERMINAL_PROMPT=0`, inherited
`GIT_*` removed, system PATH, piped stdio, hard timeout).

### 8.3 Enterprise policy

Managed settings layer, enforced **twice**:

```jsonc
{
  "pluginPolicy": {
    "managedPluginsOnly": false,
    "allowedSources": [
      { "type": "git", "url": "https://git.acme.com/tack/plugins.git", "ref": "main" },
      { "type": "hostPattern", "pattern": "^(.+\\.)?acme\\.com$" },
      { "type": "local", "path": "/opt/acme/tack-ext" }
    ],
    "plugins": {
      "review@acme": { "enabled": true,
                       "mcpServers": ["jira"],       // narrow-only intersection
                       "tools": ["create_ticket"] }  // narrow-only intersection
    }
  }
}
```

1. At add/install time — before any clone or network access; denials
   name the rule and its origin layer.
2. At load time — the effective config is filtered before the loader
   sees it (the backstop that makes every downstream consumer compliant
   by construction).

Per-plugin `tools`/`mcpServers` may only shrink the registered set;
managed `enabled` wins over user/project layers. Every decision is
audit-logged with rule and layer.

## 9. Distribution and observability

- **Curated marketplace startup sync**: settings-declared catalog kept
  fresh via git → https archive degradation; fingerprint short-circuit;
  backup/rename/swap activation; cross-process lock; a failing sync
  never blocks startup.
- **Bundle archives** (`tack ext bundle pack`, `tack ext install
  <file.tgz>`): tar.gz with hostile-input extraction (no links, no
  traversal, cumulative size cap) — the air-gapped distribution unit.
- **Catalog v2**: entries gain `installation: available | not-available
  | installed-by-default` and an inline `manifest` fallback for rich
  listing without materialization; unknown fields skipped with a
  warning. ed25519 catalog signatures with TOFU key pinning stay.
- **Metrics sidecar**: plugins declare operations/dimension enums; the
  host provides a scratch file (WASM: dedicated preopen, audited) and
  validates the drain (≤64 KiB / ≤100 lines per drain, exact dimension
  sets, enum values, finite numbers, dedup) before it enters telemetry
  with plugin attribution.
- **Load telemetry**: counts by outcome (`active | disabled | failed |
  policy-filtered`) with error classes (`manifest | handshake |
  register | policy | store`).
- **Doctor**: `tack doctor` reports plugin health — load failures with
  causes, lock drift, policy-filtered entries, WASM component support.

## 10. Compatibility stance

Given zero installed base:

- **tack-RPC v1/v2 is removed** when v3 lands (same release). No
  translation shim. `docs/extensions.md` and `docs/extensions-v2.md` are
  rewritten against v3; the old text moves to git history.
- The **lockfile, store layout, and policy settings land directly in
  their final form** (no v1→v2 migration path is built).
- What *is* preserved, deliberately: the Claude-compatible shell-hooks
  format (external ecosystem), the marketplace signature scheme, and
  project-trust semantics. Those have value beyond tack.
- Every phase still records its storage/wire decisions in
  [compatibility.md](compatibility.md) — the constraint is satisfied by
  design, not by shims.

## 11. Milestones

| Phase | Deliverable | Depends on | Status |
|---|---|---|---|
| P0 | OpenRPC schema + codegen pipeline + generated Rust types | — | landed |
| P1 | tack-RPC v3 host core (peer, initialize, namespaces) + `tack-ext-sdk` (Rust) | P0 | landed |
| P2 | TS + Python SDKs; `ext new` / `ext dev` / `ext test` / `ext inspect` | P1 | landed |
| P3 | Identity, load outcome, enable/disable, versioned store, atomic install/upgrade | P1 | landed |
| P4 | MCP server plugins (Level 2); WIT/component WASM carrier | P1 | landed |
| P5 | Enterprise policy (allow-lists, load-time filter, narrow-only) | P3 | landed |
| P6 | Distribution (curated sync, bundles, catalog v2) + observability (metrics sidecar, telemetry, doctor) | P3, P5 | landed |

P0–P2 are the DX spine and ship first — a plugin system is its
development loop. P3–P6 make it enterprise-grade; their designs are
unchanged by the protocol swap because they live above it.

P4 landed with two deliberate deviations from the sketches above,
recorded in [compatibility.md](compatibility.md):

- **The WIT world carries JSON strings, not typed records**
  (§6.3 sketched typed interfaces): the rpc3 data types (`ToolSpec`,
  `ToolOutput`, `Verdict`, …) serialize as JSON strings over the
  canonical ABI, so the OpenRPC document stays the single schema source
  and wit-bindgen is only asked to frame strings. A typed-record world
  can land later as `tack:plugin@0.4.0` without touching the schema.
- **Level 2 adapts ALL MCP tool surfaces** (tools + resource meta-tools
  + prompt tools), so plugin-attributed servers behave exactly like
  config-file servers, including the untrusted-content defense.

## 12. Open questions

1. **OpenRPC maturity**: if codegen tooling proves thin, fall back to
   JSON Schema-per-method plus hand-rolled generators (same single
   source, less glamour).
2. **WIT async**: wasmtime component-model async support is landing in
   stages; if guest async is not ready at P4, tools/hooks start
   synchronous (host keeps the 30s call timeout) and async upgrades the
   world version.
   → **Resolved at P4**: synchronous start. `tack:plugin@0.3.0` calls
   are blocking host→guest calls with per-call fuel, epoch, and memory
   limits; guest async upgrades the world version when component-model
   async is ready.
3. **MCP elicitation ↔ `ui/*`**: whether Level 2 elicitations map onto
   the Level 3 dialog surface or stay MCP-native; leaning map (one UI
   path for users).
   → **Resolved at P4**: MCP-native. Level-2 connections get the same
   elicitation handling as config-file MCP servers (TUI prompts,
   headless declines) — one UI path for users, no `ui/*` bridge.
   Sampling stays unwired for plugin connections (no session model at
   load time).
4. **Widget/MCP namespacing**: keep first-wins-with-warning collisions,
   or require plugin-prefixed ids in v3? With no legacy, prefixing is
   affordable — leaning yes for MCP server names, no for widgets.
   → **Resolved at P4**: prefixed by construction — Level-2 tools are
   `ext__<plugin-id>__<tool>` like any other plugin tool (the in-plugin
   names stay the server's own, sanitized and deduplicated).
5. **Subagent inheritance**: full plugin set vs narrowed set for
   subagent sessions (Codex `SessionIsolation`); the load-outcome
   filter makes either cheap. Needs a product decision.
   → **Resolved**: `subagents.inheritPlugins` (`none | hooks | full`,
   default `hooks`). Children are non-interactive, so the split is by
   surface, not by plugin: hook bridges follow the child by default
   (a spawned subagent must not bypass guardrail plugins), plugin tools
   only under `full`. The parent's loaded plugins are shared in-process
   (no respawn), and managed settings can pin the key.
6. **Approval chain scope**: `approval/review` is the plugin analogue of
   Codex's `ApprovalReviewContributor`; deciding how it composes with
   the built-in permission modes (order, short-circuit) needs one
   session of protocol prototyping.
   → **Resolved**: the chain sits exactly at the would-prompt point —
   deny rules → PreToolUse decisions → mode gate → allow rules/cache →
   plugin approval chain (load order, first-claim-wins) →
   PermissionRequest hooks → user prompt. Claimed `allow`/`reviewed`
   approve one-shot (audit-distinguished), `askUser` defers to the
   built-in prompt, and reviewer errors degrade to pass (fail-open).
   Wired in the TUI and rpc surfaces; acp/remote-host prompts are a
   documented follow-up.
