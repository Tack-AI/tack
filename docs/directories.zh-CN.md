# Tack 目录与加载顺序

**[English](directories.md) | 简体中文**

配置、rules、skills 与 MCP：Tack 查找它们的位置和顺序。
（一切都位于 `~/.tack/` 之下 —— 会话从不触碰 TS pi 的 `~/.pi/`。）

## 文件系统布局

```
~/.tack/
  agent/
    settings.json       # 全局设置（深合并，位于项目 .pi/settings.json 之下）
    auth.json           # API keys + OAuth 凭据（兼容 TS pi auth.json）
    models.json         # 自定义 providers（TS pi schema）
    keybindings.json    # 键位覆盖（TS pi schema）
    mcp.json            # 全局 MCP servers
    extensions/         # 已安装扩展（总是加载）
    marketplaces/       # 扩展市场目录（tack ext marketplace）
    cron.json           # 定时任务（/cron，错过的运行会在启动时触发一次）
    managed-hooks.json  # 组织管理的 hooks（配合 managedHooksOnly 强制）
    AGENTS.md           # 全局 rules（单文件，TS pi 布局）
    rules/              # 全局 rules 目录（*.md，全部加载，按文件名排序）
    skills/             # 用户 skills（优先级 3）
    memory/             # 持久记忆，用户作用域（MEMORY.md + *.md；根随 memoryDirectory / TACK_MEMORY_DIR 移动）
    memory/projects/<encoded-repo>/  # 项目作用域记忆（worktree 间共享）
    prompts/            # 用户 prompt 模板
    themes/             # 用户主题（<name>.json）
    sessions/
      --<encoded-cwd>--/*.jsonl   # 按目录划分的会话树

<project>/
  .pi/
    settings.json       # 项目设置（优先于全局）
    mcp.json            # 项目 MCP servers（同名冲突时胜出）
    AGENTS.md           # 项目 rules（通过祖先目录扫描）
    skills/             # 项目 skills（优先级 1）
    prompts/            # 项目 prompt 模板（同名冲突时胜出）
    themes/             # 项目主题（同名冲突时胜出）
    extensions/         # 项目扩展（经 /trust 信任门控）
  .agents/
    skills/             # 项目 skills（优先级 2，向上扫描祖先直至 git 根）
```

> `~/.agents/skills/`（跨工具的 Agent Skills 标准位置）**不会**被加载——
> pi→Tack 切割时已移除该用户级来源。

## Rules（上下文文件）加载顺序

上下文文件以 `<project_instructions path="…">` 块的形式注入
`<project_context>`（先加载的在前）：

1. `~/.tack/agent/AGENTS.md` —— 全局 rules（第一个匹配的候选名）
2. `~/.tack/agent/rules/*.md` —— **所有** markdown 文件，按文件名排序
3. 祖先扫描，文件系统根 → cwd（最靠根的先加载）。每个目录最多贡献
   一个文件，按此顺序取第一个匹配：
   `AGENTS.override.md` → `AGENTS.md` → `AGENTS.MD` → `CLAUDE.md` → `CLAUDE.MD`
4. `--add-dir <path>` / settings `additionalDirs`：每个额外目录一个
   上下文文件（候选顺序相同；无条件加载 —— 用户显式添加了该目录）

**信任门控**：祖先扫描（3）只对受信任的项目运行
（`project_trust`；见 `/trust`）—— 恶意克隆绝不允许向你的 prompt
注入 rules。全局文件（1–2）总是加载。

注：全文嵌入，不截断；按路径去重；cwd 的子目录**不会**被扫描；
`--no-context-files` 可在单次运行中禁用以上全部。TS pi 的 worktree
遮蔽规则已移植：当 cwd 是嵌套在其主 checkout 内的 linked worktree
时，worktree 自己的上下文文件会遮蔽主仓库的同名文件（`agent/rules/`
目录是 Tack 在 TS 布局之上的扩展）。

## Skills 加载顺序

名称冲突：**先加载者胜**（TS `resourcePrecedenceRank`：

project > user > 标准位置）：

1. `<cwd>/.pi/skills/` —— 项目 skills
2. `<cwd>/.agents/skills/` 以及向上直至（含）git 根的每个祖先目录
   （含 `.git` 的目录；家目录不参与该扫描）
3. `~/.tack/agent/skills/` —— 用户 skills
4. settings `skills` 数组 —— 额外的 skill 目录
5. CLI `--skill <path>`（可重复）—— 临时的额外目录

**信任门控**：项目级目录（1–2）只对受信任的项目加载（恶意克隆绝不
允许注入 skills）；用户目录（3）总是加载。
`--no-skills` 可在单次运行中禁用 skill 加载。

每个目录的发现规则：

- 含有 `SKILL.md` 的子目录是一个 skill；递归到此为止。
- 扫描**根**处的散装 `*.md` 文件，只有在 frontmatter 带有非空
  `description` 时才算 skill（名称默认为父目录名）。
- 以 `.` 开头的条目和 `node_modules` 会被跳过。
- 相同的真实文件（canonical 路径）会被静默去重。

Frontmatter 契约（其他字段一律忽略，与 TS pi 相同）。解析器处理扁平的
`key: value` 对，外加用于多行值的 `|`/`>` 块标量（带 `-`/`+`
chomping）；不支持嵌套 map/sequence：

```yaml
---
name: my-skill            # 可选；默认为父目录名；[a-z0-9-]，最长 64
description: What it does # 必填；最长 1024 字符
disable-model-invocation: true  # 从 <available_skills> 隐藏（仍可调用）
---
```

呈现方式：skills 列在 system prompt 的 `<available_skills>` 中（模型
按需读取 SKILL.md），并可在 TUI / print / RPC 中以
`/skill:<name> [args]` 调用（展开为 `<skill name location>…</skill>`）。
`enableSkillCommands`（settings.json，默认 `true`）仅控制自动补全。

## MCP 配置顺序

server 条目**按名称合并，项目优先**：

1. `~/.tack/agent/mcp.json` —— 全局 servers
2. `<cwd>/.pi/mcp.json` —— 项目 servers（覆盖同名全局条目）
3. ACP `session/new` 的 `mcpServers` —— 客户端提供，优先级最高
   （按名称去重，客户端胜出）

条目形状：

```jsonc
{ "mcpServers": {
    "local":  { "command": "npx", "args": ["…"], "env": {}, "cwd": null },
    "remote": { "type": "http", "url": "http://host/mcp", "headers": {} }
} }
```

连接失败会被记录并跳过（绝不让会话创建失败）。

## Settings 合并顺序

1. 默认值
2. `~/.tack/agent/settings.json`
3. `<cwd>/.pi/settings.json`（深合并覆盖在上层，项目优先）

已知 keys：`defaultProvider`、`defaultModel`、`shellPath`、`theme`、
`tuiMode`、`scopedModels`、`enableSkillCommands`、`appendSystemPrompt`、
`compaction.{enabled,reserveTokens,keepRecentTokens}`、
`retry.{enabled,maxRetries,baseDelayMs}`。未知 keys 在重写时保留。

## 其他资源目录

| 资源 | 全局 | 项目 | 优先级 |
|---|---|---|---|
| Prompt 模板（`*.md`） | `~/.tack/agent/prompts/` | `<cwd>/.pi/prompts/` | 项目胜出 |
| 主题（`*.json`） | `~/.tack/agent/themes/` | `<cwd>/.pi/themes/` | 项目胜出 |
| 键位 | `~/.tack/agent/keybindings.json` | — | 单文件 |
| 模型 | `~/.tack/agent/models.json` | — | 单文件 |
| 凭据 | `~/.tack/agent/auth.json` | — | 单文件 |
| 扩展 | `~/.tack/agent/extensions/` | `<cwd>/.pi/extensions/` | 两者都加载；项目级经信任门控 |
| 市场 | `~/.tack/agent/marketplaces/` | — | 每个目录（catalog）一个文件 |
| Cron 任务 | `~/.tack/agent/cron.json` | — | 单文件 |
| 受管 hooks | `~/.tack/agent/managed-hooks.json` | — | 单文件（企业版） |
| 会话 | `~/.tack/agent/sessions/--<cwd>--/` | — | 按 cwd 划分 |
