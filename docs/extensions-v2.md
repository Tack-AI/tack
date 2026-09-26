# tack-ext v2: WASM 沙箱载体 + 声明式 UI 组件协议（设计文档）

> 状态：v2.0 **已完成**（WASM 载体接入 `ExtensionManager`，`carrier/
> module/limits` manifest 生效，示例见 `examples/extensions/hello-wasm/`）。
> v2.1/v2.2 **已完成**（声明式 widget + autocomplete provider 的 host/TUI
> 侧集成，见 §3 与 docs/extensions.md §8）。v1 子进程协议见
> [extensions.md](extensions.md)，本文档只描述 v2 的增量。

## 1. v2 目标

v1 把插件定义为"任意可执行文件，stdio 上跑 NDJSON"，换来了语言无关和崩溃隔离，
但进程级信任粒度太粗：插件是完整进程，能读文件、开网络、执行任意命令，
只能靠项目信任（`/trust`）把门。v1 还明确排除了一类能力——自定义 widget、
autocomplete provider 等——因为它们需要**声明式组件协议**而非 RPC 调用。

v2 的两个目标正对应这两块短板：

1. **WASM 沙箱载体（v2.0）**：插件是一个 WASI 模块，跑在 wasmtime 里。
   **消息 schema 与 v1 完全相同**（同一套 envelope、握手、方法名），host 侧只是把
   "spawn 子进程 + pipe stdio" 换成 "实例化 wasmtime 模块 + WASI pipe"。
   默认全沙箱（无文件系统、无网络、无环境变量），资源有硬上限（fuel / epoch /
   内存），`exec` 等能力须经 host 函数显式授予。
2. **声明式 UI 组件协议（v2.1 / v2.2）**：插件在 `register` 里**声明** UI 组件
   （status line segment、markdown/list panel）与 autocomplete provider，
   由宿主 TUI 负责渲染和事件路由。插件不画像素，只产出结构化数据和更新事件。

载体与组件协议是正交的：声明式 UI 同时适用于子进程载体和 WASM 载体；
WASM 载体本身不要求组件协议。

## 2. WASM 载体协议（v2.0）

### 2.1 线协议：零改动

模块使用 WASI preview1 的 stdin/stdout，跑与 v1 **逐字节相同**的 NDJSON 协议：

```
host → guest(stdin)   {"type":"event","event":"initialize","payload":{"protocol":2,...}}
guest → host(stdout)  {"type":"event","event":"register","payload":{...}}
host → guest(stdin)   {"type":"request","id":1,"method":"tool.execute","params":{...}}
guest → host(stdout)  {"type":"response","id":1,"result":{...}}
```

- 握手：`initialize` → `register`，同 v1。
- `tool.execute` / `command.invoke` / `intercept.tool_call` / 生命周期事件：同 v1。
- `ui.*` / `session.*` / `provider.register` / `log`：同 v1（经 stdout 发请求，
  由 host 的 `HostServices` 处理）。
- stderr：仅用于日志，host 转发到 tracing（同 v1）。
- 单行 16MB 上限、请求超时、死插件 fail-fast：与 v1 完全一致——因为 host 侧
  复用的就是同一个 `PluginPeer`。

### 2.2 host 侧整合：传输层已天然抽象

v1 实现时 `PluginPeer` 就是传输无关的：构造函数接受任意
`AsyncRead + AsyncWrite`（子进程载体传入 child stdout/stdin，单测传入内存
duplex）。因此 **不需要抽新的 `PluginTransport` trait**：WASM 载体
（`crates/tack-ext-wasm` 的 `WasmCarrier::spawn`）只需：

1. 建两对 tokio duplex pipe；
2. 把 guest 端包成 `wasmtime_wasi::cli::AsyncStdinStream / AsyncStdoutStream`
   塞进 `WasiCtxBuilder`（WASI p1）；
3. 在后台 task 里 instantiate + 调 `_start`（async 模式，guest 阻塞读 stdin 时
   让出执行权）；
4. 把 host 端交给 `PluginPeer::new(stdout, stdin, services)`。

握手、request/response 匹配、超时、pending 清理、死插件快速失败全部复用。
guest 退出/trap → stdout 关闭 → peer 读泵收到 EOF → pending 调用立即失败，
语义与 v1 进程死亡一致。关机流程也一致：`shutdown` 事件 → 宽限 2s →
abort guest task（没有 OS 进程要 reap，比 v1 的 force kill 更干净）。

### 2.3 资源限制与沙箱边界

POC（`crates/tack-ext-wasm`）实现了全部三层限制：

| 机制 | wasmtime 配置 | 防什么 |
|---|---|---|
| **Fuel 计量** | `Config::consume_fuel(true)` + `store.set_fuel(n)` | CPU 耗尽（指令级预算，耗尽即 trap） |
| **Epoch 中断** | `Config::epoch_interruption(true)` + 引擎级 ticker（10ms 一跳）+ `store.set_epoch_deadline(k)` | 墙钟时间上限；在回边/调用点检查，能抓住纯计算死循环 |
| **内存上限** | `store.limiter(..)` + `StoreLimitsBuilder::memory_size(bytes)` | 线性内存膨胀；实例化（min pages）和 `memory.grow` 都受检 |

WASI 能力面（`WasiCtxBuilder` 默认值，POC 未放开任何一项）：

- 无 preopened 目录 → **无文件系统访问**；
- 无 args、无环境变量；
- TCP/UDP/域名解析全部默认拒绝 → **无网络**；
- 仅有的能力：stdin/stdout（协议管道）、stderr（日志）、时钟/随机数。

也就是说 **默认即全沙箱**：WASM 插件连 v1 插件"顺手"能做的事（读 cwd、看环境
变量里的 API key）都做不到。能力授予走显式路径：

| 能力 | 授予方式 | 状态 |
|---|---|---|
| `exec`（host 命令） | 协议方法，信任门控（同 v1），host 侧按 trust 决定 | 协议已支持 |
| 文件系统 | `preopened_dir(host_path, guest_path, FsPerms::ReadOnly/ReadWrite)`，按 manifest 声明 | v2.x 设计项 |
| 网络 | `allow_tcp/allow_udp/socket_addr_check` 白名单 | v2.x 设计项 |
| 宿主服务 | 协议 `ui.*`/`session.*`（不动 wasmtime linker 即可支持） | 协议已支持 |

### 2.4 与 v1 载体的对应表

| 维度 | v1 子进程载体 | v2 WASM 载体 |
|---|---|---|
| 插件形态 | 任意可执行文件 | WASI p1 core module（`.wasm`/`.wat`） |
| 线协议 | NDJSON over stdio pipes | **同一 schema**，NDJSON over WASI stdin/stdout |
| 握手 | initialize → register | 相同 |
| host 核心 | `PluginPeer`（AsyncRead+AsyncWrite） | **同一个 `PluginPeer`** |
| 隔离 | 进程边界（崩溃不伤 agent） | wasmtime 沙箱（trap 不伤 agent），无 OS 进程 |
| CPU/内存限制 | 无（信任门控代替） | fuel + epoch + memory 硬上限 |
| 文件系统/网络 | 完整进程权限 | 默认无；按声明显式授予 |
| `exec` | trust 门控 | trust 门控（协议层一致） |
| 生命周期 | shutdown 事件 → 2s → kill | shutdown 事件 → 2s → abort task |
| 语言生态 | 任意语言 | 能编到 wasm32-wasip1 的语言（Rust/C/Go(TinyGo)/JS(javy)/…） |

### 2.5 插件打包（extension.json 增量）

```json
{
  "name": "hello-wasm",
  "carrier": "wasm",
  "module": "plugin.wasm",
  "limits": { "maxFuel": 1000000000, "maxMemoryBytes": 268435456, "maxExecutionMs": null }
}
```

- `carrier: "process" | "wasm"`，缺省 `"process"`（向后兼容）。
- `module` 相对扩展目录解析（沿用 v1 path-like 参数规则）。
- `limits` 缺省用 `WasmLimits::default()`。

## 3. 声明式 UI 组件协议（v2.1）

v1 的 `ui.*` 是**命令式 RPC**（"弹个对话框，我等结果"）。组件协议反过来：插件
**声明**长期存在的 UI 单元，宿主 TUI 拥有渲染和布局，插件通过事件流推送状态
更新。这正好是 v1 明确排除的能力（custom widgets、status 区段）。

### 3.1 register 新增字段

```json
{"type":"event","event":"register","payload":{
  "name": "git-status",
  "tools": [],
  "widgets": [
    {"id": "branch", "type": "status_line_segment", "priority": 50,
     "initial": {"text": "main", "style": "dim"}},
    {"id": "diff-panel", "type": "markdown_panel", "title": "Pending diff",
     "visible": false},
    {"id": "files", "type": "list_panel", "title": "Changed files",
     "items": []}
  ]
}}
```

```jsonc
// WidgetSpec（camelCase，与 v1 payload 风格一致）
{
  "id": "branch",                    // 插件内唯一；host 侧键为 <plugin>:<id>
  "type": "status_line_segment"      // | "markdown_panel" | "list_panel"
          | "markdown_panel"
          | "list_panel",
  "priority": 50,                    // status_line_segment 排序，小在前
  "title": "Pending diff",           // panel 类必填
  "visible": true,                   // panel 初始可见性
  "initial": { /* 见 §3.2 各类型的 state */ }
}
```

### 3.2 更新事件流：`widget.update`

插件 → host 的**事件**（fire-and-forget，避免 UI 卡顿反向阻塞插件）：

```json
{"type":"event","event":"widget.update","payload":{
  "id": "branch",
  "state": {"text": "feature/wasm", "style": "info"},
  "visible": true
}}
```

各类型 state schema（TUI 渲染契约的输入）：

| 类型 | state | TUI 渲染契约 |
|---|---|---|
| `status_line_segment` | `{text, style?, tooltip?}`；`style` ∈ `default/info/warning/error/dim` | 状态栏一个区段，按 `priority` 排序；`text` 为空即隐藏；单行截断 |
| `markdown_panel` | `{markdown}` | 可切换面板，用现有 pulldown-cmark 渲染管线；宿主拥有滚动/焦点 |
| `list_panel` | `{items: [{id, label, detail?, icon?}], selectedId?}` | 列表面板；宿主渲染选择态；用户选中时发 `widget.action` |

host → 插件的用户交互回报（事件）：

```json
{"type":"event","event":"widget.action","payload":{
  "id": "files", "action": "select", "itemId": "src/main.rs"}}
```

渲染契约的硬性规则：

- **宿主拥有终端**：插件永远拿不到屏幕坐标、不能直接输出 ANSI；widget 之外
  的输出仍然只有 `ui.notify` 等 v1 原语。
- **更新是幂等的状态替换**（full-state snapshot，不是 diff），TUI 在主循环
  下一帧应用；丢帧无害。
- **插件死亡即组件消失**：peer EOF 时宿主注销该插件全部 widget，不留残影
  （与 v1"插件工具消失"语义一致）。
- **headless 模式**（print/rpc/acp）：widget 声明被接受但忽略，`widget.action`
  不产生；插件不得依赖 widget 做正确性判断。

### 3.3 autocomplete provider（v2.2）

```json
{"type":"event","event":"register","payload":{
  "autocompleteProviders": [
    {"id": "issues", "trigger": "#", "description": "GitHub issues"}
  ]
}}
```

输入行在任意位置出现 `trigger` 前缀的 token 时，host 发请求：

```json
{"type":"request","id":9,"method":"autocomplete.provide","params":{
  "providerId": "issues", "query": "wasm", "cursorOffset": 5}}
→ {"result":{"suggestions":[
     {"value": "#1234", "label": "#1234 WASM carrier", "detail": "open", "insertText": "#1234"}]}}
```

契约：

- 请求走普通 host→plugin request 通道（30s 超时不变；TUI 侧再加 300ms 的
  UI 级取消，超时/取消都静默降级为无建议）。
- `suggestions` 为空数组 = 无建议（合法）；`insertText` 缺省 = `value`。
- 一次输入可有多个 provider 命中，宿主按 register 顺序合并去重。
- v1 的扩展 slash command 补全不受影响（仍是 register 时静态已知）。

## 4. 版本协商（向后兼容）

- `initialize.payload.protocol` 升级为 `2`。**v2.1 起 host 对所有载体发
  `2`**（最初规划是仅 WASM 载体发 `2`、子进程载体保持 `1`；但 §3 的
  组件协议是载体正交的，子进程插件同样需要知道宿主支持 widget /
  autocomplete 才能声明。v1 插件对更高的版本号无感——`register` 解析
  双方均容忍未知字段，生态不受影响）。
- 插件声明组件能力（`widgets` / `autocompleteProviders`）前必须收到
  `protocol >= 2`；v1 host 收到含未知字段的 `register` payload 时按 serde
  惯例忽略未知字段（`RegisterPayload` 已有 `#[serde(default)]`，新增字段全部
  optional，旧 host 无感）。
- `widget.update` / `widget.action` / `autocomplete.provide` 对 v1 对端是未知
  envelope，协议规则（tolerate unknown fields/methods）保证互操作安全：
  v1 插件收到 `autocomplete.provide` 会回 error response，宿主降级为无建议。
- envelope 本身不加版本字段：版本只在握手时出现一次。

## 5. 分阶段路线图

| 阶段 | 内容 | 状态 |
|---|---|---|
| **v2.0 WASM 载体** | `crates/tack-ext-wasm`：wasmtime + WASI p1，复用 `PluginPeer`；fuel/epoch/内存三层限制；默认全沙箱；`carrier: "wasm"` manifest；e2e 跑通握手 + tool.execute | **已完成**：`ExtensionManager` 按 manifest 选载体（protocol 2 握手）、`limits` 配置化、`hello-wasm` 示例 + e2e 测试 |
| **v2.1 声明式 widget** | `WidgetSpec` + `widget.update`/`widget.action`；TUI 渲染三种组件；协议版本 2 | **已完成**：host 侧 `WidgetRegistry`（键 `<plugin>:<id>`，full-state 替换，插件死亡即清除）；`widget.update` 经 AppEvent 路由进 TUI 主循环；status_line_segment（priority 排序、空文本隐藏）/ markdown_panel（pulldown-cmark 管线，宿主管滚动）/ list_panel（键盘选择 → `widget.action` 定向回报 owning 插件）；按键 `ctrl+b`（面板显隐）/ `alt+p`（焦点循环）；示例见 `hello-js` |
| **v2.2 autocomplete provider** | `autocompleteProviders` + `autocomplete.provide`；TUI 输入行集成 | **已完成**：trigger 前缀 token 触发，复用现有补全 popup；多 provider 按 register 顺序合并去重；30s 协议超时 + 300ms UI 级超时静默降级；`insertText ?? value` 插入 |
| v2.x 能力授予 | preopened_dir / 网络白名单的 manifest 声明与 UI 确认流 | 设计项 |

v2.0 选择"WASM 载体先行"的原因：沙箱是信任模型的根基，组件协议（v2.1/2.2）
会让插件更深地嵌入 UI 主循环，先把不可信代码的运行边界收紧，再扩大它的
表达面。

## 6. POC 能力边界（`crates/tack-ext-wasm` 现状）

已验证（单测，macOS）：

- WASI p1 模块（`.wat` 文本 / `.wasm` 二进制）经 wasmtime 实例化，
  stdin/stdout 接 tokio duplex pipe；
- v1 握手（initialize → register）与 `tool.execute` 请求/响应经
  **未修改的 `PluginPeer`** 跑通；
- fuel 耗尽 trap、epoch 墙钟 deadline trap、内存上限拒绝实例化；
- stderr 转发 host 日志；guest 退出/trap → peer 死亡 → pending 调用快速失败。

明确不做（后续阶段）：

- 尚未接入 `ExtensionManager` / manifest（POC 是独立 crate 的 API）；
- 无 fs/网络/exec 的能力授予路径（`exec` 走协议层 trust 门控，理论可用但
  POC 未配 `HostServices` 端到端演示）；
- WASI p2/component model 未采用（p1 生态 tooling 最成熟；p1 ctx 建在 p2
  实现之上，未来迁移成本低）；
- 无热重载（v1 同为 deferred）。
