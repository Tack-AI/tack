# Tack

<p align="center">
  <img src="assets/logo.svg" alt="Tack logo — a crab pincer shaped like the letter A" width="160">
</p>

**English | [简体中文](README.zh-CN.md)**

A Rust reimplementation of the [pi](https://github.com/earendil-works/pi) coding
agent — interactive TUI, headless print mode,
[ACP (Agent Client Protocol)](https://agentclientprotocol.com) editor integration
(Zed, JetBrains, …), JSONL RPC mode, and remote sessions over TCP/WebSocket with
an embedded browser client. Wire- and storage-compatible with the TypeScript pi.

## Highlights

- **Full TS pi parity** — same session files (transparent v1–v4 migration),
  same RPC/ACP wire protocols, same provider registry and model catalog,
  same CLI flags.
- **More than a port** — background tasks, LSP navigation & diagnostics, file
  checkpoints, persistent memory, sub-agents with worktree isolation,
  cross-session search, cron, OS sandbox, declarative permissions, MCP client
  *and* server, eval framework, and more: **[docs/features.md](docs/features.md)**.
- **42 built-in providers** — 40 mirroring TS pi's registry, plus zero-config
  local **ollama** and **llama.cpp** — with the full model catalog embedded
  (1130 models: context windows, costs, reasoning flags, compat quirks).
- **Extensible** — subprocess plugins in any language (NDJSON/JSON-RPC) or
  sandboxed WASM, plus Claude-Code-compatible lifecycle hooks.
- **Remote-first** — `tack serve` hosts sessions over TCP/WebSocket/TLS with
  token auth; attach from `tack client` or a browser.
- **Private by default** — no telemetry; local crash.log, opt-in tracing, and
  `tack doctor` instead ([docs/telemetry.md](docs/telemetry.md)).

## Installation

### Prebuilt binaries

Download `tack-<target-triple>.tar.gz` / `.zip` from the newest
[GitHub release](https://github.com/sufar/tack/releases) (`tack-v*` tags;
windows x64/arm64, linux x64/arm64, macOS arm64/x64).

> **macOS Gatekeeper**: the binaries are not Developer-ID signed, so a
> browser-downloaded archive trips "Apple 无法验证…". After extracting, run
> `xattr -d com.apple.quarantine tack` once (or right-click → Open).

### Build from source

```bash
git clone https://github.com/sufar/tack.git && cd tack
cargo build --release -p tack-app        # binary: target/release/tack
```

The toolchain is pinned by `rust-toolchain.toml` (MSRV 1.85, edition 2024).

### Self-update

```bash
tack update          # fetch the latest release and atomically replace the binary
tack update --check  # report only
```

Repo resolution: `TACK_UPDATE_REPO` → settings `updateRepo` → `sufar/tack`.
In-process downloads never get the macOS quarantine flag, so self-updates are
unaffected by Gatekeeper.

## Quick start

```bash
# 1. Discover providers and log in (first run)
tack providers                   # every provider: auth status, model count, how to enable
tack login --provider anthropic  # OAuth flow (or reads an API key from stdin)
tack models claude               # browse the catalog, grouped by provider

# 2. Run
tack                             # interactive TUI (default in a terminal)
tack -c                          # resume the most recent session for this dir
tack -p "create hello.py that prints primes < 50 and run it"   # headless print mode
export ANTHROPIC_API_KEY=...      # headless: env key instead of login
```

Useful flags: `--provider`, `--model`, `--api-key`,
`--thinking off|minimal|low|medium|high|xhigh|max`, `--session-dir`,
`--system-prompt`, `--append-system-prompt`, `-t/--tools`, `--mode text|json|rpc`,
`--offline`. Full flag list & TS parity notes:
[docs/configuration.md](docs/configuration.md).

### Editor integration (ACP)

`tack acp` speaks ACP over stdio. Zed configuration:

```jsonc
// ~/.config/zed/settings.json
{
  "agent_servers": {
    "tack": {
      "type": "custom",
      "command": "C:/path/to/tack.exe",
      "args": ["acp"],
      "env": { "ANTHROPIC_API_KEY": "..." }
    }
  }
}
```

Mode (ask/acceptEdits/plan/bypass), model, and thinking level are selectable
in the client's UI. For JetBrains IDEs see
[docs/intellij-idea-acp.md](docs/intellij-idea-acp.md).

### Remote sessions & browser client

```bash
tack serve --listen ws:127.0.0.1:7749   # host sessions (+ embedded web UI)
# open http://127.0.0.1:7749/ in a browser
tack client --addr tcp:127.0.0.1:7749   # or attach from another terminal
```

`serve` speaks framed CBOR, wire-compatible with `@earendil-works/pi-protocol`
v1, over `tcp:` / `unix:` / `ws:` (WebSocket payload is byte-identical, minus
the length prefix), with `--tls` (self-signed pair generated on first use) and
`--auth-token`. The embedded web client is a single zero-dependency HTML file:
create/attach/switch sessions, stream replies, answer permission prompts.

## Using Tack

### Interactive TUI

- **Editor**: multi-line, Ctrl+R history search, undo, bracketed paste,
  `/command` + `@file` autocomplete (gitignore-aware), external editor (Ctrl+G),
  clipboard image paste (Ctrl+V).
- **Chat**: streaming markdown with syntax highlighting, thinking blocks,
  tool cards with colored diffs and streaming bash output (Ctrl+O to expand).
- **Permissions**: ask / acceptEdits / plan / bypass (Shift+Tab cycles),
  allow-once/always prompts with per-tool+input cache; project trust gating
  for `.pi/` resources (`/trust`).
- **Commands**: `/model /thinking /mode /compact /new /resume /tree /fork
  /clone /name /session /rules /todo /context /copy /export /share /import
  /login /logout /settings /fullscreen /reload /quit`, plus prompt templates
  as `/name` commands.
- **Rendering**: main-screen scrollback (default) or fullscreen alt-screen
  (`/fullscreen`) with mouse scroll, drag-to-copy, transcript search
  (Ctrl+Shift+F); inline images (Kitty/iTerm2), 14 built-in themes + user
  themes, light/dark pairs that follow the terminal background.

### Other subcommands

```bash
tack rpc          # JSONL RPC on stdio (wire-compatible with pi --mode rpc)
tack mcp-serve    # expose tack itself as an MCP server over stdio
tack stats        # cross-session token/cost report (--since 7d --json)
tack fork <session.jsonl>   # fork a session into this directory
tack compact      # manually compact the most recent session
tack doctor       # environment self-check (shell, LSP, sandbox, credentials, MCP)
tack logs         # structured trace viewer (--follow, --level, --target)
tack eval <dir>   # run eval tasks headlessly and score pass rates
tack ext ...      # install/list/remove/verify extensions, marketplaces
```

The RPC command surface has full TS parity (34 commands, `prompt`/`steer`/
`follow_up`/`abort`/`set_model`/`get_state`/`export_html`/…); see
[docs/compatibility.md](docs/compatibility.md) §2.3.

### Sessions

Stored under `~/.tack/agent/sessions/--<encoded-cwd>--/` in the transactional
v4 format (aligned with upstream TS pi; v1–v3 files migrate transparently on
open, `sessionBackend: "v3"` keeps the old write path). Tree-structured history
with branch summaries on `/tree` and `/fork` jumps.

## Providers & authentication

- **Selection**: `tack --provider <id> [--model <id>]` — provider ids match
  TS pi exactly (no aliases); an omitted model defaults to the provider's
  flagship. Registry: anthropic, openai, openai-codex, azure, google, mistral,
  deepseek, openrouter, xai, groq, cerebras, together, fireworks, baseten,
  huggingface, nvidia, zai(-coding-cn), moonshotai(-cn), kimi-coding,
  minimax(-cn), xiaomi, qwen, opencode(-go), vercel-ai-gateway, cloudflare,
  github-copilot, ant-ling, radius, amazon-bedrock, google-vertex, …
- **API keys** resolve in order: `--api-key` → `models.json` `apiKey` → the
  provider's env vars (same names as TS pi, e.g. `ZAI_API_KEY`,
  `MOONSHOT_API_KEY`, `KIMI_API_KEY`).
- **OAuth**: `tack login --provider <id>` runs the provider's flow when no
  `--api-key` is given — anthropic (Claude Pro/Max), openai-codex (ChatGPT),
  github-copilot, openrouter, kimi-coding, xai, radius. Headless machines can
  paste the redirect code or use `--device-code`. Expired tokens are refreshed
  proactively and persisted back to `auth.json`; `tack auth-status` shows
  type and expiry.
- **Local, zero config**: `ollama` (`OLLAMA_HOST`) and `llama.cpp`
  (`LLAMA_CPP_HOST`) are probed at startup; discovered models are injected
  into the catalog, so `tack --provider ollama "…"` just works when a server
  is up.
- **Custom providers**: `~/.tack/agent/models.json`, same schema as TS pi
  (`providers.<id>.{baseUrl, api, apiKey, headers, compat, models[]}`), with
  `$ENV_VAR` interpolation and `!command` execution in `apiKey`.
- **Bedrock / Vertex**: credential chains, SigV4, and ADC details live in
  [docs/providers.md](docs/providers.md).

## MCP servers

Built-in MCP client (via the `rmcp` SDK) — unlike TS pi, which leaves MCP to
extensions:

```jsonc
// ~/.tack/agent/mcp.json (global) or <project>/.pi/mcp.json (project wins)
{
  "mcpServers": {
    "everything": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-everything"] },
    "remote":     { "type": "http", "url": "http://localhost:3000/mcp",
                    "headers": { "authorization": "Bearer …" } }
  }
}
```

Stdio / Streamable HTTP / legacy SSE transports; tools appear as
`mcp__<server>__<tool>` through the normal permission pipeline; resources and
prompts are browsable via `/mcp`; remote servers support OAuth 2.1
(`"oauth": true`); lazy tool schemas (`mcpDeferThreshold`), server-initiated
sampling (`mcpSampling`, default off) and elicitation (`mcpElicitation`) are
available with hardened defaults. Details:
[docs/features.md](docs/features.md) §生态互操作 and
[docs/configuration.md](docs/configuration.md).

## Extensions

Dynamic extensions run as **subprocess plugins** (`tack-ext`): any executable
speaking newline-delimited JSON over stdio, one crash-isolated process per
plugin — or as sandboxed WASI modules (`tack-ext-wasm`).

```bash
# install from a git URL / local dir / marketplace spec
tack ext install https://github.com/example/plugin.git
tack ext list
```

…or drop a directory with an `extension.json` manifest into
`~/.tack/agent/extensions/<name>/`:

```json
{ "name": "hello-js", "command": "node", "args": ["plugin.js"] }
```

Plugins can register tools (`ext__<plugin>__<tool>`), slash commands, event
handlers, UI dialogs/notifications, session control, and runtime providers.
Ready-made examples under [`examples/extensions/`](examples/extensions/)
(Node.js hello-world, protected-paths guard, git checkpoints, session
handoff). **Full guide: [docs/extensions.md](docs/extensions.md)** (protocol,
API reference, security model) and [docs/extensions-v2.md](docs/extensions-v2.md)
(WASM carrier).

## Configuration

Four layers — managed (org) → global (`~/.tack/agent/settings.json`) →
project (`<project>/.pi/settings.json`, trust-gated) — deep-merged, with
special rules for `features.*`, `sandbox`, and `permissions.*`.

```jsonc
// ~/.tack/agent/settings.json
{
  "defaultProvider": "anthropic",
  "defaultModel": "claude-sonnet-4-5",
  "theme": "catppuccin-latte/catppuccin-mocha",
  "tokenBudget": 1000000,
  "features": { "sandbox": true, "cron": false }
}
```

`features.*` switches make a disabled feature **invisible to the agent** (no
tool schemas, no system-prompt snippets, subsystem never starts). Every
settings key, environment variable, and CLI flag, plus the precedence rules:
**[docs/configuration.md](docs/configuration.md)** — resource loading order
(rules/skills/MCP/themes): **[docs/directories.md](docs/directories.md)**.

## Documentation

Contributor-facing entry points are English; in-depth design docs are mostly
Chinese (language policy: [CONTRIBUTING.md](CONTRIBUTING.md)).

| Doc | Contents |
|---|---|
| **[docs/features.md](docs/features.md)** | Everything Tack adds over TS pi — how to use each feature and how to turn it off |
| **[docs/configuration.md](docs/configuration.md)** | Configuration reference: every settings key, env var, CLI flag, precedence |
| **[docs/onboarding.md](docs/onboarding.md)** | New-developer onboarding: source reading route, feature→code lookup, mermaid diagrams |
| [docs/architecture.md](docs/architecture.md) | Internal architecture (crate layering, streaming model, hooks, renderers) |
| [docs/providers.md](docs/providers.md) | Provider auth deep dives (Bedrock SigV4, Vertex ADC), custom provider schema |
| [docs/directories.md](docs/directories.md) | Directory & resource loading order |
| [docs/compatibility.md](docs/compatibility.md) | Compatibility & versioning policy for every interface |
| [docs/extensions.md](docs/extensions.md) / [docs/extensions-v2.md](docs/extensions-v2.md) | Extension development (process protocol; WASM carrier) |
| [docs/plugin-system.md](docs/plugin-system.md) | Plugin system overview (hooks / tack-ext / WASM / bundles / marketplace) |
| [docs/hooks.md](docs/hooks.md) | Lifecycle hooks (Claude Code compatible) |
| [docs/codebuddy.md](docs/codebuddy.md) | CodeBuddy guide (install/login, `/model`, troubleshooting) |
| [docs/intellij-idea-acp.md](docs/intellij-idea-acp.md) | JetBrains IDE setup via ACP |
| [docs/telemetry.md](docs/telemetry.md) | Why there is no telemetry, and what exists instead |
| [docs/upstream-alignment.md](docs/upstream-alignment.md) | TS pi → Tack sync tracking (weekly automated delta reports) |
| [docs/release.md](docs/release.md) | Release process (tags, cross-platform builds, self-update) |

## Development

```bash
cargo build --release -p tack-app   # build the CLI
cargo test --workspace            # full test suite, no network needed
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Repository layout

| Crate | Role |
|---|---|
| `tack-ai` | Unified message model + provider adapters (Anthropic, OpenAI, Azure, Codex, Google, Mistral, Bedrock, …) |
| `tack-agent-core` | Agent loop, `AgentTool`/`AgentHooks`/`Extension` traits, `AgentEvent` stream |
| `tack-session` | Session persistence (tree structure) + context compaction |
| `tack-tools` | Built-in tools: read, bash, git, edit, write, grep, find, ls, web_fetch, web_search, todo |
| `tack-tui` | Terminal UI library: styled lines, components, diff + alt-screen renderers |
| `tack-protocol` | CBOR remote-session protocol: schemas, framing, `RemoteClient` |
| `tack-ext` / `tack-ext-wasm` | Extension hosts: subprocess NDJSON protocol; sandboxed WASM carrier |
| `tack-app` | The binary: TUI / print / ACP / RPC / serve / mcp-serve, skills, settings, system prompt |

Dependency direction is acyclic: `tack-ai ← tack-agent-core ← tack-tools`,
`tack-ai ← tack-session`, `tack-app → all`.

### Design invariants

- **Streaming**: `EventStream<T, R>` (tokio mpsc + oneshot result); provider
  errors are always **in-band** (`Error` event + `stop_reason: Error`), never
  thrown.
- **Tools**: `serde_json::Value` parameter boundary + `schemars` schemas;
  validation failures become in-band error tool results; file mutations
  serialize on a shared lock.
- **Hooks**: a single `AgentHooks` trait object (transform_context /
  before_tool_call / after_tool_call / steering / follow-ups).
- **Cancellation**: one `CancellationToken` per prompt turn; bash kills the
  whole process tree.
- **Windows**: bash resolves Git Bash like pi's `utils/shell.ts`; an optional
  `powershell` tool is registered when listed in `defaultTools`.

New here? Start with [docs/onboarding.md](docs/onboarding.md) (guided source
tour), then [docs/architecture.md](docs/architecture.md), and read
[CONTRIBUTING.md](CONTRIBUTING.md) before opening a PR (conventional commits,
CHANGELOG entries, CI gates).

### Known gaps vs TS pi

- Extension-provided TUI widgets/dialogs (plugins cover tools, commands,
  dialogs, events, session control, provider registration — custom render
  components need the declarative v2.1/v2.2 protocol, still open), easter
  eggs, npm package manager.
- TS-module extensions run only through tack-ext's protocol (no drop-in TS
  loader).
- Deferred by decision: live multi-client collaboration; Windows sandbox is
  resource containment only (no fs isolation without AppContainer).

## Changelog, contributing, license

- Releases and per-version changes: [CHANGELOG.md](CHANGELOG.md)
- Contributing: [CONTRIBUTING.md](CONTRIBUTING.md) · Security: [SECURITY.md](SECURITY.md)
- Licensed under [Apache-2.0](LICENSE). Tack is a Rust reimplementation of the
  [pi](https://github.com/earendil-works/pi) coding agent; pi itself is
  [MIT-licensed](https://github.com/earendil-works/pi/blob/main/LICENSE) © 2025
  Mario Zechner.
