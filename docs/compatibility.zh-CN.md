# 兼容性与版本策略

**[English](compatibility.md) | 简体中文**

本文档定义 Tack 的哪些接口是稳定的、如何版本化，以及集成方（编辑器
插件、远程客户端、扩展作者、hook 编写者）可以依赖哪些保证。在判断
某个改动是否属于"破坏性变更"时，本文档是规范性参考。

读者：Tack 贡献者，以及一切基于 Tack 外部界面（surface）进行构建的人。
下文中引用的代码路径是*当前*状态的证据；*承诺*一栏才是面向未来的契约。

---

## 1. 稳定性分级

Tack 的接口分为三级：

| 级别 | 含义 | 接口 |
|---|---|---|
| **稳定 / 只增不改（append-only）** | 永不移除；新能力以后向兼容的方式加入。旧数据永远可读。 | 会话文件读取路径（v1–v4 格式）、`settings.json` keys、CLI 标志 |
| **版本化协议** | 带有显式版本号（或继承自上游规范的版本号）。破坏性变更提升版本号，并遵循 §4.3 的协议升级流程。 | CBOR 远程协议（tack-protocol）、扩展协议（tack-ext / tack-ext-wasm）、RPC 模式、ACP、MCP、Claude Code 兼容 hooks |
| **内部 / 无保证** | 可能在任何版本中不加通知地变更。不要基于这些构建。 | 所有 workspace crate 的 Rust API、内部 agent 事件类型、TUI 内部实现 |

Rust workspace 的各个 crate **不发布到 crates.io**，也不提供 semver
承诺（§6）。"Tack" 作为有版本的产品，指的是 `tack` 二进制及其外部
可观察行为。

---

## 2. 接口清单

### 2.1 会话存储格式（级别：稳定 / 只增不改）

**当前状态（证据）。**

- 当前的磁盘格式是**事务格式 v4**：一个 JSONL 事务日志，首行为
  `{"kind":"header","v":4,"storage_version":1,...}`。格式常量位于
  `crates/tack-session/src/v4/types.rs`（`V4_FORMAT_VERSION = 4`、
  `V4_STORAGE_VERSION = 1`）；规范性的线上格式（wire-format）说明是
  `docs/session-v4-protocol.md`，格式决策以及与上游 TS pi 的刻意分歧
  记录在 `crates/tack-session/V4_NOTES.md`。
- **版本探测**是首行嗅探，位于
  `crates/tack-session/src/v4/codec.rs`（`parse_session_header`）：
  满足 `kind == "header" && v == 4` 的识别为 v4 存储头；满足
  `type == "session" && version == 3` 的识别为旧版头。v1 文件不带
  版本字段，v2/v3 共用旧版头的形状。
- **打开时透明迁移**在两个地方实现：
  - 旧版链 v1→v2→v3，原地迁移，位于
    `crates/tack-session/src/manager.rs`（`migrate_to_current`，
    `crates/tack-session/src/entry.rs` 中的 `CURRENT_SESSION_VERSION = 3`）；
  - 流式旧版 v3→v4，位于 `crates/tack-session/src/v4/migrate.rs`
    （`migrate_v3_to_v4`）：两遍有界内存扫描、崩溃安全（原始内容
    复制到 `<path>.bak`，新内容先写入临时文件再原子重命名），
    加密状态保留。
- **写入路径。** 默认后端 `SessionBackend::JsonlV4` 只写 v4。旧版 v3
  写入路径作为字节级兼容的逃生通道保留：settings 中的
  `sessionBackend: "v3"` 选中它
  （`SessionBackend::from_setting`、`crates/tack-session/src/manager.rs`）。
  用 v3 后端打开 v4 文件是硬错误，而不是静默损坏
  （`SessionError::V4FileWithV3Backend`）。
- **`pi.*` 互操作性。** 上游 pi 以相同的 v4 布局写入，但使用 `pi.`
  命名空间前缀。Tack 从不写 `pi.*`，但打开 pi 写入的会话时会将其
  记录视为只读回退（分支末端、lane 配置、会话名称、条目标签；
  `tack.*` 记录一旦存在即为准），fork 时也会对 `pi.*`
  命名空间应用上游自身的投影规则
  （`crates/tack-session/src/fork_policy.rs`、
  `crates/tack-session/src/v4/store.rs`）。
- 未知或未来的条目类型 —— 以及迁移时载荷已不再符合类型化 schema 的
  保留记录（未知消息角色、缺失字段）—— 会被保留为 v4 *custom* 条目，
  而不是被拒绝或中止 —— 这是 Tack 的"不丢数据"原则
  （`crates/tack-session/src/v4/migrate.rs`）。
- `crates/tack-session/src/sqlite_backend.rs` 是一个**实验性**后端
  （`sessionBackend: "sqlite"`），不在下述保证范围内。

**承诺。**

- 对历史写入的每一种会话格式（v1、v2、v3、v4）的读取路径是
  **永久性的**。迁移代码永不移除：会话文件是用户数据，任何历史版本
  Tack（或 TS pi）写出的文件都必须保持可打开。这是本文档中最强的
  保证。
- 新会话只以当前格式写入。在同一格式内部，演进是**只增不改**的：
  可以新增条目类型和新的可选字段；既有字段永不改变含义。让线上形状
  更贴近已文档化协议的对齐修复 —— 例如 v4 消息载荷中根级
  `branchSummary.fromId` 序列化为 `null`
  （v3 的 `"root"` 哨兵只保留在 v3 记录内）—— 属于 bug 修复，
  而非格式变更：修复前写入的文件依然可读。
- 如果未来真的引入 v5 格式，v1–v4 的读取/迁移链会被扩展而非替换，
  并且在默认格式切换的同一个版本里，会附带一个等同于
  `sessionBackend: "v3"` 的字节级兼容写入逃生通道。
- 只要"与逐字节解析 v3 JSONL 的工具互操作"仍是被声明支持的场景，
  `sessionBackend: "v3"` 旧版写入路径就会保留；它的移除（如果真有
  那天）属于 §4 下的破坏性变更，需要经历弃用窗口。

### 2.2 CBOR 远程协议 —— `tack-protocol`（级别：版本化）

**当前状态（证据）。**

- 分帧 CBOR：4 字节大端长度前缀 + CBOR 载荷
  （`crates/tack-protocol/src/framing.rs`），最大帧 16 MiB
  （`DEFAULT_MAX_FRAME_LENGTH`），CBOR 嵌套上限 128 层
  （`MAX_CBOR_NESTING_DEPTH`）。与 TS pi 的
  `packages/protocol/src/framing.ts` / `codec.ts` 字节级兼容。
- **版本**：`crates/tack-protocol/src/schemas.rs` 中的
  `PROTOCOL_VERSION: u32 = 1`。schema 与上游
  `packages/protocol/src/schemas.ts` 逐字段对齐。
- **扩展面（只增加扩展）**：插件 slash 命令、声明式 widget、
  自动补全提供者、`ui/select`/`ui/confirm`/`ui/input` 对话框以及
  MCP elicitation 作为 v1 的只增加扩展暴露在远程协议上。新增命令
  （`list_ext_commands`、`invoke_ext_command`、`list_ext_widgets`、
  `ext_widget_action`、`list_ext_autocomplete`、`ext_autocomplete`、
  `ext_dialog_response`）—— 扩展前的服务端对它们回保留错误
  `unknown command (newer protocol extension?)`。新增事件
  （`ext_widget_update`、`ext_widgets_removed`、`ext_dialog_request`、
  `ext_dialog_closed`）在扩展前的客户端上解码为 `Unknown` 兑底。
  服务端推送的事件还额外受 hello 能力协商门控：客户端 hello 携带
  可选的 `capabilities` 列表（`ext_widgets`、`ext_dialogs`；服务端
  hello 回显其支持的集合），连接只会收到它选择加入的受门控事件
  —— 扩展前的客户端看到的就是原样 v1 事件流。插件对话框没有会话
  归属（插件连接是 host 级单例），因此 —— 与多客户端 attach 时的
  `PermissionRequest` 相同 —— 它们广播给每个 dialog-capable 连接，
  首个 `ext_dialog_response` 胜出；没有任何 dialog-capable 连接时，
  插件对话框立即失败（capability-not-granted / elicitation 拒绝），
  而不是空等。
- **已存在版本协商**：`RemoteClient::connect`
  （`crates/tack-protocol/src/client.rs`）发送 `Hello { version }` 并
  检查服务端的 hello。说**更新**协议的服务端会被保留错误码
  `ProtocolErrorCode::Version` 拒绝，并附带"请升级客户端"的消息；
  **更旧**的服务端会被接受 —— 线上格式是只增的，未知字段被忽略，
  未知枚举变体解码进 `Unknown` 兜底变体。版本是有序整数，不是
  semver。

**承诺。**

- 协议 v1 只增演进：新的可选字段、新的命令/事件变体（带兜底容忍）
  都是 v1 兼容的变更，随 minor 版本发布。
- 线上破坏性变更需要走协议升级流程（§4.3）：新增
  `PROTOCOL_VERSION` 常量、过渡期内经版本协商的双栈运行，以及对
  版本不匹配对端的明确报错 —— 绝不允许静默的异常行为。

### 2.3 RPC 模式（级别：版本化 —— 事实上的 v1）

**当前状态（证据）。**

- `tack rpc`（以及 `--mode rpc`，`crates/tack-app/src/main.rs`）：
  stdin 上是 JSONL 命令，stdout 上是 JSONL 响应 + 事件，与 TS pi 的
  `pi --mode rpc` 线上兼容
  （`crates/tack-app/src/rpc/mod.rs`）。
- 命令集定义在 `handle_command` 的 match 分支和可内省的
  `RPC_COMMANDS` 常量中（34 个命令：`prompt`、`steer`、
  `abort`、`fork`、`compact`、`bash`、`get_state`、`get_commands`……）；
  未知命令会得到结构化的 `unknown or unsupported command` 错误。
- 目前的 RPC 线上格式**没有协议版本字段**；客户端通过
  `get_commands` 发现能力。

**承诺。**

- 现有 RPC 线上形状（命令名、`id`/`type`/`command`/`success`
  响应信封、事件流）在 v1 线上保持稳定。
- 新命令和新的可选命令/响应字段是只增变更，随 minor 版本发布；
  客户端应容忍未知事件类型。
- 重命名或移除命令、或改变某个响应字段的含义，属于破坏性变更
  （§4），需要弃用窗口，或者由同一改动引入显式的 RPC 协议版本字段。

### 2.4 ACP —— Agent Client Protocol（级别：版本化，跟随上游）

**当前状态（证据）。** `crates/tack-app/src/acp/` 通过
`agent-client-protocol` crate（版本 **0.9.5**，`crates/tack-app/Cargo.toml`，
启用 `unstable_session_model`、`unstable_session_usage`、
`unstable_session_info_update` feature）实现
[Agent Client Protocol](https://agentclientprotocol.com)，并协商
`agent_client_protocol::ProtocolVersion::LATEST`
（`crates/tack-app/src/acp/agent.rs`）。协议版本由上游 crate/规范定义，
而非由 tack 定义。

**承诺。** Tack 跟踪上游 ACP 的稳定版本，并按 ACP 规范协商版本。
ACP 侧的破坏性变更只通过上游 crate 升级进入，会在 CHANGELOG 中
明确指出，并在 `docs/upstream-alignment.md`（§5）中跟踪。

### 2.5 扩展协议 —— tack-RPC 与 WASM 载体（级别：版本化）

**当前状态（证据）。**

- 插件（进程或 WASI 载体）说 **tack-RPC v3**：NDJSON stdio 上的
  JSON-RPC 2.0，由 OpenRPC 文档 `protocol/tack-rpc.openrpc.json`
  定义（单一事实来源；`crates/tack-ext/src/rpc3.rs` 中的 Rust 类型与
  TS/Python SDK 类型由 `cargo run -p xtask -- codegen` 从它生成，
  CI 做新鲜度检查）。
- **版本握手**：宿主在 `initialize` 中发送 semver
  `protocolVersion`（"3.0.0"）；兼容要求 major 版本相同且对端
  minor 不超过宿主（`crates/tack-ext/src/v3/mod.rs`）。
- v1/v2 NDJSON 协议（`{"type":"request|response|event"}`）已在 v3
  版本中**移除**：宿主不再说它，且生命周期面被刻意收缩
  （`session/*` 只剩 `session/get` 与 `session/sendUserMessage`；
  插件注册的快捷键与 `ui.set_status` 被移除；生命周期事件名改为
  camelCase）。
- 锁文件：`extensions-lock.json` v1 在读取时于内存中升级（裸名键
  变为 `name@user`、`store: false`），并在下一次安装/升级时改写为
  v2。旧版平铺 `extensions/<name>/` 布局继续以 `name@user` 加载；
  新安装进入版本化 store
  （`extensions/store/<source>/<name>/<version>/`）。
- **Level-2 MCP server 插件**（`extension.json` 的 `carrier: "mcp"`
  加一个 `mcpServer` 条目，形状与 `mcp.json` 相同）：宿主连接声明
  的 MCP 服务器（stdio / Streamable HTTP / 旧版 SSE），并把它的工
  具、resource 元工具与 prompt 工具适配进插件的能力列表。无
  tack-RPC 线上协议变化：适配是宿主内部的（`PluginConnection`），
  插件策略/拦截把这些工具与 tack-RPC 插件工具完全同等对待。
- **WIT component WASM 载体**（`carrier: "wasm"`，从模块格式自动
  检测）：组件从
  [`protocol/wit/tack-plugin.wit`](../protocol/wit/tack-plugin.wit)
  （`tack:plugin@0.3.0`）导出 `tack:plugin/tools` 和/或
  `tack:plugin/hooks`。载荷是承载 rpc3 类型的 JSON 字符串——
  OpenRPC 文档保持单一 schema 来源。WIT 包版本是契约的版本句柄
  （只增 = minor 提升；破坏 = 新包版本）。WASI-stdio core-module
  载体保留为 debug 载体。
- **企业插件策略**（managed 设置 `pluginPolicy`）：
  `managedPluginsOnly`、`allowedSources` 来源白名单
  （git/hostPattern/local），以及按插件的 `enabled`（managed 压过
  用户/项目层）加只收窄的 `tools`/`mcpServers` 交集。在安装时
  （克隆/网络之前与激活之前）与加载时（发现过滤 + 注册收窄）双重
  执行；被阻止的插件保留为可见行，决策连规则与来源层一并记入审计
  日志。该键直接以最终形态落地——不规划 v1→v2 迁移。
- **插件分发 + 可观测（P6）**：对所有既有面均为只增；新增的
  持久化位直接以最终形态落地：
  - **目录 v2**：条目可携带 `installation`（`available |
    not-available | installed-by-default`）与内联 `manifest`；
    未知条目键记警告并跳过，新目录在旧宿主上也能解析。签名规范化
    不变（未知键随被签名载荷一起传输）。
  - **策展同步状态**：`pluginMarketplaces`（仅全局 + managed 设置
    层）声明启动时保持新鲜的目录；暂存位于
    `marketplaces/.sync/`（锁 + 状态），被换下的目录保留为
    `<name>.json.bak`。
  - **加载报告**：`extensions/last-load.json`（v1）——带错误类别
    的逐插件结果行，每次加载后写入，`tack doctor` 读取。
  - **Bundle**：`tack ext bundle pack` 写出确定性的
    `<name>-<version>.tgz`（单一顶层目录、条目排序、mtime 置
    零）；安装时防御性解包并把 bundle 路径记为 lockfile 来源
    （`ext upgrade` 跳过）。
  - **指标 sidecar**：插件声明 `capabilities.metrics`（操作 + 维度
    枚举，全有或全无校验）；宿主提供
    `capabilities.metrics.scratchFile` 并严格校验 NDJSON 测量行
    （`{"operation", "value", "dimensions"}`）。线上形状自
    v3.0.0 起就在 OpenRPC schema 中；P6 接通宿主侧（process 与
    WASI-stdio 载体；WIT component world 无文件系统，声明记警告
    作废）。
  - 新的结构化事件 target：`plugin_load`（按错误类别计数的结果）、
    `plugin_metrics`（带插件归属的校验通过测量）、
    `marketplace_sync`（同步结果）——与 `plugin_policy` 一样汇入
    观测 JSONL/auditSink 管道。
- **Provider 桥（P7）**：v3 面新增了 provider 能力与四个方法，
  直接以最终形态落地（当时没有任何已发布插件或 v3 部署）：
  - `PluginCapabilities.provider = { stream }` 声明供推理；
    `host/registerProvider` 载荷新增了可选的 `bridge` 标志
    （`RuntimeProviderSpec.bridge`——只增的可选字段；
    `models.json` 不变）。桥接模型携带保留的 api kind
    `ext-provider-bridge`。
  - `PluginCapabilities.provider.register`（只增）声明 plain（非桥）
    注册：以 plain spec 调用 `host/registerProvider` 现在与桥供推理
    一样受能力门控（未声明则以 `ERR_CAPABILITY_NOT_GRANTED` 拒绝）
    与 managed 策略门控——managed 的 `provider` 拒绝会令声明任一
    标志的插件在加载时进入 policy-blocked。id 与内建 provider 冲突
    的运行时 provider（plain 或桥）被拒绝；`apiKeyEnv` 不再为运行时
    provider 从宿主环境解析（密钥须在 `apiKey` 中显式携带，或不带）。
  - `provider/stream`（宿主→插件，快速 ack）启动一次推理流；
    `provider/streamCancel`（通知）中止它；
    `provider/streamEvent`（插件→宿主通知）按 `streamId` 解复用，
    每条消息携带一个 `AssistantMessageEvent`，每个流恰好一个终止
    事件（`done`/`error`）；`provider/event`（插件→宿主通知）呈现
    provider 级限速/警告。`model`/`context`/`event` 是由 tack-ai
    的 serde 类型解析的 provider 形状 JSON（OpenRPC 文档将其标为
    自由形态，与 `RegisterProviderParams.provider` 同一先例）。
  - WIT world 不变（`tack:plugin@0.3.0`）：component 载体不供推理；
    来自它与 MCP 载体插件的桥注册以 `ERR_CAPABILITY_NOT_GRANTED`
    拒绝。指定了桥接模型的会话在插件不在时沿用既有的模型未找到
    路径恢复——与 CLI 消失的原生 provider 行为相同。
  - 新的结构化事件 target：`plugin_provider`（注册、流生命周期、
    协议违例、合成的终止事件），在 managed `auditSink` EnvFilter
    中固定为 INFO。
  - 原生的限速通知器泛化为 provider 事件通道
    （`tack_ai::set_provider_event_notifier`）；codebuddy provider
    搭上它，行为不变。
- **allow-always 持久化**：`permissions.json` 新增了只增的
  `extToolVersions` 映射，把每个 `ext__*` allow-always 条目绑定到
  批准时记录的插件版本。旧版 tack 完全忽略这个未知键；新版 tack
  会忽略没有记录版本的遗留 `ext__*` allow-always 条目（fail-closed
  ——条目只是不再匹配，因此 `tack ext upgrade` 无法搭 stale 批准的
  便车）。

**承诺。**

- tack-RPC 在同一 major 版本内只增：新方法、新通知与可选字段在两侧
  都是 minor 版本变更（两端都必须容忍未知方法/通知与未知字段）。
- 线上破坏性变更提升 major 协议版本并遵循 §4.3；semver 握手（major
  相同、对端 minor ≤ 宿主 minor）就是兼容机制，必须对混合版本组合
  持续有效。
- WIT 包在 `tack:plugin@0.x` 内遵循相同的只增规则：新接口或
  `host` 上的新函数是只增的（guest 导出子集，不用的 import 不必
  需）；移除或改类型的导出是破坏性的，提升包版本。

### 2.6 Hooks（级别：版本化，跟随上游）

**当前状态（证据）。** Tack hooks 与 **Claude Code 兼容**
（`docs/hooks.md`）：嵌套事件配置 schema
（`matcher` + `hooks[]`）、处理器类型 `command` / `prompt` / `agent`、
命令处理器的 stdin JSON（`session_id`、`transcript_path`、`cwd`、
`hook_event_name`……）、verdict-JSON stdout schema
（`decision`、`hookSpecificOutput.permissionDecision`、
`updatedInput` 部分合并语义、`additionalContext`、
`systemMessage`、`continue`/`stopReason`），以及退出码约定
（exit 2 = 阻止并以 stderr 作为原因；其他非零 = 警告）。
Tack 旧版扁平 hook 格式会被自动接受。所有 hook 失败都是
fail-open（失败放行）。

**承诺。** 兼容目标是 Claude Code 的稳定 hook schema（命令处理器的
I/O 契约和 settings 形状）。Tack 跟随上游 Claude Code schema 的演进：
上游新增被以只增方式采纳；分歧记录在 `docs/hooks.md`。该界面的变更
遵循 §5 的上游跟踪策略。

### 2.7 MCP —— Model Context Protocol（级别：版本化，跟随上游）

**当前状态（证据）。** MCP client 和 server 支持位于
`crates/tack-app/src/mcp_config.rs`、`mcp_serve.rs`、`mcp_elicitation.rs`、
`mcp_oauth.rs`、`mcp_sampling.rs`，构建于 `rmcp` crate（版本 **3.1.4**，
根 `Cargo.toml`）之上。传输支持引用规范 **2024-11-05**（旧版 SSE）
和 streamable HTTP（`mcp_config.rs`）；线上协议版本由上游 crate 按
MCP 规范协商。

**承诺。** 与 ACP 相同：跟随上游 `rmcp`/MCP 规范的稳定版本，按规范
协商版本，升级记录在 CHANGELOG 和 `docs/upstream-alignment.md` 中。

### 2.8 `settings.json` keys（级别：稳定 / 只增不改）

**当前状态（证据）。** Settings 由全局
`~/.tack/agent/settings.json` 和项目 `.pi/settings.json` 组成，
深合并，项目优先（`crates/tack-app/src/settings.rs`）。加载器显式
**保留未知 keys** 并在类型化访问器中忽略它们 —— 只增约定是结构性
的，而不只是愿景。目前不存在 key 重命名的别名/迁移机制；历史上 keys
只新增过（见 CHANGELOG 历史，例如 `memoryDirectory`、
`microcompact.minSavingsChars`、`sessionBackend`）。

**承诺。**

- 既有 keys 永不以后向不兼容的方式改变含义、类型或默认值。新 keys
  是只增的、可选的，并带有文档化的默认值。
- 如果某个 key 必须重命名，旧 key 会作为已弃用别名至少再工作一个
  minor 周期并发出警告，别名映射会记录在本文档和
  `docs/configuration.md` 中。
- 移除某个设置的效果属于 §4 下的破坏性变更。

### 2.9 CLI 标志（级别：稳定 / 只增不改）

**当前状态（证据）。** 标志用 clap 定义在
`crates/tack-app/src/main.rs`（`--print`、`--continue`、`--resume`、
`--session`、`--model`、`--mode`，子命令 `rpc`/`acp`/`serve`/…）。
`--mode rpc` 是 `rpc` 子命令的文档化别名。

**承诺。** 既有标志和子命令保持其含义；新标志是只增的。重命名或移除
标志、或改变标志的取值语义，属于破坏性变更（§4），并遵循弃用窗口
规则（技术上可行时：别名 + 警告，持续一个 minor 周期）。

### 2.10 Rust crate API（级别：内部 / 无保证）

**当前状态（证据）。** workspace 的九个 crate
（`tack-ai`、`tack-agent-core`、`tack-session`、`tack-tools`、`tack-app`、
`tack-protocol`、`tack-tui`、`tack-ext`、`tack-ext-wasm`）只通过路径
依赖被消费；没有任何 crate 声明 `publish`，也没有发布到 crates.io。

**承诺。** 无。workspace crate 的公开 Rust 条目可能在任何版本中变更。
如果某个 crate 将来发布到 crates.io，届时会获得自己的 semver 策略；
本文档会先更新。

---

## 3. 版本编号

**当前状态。** 整个 workspace 共享 `Cargo.toml` →
`workspace.package.version` 中的一个版本（单一事实来源；撰写时为
`1.0.0`）。发布以 `tack-vX.Y.Z` 打 tag，且 tag 必须与 workspace
版本完全一致 —— 否则发布工作流会失败（`docs/release.md`）。面向用户
的变更记录在 `CHANGELOG.md` 的 `## [x.y.z]` 标题下（由 `/changelog`
和启动时的"新功能"提示解析）。

**承诺（1.0 之后的语义）。**

| 提升 | 内容 |
|---|---|
| **PATCH** | Bug 修复；不改变行为的变更。 |
| **MINOR** | 功能；对任何稳定级或版本化接口的只增变更（新 settings keys、新 CLI 标志、新 RPC 命令、新可选协议字段、带兜底容忍的新协议变体）。 |
| **MAJOR** | §4.1 定义的任何破坏性变更。 |

弃用警告（§4.2）随 MINOR 版本发布；实际移除随下一个 MAJOR 版本落地
（或者在变更符合级别且窗口已明确宣布时随更晚的 MINOR 落地 —— 拿不
准时用 MAJOR）。

---

## 4. 破坏性变更策略

### 4.1 什么算破坏性变更

- 丧失读取或迁移任何历史会话文件格式（v1–v4）的能力。**这一条永远
  不被允许**（§2.1）。
- 移除或重命名 `settings.json` key、CLI 标志或 RPC 命令，或改变既有
  者的含义/类型。
- 改变 CBOR 协议、扩展协议或 hook I/O 契约中既有字段的语义。
- 提升 CBOR 或扩展协议的 `PROTOCOL_VERSION`（提升本身是被允许且
  预期的；它是必须遵循 §4.3 和 MAJOR 版本提升的破坏性变更）。
- 移除 `sessionBackend: "v3"` 字节级兼容写入路径。
- 以静默改变既有集成行为的方式改变默认值（需要判断；拿不准时按
  破坏性处理）。

明确**不**属于破坏性变更：新增 settings keys、CLI 标志、RPC 命令、
协议方法/事件/可选字段；改动内部级别（§2.10）的任何内容；按规范
协商的 ACP/MCP 上游驱动变更（§5）。

### 4.2 弃用窗口

当稳定级界面必须以破坏性方式变更时：

1. **版本 N（MINOR）**：旧行为继续工作；发出弃用警告（视情况使用
   stderr/日志/CLI 提示），CHANGELOG 条目标记为 **DEPRECATED**。
2. **版本 N+1 或更晚**：移除落地，在 CHANGELOG 的 "Breaking" 标题
   下显著说明。

窗口至少为一个完整的 minor 周期。安全驱动的移除可以缩短窗口，但仍
必须附带 CHANGELOG "Breaking" 条目和迁移说明。

### 4.3 协议升级流程（新增 v2）

针对 CBOR 远程协议或扩展协议：

1. 在旧常量旁定义新版本常量（`PROTOCOL_VERSION = 2`）；v1 编解码
   路径保持编译在内。
2. **双栈过渡**：较新一侧必须对 v1 对端说 v1，持续至少一个 minor
   周期，通过现有的 hello/initialize 握手选择版本（两种协议在连接时
   都已在交换版本 —— `crates/tack-protocol/src/client.rs`、
   `crates/tack-ext/src/process.rs`）。
3. **版本检测 + 明确报错**：无法服务的对端必须收到显式的版本错误
   （`ProtocolErrorCode::Version` / "请升级宿主/客户端"消息），绝不
   允许挂起或解码失败。
4. 在 MAJOR 版本中发布该升级，附带 CHANGELOG "Breaking" 条目并
   更新本文档。

---

## 5. 跟随上游的接口

ACP（§2.4）、MCP（§2.7）、Claude Code hooks（§2.6）以及线上级对齐
目标（TS pi 的会话格式、RPC 线上形状、CBOR 分帧）属于
**与外部生态兼容**的界面。对它们的策略：

- 跟随上游的**稳定**发布（agent-client-protocol crate、rmcp crate /
  MCP 规范、Claude Code hook schema、TS pi）。不跟踪上游 nightly。
- 对齐状态和上游基线在 `docs/upstream-alignment.md` 中跟踪；每次
  上游同步都更新该文件。
- 这些界面上由上游驱动的破坏性变更会被审慎采纳、记录在 CHANGELOG
  中；在规范支持协商的地方（ACP、MCP），Tack 选择协商而非硬失败。

---

## 6. 分发与 crates.io

Tack 以单二进制分发（GitHub Releases、`tack update` 自更新 —— 见
`docs/release.md` 和 `crates/tack-app/src/self_update.rs`）。没有任何
workspace crate 发布到 crates.io（任何 `Cargo.toml` 中都没有
`publish` 字段；内部依赖只有路径依赖），因此 **Rust API 没有 semver
承诺**（§2.10）。版本号的兼容性含义只适用于 §2.1 到 §2.9 的界面。

---

## 7. 维护本文档

- **改动接口的人，在同一个 PR 中更新本文档。** 触及 §2 所列任何界面
  却没有配套兼容性评估的 PR 是不完整的。
- 拿不准某个改动是否破坏性时，按 §4.1 处理，提升 MAJOR，而不是勉强
  论证成 MINOR。
- 本策略中可机器检查的部分尽可能被强制：
  `evals/docs-audit/static_check.sh` 会把文档中的 settings keys、
  hook 事件、CLI 标志和 `features.*` keys 与代码交叉核对。
- 每次发布，发布负责人核对：CHANGELOG "Breaking" 条目 ⇔ MAJOR 提升；
  超过一个 minor 周期的 DEPRECATED 条目要么移除，要么明确重新论证
  保留理由。
