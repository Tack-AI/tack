# Message v4 JSONL 协议规范

**[English](session-v4-protocol.md) | 简体中文**

本文档定义 **message v4** 会话文件的线格式（wire format），是读取方与
写入方的一致性（conformance）依据。它只描述"一个合法 v4 文件长什么样、
读写双方必须遵守什么"，不包含设计取舍（见
`crates/tack-session/V4_NOTES.md`）与 legacy 格式迁移过程（§12 附录仅
给出识别规则）。

- 格式版本：`v = 4`；存储布局版本：`storageVersion = 1`
- 参考实现：`crates/tack-session/src/v4/{types,codec,store,fork}.rs`、
  `crates/tack-session/src/fork_policy.rs`

---

## 1. 术语与文件模型

**会话文件**是按行组织的 JSONL 文本文件，UTF-8 编码。

文件在逻辑上是一份**事务日志（transaction log）**：

- 第 1 行为 **header**（§3）；
- 其后每行为一个 **事务（transaction）**：一个 **write** 对象，或一个
  由 write 组成的数组（数组内 write 属于同一事务，空数组合法）。

读取方通过**重放（replay）**从事务日志折叠出会话的当前状态：

1. **entry 集合**：不可变事件（消息、压缩检查点……），以
   `id`/`parentId` 构成一棵树（或多棵树的森林），全局以 `seq` 定序；
2. **usage 账本**：usage 行的只追加序列；
3. **scalar 当前值**：每个 `(namespace, key)` 地址的最新值；
4. **list 当前内容**：每个 `(namespace, key)` 地址上，最后一次
   `delete` 之后 `append` 的元素序列（按 `seq` 升序）。

## 2. 行格式

- 每行以 `\n` 终止。解析前可去掉行尾空白。
- **撕裂尾（torn tail）**：文件最末一行缺少 `\n` 终止符时，该行必须被
  丢弃，不参与重放（真实写入方总会终止行；该规则容忍崩溃截断）。
- 除撕裂尾外，任何一行解析失败（非法 JSON、非法 write、校验违反）都
  意味着**整个文件损坏**——读取方必须整体拒绝，不得跳过单行继续。
- 空行（仅含空白）非法。

## 3. Header

第 1 行必须是 header 对象：

```json
{"v":4,"kind":"header","id":"s1","storageVersion":1,"createdAt":1767225600000,"cwd":"/work","parentSessionId":"s0","nextSeq":15}
```

| 字段 | 类型 | 必填 | 约束与含义 |
|---|---|---|---|
| `v` | int | ✓ | 恒为 `4` |
| `kind` | string | ✓ | 恒为 `"header"` |
| `id` | string | ✓ | 会话 id |
| `storageVersion` | int | ✓ | ≥ 1；本规范仅定义 `1`，其他值必须拒绝打开 |
| `createdAt` | int | ✓ | ≥ 0，创建时间（Unix 纪元毫秒） |
| `cwd` | string | ✓ | 会话所属工作目录 |
| `parentSessionId` | string | — | fork 来源会话 id |
| `legacyParentSessionPath` | string | — | 父会话 id 不可解析时保留的文件路径（迁移兜底） |
| `nextSeq` | int | — | ≥ 1；序列号高水位（§5.2） |

识别：`kind == "header" && v == 4` 且上表类型约束全部满足。**未识别字段
必须被忽略**（前向兼容）。header 永远以明文出现（§10 加密仅作用于
事务行）。

## 4. Write 类型

每个 write 是一个 JSON 对象，以 `kind` 字段判别，共 4 种：`entry`、
`usage`、`value`、`list`。未知 `kind` 属于文件损坏。

### 4.1 `entry` — 不可变事件

entry write 是拍平结构：`{kind:"entry", ...Entry}`。Entry 以 `type`
判别，携带公共结构字段（`EntryBase`）：

| 字段 | 类型 | 含义 |
|---|---|---|
| `id` | string | entry id（与 usage 行 id 同一命名空间） |
| `parentId` | string \| null | 父 entry；`null` 表示树根 |
| `seq` | int | 会话级序列号（§5.1） |
| `timestamp` | int | ≥ 0，提交时间（纪元毫秒） |

未知 `type` 属于文件损坏。共 4 种 entry：

#### 4.1.1 `message`

```json
{"kind":"entry","type":"message","id":"e1","parentId":null,"seq":2,"timestamp":1767225601000,
 "message":{"role":"user","content":"hi","timestamp":1767225600999}}
```

| 字段 | 类型 | 含义 |
|---|---|---|
| `message` | AgentMessage | 消息载荷（见 §4.1.5） |
| `terminate` | `true` | 可选；泳道终止标记，仅在 `true` 时出现 |

#### 4.1.2 `compaction`

```json
{"kind":"entry","type":"compaction","id":"c1","parentId":"e9","seq":10,"timestamp":1767225610000,
 "summary":"...","retainedTail":[{"role":"user","content":"kept","timestamp":1}],
 "tokensBefore":12000,"fromHook":false}
```

| 字段 | 类型 | 含义 |
|---|---|---|
| `summary` | string | 压缩摘要 |
| `retainedTail` | AgentMessage[] | 物化的保留消息（检查点自足，读取方无需回查 entry 树） |
| `tokensBefore` | int | 压缩前的上下文 token 数 |
| `details` | any | 可选扩展载荷 |
| `usage` | Usage | 可选；压缩这次 LLM 调用的用量（§4.2.1） |
| `fromHook` | bool | 是否由 hook 产生 |

上下文投影规则：`summary` 在前、`retainedTail` 随后，二者一起替代被
压缩的历史。

#### 4.1.3 `branch_summary`

```json
{"kind":"entry","type":"branch_summary","id":"b1","parentId":"e9","seq":11,"timestamp":1767225611000,
 "fromId":"e5","summary":"...","fromHook":false}
```

| 字段 | 类型 | 含义 |
|---|---|---|
| `fromId` | string \| null | 摘要分叉自哪个 entry；`null` = 树根 |
| `summary` | string | 摘要文本（空串的 entry 不投影出上下文消息） |
| `details` / `usage` / `fromHook` | — | 同 4.1.2 |

#### 4.1.4 `custom`

```json
{"kind":"entry","type":"custom","id":"x1","parentId":"e9","seq":12,"timestamp":1767225612000,
 "customType":"model_change","data":{"provider":"anthropic","modelId":"claude"}}
```

| 字段 | 类型 | 含义 |
|---|---|---|
| `customType` | string | 应用定义的类型标签 |
| `data` | any | 可选应用载荷 |

存储层不解释 custom 条目；是否投影进上下文由应用决定。
tack 约定（读取方应原样保留，无义务解释）的 customType：
`model_change`（`{provider, modelId}`）、`thinking_level_change`
（`{thinkingLevel}`）、`session_info`（`{name}`）、`label`
（`{targetId, label}`）。

#### 4.1.5 AgentMessage

`message` 载荷与 `retainedTail` 元素是 `AgentMessage`：以 `role` 判别
的消息联合（`user` / `assistant` / `toolResult` / `system` / `custom` /
`bashExecution` / `branchSummary` / `compactionSummary`），其内容块、
usage 与时间戳 schema 定义于 `crates/tack-agent-core/src/message.rs`
（与上游 pi 的 message 线格式一致，本文不展开）。

### 4.2 `usage` — 用量账本行

```json
{"kind":"usage","id":"u1","seq":13,"usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"entryId":"e2","adjustment":false}
```

| 字段 | 类型 | 含义 |
|---|---|---|
| `id` | string | 行 id（与 entry id 同一命名空间） |
| `seq` | int | 会话级序列号 |
| `usage` | Usage | 见 §4.2.1 |
| `entryId` | string | 可选；归属的 entry |
| `adjustment` | bool | `true` = 账本修正（如格式导入行），`false` = 新消耗 |
| `details` | any | 可选扩展载荷（导入行约定 `{source:"v3-import"}`） |

账本行不进 entry 树；会话级用量统计 = 账本行 `usage` 之和。entry 载荷
内的 `usage` 字段只是数据，不进统计。

#### 4.2.1 Usage

```json
{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"cacheWrite1h":0,"reasoning":0,
 "totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}}
```

整数字段为 token 数；`cacheWrite1h`、`reasoning` 可选（缺省 = 不统计该
类别）；`cost.*` 为浮点金额。

### 4.3 `value` — scalar 当前状态

```json
{"kind":"value","op":"set","seq":14,"namespace":"tack.branch.tip","key":"main","value":"e9"}
{"kind":"value","op":"delete","seq":15,"namespace":"tack.entry.label","key":"e3"}
```

| 字段 | 类型 | 含义 |
|---|---|---|
| `op` | `"set"` \| `"delete"` | 其他值属于文件损坏 |
| `seq` | int | 会话级序列号 |
| `namespace` | string | 命名空间（§7） |
| `key` | string | 地址键；空字符串表示单例地址 |
| `value` | any | 仅 `set`；任意 JSON 值 |

地址的当前值 = 重放中最后一个 set/delete 决定（delete 后该地址无当前
值；再次 set 恢复）。

### 4.4 `list` — 列表当前状态

```json
{"kind":"list","op":"append","seq":16,"namespace":"app.log","key":"k","value":{"n":1}}
{"kind":"list","op":"delete","seq":17,"namespace":"app.log","key":"k"}
```

字段同 4.3。`append` 追加一个元素；`delete` 清空整个列表。**存活元素**
= 最后一次 `delete` 之后 `append` 的全部元素（按 `seq` 升序）；从未
`delete` 时 = 全部 `append`。

## 5. 不变量

### 5.1 序列号（seq）

- `seq` 是**会话级**整数序列，从 1 开始，由写入方在提交时分配；一个
  事务内的多个 write 取连续值。
- 重放时所有 write（含 entry/usage/value/list）的 `seq` 必须**全局严格
  递增**。
- 每个 write `seq ≥ 1`。

### 5.2 `nextSeq` 高水位

快照式重写（fork 等只复制存活 write 的过程）会在目标 header 写入
`nextSeq`。读取方打开后，下一个可分配 seq 为
`max(已见最大 seq + 1, header.nextSeq)`。它保证重写后的文件不会复用
源文件已消耗过的序列号。

### 5.3 id 唯一性与 parent 完整性

- 所有 entry/usage 的 `id` 全局唯一（跨两种 kind 同一命名空间）。
- entry 的 `parentId` 为 `null`，或指向一个已重放存在的 entry；同
  一事务内先出现的父 entry 合法（父 seq 因此总小于子 seq）。

### 5.4 时间戳

- `timestamp` 只出现在 entry write 上，纪元毫秒。usage/value/list 不
  携带时间戳。
- 消息载荷（AgentMessage）内部的时间戳同为纪元毫秒，与 entry 的提交
  时间戳独立。

### 5.5 重放校验清单

读取方必须按事务顺序执行以下校验，任一违反即文件损坏：

1. 行是合法 JSON，且为对象或对象数组；
2. 每个 write 的 `kind`、`op` 是本规范枚举值，`seq ≥ 1`；
3. `seq` 全局严格递增（§5.1）；
4. id 唯一、parent 完整（§5.3）；
5. header `storageVersion` 受支持（§3）。

只读的场景（列表、搜索、统计）可以使用**宽松重放**：跳过无法解析的
行、不校验不变量，但不得把宽松读取的结果用于写回。

## 6. 写入方规则

1. 所有变更以一个事务一行追加；多 write 的事务必须原子落盘（不可分割
   在两个事务中）。
2. 创建文件时先写 header，再写初始事务（可为空，即只有 header）。
3. 快照式重写（fork、任何形式的整文件重写）必须：先写临时文件再原子
   改名；header 写入正确的 `nextSeq`（§5.2）。
4. entry 的 `parentId` 在写入时必须满足 §5.3。

## 7. 命名空间

当前状态按 `namespace` 字符串寻址。命名空间分两类：

- **保留命名空间**：前缀 `tack.`（tack）或 `pi.`（上游 pi）。两者布局
  相同、前缀不同；实现**不得**向对方的保留前缀写入（读取回退规则见
  §7.2）。
- **应用命名空间**：其他任意字符串，应用自行解释。

### 7.1 保留地址表

| namespace（`tack.`/`pi.` 前缀） | key | value 类型 | 含义 |
|---|---|---|---|
| `.session.name` | `""` | string | 会话显示名 |
| `.entry.label` | entryId | string | entry 标签 |
| `.branch.tip` | branch 名 | string \| null | 分支 tip 的 entry id；null = 空分支 |
| `.lane.config` | lane 名 | LaneConfiguration | 泳道配置 |
| `.lane.state` | lane 名 | LaneState | 泳道运行状态 |
| `.result` | operationId | OperationResultRecord | 已结束 operation 的结果 |
| `.op.*` | 由实现定义 | operation 运行态 |
| `.pending.*` | 由实现定义 | 待物化/挂起状态 |

```
LaneConfiguration = {"model":{"provider":string,"modelId":string},
                     "thinkingLevel":string,"activeToolNames":string[]}
LaneState         = {"currentOperationId":string|null,
                     "lastOperationId":string|null,
                     "inbox":[{"entryId":string,"kind":string}]}
idle LaneState    = {"currentOperationId":null,"lastOperationId":null,"inbox":[]}
```

**branch**：entry 树（§1）上的一条具名路径，`.branch.tip/{branch}` 指向
其 tip；主分支名约定为 `"main"`。**完整 lane** = 同名 branch tip 行 +
lane config + lane state 三者齐备；支持 lane 的写入方应在一个事务内
原子创建三者。

### 7.2 跨前缀读取回退（tack 实现）

tack 打开只含 `pi.*` 行的会话时，以下读取在 `tack.*` 行缺失时回退到
`pi.*` 行：branch tip、lane config、session name、entry label；
`tack.*` 行一旦存在即优先。tack 永不写 `pi.*`——混合文件里 pi 前缀的
tip 行会随 tack 追加而陈旧，重新交给 pi 打开时将回退到该旧 tip（已声明
的跨实现限制）。

## 8. Fork 契约

fork 把源会话投影进一个新 v4 文件（源不被修改）：

- 复制的 write **保留原 seq**；目标 header 写入
  `parentSessionId = 源 id` 与源的高水位 `nextSeq`（§5.2）。
- entry 选择：
  - **`tree` scope**：复制全部 entry；
  - **`branch` scope**：源分支必须是**完整 lane**（§7.1），只复制其
    tip 的祖先链；`entryId` 默认取 tip，`position:"before"` 切到该
    entry 的父（可产生 null tip）。
- 当前状态投影是**封闭分类器**——对 `tack.*` 与 `pi.*` 前缀分别适用
  同一套规则（下表）；usage 账本行永不复制；已删除（非存活）的历史行
  不参与投影。

| namespace | tree scope | branch scope |
|---|---|---|
| `*.session.name` | 复制 | 复制 |
| `*.entry.label` | 仅当其 entry 被复制 | 同左 |
| `*.branch.tip` | 全部分支原样 | 仅该分支，值重写为切割点（可为 null） |
| `*.lane.config` | 全部泳道 | 仅该泳道 |
| `*.lane.state` | 全部泳道，值重置为 idle | 仅该泳道，值重置为 idle |
| `*.result`、`*.op.*`、`*.pending.*` | **排除** | **排除** |
| 其他以 `tack`/`tack.` 或 `pi`/`pi.` 开头的 namespace | **fork 失败**（fail-closed） | 同左 |
| 应用 namespace | 复制 | 排除 |

fail-closed 是本契约的一部分：为保留前缀引入新 namespace 的实现，必须
同时在分类器中声明其 fork 语义，否则含该 namespace 存活状态的会话
fork 必须失败，而不是被静默复制或丢弃。

## 9. 加密层（tack 扩展）

加密对协议是**透明的行级封装**：

- header 永远明文；事务行可整行加密为 `tack-enc:v1:<payload>`（算法与
  密钥派生见 `crates/tack-session/src/crypto.rs`）。
- 读取方先按行识别并解密，再按本规范解析；无法解密的加密行（无密钥、
  密钥错误、密文被篡改）必须使整个文件以"加密无法读取"失败，而不是
  跳过或按损坏处理。
- 写入方启用加密时，所有事务行（含初始事务与快照重写的产物）都必须
  加密；加密失败则该事务不得落盘——绝不降级为明文。
- 不持密钥的读取方可以把 `tack-enc:v1:` 行当作不可解析行：按 §2，这
  意味着整个文件不可用（对 pi 等无加密概念的实现即"文件损坏"）。

## 10. 版本与扩展规则

- `v` / `storageVersion` 双重版本：格式（v4）之外，布局变更通过
  `storageVersion` 表达；不支持的版本必须拒绝打开（§3）。
- 对象上的**未识别字段**必须被忽略（header、write、entry、载荷皆然）。
- 未识别的 `kind`、`type`、`op` 枚举值是**文件损坏**（它们无法安全地
  被忽略）。
- 新保留 namespace 必须经 §8 的 fail-closed 规则登记 fork 语义。

## 11. 一致性要求（conformance）

**合法读取方**：满足 §2 行规则、§3 header 识别、§5.5 校验清单、
§5.2 `nextSeq` 推进、§8 fork 投影表；支持 §9 时可声明加密能力。

**合法写入方**：满足 §5 全部不变量、§6 写入规则、§8 快照重写要求；
不向对方保留前缀写入（§7）。

## 12. 附录

### A. 完整示例

```jsonl
{"v":4,"kind":"header","id":"s1","storageVersion":1,"createdAt":1767225600000,"cwd":"/work"}
{"kind":"value","op":"set","seq":1,"namespace":"tack.branch.tip","key":"main","value":null}
[{"kind":"entry","type":"message","id":"a1b2c3d4","parentId":null,"seq":2,"timestamp":1767225601000,"message":{"role":"user","content":"hi","timestamp":1767225600999}},{"kind":"value","op":"set","seq":3,"namespace":"tack.branch.tip","key":"main","value":"a1b2c3d4"}]
[{"kind":"entry","type":"custom","id":"e5f6a7b8","parentId":"a1b2c3d4","seq":4,"timestamp":1767225602000,"customType":"model_change","data":{"provider":"anthropic","modelId":"claude"}},{"kind":"value","op":"set","seq":5,"namespace":"tack.branch.tip","key":"main","value":"e5f6a7b8"}]
[{"kind":"entry","type":"custom","id":"c9d0e1f2","parentId":"e5f6a7b8","seq":6,"timestamp":1767225603000,"customType":"thinking_level_change","data":{"thinkingLevel":"high"}},{"kind":"value","op":"set","seq":7,"namespace":"tack.branch.tip","key":"main","value":"c9d0e1f2"},{"kind":"value","op":"set","seq":8,"namespace":"tack.lane.config","key":"main","value":{"model":{"provider":"anthropic","modelId":"claude"},"thinkingLevel":"high","activeToolNames":[]}},{"kind":"value","op":"set","seq":9,"namespace":"tack.lane.state","key":"main","value":{"currentOperationId":null,"lastOperationId":null,"inbox":[]}}]
{"kind":"usage","id":"d3e4f5a6","seq":10,"usage":{"input":12,"output":4,"cacheRead":0,"cacheWrite":0,"totalTokens":16,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"entryId":"a1b2c3d4","adjustment":false}
```

逐行：① header；② 创建时的空主分支 tip（裸对象事务）；③ 用户消息
+ tip 移动（数组事务）；④ model 变更（custom entry + tip 移动）；
⑤ thinking 变更——model 与 thinking 齐备，同事务镜像 lane config 与
idle lane state；⑥ 一条 usage 账本行。

### B. 非 v4 文件的识别

首行 `{"type":"session","version":3,...}` 为 legacy v3 文件（更早版本
先经 v3 链式升级）。v3 与 v4 是**不同格式**：v3 每行一条 entry 记录、
时间戳为 ISO 字符串、无事务/namespace 概念。v3 → v4 的迁移规则与实现
见 `crates/tack-session/V4_NOTES.md` 与 `src/v4/migrate.rs`，不属于本
规范。

### C. 参考实现

| 实现 | 位置 |
|---|---|
| tack（Rust） | `crates/tack-session/src/v4/{types,codec,store,fork}.rs`、`src/fork_policy.rs`、`src/v4_bridge.rs` |
| 上游 pi（TypeScript） | `packages/agent/src/harness/session/jsonl/*.ts`、`session/{commit,fork-policy,values}.ts` |

---

*协议版本：format v4 / storage v1。修改本规范请同步
`crates/tack-session/V4_NOTES.md` 与 `docs/compatibility.md` §2.1。*
