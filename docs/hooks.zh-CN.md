# Hooks（生命周期钩子）

**[English](hooks.md) | 简体中文**

Tack 的 hooks 是 **Claude Code 兼容** 的生命周期钩子：不写扩展，用
shell 命令或 LLM 评估在 agent 关键事件点介入。配置来源（按合并顺序）：

1. 托管 hooks：`~/.tack/agent/managed-hooks.json`（企业管理面）
2. 用户/项目 settings 的 `hooks.*`（全局 `~/.tack/agent/settings.json` +
   项目 `.pi/settings.json`，深度合并）
3. 扩展 bundle：`extension.json` 的 `hooks` 字段（见
   [extensions.md](extensions.zh-CN.md) §2）

`managedHooksOnly: true`（settings）时只保留托管 hooks（Codex 的
`allow_managed_hooks_only` 语义）。

## 事件

| 事件 | 时机 | matcher 匹配对象 | 裁决能力 |
|---|---|---|---|
| `PreToolUse` | 工具调用前 | 工具名 | block、`updatedInput` 改参数、`permissionDecision` |
| `PermissionRequest` | 即将弹权限框时 | 工具名 | `permissionDecision` 代替用户回答 |
| `PostToolUse` | 工具执行后 | 工具名 | block（原因反馈给模型）、`additionalContext` |
| `UserPromptSubmit` | 用户提交 prompt 后 | — | block（丢弃 prompt）、`additionalContext` |
| `SessionStart` | 会话创建 | — | `additionalContext` 注入系统提示 |
| `SessionEnd` | 会话退出 | — | fire-and-forget |
| `PreCompact` / `PostCompact` | 自动/手动压缩前后 | — | fire-and-forget |
| `Stop` | agent 一轮结束 | — | block → 以 reason 续跑（每停止点最多一次） |
| `SubagentStart` | 子 agent 启动前（worktree 创建/token 消耗之前） | 子 agent 名 | block（拒绝启动，工具返回错误）、`additionalContext`（追加进 task） |
| `SubagentStop` | 子 agent 完成 | 子 agent 名 | fire-and-forget |
| `Interrupt` | Esc 中断运行 | — | fire-and-forget |
| `Notification` | 权限框弹出等需要用户注意时 | — | fire-and-forget |

`SubagentStart` 的 payload 在公共字段外加：`agent_id`（自定义 agent 参数或
null）、`agent_type`（自定义 agent 名，默认 `"subagent"`）、`prompt`（task
前 2000 字符）、`description`、`isolation`（`"worktree"`）、`background`
（bool）。后台子代理（`run_in_background`）同步评估——block 时不会注册
注定失败的任务。

## 配置格式

Claude 嵌套格式（推荐）与 Tack 旧扁平格式（自动兼容）：

```json
"hooks": {
  "PreToolUse": [
    { "matcher": "bash|edit",
      "hooks": [
        { "type": "command", "command": "check.sh", "timeout": 30 },
        { "type": "prompt",  "prompt": "这个操作安全吗？只答 verdict JSON" },
        { "type": "agent",   "prompt": "检查该命令引用的文件是否存在风险" }
      ] }
  ],
  "SessionStart": [ { "command": "cat .pi/context.md" } ],
  "Stop":         [ { "hooks": [ { "type": "command", "command": "notify.sh", "async": true } ] } ]
}
```

handler 字段：

| 字段 | 说明 |
|---|---|
| `type` | `command`（默认）\| `prompt` \| `agent` |
| `command` | shell 命令（经 `$SHELL -c` 执行） |
| `timeout` | 秒，默认 60；超时 kill，不泄漏进程 |
| `async` | true = fire-and-forget（不等待、忽略裁决） |
| `prompt` | prompt/agent handler 的评估指令 |
| `model` | `provider/id`，缺省用会话模型 |

**matcher**：空/`*` 匹配全部；无正则元字符时按精确名匹配（`|` 多选）；
否则编译为正则（如 `mcp__.*`）。

## 命令 handler 协议

- **stdin**：一个 JSON 对象（snake_case）：`session_id, transcript_path,
  cwd, hook_event_name, model, permission_mode` + 事件字段
  （`tool_name, tool_input, tool_use_id` / `prompt` / `trigger` / …）。
  例外：Stop 事件的用量字段沿用上游 camelCase（`totalTokens`/`totalCost`）。
- **exit 0**：stdout 可为空，或一个 verdict JSON（宽松解析，未知字段忽略）：
  ```json
  {
    "decision": "block", "reason": "…",
    "hookSpecificOutput": {
      "permissionDecision": "allow | deny | ask",
      "permissionDecisionReason": "…",
      "updatedInput": { "command": "ls" },
      "additionalContext": "注入模型的上下文"
    },
    "systemMessage": "展示给用户",
    "continue": false, "stopReason": "…"
  }
  ```
- **exit 2**：block，stderr 为原因（PreToolUse 拦截工具；UserPromptSubmit
  丢弃 prompt；PostToolUse 作为错误反馈给模型）。
- **其他非零**：警告，不影响运行。

语义细则：

- `updatedInput` 是**部分合并**（覆盖在原参数上，不是整体替换）；改写后
  会重新做 schema 校验。
- `permissionDecision: allow` 跳过权限弹窗（declarative deny 规则仍优先）；
  `ask` 强制弹窗；`deny` 直接拦截。
- 多个 handler 的裁决合并：block 先到先得；permission 取最严
  （deny > ask > allow）；`additionalContext` 累加。
- hook 失败**永远 fail-open**：坏 hook 不会卡住 agent（仅日志 + 警告）。
- 命令 hook 的非 JSON stdout 按纯文本注入为 additionalContext（所有事件，
  兼容旧行为）。

## prompt / agent handler（LLM 评估）

不跑命令，让模型裁决：hook 输入 JSON 连同你的 `prompt` 指令发给模型，
要求只回答 verdict JSON（同上 schema）。`agent` 型额外允许多轮 + 只读
工具（read/grep/find/ls）先勘察工作区再回答。评估调用使用会话模型
（或 handler 的 `model` 覆盖），不经扩展事件通道。

```json
"PreToolUse": [
  { "matcher": "bash",
    "hooks": [ { "type": "prompt",
                 "prompt": "如果这个 bash 命令可能破坏用户数据，deny；不确定就 ask。" } ] }
]
```

## 相关设置

| 键 | 说明 |
|---|---|
| `features.shellHooks` | false 时 hooks 全部不执行 |
| `managedHooksOnly` | true 时只执行 managed-hooks.json |
| `hooks.*` | settings 内的 hooks 声明（全局 + 项目深合并） |
