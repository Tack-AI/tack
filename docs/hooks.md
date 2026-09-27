# Hooks (lifecycle hooks)

**English | [简体中文](hooks.zh-CN.md)**

Tack hooks are **Claude Code compatible** lifecycle hooks: no extension
needed — intervene at key agent events with shell commands or LLM
evaluation. Configuration sources (in merge order):

1. Managed hooks: `~/.tack/agent/managed-hooks.json` (org management plane)
2. `hooks.*` in user/project settings (global
   `~/.tack/agent/settings.json` + project `.pi/settings.json`,
   deep-merged)
3. Extension bundles: the `hooks` field of `extension.json` (see
   [extensions.md](extensions.md) §2)

With `managedHooksOnly: true` (settings), only managed hooks are kept
(Codex's `allow_managed_hooks_only` semantics).

## Events

| Event | Timing | matcher matches against | Verdict capabilities |
|---|---|---|---|
| `PreToolUse` | Before a tool call | tool name | block, `updatedInput` argument rewriting, `permissionDecision` |
| `PermissionRequest` | Just before a permission dialog pops | tool name | `permissionDecision` answers in place of the user |
| `PostToolUse` | After tool execution | tool name | block (reason fed back to the model), `additionalContext` |
| `UserPromptSubmit` | After the user submits a prompt | — | block (discards the prompt), `additionalContext` |
| `SessionStart` | Session creation | — | `additionalContext` injected into the system prompt |
| `SessionEnd` | Session exit | — | fire-and-forget |
| `PreCompact` / `PostCompact` | Before/after auto or manual compaction | — | fire-and-forget |
| `Stop` | End of an agent turn | — | block → continue the run with the reason (at most once per stop point) |
| `SubagentStart` | Before a subagent starts (before worktree creation / token spend) | subagent name | block (refuses the start; the tool returns an error), `additionalContext` (appended to the task) |
| `SubagentStop` | Subagent completion | subagent name | fire-and-forget |
| `Interrupt` | Esc interrupts a run | — | fire-and-forget |
| `Notification` | Permission dialog or other events needing user attention | — | fire-and-forget |

The `SubagentStart` payload adds, on top of the common fields: `agent_id`
(custom agent argument or null), `agent_type` (custom agent name, default
`"subagent"`), `prompt` (first 2000 characters of the task), `description`,
`isolation` (`"worktree"`), `background` (bool). Background subagents
(`run_in_background`) are evaluated synchronously — a blocked one never
gets registered as a doomed task.

## Configuration format

Claude nested format (recommended) and the Tack legacy flat format
(auto-accepted):

```json
"hooks": {
  "PreToolUse": [
    { "matcher": "bash|edit",
      "hooks": [
        { "type": "command", "command": "check.sh", "timeout": 30 },
        { "type": "prompt",  "prompt": "Is this operation safe? Answer verdict JSON only" },
        { "type": "agent",   "prompt": "Check whether the files referenced by this command pose a risk" }
      ] }
  ],
  "SessionStart": [ { "command": "cat .pi/context.md" } ],
  "Stop":         [ { "hooks": [ { "type": "command", "command": "notify.sh", "async": true } ] } ]
}
```

Handler fields:

| Field | Notes |
|---|---|
| `type` | `command` (default) \| `prompt` \| `agent` |
| `command` | shell command (executed via `$SHELL -c`) |
| `timeout` | seconds, default 60; killed on timeout, no leaked processes |
| `async` | true = fire-and-forget (not awaited, verdict ignored) |
| `prompt` | evaluation instruction for prompt/agent handlers |
| `model` | `provider/id`; defaults to the session model |

**matcher**: empty/`*` matches everything; without regex metacharacters it
matches exact names (`|` for alternation); otherwise it is compiled as a
regex (e.g. `mcp__.*`).

## Command handler protocol

- **stdin**: a JSON object (snake_case): `session_id, transcript_path,
  cwd, hook_event_name, model, permission_mode` + event fields
  (`tool_name, tool_input, tool_use_id` / `prompt` / `trigger` / …).
  Exception: the Stop event's usage fields keep upstream camelCase
  (`totalTokens`/`totalCost`).
- **exit 0**: stdout may be empty, or a verdict JSON (loosely parsed,
  unknown fields ignored):
  ```json
  {
    "decision": "block", "reason": "…",
    "hookSpecificOutput": {
      "permissionDecision": "allow | deny | ask",
      "permissionDecisionReason": "…",
      "updatedInput": { "command": "ls" },
      "additionalContext": "context injected into the model"
    },
    "systemMessage": "shown to the user",
    "continue": false, "stopReason": "…"
  }
  ```
- **exit 2**: block, with stderr as the reason (PreToolUse blocks the tool;
  UserPromptSubmit discards the prompt; PostToolUse feeds it back to the
  model as an error).
- **other non-zero**: warning, does not affect the run.

Semantic details:

- `updatedInput` is a **partial merge** (overlaid on the original
  arguments, not a wholesale replacement); the rewritten input is
  re-validated against the schema.
- `permissionDecision: allow` skips the permission dialog (declarative deny
  rules still win); `ask` forces the dialog; `deny` blocks outright.
- Verdicts from multiple handlers are merged: first block wins; permission
  takes the strictest (deny > ask > allow); `additionalContext`
  accumulates.
- Hook failures are **always fail-open**: a broken hook never stalls the
  agent (log + warning only).
- Non-JSON stdout from a command hook is injected as plain-text
  additionalContext (all events, for legacy compatibility).

## prompt / agent handlers (LLM evaluation)

No command is run — the model decides: the hook input JSON is sent to the
model together with your `prompt` instruction, requiring an answer of
verdict JSON only (same schema as above). The `agent` type additionally
allows multiple turns plus read-only tools (read/grep/find/ls) to inspect
the workspace before answering. Evaluation calls use the session model (or
the handler's `model` override) and do not go through the extension event
channel.

```json
"PreToolUse": [
  { "matcher": "bash",
    "hooks": [ { "type": "prompt",
                 "prompt": "If this bash command could destroy user data, deny; if unsure, ask." } ] }
]
```

## Related settings

| Key | Notes |
|---|---|
| `features.shellHooks` | false disables all hooks from executing |
| `managedHooksOnly` | true runs only managed-hooks.json |
| `hooks.*` | hook declarations in settings (global + project deep-merged) |
