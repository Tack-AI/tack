# CodeBuddy Usage Guide

**English | [简体中文](codebuddy.zh-CN.md)**

Use the `codebuddy` CLI as a first-class Tack provider: the TUI, tools, skills,
and extensions all stay on the Tack side; CodeBuddy only handles model inference
(executed by the `codebuddy` CLI already installed on your machine). No Node
SDK, no local HTTP translation layer, no extra credential setup.

> For implementation details (tool bridge, session sync, etc.) see the
> "CodeBuddy native provider" section of [features.md](features.md); this
> document only covers usage.

## Prerequisites

1. **Install the codebuddy CLI** and confirm it is on PATH:

   ```bash
   which codebuddy        # or Windows: where codebuddy
   ```

   If it is not on PATH, point `CODEBUDDY_PATH` at the binary (`.cmd`/`.bat`/
   `.py` scripts are also accepted):

   ```bash
   export CODEBUDDY_PATH=/path/to/codebuddy
   ```

2. **A working login state** — Tack does not hold CodeBuddy credentials; it
   fully reuses the CLI's own login state. Any of the following that makes
   `codebuddy` usable in your terminal is enough:

   ```bash
   codebuddy login                              # browser login (recommended)
   export CODEBUDDY_API_KEY="your-api-key"      # or API key
   export CODEBUDDY_INTERNET_ENVIRONMENT=ioa    # Tencent iOA intranet + codebuddy login
   ```

   The rule of thumb is simple: **if `codebuddy` runs in your terminal, it
   works in Tack.**

## Quick start

```text
tack
/model            # select a model with the codebuddy/ prefix
```

On first launch Tack runs a one-time CLI handshake to discover the model list
(the result is cached to `~/.tack/agent/codebuddy-models.json`; later launches
read the cache directly and refresh in the background). If discovery fails and
there is no cache, it falls back to a single `codebuddy/default` pass-through
model, which is still selectable in `/model`.

To skip `/model` every time, set defaults in `~/.tack/agent/settings.json`:

```json
{
  "defaultProvider": "codebuddy",
  "defaultModel": "hy3-preview-agent-ioa"
}
```

Tools, skills, extensions, `/compact`, steer, etc. behave exactly as with other
providers — tool calls are still executed by Tack's permission/UI; the
CodeBuddy side only plans and invokes.

## Thinking levels

Tack's thinking levels map to CodeBuddy's `--effort`:

| Tack level | CodeBuddy effort |
|-----------|------------------|
| off | not passed (CLI default) |
| minimal / low | low |
| medium | medium |
| high | high |
| xhigh / max | xhigh |

In the TUI, use the `--thinking` startup flag or switch the thinking level
mid-session.

**Note**: `--effort` is a CLI process startup argument; switching the thinking
level mid-session restarts that session's CLI process (history is preserved via
transcript replay, but the CLI-side cache resets). Switching models is
different — it hot-switches via the `set_model` control request, preserving
both the session and the CLI cache.

## Session isolation (vs. standalone codebuddy)

codebuddy sessions started by Tack are **isolated** from your personal CodeBuddy
configuration (aligned with the default behavior of the TS reference plugin
pi-codebuddy-sdk):

- `--setting-sources none`: user/project/local settings are not loaded
  (AGENTS.md etc. are already carried by Tack's own system prompt);
- `--strict-mcp-config`: MCP servers registered in your codebuddy config do
  **not** enter the session — the model can only call tools bridged over by
  Tack (Tack's tools are declared as an SDK MCP server via `sdkMcpServers`,
  transported over `mcp_message` control frames, unrelated to user MCP
  config);
- the CLI's built-in tools (Read/Write/Bash, etc.) are disabled wholesale, to
  avoid bypassing Tack's permissions and UI;
- Tack's system prompt **replaces** CodeBuddy's default identity
  (`--system-prompt`); the model behaves as Tack, not as standalone CodeBuddy
  Code;
- the CLI's auto-update (prevents mid-session restart/disconnect), auto-memory,
  auto-compaction (context management is Tack's job), and background tasks are
  disabled.

Model metadata (context window, max output, thinking/image support) is
initially estimated from the model id (the CLI's model list carries no
capability info): gemini 1M ctx, claude/gpt 200K, everything else 128K; gpt
16K max output, everything else 8K. **Inaccurate estimates are corrected
automatically**: the CLI's result event carries the real serving parameters
(`modelUsage.*.contextWindow/maxOutputTokens`); Tack learns them, writes them
to the registry, and persists them to
`~/.tack/agent/codebuddy-models.json` (in the current session this takes effect
after re-selecting via `/model` or restarting). You can also override manually
— write a same-named provider entry in `models.json` (the provider-level
api/baseUrl fields are still managed by the built-in provider; only model
metadata takes effect):

```json
{
  "providers": {
    "codebuddy": {
      "models": [
        { "id": "hy3-preview-agent-ioa", "contextWindow": 1048576, "maxTokens": 32768 }
      ]
    }
  }
}
```

The model list itself is also cached to that file: at launch the cache is
registered synchronously (`/model` is usable immediately), then a background
handshake refreshes the discovery; if discovery fails the cache is kept, and
only when there is no CLI and no cache does it fall back to the
`codebuddy/default` pass-through model.

## Known behaviors

- **Compaction / history rewrite / thinking-level switch**: rewrite the
  CodeBuddy native session file (JSONL), then restart the CLI with
  `--resume` — the multi-turn structure and the CLI-side cache are preserved
  (aligned with the reference plugin's session-rebuild mechanism; falls back
  to flat transcript replay if the file write/validation fails). **Model
  switches do not restart**: they hot-switch via the `set_model` control
  request (falling back to the rebuild above on failure).
- **Argument fallback for parallel tool calls**: the CLI's stream_event replay
  is lossy for parallel tool_use (multiple blocks share the same content
  index, and input_json deltas may interleave or not stream at all). At tool
  boundaries Tack rewrites the arguments based on the MCP `tools/call` frames
  the CLI dispatches next (the frames come from the CLI's complete assistant
  message), and matches replies per frame (unaffected by argument
  normalization or same-name parallel reordering); block start also uses
  `content_block.input` as seed arguments (aligned with the reference plugin).
- **In-turn API retries**: on retry the CLI resends `message_start` and reuses
  content indexes; Tack closes any unclosed text/thinking blocks left over
  from the previous attempt in place (emitting End events), avoiding dangling
  blocks and misrouted deltas (garbled thinking stream).
- **abort (Esc)**: interrupting during generation (no tool call in flight)
  does not rebuild the session — residual messages from the interrupted turn
  are drained and execution continues; interrupting during tool execution
  rebuilds with a fresh session id (the killed old process may still be
  writing to the old session file; this avoids orphaned-write collisions).
- **usage/cost**: taken from the CLI's result event; cost is 0 under a
  subscription, token counts are normal.
- **rate limit**: rate-limit events show an inline warning in the TUI plus a
  desktop notification (controlled by the `notifications` setting); headless
  mode logs them.
- **session cleanup**: `/new` and the end of RPC/print sessions close the
  corresponding codebuddy CLI process; sessions idle for more than 2 hours
  are also reclaimed automatically.

## Troubleshooting

Enable provider debug logs:

```bash
RUST_LOG=tack_ai::codebuddy=debug tack
```

For protocol-layer pitfalls (parallel tool calls losing arguments, in-turn
retries, packet-capture methods, etc.) see
[codebuddy-pitfalls.md](codebuddy-pitfalls.md).

Common issues:

- **No codebuddy entries in `/model`**: does `which codebuddy` succeed? When
  it is not installed, the provider is not registered at all. Set
  `CODEBUDDY_PATH` and restart.
- **Only `codebuddy/default`**: the model-discovery handshake failed (CLI too
  old or a network issue). Upgrade the codebuddy CLI and restart Tack; the
  default model works directly (the CLI picks the model itself).
- **Tool calls report permission/timeout errors**: this is Tack-side behavior,
  the same as with other providers, and unrelated to CodeBuddy; check Tack's
  permission settings.
- **Windows**: the npm-globally-installed `codebuddy.cmd` shim is launched via
  `cmd /c`, and the whole tree is cleaned up on session reclaim; over-long
  system prompts are automatically injected via stdin (unaffected by the
  Windows command-line length limit).

## The ask_codebuddy tool (delegated subtasks)

When the codebuddy CLI is installed, Tack automatically registers the built-in
tool `ask_codebuddy`: delegate a focused subtask to an independent CodeBuddy
call (second opinion, code review, architecture questions, debugging
hypotheses); in full mode it can also act autonomously.

- **Clean session**: the delegated call only sees the prompt, not the current
  conversation — the prompt must be self-contained (state the question,
  relevant file paths, and concerns clearly).
- **mode**: `read` (default; read-only exploration; Write/Edit/Bash etc.
  forbidden), `full` (allows writes and bash; no Tack receipts; use with
  care), `none` (pure general knowledge, no file access). This uses
  CodeBuddy's own built-in tools, unrelated to Tack's tools/permissions.
- **model / thinking**: optionally override the model and thinking level
  (mapped to `--effort`).
- Execution is capped at 10 minutes; Esc interrupts the delegated call; the
  response body plus a tool-usage summary is returned.

## Differences from the TS plugin pi-codebuddy-sdk

Tack's alignment target is **identical production behavior** (startup
arguments, streaming, effort, argument tolerance); architectural differences
cause the following plugin capabilities to take a different form:

- **The `codebuddy-sdk.json` config file**: Tack does not read it (use the
  same-named codebuddy entry in models.json for overrides instead; see the
  "Model metadata" section);
- **The AskCodebuddy delegation tool**: provided as Tack's built-in tool
  `ask_codebuddy` (see the previous section); sharing the current conversation
  history is not yet supported (clean session);
- **Cross-process resume**: the mapping between Tack sessions and codebuddy
  session ids is persisted in `~/.tack/agent/codebuddy-sessions.json` — after
  restarting Tack, the first fork rebuild reuses the previous codebuddy
  session file (the new process does a native rebuild directly, without first
  spinning up a throwaway CLI).
