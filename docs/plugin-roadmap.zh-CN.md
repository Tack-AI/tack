# 插件系统重设计与企业级路线图

**[English](plugin-roadmap.md) | 简体中文**

> 本文档是 tack 插件系统的**清零重设计**。前提（2026-05）：tack
> **没有用户、没有插件生态**——没有历史包袱需要保护。因此 v1/v2
> NDJSON 协议与平铺安装布局被明确视为**可抛弃的**：凡是有更好答案
> 的地方就直接替换，而不是叠加兼容层。被保留的是*架构骨架*（双载
> 体、声明式优先、hooks 链、fail-open 语义），加上从 Codex
> （`codex-rs/core-plugins`、`codex-rs/ext/extension-api`）借鉴的六个
> 管理面模式。
>
> 现状见 [plugin-system.zh-CN.md](plugin-system.zh-CN.md)。本文档是
> 目标形态；P0–P6 各阶段独立交付，落地时记录进
> [compatibility.zh-CN.md](compatibility.zh-CN.md)。

## 1. 目标与非目标

**目标，按优先级**

1. **开发体验（DX）**：hello-world 插件在 Rust、TypeScript、Python
   中都不超过 15 行，一条命令生成脚手架，不依赖 TUI 即可调试，
   不依赖模型即可在 CI 中测试。
2. **单一事实来源的类型安全**：协议只定义一次，以机器可读的
   schema 存在；宿主与 SDK 的类型全部生成。
3. **沙箱化分发**：WASM/组件载体是插件被*分享*的形态；进程载体是
   *开发*形态。
4. **企业级管理面**：可治理（托管策略）、可审计（结构化记录）、
   可观测（指标、doctor）、可靠（原子安装）。

**非目标**

- **生产会话热加载**。插件能力在会话启动时织入 agent loop。
  （*开发*回路可以随意重启插件进程——那按定义就是新会话。）
- 进程内动态加载（dylib/稳定 ABI）。
- **向后兼容 tack-ext 协议 v1/v2**。没有用户；维护翻译层的代价大
  于收益。旧协议随 v3 落地时移除（§10）。

## 2. 设计原则

1. **声明式优先，不得已才写代码**。护栏、上下文注入、MCP 工具、
   skills 都不需要插件进程。
2. **标准优于自研**。JSON-RPC 2.0 信封（到处有现成库和校验器）、
   MCP 承接纯工具型插件（白拿生态）、WIT 承接沙箱载体（白拿各语
   言绑定）。只有标准确实表达不了的语义（拦截、widget、会话控
   制）才用自定义表面。
3. **窄能力优于胖协议**。Codex `extension-api` 的关键经验：十五
   个 contributor trait 胜过一个四十方法的插件 trait。tack-RPC
   拆成独立的能力命名空间；插件逐项显式启用，不用的部分零成本。
4. **错误是数据**。加载失败的插件是加载结果里的
   `LoadedPlugin{enabled, error}`，不是一行日志。所有消费方按
   `is_active()` 过滤。
5. **宿主保留主权**。插件只*贡献*和*请求*；渲染、准入、策略、
   持久化由宿主决定。

## 3. Codex 的经验（管理面）

六个模式原样引入其精神；落点见 §8/§9：

1. `LoadedPlugin{enabled, error}` + `is_active()`——失败是一等状
   态（`plugin/src/load_outcome.rs`）。
2. 双段身份 `name@source`，分段字符白名单保证 ID 可安全用作路径
   （`core-plugin-common/src/plugin_id.rs`）。
3. 版本化不可变 store，激活由规则推导（`local` 优先，否则最高
   semver），安装走 staging → 复检 → rename 交换 → 回滚
   （`core-plugins/src/store.rs`）。
4. 策略是加载时对生效配置的过滤器，而非散落的调用点检查；用户
   层策略只能收窄（`core-plugins/src/marketplace_policy.rs`）。
5. 不可信进程的声明式遥测 sidecar：声明操作/维度、交给沙箱授权
   的文件、严格校验回收内容
   （`core-plugins/src/plugin_metrics_sidecar.rs`）。
6. 指纹幂等的多通道同步 + scrub 过的 git 环境
   （`core-plugins/src/startup_sync.rs`、`git_policy.rs`）。

## 4. 三级插件模型

插件作者只为其想法所需的机制付费：

```
Level 1   声明式 bundle           extension.json + hooks/MCP/skills 文件
          （无代码）              护栏、上下文、工具接线

Level 2   MCP server 插件         extension.json 声明一个 MCP server
          （标准生态）            带插件身份、策略、归因的工具贡献——
                                  零 tack 专属代码；任何现存 MCP server
                                  直接够格

Level 3   tack-RPC 插件           进程（开发）或 WASM 组件（分发），
          （全能力面）            讲 tack-RPC v3：拦截、生命周期、
                                  widget、会话控制、审批、配置、指标
                                  ——经官方 SDK
```

- **Level 1** 即今天的 bundle，原样保留。
- **Level 2** 把 MCP 变成一种*插件形态*而非仅仅是配置项：宿主拉
  起 server，把它的工具以插件身份接入插件模型（归因、策略、拦截
  一律生效），它的 resources/prompts 视为 bundle 贡献。只需要
  "给 agent 加工具"的作者永远不接触 tack-RPC。
- **Level 3** 承载 tack 专属语义——MCP 没有词汇表达的东西：在工
  具执行前拦截/改写、观察 agent loop、声明式 UI、会话控制、审批
  链。

## 5. tack-RPC v3

v1/v2 协议（自研信封、手写 peer、文档即规范）被替换为 schema 优
先、为 SDK 生成而设计的协议。

### 5.1 传输与分帧

- 进程载体：stdio 上的换行分隔 JSON（不变——每种语言处理
  NDJSON 都轻而易举，stdout 仍是唯一总线）。
- WASM 载体：WIT/组件模型调用（§6.3）；NDJSON-over-WASI-stdio
  仅保留为调试载体。
- 双方都可并发发起请求；id 各自编号（JSON-RPC 语义）。

### 5.2 信封：JSON-RPC 2.0

```json
{"jsonrpc":"2.0","id":7,"method":"tools/execute","params":{...}}
{"jsonrpc":"2.0","id":7,"result":{...}}
{"jsonrpc":"2.0","id":7,"error":{"code":-32001,"message":"denied by policy","data":{...}}}
{"jsonrpc":"2.0","method":"events/turnStart","params":{...}}
```

- 标准错误码 + 保留的领域码段：`-32001` policyDenied、
  `-32002` capabilityNotGranted、`-32003` pluginUnavailable、
  `-32004` requestTimeout。SDK 把它们映射为类型化错误。
- 取消：`$/cancelRequest`；长任务用 `$/progress` 配 work-done
  token 上报进度（LSP 惯例——写过 LSP/MCP peer 的人零学习成
  本）。
- 方法命名沿用 MCP：`<namespace>/<verb>`，verb 用 camelCase。

### 5.3 Initialize 与能力协商

以 MCP 的 initialize 为蓝本：

```json
→ {"method":"initialize","params":{
     "protocolVersion":"3.0.0",
     "host":{"name":"tack","version":"x.y.z"},
     "mode":"tui | print | rpc | acp",
     "cwd":"…","trusted":true,
     "capabilities":{"widgets":true,"sessionControl":true,
                     "snapshot":true,"metrics":{"scratchFile":"/…"}},
     "config":{ /* 已按插件的配置 schema 校验 */ }}}
← {"result":{
     "protocolVersion":"3.0.0",
     "plugin":{"name":"acme-review","version":"1.4.2"},
     "capabilities":{
       "tools":[{…}],"commands":[{…}],
       "hooks":{"beforeToolCall":true,"transformContext":false},
       "events":["turnStart","turnEnd"],
       "widgets":[{…}],"autocompleteProviders":[{…}],
       "config":{"schema":{…}},
       "metrics":{"operations":{…}} }}}
```

- 版本是 semver 字符串；双方声明自己支持的区间；不匹配是干净
  的握手错误，而不是静默降级。
- 宿主的 `capabilities` 块让插件事先知道模式相关的可用性（print
  模式没有 widget），而不是靠请求失败来发现。
- 插件的各项能力**全部可选且相互独立**——只做指标的插件只声明
  `metrics`。

### 5.4 方法命名空间

| 命名空间 | 方向 | 用途 |
|---|---|---|
| `initialize`、`shutdown` | 双向 | 生命周期握手 |
| `tools/execute` | 宿主 → 插件 | 执行贡献的工具 |
| `commands/invoke` | 宿主 → 插件 | 执行斜杠命令 |
| `hooks/beforeToolCall` | 宿主 → 插件 | allow / deny / **rewrite**（结构化裁决） |
| `hooks/transformContext` | 宿主 → 插件 | COW 上下文管道（opt-in） |
| `hooks/afterToolCall` | 宿主 → 插件 | 按字段合并的结果补丁（opt-in） |
| `events/*` | 宿主 → 插件 | 生命周期通知（订阅门控） |
| `widgets/update`、`widgets/action` | 双向 | 声明式 UI 状态推送 / 交互回报 |
| `autocomplete/provide` | 宿主 → 插件 | 输入行补全 |
| `approval/review` | 宿主 → 插件 | 审批链参与者，first-claim-wins |
| `session/get`、`session/sendUserMessage` 等 | 插件 → 宿主 | 会话控制（信任/模式门控） |
| `snapshot/get` | 插件 → 宿主 | 版本化只读会话摘要 |
| `config/get` | 插件 → 宿主 | 生效的插件配置 |
| `ui/notify`、`ui/select`、`ui/confirm`、`ui/input` | 插件 → 宿主 | 交互对话框（模式门控） |
| `exec/run` | 插件 → 宿主 | 在宿主执行 shell（信任门控） |
| `warnings/emit`、`logs/emit` | 插件 → 宿主 | 结构化的用户可见 / 诊断通道 |
| `host/registerProvider` | 插件 → 宿主 | 动态 LLM provider 桥 |

新增命名空间遵循 Codex contributor 列表的同一条规则：每个都是
小而有精确契约、独立版本化的表面，而不是往 god-object 上堆方法。

## 6. 一份 schema，三个 SDK

### 6.1 单一事实来源

`protocol/tack-rpc.openrpc.json`——一份描述全部方法、通知与类型
的 [OpenRPC](https://open-rpc.org) 文档。CI 强制：

- `tack-ext` 中生成的 Rust 类型是最新的（由 `xtask codegen`
  的新鲜度检查保证），
- 生成的 TS/Python SDK 类型是最新的，
- 协议参考文档是生成的，不是手写的。

手工维护的协议文档（目前"文档即规范"的漂移风险）随之消失。

### 6.2 SDK

- **Rust**（`tack-ext-sdk`）：在生成类型之上提供过程宏——

  ```rust
  #[tack::plugin(name = "acme-review")]
  impl Plugin for Acme {
      #[tool(description = "Review the current diff")]
      async fn review(&self, args: ReviewArgs, cx: &Cx) -> Result<ToolOutput> { … }

      #[hook]
      async fn before_tool_call(&self, call: &ToolCall) -> Verdict { Verdict::Allow }
  }
  ```

- **TypeScript**（`@tack/plugin`）：builder API，zod 风格 schema，

  ```ts
  export default plugin({
    name: "acme-review",
    tools: { review: tool({ description: "…", schema: … },
                          async (args, cx) => ({ text: "…" })) },
    hooks: { beforeToolCall: async (call) => allow() },
  });
  ```

- **Python**（`tack_plugin`）：装饰器 API，pydantic schema。

每个 SDK 负责分帧、id 关联、取消、超时、版本协商——插件作者永远
看不到信封。

### 6.3 WASM 载体：WIT，而非手写 WAT

沙箱载体从"WASI stdio 上的 NDJSON + 手写 WAT 参考插件"升级为
**组件模型 world**：

```wit
package tack:plugin@0.3.0;

interface tools { execute: func(call: tool-call) -> result<tool-output, string>; }
interface hooks { before-tool-call: func(call: tool-call) -> verdict; }
interface host  { /* config/get、snapshot/get、logs/emit、metrics … */ }

world plugin {
    import host;
    export tools;
    export hooks;   // 实践中每个接口都可经 world 变体做到可选
}
```

- guest 用 `wit-bindgen`（Rust、C、Go、JS、Python）——沙箱插件
  的 SDK 问题由工具链解决，而不是由我们解决。
- 宿主侧：wasmtime component linker；沿用现有的能力授权、
  fuel/内存/墙钟限制与审计日志。
- 手写 WAT 时代结束；`examples/` 换成 wit-bindgen guest。

## 7. 开发回路

工具链是协议职责的一部分：

| 命令 | 用途 |
|---|---|
| `tack ext new <dir> <rust|ts|python>` | 脚手架：清单、SDK 依赖、hello 工具、CI 测试 |
| `tack ext dev` | mock host：脚本化事件场景、REPL 直接调用 tools/hooks、监视文件变化并重启插件进程（开发回路重启，非会话热加载） |
| `tack ext test` | 用 fixture host 跑插件，断言 API（`expect_tool_call`、`feed_event`、`assert_widget`）——CI 友好，无模型、无网络 |
| `tack ext inspect` | 执行握手并以 JSON dump 声明的能力、配置 schema、指标 schema |

fixture host 就是喂脚本化输入的同一个 `PluginPeer`，插件测试走
的是端到端的真实协议。

## 8. 管理面（Codex 模式落地）

此处为浓缩版；模式见 §3。全部直接以最终形态落地——不建迁移层。

### 8.1 身份与加载结果

```text
plugin-id = name "@" source      name: [a-z0-9][a-z0-9.-]{0,63}（不允许 ".."）
                                 source: 市场名 | "user" | "project" | "local" | "mcp"
```

- `PluginLoadOutcome { plugins: Vec<LoadedPlugin>, warnings }`，其中
  `LoadedPlugin { id, enabled, error, capabilities }`，
  `is_active() = enabled && error.is_none()`。
- 能力级问题（一个坏 hooks 文件、一条格式错误的 MCP 条目）记
  warning 而非 error：插件照常加载，仅丢弃该项能力。
- 启用/禁用存于 settings `plugins."<id>".enabled`；CLI 为
  `tack ext enable|disable <id>`。被禁用的插件保留元数据、不拉起。
- `tack ext list` 展示 `ID / STATE / SOURCE / VERSION /
  CAPABILITIES`；失败与被策略拦截是列表里的行，而不是消失。

### 8.2 版本化 store、原子安装

```text
~/.tack/agent/extensions/
├── store/<source>/<name>/<version>/   # 激活 = 有 "local" 优先，否则最高 semver
└── data/<source>/<name>/              # 可写的插件数据根（WASM：preopen 到 /data）
```

每次变更：stage → 解析并重读 manifest 字节比对（防 TOCTOU）→
策略检查 → rename / 备份交换 + 回滚 → 写锁文件 → 清理被取代版
本。`tack ext upgrade [id]` 指纹幂等。git 在 scrub 环境下运行
（`GIT_TERMINAL_PROMPT=0`、移除继承的 `GIT_*`、系统 PATH、管道
stdio、硬超时）。

### 8.3 企业策略

托管 settings 层，**双重**拦截：

```jsonc
{
  "pluginPolicy": {
    "managedPluginsOnly": false,
    "allowedSources": [
      { "type": "git", "url": "https://git.acme.com/tack/plugins.git", "ref": "main" },
      { "type": "hostPattern", "pattern": "^(.+\\.)?acme\\.com$" },
      { "type": "local", "path": "/opt/acme/tack-ext" }
    ],
    "plugins": {
      "review@acme": { "enabled": true,
                       "mcpServers": ["jira"],       // 只收窄的交集
                       "tools": ["create_ticket"] }  // 只收窄的交集
    }
  }
}
```

1. 添加/安装时——在任何 clone 或网络访问之前；拒绝信息指明规
   则及其来源配置层。
2. 加载时——先生成过滤后的生效配置再交给 loader（让一切下游
   消费方天然合规的兜底）。

插件级 `tools`/`mcpServers` 只能缩小注册集合；托管层 `enabled`
胜过用户/项目层。每个决定都带规则与配置层来源记入审计日志。

## 9. 分发与可观测

- **策展市场启动同步**：settings 声明的目录经 git → https 归档降
  级保持新鲜；指纹短路；备份/rename/交换激活；跨进程锁；同步失
  败永不阻塞启动。
- **Bundle 归档**（`tack ext bundle pack`、`tack ext install
  <file.tgz>`）：tar.gz，敌意输入防护解包（拒绝链接、拒绝穿越、
  累计大小上限）——离线分发单元。
- **目录 v2**：条目增加 `installation: available | not-available
  | installed-by-default` 与内联 `manifest` 兜底（未物化也能丰富
  展示）；未知字段跳过记警告。ed25519 目录签名 + TOFU 密钥锁定
  保留。
- **指标 sidecar**：插件声明操作/维度枚举；宿主提供暂存文件
  （WASM：专用 preopen，记审计），回收时严格校验（每次 drain
  ≤64 KiB / ≤100 行、维度集合精确相等、枚举值合法、数值有限、
  去重）后带插件归因进入遥测。
- **加载遥测**：按结果计数（`active | disabled | failed |
  policy-filtered`），失败带错误类别（`manifest | handshake |
  register | policy | store`）。
- **Doctor**：`tack doctor` 报告插件健康——加载失败及原因、锁
  漂移、被策略过滤的条目、WASM 组件支持情况。

## 10. 兼容性立场

鉴于装机量为零：

- **tack-RPC v1/v2 随 v3 落地（同一版本）移除**。不建翻译层。
  `docs/extensions.md` 与 `docs/extensions-v2.md` 按 v3 重写；旧
  文本留在 git 历史里。
- **锁文件、store 布局、策略 settings 直接以最终形态落地**（不
  建 v1→v2 迁移路径）。
- 被刻意保留的：Claude 兼容的 shell hooks 格式（外部生态）、市
  场签名方案、项目信任语义——它们的价值超出 tack 自身。
- 每个阶段仍把存储/协议决定记录进
  [compatibility.zh-CN.md](compatibility.zh-CN.md)——硬约束靠设
  计满足，而不是靠兼容层。

## 11. 里程碑

| 阶段 | 交付物 | 依赖 |
|---|---|---|
| P0 | OpenRPC schema + 代码生成管线 + 生成的 Rust 类型 | — |
| P1 | tack-RPC v3 宿主核心（peer、initialize、命名空间）+ `tack-ext-sdk`（Rust） | P0 |
| P2 | TS + Python SDK；`ext new` / `ext dev` / `ext test` / `ext inspect` | P1 |
| P3 | 身份、加载结果、启用/禁用、版本化 store、原子安装/升级 | P1 |
| P4 | MCP server 插件（Level 2）；WIT/组件 WASM 载体 | P1 |
| P5 | 企业策略（白名单、加载时过滤、只收窄） | P3 |
| P6 | 分发（策展同步、bundle、目录 v2）+ 可观测（指标 sidecar、遥测、doctor） | P3、P5 |

P0–P2 是 DX 脊柱，最先交付——插件系统就是它的开发回路。P3–P6
把它做成企业级；它们的设计位于协议之上，不受协议替换影响。

## 12. 开放问题

1. **OpenRPC 成熟度**：若代码生成工具链太薄弱，退回到每方法一
   份 JSON Schema + 手写生成器（单一事实来源不变，少一点花哨）。
2. **WIT async**：wasmtime 组件模型 async 支持分阶段落地；若 P4
   时 guest async 未就绪，tools/hooks 先以同步形态发布（宿主保
   留 30s 调用超时），async 通过 world 版本升级引入。
3. **MCP elicitation ↔ `ui/*`**：Level 2 的 elicitation 是映射到
   Level 3 的对话框表面还是保持 MCP 原生；倾向映射（用户只有一
   条 UI 路径）。
4. **Widget/MCP 命名空间**：冲突保持先注册者胜记警告，还是 v3
   强制 ID 带插件前缀？没有包袱，前缀代价可承受——倾向 MCP
   server 名强制前缀、widget 不强制。
5. **子代理继承**：子代理会话继承完整插件集还是收窄集合
   （Codex `SessionIsolation`）；加载结果过滤器让两种做法都很便
   宜。需要产品决策。
6. **审批链范围**：`approval/review` 是 Codex
   `ApprovalReviewContributor` 的插件对应物；它与内置权限模式
   的组合方式（顺序、短路）需要一轮协议原型验证。
