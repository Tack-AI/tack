# Tack Configuration Reference

**English | [简体中文](configuration.zh-CN.md)**

Every settings.json key, environment variable, CLI flag, and on-disk file —
with types, defaults, and precedence.

## Configuration tiers & precedence

Tack reads configuration from four tiers; **later tiers override earlier
ones** (some keys have special rules, see below):

| Tier | Location | Notes |
|---|---|---|
| **Managed** (org-enforced) | Linux `/etc/tack/managed-settings.json` · macOS `/Library/Application Support/tack/managed-settings.json` · Windows `%ProgramData%\tack\managed-settings.json` (path overridable via `TACK_MANAGED_SETTINGS` in development/test builds only — release builds ignore the variable) | Highest priority. Applied last in the deep merge. |
| **Global** (user) | `~/.tack/agent/settings.json` | User defaults. |
| **Project** | `<project>/.pi/settings.json` | Loaded only when the project is trusted (`/trust`). |

Special merge rules (**not** simple last-wins):

- **`features.*`**: project can only **disable**, never enable (AND merge);
  managed can force either direction.
- **`sandbox`**: global may enable; project can only explicitly disable;
  managed both directions.
- **`permissions.allow/deny`**: **UNION merge** across the three tiers —
  rules added at any tier take effect; an org's deny list cannot be
  discarded.
- **`hooks.*`**: deep-merged (project hooks for the same event override the
  global list).

## settings.json full key reference

### Model & provider

| Key | Type | Default | Notes |
|---|---|---|---|
| `defaultProvider` | string | `"anthropic"` | Default provider |
| `defaultModel` | string | provider default | Default model id |
| `scopedModels` | string[] | `[]` | Model scope for ctrl+p cycling ("provider/id") |
| `fallbackModels` | string[] | `[]` | Fallback chain (ordered "provider/id"). Automatically degrades on 429/overloaded/5xx/timeout after the primary model's retries are exhausted |
| `transport` | string | `"auto"` | Codex transport: `auto` (WS first, SSE fallback) \| `sse` \| `websocket` |
| `modelCatalogRefresh` | bool | `false` | Pull the latest model catalog from `@earendil-works/pi-ai` on npm at startup (background, failures only logged). Manual `/models refresh` is not gated by this switch; an already-fetched catalog cache is always loaded |
| `backgroundAutoWake` | bool | `true` | Background task finishes while the agent is idle: automatically starts a turn so the agent reads the output and keeps working (same mode as cron triggers). Set to `false` to only post a transcript notification without auto-waking |
| `httpIdleTimeoutMs` | number | — | HTTP read idle timeout |
| `retry` | object | `{enabled:true, maxRetries:3, baseDelayMs:2000, maxAgentDelayMs:60000}` | Automatic retry of transient errors (`maxAgentDelayMs` is the per-backoff cap) |
| `compaction` | object | `{enabled:true, reserveTokens:…, keepRecentTokens:…, goalRecitation:true, minKeptTurns:2}` | Automatic context compaction. `goalRecitation`: after compaction, recite the summary's Goal/Next Steps back into context as a trailing user message (LLM-side copy only, append-only so the cache is not broken); `minKeptTurns`: minimum turns the compaction cut point must keep |
| `microcompact` | object | `{enabled:true, maxChars:20000, keepRecent:3, minSavingsChars:8000}` | In-place trimming of old overlong tool results (full text persisted to disk for later re-reads). `minSavingsChars`: if total reclaimed bytes are below this, history is not rewritten (protects the prompt-cache prefix). `enabled:false` also disables the three history optimizations below |
| `maskDuplicateReads` | bool | `true` | When the same file is read twice with byte-identical content, replaces the earlier result with a pointer (the newer copy stays in context; zero information loss) |
| `toolResultMaxChars` | number | `60000` | Hard cap for any tool result (including the most recent turns); excess is truncated and the full text persisted to `<id>.full.log`; `0` disables |
| `rulesMaxChars` | number | `40000` | System-prompt budget for a single AGENTS.md/CLAUDE.md context file; excess is truncated and annotated with the on-disk path; `0` means unlimited |

### Budgets

| Key | Type | Default | Notes |
|---|---|---|---|
| `tokenBudget` | number | — | Total session token budget |
| `tokenBudgetAction` | string | `"warn"` | Over-budget action: `warn` (TUI warning) \| `pause` (stop the run at a turn boundary) \| `downgrade` (switch to a cheaper model) |
| `budgetDowngradeModel` | string | last entry of fallbackModels | Downgrade target model ("provider/id") |
| `subagents.maxConcurrent` | number | `0` (unlimited) | Subagent concurrency cap: shared semaphore over simultaneously running subagent loops; excess waits in queue |
| `subagents.budgetTokens` | number | `0` (unlimited) | Shared subagent token budget: cap on cumulative usage of all subagents this session (including background ones); new subagent calls are rejected past the limit (subagents don't land in the session file, so budget hooks can't see their usage — hence the separate control) |
| `subagents.inheritPlugins` | string | `"hooks"` | Which plugin surfaces subagent child loops inherit: `none` (built-in tools + deny rules only) \| `hooks` (plugin hook bridges — beforeToolCall interception, context transform, result patching — follow the child, closing the guardrail bypass) \| `full` (hooks + plugin tools; agent-definition `tools` whitelists still narrow the combined set). Layered like any settings key, so managed settings can pin it |

### Feature flags (features.*)

All default to **true** (including `sandbox`; no explicit opt-in needed
anymore). A disabled feature is completely invisible to the agent: tools
not registered, zero trace in the system prompt, subsystem not started.

| Key | Controls |
|---|---|
| `features.lsp` | lsp tool (diagnostics/definition/references/implementation/symbols/rename/hover/code_actions/workspace_symbols/incoming_calls/outgoing_calls) + edit/write diagnostic feedback + language server processes |
| `features.checkpoints` | per-turn file snapshots + git baseline + `/checkpoints` rollback |
| `features.backgroundTasks` | bash `run_in_background` + `bash_output`/`bash_wait`/`kill_shell` + background subagents |
| `features.memory` | `memory` tool + MEMORY.md system-prompt injection (project/user dual scope) + `/memory` |
| `features.shellHooks` | shell commands in `hooks.*` (not executed even if configured) |
| `features.cron` | scheduled tasks (`/cron` + triggers) |
| `features.sandbox` | OS sandbox (equivalent to the `sandbox` key) |

Legacy key mapping: `lspDisabled: true` ≡ `features.lsp: false`;
`checkpointsDisabled: true` ≡ `features.checkpoints: false` (new keys win).

### LSP

| Key | Type | Default | Notes |
|---|---|---|---|
| `lspServers` | object | built-in table | Extension → server override: `{"rs": "rust-analyzer", "ts": {"command": "typescript-language-server", "args": ["--stdio"]}}` |
| `lspEditFeedback` | bool | `true` | Attach a diagnostics summary to edit/write results |

Built-in server probing: `rs`→rust-analyzer, `ts/tsx/js/jsx/mts/cts/mjs/cjs`→typescript-language-server --stdio, `py/pyi`→pyright-langserver --stdio, `go`→gopls, `c/h/cpp/cc/cxx/hpp/hh`→clangd. Extensions whose binary is not on PATH are silently disabled (visible in `tack doctor`).

### Permissions (permissions.*)

```json
"permissions": {
  "allow": ["Bash(npm run *)", "Edit(src/**)", "WebFetch"],
  "deny": ["Bash(rm -rf *)", "Edit(**/.env)", "Edit(**/secrets/**)"]
}
```

- Syntax: `Tool(pattern)` or a bare `Tool` (all invocations of that tool).
- bash commands: `*` wildcards (anchored at both ends); file paths: glob
  (`**` crosses directories).
- **deny always wins**, and applies in headless modes (print/rpc/serve/CI)
  too.
- "always" answers in the TUI persist to the `allowAlways` array in
  `~/.tack/agent/permissions.json` and survive restarts. For plugin tools
  (`ext__*`) the loaded plugin version is recorded alongside
  (`extToolVersions` map) and the entry only applies while that version is
  still loaded — `tack ext upgrade` invalidates stale approvals.
- The untrusted-content defense (see the features doc) temporarily bypasses
  allow rules to force a prompt.

### Sandbox

| Key | Type | Default | Notes |
|---|---|---|---|
| `sandbox` | string/bool | `"on"` | `"off"`/`false` disables the OS sandbox (on by default: bash writes restricted to the workspace) |
| `sandboxNetwork` | bool | `true` | Allow network inside the sandbox |
| `sandboxMaxProcesses` | number | — | Windows Job Object process count limit |
| `sandboxMaxMemoryMb` | number | — | Windows Job Object memory limit (MB) |

Backends: Linux bubblewrap (read-only system + writable workspace), macOS
seatbelt, Windows Job Objects (process-tree kill + resource limits, not
filesystem isolation). Platforms without a backend degrade with a one-time
warning.

**Writable set** = main cwd + every directory from
`additionalDirs`/`--add-dir` + built-in allowances (macOS/Linux: `/tmp`,
`/private/tmp`, `/private/var/folders`, `/dev`). Writes outside the set
fail with EPERM inside the sandbox; the output includes a hint listing the
current writable roots.

**CARGO_HOME inside the sandbox**: the default `~/.cargo` is usually not in
the writable set, so any sandboxed cargo command that writes to the
registry fails with EPERM. The executor therefore auto-falls back — when
`CARGO_HOME` is not explicitly set and `~/.cargo` is not writable, it
injects `CARGO_HOME=$TMPDIR/tack-cargo-home` (writable under every backend;
after the OS cleans it, cargo re-fetches automatically). To make sandboxed
cargo use the **real** `~/.cargo` (sharing the cache with cargo in your
terminal), add its **absolute path** to `additionalDirs` (settings parsing
does not expand `~`):

```json
{ "additionalDirs": ["/home/you/.cargo"] }
```

Once `~/.cargo` is covered by the writable set, the auto-fallback no longer
triggers and cargo uses the default path directly. Note: settings load at
startup, so changes require a **restart**; `additionalDirs` is also the
multi-root mechanism (directories appear in the system prompt, their
AGENTS.md is loaded, they join the LSP roots), and the sandbox's write
protection for that directory disappears along with it — that is the
point, but it means the agent's bash can modify it arbitrarily. Merely
`export CARGO_HOME=~/.cargo` **does not work**: explicit values take
precedence over the auto-fallback, and the directory still EPERMs when not
writable.

### Hooks (hooks.*)

**Claude Code compatible** lifecycle hooks (command / LLM-eval handlers, 12
events, `updatedInput` argument rewriting, `permissionDecision` permission
verdicts, `additionalContext` injection, managed hooks). Full protocol:
[hooks.md](hooks.md).

```json
"hooks": {
  "PreToolUse": [{"matcher": "bash", "hooks": [{"type": "command", "command": "check.sh"}]}],
  "SessionStart": [{"command": "cat .pi/context.md"}]
}
```

Related setting: `managedHooksOnly: true` runs only
`~/.tack/agent/managed-hooks.json`.

### Credentials & security

| Key | Type | Default | Notes |
|---|---|---|---|
| `credentialStore` | string | `"auto"` | Credential store: `auto` (keyring if available, else file) \| `keyring` (force OS credential store) \| `file` (auth.json plaintext) |
| `defaultProjectTrust` | string | `"ask"` | Project trust: `ask` \| `always` \| `never` |

### Observability

| Key | Type | Default | Notes |
|---|---|---|---|
| `observability.enabled` | bool | `false` | Structured JSONL trace to `~/.tack/agent/logs/` |
| `observability.level` | string | `"info"` | File log level |

Env vars `TACK_TRACE_FILE=1` / `TACK_TRACE_LEVEL=debug` take precedence.
Automatic redaction before writing to disk (token/authorization/Bearer/URL
sensitive params).

### Web

| Key | Type | Default | Notes |
|---|---|---|---|
| `webRender` | string | `"auto"` | Headless rendering: `auto` (auto-fallback on JS shell pages) \| `always` \| `off` |
| `webSearch.provider` | string | `"bing"` | Search backend: `bing` \| `duckduckgo` \| `brave` \| `tavily` \| `exa`. The two keyless scrapes (`bing`/`duckduckgo`) fall back to each other on failure |
| `webSearch.apiKey` | string | — | Backend API key (or use the `BRAVE_API_KEY`/`TAVILY_API_KEY`/`EXA_API_KEY` env vars) |

### Session & history

| Key | Type | Default | Notes |
|---|---|---|---|
| `sessionBackend` | string | `"v4"` | Session storage: `v4` (transaction log; transparently migrates old v3/v2/v1 sessions on open, keeping a .bak) \| `v3` (legacy JSONL, byte-compatible with old TS pi; refuses to open v4 files) \| `sqlite` (experimental; cross-session search already covers sqlite sessions) |
| `sessionEncryption` | bool | `false` | Session at-rest encryption: entry lines encrypted with AES-256-GCM on disk (key stored in the OS credential store, auto-generated on first use) |
| `steeringMode` | string | `"all"` | Steering delivery: `all` \| `one-at-a-time` |
| `followUpMode` | string | — | Follow-up delivery: same as above |
| `doubleEscapeAction` | string | `"tree"` | Double Esc in an empty editor: `tree` \| `fork` \| `none` |
| `treeFilterMode` | string | `"default"` | /tree filtering: `default` \| `no-tools` \| `user-only` \| `labeled-only` \| `all` |
| `additionalDirs` | string[] | `[]` | Multi-workspace: extra working directories (their AGENTS.md is loaded, merged into the sandbox writable set — usable to allow `~/.cargo` etc., see the Sandbox section; must be absolute paths) |

### TUI appearance & behavior

| Key | Type | Default | Notes |
|---|---|---|---|
| `theme` | string | terminal detection | Theme name (built-in dark/light + catppuccin-mocha/-latte, tokyo-night, gruvbox-dark/-light, nord, dracula, one-dark, solarized-dark/-light, kanagawa, monokai, rose-pine/-dawn; plus custom themes/ directories; supports `"light/dark"` auto-pairing, e.g. `"catppuccin-latte/catppuccin-mocha"`) |
| `tuiMode` | string | `"regular"` | `regular` \| `fullscreen` (alt-screen) |
| `language` | string | LANG env var | UI language: `en` \| `zh` — full TUI copy (slash-command help, dialogs, notifications/errors, status bar, footer) covered in both languages; log/debug output, command names, and protocol values are not translated |
| `mermaid` | string | `"image"` | mermaid rendering: `image` \| `off` (requires the `mermaid` cargo feature at compile time, off by default; without it code blocks render as plain code) |
| `terminal.showImages` | bool | `true` | Inline images |
| `terminal.imageWidthCells` | number | — | Image width (cells) |
| `terminal.clearOnShrink` | bool | `false` | Clear screen when the terminal shrinks |
| `terminal.hyperlinks` | bool \| `"auto"` | `"auto"` | OSC 8 hyperlink capability override; `auto` keeps probing (takes precedence over `TACK_HYPERLINKS`) |
| `terminal.images` | `"kitty"` \| `"iterm2"` \| `false` \| `"auto"` | `"auto"` | Inline image protocol capability override (takes precedence over `TACK_IMAGE_PROTOCOL`) |
| `terminal.trueColor` | bool \| `"auto"` | `"auto"` | 24-bit color capability override (takes precedence over `TACK_TRUE_COLOR`) |
| `fullscreenCopyOnSelect` | bool | `true` | Drag-select in fullscreen auto-copies via OSC 52; with `false` the selection stays highlighted and `Ctrl+X` copies it (or the last assistant message if nothing is selected) |
| `images.blockImages` | bool | `false` | Don't send images to the LLM |
| `imageProtocol` | string | auto-detect | Image protocol: `kitty` \| `iterm2` \| `half-block` |
| `editorPaddingX` | number (0-3) | `0` | Editor left/right padding |
| `markdown.codeBlockIndent` | string | — | Extra code block indent |
| `autocompleteMaxVisible` | number (3-20) | — | Visible entries in the completion list |
| `fullscreenExitOutput` | string | — | Print the session transcript on fullscreen exit with `"transcript"` |
| `externalEditorCommand` | string | `$EDITOR` | External editor command |
| `hideThinkingBlock` | bool | `false` | Hide thinking blocks |
| `collapseChangelog` | bool | `false` | Show only one line of the startup changelog |
| `quietStartup` | bool | `false` | Skip banner + changelog |
| `showCacheMissNotices` | bool | `false` | Prompt-cache miss notifications |
| `cacheRetention` | `"short"` \| `"long"` \| `"off"` | `"short"` | Prompt cache retention: `short`=5-minute writes (provider default; Kimi Messages sends an explicit `ttl:"5m"`); `long`=1-hour writes (24h for OpenAI, higher write fees; Kimi/Anthropic protocols send `ttl:"1h"`, Moonshot OpenAI protocol sends `prompt_cache_options`); `off`=no cache markers sent (read-only where supported). When unset, falls back to the `TACK_CACHE_RETENTION` env var (`long` takes effect), then to `short`. Note: Kimi's cache TTL locks in after the first write; switching mid-flight takes effect only after old entries expire |
| `enableSkillCommands` | bool | `true` | `/skill:<name>` completion |
| `defaultTools` | string[] | `[]` (all) | Built-in tool allowlist (intersected with features.*). The optional `powershell` tool (Windows) is off by default and registered only when explicitly listed here — e.g. `["read", "powershell", "edit", "write"]` replaces bash, or list both. Prefers `pwsh.exe`, falls back to `powershell.exe`, launched with `-NoProfile -NonInteractive -ExecutionPolicy Bypass`; registering it on non-Windows platforms errors at execution. Permission rules are written `PowerShell(...)` with the same wildcard semantics as `Bash(...)` |
| `updateRepo` | string | `"Tack-AI/tack"` | Self-update GitHub repo |
| `updateCheck` | bool | `true` | Background new-version check at TUI startup (result cached in `update-check.json`, 24h TTL); update hints appear in the footer and chat area. Skipped under `--offline`/`TACK_OFFLINE`; checks only, never installs |
| `notifications` | bool | `true` | Desktop notifications (OSC 9 / OSC 777 escape): permission popups, agent run completion/errors, background task completion (5s throttle per event source). Silently inert if the terminal doesn't support them |
| `shellPath` | string | auto-detect | Shell path override for the bash tool (probes Git Bash → `where bash` by default, Windows) |
| `memoryDirectory` | string | `~/.tack/agent/memory` | Root directory of the persistent-memory user scope (project scope lives under it at `projects/<repo>/`); `TACK_MEMORY_DIR` env var takes precedence |
| `appendSystemPrompt` | string | — | Text appended to the end of the system prompt (equivalent to the `--append-system-prompt` flag) |

### Extension resource directories

| Key | Notes |
|---|---|
| `skills` | Extra skills directories (string[]) |
| `prompts` | Extra prompt template directories |
| `themes` | Extra theme directories |

Also: `extensionLockRequired` (bool, default `true`) — extension supply-chain
locking: plugins installed into the user directory via `ext install` have
their git HEAD checked against the resolvedCommit in
`extensions-lock.json` at startup; on mismatch Tack warns and skips the
plugin; with `false` it only warns but still loads. Plugins without a lock
entry are unaffected (see docs/extensions.md §1c). The managed tier can
force this value either way.

Also: `plugins` (object) — per-plugin settings keyed by plugin id
(`name@source`, see docs/plugin-roadmap.md). Today the only per-plugin key
is `enabled` (bool, default `true`): a disabled plugin stays installed but
is never spawned (its metadata still shows up in `ext list`). Manage it
with `tack ext enable|disable <id>` or edit the file directly.

Also: `pluginMarketplaces` (object) — curated marketplace catalogs kept
fresh by the background startup sync. Keys are marketplace names; values
are a source string or an object `{"source", "ref"?, "path"?,
"publicKey"?}` (git repo URL, https `.json` catalog, or local file/dir).
Read from the **global and managed layers only** (a catalog can push code
via `installed-by-default`, so a project layer must not redirect it —
same rule as `updateRepo`). See docs/extensions.md §5.2.

### Managed-tier-only keys

Effective only in managed-settings.json:

| Key | Notes |
|---|---|
| `disableBypass` | Disable bypass permission mode (skipped in the /mode cycle + hook-layer fallback downgrade) |
| `lockedProvider` | Lock the provider (blocked at startup and in /model) |
| `lockedModel` | Lock the model id |
| `auditSink` | Audit reporting: `{"url": "…", "token": "…", "intervalMs": 5000}` — batch POSTs of trace events (newline-delimited JSON, Bearer auth). Setting this force-enables observability |
| `pluginPolicy` | Enterprise plugin policy: `managedPluginsOnly`, `allowedSources` (git/hostPattern/local source allow-list), per-plugin `enabled` (wins over user/project), narrow-only `tools`/`mcpServers` intersections, the `provider` bridge-serving gate (`false` policy-blocks a provider-stream plugin at load), and the `hooks` capability gate (`false` strips the plugin's hook bridges at registration — no tool-call interception — while its tools/commands still load). Enforced at install time and at load time; decisions are audit-logged. See docs/extensions.md §9 |

### MCP (mcp.json)

Each server entry, besides `command`/`args`/`env` (stdio) or
`url`/`headers` (HTTP):

| Key | Notes |
|---|---|
| `enabled` | `false` keeps the entry listed (status surfaces show "disabled") but never connects to it. Default `true` |
| `timeout` | Per-request timeout in seconds for tool/resource/prompt calls (default 60; `0` disables). Progress notifications reset the clock — a long call that streams progress is not stuck |
| `exposure` | How the server's tools reach the model: `"direct"` (default — declared like built-ins), `"deferred"` (NOT declared; `tool_search` loads matches on demand, the system prompt's "Deferred MCP tools" section names these servers), `"hidden"` (registered nowhere). pi's `"codemode"`/`"codemode-deferred"` are accepted and map to `"deferred"` (tack has no codemode tool) |
| `toolExposure` | Per-tool overrides of `exposure`: keys are exact server tool names or `*` patterns (e.g. `{"search_code": "direct", "delete_*": "hidden"}`). Exact names win; among patterns the longest (most specific) wins — a deliberate upgrade of pi's first-match rule (JSON object order is not preserved) |
| `oauth` | `true` or an object — OAuth 2.1 authorization for remote servers (PKCE + dynamic registration, tokens cached in `mcp-tokens.json` and auto-renewed). Object keys: `clientId`, `clientSecret` (for pre-registered clients), `scopes` (array), `callbackPort` (fixed loopback port for providers with a fixed redirect URI), `callbackUrl` (full redirect URI override; must be plain HTTP on `localhost`/`127.0.0.1`/`[::1]`) |

`env` and `headers` values (and `oauth.clientSecret`) interpolate
`${VAR}` from the process environment — the syntax other MCP clients
share; an unset variable expands to empty with a warning.

Servers are managed from the shell without a session: `tack mcp list`
(connects every enabled server, prints state/tools/errors, exits 1 when
an entry is invalid or an enabled server is not connected), `tack mcp
add <name> [--local] [--env K=V]... [--header K=V]... [--url URL
[--transport sse] [--bearer-token-env-var VAR]] [-- command args...]`,
`tack mcp remove <name> [--local]`, `tack mcp login <name>` (interactive
OAuth) and `tack mcp logout <name>` (drops the cached token).

Connection resilience: a tool call that finds its connection closed
reconnects (re-spawning stdio children / re-dialing HTTP) before
failing, and `tools|resources|prompts/list_changed` notifications
refresh the cached capability lists in place. Server-declared tool
annotations (`readOnlyHint` etc.) feed the permission system — a
read-only MCP tool skips prompts like the built-in read tools (a
contradictory `destructiveHint` wins) — and are passed to approval-chain
plugins as evidence.

settings.json also has `mcpDeferThreshold` (number, default 0=off): when the
total tool count exceeds the threshold, MCP tools are lazily loaded and the
agent activates them on demand via `tool_search` (see the features doc).
Configuring `exposure` on ANY server opts out of that blanket rule so
per-server choices stay predictable.

Two more MCP switches in settings.json:

| Key | Notes |
|---|---|
| `mcpSampling` | boolean, default `false`. When `true`, MCP servers may request LLM completions in reverse (`sampling/createMessage`): executed in an isolated context with the current session's provider/model (server-provided messages are treated as `<untrusted_content>` and never enter the main session; tools/toolChoice and audio are refused), usage counts toward the session and is logged. The capability is not advertised when off. |
| `mcpElicitation` | boolean, default `true`. MCP servers may ask the user for structured input (`elicitation/create`): the TUI pops a per-field text input (string/number/integer/boolean/enum conversion per schema, Esc cancels); `tack serve` forwards the form to dialog-capable remote clients and declines when none is connected; print/rpc/acp auto-decline; URL mode always declines. |

## Environment variables

| Variable | Notes |
|---|---|
| `TACK_AGENT_DIR` | Agent directory override (default `~/.tack/agent`) |
| `TACK_MANAGED_SETTINGS` | Managed settings file path override (development/test builds only; ignored in release builds) |
| `TACK_TRACE_FILE` | =1 enables JSONL trace export |
| `TACK_TRACE_LEVEL` | Trace file level (default info) |
| `TACK_BROWSER` | Browser executable path for headless rendering |
| `TACK_REMOTE_TOKEN` | Shared token for serve/client (equivalent to --auth-token) |
| `TACK_PROVIDER` / `TACK_MODEL` | provider/model overrides for ACP mode (`--provider`/`--model` take precedence over them) |
| `TACK_THINKING` | Thinking level for ACP mode (`--thinking` takes precedence) |
| `TACK_IMAGE_PROTOCOL` | Force image protocol (kitty/iterm2/half-block) |
| `TACK_HYPERLINKS` | OSC 8 hyperlink capability override: `1` \| `0` \| `auto` (the `terminal.hyperlinks` setting takes precedence) |
| `TACK_IMAGE_PROTOCOL` | Image protocol capability override: `kitty` \| `iterm2` \| `none` \| `auto` (the `terminal.images` setting takes precedence) |
| `TACK_TRUE_COLOR` | 24-bit color capability override: `1` \| `0` \| `auto` (the `terminal.trueColor` setting takes precedence) |
| `TACK_UPDATE_REPO` | Self-update repo override |
| `TACK_MODEL_CATALOG_REGISTRY` | npm registry base URL for model catalog refresh (default registry.npmjs.org) |
| `TACK_OFFLINE` | Offline mode (disables fd/rg downloads, sharing, OAuth) |
| `TACK_CACHE_RETENTION` | Enables long cache writes (1h; 24h for OpenAI) when `long`; the `cacheRetention` setting takes precedence |
| `ANTHROPIC_BASE_URL` / `ANTHROPIC_MODEL` | Anthropic-compatible endpoint proxy |
| `CODEBUDDY_PATH` | codebuddy CLI path override (when not on PATH; accepts `.cmd`/`.bat`/`.py`). Usage details: [codebuddy.md](codebuddy.md) |
| `BRAVE_API_KEY` / `TAVILY_API_KEY` / `EXA_API_KEY` | Search backend keys |
| Standard per-provider keys | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, etc. (see the providers table) |
| `LANG` | TUI defaults to Chinese when `zh*` (the `language` setting takes precedence) |

## CLI flags

Main command (`tack [flags] [prompt | @file …]`):

| Flag | Notes |
|---|---|
| `-p, --print <PROMPT>` | Headless one-shot mode |
| `--provider / --model / --api-key` | Override provider/model/credential |
| `-c, --continue` / `-r, --resume` / `--session / --session-id / --fork` / `--name` / `--no-session` / `--session-dir` | Session control |
| `--models a,b,c` | ctrl+p cycling scope |
| `-t, --tools` / `--exclude-tools` / `--no-tools` / `--no-builtin-tools` | Tool filtering (intersected with features.*) |
| `--no-skills` / `--no-context-files` / `--no-prompt-templates` / `--no-themes` | Resource switches |
| `--skill <dir>` / `--add-dir <dir>` | Extra skills directory / extra working directory (repeatable) |
| `--mode text\|json\|rpc` | Output mode |
| `--tui-mode regular\|fullscreen` / `--use-theme <name>` | TUI overrides |
| `--thinking off\|minimal\|low\|medium\|high\|xhigh\|max` | Thinking level |
| `--append-system-prompt <text|path>` | Append to the system prompt (repeatable) |
| `--system-prompt` / `--prompt-template "name args"` | System prompt override / template expansion |
| `--export <path>` | Copy the session file on exit |
| `--verbose` / `--offline` | Debug logging / offline |
| `-a, --approve` / `--no-approve` | Project trust override |
| `--list-models [pattern]` | List models and exit |

Subcommands:

| Subcommand | Notes |
|---|---|
| `tack acp` | ACP server (Zed and other editors) |
| `tack rpc` | JSONL RPC (stdin commands / stdout events) |
| `tack mcp list` / `tack mcp add` / `tack mcp remove` / `tack mcp login` / `tack mcp logout` | Session-free MCP server management (see MCP (mcp.json) above) |
| `tack mcp-serve` | MCP server (exposes the agent to other MCP clients) |
| `tack serve` | Remote session host (CBOR; `--listen`, `--auth-token`/`--auth-token-file`, `--tls`/`--tls-cert`/`--tls-key`, `--allow-no-auth`). `--tls` applies to both `tcp:` and `ws:` listeners (the latter becoming wss). `--allow-no-auth` allows starting a non-loopback listener without a token (dangerous: anyone who can reach the port can execute commands on your machine; not needed for loopback/unix listeners) |
| `tack client` | Connect to serve (`--addr`, `--auth-token`, `--tls`/`--tls-ca`/`--tls-insecure`; `--addr ws:`/`wss:` uses WebSocket, `wss:` implies `--tls`) |
| `tack stats` | Cross-session usage report: token/cost estimates per provider×model, daily time series (`--since/--until/--json/--dir`) |
| `tack doctor` | Environment self-check (shell/git/LSP/sandbox/browser/credentials/MCP/fd+rg; `--json` machine-readable output, `--bundle [PATH]` packs a diagnostic tar.gz: report + redacted settings.json + crash.log, written to a timestamped file in the current directory by default) |
| `tack logs` | Trace viewing (`--tail/--level/--target/--follow`) |
| `tack eval <dir>` | Evals (`--runs/--filter/--report/--baseline`) |
| `tack login / logout / auth-status` | Credential management (OAuth flows or `--api-key`) |
| `tack fork <file>` / `tack compact` | Session forking / manual compaction |
| `tack ext install/list/remove` | Extension management |
| `tack update` | Self-update (`--check`/`--force`) |

## Agent directory on-disk files

```
~/.tack/agent/
  settings.json          # global config
  auth.json              # credentials (only a {"type":"keyring"} placeholder in keyring mode)
  models.json            # custom providers
  keybindings.json       # keybinding overrides
  mcp.json               # global MCP servers
  mcp-tokens.json        # MCP OAuth token cache (0600, contains refresh_token/client_id)
  permissions.json       # allow-always persistence ({"allowAlways": [...], "extToolVersions": {...}})
  cron.json              # scheduled tasks
  catalog.json           # model catalog cache pulled by refresh (deleted by /models reset)
  catalog.meta.json      # catalog origin and fetch time (version/providers/models/fetchedAt)
  update-check.json      # TUI startup update-check cache (checked_at/latest, 24h TTL)
  trust.json             # project trust decisions
  serve-cert.pem / serve-key.pem   # serve --tls self-signed certificate (auto-generated)
  AGENTS.md / rules/     # global rules
  skills/ prompts/ themes/ agents/ # resource directories (agents = custom subagents)
  memory/                # persistent memory (MEMORY.md index + one file per entry)
  sessions/--<cwd>--/*.jsonl       # sessions (or sessions.db in sqlite mode)
  checkpoints/<session-id>/turn-N/ # file snapshots (meta.json + blobs + state.json)
  plans/plan-<ts>.md     # plans persisted by plan mode
  logs/tack-<day>.jsonl # structured trace (observability)
  microcompact/<id>.log  # full text of trimmed overlong tool output
  microcompact/<id>.full.log  # full tool output before hard-cap truncation
  bin/                   # managed fd/rg
```

## Project directory

```
<project>/.pi/
  settings.json   # project config (requires trust; can only disable features, may add permissions/rules)
  mcp.json        # project MCP servers (requires trust)
  agents/         # project-level custom subagents (requires trust; same name overrides global)
  skills/ prompts/ themes/
  AGENTS.md       # project rules (ancestor scanning)
```
