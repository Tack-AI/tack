# tack-ext：动态扩展（子进程插件）

**[English](extensions.md) | 简体中文**

Tack 以**子进程插件**的形式运行动态扩展：任何能在 stdio 上按换行分隔
JSON（NDJSON）通信的可执行文件都可以。每个插件一个进程，与 agent 循环
崩溃隔离。与语言无关 —— 插件可以用 JavaScript、Python、Rust、Go，或
任何能读 stdin、写 stdout 的语言编写。

本文档涵盖安装、线上协议、插件 API 与安全模型。参考实现：

- [`examples/extensions/hello-js/plugin.js`](../examples/extensions/hello-js/plugin.js) —— Node.js
- `tack ext-demo-plugin` —— 内置的最小实现（同时也是 e2e 测试夹具）
- [`crates/tack-ext/src/protocol.rs`](../crates/tack-ext/src/protocol.rs) —— 权威类型定义

---

## 1. 架构

```
┌──────────── tack (host) ────────────┐         ┌──── plugin process ────┐
│  ExtensionManager                    │  NDJSON │                        │
│   ├─ PluginProcess (stdin/stdout) ───┼─────────┼─► your code            │
│   ├─ ExtTool ──► agent loop tools    │◄────────┼─  tool.execute results │
│   ├─ ExtHooks ──► tool_call intercept│         │  ui.* requests         │
│   └─ TuiExtServices ──► dialogs      │         │  exec / log            │
└──────────────────────────────────────┘         └────────────────────────┘
```

- **每个插件一个进程**，启动时拉起，`kill_on_drop`，stderr 转发到
  宿主日志。
- **双方并发发起请求**；响应通过 `id` 匹配（每个发送方各自的计数器）。
- **超时**：宿主→插件调用 30 秒（拦截、工具执行），插件故障是
  *fail-open* —— 插件死掉或变慢绝不会卡住 agent。
- **生命周期**：退出时发送 `shutdown` 事件 → 2 秒宽限期 → 强制杀死。

同一套消息模式也设计为可在未来服务于 WASM 载体（沙箱化、能力受限），
无需改动协议。

## 1b. 插件市场（Marketplace）

插件市场是一个注册在 `~/.tack/agent/marketplaces/` 下的命名 JSON 目录
（`{"name": ..., "plugins": {"<name>":
{"source": "<git-url|path>", "description": ..., "rev": "<ref>"}}}`）：

```sh
tack ext marketplace add acme ./acme-marketplace.json   # 或一个 https URL
tack ext marketplace list                               # 已注册的目录
tack ext marketplace list acme                          # 某个目录中的插件
tack ext install code-review@acme                       # 按 spec 安装
tack ext marketplace remove acme
```

`tack ext install <git-url|dir>` 仍然可以直接使用；
`<plugin>@<marketplace>` 形式通过目录解析来源，并安装在市场键下。
目录条目中可选的 `"rev"` 将插件钉定到某个 git ref
（tag/分支/commit sha），与显式的 `#<ref>` 后缀（见下文）完全等价；
命令行上显式的 `#<ref>` 优先。

### 插件市场签名（ed25519，TOFU 密钥钉定）

目录可以携带一个顶层签名：

```json
{
  "name": "acme",
  "plugins": { "...": {} },
  "signature": { "algorithm": "ed25519", "value": "<128 hex chars>" }
}
```

被签名的载荷是**规范化目录**：移除顶层 `signature` 键后的目录 JSON，
用 `serde_json::to_string` 重新序列化 —— 这就是全部的规范化规则。
因为 Tack 构建 serde_json 时未启用 `preserve_order`，对象键按
BTreeMap 排序（字典序），所以规范化形式是确定性的，与原始文件的
键顺序和空白符无关。

注册带签名的目录需要提供签名者的公钥：

```sh
tack ext marketplace add acme https://acme.com/marketplace.json --public-key <64 hex chars>
```

第一个验证通过的密钥会被**钉定**到
`~/.tack/agent/marketplaces/acme.key`（首次使用信任，TOFU）。之后每次
`tack ext install <plugin>@acme` 都会用钉定的密钥重新验证已注册的
目录；被篡改或重新签名的目录会被拒绝。重新注册同一个市场时，若省略
`--public-key` 则使用已钉定的密钥。未签名的目录照旧注册，但会警告
安装不受完整性保护。

## 1c. Ref 钉定、lockfile 与 verify

Git 安装接受可选的 ref 后缀：

```sh
tack ext install https://github.com/acme/tack-ext-review.git#v1.2.0   # tag/分支/sha
```

ref 可以是 tag、分支，或完整/缩写的 commit sha；Tack 克隆后执行
`git checkout <ref>`（钉定的安装是完整克隆，未钉定的保持
`--depth 1`）。本地目录安装不支持 ref。

每次用户目录安装都会记录在 `~/.tack/agent/extensions-lock.json` 中：

```json
{
  "version": 1,
  "plugins": {
    "tack-ext-review": {
      "source": "https://github.com/acme/tack-ext-review.git",
      "rev": "v1.2.0",
      "resolvedCommit": "<40-char sha HEAD resolved to>",
      "installedAt": 1735689600,
      "marketplace": "acme"
    }
  }
}
```

`rev`/`marketplace` 缺省时为 `null`；本地目录安装记录
`resolvedCommit: null`。`ext remove` 会删除对应条目。项目本地
（`--local`）安装已经有信任门控，因此不进入 lockfile。Git 安装
保留其 `.git` 目录，以便审计检出内容。

`tack ext verify` 将每个已锁定插件的 `git rev-parse HEAD` 与
`resolvedCommit` 对比，并按插件打印 `ok` / `changed` /
`not-a-git-repo` / `missing`；只要有插件发生变化或缺失，就以非零
状态退出。同样的检查也会在启动时运行：检出内容与 lock 条目漂移的
用户目录插件默认会被**跳过**（并给出警告）—— 在设置中将
`"extensionLockRequired": false` 可将不匹配降级为警告并照常加载。
没有 lock 条目的插件（手动拷贝的目录、本地目录安装）永不受门控。
托管设置（managed settings）可以沿任一方向强制
`extensionLockRequired`。


## 2. 安装与发现

扩展是一个**包含 `extension.json` 的目录**：

```json
{
  "name": "hello-js",
  "command": "node",
  "args": ["plugin.js"],
  "env": { "MY_VAR": "1" }
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | 是 | 用于工具名（`ext__<name>__<tool>`）和日志。 |
| `command` | process 载体必填 | 要拉起的可执行文件（必须在 PATH 上或使用绝对路径）。 |
| `args` | 否 | 参数。**形似路径的参数**（包含 `/`、`\`，或以 `.` 开头）会相对于扩展目录解析；普通单词（子命令、标志）原样透传。 |
| `env` | 否 | 传给插件进程的额外环境变量。 |
| `carrier` | 否 | `"process"`（默认）\| `"wasm"` —— 将插件作为 WASI 模块运行在 wasmtime 沙箱中（同一套 NDJSON 协议，`protocol: 2` 握手）。 |
| `module` | wasm 载体必填 | `.wasm`/`.wat` 文件，相对于扩展目录解析。 |
| `limits` | 否 | WASM 沙箱限制：`{maxFuel, maxMemoryBytes, maxExecutionMs}`（默认 1e9 fuel、256MB、无 wall-clock 上限）。 |
| `capabilities` | 否 | WASM 能力授权（默认：完全沙箱化）：`{fs: [{host, guest, access: "read-only"\|"read-write"}], env: {K:V} or [K,...]（宿主透传）, args: [...], network: {tcp, udp, dns}}`。相对路径的 `host` 相对于扩展目录解析。每次授权都会在加载时记入审计日志。对 WASI p1 guest 而言网络标志是惰性的（wasmtime-wasi p1 没有 socket ABI）。 |
| `hooks` | 否 | **捆绑**：Claude 格式 `hooks.json` 的路径（可多个）—— 并入会话 hook 配置（见 [hooks.md](hooks.zh-CN.md)）。 |
| `mcpServers` | 否 | **捆绑**：文件路径或内联的 server 映射 —— 合并进会话的 MCP servers。 |
| `skills` | 否 | **捆绑**：skill 目录 —— 合并进 skill 发现。 |

只有捆绑字段（没有 `command`/`module`）的清单也是合法的：它只贡献
声明式资源，不运行插件。WASM 载体插件的示例见
`examples/extensions/hello-wasm/`。

发现根（启动时全部扫描）：

| 根 | 信任 |
|---|---|
| `~/.tack/agent/extensions/*/` | 总是加载 |
| `extensionPaths` 设置（目录数组，全局 settings.json） | 总是加载 |
| `<project>/.pi/extensions/*/` | **需要项目信任**（`/trust`）—— 插件会执行代码 |

损坏的插件（清单错误、拉起失败、握手超时）会被记录并跳过 —— 它
永远不会阻止会话创建。

## 3. 线上协议

每行一个 JSON 对象。三种信封：

```json
{"type":"request","id":1,"method":"tool.execute","params":{...}}
{"type":"response","id":1,"result":{...}}
{"type":"response","id":1,"error":"something failed"}
{"type":"event","event":"agent_start","payload":{...}}
```

### 3.1 握手

1. 宿主拉起插件并发送：
   ```json
   {"type":"event","event":"initialize","payload":{
     "protocol": 1, "mode": "tui", "cwd": "D:/work/repo",
     "trusted": true, "host": "tack/0.1.0"}}
   ```
2. 插件回复其注册信息：
   ```json
   {"type":"event","event":"register","payload":{
     "name": "hello-js",
     "tools": [{"name":"echo","description":"Echo","parameters":{...}}],
     "commands": [{"name":"hello-js","description":"Greet"}],
     "shortcuts": [],
     "subscriptions": ["agent_start","tool_call"]}}
   ```

`subscriptions` 过滤插件接收哪些生命周期事件。为空 = 默认集合
（除 `message_update` 之外的全部 —— 它因频率高而需要显式 opt-in）。
订阅中包含 `tool_call` 即启用拦截（§4.3）。

### 3.2 给插件作者的默认值与健壮性规则

- 按**整行**读取，容忍空行和未知的信封字段。
- 如果你崩溃了，宿主会失败其挂起的调用并继续运行 —— 但你的工具
  会在会话中途消失，所以请自己捕获错误。
- 收到 `shutdown` 事件 → 立即退出（宿主 2 秒后强制杀死）。

## 4. 插件 API

### 4.1 宿主 → 插件：生命周期事件

| 事件 | 载荷 | 时机 |
|---|---|---|
| `session_start` | `{sessionId, resumed, cwd}` | 应用启动 |
| `session_shutdown` | `{sessionId}` | 应用退出 |
| `agent_start` / `agent_end` | agent 事件 JSON | 一次 run 开始/结束 |
| `turn_start` / `turn_end` | agent 事件 JSON | 每个 turn |
| `message_start` | `{message}` | assistant 消息开始流式输出 |
| `message_end` | `{message}` | assistant 消息终态（含 usage） |
| `message_update` | `{message}` | 流式增量 —— **仅限 opt-in** |
| `tool_execution_start` | `{toolCallId, toolName, args}` | 工具开始执行 |
| `tool_execution_end` | `{toolCallId, toolName, result, isError}` | 工具执行完毕 |
| `before_provider_request` | `{provider, model, messageCount, toolCount, hasSystemPrompt}` | 每次 LLM 调用之前（已脱敏 —— 不含消息正文） |
| `after_provider_response` | `{provider, model, stopReason, durationMs, usage, errorMessage}` | 每次 LLM 调用之后 |
| `model_select` | `{provider, modelId, source}` | 模型变更 |
| `thinking_level_select` | `{level}` | thinking 等级变更 |

### 4.2 宿主 → 插件：`tool.execute`

运行插件注册的工具。

```json
{"type":"request","id":7,"method":"tool.execute","params":{
  "name":"echo","toolCallId":"call_1","arguments":{"text":"hi"}}}
```

结果约定（任一即可）：
```json
{"result":{"content":"plain text"}}
{"result":{"content":[{"type":"text","text":"block text"}]}}
{"result":"plain text"}
```

插件工具对模型呈现为 `ext__<plugin>__<tool>`，并流经标准管线：
权限模式、会话持久化、TUI 工具卡片。

### 4.3 宿主 → 插件：`intercept.tool_call`

需要在 `subscriptions` 中包含 `"tool_call"`。在每次内置工具调用
之前触发；插件返回裁决：

```json
{"result":{"action":"allow"}}
{"result":{"action":"deny","reason":"blocked by policy"}}
{"result":{"action":"rewrite","arguments":{ /* full replacement args */ }}}
```

`deny` 会把工具调用变成带 `reason` 的错误结果；`rewrite` 会对新
参数重新校验并以其执行。拦截失败/超时是 **fail-open**（调用照常
进行）。

### 4.3b 宿主 → 插件：`intercept.context`（订阅门控）

订阅了 `"context"` 的插件会在每次 LLM 调用前收到完整的消息列表，
并可返回替换内容（`context` 变更点）：

```json
{"type":"request","id":9,"method":"intercept.context","params":{
  "messages": [ /* AgentMessage... */ ]}}
→ {"result":{"messages":[ /* replacement AgentMessage... */ ]}}
```

仅限 opt-in（载荷是整个上下文）；缺失/非法的回答和超时都是
fail-open —— 使用原始上下文。

### 4.4 宿主 → 插件：`command.invoke`

运行插件注册的斜杠命令（用户输入的 `/<name>`，参数为字符串）。
扩展命令像内置命令一样自动补全，且优先于 prompt 模板。

### 4.5 插件 → 宿主：UI 方法

| 方法 | 参数 | 结果 |
|---|---|---|
| `ui.notify` | `{message, level?}`（`info`/`warning`/`error`） | `null` |
| `ui.select` | `{title, options: [...]}` | 选中的选项字符串，取消时为 `null` |
| `ui.confirm` | `{title, message}` | `true`/`false` |
| `ui.input` | `{title, placeholder?}` | 输入的文本，取消时为 `null` |
| `ui.set_status` | `{text}`（`null` 清除） | `null` |

对话框在 TUI 主循环上渲染；插件的请求只是等待用户。在 headless
运行模式（print/rpc/acp）中，插件以降级服务加载：
`ui.notify`/`ui.set_status` 被接受（仅记日志），
`ui.select`/`ui.confirm`/`ui.input` 返回错误（没有用户可询问），
`session.*`/`provider.register` 不可用。`initialize` 载荷的
`mode` 字段（"tui"/"print"/"rpc"/"acp"）告诉插件它被托管在哪种
模式下 —— 交互式请求绝不能成为正确性的承重部分。

### 4.6 插件 → 宿主：`exec`

```json
{"type":"request","id":3,"method":"exec","params":{"command":"git status","timeout_ms":120000}}
→ {"result":{"stdout":"...","stderr":"...","code":0}}
```

**信任门控**：仅在项目受信任时才被受理（用户目录插件总是受信任；
项目插件继承项目信任）。

### 4.7 插件 → 宿主：会话控制（`session.*`）

从插件驱动会话（等价于 TS 的 ExtensionCommandContext）：

| 方法 | 参数 | 效果 |
|---|---|---|
| `session.get_info` | — | `{sessionId, cwd, provider, modelId, thinking, messageCount, running}` |
| `session.new` | — | 开始一个全新会话 |
| `session.switch` | `{session: "path-or-id-prefix"}` | 恢复另一个会话文件 |
| `session.branch` | `{entryId}` | 跳转会话树（先对被放弃的分支做摘要） |
| `session.set_model` | `{provider, modelId}` | 切换模型 |
| `session.set_thinking` | `{level}` | 切换 thinking 等级 |
| `session.set_name` | `{name}` | 为会话命名 |
| `session.send_user_message` | `{text}` | 注入一条用户消息（运行中时排队，空闲时启动一次 run） |

### 4.8 插件 → 宿主：`provider.register`

注册自定义 LLM provider（models.json 的动态等价物）：

```json
{"type":"request","id":5,"method":"provider.register","params":{
  "id": "corp-gateway",
  "baseUrl": "https://llm.corp.internal/v1",
  "api": "openai-completions",
  "apiKeyEnv": "CORP_LLM_KEY",
  "headers": {"x-team": "search"},
  "models": [{"id": "corp-1", "contextWindow": 128000, "maxTokens": 8192}]
}}
```

`api` 可以是任何 Tack 线上协议（`anthropic-messages`、
`openai-completions`、`openai-responses`、`google-generative-ai`、
`mistral-conversations`、`bedrock-converse-stream`、`tack-messages`，
……；接受旧别名 `pi-messages`）。注册的模型会立即出现在 `/model`
中，并通过匹配的适配器解析。以相同的 `id` 重复注册会替换该条目。

### 4.9 插件 → 宿主：`log` 事件

```json
{"type":"event","event":"log","payload":{"level":"info","message":"..."}}
```

转发到宿主的 tracing 日志（stderr / `--verbose`）。

## 5. 安全模型

- **项目信任**（`/trust`、`~/.tack/agent/trust.json`）：项目本地的
  `.pi/extensions/` 仅在受信任时加载 —— 恶意的克隆无法自动运行
  代码。`defaultProjectTrust: "never"` 会全局阻止它们。
- **进程隔离**：插件崩溃不会拖垮 agent；死掉的插件会失败其挂起的
  调用并退出。
- **exec 门控**：任意宿主命令执行需要信任。
- **无环境访问**：插件只能拿到协议提供的东西 —— 会话数据通过事件
  到达；没有对宿主文件系统/内存的直接访问。
- **供应链**：安装被钉定在 `extensions-lock.json` 中（每个插件记录
  解析出的 commit；§1c），`ext verify` 审计漂移，启动检查会跳过
  漂移的插件（`extensionLockRequired`，默认开启），市场目录可以用
  ed25519 签名并配合 TOFU 钉定的密钥（§1b）。
- 沙箱加固（WASI 能力、内存/fuel 限制）是 WASM v2 变体的职责；
  子进程插件是完整的进程，应当以对待任何已安装程序同等的信任
  来对待它们。

## 6. 编写插件（Node.js 快速上手）

```js
import readline from "node:readline";
const rl = readline.createInterface({ input: process.stdin, terminal: false });
const send = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");

rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.type === "event" && msg.event === "initialize") {
    send({ type: "event", event: "register", payload: {
      name: "my-ext",
      tools: [{ name: "ping", description: "Ping", parameters: { type: "object", properties: {} } }],
      commands: [{ name: "my-cmd", description: "My command" }],
      subscriptions: ["agent_start"],
    }});
    return;
  }
  if (msg.type === "request" && msg.method === "tool.execute") {
    send({ type: "response", id: msg.id, result: { content: "pong" } });
    return;
  }
  if (msg.type === "request" && msg.method === "command.invoke") {
    send({ type: "response", id: msg.id, result: { ok: true } });
    return;
  }
  if (msg.type === "event" && msg.event === "shutdown") process.exit(0);
});
```

配合 `extension.json`：
```json
{ "name": "my-ext", "command": "node", "args": ["plugin.js"] }
```

安装到 `~/.tack/agent/extensions/my-ext/`，重启 Tack，之后
`/my-cmd` 即可使用，模型也可以调用 `ext__my-ext__ping`。

**调试技巧**：运行 `tack --verbose` 查看插件的 stderr 和握手日志；
把一行 `initialize` 通过管道喂给插件即可独立测试它；
`tack ext-demo-plugin` 展示了一个最小且正确的实现。

## 7. 范围

**目前支持**：生命周期事件（包括 provider 边界）、插件工具、斜杠
命令、tool_call 拦截（allow/deny/**rewrite**）、上下文变换
（`intercept.context`，订阅门控）、UI 对话框
（notify/select/confirm/input/status）、信任门控的 exec、会话控制
（`session.*`）、运行时 provider 注册（`provider.register`）、日志、
**WASM 沙箱载体**（`carrier: "wasm"`，protocol 2 握手 —— 见
`examples/extensions/hello-wasm/`）、**声明式 widget 与自动补全
provider**（v2.1/v2.2 —— 见 §8），以及**捆绑资源**（通过清单字段
声明 hooks/MCP servers/skills；hooks 引擎文档见
[hooks.md](hooks.zh-CN.md)）。

**明确不在范围内（v2.x）** —— 这些需要声明式组件协议而非 RPC 调用：

- 工具渲染组件与自定义消息渲染器
- Markdown 变换器
- 请求变更（`before_provider_request` 的 headers/载荷改写 ——
  事件是脱敏后的通知）
- 热重载

v2 设计见 [extensions-v2.md](extensions-v2.zh-CN.md)。

## 8. 声明式 widget 与自动补全 provider（v2.1 / v2.2）

上面的 `ui.*` 方法是命令式 RPC（"打开一个对话框，我等着"）。组件
协议正好相反：插件在注册时**声明**长生命周期的 UI 单元，宿主 TUI
负责渲染和布局，插件通过事件流推送状态更新。完整设计：
[extensions-v2.md](extensions-v2.zh-CN.md) §3。子进程和 WASM 载体
都受支持；v2.1+ 的宿主会与每个载体协商 `protocol: 2`（v1 插件
只是忽略这个更大的数字）。

### 8.1 声明 widget（`register.widgets`）

```json
{"type":"event","event":"register","payload":{
  "name": "git-status",
  "widgets": [
    {"id": "branch", "type": "status_line_segment", "priority": 50,
     "initial": {"text": "main", "style": "dim"}},
    {"id": "diff-panel", "type": "markdown_panel", "title": "Pending diff",
     "visible": false},
    {"id": "files", "type": "list_panel", "title": "Changed files",
     "initial": {"items": []}}
  ]
}}
```

`id` 在插件内唯一；宿主以 `<plugin>:<id>` 为 widget 键。widget 种类
及其状态模式（`initial` 以及此后每次 `widget.update.state` 的形状）：

| `type` | state | 宿主渲染 |
|---|---|---|
| `status_line_segment` | `{text, style?, tooltip?}`；`style` ∈ `default/info/warning/error/dim` | 一个状态栏片段，按 `priority` 升序排列；`text` 为空时隐藏；截断为一行 |
| `markdown_panel` | `{markdown}` | 可开合的面板，用宿主的 Markdown 管线渲染；宿主掌管滚动/焦点并限制面板高度 |
| `list_panel` | `{items: [{id, label, detail?, icon?}], selectedId?}` | 带宿主渲染选中态的条目列表；用户选择会以 `widget.action` 回传 |

### 8.2 事件：`widget.update`（插件 → 宿主）、`widget.action`（宿主 → 插件）

```json
{"type":"event","event":"widget.update","payload":{
  "id": "branch", "state": {"text": "feature/wasm", "style": "info"},
  "visible": true}}
```

更新是**幂等的全量状态替换**（不是 diff），在宿主的下一帧应用；
丢帧无害。未知的 widget id 会被警告并忽略。

```json
{"type":"event","event":"widget.action","payload":{
  "id": "files", "action": "select", "itemId": "src/main.rs"}}
```

`widget.action` 只发送给 widget 的**属主**插件（不受订阅门控，
绝不广播）。

渲染契约（硬性规则）：

- **宿主拥有终端。** 插件永远拿不到屏幕坐标，也不能输出 ANSI；
  在 widget 之外，`ui.*` 仍是唯一的 UI 通道。
- **插件死亡 = widget 移除。** 当插件的对端到达 EOF，宿主会丢弃
  它的全部 widget —— 不留残余。
- **Headless 模式**（print/rpc/acp）：widget 声明被接受但忽略；
  绝不产生 `widget.action`。插件不得依赖 widget 保证正确性。

TUI 键位绑定（可通过 `keybindings.json` 重新绑定）：

| 动作 | 默认 | 效果 |
|---|---|---|
| `app.ext.panels.toggle` | `ctrl+b` | 隐藏/显示全部扩展面板 |
| `app.ext.panel.focusNext` | `alt+p` | 在可见面板间循环键盘焦点；聚焦的 list 面板：`↑↓` 移动，`enter` 选择（发送 `widget.action`），`esc` 取消聚焦；聚焦的 markdown 面板：`↑↓`/`pgup`/`pgdn` 滚动 |

### 8.3 自动补全 provider（`register.autocompleteProviders`，v2.2）

```json
{"type":"event","event":"register","payload":{
  "autocompleteProviders": [
    {"id": "issues", "trigger": "#", "description": "GitHub issues"}
  ]
}}
```

当输入行的当前词元带有某个 provider 的 `trigger` 前缀时，宿主会
请求建议：

```json
{"type":"request","id":9,"method":"autocomplete.provide","params":{
  "providerId": "issues", "query": "wasm", "cursorOffset": 5}}
→ {"result":{"suggestions":[
     {"value": "#1234", "label": "#1234 WASM carrier",
      "detail": "open", "insertText": "#1234"}]}}
```

契约：

- 请求走常规的宿主→插件通道（30 秒协议超时）；TUI 额外加一道
  **300ms 的 UI 级超时** —— 超时、取消、错误响应和死掉的插件
  都会静默降级为"无建议"。
- 空的 `suggestions` 数组是合法的"无建议"回答；接受时
  `insertText` 缺省取 `value`。
- 多个 provider 可以共用一个 trigger；宿主会全部查询并按注册
  顺序合并，按 `value` 去重。
- 内置的 `/command` 和 `@file` 补全优先于扩展 trigger（复用
  `/` 或 `@` 的 provider 在这些词元上永远不会触发）。

可运行的演示（状态栏片段 + list 面板 + `#` provider）见
`examples/extensions/hello-js/`。
