# Tack 扩展（tack-RPC v3）

**[English](extensions.md) | 简体中文**

> 本文档描述 tack-RPC v3 重设计后的扩展系统（背景与阶段见
> [plugin-roadmap.zh-CN.md](plugin-roadmap.zh-CN.md)）。协议的单一事实
> 来源是 OpenRPC 文档
> [`protocol/tack-rpc.openrpc.json`](../protocol/tack-rpc.openrpc.json)——
> 本页覆盖周边机制（清单、身份、store、安装、市场、开发工具）。
> v3 之前的 NDJSON 协议已在同一版本中移除。

## 1. 三个级别

| 级别 | 形态 | 适用 |
|---|---|---|
| **1 — 声明式 bundle** | `extension.json` + hooks/MCP/skills 文件，无代码 | 护栏、上下文、工具接线 |
| **2 — MCP server 插件** | `extension.json` 用 `carrier: "mcp"` 声明一个 MCP 服务器 | 来自 MCP 生态的工具贡献，带插件身份 |
| **3 — tack-RPC 插件** | 讲 tack-RPC v3 的进程或 WASM 载体可执行体 | 拦截、生命周期、widget、会话控制、审批、配置、指标 |

本文档大部分覆盖 Level 3。插件用 SDK 开发（Rust `tack-ext-sdk`、TypeScript
`@tack/plugin`、Python `tack-plugin`）——插件代码永远看不到
JSON-RPC 信封。用 `tack ext new <dir> <rust|ts|python>` 生成脚手架。

## 2. extension.json 字段参考

```jsonc
{
  "name": "my-ext",                    // 必填；id 分段（见 §3）
  "version": "1.2.0",                  // 可选 semver；成为 store 版本目录
  "command": "node",                   // 进程载体：要启动的可执行体
  "args": ["plugin.js"],               // 路径形态的条目相对扩展目录解析
  "env": { "FOO": "bar" },             // 显式环境变量（敏感的宿主变量会被剥离）
  "carrier": "process",                // "process"（默认）| "wasm" | "mcp"
  "module": "plugin.wat",              // wasm 载体：模块文件（.wasm/.wat，
                                       //   core module 或 WIT component——自动检测）
  "mcpServer": {                       // mcp 载体（Level 2）：一个 MCP server 条目
    "command": "node",                 //   （形状与 mcp.json 的 server 相同：
    "args": ["server.js"],             //   command/args/env，或 url/headers/type/oauth）
    "env": { "DEBUG": "1" }
  },
  "limits": { "maxFuel": 100000000, "maxMemoryBytes": 16777216, "maxExecutionMs": 60000 },
  "capabilities": {                    // wasm 载体：显式沙箱授权
    "fs": [{ "host": "data", "guest": "/data", "access": "read-only" }],
    "env": { "LITERAL": "1" },
    "args": ["--verbose"]
  },
  "failMode": "closed",                // hook 失败时拦截（默认 "open"：放行）
  "hooks": "hooks/hooks.json",         // bundle：Claude 格式的 hook 声明
  "mcpServers": "mcp.json",            // bundle：文件路径或内联 server 表
  "skills": ["skills/"]                // bundle：skill 目录
}
```

只有 bundle 字段的清单（无 `command`/`module`）是合法的：它只贡献
声明式资源，不运行插件进程。

## 2.1 Level 2：MCP server 插件（`carrier: "mcp"`）

Level-2 插件的 `extension.json` 恰好声明一个 MCP 服务器——服务器
**就是**插件；永远不启动 tack-RPC 进程。任何已有的 MCP 服务器都
符合条件（stdio、Streamable HTTP 或旧版 SSE，条目形状与 `mcp.json`
相同）。宿主在加载时连接并把探测结果适配进插件模型：

- **能力**：服务器的工具成为插件对外声明的工具；有 resources 时，
  `list_resources` / `read_resource` 元工具加入；其 prompts 以
  `prompt__<name>` 工具加入——与配置文件 MCP 服务器相同的表面。
- **身份**：面向 agent 的工具名是 `ext__<plugin-id>__<tool>`
  （plugin-id 前缀经过消毒）；归因、`ext list`、策略与 hook 拦截
  把它们与其他插件工具完全同等对待。拦截是统一的：其他插件的
  `hooks/beforeToolCall` 看到这些调用与任何其他调用一样。
- **untrusted-content 防御**：工具结果包装在 `<untrusted_content>`
  中并置会话的 untrusted 标志（权限提升），与配置文件 MCP 工具
  完全一致。
- **stdio 解析**：服务器以扩展目录为 cwd 运行；路径形态的相对
  `command`（包含 `/` 或以 `.` 开头）相对该目录解析。参数从不
  被重写（npm 包名如 `@scope/pkg` 含 `/` 但不是路径）。
- **Sampling / elicitation**：elicitation 跟随运行模式（TUI 弹窗，
  headless 模式拒绝）。Sampling **不**为插件连接接线——加载时不
  存在会话模型——所以服务器的 sampling 请求会得到 method-not-found
  （文档化的 Level-2 限制）。
- **失败即数据**：连接或探测失败的服务器以错误落入 `ext list`
  （`LoadedPlugin.error`），与其他插件一样。shutdown 取消连接
  （服务器子进程被杀）。

MCP 无法表达的能力（hooks、widget、会话控制……）从不被声明；
宿主 bug 若调用其一，得到 `ERR_CAPABILITY_NOT_GRANTED`。

## 2.2 WASM 载体：WASI stdio 与 WIT component

`carrier: "wasm"` 覆盖两种模块格式，从模块字节（或文本形态的
`(component`）自动检测：

- **WASI-stdio core module**（debug 载体）：插件经 WASI stdin/stdout
  说同一个 tack-RPC v3 NDJSON 协议；宿主侧与 process 载体共享
  JSON-RPC peer。清单 `capabilities`（fs preopen、env、args）在此
  生效。
- **WIT component**（分发载体，`tack:plugin@0.3.0`——
  [`protocol/wit/tack-plugin.wit`](../protocol/wit/tack-plugin.wit)）：
  组件导出 `tack:plugin/tools` 和/或 `tack:plugin/hooks`，JSON
  字符串载荷承载 rpc3 数据类型（OpenRPC schema 保持单一事实来源；
  wit-bindgen 在任意 guest 语言里负责字符串分帧）。world 不导入
  任何 WASI 接口——沙箱（无 fs、无 env、无网络）是结构性的，
  WASI 能力授权被忽略并记警告。0.3.0 中调用是同步的（每次调用
  的 fuel、墙钟与内存上限，上限与 stdio 载体相同）；trap 的调用
  杀死插件。

组件的能力列表在启动时探测：`tools.list`（rpc3 `ToolSpec[]` JSON）
加上 `hooks.before-tool-call` 是否存在。子集也是合法插件——只做
hooks 的组件只导出 `tack:plugin/hooks`。手写 component-WAT 参考
实现见
[`examples/extensions/hello-component/`](../examples/extensions/hello-component/)
（guest SDK 来自 wit-bindgen 工具链，不由我们提供）。

## 3. 身份、store、启用/禁用

每个插件有稳定的 id **`name@source`**：

- `source` 是目录安装时的市场名，直接安装为 `user`，
  `.pi/extensions` 为 `project`，`extensionPaths` 检出为 `local`。
- name 文法：1–64 个 `[a-z0-9.-]` 字符（不允许 `..`，不允许首尾的
  `.`）；source：`[A-Za-z0-9_-]`。id 按构造就是安全的文件系统段。

```text
~/.tack/agent/extensions/
├── store/<source>/<name>/<version>/   # 版本化安装
├── data/<source>/<name>/              # 可写的插件数据根目录
└── <name>/                            # 旧版平铺安装（v3 之前，仍加载）
```

store 插件的激活版本：有 `local` 则优先，否则最高的 semver 目录。

不卸载即可启用/禁用，持久化在 settings
（`plugins."<id>".enabled`）：

```sh
tack ext disable review@acme
tack ext enable review@acme     // 无歧义时裸名 "review" 也可以
```

`tack ext list` 展示 `id`、状态（active/disabled）、版本、布局
（store/legacy）与目录。

## 4. 安装、升级、校验

```sh
tack ext install <git-url>[#<ref>] | <dir>   // 安装；#ref 锁定 tag/branch/sha
tack ext install <plugin>@<marketplace>      // 经目录解析（§5）
tack ext install --local <dir>               // 安装到项目的 .pi/extensions
tack ext upgrade [id]                        // 重新拉取；commit 未变时是 no-op
tack ext remove <id>                         // 删除 store 版本 + 锁条目
tack ext verify                              // 按锁文件审计已安装的检出
```

`--local` 安装平铺在 `.pi/extensions` 下（由项目信任门控），不进锁文件
与 store。

安装是**原子的**：先把来源放进同级的暂存目录，解析并重读清单
（要求字节一致——防 TOCTOU 换包），然后把暂存目录 rename 进
`store/<source>/<name>/<version>`（已存在的版本先换出为备份，失败
时回滚）。git 在 scrub 环境运行（`GIT_TERMINAL_PROMPT=0`、不继承
`GIT_*` 配置、管道 stdio、超时）。被取代的旧版本会被清理；`local`
永不清理。

store 版本取值：清单的 `version` 是 semver 时取它；否则取去掉前缀
`v` 后是 semver 的安装 ref；否则取 `local`。

### 锁文件 v2

`~/.tack/agent/extensions-lock.json` 锁定每个 store 安装：

```jsonc
{
  "version": 2,
  "plugins": {
    "review@acme": {
      "source": "https://git.acme.com/review.git",
      "rev": "v1.4.2",
      "resolvedCommit": "<sha>",
      "installedAt": 1718000000,
      "version": "1.4.2",
      "store": true,
      "marketplace": "acme"
    }
  }
}
```

v1 锁文件（裸名键、无 `store`/`version` 字段）在读取时于内存中升级
（`name` ⇒ `name@user`、`store: false`），并在下一次安装/升级时
改写为 v2。启动检查（`extensionLockRequired`，默认开）会跳过检出
已偏离锁定 commit 的插件（HEAD 不一致或工作区脏）；`ext verify`
逐插件报告 ok/changed/not-a-git-repo/missing，有变更时退出码非零。

## 5. 市场

市场是命名的 JSON 目录（`{"name": ..., "plugins": {"<name>":
{"source": ..., "rev": ...}}}`），注册在
`~/.tack/agent/marketplaces/`：

```sh
tack ext marketplace add acme ./acme-marketplace.json   // 或 https URL
tack ext marketplace list                               // 已注册的目录
tack ext marketplace list acme                          // 目录里的插件
tack ext marketplace remove acme
```

`<plugin>@<marketplace>` 经目录解析来源并按市场键安装（插件 id 成为
`<plugin>@<marketplace>`）。目录条目的可选 `"rev"` 锁定 git ref。

目录可携带 ed25519 签名（TOFU 密钥锁定）：注册时需提供一次
`--public-key <hex>`；密钥锁定到 `<name>.key`，之后每次解析/安装
都重新验证。被签名的内容是把顶层 `signature` 键移除后用 serde_json
重新序列化的目录（BTreeMap 键序）。

## 6. 开发工具

```sh
tack ext new my-plugin rust        // 脚手架（rust | ts | python）
tack ext inspect my-plugin         // 握手 + dump 声明的能力
tack ext test my-plugin            // 运行 my-plugin/plugin.scenario.json
tack ext dev my-plugin             // 运行并流式查看插件日志（Ctrl-C 停止）
tack ext dev my-plugin s.json      // 交互式运行一个场景
```

场景是 JSON，跑的是真实协议（插件先被启动并完成握手）：

```jsonc
{
  "initialize": { "trusted": true, "config": {} },   // 可选覆盖
  "steps": [
    { "call": "tools/execute", "params": { "name": "hello.echo", "toolCallId": "c-1",
        "arguments": {"text": "hi"} },
      "expect": { "content": [ { "type": "text" } ] } },
    { "call": "hooks/beforeToolCall", "params": { "toolCall": { "toolCallId": "c-2",
        "toolName": "bash", "arguments": {"command": "rm -rf /"} } },
      "expectError": -32001 },
    { "notify": "events/lifecycle", "params": { "event": "turnStart", "payload": {} } },
    { "expectHostRequest": "ui/select", "respond": "b" },
    { "sleepMs": 50 }
  ]
}
```

`expect` 是递归子集匹配；`expectError` 断言 JSON-RPC 错误码；
`expectHostRequest` 编排插件→宿主的应答队列（对话框、exec）；步骤
失败时退出码非零。

## 7. 运行模式

所有运行模式都会加载插件。非 TUI 模式确定性降级：
`ui/select|confirm|input` 返回 `ERR_CAPABILITY_NOT_GRANTED`，
`session/*` 与 `host/registerProvider` 返回 `ERR_METHOD_NOT_FOUND`，
`ui/notify` 进日志，`exec/run` 仍由信任门控。插件从 initialize
payload 的 `mode` 与 `capabilities` 获知当前模式与可用表面。

## 8. 安全模型（原则不变）

- **崩溃隔离**：每插件一个载体；死插件的挂起调用立即失败，其
  widget 随之消失。
- **项目信任**：项目本地扩展仅在信任后加载；`exec/run` 由信任门控。
- **凭据卫生**：敏感的宿主环境变量默认从插件子进程剥离，除非清单
  显式重新声明。
- **WASM 沙箱**：默认无 preopen/环境变量/网络；能力授权显式且记
  审计日志。WIT component 插件构造上即无能力（world 不含任何
  WASI）。
- **Untrusted content**：MCP 工具结果（配置文件服务器与 Level-2
  MCP 插件相同）被包装并标记给权限层。
- **供应链**：commit 锁定 + 漂移跳过、ed25519 目录签名、原子安装。
- **fail-open hooks**：hook 失败记警告并放行（`failMode: "closed"`
  可按插件反转）。
