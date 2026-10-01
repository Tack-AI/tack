# Tack 扩展（tack-RPC v3）

**[English](extensions.md) | 简体中文**

> **刚来？先看实战教程：[plugin-development.md](plugin-development.zh-CN.md)。**
>
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
  headless 模式拒绝）。Sampling（`mcpSampling`，默认关）**延迟**
  解析会话模型：插件连接比任何会话都长寿，所以 executor 按请求
  读取一个各面共享的 cell——会话启动与模型切换时发布——sampling
  请求永远跑在当前模型上；在任何会话开始前到达的请求得到一个
  干净的错误而不是 method-not-found。
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
（store/legacy）与目录。在 TUI 内，`/ext`（或 `/ext list`）渲染同一份
会话启动时的快照——每个被发现的插件及其加载结果
（active/disabled/failed/policy-filtered）、声明的能力或失败原因，
以及加载警告。它是只读的：插件的安装在 CLI 上进行、会话启动时加载
（生产会话内的热加载是非目标，见
[plugin-roadmap.zh-CN.md](plugin-roadmap.zh-CN.md)）。

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
tack ext marketplace sync [acme]                        // 立即同步声明的目录（§5.2）
```

`<plugin>@<marketplace>` 经目录解析来源并按市场键安装（插件 id 成为
`<plugin>@<marketplace>`）。目录条目的可选 `"rev"` 锁定 git ref。

目录可携带 ed25519 签名（TOFU 密钥锁定）：注册时需提供一次
`--public-key <hex>`；密钥锁定到 `<name>.key`，之后每次解析/安装
都重新验证。被签名的内容是把顶层 `signature` 键移除后用 serde_json
重新序列化的目录（BTreeMap 键序）。

### 5.1 目录格式 v2

目录条目还可携带两个字段；未知条目键记警告并跳过（前向兼容）：

```jsonc
{
  "plugins": {
    "review": {
      "source": "https://git.acme.com/review.git",
      "rev": "main",
      // available（默认）| not-available | installed-by-default
      "installation": "installed-by-default",
      // 不物化插件即可富列表（`marketplace list <name>` 显示版本）
      "manifest": {"name": "review", "version": "1.4.2"}
    }
  }
}
```

- `not-available` 拒绝解析并给出明确错误（策展方下架）。
- `installed-by-default` 由启动同步（§5.2）在缺失时自动安装——仍经
  安装通道的策略门控（§9）。卸载它会在下次同步时被恢复；如不需要
  请改用禁用（`tack ext disable <id>`）。

### 5.2 策展市场启动同步（`pluginMarketplaces`）

设置中声明的目录会自动保持新鲜。声明只来自全局层与 MANAGED 层——
目录可以通过 `installed-by-default` 推送代码，项目层不得重定向它
（与 `updateRepo` 同规则）：

```jsonc
{
  "pluginMarketplaces": {
    "acme": {
      "source": "https://git.acme.com/tack/plugins.git", // git 仓库、https
                                                          // .json 目录或本地
                                                          // 文件/目录
      "ref": "main",                 // git ref（默认：远端 HEAD）
      "path": "marketplace.json",    // 仓库内的目录文件
      "publicKey": "<ed25519 hex>"   // 签名目录（TOFU 锁定）
    },
    "onprem": "/opt/tack/acme-catalog.json"             // 简写形式
  }
}
```

启动时同步在后台进行（失败的同步绝不阻塞启动；旧目录继续可用）：

1. 跨进程锁（`marketplaces/.sync/<name>.lock`，10 分钟后视为陈旧）
   串行化并发 tack 进程。
2. 指纹短路跳过未变化的目录：git 来源用 `git ls-remote`，其余用
   内容哈希。
3. 传输：git clone；git 失败且为 https 来源时降级为 forge 的归档
   （先 GitHub/Gitea 形态 `/archive/<ref>.tar.gz`，再 GitLab 形态
   `/-/archive/...`），防御性解包。
4. 激活 = 校验 → 签名检查 → 备份/改名/交换（保留
   `<name>.json.bak`）。坏的或签名错误的同步绝不替换可用目录。
5. 新增的 `installed-by-default` 条目经正常通道安装（策略检查、
   记录 lockfile；失败仅记警告）。

`tack ext marketplace sync [name]` 是同步的手动形式。每轮同步都发出
结构化事件（target `marketplace_sync`），汇入观测 JSONL 与 managed
auditSink。

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
    { "providerStream": { "model": {…}, "context": {…}, "options": {} },
      "expectEvents": [ {"type": "start"}, {"type": "done"} ] },
    { "sleepMs": 50 }
  ]
}
```

`expect` 是递归子集匹配；`expectError` 断言 JSON-RPC 错误码；
`expectHostRequest` 编排插件→宿主的应答队列（对话框、exec）；步骤
失败时退出码非零。`providerStream` 步骤（P7）在 provider 插件上
驱动一次推理流：dev host 分配 `streamId`，捕获
`provider/streamEvent` 通知，将其与 `expectEvents` 做子集匹配，
并可用 `cancelAfterMs` 编排一次取消竞争（终止等待另有
`timeoutMs`）。

## 7. 运行模式

所有运行模式都会加载插件。非 TUI 模式确定性降级：
`ui/select|confirm|input` 返回 `ERR_CAPABILITY_NOT_GRANTED`，
`session/*` 返回 `ERR_METHOD_NOT_FOUND`，`ui/notify` 进日志，
`exec/run` 仍由信任门控。`host/registerProvider` 在每种模式都被
受理（provider 注册与模式无关），桥接 provider 像原生 provider
一样在无头模式下供推理。插件从 initialize payload 的 `mode` 与
`capabilities` 获知当前模式与可用表面。

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

## 9. 企业策略（managed `pluginPolicy`）

组织通过 **managed 设置层** 约束插件面（最高优先级设置文件——见
[configuration.zh-CN.md](configuration.zh-CN.md)；用户层和项目层无法
设置或削弱策略）：

```jsonc
// /etc/tack/managed-settings.json（Linux；路径因操作系统而异）
{
  "pluginPolicy": {
    // 为 true 时，只有在 `plugins` 中显式列名的插件才能加载。
    "managedPluginsOnly": false,
    // 插件来源白名单。缺省/为空 = 不限制。
    "allowedSources": [
      // 精确匹配 git URL；可选 "ref" 只允许锁定到该 ref 的安装
      //（未锁定的克隆会被拒绝）。
      { "type": "git", "url": "https://git.acme.com/tack/plugins.git", "ref": "main" },
      // 对来源 URL 的 host 做正则匹配（https 与 scp 风格 git@host: 均适用）。
      { "type": "hostPattern", "pattern": "^(.+\\.)?acme\\.com$" },
      // 本地目录来源必须等于该路径或位于其下。
      { "type": "local", "path": "/opt/acme/tack-ext" }
    ],
    "plugins": {
      "review@acme": {
        // managed 的 `enabled` 在两个方向上都压过用户/项目层
        //（强制启用或强制禁用）。
        "enabled": true,
        // 只收窄：与插件实际注册集合求交集——只缩不扩：
        "mcpServers": ["jira"],      // 保留的 bundle MCP 服务器
        "tools": ["create_ticket"]   // 保留的已注册工具
      },
      "acme-agent@acme": {
        // 不可分的能力闸门：managed 置 `false` 时，声明了
        // provider.stream 的插件在加载时被策略阻止。
        "provider": false
      }
    }
  }
}
```

策略执行 **两次**：

1. **安装时**——在任何克隆或网络访问之前先检查来源是否在
   `allowedSources` 内；清单解析后（插件 id 已知）、激活前，再以
   `managedPluginsOnly` 成员资格和 managed `enabled: false` 拒绝安装。
   拒绝信息会指明规则及其来源层。`tack ext upgrade` 在重新拉取前
   也会重新检查锁定的来源。
2. **加载时**（兜底）——发现集合在任何载体启动前先过滤：
   `managedPluginsOnly` 下未列名的插件，以及其锁定来源（store 安装）
   或目录（user/project/extensionPaths 检出）不匹配任何
   `allowedSources` 规则的插件——手工篡改 store 或 lockfile 无法
   偷运未批准的来源。工具与 MCP 服务器的收窄在注册时应用，因此
   所有下游消费者天然合规。

被策略阻止的插件 **是行而不是缺席**：`tack ext list` 显示为
`policy-blocked (<原因>)`。每个策略决策——deny、filter、narrow、
`enabled` 覆盖——都以结构化 tracing 事件（target `plugin_policy`）
记入审计日志，指明规则与来源层；配置了 managed `auditSink` 时，
这些事件会上报到组织收集器。当 managed 层钉住相反的值时，
`tack ext enable|disable` 会给出提示。

## 10. Bundle 归档（离线分发）

bundle 是扩展目录的 `.tgz`——无法访问 git forge 的机器的分发单元：

```sh
tack ext bundle pack ./my-plugin            // 生成 my-plugin-1.2.3.tgz
tack ext bundle pack ./my-plugin --output dist/plugin.tgz   // -o 亦可
tack ext install my-plugin-1.2.3.tgz        // 经正常通道安装
```

- **打包是确定性的**：条目排序、mtime/属主置零、权限规整——相同
  输入总产生相同字节。`.git` 与输出文件本身被排除；指向插件根内
  的符号链接按目标内容打包，其余记警告并跳过。
- **安装是防御性的**：解包拒绝链接、设备节点、绝对路径与 `..`
  分量，并有累计大小上限（256 MiB）与单文件/条目数上限。解出的
  目录树与任何安装一样经过 staging → 清单复读 → 策略检查 → 原子
  激活，并记入 lockfile（bundle 路径作为来源）。
  `tack ext upgrade` 跳过 bundle 安装（没有可前进的远端）。
- managed `allowedSources` 的 `local` 规则适用于 bundle 路径。

## 11. 可观测性

### 11.1 加载遥测与 `tack doctor`

每次加载发出一条结构化事件（target `plugin_load`）：按结果计数
（`active | disabled | failed | policy-filtered`），并按错误类别
细分（`manifest | handshake | register | policy | store`）；同时
持久化 `~/.tack/agent/extensions/last-load.json`——`tack doctor`
读取的逐插件报告（doctor 绝不启动插件）。`tack doctor` 报告：
带原因与类别的加载失败、被策略过滤的条目、锁漂移（变化/缺失的
安装）、WASM component 支持。

### 11.2 指标 sidecar

Level-3 插件在宿主不信任其进程的前提下发出遥测：插件在 initialize
时声明 schema，宿主交出经沙箱授权的 scratch 文件，并在任何内容带
着插件归属进入遥测之前严格校验每次抽取：

```jsonc
// initialize 结果（插件声明）：
"metrics": {"operations": {
    "review.run": {"dimensions": {"outcome": ["ok", "error"]}}
}}
// initialize 参数（宿主提供）：
"metrics": {"scratchFile": "/…/extensions/data/user/review/metrics/metrics.ndjson"}
// 插件追加 NDJSON 测量行：
{"operation": "review.run", "value": 1, "dimensions": {"outcome": "ok"}}
```

- **声明校验全有或全无**：操作 id 匹配 `[a-z][a-z0-9_.]{0,63}`，
  每操作至多 8 个维度，枚举为 1–64 个非空值。任何违规使整个声明
  作废并记加载警告。
- **scratch 文件** 位于插件数据根。WASI-stdio WASM 插件通过专用
  读写 preopen（`/metrics`，与其他授权一起记审计）获得。WIT
  component world 完全没有文件系统，component 载体的声明暂记警告
  作废，待未来的类型化 world。
- **抽取校验**：宿主每 30 秒及关闭时抽取；每次抽取上限 64 KiB /
  100 行（超出部分记违规并丢弃），维度集合必须与声明完全一致，
  数值必须有限，同批重复行去重。三次违规本会话禁用该 sidecar。
- 校验通过的测量以结构化事件（target `plugin_metrics`）携带插件
  id 进入遥测——与其他事件一样汇入观测 JSONL 与 managed auditSink。

Rust SDK 以 `MetricsRecorder` 提供此能力
（`cx.capabilities().metrics.scratch_file`）；TS/Python 插件可直接
追加同样的 NDJSON 行。
