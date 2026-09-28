# Tack 插件系统

**[English](plugin-system.md) | 简体中文**

> 本文档是 Tack 插件系统的总体设计与现状说明（2026-05，P1–P4 落地后）。
> 协议细节分散在三份专题文档：[hooks.md](hooks.zh-CN.md)（生命周期钩子）、
> [extensions.md](extensions.zh-CN.md)（tack-ext 子进程/WASM 插件协议）、
> [extensions-v2.md](extensions-v2.zh-CN.md)（WASM 载体设计）。本文档把它们
> 拼成一张全景图，并说明设计取舍。清零重设计方案——DX 优先的三级
> 插件模型、schema 生成的 tack-RPC v3，以及企业级管理面——见
> [plugin-roadmap.md](plugin-roadmap.zh-CN.md)。

## 1. 架构：一个内核，三个入口

```
                ┌──────────────── tack 内核 ────────────────┐
                │  tack-agent-core::AgentHooks（事件 + 突变点） │
                │  before/after_tool_call · transform_context │
                │  链式组合（HooksChain）· 全部 fail-open     │
                └──────┬───────────────┬──────────────┬──────┘
                       │               │              │
              ┌────────┴───────┐ ┌─────┴───────┐ ┌────┴─────────┐
              │ hooks 引擎      │ │ tack-ext RPC  │ │ 内置 hook    │
              │ Claude 兼容     │ │ 长驻插件     │ │（权限/压缩/  │
              │ 声明式一次性命令│ │ process/wasm│ │ 预算/队列）  │
              └────────────────┘ └─────────────┘ └──────────────┘
```

设计取自两个被验证过的样本：

- **内核学 pi（TS 上游）**：`AgentHooks` trait 是带突变点的扩展总线——
  工具调用可拦截/改写、上下文可变换、结果可修补。所有扩展形态都只是
  这个内核的前端。
- **入口学 Codex**：最高频的插件需求（guardrail、策略、上下文注入）
  用**声明式 Claude Code 兼容 hooks** 覆盖，一次性命令 + JSON
  stdin/stdout，不需要长驻进程，且直接兼容 Claude Code 的 hooks 生态。

| 层 | 形态 | 适合场景 | 文档 |
|---|---|---|---|
| **hooks 引擎** | settings/bundle 里声明的一次性命令或 LLM 评估 | 拦截、策略、注入、通知 | [hooks.md](hooks.zh-CN.md) |
| **tack-ext 插件** | 长驻子进程或 WASM 模块，NDJSON RPC | 长驻状态、自定义工具/命令、provider 桥、交互对话框 | [extensions.md](extensions.zh-CN.md) |
| **内置 hooks** | Rust 实现（权限、压缩、预算、队列） | 核心行为 | — |

## 2. hooks 引擎（Claude Code 兼容）

**配置即插件**：不需要写扩展代码，在 settings 里声明即可。

### 2.1 配置来源与合并

按序合并，后面的追加在前：

1. **托管 hooks**：`~/.tack/agent/managed-hooks.json`（企业管理面；
   `managedHooksOnly: true` 时**只**保留它——Codex 的
   `allow_managed_hooks_only` 语义）
2. **settings `hooks.*`**：全局 `~/.tack/agent/settings.json` + 项目
   `.pi/settings.json` 深度合并
3. **扩展 bundle**：`extension.json` 的 `hooks` 字段指向的 hooks.json

`features.shellHooks: false` 全部禁用。

### 2.2 事件与裁决能力

| 事件 | 时机 | 裁决 |
|---|---|---|
| `PreToolUse` | 工具调用前 | block / `updatedInput` 改参数（**部分合并** + 重新 schema 校验）/ `permissionDecision`（allow 跳过弹窗、ask 强制弹窗、deny 拦截） |
| `PermissionRequest` | 即将弹权限框 | `permissionDecision` 代替用户回答 |
| `PostToolUse` | 工具执行后 | block（原因作为错误反馈给模型）/ `additionalContext` |
| `UserPromptSubmit` | 用户提交后 | block 丢弃 prompt / `additionalContext` 注入本轮 |
| `SessionStart` | 会话创建 | `additionalContext` 注入系统提示（非 JSON stdout 按纯文本注入，兼容旧行为） |
| `SessionEnd` / `PreCompact` / `PostCompact` / `SubagentStop` / `Interrupt` / `Notification` | 各生命周期点 | fire-and-forget |
| `Stop` | agent 一轮结束 | block → 以 reason 续跑（每停止点最多一次，`stop_hook_active` 防循环） |

`SubagentStart` 可解析、暂未发射。

### 2.3 handler 三种类型

```json
{ "matcher": "bash|edit",
  "hooks": [
    { "type": "command", "command": "check.sh", "timeout": 30, "async": false },
    { "type": "prompt",  "prompt": "这个操作安全吗？", "model": "openai/gpt-5-mini" },
    { "type": "agent",   "prompt": "检查该命令引用的路径风险" }
  ] }
```

- **command**：`$SHELL -c` 执行，hook 输入 JSON 走 stdin，verdict JSON 走
  stdout；exit 2 = block（stderr 为原因）；超时 kill 不泄漏进程。
- **prompt**：把 hook 输入连同指令发给 LLM，模型只回答 verdict JSON——
  零代码写智能 guardrail（"不确定就 ask"）。
- **agent**：同 prompt，但允许多轮 + 只读工具（read/grep/find/ls）先
  勘察工作区再裁决。

LLM 评估经独立的 provider 适配器（不经扩展事件通道，不对插件可见），
默认用会话模型，`model` 字段可覆盖。

### 2.4 语义保证

- matcher：空/`*` 全匹配；无元字符按精确名（`|` 多选）；否则正则。
- 多 handler 合并：block 先到先得；permission 取最严（deny>ask>allow）；
  `additionalContext` 累加。
- **一切 fail-open**：hook 失败/超时/坏 JSON 只产生警告，永不卡住 agent。

## 3. tack-RPC v3 长驻插件（process + WASM 载体）

插件是讲 **tack-RPC v3** 的独立进程（或 WASM 模块）：NDJSON stdio 上
的 JSON-RPC 2.0（双向并发请求、`$/cancelRequest`、30s 调用超时、崩
溃隔离）。协议的单一事实来源是
[`protocol/tack-rpc.openrpc.json`](../protocol/tack-rpc.openrpc.json)；
宿主类型（`tack_ext::rpc3`）与 TypeScript、Python SDK 的类型都从它
生成，三个 SDK 共享一份 schema。

### 3.1 能力面

插件在 `initialize` 握手时声明**相互独立、全部可选**的能力；未声明
的能力宿主从不调用（必须应答时返回 `ERR_CAPABILITY_NOT_GRANTED`）。

| 方向 | 方法/通知 |
|---|---|
| 宿主 → 插件 | `tools/execute`、`commands/invoke`、`hooks/beforeToolCall`（allow/deny/**rewrite**）、`hooks/transformContext`（整体替换上下文）、`hooks/afterToolCall`（按字段结果补丁）、`approval/review`（审批链）、`autocomplete/provide`、`events/lifecycle`（订阅门控）、`widgets/action` |
| 插件 → 宿主 | `ui/notify/select/confirm/input`（TUI 对话框）、`session/get`、`session/sendUserMessage`、`snapshot/get`（只读摘要）、`config/get`、`exec/run`（信任门控）、`host/registerProvider`（LLM provider 桥）、`widgets/update`、`logs/emit`、`warnings/emit` |

SDK 覆盖 Rust（`tack-ext-sdk`）、TypeScript（`@tack/plugin`）、
Python（`tack-plugin`）；`tack ext new` 生成任一脚手架，
`tack ext dev`/`ext test` 用 mock-host 场景文件驱动插件，无需会话。

### 3.1b 运行模式与无头降级

四种运行模式都加载插件（矩阵见 §6）。非 TUI 模式（print/rpc/acp）
确定性降级：工具、拦截、生命周期事件、`exec/run`（信任门控）照常；
`ui/select|confirm|input` 返回 `ERR_CAPABILITY_NOT_GRANTED`；
`session/*` 与 `host/registerProvider` 返回
`ERR_METHOD_NOT_FOUND`；`ui/notify` 进日志。插件从 initialize
payload 的 `mode` 与 `capabilities` 获知当前模式与可用表面。

### 3.2 身份、加载结果与 store

每个插件有稳定的 id **`name@source`**（市场名，或保留的
`user`/`project`/`local` 来源）。安装进入版本化 store
（`extensions/store/<source>/<name>/<version>/`；激活 = 有 `local`
优先，否则最高 semver），安装/升级原子 staging/交换可回滚，锁文件
v2 锁定并做漂移检查。`plugins."<id>".enabled` 不卸载即可禁用；
`tack ext enable|disable|upgrade|list` 负责管理。

加载失败是**一等状态**：manager 把每个发现的插件以
`LoadedPlugin { id, enabled, error, … }` 返回，所有消费方按
`is_active()` 过滤——坏插件出现在 `ext list`/doctor 中，而不是带
着一行日志消失（Codex 的 `PluginLoadOutcome` 模式）。

### 3.3 两种载体

| | process（默认） | wasm |
|---|---|---|
| 插件形态 | 任意可执行体 | WASI p1 模块（`.wasm`/`.wat`） |
| 协议 | **stdio 上的 tack-RPC v3**（同一 schema） | 相同 |
| 隔离 | 进程边界 | wasmtime 沙箱：无文件系统/网络/环境变量 |
| 资源限制 | 无（信任门控） | fuel + epoch 墙钟 + 内存硬上限（manifest `limits`，宿主钳制） |
| 能力授权 | —（进程天然全权限） | manifest `capabilities` 显式声明：fs preopen（ro/rw）、env（字面量或宿主透传）、args；加载时记审计日志 |

两种载体共享同一个 `JsonRpcPeer`（传输层抽象为
AsyncRead/AsyncWrite）：握手、超时、取消、死插件 fail-fast 语义完全
一致。示例：[`examples/extensions/hello-wasm/`](../examples/extensions/hello-wasm/)
（手写 WAT 的 v3 协议参考实现）。

## 4. 分发：bundle + marketplace

### 4.1 extension.json 全字段

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

后三个是**bundle 字段**：一个扩展目录可以同时贡献声明式资源（hooks
并入会话 hook 配置、MCP 服务器并入连接、技能目录并入发现），不需要
运行插件进程——**bundle-only 清单（无 command/module）是合法的**。
这对应 Codex 的"插件 = 声明式数据包"模型。

### 4.2 安装与 marketplace

```sh
tack ext install <git-url>[#<ref>] | <dir>   # 直接安装；#ref = tag/branch/commit
tack ext list / remove <name>
tack ext verify                              # 对照 lockfile 校验安装未被篡改

tack ext marketplace add acme <file|url> [--public-key <hex>]  # 注册目录（签名 catalog 需公钥，TOFU pin）
tack ext marketplace list [acme]
tack ext install <plugin>@acme            # 经目录解析安装（重验签名）
tack ext marketplace remove acme
```

安装即 pin：git 安装的 commit 写入 `~/.tack/agent/extensions-lock.json`，
启动时校验 HEAD 不一致默认**跳过加载**（`extensionLockRequired: false`
退化为仅警告）。marketplace catalog 支持 ed25519 签名（详见
[extensions.md](extensions.zh-CN.md) §1b/§1c）。

发现路径：`~/.tack/agent/extensions/*`（总是加载）→ settings
`extensionPaths` → `<project>/.pi/extensions/*`（**项目信任门控**）。

## 5. 安全模型

- **进程/WASM 隔离**：插件崩溃不伤 agent；死插件的 pending 调用立即失败。
- **项目信任**（`/trust`）：项目本地扩展与 `.pi/settings.json` 的 hooks
  需信任后才生效；`exec` 方法 trust 门控。
- **declarative deny 优先**：`permissions.deny` 规则压倒一切（包括 hook 的
  permissionDecision allow 和 bypass 模式）。
- **WASM 默认全沙箱**：无 preopened 目录、无网络、无环境变量；能力授予
  是显式路径（fs/网络白名单为 v2.x 设计项）。
- **managed hooks**：企业可锁定只执行托管 hooks。
- 裁决链顺序：SessionHooks → **生命周期 hooks** → 权限 hooks →
  队列/预算 → tack-ext 插件（最后看到的就是最终参数）。

## 6. 运行模式支持矩阵

| 能力 | TUI | print | rpc | acp |
|---|---|---|---|---|
| hooks 引擎（全部事件） | ✓ | PreToolUse/PostToolUse/UserPromptSubmit/Compact/SubagentStop | PreToolUse/PostToolUse | — |
| prompt/agent handler（LLM 评估） | ✓ | ✓ | ✓ | — |
| tack-ext 插件（process + wasm） | ✓ | ✓（headless 降级） | ✓（headless 降级） | ✓（headless 降级） |
| bundle 资源（hooks/mcp/skills） | ✓ | ✓ | ✓ | mcp |
| 声明式 widget / autocomplete | ✓ | —（声明被接受但忽略） | — | — |

## 7. 示例

| 示例 | 演示 |
|---|---|
| `crates/tack-ext-sdk/examples/hello_rpc3.rs` | 最小 Rust SDK 插件 |
| `tack-v3-demo-plugin`（tack-ext-sdk bin） | 全功能 fixture：工具、命令、hooks、事件、widget、自动补全 |
| `sdk/typescript` / `sdk/python` | TS/Python SDK 包及其 e2e 测试 |
| `examples/extensions/hello-wasm/` | **WASM 沙箱插件**（手写 WAT，v3 协议参考） |
| `examples/extensions/hello-wasm-caps/` | **WASM 能力授予**（fs preopen 演示 readfile；无授予则 errno） |
| `tack ext new <dir> <rust|ts|python>` | 带起步场景的脚手架 |

## 8. 测试与现状

- hooks 引擎：20+ 个测试（配置双格式解析、Claude verdict 协议、matcher、
  exit-2/超时杀进程、updatedInput 合并、permissionDecision 记录与消费）
- tack-RPC v3（`tack_ext::v3` + `tack-ext-sdk`）：peer 往返、取消、超时、
  死插件 fail-fast，12 个 SDK e2e（握手、verdict、能力门控、宿主服务、
  关闭）+ TS（7）与 Python（8）SDK e2e
- WASM 载体（tack-ext-wasm）：20 个测试（v3 握手 + tools/execute e2e、
  fuel/epoch/内存/表/实例上限、能力授权、示例 WAT）
- 扩展宿主（tack-app）：身份/发现、store 布局、原子安装/升级/校验、
  锁文件 v2、启用/禁用、市场签名、加载结果（失败/禁用插件）、widget
  注册表、bundle 资源、wasm e2e
- tack-app 全量：410+ lib 测试 + 集成套件全绿

**明确未做**：

- 上游 TS pi 扩展的直接兼容（进程内 ExtensionAPI 与 RPC 协议是两种
  世界；可行的路径是 Node sidecar 扩展宿主，未立项）
- `SubagentStart` 事件发射、ACP 模式的 hooks 引擎接线（ACP 插件可用，
  但不跑 shell hooks）
- 热重载（插件工具/hooks 在会话启动时织入 agent loop，热替换成本远超
  收益；用 `ext install` + 重启代替）
- WIT/组件 WASM 载体与 MCP server 插件（路线图 P4）、企业策略（P5）、
  指标 sidecar 与分发同步（P6）
