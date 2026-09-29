# 插件开发指南

**[English](plugin-development.md) | 简体中文**

> 编写 tack 插件的实战教程：**三种载体**（process / WASM / MCP）×
> **三种 SDK 语言**（Rust / TypeScript / Python）。本页是"亲手做一个"
> 的路径；上手之后，参考手册是 [extensions.md](extensions.zh-CN.md)
> （manifest schema、身份/store、市场、企业策略、可观测性），架构全景是
> [plugin-system.md](plugin-system.zh-CN.md)。

插件讲 **tack-RPC v3** —— NDJSON stdio 上的 JSON-RPC 2.0 —— 唯一 schema
来源是 [`protocol/tack-rpc.openrpc.json`](../protocol/tack-rpc.openrpc.json)。
用 SDK 就完全看不到信封；不用 SDK 的话，任何能读 stdin、写 stdout 的
语言都可以做插件。

## 1. 选载体

载体决定 host 如何运行你的代码，在 `extension.json` 里声明。

| 载体 | 你的代码 | 隔离 | 最适合 |
|---|---|---|---|
| **`process`**（默认） | 任何讲 tack-RPC v3 的可执行文件；Rust / TS / Python 有一等 SDK | OS 进程（崩溃隔离、环境变量清洗） | 大多数插件：工具、防护 hooks、审批、对话框、provider bridge |
| **`wasm`** | WASI-stdio 核心模块（调试载体）或 WIT 组件（分发载体） | wasmtime 沙箱——默认无 fs / 环境变量 / 网络；WIT 组件*构造上*无能力 | 第三方或不可信代码；市场分发 |
| **`mcp`** | 不用自己写——一个现成的 MCP server（stdio / Streamable HTTP / SSE，任意语言）*就是*插件 | server 以自身进程运行 | 复用 MCP 生态，并带完整插件身份 |

各载体的能力：

| 能力 | `process` | WASI-stdio | WIT 组件 | `mcp` |
|---|---|---|---|---|
| 工具（`ext__<plugin>__<tool>`） | ✓ | ✓ | ✓ | ✓（另含资源与提示） |
| Hooks（`beforeToolCall` 改写/拒绝……） | ✓ | ✓ | ✓（仅 `before-tool-call`） | — |
| 审批链（`approval/review`） | ✓ | ✓ | — | — |
| UI 对话框 / widget / 自动补全 | ✓ | ✓ | — | —（elicitation 随运行模式） |
| Provider 注册与 bridge | ✓ | ✓ | — | —（仅普通注册） |
| 命令 / 生命周期事件 / 指标 | ✓ | ✓ | — | — |

各载体的语言：

- **`process`**：Rust（`tack-ext-sdk`）、TypeScript（`@tack/plugin`）、
  Python（`tack-plugin`）SDK —— 或者任何语言，裸讲协议即可。
- **`wasm`**：WASI-stdio —— 把任何能讲 stdio 协议的语言编译到
  `wasm32-wasip1`；WIT 组件 —— 任意
  [wit-bindgen](https://github.com/bytecodealliance/wit-bindgen) 工具链
  （Rust、C、Go、JS、Python），负载是 rpc3 JSON 字符串。
- **`mcp`**：server 可以用任何语言写；根本没有插件侧 tack-RPC 代码。

## 2. 公共骨架

每个插件都是一个**含 `extension.json` 的目录**：

```jsonc
{
  "name": "my-plugin",                 // 必填；成为 id 的一部分
  "version": "1.0.0",                  // 可选 semver → store 版本
  "command": "node",                   // process 载体
  "args": ["plugin.js"],
  // "carrier": "process" | "wasm" | "mcp"   （默认 process）
  // "module": "plugin.wasm",                // wasm 载体
  // "mcpServer": { … },                     // mcp 载体（一个 server）
  // "failMode": "block",                    // hook 失败阻断（默认失败开放）
}
```

完整的 manifest 参考（env、WASM `limits`、hooks/MCP/skills 的 bundle
字段……）见 [extensions.md §2](extensions.zh-CN.md)。任何载体的开发
循环都一样：

```sh
tack ext inspect <dir>     # 握手 + 转储已声明的能力
tack ext dev <dir>         # 运行并跟随插件日志（Ctrl-C 停止）
tack ext test <dir>        # 跑 plugin.scenario.json 断言（适合 CI）
```

## 3. process 载体：一个插件，三种语言

我们用每个 SDK 各写一遍同一个小插件——一个工具（`hello.echo`）加一条
拒绝 `rm -rf /` 的防护 hook：

### 3.1 脚手架

```sh
tack ext new my-plugin rust     # 或：ts | python
```

| 语言 | 生成的文件 | manifest 命令 |
|---|---|---|
| `rust` | `Cargo.toml`、`src/main.rs` | `cargo run --quiet --manifest-path ./Cargo.toml` |
| `ts` | `package.json`、`plugin.js`（ESM） | `node plugin.js` |
| `python` | `plugin.py` | `python3 plugin.py` |

每个脚手架还会生成 `extension.json`、可直接运行的
`plugin.scenario.json` 和 `README.md`。SDK **尚未发布**到
crates.io / npm / PyPI，所以把依赖指向你的 tack 检出（脚手架里有注释
说明）：Rust —— `tack-ext-sdk = { path = "…/crates/tack-ext-sdk" }`，
TS —— `"@tack/plugin": "file:…/sdk/typescript"`，Python ——
`pip install …/sdk/python`。

### 3.2 Rust

```rust
use serde_json::json;
use tack_ext_sdk::{Plugin, ToolSpec, allow, deny, text_output};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    Plugin::builder(env!("CARGO_PKG_NAME"))
        .version("0.1.0")
        .tool(
            ToolSpec {
                name: "hello.echo".to_string(),
                label: None,
                description: "Echo the arguments back".to_string(),
                parameters: json!({"type": "object"}),
            },
            |params, _cx| async move {
                Ok(text_output(format!("echo: {}", params.arguments)))
            },
        )
        .before_tool_call(|params, _cx| async move {
            let command = params.tool_call.arguments
                .get("command").and_then(|v| v.as_str()).unwrap_or("");
            if params.tool_call.tool_name == "bash" && command.contains("rm -rf /") {
                Ok(deny("refusing to delete the world"))
            } else {
                Ok(allow())
            }
        })
        .run()
        .await
}
```

verdict 有三种：`allow()`、`deny(reason)`（reason 成为错误工具结果）、
`rewrite(arguments)`（整体替换参数——链式插件能观察到前一个插件的
改写；首个 `deny` 短路）。

### 3.3 TypeScript

```js
import { plugin, textOutput, allow, deny } from "@tack/plugin";

plugin({ name: "my-plugin", version: "0.1.0" })
  .tool(
    { name: "hello.echo", description: "Echo the arguments back",
      parameters: { type: "object" } },
    async (params, cx) =>
      textOutput(`echo: ${JSON.stringify(params.arguments)}`),
  )
  .beforeToolCall(async (params) => {
    const command = params.toolCall.arguments?.command ?? "";
    return params.toolCall.toolName === "bash" && command.includes("rm -rf /")
      ? deny("refusing to delete the world")
      : allow();
  })
  .run(); // 在 stdio 上服务
```

### 3.4 Python

```python
from tack_plugin import Plugin, text_output, allow, deny

plugin = (
    Plugin("my-plugin", version="0.1.0")
    .tool(
        {"name": "hello.echo", "description": "Echo the arguments back",
         "parameters": {"type": "object"}},
        lambda params, cx: text_output(f"echo: {params['arguments']}"),
    )
    .before_tool_call(lambda params, cx: (
        deny("refusing to delete the world")
        if params["toolCall"]["toolName"] == "bash"
        and "rm -rf /" in (params["toolCall"]["arguments"] or {}).get("command", "")
        else allow()
    ))
)

plugin.run()  # 在 stdio 上服务（底层是 asyncio）
```

Python 的 handler 同步异步皆可。

> **stdout 就是 RPC 总线。** 插件里永远不要 `println!` / `console.log` /
> `print` —— 一行杂散输出就会破坏协议。用 `cx.host.log(...)` /
> `cx.host.warn(...)`，它们落在 host 的 tracing 里。

### 3.5 handler 表面三语对照

| Rust（`PluginBuilder`） | TypeScript | Python | 声明了什么 |
|---|---|---|---|
| `.tool(spec, h)` | `.tool(spec, h)` | `.tool(spec, h)` | 模型可调用的工具 |
| `.command(spec, h)` | `.command(spec, h)` | `.command(spec, h)` | `/斜杠` 命令 |
| `.before_tool_call(h)` | `.beforeToolCall(h)` | `.before_tool_call(h)` | allow / deny / **改写** |
| `.after_tool_call(h)` | `.afterToolCall(h)` | `.after_tool_call(h)` | 按字段修补结果 |
| `.transform_context(h)` | `.transformContext(h)` | `.transform_context(h)` | 全量上下文替换 |
| `.approval_review(h)` | `.approvalReview(h)` | `.approval_review(h)` | 加入审批链 |
| `.events(&[…], h)` | `.events([…], h)` | `.events([…], h)` | 生命周期事件（camelCase：`agentStart`、`turnEnd`……） |
| `.widget(spec)` + `.on_widget_action(h)` | `.widget(spec)` + `.onWidgetAction(h)` | `.widget(spec)` + `.on_widget_action(h)` | TUI widget |
| `.autocomplete(spec, h)` | `.autocomplete(spec, h)` | `.autocomplete(spec, h)` | 参数自动补全 |
| `.config_schema(v)` | `.configSchema(v)` | `.config_schema(v)` | 插件级配置 schema |
| `.metrics(decl)` | `.metrics(decl)` | `.metrics(decl)` | metrics sidecar schema |
| `.provider_register(bool)` | `.providerRegister(bool)` | `.provider_register(bool)` | 可注册 HTTP-shim provider |
| `.provider_stream(h)` + `.on_ready(h)` | `.providerStream(h)` + `.onReady(h)` | `.provider_stream(h)` + `.on_ready(h)` | provider bridge（直接提供推理） |

未声明的能力永远不会被调用——host 会应答
`ERR_CAPABILITY_NOT_GRANTED`。

### 3.6 回调 host

每个 handler 都拿到上下文（`cx`）：协商出的环境（Rust 里
`cx.mode()` / `cx.trusted()` / `cx.cwd()` / `cx.capabilities()` /
`cx.config()`；其余 SDK 有对应物）加一个类型化 host client：

| host 服务 | Rust（`cx.host()`） | 说明 |
|---|---|---|
| `ui/notify` | `.notify(...)` | TUI 提示条；headless 模式记日志 |
| `ui/select` / `ui/confirm` / `ui/input` | `.select(...)` / `.confirm(...)` / `.input(...)` | TUI 对话框；headless → `ERR_CAPABILITY_NOT_GRANTED` |
| `exec/run` | `.exec(...)` | 信任门控（不受信任的会话会被拒绝） |
| `session/get` · `session/sendUserMessage` | `.session()` · `.send_user_message(...)` | 会话状态；注入一条用户消息 |
| `snapshot/get` | `.snapshot()` | 只读会话摘要 |
| `config/get` | `.config()` | 本插件合并后的配置 |
| `logs/emit` · `warnings/emit` | `.log(level, msg)` · `.warn(msg)` | 落在 host tracing |

headless 降级是确定性的（见 [extensions.md §7](extensions.zh-CN.md)）：
print/rpc/acp 下工具、hooks、事件、`exec/run` 照常；对话框调用应答
`ERR_CAPABILITY_NOT_GRANTED`；`ui/notify` 进日志。

## 4. WASM 载体

`carrier: "wasm"` + `"module": "plugin.wasm"`（可加 `limits`）把插件跑
进 wasmtime 沙箱。模块格式自动识别：

### 4.1 WASI-stdio 核心模块（调试载体）

插件在 WASI stdin/stdout 上讲同一套 tack-RPC v3 NDJSON 协议——host 侧
与 process 载体共用 JSON-RPC peer，所以能力面相同（包括审批与
provider bridge）。manifest 的 `capabilities`（fs preopen、env、args）
在此生效；fuel / 墙钟 / 内存由 host 硬性钳制。把任何语言编译到
`wasm32-wasip1` 并讲协议（或用的 SDK 支持该 target 的话直接用 SDK）。
参考：[`examples/extensions/hello-wasm/`](../examples/extensions/hello-wasm/)
（手写 WAT）。

```jsonc
{ "name": "hello-wasm", "carrier": "wasm", "module": "hello.wasm",
  "capabilities": { "fs": ["./data"], "env": ["MY_VAR"] },
  "limits": { "fuel": 1000000000, "memoryMb": 64, "timeoutMs": 30000 } }
```

### 4.2 WIT 组件（分发载体）

面向 [`tack:plugin@0.3.0`](../protocol/wit/tack-plugin.wit) 的组件导出
类型化接口，**负载是 rpc3 JSON 字符串**（OpenRPC schema 仍是唯一来源；
wit-bindgen 在 Rust、C、Go、JS、Python 里处理字符串成帧）：

- `tack:plugin/tools` —— `list: func() -> string`（rpc3 `ToolSpec[]`
  JSON），`execute: func(call: string) -> result<string, string>`
- `tack:plugin/hooks` —— `before-tool-call: func(call: string) ->
  result<string, string>`（入 `BeforeToolCallParams`，出 `Verdict`）
- 只 import `tack:plugin/host` —— `log(level, message)`

这个 world **不 import 任何 WASI 接口**，所以沙箱（无 fs、无环境变量、
无网络）是结构性的而非配置出来的——这正是组件成为市场分发格式的原因。
接口**子集也是合法插件**：纯 hooks 插件只导出 `tack:plugin/hooks`。
调用是同步的，带逐调用 fuel/墙钟/内存上限；一次 trap 只杀死该插件。
真实工具链发出的是带版本号的接口名（`tack:plugin/tools@0.3.0`）——
host 能解析这些，并为手写 guest 回退到裸名。参考：
[`examples/extensions/hello-component/`](../examples/extensions/hello-component/)
（手写组件 WAT）。

## 5. MCP 载体

Level-2 插件声明**一个 MCP server**——server *就是*插件；不会派生
tack-RPC 进程：

```jsonc
{ "name": "filesystem-tools", "carrier": "mcp",
  "mcpServer": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] } }
```

任何现成 MCP server 都可以（stdio、Streamable HTTP 或旧式 SSE——条目
形状与 `mcp.json` 相同，含 `url`/`headers`/`oauth`）。host 在加载时
连接并把探测结果接入插件模型：

- 工具变成名为 `ext__<plugin>__<tool>` 的插件工具；资源带来
  `list_resources` / `read_resource` 元工具；提示成为 `prompt__<name>`
  工具；
- 完整插件身份：归属、`ext list`、策略、hook 拦截（别的插件的
  `beforeToolCall` 看得见这些调用）；
- 不可信内容防御与配置文件里的 MCP server 完全一致
  （`<untrusted_content>` 包裹、权限升级）；
- stdio server 以扩展目录为 cwd 运行；路径形态的相对 `command` 相对
  它解析。

限制：MCP 表达不了的能力（hooks、审批、widget、会话控制）永远不会被
声明；elicitation 随运行模式；**sampling 未接线**到插件连接（已记录的
Level-2 限制）。

## 6. 调试与测试

```sh
tack ext inspect my-plugin      # 握手；转储已声明的能力
tack ext dev my-plugin          # 服务并跟随日志直到 Ctrl-C
tack ext dev my-plugin s.json   # 交互式跑一个场景
tack ext test my-plugin         # 断言；失败退出码非零
```

`tack ext test` 默认用 `<dir>/plugin.scenario.json`。场景跑在**真实
协议**上（插件会被派生并完成握手）：

```jsonc
{
  "initialize": { "trusted": true, "config": {} },   // 可选覆盖
  "steps": [
    { "call": "tools/execute", "params": { "name": "hello.echo", "toolCallId": "c-1",
        "arguments": {"text": "hi"} },
      "expect": { "content": [ { "type": "text" } ] } },        // 递归子集匹配
    { "call": "hooks/beforeToolCall", "params": { "toolCall": { "toolCallId": "c-2",
        "toolName": "bash", "arguments": {"command": "rm -rf /"} } },
      "expectError": -32001 },                                   // JSON-RPC 错误码
    { "notify": "events/lifecycle", "params": { "event": "turnStart", "payload": {} } },
    { "expectHostRequest": "ui/select", "respond": "b" },        // 编排 plugin→host 应答
    { "providerStream": { "model": {…}, "context": {…}, "options": {} },
      "expectEvents": [ {"type": "start"}, {"type": "done"} ],
      "cancelAfterMs": 50 },                                     // provider-bridge 插件
    { "sleepMs": 50 }
  ]
}
```

会话中插件行为异常时，`tack doctor` 读 `extensions/last-load.json`，
报告带原因的加载失败（manifest / handshake / register / policy /
store）以及被策略过滤的条目。

## 7. 安装、迭代、分发

**实时开发循环**——把检出目录加进 settings 的 `extensionPaths`
（单个扩展目录或其父目录；project 层的条目受信任门控）。这样加载的
插件 source 为 `local`，直接从工作目录读取——改完不用重装：

```jsonc
// ~/.tack/agent/settings.json
{ "extensionPaths": ["~/work/my-plugin"] }
```

**store 安装**——`tack ext install <目录|git-url[#ref]|name@marketplace|file.tgz>`
复制进版本化 store
（`~/.tack/agent/extensions/store/<source>/<name>/<version>/`，source 为
`user`；`--local` 装进项目的 `.pi/extensions`，source 为 `project`）。
身份是 `name@source`；活动版本取最高 semver。然后：

```sh
tack ext list                    # id、状态、版本、布局——含失败/被策略拦截
tack ext disable my-plugin@user  # 持久化为 plugins."<id>".enabled
tack ext upgrade [my-plugin]     # 指纹幂等；无变化时为空操作
tack ext verify                  # 对照 lockfile 校验安装
```

**分发**：

- `tack ext bundle pack <dir>` 产出确定性的 `<name>-<version>.tgz`——
  离线分发单元；`tack ext install <file.tgz>` 按敌意输入规则解包。
- 市场：`tack ext marketplace add <name> <file|url>`（签名 catalog 固定
  ed25519 公钥，TOFU），然后 `tack ext install <plugin>@<marketplace>`。
  catalog v2（`installation`、内联 `manifest`）与策划启动同步
  （`pluginMarketplaces`）见 [extensions.md §5](extensions.zh-CN.md)。

## 8. 接下来读什么

| 主题 | 文档 |
|---|---|
| manifest schema、store、安装/升级、市场、策略、可观测性 | [extensions.md](extensions.zh-CN.md) |
| 审批链语义（认领/顺延/失败开放、提示注入防线） | [plugin-system.md §3.1c](plugin-system.zh-CN.md) |
| provider bridge（插件直接提供推理） | [plugin-provider-bridge.md](plugin-provider-bridge.zh-CN.md) |
| 架构全景、hooks 引擎、运行模式 | [plugin-system.md](plugin-system.zh-CN.md) |
| 线协议本身 | [`protocol/tack-rpc.openrpc.json`](../protocol/tack-rpc.openrpc.json)、[`protocol/wit/tack-plugin.wit`](../protocol/wit/tack-plugin.wit) |
| 设计缘由（三级模型、企业管理面） | [plugin-roadmap.md](plugin-roadmap.zh-CN.md) |
