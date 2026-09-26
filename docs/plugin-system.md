# Tack 插件系统

> 本文档是 Tack 插件系统的总体设计与现状说明（2026-05，P1–P4 落地后）。
> 协议细节分散在三份专题文档：[hooks.md](hooks.md)（生命周期钩子）、
> [extensions.md](extensions.md)（tack-ext 子进程/WASM 插件协议）、
> [extensions-v2.md](extensions-v2.md)（WASM 载体设计）。本文档把它们
> 拼成一张全景图，并说明设计取舍。

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
| **hooks 引擎** | settings/bundle 里声明的一次性命令或 LLM 评估 | 拦截、策略、注入、通知 | [hooks.md](hooks.md) |
| **tack-ext 插件** | 长驻子进程或 WASM 模块，NDJSON RPC | 长驻状态、自定义工具/命令、provider 桥、交互对话框 | [extensions.md](extensions.md) |
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

## 3. tack-ext 长驻插件（process + WASM 载体）

插件是独立进程（或 WASM 模块），stdio 上跑 NDJSON 协议（request /
response / event 三种 envelope，30s 调用超时，崩溃隔离）。

### 3.1 能力面

| 方向 | 方法/事件 |
|---|---|
| host → 插件 | `tool.execute`、`command.invoke`、`intercept.tool_call`（allow/deny/**rewrite**）、`intercept.context`（订阅 `"context"` 门控，可替换完整消息列表）、生命周期事件（session/agent/turn/message/tool_execution/model_select/provider 边界…） |
| 插件 → host | `ui.notify/select/confirm/input/set_status`（TUI 对话框）、`session.*`（new/switch/branch/set_model/send_user_message…）、`exec`（trust 门控）、`provider.register`（动态注册 LLM provider）、`log` |

### 3.1b 运行模式与 headless 降级

四种运行模式都加载插件（见 §6 矩阵）。非 TUI 模式（print/rpc/acp）用
headless HostServices：工具、拦截、生命周期事件、`exec`（trust 门控）
照常工作；需要终端 UI 的请求**确定性降级**——`ui.notify`/`ui.set_status`
进日志，`ui.select/confirm/input` 返回错误，`session.*`/`provider.register`
返回错误。插件从 `initialize.payload.mode` 得知宿主模式，不得把交互
请求当成正确性依赖。

### 3.2 两种载体

| | process（默认） | wasm |
|---|---|---|
| 插件形态 | 任意可执行文件 | WASI p1 模块（`.wasm`/`.wat`） |
| 协议 | NDJSON over stdio，握手 `protocol: 1` | **同一 schema**，握手 `protocol: 2` |
| 隔离 | 进程边界 | wasmtime 沙箱：无 fs/网络/环境变量 |
| 资源限制 | 无（trust 门控） | fuel + epoch 墙钟 + 内存硬上限（manifest `limits`） |
| 能力授予 | —（进程天然全权限） | manifest `capabilities` 显式声明：fs preopen（ro/rw）、env（字面量或 host 透传）、args、network 标志（p1 暂 inert）；加载时审计日志 |

WASM 载体复用同一个 `PluginPeer`（传输层抽象为 AsyncRead/AsyncWrite），
握手、超时、死插件 fail-fast 语义与子进程完全一致。示例：
[`examples/extensions/hello-wasm/`](../examples/extensions/hello-wasm/)
（手写 WAT 的协议参考实现）。

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
[extensions.md](extensions.md) §1b/§1c）。

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
| `examples/extensions/hello-js/` | 最小子进程插件（Node） |
| `examples/extensions/hello-wasm/` | **WASM 沙箱插件**（手写 WAT，协议参考） |
| `examples/extensions/hello-wasm-caps/` | **WASM 能力授予**（fs preopen 演示 readfile；无授予则 errno） |
| `examples/extensions/git-checkpoint/` | 事件 + exec + 命令 |
| `examples/extensions/protected-paths/` | tool_call 拦截 |
| `examples/extensions/handoff/` | 会话控制 |
| `tack ext-demo-plugin` | 内置最小协议实现（e2e fixture） |

## 8. 测试与现状

- hooks 引擎：20+ 个测试（配置双格式解析、Claude verdict 协议、matcher、
  exit-2/超时杀进程、updatedInput 合并、permissionDecision 记录与消费）
- tack-ext：40+ 个测试（含 intercept.context 替换/门控）
- WASM 载体（tack-ext-wasm）：9 个测试（握手 + tool.execute e2e、fuel/epoch
  墙钟/内存/表/实例上限拒绝、示例 WAT）+ ExtensionManager 加载 e2e
  （wasm 插件 + bundle 收集）
- tack-app 全量：230+ lib 测试 + 集成套件全绿

**明确未做**：

- 上游 TS pi 扩展的直接兼容（进程内 ExtensionAPI 与 NDJSON 协议是两种
  世界；可行的路径是 Node sidecar 扩展宿主，未立项）
- `SubagentStart` 事件发射、ACP 模式的 hooks 引擎接线（ACP 插件可用，
  但不跑 shell hooks）
- 热重载（插件工具/hooks 在会话启动时织入 agent loop，热替换成本远超
  收益；用 `ext install` + 重启代替）
- WASM 网络能力的实际生效（wasmtime-wasi p1 无 socket ABI，manifest
  标志仅面向未来）
