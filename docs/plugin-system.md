# Tack Plugin System

**English | [简体中文](plugin-system.zh-CN.md)**

> This document is the overall design and current-state description of the
> Tack plugin system (2026-05, after P1–P4 landed). Protocol details live
> in three topical documents: [hooks.md](hooks.md) (lifecycle hooks),
> [extensions.md](extensions.md) (the tack-ext subprocess/WASM plugin
> protocol), and [extensions-v2.md](extensions-v2.md) (WASM carrier
> design). This document stitches them into one panorama and explains the
> design trade-offs. The clean-slate redesign — DX-first three-level
> plugin model, schema-generated tack-RPC v3, plus the enterprise
> management plane — lives in [plugin-roadmap.md](plugin-roadmap.md).

## 1. Architecture: one core, three entry points

```
                ┌──────────────── tack core ────────────────┐
                │  tack-agent-core::AgentHooks (events +     │
                │  mutation points)                          │
                │  before/after_tool_call · transform_context│
                │  chained (HooksChain) · all fail-open      │
                └──────┬───────────────┬──────────────┬──────┘
                       │               │              │
              ┌────────┴───────┐ ┌─────┴───────┐ ┌────┴─────────┐
              │ hooks engine   │ │ tack-ext RPC│ │ built-in     │
              │ Claude-compat  │ │ long-lived  │ │ hooks        │
              │ declarative    │ │ plugins     │ │ (permissions/│
              │ one-shot cmds  │ │ process/wasm│ │ compaction/  │
              │                │ │             │ │ budget/queue)│
              └────────────────┘ └─────────────┘ └──────────────┘
```

The design borrows from two proven samples:

- **The core follows pi (the TS upstream)**: the `AgentHooks` trait is an
  extension bus with mutation points — tool calls can be
  intercepted/rewritten, context transformed, results patched. Every
  extension form is just a frontend over this core.
- **The entry points follow Codex**: the most frequent plugin needs
  (guardrails, policies, context injection) are covered by **declarative
  Claude Code-compatible hooks** — one-shot commands with JSON
  stdin/stdout, no long-lived processes, and direct compatibility with the
  Claude Code hooks ecosystem.

| Layer | Form | Best for | Document |
|---|---|---|---|
| **hooks engine** | One-shot commands or LLM evaluations declared in settings/bundles | Interception, policies, injection, notifications | [hooks.md](hooks.md) |
| **tack-ext plugins** | Long-lived subprocesses or WASM modules, NDJSON RPC | Long-lived state, custom tools/commands, provider bridges, interactive dialogs | [extensions.md](extensions.md) |
| **built-in hooks** | Rust implementations (permissions, compaction, budget, queue) | Core behavior | — |

## 2. The hooks engine (Claude Code compatible)

**Configuration as plugins**: no extension code required — declare them in
settings.

### 2.1 Configuration sources and merging

Merged in order; later sources append after earlier ones:

1. **Managed hooks**: `~/.tack/agent/managed-hooks.json` (the enterprise
   management surface; with `managedHooksOnly: true` **only** this source
   is kept — Codex's `allow_managed_hooks_only` semantics)
2. **settings `hooks.*`**: global `~/.tack/agent/settings.json` + project
   `.pi/settings.json`, deep-merged
3. **Extension bundles**: the hooks.json pointed to by the `hooks` field
   of `extension.json`

`features.shellHooks: false` disables them all.

### 2.2 Events and verdict capabilities

| Event | Timing | Verdicts |
|---|---|---|
| `PreToolUse` | Before a tool call | block / `updatedInput` rewrites arguments (**partial merge** + schema re-validation) / `permissionDecision` (allow skips the prompt, ask forces it, deny intercepts) |
| `PermissionRequest` | About to show a permission prompt | `permissionDecision` answers in place of the user |
| `PostToolUse` | After tool execution | block (the reason is fed back to the model as an error) / `additionalContext` |
| `UserPromptSubmit` | After the user submits | block discards the prompt / `additionalContext` injected into this turn |
| `SessionStart` | Session creation | `additionalContext` injected into the system prompt (non-JSON stdout is injected as plain text, for backward compatibility) |
| `SessionEnd` / `PreCompact` / `PostCompact` / `SubagentStop` / `Interrupt` / `Notification` | Various lifecycle points | fire-and-forget |
| `Stop` | End of an agent turn | block → continue running with the reason (at most once per stop point; `stop_hook_active` prevents loops) |

`SubagentStart` is parsed but not yet emitted.

### 2.3 The three handler types

```json
{ "matcher": "bash|edit",
  "hooks": [
    { "type": "command", "command": "check.sh", "timeout": 30, "async": false },
    { "type": "prompt",  "prompt": "Is this operation safe?", "model": "openai/gpt-5-mini" },
    { "type": "agent",   "prompt": "Check the path risks referenced by this command" }
  ] }
```

- **command**: executed via `$SHELL -c`; hook input JSON goes in on stdin,
  verdict JSON comes out on stdout; exit 2 = block (stderr is the reason);
  timeouts are killed without leaking processes.
- **prompt**: sends the hook input plus instructions to an LLM; the model
  answers with verdict JSON only — a zero-code intelligent guardrail
  ("when unsure, ask").
- **agent**: like prompt, but allows multiple turns plus read-only tools
  (read/grep/find/ls) to survey the workspace before ruling.

LLM evaluation goes through a dedicated provider adapter (not over the
extension event channel, not visible to plugins); it uses the session
model by default, overridable via the `model` field.

### 2.4 Semantic guarantees

- matcher: empty/`*` matches everything; without metacharacters it is an
  exact name match (`|` for alternation); otherwise a regex.
- Multiple handlers merge: first block wins; permissions take the
  strictest (deny>ask>allow); `additionalContext` accumulates.
- **Everything is fail-open**: hook failure/timeout/bad JSON only produces
  a warning and never stalls the agent.

## 3. tack-RPC v3 long-lived plugins (process + WASM carriers)

A plugin is an independent process (or WASM module) speaking **tack-RPC
v3**: JSON-RPC 2.0 over NDJSON stdio (both-directions requests,
`$/cancelRequest`, 30s call timeout, crash isolation). The protocol's
single source of truth is
[`protocol/tack-rpc.openrpc.json`](../protocol/tack-rpc.openrpc.json);
the host types (`tack_ext::rpc3`) and the SDK types for TypeScript and
Python are generated from it, so all three SDKs share one schema.

### 3.1 Capability surface

Plugins declare **independent, optional capabilities** at the
`initialize` handshake; undeclared capabilities are never called (the
host answers `ERR_CAPABILITY_NOT_GRANTED` if it must).

| Direction | Methods/notifications |
|---|---|
| host → plugin | `tools/execute`, `commands/invoke`, `hooks/beforeToolCall` (allow/deny/**rewrite**), `hooks/transformContext` (full-context replacement), `hooks/afterToolCall` (per-field result patch), `approval/review` (approval chain), `autocomplete/provide`, `events/lifecycle` (subscription-gated), `widgets/action`, `provider/stream` + `provider/streamCancel` (provider bridge) |
| plugin → host | `ui/notify/select/confirm/input` (TUI dialogs), `session/get`, `session/sendUserMessage`, `snapshot/get` (read-only digest), `config/get`, `exec/run` (trust-gated), `host/registerProvider` (LLM provider registration), `provider/streamEvent` + `provider/event` (provider bridge), `widgets/update`, `logs/emit`, `warnings/emit` |

SDKs exist for Rust (`tack-ext-sdk`), TypeScript (`@tack/plugin`), and
Python (`tack-plugin`); `tack ext new` scaffolds any of them, and
`tack ext dev`/`ext test` drive a plugin against a mock-host scenario
file without a session.

### 3.1b Run modes and headless degradation

All four run modes load plugins (see the matrix in §6). Non-TUI modes
(print/rpc/acp) degrade deterministically: tools, interception,
lifecycle events, and `exec/run` (trust-gated) work as usual;
`ui/select|confirm|input` answer `ERR_CAPABILITY_NOT_GRANTED`;
`session/*` answers `ERR_METHOD_NOT_FOUND`; `ui/notify` goes to the log.
`host/registerProvider` is honored in every mode — provider
registration is mode-independent (it writes the process-global runtime
registry that every mode's model resolution reads), and bridged
providers serve inference headless exactly like native ones. A plugin
learns the mode and the available surfaces from the initialize payload's
`mode` and `capabilities`.

### 3.1c The approval chain (`approval/review`)

When the built-in permission flow is about to prompt a human, plugins
that declared `capabilities.hooks.approvalReview` get first crack at the
decision — every reviewer is consulted in load order, the first
`allow`/`reviewed` claim wins, and a pass (null) or `askUser` moves to
the next reviewer. Plugin `hooks/beforeToolCall` bridges run BEFORE the
permission layer in every surface's hook chain, so reviewers (and the
dialog) see the FINAL, post-rewrite arguments. Composition with the
built-in modes:

```text
deny rules → PreToolUse hook decisions → mode gate (plan/acceptEdits/bypass)
  → allow rules + allow-always cache → PLUGIN APPROVAL CHAIN
  → PermissionRequest hooks → user prompt
```

- A claimed `allow`/`reviewed` approves the call one-shot (nothing
  persists into allow-always state); `reviewed` exists so a reviewer
  that vetted the call itself (an LLM pass, its own UI) is
  distinguishable from a blanket auto-allow in the audit event.
  `askUser` claims nothing: it is logged for audit and iteration
  continues — an early cautious reviewer cannot wedge a later
  auto-approver.
- The chain sees only calls that WOULD prompt: allow-rule/mode-approved
  calls never reach it (plugins observe those via
  `hooks/beforeToolCall`), and bypass mode / headless print runs never
  prompt at all. Two more guards sit in front of it: a PreToolUse
  `permissionDecision: "ask"` verdict forces the human dialog past the
  chain, and once untrusted web/MCP content entered the context,
  mutating (non-read-only) calls skip the chain — the human must be
  asked; a chain claim must not silently approve (the same distrust the
  prompt-injection defense applies to allow rules and the allow-always
  cache).
- Fail-open: a reviewer error — including `unsupported_capability` from
  carriers that do not implement `approval/review` (Level-2 MCP and WIT
  component carriers today) — degrades to "pass", and every call carries
  the standard 30s request timeout. Claims, passes, and reviewer
  failures are structured tracing events (target `plugin_approval`), so
  managed `auditSink` deployments see them like policy decisions.
- Wired surfaces: **TUI, rpc, ACP, and the remote host** — every
  prompt-capable surface consults the chain at its would-prompt point
  (the TUI dialog, the rpc `permission_request` event, ACP
  `session/request_permission`, the remote `PermissionRequest`
  broadcast).
- Managed policy can strip a plugin's hook bridges wholesale with
  `pluginPolicy.plugins."<id>".hooks: false`: the plugin's
  tools/commands still load, but it contributes no
  `beforeToolCall`/`transformContext`/`afterToolCall`/`approvalReview`
  bridge (audited with `audit_narrow`). An org that allows a plugin
  for one benign tool must not silently grant it rewrite/block power
  over every tool call in the session.

### 3.1d Provider bridges (`provider/stream`)

A plugin that declares `capabilities.provider.stream` can serve
inference **directly** — no HTTP hop. It registers a provider with
`bridge: true` (typically from the SDK's `on_ready` hook):

```jsonc
// plugin → host: host/registerProvider
{ "provider": { "id": "acme-agent", "bridge": true, "models": [ … ] } }
```

Every model is assigned the reserved api kind **`ext-provider-bridge`**
(a conflicting explicit `api` is a registration error; `baseUrl`/
`apiKey`/`headers` are ignored — a bridge manages its own credentials,
CLI-login style). The models become selectable via `/model` and
resolvable like any runtime provider, in **all four run modes**. The
host resolves them to the plugin's serving connection at stream time.

The streaming model fits the v3 peer's 30s request bound:
`provider/stream` is a **fast ack** (synchronous validation only); the
turn's events then flow as plugin→host `provider/streamEvent`
notifications demuxed by `streamId` — one `AssistantMessageEvent` per
notification, ending with exactly one terminal event (`done`/`error`).
`provider/streamCancel` aborts an in-flight stream (user pressed Esc);
the host synthesizes a terminal in-band `Error` after a 5s grace period
if the plugin goes silent, and also on carrier death or protocol
violations — so a misbehaving plugin degrades only its own provider and
the agent loop treats a bridged provider exactly like a native one.
Carrier support: process and WASI-stdio WASM serve streams; the WIT
component and MCP carriers structurally cannot (bridge registrations
from them are rejected). The SDKs own the plumbing (streamId scoping,
ack/cancel wiring, terminal enforcement); usage/cost is pass-through —
the plugin is the source of truth for its own billing.

`provider/event` (P7c) surfaces out-of-band conditions — rate limits,
warnings — exactly like the native rate-limit path: TUI inline notice
plus a settings-gated desktop notification, log line headless, all
audited under the `plugin_provider` tracing target alongside the other
plugin audit targets. Managed policy can deny serving with
`pluginPolicy.plugins."<id>".provider: false` (the plugin becomes
policy-blocked at load, audited with `audit_narrow`). The same gate
covers the plain (non-bridge) path: `host/registerProvider` without
`bridge: true` requires the declared `capabilities.provider.register`
capability plus managed policy. Built-in provider id collisions are
rejected — a plugin must not shadow a built-in's `baseUrl` while the
host keeps handing it the user's stored credentials — and `apiKeyEnv`
is not resolved from the host environment for plugin-registered
providers (a plugin-chosen variable name paired with a plugin-chosen
`baseUrl` would harvest host credentials). See
[plugin-provider-bridge.md](plugin-provider-bridge.md) §7 for the full
threat model.

### 3.2 Identity, load outcome, and the store

Every plugin has a stable id **`name@source`** (marketplace name, or the
reserved `user`/`project`/`local` sources). Installs live in the
versioned store (`extensions/store/<source>/<name>/<version>/`; active =
`local` else highest semver), atomically staged/swapped with rollback,
pinned in lockfile v2 with drift checks. `plugins."<id>".enabled`
disables without uninstalling; `tack ext enable|disable|upgrade|list`
manage them.

Load failures are **first-class state**: the manager returns every
discovered plugin as `LoadedPlugin { id, enabled, error, … }` and all
consumers filter on `is_active()` — a broken plugin shows up in
`ext list`/doctor instead of vanishing with a log line (Codex's
`PluginLoadOutcome` pattern).

### 3.3 The carriers

| | process (default) | wasm (WASI stdio) | wasm (WIT component) |
|---|---|---|---|
| Plugin form | Any executable | WASI p1 module (`.wasm`/`.wat`) | Component-model binary/text |
| Protocol | **tack-RPC v3 over stdio** (same schema) | same | `tack:plugin@0.3.0` exports, JSON-string payloads (rpc3 types) |
| Isolation | Process boundary | wasmtime sandbox: no fs/network/environment variables | Structural: the world imports no WASI at all |
| Resource limits | None (trust gating) | fuel + epoch wall-clock + memory hard caps (manifest `limits`, host-clamped) | same ceilings, per call |
| Capability grants | — (a process is inherently fully privileged) | Explicit manifest `capabilities` declarations: fs preopen (ro/rw), env (literals or host passthrough), args; audit log at load time | — (grants are WASI-only, ignored with a warning) |

The process and WASI-stdio carriers share one `JsonRpcPeer` (transport
abstracted as AsyncRead/AsyncWrite): handshake, timeout, cancellation,
and dead-peer fail-fast semantics are identical. The component carrier
implements the same host→plugin surface (`PluginConnection`) over typed
WIT calls instead of a wire protocol. Examples:
[`examples/extensions/hello-wasm/`](../examples/extensions/hello-wasm/)
(a handwritten-WAT v3 protocol reference),
[`examples/extensions/hello-component/`](../examples/extensions/hello-component/)
(a handwritten component-WAT reference).

### 3.4 Level 2: MCP server plugins

An extension can skip tack-RPC entirely: `carrier: "mcp"` plus one
`mcpServer` entry makes an MCP server **the plugin** (see extensions.md
§2.1). The host connects at load, adapts tools/resources/prompts into
the plugin's capability list with the plugin's identity (attribution,
policy, interception, and the untrusted-content defense all apply
uniformly), and never spawns a tack-RPC process. This is the third
carrier family beside process/WASM: same load-outcome model, same
`PluginConnection` surface, MCP underneath.

## 4. Distribution: bundles + marketplace
## 4. Distribution: bundles + marketplace

### 4.1 Full extension.json field list

```json
{
  "name": "my-ext",
  "command": "node", "args": ["plugin.js"], "env": {},
  "carrier": "process | wasm",
  "module": "plugin.wasm",
  "limits": { "maxFuel": 1000000000, "maxMemoryBytes": 268435456, "maxExecutionMs": null },
  "capabilities": {
    "fs": [{"host": "data", "guest": "/data", "access": "read-only"}],
    "env": {"LITERAL": "1"},
    "args": ["--verbose"],
    "network": {"tcp": true}
  },

  "hooks": "hooks/hooks.json",
  "mcpServers": "mcp.json",
  "skills": ["skills/"]
}
```

The last three are **bundle fields**: one extension directory can
simultaneously contribute declarative resources (hooks merged into the
session hook configuration, MCP servers merged into connections, skill
directories merged into discovery) without running a plugin process —
**a bundle-only manifest (no command/module) is legal**. This corresponds
to Codex's "plugin = declarative data package" model.

### 4.2 Installation and marketplace

```sh
tack ext install <git-url>[#<ref>] | <dir>   # direct install; #ref = tag/branch/commit
tack ext list / remove <name>
tack ext verify                              # verify installed files against the lockfile

tack ext marketplace add acme <file|url> [--public-key <hex>]  # register a catalog (signed catalogs need the public key, TOFU pin)
tack ext marketplace list [acme]
tack ext install <plugin>@acme            # install resolved via the catalog (signature re-verified)
tack ext marketplace remove acme
```

Install means pin: the commit of a git install is written to
`~/.tack/agent/extensions-lock.json`, and at startup a HEAD mismatch
defaults to **skipping the load** (`extensionLockRequired: false`
degrades this to a warning only). Marketplace catalogs support ed25519
signatures (see [extensions.md](extensions.md) §1b/§1c).

Discovery paths: `~/.tack/agent/extensions/*` (always loaded) → settings
`extensionPaths` → `<project>/.pi/extensions/*` (**project-trust
gated**).

## 5. Security model

- **Process/WASM isolation**: plugin crashes don't hurt the agent;
  pending calls to a dead plugin fail immediately.
- **Project trust** (`/trust`): project-local extensions and hooks from
  `.pi/settings.json` take effect only after trust is granted; the `exec`
  method is trust-gated.
- **Declarative deny wins**: `permissions.deny` rules override everything
  (including a hook's `permissionDecision` allow and bypass mode).
- **WASM fully sandboxed by default**: no preopened directories, no
  network, no environment variables; capability grants are an explicit
  path (fs/network allowlists are a v2.x design item).
- **Managed hooks**: enterprises can lock down execution to managed hooks
  only.
- **Subagents inherit plugin guardrails**: child loops run the session's
  plugin hook bridges by default (`subagents.inheritPlugins: "hooks"`),
  so delegation cannot bypass a guardrail plugin's `beforeToolCall`
  verdicts; `"full"` additionally inherits plugin tools.
- Verdict chain order: SessionHooks → **lifecycle hooks** → tack-ext
  plugins → permission hooks → queue/budget. The permission layer
  (declarative deny, the mode gate, the approval chain, and the dialog)
  runs AFTER plugin `beforeToolCall` bridges, so it sees — and the
  dialog displays — the FINAL, post-rewrite arguments; a rewrite can
  no longer smuggle content past a deny rule or an approval.

## 6. Run-mode support matrix

| Capability | TUI | print | rpc | acp |
|---|---|---|---|---|
| hooks engine (all events) | ✓ | PreToolUse/PostToolUse/UserPromptSubmit/Compact/SubagentStop | PreToolUse/PostToolUse | — |
| prompt/agent handlers (LLM evaluation) | ✓ | ✓ | ✓ | — |
| tack-ext plugins (process + wasm) | ✓ | ✓ (headless degradation) | ✓ (headless degradation) | ✓ (headless degradation) |
| approval chain (`approval/review`) | ✓ | — (no prompts in bypass) | ✓ | — |
| bundle resources (hooks/mcp/skills) | ✓ | ✓ | ✓ | mcp |
| declarative widgets / autocomplete | ✓ | — (declarations accepted but ignored) | — | — |

## 7. Examples

| Example | Demonstrates |
|---|---|
| `crates/tack-ext-sdk/examples/hello_rpc3.rs` | Minimal Rust SDK plugin |
| `tack-v3-demo-plugin` (tack-ext-sdk bin) | Feature-rich fixture: tools, commands, hooks, events, widgets, autocomplete |
| `sdk/typescript` / `sdk/python` | The TS/Python SDK packages with e2e tests |
| `examples/extensions/hello-wasm/` | **WASM sandbox plugin** (handwritten WAT, v3 protocol reference) |
| `examples/extensions/hello-wasm-caps/` | **WASM capability grants** (fs preopen demonstrating readfile; errno without the grant) |
| `tack ext new <dir> <rust|ts|python>` | Scaffolding with a starter scenario |

## 8. Tests and current state

- hooks engine: 20+ tests (dual-format config parsing, Claude verdict
  protocol, matcher, exit-2/timeout killing, updatedInput merge,
  permissionDecision recording and consumption)
- tack-RPC v3 (`tack_ext::v3` + `tack-ext-sdk`): peer roundtrips,
  cancellation, timeouts, dead-peer fail-fast, 12 SDK e2e (handshake,
  verdicts, capability gating, host services, shutdown) + TS (7) and
  Python (8) SDK e2e
- WASM carrier (tack-ext-wasm): 20 tests (v3 handshake + tools/execute
  e2e, fuel/epoch/memory/table/instance caps, capability grants, example
  WAT)
- extension host (tack-app): identity/discovery, store layout, atomic
  install/upgrade/verify, lockfile v2, enable/disable, marketplace
  signatures, load outcome (failed/disabled plugins), widget registry,
  bundle resources, wasm e2e
- Full tack-app: 410+ lib tests + the integration suite, all green

**Explicitly not done**:

- Direct compatibility with upstream TS pi extensions (an in-process
  ExtensionAPI and an RPC protocol are two different worlds; the viable
  path is a Node sidecar extension host, not yet a project)
- `SubagentStart` event emission, and wiring the hooks engine into ACP
  mode (ACP plugins work, but shell hooks do not run there)
- Hot reload (plugin tools/hooks are woven into the agent loop at session
  start; hot-swapping costs far more than it gains — use `ext install` +
  restart instead)
- Enterprise policy (P5), metrics sidecar + distribution sync (P6) —
  Level-2 MCP server plugins and the WIT/component WASM carrier (P4)
  landed and are documented in §3.3/§3.4
- First-class provider bridges (P7) landed: §3.1d and
  [plugin-provider-bridge.md](plugin-provider-bridge.md)
