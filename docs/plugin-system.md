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

## 3. tack-ext long-lived plugins (process + WASM carriers)

A plugin is an independent process (or WASM module) speaking an NDJSON
protocol over stdio (three envelope kinds: request / response / event,
30s call timeout, crash isolation).

### 3.1 Capability surface

| Direction | Methods/events |
|---|---|
| host → plugin | `tool.execute`, `command.invoke`, `intercept.tool_call` (allow/deny/**rewrite**), `intercept.context` (gated on subscribing to `"context"`, may replace the full message list), lifecycle events (session/agent/turn/message/tool_execution/model_select/provider boundaries…) |
| plugin → host | `ui.notify/select/confirm/input/set_status` (TUI dialogs), `session.*` (new/switch/branch/set_model/send_user_message…), `exec` (trust-gated), `provider.register` (dynamically register an LLM provider), `log` |

### 3.1b Run modes and headless degradation

All four run modes load plugins (see the matrix in §6). Non-TUI modes
(print/rpc/acp) use headless HostServices: tools, interception, lifecycle
events, and `exec` (trust-gated) work as usual; requests that need a
terminal UI **degrade deterministically** — `ui.notify`/`ui.set_status`
go to the log, `ui.select/confirm/input` return errors, and
`session.*`/`provider.register` return errors. A plugin learns the host
mode from `initialize.payload.mode` and must not depend on interactive
requests for correctness.

### 3.2 The two carriers

| | process (default) | wasm |
|---|---|---|
| Plugin form | Any executable | WASI p1 module (`.wasm`/`.wat`) |
| Protocol | NDJSON over stdio, handshake `protocol: 1` | **Same schema**, handshake `protocol: 2` |
| Isolation | Process boundary | wasmtime sandbox: no fs/network/environment variables |
| Resource limits | None (trust gating) | fuel + epoch wall-clock + memory hard caps (manifest `limits`) |
| Capability grants | — (a process is inherently fully privileged) | Explicit manifest `capabilities` declarations: fs preopen (ro/rw), env (literals or host passthrough), args, network flags (inert under p1 for now); audit log at load time |

The WASM carrier reuses the same `PluginPeer` (the transport layer is
abstracted as AsyncRead/AsyncWrite); handshake, timeout, and fail-fast
semantics for dead plugins are identical to the subprocess carrier.
Example: [`examples/extensions/hello-wasm/`](../examples/extensions/hello-wasm/)
(a handwritten-WAT protocol reference implementation).

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
- Verdict chain order: SessionHooks → **lifecycle hooks** → permission
  hooks → queue/budget → tack-ext plugins (what the last one sees is the
  final set of arguments).

## 6. Run-mode support matrix

| Capability | TUI | print | rpc | acp |
|---|---|---|---|---|
| hooks engine (all events) | ✓ | PreToolUse/PostToolUse/UserPromptSubmit/Compact/SubagentStop | PreToolUse/PostToolUse | — |
| prompt/agent handlers (LLM evaluation) | ✓ | ✓ | ✓ | — |
| tack-ext plugins (process + wasm) | ✓ | ✓ (headless degradation) | ✓ (headless degradation) | ✓ (headless degradation) |
| bundle resources (hooks/mcp/skills) | ✓ | ✓ | ✓ | mcp |
| declarative widgets / autocomplete | ✓ | — (declarations accepted but ignored) | — | — |

## 7. Examples

| Example | Demonstrates |
|---|---|
| `examples/extensions/hello-js/` | Minimal subprocess plugin (Node) |
| `examples/extensions/hello-wasm/` | **WASM sandbox plugin** (handwritten WAT, protocol reference) |
| `examples/extensions/hello-wasm-caps/` | **WASM capability grants** (fs preopen demonstrating readfile; errno without the grant) |
| `examples/extensions/git-checkpoint/` | Events + exec + commands |
| `examples/extensions/protected-paths/` | tool_call interception |
| `examples/extensions/handoff/` | Session control |
| `tack ext-demo-plugin` | Built-in minimal protocol implementation (e2e fixture) |

## 8. Tests and current state

- hooks engine: 20+ tests (dual-format config parsing, Claude verdict
  protocol, matcher, exit-2/timeout killing, updatedInput merge,
  permissionDecision recording and consumption)
- tack-ext: 40+ tests (including intercept.context replacement/gating)
- WASM carrier (tack-ext-wasm): 9 tests (handshake + tool.execute e2e,
  fuel/epoch wall-clock/memory/table/instance-cap rejections, example
  WAT) + ExtensionManager load e2e (wasm plugin + bundle collection)
- Full tack-app: 230+ lib tests + the integration suite, all green

**Explicitly not done**:

- Direct compatibility with upstream TS pi extensions (an in-process
  ExtensionAPI and an NDJSON protocol are two different worlds; the
  viable path is a Node sidecar extension host, not yet a project)
- `SubagentStart` event emission, and wiring the hooks engine into ACP
  mode (ACP plugins work, but shell hooks do not run there)
- Hot reload (plugin tools/hooks are woven into the agent loop at session
  start; hot-swapping costs far more than it gains — use `ext install` +
  restart instead)
- WASM network capabilities actually taking effect (wasmtime-wasi p1 has
  no socket ABI; the manifest flags are future-facing only)
