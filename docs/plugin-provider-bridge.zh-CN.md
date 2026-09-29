# P7：一等 Provider 桥

**[English](plugin-provider-bridge.md) | 简体中文**

> 状态：**已落地**。本文档是 P7 里程碑的设计记录；已发布行为的
> 规范描述在 [plugin-system.zh-CN.md](plugin-system.zh-CN.md) §3.1d，
> 协议新增记录在 [compatibility.zh-CN.md](compatibility.zh-CN.md) §2.5。
> 落地时的实现决策（§8 的开放问题以及与下文草图的小偏差）：
>
> - **取消转发**：宿主的取消令牌作为单独的进程内参数传给
>   `ProviderStreamBridge::stream`（它永不过线——§4.1 的排除不变），
>   因此桥在同一处拥有取消监听、`provider/streamCancel` 与宽限期
>   合成。宽限期：按倾向定为 **5s**。
> - **成本（开放问题 2）**：透传——采纳终止 `AssistantMessage`
>   上报的 `usage`；宿主不根据模型的静态 `cost` 声明重新计算
>   （与原生 codebuddy provider 的姿态一致）。
> - **空闲看门狗（开放问题 1）**：v1 不设宿主侧墙钟上限——插件
>   自己拥有其后端的策略，与原生适配器一致。
> - **SDK 面**：SDK 额外获得了 `on_ready` 启动 hook（Rust
>   `PluginBuilder::on_ready`、TS `.onReady`、Python `.on_ready`）——
>   provider 插件所需的注册入口；没有它，provider 插件在被使用前
>   没有机会注册。
> - **死亡监听**改为轮询连接存活（200ms 节奏，沿用 MCP 载体的
>   先例），而不是 `wait_dead`——其 pump-handle 获取是单等待者的。
> - **加固（落地后）**：plain（非桥）注册同样做能力门控——
>   `capabilities.provider.register`——走同一策略/审计路径；内建 id
>   冲突被拒绝，`apiKeyEnv` 不再从宿主环境解析（§7）。

## 1. 缺口

今天的 `host/registerProvider` 注册的是**注册表条目，仅此而已**。载荷是
`RuntimeProviderSpec`（`tack-ai/src/providers.rs`）：

```rust
pub struct RuntimeProviderSpec {
    pub id: String,
    pub base_url: String,
    /// 线协议 id（`openai-completions`、`anthropic-messages`……）。
    pub api: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<serde_json::Value>,
    pub models: Vec<CustomModel>,
}
```

`register_runtime_provider` 构造一个 `CustomProviderModels` 条目；随后由
host 内置的 **HTTP** adapter 对着 `baseUrl` 做推理。整个 v3 表面**没有任何
host→plugin 的推理回调**（host→plugin 方法只有 `tools/execute`、
`commands/invoke`、`hooks/*`、`approval/review`、`autocomplete/provide`、
`events/lifecycle`、`widgets/action`——没有 `provider/*`）。

两个后果：

1. **非 HTTP API 的 provider 做不了插件。** 本地 CLI/agent 桥
   （CodeBuddy 式：spawn CLI、说 JSONL、把工具桥回来）、进程内厂商 SDK、
   或鉴权代理网关，今天只有两条路：写在 crate 内的 `Provider` 实现
   （`tack-ai/src/codebuddy.rs`——3800 行，挂在 `CODEBUDDY_API` 上），
   或者起一个 localhost HTTP shim 翻译 host 已经会说的线协议——在用户和
   模型之间多一层流式解析/序列化，还叠着 host adapter 的重试/超时/错误
   映射。
2. **注册只在 TUI 可用。** print/rpc/acp 模式下 `host/registerProvider`
   返回 `ERR_METHOD_NOT_FOUND`（`tack-app/src/ext_headless.rs`，有测试
   钉死），所以连 shim 路线都退化成仅 TUI——而原生 provider 四种运行模式
   全部可用。

上游 TS pi 没有这个缺口：它的扩展 API 注册 provider 时给的是一个进程内
推理**函数**（`streamSimple`），这正是 pi-codebuddy-sdk 能做纯插件、无
HTTP 代理的原因。tack 的 `Provider` trait（`tack-ai/src/provider.rs`）是
完全对应物——`stream(model, context, options) -> AssistantMessageEventStream`
——但插件协议还够不到它。

**设计自由度**：当前没有发布过的插件，没有 v3 部署，没有任何历史包袱。
P7 直接把 v3 的 provider 面塑成最终形态——不要别名、不要迁移 shim、
不要考古式保留字段。（TS-pi 的线/存储兼容性无论如何都不受影响；它与
这个面不相交。见 §6。）

## 2. 目标与非目标

**目标**

- G1——插件可以**直接供推理**：收到 `(model, context, options)`，流式
  回传 `AssistantMessage` 事件。无 HTTP 跳转。
- G2——provider 插件在**全部四种运行模式**可用（TUI、print、rpc、
  acp），与原生 provider 一致。
- G3——**原生 UX 对等**：`/model` 列表、thinking 级别透传、abort
  （Esc）、rate-limit/警告上浮、usage/cost 上报。
- G4——provider 桥活在**与其他能力相同的加载结果、策略、审计模型**里
  （P3/P5/P6）：被策略拦截或禁用的插件什么都注册不了；一切可审计。
- G5——Rust/TS/Python 三 SDK **对等**，且 `ext dev` / `ext test` 能用
  scenario 在无会话的情况下驱动 provider 插件。

**非目标**

- **WIT 组件载体**上的推理——结构性不可行：world 不导入任何 WASI
  （无网络、无文件系统），且同步的按调用资源上限（fuel/epoch/墙钟）
  承载不了分钟级流。文档化为不支持，与该载体上 `approval/review` 的
  现状一致。
- **MCP 载体**上的 provider 桥——MCP 没有"为 host 供推理"的概念
  （sampling 是反方向）。
- 把内置 provider 迁到桥上。CodeBuddy 保持原生；桥是给第三方的。
  （未来某个原生 provider *可以*移出树外做 dogfood，但那是产品决策，
  不是 P7。）
- 模型列表推送通知。经 `host/registerProvider` 重注册（注册表按 id
  替换）已覆盖发现刷新与元数据学习，与原生 codebuddy provider 更新
  缓存的方式一致。

## 3. P7a——headless `registerProvider`（小解锁）

把 `register_runtime_provider` 接进 headless 宿主服务
（`ext_headless.rs`）的 print/rpc/acp 路径，替换掉钉死的
`ERR_METHOD_NOT_FOUND`。`session/*` 的模式门控保留（headless 会话没有
交互属主）；provider 注册与模式无关——它只是写进程级 runtime 注册表，
每个模式的模型解析本来就读它。

它自身的效果：HTTP-shim provider 插件在 headless 可用。它也是 P7b 桥
provider 的注册通路，所以先落地。

- 更新钉死的 headless 测试：`host/registerProvider` 成功且 provider
  出现在模型解析里；`session/*` 保持 `ERR_METHOD_NOT_FOUND`。
- ACP 注记：acp 以 headless 降级加载插件；注册之后，桥模型对 acp
  客户端可像任何 runtime provider 一样被选择。

## 4. P7b——provider 流式桥

### 4.1 协议新增（OpenRPC 为单一来源）

插件声明的能力（在 `InitializeResult.capabilities` 里）：

```json
"provider": { "register": true, "stream": true }
```

- `register`——插件以 **plain**（非桥）spec 调用
  `host/registerProvider`（HTTP-shim 运行时 provider）。
- `stream`——插件实现 `provider/stream`（桥注册，`bridge: true`）。

未声明 ⇒ host 永不调用 `provider/stream`，且对应注册被拒以
`ERR_CAPABILITY_NOT_GRANTED`——与其他能力同规则。两道门都会（有界、
fail-closed）等待握手答案，因此与插件自身 initialize 赛跑的注册由
已发布的能力决定，而不是由竞态决定。managed `pluginPolicy` 对
`provider` 的拒绝会为两个标志发布 `false`，并令声明该能力的插件在
加载时进入 policy-blocked（§4.6）。

新的 host→plugin 方法：

| 方法 | 形态 | 用途 |
|---|---|---|
| `provider/stream` | 请求 → `null` ack | 启动一条推理流。只做同步校验（能力、参数形状）；ack 之后的一切走事件通道。 |
| `provider/streamCancel` | 通知 | 中止在飞的流（host 取消 / 用户按 Esc）。 |

新的 plugin→host 通知：

| 方法 | 用途 |
|---|---|
| `provider/streamEvent` | 每条通知携带一个 `AssistantMessageEvent`，按 `streamId` 分路。恰一个终态事件（`Done`/`Error`）结束流。 |
| `provider/event` | （P7c，§5）provider 级带外事件：rate limit、警告。 |

`ProviderStreamParams`：

```json
{
  "streamId": "host 生成，每连接唯一",
  "model":    { "...": "解析后的注册表 Model 条目，JSON" },
  "context":  { "...": "完整会话 Context：系统提示、消息、工具" },
  "options":  {
    "maxTokens": 32768, "temperature": 1.0,
    "reasoning": "high", "thinkingBudgets": { "...": "..." },
    "toolChoice": "auto", "cacheRetention": "short",
    "sessionId": "…", "headers": {}, "samplingParams": {}
  }
}
```

`options` 是 tack-ai `StreamOptions` 的**可序列化子集**。有意排除：

- `apiKey`——桥 provider 自己管凭证（CLI 登录模型："工具在你的终端里
  能用，这里就能用"）。host 不为桥代理密钥。
- `cancel` / `retryCancel`——传输层关切。取消走
  `provider/streamCancel`；重试策略归插件（§4.2）。

`RegisterProviderParams.provider`（`RuntimeProviderSpec` 形状）新增一个
可选字段：

```json
{ "id": "acme-agent", "bridge": true, "models": [ … ] }
```

`bridge: true` 时：

- host 给每个模型指派保留 api 种类 **`ext-provider-bridge`**（`ext-`
  前缀与 `ext__` 工具命名约定一致）；显式冲突的 `api` 是注册错误；
- `baseUrl`/`apiKey`/`headers` 被忽略（不存在 HTTP 端点）；
- 一个插件可服务多个 provider id（各调一次 `registerProvider`）；桥
  注册表把 provider id 映射到服务连接。

schema 分工沿用 `RegisterProviderParams.provider` 先例：OpenRPC 文档
持有方法信封与能力形状；`model`/`context`/`event` 类型化为
provider 形/事件形 JSON（`{}`），由 tack-ai 的 serde 类型解析。为此
`AssistantMessageEvent` 要加 `Serialize`/`Deserialize` derive（今天
只有 `Clone + Debug`）；`Model`、`Context`、`AssistantMessage` 已经是
serde。

### 4.2 流式模型：事件走通知

v3 peer 强制 30 秒请求超时；推理要跑几分钟。所以 `provider/stream`
是快速 ack，本轮事件以 plugin→host `provider/streamEvent` 通知流动：

```
host                     plugin
 │── provider/stream ─────▶│  （ack：null，或能力/校验错误）
 │◀─ provider/streamEvent ─│  Start
 │◀─ provider/streamEvent ─│  ThinkingDelta、TextDelta、ToolCallEnd……
 │◀─ provider/streamEvent ─│  Done{message}        （终态，恰一个）
 │── provider/streamCancel▶│  （仅中止时；插件以 Error{Aborted} 收尾）
```

终态语义复用现有契约（`AssistantMessageEvent::is_terminal` /
`final_message`）：`Done` 携带最终消息，`Error` 携带
`stop_reason: Error | Aborted` 的错误消息。这保持了 `Provider` trait
契约——**流错误在带内**，永不是传输失败——因此 agent loop 对待桥
provider 与原生完全一致。

fail-open 综合：以下情况由 host 综合出终态 `Error` 事件——

- 载体在流中死亡（dead-peer fail-fast，v3 不变量）；
- `streamCancel` 后的宽限期（倾向 5 秒，实现时调）内插件没有自发
  终态事件；
- 插件违反协议（终态后还有事件、第二个终态、未知 `streamId`）——
  warn、审计，若流仍开着则综合 `Error`。

v1 **没有 host 侧墙钟上限**：原生 adapter 自己持有重试/超时策略，桥
插件同样如此（插件知道自己后端的限制；host 不知道）。空闲看门狗的
讨论见开放问题 §8.1。

### 4.3 Host 侧架构

依赖方向由 **trait 擦除**保持——与审批链同款模式
（`tack-app::approval` 的 `ApprovalReviewer` 擦除了 tack-ext 依赖）：

**tack-ai**（不依赖 tack-ext）：

```rust
/// 插件服务 provider 的保留 api 种类。
pub const EXT_PROVIDER_BRIDGE_API: &str = "ext-provider-bridge";

/// 一个桥 provider id 的服务端点。在 tack-app 里基于
/// `PluginConnection` 实现。
pub trait ProviderStreamBridge: Send + Sync + std::fmt::Debug {
    /// 启动流；事件投递到 `sink` 直到终态事件。此处返回的错误是
    /// ack 前（校验）错误。
    fn stream(
        &self,
        params: BridgeStreamParams,          // ProviderStreamParams 的 serde 镜像
        sink: AssistantMessageEventSender,   // 流的事件通道
    ) -> Result<(), String>;
    /// 尽力中止（host 取消）。
    fn cancel(&self, stream_id: &str);
}

pub fn register_provider_bridge(id: &str, bridge: Arc<dyn ProviderStreamBridge>);
pub fn unregister_provider_bridge(id: &str);
```

`provider_for(model)` 新增分支：`EXT_PROVIDER_BRIDGE_API` →
`BridgedProvider { provider_id }`——一个 `Provider` 实现，在 `stream()`
时查注册表（插件重载后重注册即可拾取新连接），把 sink 接进
`AssistantMessageEventStream`，并把一切失败转成带内 `Error` 事件。

**tack-app**：

- `ExtProviderBridge` 基于 `Arc<dyn PluginConnection>` 实现
  `ProviderStreamBridge`。`PluginConnection` 新增
  `provider_stream` / `provider_stream_cancel`；不能服务的载体返回
  现有 `unsupported_capability()`（-32002）。
- **事件路由**：宿主服务（TUI `extension_host.rs` 与
  `ext_headless.rs`）本就接收所有 plugin→host 流量。一个
  `StreamSinks` 注册表把 `(连接身份, streamId)` 映射到
  `AssistantMessageEventSender`；ack 返回前登记，终态/取消/载体死亡
  时清理。
- **生命周期**：`registerProvider(bridge: true)` 原子注册模型与桥。
  插件禁用/卸载/崩溃 ⇒ 两者都注销（runtime 注册表本就支持按 id
  替换/移除）；在飞流收到综合的带内 `Error`。被策略拦截或禁用的插件
  什么都注册不了——加载结果语义（P3）原样适用。
- **并发**：`streamId` 分路；子代理 loop 进程内共享父插件连接
  （`subagents.inheritPlugins` 决策），父子并发流在一条连接上自然
  复用。TS pi-codebuddy-sdk 曾为这个共享 `streamFn` 隐患搞了个
  `Symbol.for` 全局守卫；桥在设计上消除了它。

### 4.4 载体矩阵

| 载体 | `provider/stream` | 原因 |
|---|---|---|
| process | ✓ | 完全特权：spawn CLI、开 socket、持有厂商 SDK。 |
| WASI-stdio（wasm） | 协议层 ✓ | JSON-RPC 方法完全一致；实用要等网络能力授权（文档化的 v2.x 事项——今天沙箱模块够不到 LLM API）。 |
| WIT 组件 | ✗ `unsupported_capability` | 结构性：world 不导入 WASI（无网络），同步的按调用 fuel/epoch/墙钟上限承载不了长流。P7 **不动** world（`tack:plugin@0.3.0`）。 |
| MCP（Level 2） | ✗ `unsupported_capability` | MCP 没有供推理的概念（sampling 是反方向）。 |

非服务载体上的桥注册在注册时就被拒以明确错误；因此流路径在运行时
不会遇到 `unsupported_capability`。

### 4.5 SDK 表面（草图）

SDK 持有管线：streamId 作用域、ack/取消接线、**终态事件强制**——恰
一个终态事件；handler panic/抛异常或未给终态就返回时自动补 `Error`。

Rust（`tack-ext-sdk`）：

```rust
Plugin::builder("acme-provider")
    .provider_stream(|params, events, cx| async move {
        let mut turn = acme::query(&params.context).await?;
        while let Some(delta) = turn.next().await {
            events.text_delta(delta)?;
        }
        events.done(assistant_message)?;   // 或 events.error(...)
        Ok(())
    })
```

TypeScript（`@tack/plugin`）：`.providerStream(async (params, events, cx) => { … })`。
Python（`tack-plugin`）：`@plugin.provider_stream` 装饰器。

取消以 `cx.cancel` 信号呈现，handler 可轮询/等待；忽略它是合法的
（宽限期后 host 综合终态）但不提倡。

### 4.6 策略、审计、遥测

- **信任/模式门控**：注册沿用现有 `host/registerProvider` 门。这个
  门槛是诚实的：桥 provider 看得到完整对话——它*就是*模型。
- **P5 策略**：`capabilities.provider` 与工具一样在加载时收窄
  （initialize 求交集；managed 拒绝则插件转为 policy-blocked 并记
  `audit_narrow`）。managed `enabled` 钉值原样生效。
- **审计**：结构化 tracing target **`plugin_provider`**，覆盖注册、
  流开始/结束/取消、协议违规、综合终态——在 managed `auditSink` 的
  EnvFilter 里与 `plugin_policy` / `plugin_approval` /
  `plugin_metrics` / `plugin_load` 一起钉死 INFO。
- **遥测**：加载遥测统计 provider 桥插件；注册失败进
  `last-load.json`（doctor 读取；doctor 绝不 spawn 插件）。

### 4.7 开发回路

- `ext dev` scenario 格式新增 `providerStream` 步骤：脚本化
  `(model, context, options)` 进，断言事件序列出。DevHost 捕获
  `provider/streamEvent` 通知，可脚本化取消竞态。
- `ext inspect` 打印 provider 能力与桥注册。
- `tack-v3-demo-plugin` fixture 长出一个确定性假模型（可脚本化
  delta、thinking、工具调用、错误、慢流用于取消测试）——它成为整个
  桥测试套件的 e2e provider，也是插件作者的参考实现。

## 5. P7c——provider 事件与 usage/cost

原生 provider 能上浮带外状况——codebuddy 的 rate-limit 路径显示
TUI 内联警告加桌面通知（`tack-ai/src/codebuddy.rs` 的
`set_rate_limit_notifier`；headless 记日志）。P7c 把这个单 provider
钩子泛化为桥宽通道：

```json
provider/event  （plugin → host，通知）
{ "provider": "acme-agent",
  "kind": "rateLimited" | "warning" | "info",
  "message": "…", "detail": { "…": "可选结构化数据" } }
```

- tack-ai：`set_rate_limit_notifier` 泛化为 provider 事件通知器
  （kind + provider id + message）；codebuddy provider 改指向它，行为
  不变。
- TUI：内联警告 + 桌面通知（`notifications` 设置门控），headless 模式
  记日志——与原生路径一致。
- 审计：`plugin_provider` target。

**usage/cost**：终态 `AssistantMessage` 携带标准 `Usage`（token 加
`cost: UsageCost`）。桥 provider 报告其后端所报；订阅制后端 cost 报零、
token 数正常——与原生 codebuddy 语义完全一致。倾向：透传（插件是自己
计费模型的权威）；当插件报零时 host 是否按模型静态 `cost` 声明重算，
在实现时决定并写进落地注记。

## 6. 兼容性立场

- **v3 插件协议**：没有发布过的插件，没有部署——P7 直接重塑这个面，
  落地后按正常规则冻结。不要别名，不要迁移 shim。
- **TS-pi 兼容性**：不受影响。会话格式、RPC/ACP 线协议、provider
  注册表文件形状、CLI flag 都不与本改动相交。`RuntimeProviderSpec`
  新增一个可选字段；`models.json` 不变。
- **WIT world**：不变（`tack:plugin@0.3.0`）；组件载体不供推理
  （§4.4）。
- **命名了桥模型的会话**持久化其 provider/model id 与
  `ext-provider-bridge` api 种类；没有该插件时恢复会话走现有的
  模型未找到路径——与原生 provider 的 CLI 消失时行为相同。
- 落地时按惯例在 [compatibility.zh-CN.md](compatibility.zh-CN.md) 记
  一笔。

## 7. 安全与信任

- 注册是信任与模式门控（现有 `registerProvider` 门）加**两条路径**
  的能力门控（plain spec 要求 `capabilities.provider.register`，桥
  spec 要求 `capabilities.provider.stream`），上有 managed 策略收窄
  （§4.6）。
- **内建 id 冲突被拒绝。** id 与内建 provider 相同的运行时
  provider（plain 或桥）注册失败：影子会覆盖内建的 `baseUrl`，而
  宿主仍会把用户存储的凭据交给它——插件操纵的凭据外泄，与目录供给
  的 `baseUrl` 被防范的是同一类攻击。
- **`apiKeyEnv` 不从宿主环境解析**（对运行时 provider 而言）：插件
  选择的变量名配上插件选择的 `baseUrl` 会收割宿主凭据。运行时
  provider 必须在 `apiKey` 中显式携带密钥（或不带）。
- **`provider/event` 所有权**：插件只能为它实际注册过的 provider id
  发事件；其余一律丢弃并记审计警告（不能伪造 `anthropic` 的限速
  警告）。
- plain 注册与桥注册共用同一死亡监听：崩溃或被关闭的插件等同于
  没有注册任何东西（load-outcome 语义）。
- 桥不授予插件**任何新的 host 特权**：process 载体本就完全特权；
  `exec/run` 保持独立的信任门控；除 §4.1/§5 外没有新的双向表面。
- 不可信内容防御与模型来源无关：桥模型的工具调用是普通
  `AssistantMessage` 内容，流经同一权限层——声明式 deny、模式门、
  审批链、弹窗全部原样适用；web/MCP 不可信上下文规则继续把改写型
  调用留在人类路径上。
- 恶意或有 bug 的插件只能作践自己的 provider（坏事件、挂起），影响
  不到别的：协议违规按流隔离、被审计、fail-open 成带内 `Error`。

## 8. 里程碑、测试、开放问题

**排序**：P7a（headless `registerProvider`）很小，先落地。P7b 是大头
——schema/codegen → tack-ai 分发 → `PluginConnection` 与 host 路由
→ 三个 SDK → 开发回路。P7c 跟在 P7b 后，很小。

**测试计划**（仓库门禁：`clippy --all-targets` 0 警告、`fmt`、
`rustdoc -D warnings`、`xtask codegen --check`、docs-audit）：

- 新 schema 的 rpc3 codegen 往返；在既有的 duplex + `HostClient` +
  脚本化 peer 模式上做 peer 级流式 e2e。
- `BridgedProvider`：载体死亡/取消宽限/协议违规下的终态综合；带内
  错误映射；并发 `streamId` 分路；重注册后的注册表查找。
- Host：加载/禁用/崩溃下的注册与注销；四种运行模式；策略收窄；审计
  事件；headless 注册测试替换钉死的 `ERR_METHOD_NOT_FOUND` 测试。
- SDK：每语言 e2e 对齐现有套件（Rust 12 / TS 7 / Python 8），由假
  模型 demo fixture 驱动；`ext dev`/`ext test` scenario 步骤含取消
  竞态。
- 测试不触网，遵守仓库规则——假模型在进程内。

**开放问题**

1. **空闲看门狗**：host 是否该为 N 分钟无事件的流综合 `Error`？
   倾向 **v1 不做**——插件持有自己后端的策略，原生 adapter 也没有
   这种 host 侧上限。有了实战经验再议。
2. **cost 重算**：插件报零 cost 时透传还是 host 重算（§5）。倾向
   透传；落地注记里定。
3. **WASI 网络授权**时间线：决定 wasm 载体何时对桥实用（协议从
   第一天就就绪）。
4. **模型列表推送**：`provider/modelsChanged` 通知让插件不重注册
   就刷新模型列表。倾向不必要——注册表按 id 替换已经能用，且与
   原生学习流程一致。
