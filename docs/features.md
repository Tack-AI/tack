# Tack Feature Guide

**English | [简体中文](features.zh-CN.md)**

A feature-by-feature guide to everything beyond TS pi: what it is, how to use
it, and how to turn it off (everything can be disabled via `features.*`
or a corresponding setting — see [configuration.md](configuration.md)).

## Agent Execution

### Live model catalog refresh

The embedded catalog is a build-time snapshot; new models have to wait for a
Tack release. `/models refresh` pulls the latest catalog from the
`@earendil-works/pi-ai` package published on npm (the `dist/providers/data/*.json`
files inside the tarball — the same source data as the embedded catalog),
converts it to Tack's format, applies it immediately, and caches it at
`~/.tack/agent/catalog.json` (loaded on every startup, including headless mode):

```
/models            # show the current catalog source (embedded/refreshed)
/models refresh    # refresh now (respects --offline / TACK_OFFLINE)
/models reset      # delete the cache and return to the embedded catalog (takes effect after restart)
```

With `"modelCatalogRefresh": true` the TUI refreshes automatically in the
background on every startup (non-blocking, failures are non-fatal); the
registry can be overridden with `TACK_MODEL_CATALOG_REGISTRY`. Truncation
guard included (results with <20 providers or <50 models are refused);
data files for unknown providers are skipped.

### Provider/model discovery (first-run onboarding)

New users don't know which providers exist, what models each provider has,
or how to configure keys:

- `tack providers`: full provider overview — auth status (✓/not configured),
  model count, how to enable (env var name or `tack login --provider <id>`);
  includes custom providers from models.json; ends with a summary of how many
  providers are ready
- `tack models [pattern]` (= `--list-models`): model catalog grouped by
  provider, provider headers carry auth markers; substring filter on id/name
- In-TUI `/providers`: the same provider overview; `/model` to browse and
  pick a model
- On TUI startup, if there are no credentials at all (models.json/env/cloud
  ambient chain included), a four-step guide is shown directly in the
  conversation (providers → login → env var → /model) instead of failing with
  "no API key" on the first submit
- The missing-key error in print mode likewise includes `tack providers` /
  `login` hints

### CodeBuddy native provider (`codebuddy/*`)

Integrates the `codebuddy` CLI as a first-class provider (tack-ai
`codebuddy-stream` adapter — no Node SDK, no local HTTP translation layer).
**Usage guide: [codebuddy.md](codebuddy.md)** (install, login, thinking
levels, session isolation, troubleshooting). Implementation highlights:

- **stream-json long-lived session**: spawns a long-lived process with
  `codebuddy -p --input-format stream-json --output-format stream-json`
  (`CODEBUDDY_PATH` or PATH probing), handshakes via `control_request
  initialize`; the model catalog is discovered from the handshake response
  and injected at startup (falls back to a single `default` model on
  failure); CLI-side auth (`codebuddy login`) is reused directly
- **Tool bridge (SDK MCP / parked tools/call)**: Tack's tools are exposed to
  the model through an **SDK MCP server** — after the `initialize` handshake
  declares `sdkMcpServers: ["tack"]`, the CLI runs MCP JSON-RPC
  (initialize/tools/list/tools/call) over `mcp_message` control frames, and
  Tack answers with `control_response` (`response.response.mcp_response`).
  SDK MCP tools are native CLI tools (not deferred like HTTP MCP), so with
  `--tools ""` disabling built-in tools the model still sees the
  `mcp__tack__*` schemas; a model tool_use ends the current LLM response and
  **parks** the CLI-side tools/call; Tack's agent loop executes the tool with
  its own permission/UI, and the next stream call resolves the parked MCP
  request so the CLI session continues natively — CLI-side cache is
  preserved, with no fragile prefix-hash continuation mechanism. Note the CLI
  reuses the same content index for **parallel** tool_use blocks and emits
  only one `content_block_stop` (Tack stops all active blocks by index);
  after a tool round the CLI replays the full assistant message in blocks
  (stale echo — the whole line is skipped until the first stream_event of the
  next round)
- **Session sync**: the provider compares already-synced messages by
  semantic fingerprint (timestamps stripped); when compaction/history
  rewriting causes divergence, the CLI session is restarted and replayed as
  a flat transcript (functionally correct, but CLI-side cache is lost)
- Image blocks map natively (base64); usage comes from the result event
  (subscription-based, cost is 0). Launch args/environment mirror the TS
  reference plugin (pi-codebuddy-sdk → @tencent-ai/agent-sdk): `--tools ""`
  disables CLI built-in tools, `--strict-mcp-config` ignores user MCP
  config, `--permission-mode bypassPermissions` (tool gating is Tack's job),
  `--setting-sources none` isolates user/project settings, `--system-prompt`
  replaces the CodeBuddy default identity, `--allowedTools mcp__tack` allows
  only bridged tools; `--include-partial-messages` enables `stream_event`
  incremental streaming (older CLIs automatically fall back to whole
  assistant messages); pi thinking levels map to `--effort`
  (minimal/low→low, medium→medium, high→high, xhigh/max→xhigh, with the
  model's thinkingLevelMap taking precedence); model metadata estimated by
  id (gemini 1M ctx, claude/gpt 200K, default 128K/8K); tool-argument alias
  tolerance (`file_path`→`path`, `old_string`→`oldText`, etc.; bash defaults
  to a 120s timeout); env vars disable background tasks/auto-update/
  auto-memory/auto-compact (`CODEBUDDY_CODE_DISABLE_BACKGROUND_TASKS`,
  `DISABLE_AUTOUPDATER`, `CODEBUDDY_DISABLE_AUTO_MEMORY`,
  `DISABLE_AUTO_COMPACT`); on abort, pending MCP calls are gracefully
  resolved and the session is marked for rebuild on the next round

Prerequisites: the `codebuddy` CLI is installed and logged in. Windows is
also supported: PATH probing uses `where` (an npm-global `codebuddy.cmd`
shim is launched via `cmd /c`, and session teardown uses `taskkill /T /F`
to kill the whole tree, avoiding orphan node processes); `CODEBUDDY_PATH`
can also point directly at a `.cmd`/`.bat`/`.py` script. Supersedes the old
OpenAI shim bridge plugin approach (no Node, no local HTTP translation
layer).

### Background tasks

The `bash` tool with `run_in_background: true` returns a task id immediately
without blocking:

- `bash_output`: read output (omit task_id to list all tasks);
- `bash_wait`: block until the task finishes (or timeout, default 600s),
  returning the moment it completes — use it when the next step depends on
  the task result; don't `sleep` in the foreground (a sleep can't be
  interrupted by the completion notification);
- `kill_shell`: kill the whole process tree;
- on completion the TUI shows a notification and auto-steers the agent
  ("task bg3 finished"), so the agent can read the result and keep working;
- if a task completes while the agent is idle: auto-wake starts a new round
  (same pattern as cron triggers), no human nudge needed. Disable:
  `"backgroundAutoWake": false`.

Typical uses: dev servers, `cargo build`, long test runs. Background tasks
survive across runs (TUI/RPC session scope) and are reaped when the process
exits.

Disable: `"features": {"backgroundTasks": false}` — the `run_in_background`
parameter disappears from bash's schema entirely; the model never knows it
exists.

### Background subagents (fire-and-forget)

The `subagent` tool with `run_in_background: true`: dispatch a full subagent
to run a long task in the background, into the same task manager
(`bash_output` shows live progress — tool calls logged one by one;
`bash_wait` can block until it finishes), and completion notifications use
the same channel.

```json
{"task": "Refactor the src/auth module and run the tests", "description": "auth refactor", "run_in_background": true}
```

### Model fallback chain

```json
"fallbackModels": ["anthropic/k3", "openai/gpt-5.2"]
```

If the primary model still hits 429/overload/5xx/timeout after provider-level
retries are exhausted → automatically switch to the next model and retry the
current turn; the TUI notifies and updates the current-model display. 401-type
auth errors do **not** fall back (switching models won't help). Subagents have
their own explicit model selection and don't inherit the chain.

### Budget circuit breaker

```json
"tokenBudget": 500000,
"tokenBudgetAction": "pause",          // warn (default) | pause | downgrade
"budgetDowngradeModel": "openai/gpt-5-mini"  // downgrade target; defaults to the last entry of fallbackModels
```

`pause` stops the run at a turn boundary (the budget warning explains how to
continue); `downgrade` fires only once and switches to the budget model.
`/cost` shows the per-model token/cost breakdown and budget progress anytime.

## Code Awareness

### LSP integration

Operations of the `lsp` tool:

```
diagnostics        (default) compile/type errors for a file; omitting path = all files with problems.
                   When the background type check (rust-analyzer flycheck) hasn't finished,
                   this is explicitly marked "analysis still running" — a syntax-level clean
                   file is never misreported as type-check-passed
definition         path + line + column (or name) → definition location
references         path + line + column (or name) → all references
implementation     path + line + column (or name) → trait/interface implementations
symbols            path → file symbol outline (class/function tree)
workspace_symbols  path + query → project-wide symbol search (query is a substring, empty = all)
hover              path + line + column (or name) → type/signature info (markdown, truncated to 2000 chars)
incoming_calls     path + line + column (or name) → who calls this symbol (call hierarchy)
outgoing_calls     path + line + column (or name) → what this symbol calls
code_actions       path [+ line + column] → list quickfixes; apply=<index> + line + column → apply (written to disk, checkpointed first)
rename             path + line + column + new_name → project-wide rename (written to disk, checkpointed first)
```

`name=<symbol>` is supported instead of line/column: located via exact
workspace/symbol match; when there are multiple candidates their locations
are listed so you can disambiguate with path+line+column; when path is
omitted, all started servers are queried.

Context diagnostics for code_actions are automatically scoped to the range
at the cursor position; Command-type actions returned by the server are
marked as not applicable (only actions carrying a WorkspaceEdit are applied).

Multi-root sessions: extra directories from `--add-dir` (when they exist and
aren't duplicates) are sent to the language server as workspaceFolders, so
cross-directory dependencies resolve. At most 64 documents are open per
server at a time; beyond that the least-recently-synced document is
automatically didClose'd (its cached diagnostics are dropped too).

edit/write results automatically come with the file's latest diagnostics
(disable with `lspEditFeedback: false`); diagnostic lines carry source and
code (e.g. `[rustc E0308]`), plus a summary of error counts in other files
(e.g. "3 error(s) in 2 other file(s): tui/mod.rs (2), chat.rs (1)") —
cross-file breakage from changing an enum/signature is visible without
waiting for a build. Servers start lazily on demand and stay warm; the first
query waits for indexing. Language servers are auto-detected by extension;
`lspServers` can override.

Known rust-analyzer behaviors (handled built-in): empty diagnostics are sent
before real ones (the client has a settle loop, waiting per file); definition
returns empty before the first analysis completes (warmup gate); rename
returns ContentModified while analysis is in flight (auto-retried); type
diagnostics come from the background flycheck (tracked via
serverStatus/progress, so diagnostic feedback distinguishes "syntax-level
clean" from "type check complete").

Server processes restart transparently after a crash (at most 3 times per
extension; beyond that the server is marked failed to avoid a crash loop);
each request has a hard 60s timeout, so tool calls can never hang forever.

Disable: `"features": {"lsp": false}`.

### File checkpoint rollback

At the start of every user turn:

1. inside a git repo: snapshot all dirty/untracked files (git baseline);
2. within the turn: snapshot original contents before the edit/write tools
   write a file.

`/checkpoints` lists which files changed in each turn; `/checkpoints restore
<turn>` rolls back:

- files changed by edit/write → restored exactly from blobs;
- tracked files changed by bash (`sed -i`, `cargo fmt`) → restored via
  `git checkout`;
- files created by bash → deleted;
- **files that were already dirty before the turn started → restored to
  their dirty content at that time** (not HEAD).

Complementary to `/rewind`: rewind only rolls back the conversation tree,
checkpoints restore files.

Disable: `"features": {"checkpoints": false}`.

### Custom subagents

`.pi/agents/*.md` (project-level, requires trust) or
`~/.tack/agent/agents/*.md` (global):

```markdown
---
name: reviewer
description: Code review — invoke after implementing a feature
tools: read, grep, find, ls, lsp
model: anthropic/k3
---

You are a rigorous code reviewer. …… (the body becomes the subagent's system prompt)
```

Then call it from the `subagent` tool with `agent: "reviewer"`: independent
system prompt + tool whitelist + model override. A project-level agent with
the same name overrides the global one.

### Worktree isolation

`subagent` with `isolation: "worktree"`: automatically `git worktree add` a
temporary branch; the subagent edits files inside it, and the result comes
with the branch name + path + diff stat. Multiple subagents editing the same
codebase in parallel don't step on each other; the parent agent decides
whether to merge or discard.

### Concurrency cap and shared budget

```jsonc
// settings.json
{ "subagents": { "maxConcurrent": 4, "budgetTokens": 200000 } }
```

- `maxConcurrent`: cap on simultaneously running subagent loops (semaphore),
  shared by parallel batches + background subagents; overflow queues (and can
  be cancelled).
- `budgetTokens`: cumulative token cap for all subagents in this session
  (including fire-and-forget background ones); once exceeded, new `subagent`
  calls fail immediately. Subagents don't write session files, so the global
  `tokenBudget` hook can't see their usage — hence this separate ledger; the
  tool result carries `usageTokens`/`budgetTokensUsed` so the parent agent is
  aware of delegation cost.

Both keys are `0` = unlimited (default).

### Plugin inheritance

```jsonc
// settings.json
{ "subagents": { "inheritPlugins": "hooks" } }
```

Subagents are non-interactive (no permission prompts), so the plugin
surfaces they inherit are a deliberate choice:

- `"hooks"` (**default**): plugin hook bridges (tool-call interception,
  context transform, result patching) run inside the child loop, ordered
  after the child's `permissions.deny` rules — a spawned subagent cannot
  bypass a guardrail plugin's `beforeToolCall` verdicts. Plugin tools are
  NOT added (the child's tool surface stays the built-in coding set).
- `"full"`: hooks + plugin tools (`ext__*`) join the child tool set,
  narrowed by the agent definition's `tools` whitelist like any tool.
- `"none"`: legacy behavior — children see no plugin surfaces at all
  (guardrail plugins don't see child tool calls).

The parent's loaded plugins are shared in-process (no respawn); the
JSON-RPC peer multiplexes concurrent parent/child calls. The key layers
like any settings key, so managed settings can pin it.

### Controlled git tool

Built-in `git` tool: the agent invokes git structurally with `command:
"status"` / `"add -A"` / `"commit -m msg"`; the tool parses the subcommand
itself, so the permission system classifies by subcommand instead of matching
free text (bash wildcards are easily bypassed by `git -C`, aliases, quoting):

- Purely read-only subcommands (status/log/diff/show/blame/reflog/rev-list/
  ls-tree/ls-remote/branch in list form/explicitly read forms like
  `config --get`/…) are prompt-free in plan mode and ask mode, and
  prompt-free under `Git` rules; all other subcommands go through the normal
  permission prompt.
- Quote-aware parsing: shell metacharacters are only rejected **outside
  quotes**, so commit messages like `commit -m "fix: a > b \`x\` $(y)"` work
  fine; every parsed argument is re-escaped with single quotes before
  execution, making quoted content fully inert to the shell.
- `commands: ["status", "log --oneline -10"]` batch form: multiple
  invocations executed in one call (in order, stop on first error). The
  read-only classification requires **every** entry to be read-only;
  permission rules match batch calls per entry — a deny hit on any entry
  rejects the whole call, allow requires all entries to match.
- Validation rejects unquoted shell metacharacters, global options
  (`-C`/`--git-dir`), and subcommands not on the whitelist — which also
  blocks the config-alias (`alias.x = !shell`) injection path. Execution goes
  through the bash executor, with sandbox policy aligned with the ACP client
  terminal.
- Permission rules are written just like bash: `permissions.allow:
  ["Git(push *)"]`, `permissions.deny: ["Git(reset --hard*)"]` (wildcard
  matching, deny wins).

bash is still available (temp directories, non-git tasks), but the system
prompt steers toward the git tool first.

### Context usage visualization

`/context`: shows how many tokens each segment of the current context window
occupies — system prompt, tool schemas, user messages, assistant messages,
thinking, tool results (with the largest tools sorted by size) — calibrated
against the real values reported by the provider, with window occupancy and
headroom before auto-compaction triggers. You can see which segment is
ballooning before compaction gets expensive and slow. `/todo` shows the
session task list (`/todo clear` empties it), same state as the TUI panel.

## Memory & Sessions

### Persistent memory

The `memory` tool (save/delete/list) writes two scopes in the **Claude
Code-style auto memory** fashion:

- **project** (default): conventions/decisions/environment quirks of this
  repo, stored in `<memory-root>/projects/<encoded-repo-path>/`. Keyed by the
  main worktree root, **shared by all worktrees of the same repo**;
  non-git directories fall back to cwd.
- **user**: cross-project user preferences/ways of working, stored in
  `<memory-root>/` (default `~/.tack/agent/memory/`; changeable via the
  `memoryDirectory` setting or the `TACK_MEMORY_DIR` env var, env wins).

The MEMORY.md indexes of both scopes are automatically injected into the
system prompt of **every** new session (TUI/RPC/ACP/print all go through
`assemble_system_prompt`). `/memory` views by section, `/memory forget <name>
[project|user]` deletes (project first, then user, when no scope is given),
`/memory edit [<name>|user|project]` edits memory files in an external
editor (the index is rebuilt automatically afterwards; without a name it
edits that scope's MEMORY.md directly).

Index constraints aligned with Claude Code: when the entries section exceeds
**200 lines / 25KB**, injection is truncated with an explicit warning; at
≥80% usage the system prompt reminds the agent to consolidate; **a write
that would push the index over the limit fails with an explicit error**
(rather than silently truncating content future sessions would see), and the
error message tells you to compress first and retry. Memory files carry a
`modified` (ISO 8601) timestamp in their frontmatter.

The agent is explicitly told not to record things the repo already answers
(code structure, CLAUDE.md content, git history).

Disable: `"features": {"memory": false}` (index injection disappears too).

### Session storage v4

Follows the upstream TS pi WP01–WP08 storage evolution (`tack_session::v4`
+ `v4_bridge` modules; see crates/tack-session/V4_NOTES.md for details).
**v4 is the default write path** (`sessionBackend` defaults to `v4`):

- **Transaction log format**: header on the first line (field-level alignment
  with upstream `JsonlStorageHeader`), then one transaction per line
  (`entry/usage/value/list` write types aligned with upstream
  `CommittedWrite`), `seq` monotonic across the whole session; branch/lane
  separation and named branches (WP06/08 minimal usable semantics).
- **Migrate on open**: v3/v2/v1 session files are transparently migrated to
  v4 when opened (two streaming passes, `.bak` kept, atomic publish,
  encrypted sessions stay encrypted, original untouched on failure).
  `sessionBackend: "v3"` keeps the old write path as an escape hatch (and
  refuses to open v4 files).
- **Write-path mapping**: one transaction per entry = entry + branch tip move
  + mirrored values (session name/label/lane config) + usage ledger row (LLM
  usage of assistant/compaction/branch_summary);
  `model_change`/`thinking_level_change`/`session_info`/`label` are stored as
  custom entries with current values mirrored — both session-context rebuild
  (model/thinking level) and v4-native tooling (fork projection) are correct.
- **fork-policy projection rules** are live on the production path: `tack
  fork` produces a v4 fork via `run_v4_fork` (lane state reset, usage ledger
  not copied, `parentSessionId` pointer); unknown `tack.*` preserved
  namespaces error out instead of being silently copied.
- **No data loss**: unknown/future entry types (written by TS extensions) are
  preserved as custom entries on migration (upstream errors out directly);
  corrupted lines without an id are skipped (payload stays in .bak) — one bad
  line never locks up a whole session.
- SQLite backend unchanged (upstream WP07 not yet aligned).

### Cross-session search

`/search <query>`: full-text search across the user/assistant messages of all
historical sessions, snippets in reverse chronological order, opened with
`/resume`. "How did we solve that problem last time" is directly searchable.

### Session encryption at rest

`"sessionEncryption": true` — entry lines of session files are encrypted
on disk with AES-256-GCM (`tack-enc:v1:` prefix; header lines stay plaintext
so listing/search keep working). The key lives in the OS credential store
(auto-generated on first use); when the key is unavailable, encrypted lines
are silently skipped. Enable for compliance scenarios (lost laptop/shared
directories).

### Scheduled tasks

```
/cron add "every 10m" check whether cargo test is hanging
/cron add "*/5 * * * *" summarize current progress
/cron            list tasks (with next trigger time)
/cron pause|resume|remove <id>
```

Tasks persist in `~/.tack/agent/cron.json`; on trigger, if the agent is idle
the task runs immediately, otherwise it queues as steering. **Tasks missed
while Tack was off are fired once on next startup**, then the rhythm resumes.

Disable: `"features": {"cron": false}`.

## Defense in Depth

Three-layer defense model (see configuration.md for configuration details):

| Layer | Controls | Configuration |
|---|---|---|
| Capability | whether a feature exists at all | `features.*` |
| Permission | whether this call needs to ask | `permissions.allow/deny` + permission mode |
| Confinement | what a running process can touch | `sandbox` |

### Declarative permissions

```json
"permissions": {
  "allow": ["Bash(cargo *)", "Bash(npm run test*)", "Edit(src/**)"],
  "deny":  ["Bash(git push *)", "Edit(**/.env*)", "Bash(rm -rf *)"]
}
```

deny wins unconditionally (even in bypass mode), and applies in headless
mode (CI) as well. Choosing "always" in a TUI permission dialog persists
(survives restarts).

### Untrusted-content defense

Results from web_fetch/web_search/MCP:

1. are wrapped in `<untrusted_content source="…">` — the model is told it's
   data, not instructions;
2. once untrusted content has been seen in this run, bash/edit/write get a
   **mandatory prompt** (allow rules and the allow-always cache are both
   void), reset when a new user prompt arrives;
3. bypass mode is exempt (an exemption the user explicitly chose).

This is a targeted defense against the chain "a web page embeds 'ignore
previous instructions' → the agent runs it with allow-always bash".

### OS sandbox

**On by default** (`"sandbox": "off"` disables explicitly): bash (including
background tasks) runs inside:

- **Linux** bubblewrap: read-only system, writable workspace,
  `sandboxNetwork: false` cuts the network;
- **macOS** seatbelt: same kind of policy (sandbox-exec profile);
- **Windows** Job Objects: reliable process-tree termination +
  `sandboxMaxProcesses`/`sandboxMaxMemoryMb` resource limits (**note: not
  filesystem isolation**).

**Boundary caveat**: the sandbox restricts **writes**, not **reads** — a
sandboxed process can still read `~/.ssh`, cloud credentials, etc., and
`sandboxNetwork` defaults to `true`. To defend against the "read secrets and
exfiltrate" chain you must also set `sandboxNetwork: false` (or prompt on
sensitive commands at the permission layer). Also, the file tools
(read/edit/write) themselves do no working-directory fencing — the boundary
is entirely at the permission layer; keep that in mind for headless/
auto-approve deployments.

Platforms without a backend degrade gracefully and warn once in the log.
LSP/edit tools don't go through the sandbox (they go through the permission
system).

### serve remote security

```
tack serve --listen tcp:0.0.0.0:7749 --auth-token $(openssl rand -hex 32) --tls
```

- `--auth-token`/`--auth-token-file`: hello frame carries the token, SHA256
  constant-time comparison;
- `--tls`: a self-signed cert is auto-generated on first start
  (serve-cert.pem/serve-key.pem in the agent directory, 0600); clients use
  `--tls --tls-ca <cert>` or `--tls-insecure`;
- a prominent warning is printed when binding non-loopback with no token and
  no TLS.

`--listen ws:ADDR` (WebSocket transport + embedded zero-dependency browser
client, HTTP `GET /` returns the page) is likewise protected by
`--auth-token` (hello frame carries the token); `--tls` now works for `ws:`
listeners too — the same self-signed cert serves wss:// directly (clients:
`tack client --addr wss:host:port`, or `ws:` + `--tls`), no external TLS-
terminating reverse proxy needed anymore.

Web client support: permission dialogs (allow once/always/deny; parallel
tool calls queued and prompted one by one), permission-mode switching
(ask/acceptEdits/plan/bypass, via the new `set_mode` command), model and
thinking-level selection, session list/create/attach/switch. Protocol
extensions are all additive (new optional fields + `serde(other)` unknown-
message catch-all), and remote sessions default to bypass mode — old clients
that can't answer never receive the new `permission_request` events, staying
wire-compatible with TS `@earendil-works/pi-protocol` v1.

Plugin surfaces are exposed the same way (`ext_*` commands + gated events,
opt-in via hello `capabilities`: `ext_widgets`, `ext_dialogs`): remote
clients can list/invoke plugin slash commands, list widgets and report
widget actions, query autocomplete providers, and answer plugin
`ui/select`/`ui/confirm`/`ui/input` dialogs and MCP elicitations
(broadcast, first answer wins). `tack client` opts into both and provides
`/ext`, `/widgets`, `/complete`, `/answer`, `/cancel`.

### Managed settings (organization-enforced)

The organization-level policy file (path in configuration.md) can: force
sandbox on, force features on/off, disable bypass mode, lock providers/
models, append non-removable deny rules, and govern plugins (`pluginPolicy`:
managed-only loading, source allow-lists, per-plugin capability narrowing —
see "Extension system" below). Combined with the project-trust
mechanism, neither the user side nor the repo side can bypass organizational
policy in an enterprise deployment.

### Credential keyring

`credentialStore: "auto"` (default): when the OS credential store
(DPAPI/Keychain/libsecret) is available, tokens/keys are stored there and
auth.json keeps only placeholders; when unavailable (headless Linux) it
automatically falls back to files. `"file"` locks in the old behavior.
Existing file credentials keep working seamlessly.

### Trace redaction

The observability JSONL logs are automatically redacted before hitting disk:
sensitive field names
(token/key/authorization/secret/password/credential/cookie), `Bearer …`
values, and sensitive URL query parameters are all replaced with `***`.

## Ecosystem Interop

### Hooks (13 events)

PreToolUse / PermissionRequest / PostToolUse / UserPromptSubmit / SessionStart /
SessionEnd / PreCompact / PostCompact / Stop / SubagentStart / SubagentStop /
Interrupt / Notification. Claude Code compatible: nested
`{matcher, hooks}` config (legacy flat format auto-accepted), regex matchers,
three handler types command/prompt/agent, stdout JSON verdict protocol
(block / permissionDecision / updatedInput rewrite / additionalContext
injection). `SubagentStart` fires before a subagent starts (payload carries
`agent_type`/`prompt`/`description`/`isolation`/`background`, matcher matches
the subagent name): a block verdict refuses the launch (the tool returns an
error with zero token consumption; the background-subagent sync gate never
registers a doomed task), `additionalContext` is appended to the subagent
task. Zero extension-process cost — shell commands directly in settings.json;
enterprises can use `managed-hooks.json` + `managedHooksOnly` to lock down to
managed hooks only. See [hooks.md](hooks.md) for details.

### Extension system (tack-RPC v3)

- **tack-RPC v3 protocol**: plugins speak JSON-RPC 2.0 over NDJSON stdio
  (both-directions requests, `$/cancelRequest`, 30s call timeout, crash
  isolation), defined schema-first in `protocol/tack-rpc.openrpc.json` —
  host types (`tack_ext::rpc3`) and the TypeScript/Python SDK types are
  generated from it (`cargo run -p xtask -- codegen`, freshness-checked in
  CI). The pre-v3 NDJSON protocol was removed in the same release.
- **SDKs + dev tooling**: Rust (`tack-ext-sdk`), TypeScript (`@tack/plugin`,
  `sdk/typescript`), Python (`tack-plugin`, `sdk/python`) — builder APIs;
  plugin code never sees a JSON-RPC envelope. `tack ext new <dir>
  <rust|ts|python>` scaffolds, `tack ext inspect` dumps the handshake
  capabilities, `tack ext dev`/`ext test` drive a plugin against mock-host
  scenario files (scripted plugin→host answers, recursive subset-match
  assertions, non-zero exit on failure).
- **Three carriers**: `process` (default crash-isolated subprocess), `wasm`
  (wasmtime sandbox; auto-detects WASI-stdio core modules and WIT components
  `tack:plugin@0.3.0` — components are capability-free by construction; fuel
  / wall-clock / memory hard caps; examples `examples/extensions/hello-wasm*/`,
  `hello-component/` in hand-written WAT), and `mcp` (an MCP server *is* the
  plugin — its tools/resources/prompts are adapted with full plugin identity:
  `ext__` naming, attribution, policy, hook interception, and the
  untrusted-content defense).
- **Plugin identity + versioned store**: plugins are `name@source`; installs
  land in `extensions/store/<source>/<name>/<version>/` with atomic
  stage-verify-swap-rollback and fingerprint-idempotent upgrades; lockfile v2
  (v1 auto-upgraded); legacy flat `extensions/<name>/` installs keep loading.
  Load failures are first-class state — broken/disabled/policy-blocked
  plugins stay visible in `tack ext list`. `tack ext enable|disable|upgrade`.
- **Capability surface** (declared at `initialize`, undeclared = never
  called): tools (`ext__<plugin>__<tool>`, may return image blocks), slash
  commands, `beforeToolCall` (allow/deny/**rewrite**), `transformContext`,
  `afterToolCall` result patching, lifecycle events, UI dialogs/widgets,
  autocomplete, `session/get` + `session/sendUserMessage`, trust-gated
  `exec/run`, runtime provider registration, and **provider bridges**
  (`capabilities.provider.stream`: the plugin serves inference directly — no
  HTTP shim — in all four run modes; models get the reserved api kind
  `ext-provider-bridge`, appear in `/model`, credentials stay plugin-side;
  see [plugin-provider-bridge.md](plugin-provider-bridge.md)).
- **Approval chain** (`capabilities.hooks.approvalReview`): when the
  permission flow is about to prompt a human, reviewer plugins get first
  crack in load order — first claim wins, pass/`askUser` defers, errors
  fail open. Claims approve one-shot (nothing persists into allow-always),
  audited under `plugin_approval`; wired on every prompt-capable surface
  (TUI, rpc, ACP, remote host). Plugin `beforeToolCall` bridges
  run BEFORE the permission layer on every surface, so reviewers (and the
  dialog) see the final post-rewrite arguments. Calls that entered untrusted
  web/MCP content skip the chain — the human must be asked.
- **Extension bundle (Level 1)**: `extension.json` can declare `hooks`
  (Claude format, merged into session hooks), `mcpServers` (merged into MCP
  connections), `skills` (merged into skill discovery); bundle-only manifests
  (no command/module) are valid.
- **Marketplace + distribution**: `tack ext marketplace add/list/remove/sync`
  registers JSON catalogs (signed catalogs pin an ed25519 key, TOFU); settings
  `pluginMarketplaces` keeps catalogs fresh in a background startup sync that
  never blocks; catalog v2 entries add `installation` (incl.
  `installed-by-default`) and inline `manifest`. `tack ext bundle pack`
  writes a deterministic `<name>-<version>.tgz` — the air-gapped unit,
  extracted under hostile-input rules (no links, no traversal, size caps).
- **Observability**: load telemetry counts by outcome (`active | disabled |
  failed | policy-filtered`, target `plugin_load`) and persists
  `extensions/last-load.json`, which `tack doctor` reads to report failures
  with causes; Level-3 plugins can emit metrics untrusted through the
  declare-at-initialize metrics sidecar (strict drain validation, target
  `plugin_metrics`; Rust SDK `MetricsRecorder`).
- **Enterprise plugin policy** (managed `pluginPolicy` key):
  `managedPluginsOnly`, `allowedSources` (git URL with optional `ref` pin,
  host regex, local roots), per-plugin `enabled` (wins over user/project in
  both directions), narrow-only per-plugin `tools`/`mcpServers`/`hooks`/
  `provider` intersections. Enforced twice — at install time (before any
  clone/network) and at load time (discovery filter + registration-time
  narrowing); blocked plugins stay visible as `policy-blocked (<reason>)`
  rows; every decision is audit-logged with rule and origin layer (shipped
  via the managed `auditSink`).
- **Subagent inheritance**: `subagents.inheritPlugins` (none | hooks | full,
  default hooks) shares the parent's plugins with child loops — see
  "Plugin inheritance" under Agent Execution.

See [plugin-system.md](plugin-system.md), [extensions.md](extensions.md),
[plugin-provider-bridge.md](plugin-provider-bridge.md) for details.

### MCP server mode

`tack mcp-serve` (stdio): other agents/IDEs can call Tack as an MCP server.

| Tool | Description |
|---|---|
| `prompt(text)` | run the full coding agent (context accumulates for the lifetime of the service) |
| `get_session_stats()` | cumulative token/cost |
| `read_context(maxMessages?)` | recent message digest |
| `list_available_tools()` | list of available tools |
| `reset_session()` | clear the context |

### MCP client OAuth 2.1

Add `"oauth": true` or `{"clientId": "…", "scopes": […]}` to a remote (HTTP)
server in mcp.json to enable: metadata discovery → dynamic client
registration (or the configured clientId) → PKCE browser authorization
(loopback callback, manual paste supported). `clientSecret`, `callbackPort`
and `callbackUrl` (loopback-validated) cover pre-registered clients and
fixed redirect URIs. Tokens are cached in
`~/.tack/agent/mcp-tokens.json` (0600), auto-renewed with refresh_token when
expired. The cache is indexed by **server name** (the key in mcp.json) — a
server authorized in the TUI hits the cache directly under the same-name
config in print/rpc/serve and other headless modes. The TUI authorizes
interactively; print/rpc/serve only use cached/renewed tokens and never pop
a browser.

### MCP sampling and elicitation (server-initiated requests)

Besides tools/resources/prompts, an MCP server can also make reverse
requests to the client:

- **Sampling** (`sampling/createMessage`): the server requests an LLM
  completion. Enable with `"mcpSampling": true` in settings.json (**off by
  default**, capability not declared). When enabled it runs with the current
  session's provider/model — resolved per request, so model changes are
  picked up by every connection, including Level-2 plugin MCP servers
  (which connect at extension-load time, before any session exists) — with
  the same defenses as MCP tool results: the
  server-provided system prompt/messages are all wrapped in
  `<untrusted_content>` with a guard prefix, used only in an isolated
  sub-call context, never entering the main session; tools/toolChoice are
  refused (no side channel for the server to drive local tools), as is
  audio; `modelPreferences.hints` are ignored (the current model is always
  used). Every call is written to the tracing log, and token usage counts
  into the session (TUI bottom stats + notice).
- **Elicitation** (`elicitation/create`): the server requests structured
  input from the user. `"mcpElicitation": true` (**on by default**). The TUI
  prompts field by field per the server-given JSON schema
  (string/number/integer/boolean/enum auto type conversion, required-empty
  re-asks, Esc = cancel); `tack serve` forwards the form to dialog-capable
  remote clients (`ext_dialog_request`, first answer wins) and declines
  when none is connected; the remaining headless modes (print/rpc/acp)
  auto-decline; URL mode (server-specified browser flow) is always
  declined. The decision logic (mode × switch → prompt/decline) lives in
  `mcp_elicitation::elicitation_decision` and has unit tests.

### Tool schema lazy loading (tool search)

`mcpDeferThreshold: N` — when the total tool count exceeds N, MCP tools don't
enter the model's tool table directly (zero schema overhead); they go into a
deferred pool invisible to the agent; the agent retrieves by capability with
`tool_search`, and matched tools become callable from the next step on.
N=0 (default) disables this.

Per-server control (mcp.json, pi-style) refines this: `"exposure": "deferred"`
pools one server's tools regardless of the threshold, `"hidden"` makes them
unreachable, `"direct"` (default) declares them like built-ins, and
`toolExposure` overrides per tool by exact name or `*` pattern (exact names
win, then the longest pattern). pi's `"codemode"` values map to `"deferred"`
(tack has no codemode tool). Deferred servers are named in a "Deferred MCP
tools" system-prompt section so the model knows `tool_search` reaches them;
configuring any `exposure` opts out of the blanket threshold rule.

### Eval regression testing

```
evals/
  examples/
    fix-typo/
      task.json   {"prompt": "…", "setup": "…", "verify": "…", "timeoutSecs": 300}
  solutions/    # reference solutions for selftest.sh (validate tasks without a model)
```

```
tack eval evals/examples --runs 3 --report report.json --baseline baseline.json
```

Per task: setup (optional) → run the agent headless → verify (exit 0 =
pass). The report includes pass rate/token/cost; `--baseline` compares
against an old report, with ▲▼ marking rises/drops.
`examples/ci/tack-eval.yml` is a ready-made GitHub Action regression gate
(▼ means fail).

**Docs audit** (`evals/docs-audit/`): `static_check.sh` runs without a
model — it cross-checks the settings keys / hook events / CLI flags /
`features.*` keys documented in docs against the code implementation for
drift (documented but absent in code = FAIL; known exemptions come with a
reasoned list). There are also 3 agent-driven deep-audit tasks (sandbox
default values, permission deny precedence, hooks verdict protocol) that
generate expectations from deterministic anchors, so the tasks flip their
expectations automatically when either docs or code drifts — no manual
definition edits needed.

## Ops Tooling

- **`tack stats`**: cross-session usage reports — scans all project sessions
  in the agent directory, aggregates per-provider×model tokens
  (input/output/cache read/cache write/thinking) and costs estimated from
  catalog pricing, plus per-day time series;
  `--since 7d`/`--until`/`--json`/`--dir <path>`. Read-only defensive
  parsing: both v3/v4 session formats supported (v4 uses the usage ledger as
  authoritative, no double counting), encrypted entries (without a key) and
  corrupted lines are skipped and counted, the SQLite backend is included via
  the public API; models without catalog pricing are marked `-` and excluded
  from total cost.
- **`tack doctor`**: item-by-item self-check (shell/git/5 LSP servers/
  sandbox backends/headless browser/credentials/MCP server commands/fd+rg),
  each item with a fix suggestion; hard failures exit with code 1. Run it
  before reporting a bug. `--json` outputs a machine-readable report (with
  version/platform/TERM and other metadata); `--bundle [PATH]` generates a
  bug-report tar.gz (doctor report text+json, redacted settings.json,
  crash.log; auth.json is never included), default filename timestamped.
- **`tack logs`**: `--tail 100 --level warn --target tack_tools::lsp
  --follow`; in-TUI `/trace [level] [target]`.
- **Structured trace**: `observability.enabled` or `TACK_TRACE_FILE=1`
  enables JSONL export (`~/.tack/agent/logs/`); redaction as above.
- **Managed audit sink**: `auditSink: {"url": "https://…", "token": "…",
  "intervalMs": 5000}` in managed-settings.json — trace events batched over
  HTTP (Bearer auth, 50 entries or interval trigger, failures dropped without
  blocking, local JSONL retained). Only settable at the managed layer; once
  set, observability is force-enabled.

## TUI Experience

- **Startup update notice**: after TUI startup a background check for new
  versions runs (result cached 24h, skipped offline); when an update exists
  the footer shows `↑ vX.Y.Z` and the chat area hints `tack update`.
  Disable: `"updateCheck": false`.
- **Ctrl+R reverse history search**: Ctrl+R in the editor enters incremental
  search — typing filters prompt history (newest→oldest, case-insensitive),
  ↑/↓/repeated Ctrl+R cycle matches, Enter accepts, Esc cancels and restores
  the draft. (The original Ctrl+R dequeue binding moved to Alt+↑.)
- **Desktop notifications**: OSC 9 / OSC 777 terminal notifications fire on
  permission dialogs, run completion/errors, and background-task completion
  (5s throttle per event source; silently inert on unsupported terminals).
  Disable: `"notifications": false`.
- **plan mode loop closure**: under `/mode plan` the agent gets an
  `exit_plan_mode` tool — the plan lands on disk at
  `~/.tack/agent/plans/` and an approval dialog pops: Yes→acceptEdits (edits
  free, commands still ask), Always→bypass, No→stay in plan mode to revise.
- **ask_user tool**: the agent can pause mid-run and ask the user structured
  questions (`ask_user`, 1–4 per call) — multiple-choice (2–4 options with
  descriptions plus an "Other…" free-text escape; `multi_select: true` makes
  it a pick-several checkbox dialog whose answer is all selected labels) or
  plain free-text when options are omitted. The TUI walks the questions one
  dialog at a time (Esc dismisses the batch and the tool result tells the
  model to decide on its own). Headless modes (print/rpc/acp/serve) and
  subagents register the tool without an interactive handler: calling it
  returns an in-band "no interactive user" message so the model proceeds
  with its best judgment instead of blocking (same policy as MCP elicitation
  decline).
- **Multi-workspace**: `--add-dir <path>` (repeatable) or `additionalDirs` —
  extra directories' AGENTS.md enter the context, the system prompt lists the
  in-scope directories, and the sandbox writable set is merged.
- **i18n**: `language: "zh"` (or LANG=zh*) — all TUI user-visible copy is
  bilingual (slash-command help and descriptions, dialog titles/buttons/
  prompts, notifications/errors, status bar, footer, /settings /model
  /providers /trust /resume and other screens); logs/debug output, command
  names, and protocol values (permission modes, thinking levels) are not
  translated; missing translations fall back to English automatically.
