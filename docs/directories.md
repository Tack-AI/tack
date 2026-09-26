# Tack 目录与加载顺序

Configuration, rules, skills, and MCP: where Tack looks, and in what order.
(Everything lives under `~/.tack/` — sessions never touch TS pi's `~/.pi/`.)

## Filesystem layout

```
~/.tack/
  agent/
    settings.json       # global settings (deep-merged under project .pi/settings.json)
    auth.json           # API keys + OAuth credentials (TS pi auth.json compatible)
    models.json         # custom providers (TS pi schema)
    keybindings.json    # keybinding overrides (TS pi schema)
    mcp.json            # global MCP servers
    extensions/         # installed extensions (always loaded)
    marketplaces/       # extension marketplace catalogs (tack ext marketplace)
    cron.json           # scheduled jobs (/cron, missed runs fire once at startup)
    managed-hooks.json  # org-managed hooks (enforced with managedHooksOnly)
    AGENTS.md           # global rules (single file, TS pi layout)
    rules/              # global rules directory (*.md, all loaded, sorted by name)
    skills/             # user skills (priority 3)
    memory/             # persistent memory, user scope (MEMORY.md + *.md; root moves with memoryDirectory / TACK_MEMORY_DIR)
    memory/projects/<encoded-repo>/  # project-scope memory (worktree-shared)
    prompts/            # user prompt templates
    themes/             # user themes (<name>.json)
    sessions/
      --<encoded-cwd>--/*.jsonl   # per-directory session trees

<project>/
  .pi/
    settings.json       # project settings (wins over global)
    mcp.json            # project MCP servers (wins on name conflicts)
    AGENTS.md           # project rules (via ancestor scan)
    skills/             # project skills (priority 1)
    prompts/            # project prompt templates (win on name conflicts)
    themes/             # project themes (win on name conflicts)
    extensions/         # project extensions (trust-gated via /trust)
  .agents/
    skills/             # project skills (priority 2, ancestors up to git root)
```

> `~/.agents/skills/`（跨工具的 Agent Skills 标准位置）**不会**被加载——
> pi→Tack 切割时已移除该用户级来源。

## Rules（上下文文件）加载顺序

Context files are injected as `<project_instructions path="…">` blocks in
`<project_context>` (first-loaded first):

1. `~/.tack/agent/AGENTS.md` — global rules (first matching candidate name)
2. `~/.tack/agent/rules/*.md` — **all** markdown files, sorted by filename
3. Ancestor scan, filesystem root → cwd (root-most first). Each directory
   contributes at most one file, first match in this order:
   `AGENTS.override.md` → `AGENTS.md` → `AGENTS.MD` → `CLAUDE.md` → `CLAUDE.MD`
4. `--add-dir <path>` / settings `additionalDirs`: one context file per extra
   directory (same candidate order; loaded unconditionally — the user
   explicitly added the dir)

**Trust gating**: the ancestor scan (3) only runs for trusted projects
(`project_trust`; see `/trust`) — a malicious clone must not inject rules
into your prompt. Global files (1–2) always load.

Notes: full content embedded, no truncation; dedup by path; cwd subdirectories
are **not** scanned; `--no-context-files` disables all of the above for one
run. TS pi's worktree shadowing rule is ported: when cwd is a
linked worktree nested inside its main checkout, the worktree's own context
file shadows the main repo's same-named file (the `agent/rules/` directory is
a Tack extension on top of the TS layout).

## Skills 加载顺序

Name collisions: **first loaded wins** (TS `resourcePrecedenceRank`:

project > user > standard locations):

1. `<cwd>/.pi/skills/` — project skills
2. `<cwd>/.agents/skills/` and each ancestor up to (including) the git root
   (the dir containing `.git`; home excluded from the walk)
3. `~/.tack/agent/skills/` — user skills
4. settings `skills` array — extra skill directories
5. CLI `--skill <path>` (repeatable) — ad-hoc extra directories

**Trust gating**: project-level dirs (1–2) load only for trusted projects
(a malicious clone must not inject skills); user dir (3) always loads.
`--no-skills` disables skill loading for one run.

Discovery rules per directory:

- A subdirectory containing `SKILL.md` is one skill; recursion stops there.
- Loose `*.md` files at the scan **root** are skills only with a non-empty
  `description` in frontmatter (name defaults to the parent dir name).
- Entries starting with `.` and `node_modules` are skipped.
- Identical real files (canonical path) are deduped silently.

Frontmatter contract (all other fields ignored, same as TS pi). The parser
handles flat `key: value` pairs plus `|`/`>` block scalars (with `-`/`+`
chomping) for multi-line values; nested maps/sequences are not supported:

```yaml
---
name: my-skill            # optional; defaults to parent dir name; [a-z0-9-], max 64
description: What it does # required; max 1024 chars
disable-model-invocation: true  # hide from <available_skills> (still callable)
---
```

Surfacing: skills are listed in `<available_skills>` in the system prompt
(model reads SKILL.md on demand) and are invocable as `/skill:<name> [args]`
in TUI / print / RPC (expanded to `<skill name location>…</skill>`).
`enableSkillCommands` (settings.json, default `true`) gates autocomplete only.

## MCP 配置顺序

Server entries merge **by name, project wins**:

1. `~/.tack/agent/mcp.json` — global servers
2. `<cwd>/.pi/mcp.json` — project servers (override same-named global entries)
3. ACP `session/new` `mcpServers` — client-provided, highest precedence
   (deduped by name, client wins)

Entry shapes:

```jsonc
{ "mcpServers": {
    "local":  { "command": "npx", "args": ["…"], "env": {}, "cwd": null },
    "remote": { "type": "http", "url": "http://host/mcp", "headers": {} }
} }
```

Connection failures are logged and skipped (never fail session creation).

## Settings 合并顺序

1. Defaults
2. `~/.tack/agent/settings.json`
3. `<cwd>/.pi/settings.json` (deep-merged on top, project wins)

Known keys: `defaultProvider`, `defaultModel`, `shellPath`, `theme`,
`tuiMode`, `scopedModels`, `enableSkillCommands`, `appendSystemPrompt`,
`compaction.{enabled,reserveTokens,keepRecentTokens}`,
`retry.{enabled,maxRetries,baseDelayMs}`. Unknown keys are preserved on rewrite.

## 其他资源目录

| Resource | Global | Project | Precedence |
|---|---|---|---|
| Prompt templates (`*.md`) | `~/.tack/agent/prompts/` | `<cwd>/.pi/prompts/` | project wins |
| Themes (`*.json`) | `~/.tack/agent/themes/` | `<cwd>/.pi/themes/` | project wins |
| Keybindings | `~/.tack/agent/keybindings.json` | — | single file |
| Models | `~/.tack/agent/models.json` | — | single file |
| Credentials | `~/.tack/agent/auth.json` | — | single file |
| Extensions | `~/.tack/agent/extensions/` | `<cwd>/.pi/extensions/` | both loaded; project trust-gated |
| Marketplaces | `~/.tack/agent/marketplaces/` | — | per-catalog files |
| Cron jobs | `~/.tack/agent/cron.json` | — | single file |
| Managed hooks | `~/.tack/agent/managed-hooks.json` | — | single file (enterprise) |
| Sessions | `~/.tack/agent/sessions/--<cwd>--/` | — | per-cwd |
