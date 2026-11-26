# TS pi → Tack 上游对齐追踪

**[English](upstream-alignment.md) | 简体中文**

追踪 [earendil-works/pi](https://github.com/earendil-works/pi)（TypeScript 原版）
的演进在 Tack 中的对齐情况。每次同步上游后更新本文档。

## 当前基线

| 项 | 值 |
| --- | --- |
| 上游仓库 | `/data/github/pi`（origin: earendil-works/pi） |
| 对齐到的 commit | `10d1ad621f9dc4b6c441221f14057a770d1d0fa2`（2026-09-21，`feat: add transactional replicated state`） |
| 对齐日期 | 2026-09-21 |
| Tack 版本 | v1.0.9 之后（未发布） |

## 同步流程

1. （自动）每周一 04:17 UTC，[.github/workflows/upstream-delta.yml](../.github/workflows/upstream-delta.yml)
   运行 `scripts/upstream-delta.sh` 生成上游增量审阅报告：有新增 commit 时
   追加到标题含 `[upstream-delta]` 的跟踪 issue（没有则新建），完整报告同时
   存为 workflow artifact。手工审阅从 issue 里的报告开始。
   本地手动生成报告：`UPSTREAM_REPO=/path/to/pi scripts/upstream-delta.sh`
   （`UPSTREAM_REPO` 指向上游 pi 的本地 clone；不设置则从 GitHub 克隆）。
2. 逐条评估报告中的 commit：可直接落地 / 借鉴设计 / 无需跟进（记录原因）。
   需要看原始提交细节时：`git -C "$UPSTREAM_REPO" fetch origin &&
   git -C "$UPSTREAM_REPO" log --oneline <上次基线>..origin/HEAD`。
3. 落地项完成后更新本文档基线与条目状态。
4. 模型目录（`crates/tack-ai/catalog.json`）从已发布的 `@earendil-works/pi-ai`
   npm 包转换（`npm pack @earendil-works/pi-ai@latest`，数据在
   `dist/providers/data/*.json`），注意发布后合入的路由规则需手工叠加
   （见 2026-09-08 的 Copilot 条目）。

## 2026-11-22 上游增量预审（10d1ad621 → 9c4652688）

本次是**针对性预审，不是已完成的对齐批次**。审阅范围包括 v0.87.0–v1.6.0
changelog、feature/fix 提交记录，以及与 Tack 实现的定点对比；并未逐条审阅
全部 commit。因此对齐基线仍保持 `10d1ad621`。本节条目除非明确列入落地表，
否则均不视为已落地。

| 项 | 值 |
| --- | --- |
| 预审目标 | `9c4652688590abb836b40ebd1e4725ecf9a7c7d1`（2026-11-22；v1.6.0 加发布后提交） |
| 距基线增量 | 1,684 个 commit（1,660 个非合并 commit） |
| 预审时的 Tack | v1.0.21，`09e887eec096a488a84108b52c49a43db8742cc1` |
| 验证程度 | 仅源码/changelog 对比；未做真实 provider 或真实终端验证 |

以下成本估算包含针对性实现与回归测试：**S** 为局部小改，**M** 跨模块或需要
平台验证，**L** 为多日/跨 crate 项目，**XL** 应有独立设计与交付计划。

### 同步自动化阻塞点（下一对齐批次前先修）

本地实际运行 `UPSTREAM_REPO=/Users/zugle/workspaces/github/pi
scripts/upstream-delta.sh`，失败于 `无法解析基线 commit（得到 ''）`。修复前，
英文 canonical 文档使用 `Aligned-to commit`，而 `scripts/upstream-delta.sh`
只 grep `对齐到的 commit`；`.github/workflows/upstream-delta.yml` 又没有覆盖
`UPSTREAM_BASE`，因此其报告步骤有相同故障。这可能解释了对齐基线为何停止
推进，但本次没有审计 workflow 运行历史，不能据此断定因果。

双语解析修复已记入下方“首批小修落地”表。扩展报告分组和“稳定主线 /
实验架构”视图已在第三批落地表记录；离线测试使用真实的分目录 commit，
而不是 mock 路径。

### 立即候选：正确性、安全性与 Provider 兼容

下表保留审阅时的现状判断；已落地项目的当前实现以文末各落地表为准。

| 上游 commit | 候选 | Tack 现状 / 实现注意点 | 成本 |
| --- | --- | --- | --- |
| `87f9a3641` + `1b2aa0ca0` | 最终工具参数严格完整性 | 流式预览可容错，但最终 JSON 畸形、Responses 调用缺 `output_item.done` 时不得执行。Tack 目前对最终参数仍调用 `parse_streaming_json()`，且没有 `argumentsError`；应保留原始参数供模型纠错，而不是执行修复后的前缀。 | M |
| `f715a15c1` | 增量流式 JSON 解析器 | Tack 的 coalescer 虽降低解析频率，Anthropic、OpenAI Completions/Responses、Bedrock、Mistral 等 adapter 仍会在 flush 时重解析累计文本。应实现共享增量解析器，并与上面的“流中容错 / 最终严格”区分一起落地，不能只搬性能半部分。 | L |
| `b82477b7b` + `7a6bd5a97` + `14b804d80` | OpenAI-compatible 请求兼容 | 非 OpenAI 的 instruction 消息默认用 `system`；工具禁用时省略空 `tools: []`；新增 `requiresNonNullAssistantContent`。Tack 仍有 tool history 时发送空数组，也缺后一个 compat 开关。可修 Groq、vLLM/LiteLLM 与代理 400。 | S–M |
| `406e719a6` + `4a6a58eb5` + `aa47cedce` | strict/untyped 与 Anthropic schema | 含无 `type` 节点时退出 strict；Anthropic 非 strict schema 保留根 `$defs`、`definitions`、`additionalProperties`。Tack 的 `convert_tools()` 当前重建窄根对象，可能破坏 MCP `$ref`，或丢失禁止额外参数的约束。 | M |
| `6fcb48670` + `441f5767c` | 摘要上限与 print 截断退出码 | 取消摘要固定输出上限，避免 reasoning 耗尽摘要额度；print 保留截断输出但告警并在 `stopReason=length` 时非零退出。Tack 有固定摘要预算，print 目前只有 `Error` 设置退出 1。 | S |
| `0a4fad548` + `d3398f826` + `73f8de80b` + `3dd803d7e` | overflow 识别与 output-token 钳制 | 为 OpenRouter tokenizer 高估预留余量；识别 LiteLLM、Copilot HTTP 413、Z.AI CN 的 overflow 形式，避免不必要的 400 与误 compaction。Tack 已有摘要重试和摘要禁缓存，不要重写这些部分。 | M |
| `a00a5048d` + `25190b72c` + `4699b5b26` | 重试分类 | 重试临时流内 provider 故障、空/不完整 gateway 响应和独立 socket 错误，同时排除永久配额错误。需同时覆盖 assistant classifier 与初始 HTTP 层，只改一层会留下不一致行为。 | M |
| `1d74741e1` + `bde882c74` + `b2cb62923` + `f10993bc7` | OAuth 刷新生命周期 | 跨进程串行刷新；调用方取消后仍保存 rotated token；退出时等待已启动刷新落盘；MCP 登录可取消。Tack 有进程内 provider 锁，但没有等价的跨进程/退出生命周期。 | M–L |
| `5806068c2` + `d850edee9` + `e529a82c9` + `9ffd27cfa` | MCP OAuth 存储与授权加固 | 凭证按服务器 name **和 URL** 隔离；支持 metadata URL override、RFC 9207 `iss`；step-up 保留 granted scopes；授权 URL 只允许 HTTP(S)。旧 token 项需要迁移，不能简单换 key 使其静默失效。 | M |

### 高价值功能与架构候选

| 优先级 | 上游 commit / 来源 | 候选与建议形态 | 成本 |
| --- | --- | --- | --- |
| 高——存储兼容 | `466db0fec` | **`ContextEditEntry` 与安全 context 边界。** 先实现读取/projection，再考虑写入：按分支隐藏/替换模型上下文而不改 raw history，并统一作用于 compaction、branch summary、失败恢复和 fork。拆分普通 `context` 与显式全量 `context_with_system`；Tack 当前 transform hook 暴露完整消息，且没有 context-edit projection。 | L |
| 高——新 agent 能力 | `8562bcf66`；`packages/codemode/README.md` | **非持久 Code Mode MVP。** 模型用 JavaScript 循环、分支、并发组合工具，中间结果留在脚本内，只有最终输出进入模型上下文。首版 opt-in，QuickJS/WASM，无 fs/net/env/timers，严格限制时间/内存/输出/并发/嵌套深度；嵌套调用统一走保留参数校验、hooks、权限、输出上限的 executor。复用 WASM 资源限制经验，不复用长期插件 ABI。确定性 journal replay 与 durable 恢复后置。 | XL |
| 高——延迟与工具控制 | `e029c3ed0` + `30a1d1849` + `ddaa0a034` + `04b97ef00` | **deferred MCP 连接与分层工具选择。** 首个 prompt 只等待 direct server；搜索/调用时再等待所需 deferred server，重连后恢复工具。增加 `+name`/`-name`、`*` 模式和禁用 MCP 的上游 CLI 开关；普通 `--tools` 不得意外清空 MCP 工具。 | L |
| 高——项目 MCP 配置 | `1387af7b4` + `91f9f3b5d` | 项目 MCP 条目可只覆盖全局 server 的 `enabled`/`exposure`/`toolExposure`；MCP server 可通过 `auth.provider` 复用 provider 登录。 | M |
| 中——缓存与成本 | `60ac3bcc3` + `fcc33744b` + `b271b0a52` + `caa6967b6` | GPT thinking level 切换及动态工具增删时保留 prompt cache。按协议使用 `configuration_update`、Anthropic inline additions、Responses 消息锚定 additional tools；历史编辑和工具重定义需要显式回退行为。 | L |
| 中——provider 目录与计费 | `ee409a85f` + `7c22a93d0` | 拉取 OpenRouter 按 API key 定制的模型、限额与价格；区分 provider 实报账单总额与目录估算。Tack 已有长上下文分级定价，不要重复实现。 | M |
| 中——provider continuation | `a758f3ae4` + `49fe0a237` | **Anthropic Message Threads。** 有效时只发送新增消息，早先 context/tool 变化后回退完整请求，并清理 per-session 资源。收益是减少重复上传与延迟，不是 token 单价下降。cache warming 和摘要不得触碰主 thread。 | L |
| 中——协议对齐 | `e473b5cd8` + `beffcde92` + `6aab551a7` + `e03dca7a3` | RPC prompt 以 additive 字段返回 `started`/`queued`/`handled` disposition；session replacement 的生命周期事件恰好一次，并先订阅再执行 startup handler，避免 JSON/RPC 客户端丢失 `session_start` 事件。 | M–L |
| 中——当前模型 | `a37306d43` + `a3f265b1c` + `139707002` + `3e4f87265` + `1692bba63` + `fe8a6f5dc` + `86918f9ee` | 内置目录刷新要与协议迁移一起：Azure Foundry Chat Completions、Kimi Coding 改 OpenAI Completions、Qwen 新网关与 Individual CN、Gemini 4/带前缀 Gemini 3+、Copilot reasoning metadata。只刷新 JSON 不够；`providers.rs` 仍有旧 Azure ID、Kimi 协议/base URL 和 Qwen host。 | M–L |
| 中——工具发现 | `ae823bc0b` + `cc44fa216` + `e033f255f` + `79006f061` + `6d3e342b2` | 改进 `tool_search`：名称优先与多 query term 已在 deferred MCP 批次落地；仍待完整名称/排名细化、Unicode 规范化，以及局部 `$ref` 后属性可搜索。 | M |
| 后期——workbench | p2 command menu/projects；`4839e41d6`、`5a14e69c0` | 用 Cmd/Ctrl+K 统一暴露已有内建/会话/模型/扩展命令；增加按 cwd/project 分组的 sidebar，显示 working/failed/unread/子代理状态。权限与命令可用性由 host 决定，不增加远程任意 CLI 执行接口。 | M–L |
| 后期——agent 产出 | p2 artifacts；`6f1a96e1d`、`da57c1662` | 增加版本化 Markdown/静态 HTML artifacts，支持 list/read/write/edit/show 与 transcript 引用。使用 session sidecar 或独立 store，不改 v4 格式。首版只做沙箱静态渲染；HTML eval、自由批注、共享插件 pane 后置。 | L |
| 后期——remote UI 基础 | Chord delta/sync 设计 | 在现有 CBOR 协议上增加单调 progress revision、`afterRevision`、有界操作日志、transcript window、snapshot reset/gap 恢复。只借鉴可恢复 watch 语义，不换成 Chord，也不宣称多 writer CRDT。 | L |
| 后期——durable 等待 | `473515f9c`；`packages/durable` | 选一个显式 park 的工具（webhook 或人工审批）做原型：typed signal inbox、持久 checkpoint/deadline、timeout/cancel/duplicate 处理与重启恢复。不要尝试冻结任意 bash、Tokio future 或运行中的子代理，也不承诺外部副作用 exactly-once。 | L–XL |

### 值得集中成批的低成本 TUI 与平台修复

| 上游 commit | 候选 | Tack 现状 |
| --- | --- | --- |
| `9e9609216` | bash 超长行后接空行时仍保留输出 | `truncate_tail()` 仅在累计 output 为空时保留超长行尾部；尾部空行可能因此抑制有用文本。同时检查 bash footer 的 `endLine` 与 `startLine`。 |
| `6100fe5a8` + `3bae85274` | 编辑器 Home/End 与 selector PageUp/PageDown | fullscreen 当前先于 editor/autocomplete/dialog 截获这些键。transcript 首尾改用 Ctrl+Home/Ctrl+End，打开的菜单优先处理选择键。 |
| `a635f390` | `/resume` 刷新/重命名/删除后选中稳定 | Tack 会重建 select list 并重置过滤选择；应按 path 保持身份，定义删除后的回退，不必移植上游 progressive-loading 机制。 |
| `be2744d95` | full redraw 后 Kitty 图片仍有效 | 主屏 ED2/ED3 可能使图片失效，而 upload cache 随后只发 placement。清屏时应失效并重传，或避免破坏性清屏。 |
| `672000c80c` + `4622edb79` + `4464442f9` + `1c8cb1781` | fullscreen 图片最后绘制；裁剪 anchor 在 viewport 上方的图片 | 分离文本清屏/输出与图片 placement，再增加有界图片高度元数据/搜索来生成裁剪 placement。需在 Kitty/WezTerm/Orca 实测，不能当成单终端修复。 |
| `e4f50d76d` + `3107a58d2` | 外部 SIGTSTP 走 Ctrl+Z 的终端恢复路径 | 必须包含 terminal ownership 后续语义：start 前不监听，恢复终端后再移除监听，不干扰外部编辑器。 |
| `31b407dc6` | Windows console close 的 shutdown cleanup | close/SIGHUP 走与正常退出相同且有时限的 shutdown。现有 Job Object 能回收多数子进程，但不能替代扩展/OAuth 清理。 |
| `b7bb7ca90` | fullscreen 滚动剩余性能优化 | Tack 已有 visible-window materialization 和有界 cache。只优化剩余的全量 offsets/spans 扫描（view-only frame、复用、二分），不要移植上游 readonly render array 的 breaking API。 |

### 已核对，无需立即重复移植

- `4438c58ba`（abort 批次补齐所有 tool result）：Tack 顺序与并行路径均已给未开始
  调用写入 `"Operation aborted"` 结果。可选小收尾是把取消检查提前到 lookup/hooks 前。
- `3a7bb6f48`（流式 read）：已有有界 `BufReader` 行窗口实现覆盖。
- `cb1205e46`（compaction 后极多 entry）：Tack 迭代构建 session path，不受 JavaScript
  spread/调用栈故障影响。
- `51ba3ab5b`（retry abort listener 堆积）：Rust cancellation token/future 没有 Node
  listener 保留机制。
- `5b5be2c3`（超大自渲染 tool result）：上游崩溃是 V8 spread 参数上限；Tack 使用 Rust
  迭代/extend。大渲染性能是独立问题，不是同一个 crash fix。

### 明确暂缓 / 不整体移植

- **p2**：README 明确称其为 experimental prototype，且“will be gone soon”。只借鉴
  command menu、project/sidebar、workflow、artifact 思路。
- **p3**：目前仍是 headless 骨架，不是完整继任产品。Tack 已有更丰富的 process/WASM
  插件载体；新 host capability 应增量 dogfood，不把核心重写为插件。
- **Chord / Durable**：不整体迁移。分别借鉴可恢复 watch 与显式 parked wait。
  Durable Code Mode replay 是更后期的独立项目。
- **Radius/cloud backup、通用 rooms/CRDT 协作、remote execution environment**：当前不作
  对齐承诺；现有 `tack serve` 仍是 remote-session 路径。
- **pi-schema**：主要解决 TypeScript/CSP/TypeBox 问题，不是 Tack 的独立产品功能。
- **agent 自写并自我应用生产插件**：不得绕过 Tack 的安装策略、审核和 WASM capability
  边界。

### 实现时的兼容约束

1. **Context edit**：写新 entry 前先增加 v3/v4 互操作 fixture。若忽略 context projection，
   仅把未知 entry 保留为 custom 并不够。覆盖 migration、fork、compaction、summary、usage。
2. **OAuth 存储**：name → name+URL 的 key 变化需要向后兼容迁移、跨进程
   read/modify/write 保护，以及按 symlink 解析后路径确定的锁身份。
3. **RPC**：disposition 等可选响应字段是 additive；生命周期事件顺序不是。为
   startup/new/switch/fork/clone/reload 和 JSON print 增加 golden trace，覆盖恰好一次订阅及
   handler 发出的事件。
4. **Thread/缓存保持**：compaction、context edit、system/tool 变化、summary、cache warming
   需要完整请求或 disable-continuation 回退，以及 session resource cleanup。
5. **Code Mode**：嵌套调用必须与顶层调用共享安全/权限路径。版本化 schema 设计前，
   程序用 structured result 不进入持久 wire；限制 fan-out、嵌套调用、输出和资源用量。
6. **Project/artifact**：使用 additive 协议元数据与 sidecar store。UI 分组、已读状态、
   artifact 版本不得改变 session v4。

### 建议交付顺序

1. 修复 delta 自动化，并增加解析/分组测试。
2. 以聚焦 PR 落地最终参数完整性、OpenAI/schema 兼容、摘要/print 修复及低成本 TUI 批次。
3. 加固 OAuth/MCP 生命周期与关闭清理。
4. 将共享增量 JSON 解析器作为独立性能项目交付。
5. 增加 ContextEdit 读兼容、deferred MCP 启动、provider/catalog 迁移及互操作测试。
6. 在 feature gate 后实现非持久 Code Mode MVP。
7. 若浏览器 workbench 是产品重点，依次增加 command menu/project sidebar、可恢复 watch、
   静态版本化 artifacts。

### 首批小修落地（未发布）

以下低风险项已在当前工作树实现，并有针对性回归测试，但尚未进入 tagged release。

| 上游 commit | Tack 落地 |
| --- | --- |
| delta 自动化阻塞点 | `scripts/upstream-delta.sh` 同时接受 `Aligned-to commit` 与 `对齐到的 commit`；新增离线 `scripts/upstream-delta.test.sh` 覆盖两种语言，并接入 `evals-selftest` CI job。扩展分组见第三批落地表。 |
| `9e9609216` | `truncate_tail()` 在超长行后只有空行时保留该行尾部及空行后缀，并保持字节/行数元数据一致；bash footer 使用部分行真实的 `start_line`。单测覆盖 60 KB/219 KB、LF/CRLF，另有真实 bash 工具回归测试。 |
| `441f5767c` | print 在 `StopReason::Length` 时保留部分 stdout，同时在 stderr 告警并退出 1，text/JSON 两种模式一致；Error 行为不变。独立的 `6fcb48670` 摘要预算项已记入下方第二批落地表。 |
| `6100fe5a8` + `3bae85274` 的局部 | 裸 Home/End 交给 fullscreen 编辑器；Ctrl+Home/Ctrl+End 控制 transcript 首尾；autocomplete 打开时优先接收 PageUp/PageDown。dialog 本就有输入优先级。其余自定义 selector 工作与上游 `capturesViewportInput` 组件 API 未移植。 |

### 第二批小修落地（未发布）

以下后续项也已实现并有针对性测试。

| 上游 commit | Tack 落地 |
| --- | --- |
| `b82477b7b` + `7a6bd5a97` + `14b804d80` | OpenAI 官方 API 之外的 instruction 消息默认使用 `system`；只有 `requiresToolsWithToolHistory` 显式开启时才发送空 `tools: []`；`requiresNonNullAssistantContent` 独立控制工具调用 assistant 回放使用 `""`。显式 `supportsDeveloperRole` 仍覆盖自动探测。请求级测试覆盖 OpenAI/Groq/OpenRouter role、vLLM 风格空 tools、两个 opt-in，以及 null 与空字符串 assistant content。 |
| `a635f390` 的局部 | `/resume` 刷新在 rename 和排序切换时按 path 保持选中会话；删除时选择下一行，末尾则选择前一行。Tack 是同步加载而非渐进加载，无需上游 pending-partial-list 机制。测试使用真实命名 session 文件。 |
| `6fcb48670` | compaction history、split-turn prefix、abandoned-branch summary 直接使用 `model.max_tokens`；模型上限为 0 时省略覆盖。`reserveTokens` 仅保留为 compaction 触发阈值。测试捕获 provider options，验证使用模型上限而非 reserve 派生预算。 |

### 第三批上游正确性修复落地（未发布）

以下 provider 完整性、兼容性、重试和报告项已在当前工作树实现，并有离线或
针对性测试覆盖。

| 上游 commit | Tack 落地 |
| --- | --- |
| delta 报告分组后续 | 报告显式分组 `packages/tui`、`mcp`、`codemode`、`chord`、`durable`、`apps/p2`、`apps/p2-mac`、`apps/p3`。第二个互不重复的视图区分稳定主线与触及 p2/p3/Chord/Durable 实验路径的 commit；跨目录实验 commit 归实验侧。离线测试创建各组真实 commit 并验证数量。 |
| `87f9a3641` + `1b2aa0ca0` | 流式参数继续作为容错预览；Anthropic、Bedrock、Mistral、OpenAI Completions、Responses 的已完成调用改为严格解析加字符串字面量修复。空/空白参数为 `{}`；非 object 和 malformed JSON 变为 `{}` 加可选 camelCase `argumentsError`，记录原因与原始文本。agent 在两种执行模式下均于参数准备、hooks、执行前把该错误返回为 ToolResult。Responses 在 `response.completed` 时若工具调用缺 `output_item.done`，现在返回带内 stream error。既有 length-stop 拒绝仍优先。 |
| `406e719a6` + `4a6a58eb5` + `aa47cedce` | strict 转换拒绝同时缺 `type`、`anyOf`、`enum` 的节点；`prefer` 回退非 strict，`require` 保留明确错误，enum-only 节点仍受支持。Anthropic 非 strict 转换只额外保留受支持的根字段 `additionalProperties`、`$defs`、`definitions`，保留局部 `$ref` 目标及额外参数约束，不扩大 strict schema 范围。 |
| `09f105957` + `0a4fad548` + `d3398f826` + `73f8de80b` + `3dd803d7e` | 共享 `streamSimple` 风格估算使用最近适用的 assistant usage 加尾部消息估算。Anthropic、Bedrock、Google、Mistral、OpenAI Completions、Responses 按 `max(4096, ceil(15% input))` 余量钳制调用方或模型输出上限，接近 context 上限时使用上游 32768 输出回退。overflow 新增 LiteLLM pre-call、Z.AI CN 文案及锚定 HTTP 413；正文仅提到 413 或数字 4130 不匹配，Cerebras 私有无正文规则收窄为 400。 |
| `a00a5048d` + `25190b72c` + `4699b5b26` | retry 覆盖临时 provider/gateway 故障、精确 Anthropic `api_error`、不完整响应及独立 Node socket code。quota/billing/subscription-sharing 和 Z.AI 1308 排除优先；HTTP 数字状态和 socket code 使用 ASCII token 边界。`send_with_retry` 现在读取错误正文，并与 assistant error 共用分类器：永久 HTTP 429 quota 不重试，临时 provider 正文可以恢复。 |

### 第四批 OAuth 与退出清理落地（未发布）

以下生命周期、凭据隔离、授权加固与退出清理项已实现，并配有离线 fixture
和进程级信号测试。

| 上游 commit | Tack 落地 |
| --- | --- |
| `bde882c74` + `b2cb62923` | provider OAuth 在跨进程锁与重读后分离执行刷新，调用方取消不会丢失轮换授权。条件提交避免旧响应覆盖重新登录或复活注销；命令正常退出会等待已跟踪刷新。回归测试取消真实进行中的请求并验证落盘，也验证迟到刷新不会复活注销。 |
| `1d74741e1` + `f10993bc7` | MCP 刷新以每 server 的进程内/跨进程锁覆盖重读 → 请求 → 保存，复用其他进程已更换的授权；被拒绝的 token 先刷新再回退浏览器；关闭时等待持久化。交互式发现、注册、回调与手动粘贴均可取消；`tack mcp login --timeout` 限制整个流程（默认 300 秒）。 |
| `5806068c2` + `d850edee9` + `e529a82c9` + `9ffd27cfa` | `mcp-tokens.json` 从旧 server 名 key 原子迁移为规范化 name+URL key，不让旧授权失效。`oauth.authServerMetadataUrl` 替代发现（除 loopback 外要求 HTTPS）；回调/手动粘贴的 `iss` 传给 rmcp 的 RFC 9207 校验；刷新/step-up 保留 granted scopes；授权 endpoint 仅允许 HTTP(S)。 |
| `31b407dc6` | SIGHUP/SIGTERM 与 Windows `CTRL_CLOSE_EVENT` 进入有界 TUI/print/RPC 清理，并有独立四秒兜底；终端恢复失败不再跳过扩展清理。RPC bash executor 与 prompt pump 在 teardown 前取消并排空，避免进程树逃逸与迟到写入。Unix 子进程测试验证优雅 129/143 退出、活动进程树清理和 stdin-EOF 0；真实 Windows console 验证仍是平台后续项。 |

### 第五批增量流式 JSON 落地（未发布）

共享解析器与 adapter 迁移已实现，不改变 session、RPC、ACP 或插件 wire 格式。

| 上游 commit | Tack 落地 |
| --- | --- |
| `f715a15c1` | `ToolCallArgumentsStream` 每个工具参数 delta 只消费一次，同时保留未修复原文供既有严格 final 解析。owned snapshot 以拷贝预算约束深拷贝工作量，EOF/错误收尾则强制刷新，避免尾部 delta 滞留。Anthropic、Bedrock、Mistral、OpenAI Completions/Responses、tack-messages、CodeBuddy 均接入该路径；Responses 的权威替换在非前缀时重置全部状态，延续时只发缺失后缀；grammar/custom 工具保留非 JSON 原文。测试覆盖完整文档与逐前缀差分、chunk/转义/Unicode 边界、并行调用、替换/重置、畸形 final 拒绝与尾部预览。新增 criterion bench 对比旧的累计前缀重解析与增量解析，既有 fuzz target 同时覆盖任意 UTF-8 分割点和严格 finish。 |

### 第六批上下文投影、MCP 启动与 provider 迁移落地（未发布）

存储兼容、启动延迟和当前 provider 批次已实现，并配有离线 fixture 与 mock
provider 请求。第六批的 context edit 只实现读取/投影；显式写入与 `context` /
`context_with_system` 拆分见第七批。

| 上游 commit / 来源 | Tack 落地 |
| --- | --- |
| `466db0fec` + `d0fe21308` | `context_edit` 在 v3 中是类型化条目，在 v4 中是载荷兼容的 custom 条目。分支内最新的 omit/replace 投影不改原始历史与消息元数据，并兼容导入的 assistant/toolResult 字符串替换。compaction 切点、投影 token 估算、branch summary 及所有 prompt 界面共用该投影。v3→v4 迁移会重映射 target id，并在 `details.tackRetainedTailSourceIds` 中记录重建尾部的源条目 id，因此 compaction 后的编辑仍然有效。测试覆盖 latest-wins、分支相对性、数组型 role、旧 usage 失效、compaction/summary 输入和 checkpoint 迁移。 |
| `e029c3ed0` + `30a1d1849` + `ddaa0a034` + `04b97ef00` | 所有 enabled MCP server 都在后台连接；只有 direct server 会阻塞首个 prompt（最多 10 秒），`tool_search` 则按需等待 deferred server 并把工具合并进实时池，TUI/print/RPC/ACP 共用该路径。重连会重建代理。`--tools`/`--exclude-tools` 支持 `*` 通配，纯 `+name`/`-name` 修饰符适用于 CLI 和分层 `defaultTools`，非 MCP allowlist 会把未命名 MCP 工具保留为仅搜索可达，`--no-mcp` 禁用配置的 server。覆盖动态池激活、direct 有界等待、CLI 过滤、ACP/RPC 冒烟和 agent-loop 激活路径。 |
| `a37306d43` + `a3f265b1c` + `139707002` + `3e4f87265` + `1692bba63` + `fe8a6f5dc` + `86918f9ee` | Azure provider 同时覆盖 Responses 与 Foundry Completions，共享端点规范化、部署名载荷映射及旧会话回放兼容。Kimi Coding 改为 `/coding/v1` 的 OpenAI Completions；四个 Qwen 套餐使用新网关与套餐优先的凭据回退；Gemini 4+/带前缀 id 使用 `thinkingLevel`；Copilot reasoning usage 接受顶层 `reasoning_tokens`。内嵌的 1179 模型目录已更新 Azure/Kimi/Qwen/Copilot 切片。由于本地上游 clone 没有物化生成后的 provider-data 文件，离线无法做全量生成式目录刷新；其他 provider 快照保持不变。 |

### 第七批兼容收尾落地（未发布）

本批按上游精确契约完成 Context Edit、RPC、MCP 与 provider 的定点收尾；仍不是
对 1,684 个增量 commit 的全量审阅，因此不推进正式对齐基线。

| 上游 commit / 来源 | Tack 落地 |
| --- | --- |
| `466db0fec` + `aef5fc429` | `SessionManager::append_context_edit` 提供显式写 API：活动分支与 role/content 校验、omit/replace、assistant/toolResult 字符串规范化，复用 v3/v4/SQLite/内存写路径；失败不改 leaf 或文件。tack-RPC 升级 3.1，新增 `transformContextWithSystem`：普通 handler 先处理非 system 投影，full handler 再权威处理完整 transcript，最终 prompt/tools 不因 fallback 复活。Rust/TS/Python SDK 均已接线。**未接**自动 recovery omission 与 actionable turn-boundary drafts；JSON-RPC 以结构相等近似 JS 引用相等。 |
| `e473b5cd8` + `beffcde92` 的 RPC 部分 | prompt/steer/follow_up 返回 additive `data.disposition`；扩展命令与 UserPromptSubmit block 为 `handled`，`started` 响应先于 `agent_start`。new/switch/fork/clone 先取消并排空旧 run，再各发一次 `sessionShutdown`/`sessionStart`，随后返回成功。顺带修正生命周期事件名为协议既有的 camelCase 并携带 reason。**未覆盖** JSON print 的 entry-event golden trace、`6aab551a7` 与 empty-bind reload（`e03dca7a3`）；Tack 尚无对应的完整扩展 runtime/reload 面。 |
| `1387af7b4` + `91f9f3b5d` + `4d72d05bc` | session loader 与 `tack mcp list` 共用 raw JSON 合并：无 command/url/type 的项目条目只允许 enabled/exposure/toolExposure 浅覆盖，toolExposure 整棵替换，非法项保留 global；完整项目定义带 auth 拒绝，不带 auth 替换不继承秘密。global/extension Streamable HTTP `auth.provider` 在每次 POST/GET/DELETE 重新经 provider auth resolver 取派生 api_key，覆盖静态 Authorization，永不进入 MCP OAuth；HTTPS/loopback、来源限制、provider login/logout 引导均已落地。本地长连接测试验证 tok1→tok2。 |
| `bbb61e34a` + `6671c6047` + `561a2e066` | OpenRouter 默认 affinity、Baseten 目录 opt-in、既有 Fireworks/Cloudflare 标志和 OpenCode/OpenCode Go header 均已接线；cacheRetention=none 禁止通用 affinity 但不禁止 OpenCode。显式请求头大小写不敏感优先，覆盖 Chat Completions、Anthropic、Responses 与 Google 的适用路径。 |
| `16235fd93` | Google/Vertex 按 `thinkingLevelMap` 对请求与 off 做先向上后向下 clamp，保留 native medium、规范化映射大小写、拒绝非法映射；不再硬编码 MINIMAL/HIGH。 |
| `1e39862f6` | llama.cpp 仅对 loaded 模型请求 `/props?model=<id>&autoload=false`，避免 sleeping/unloaded 的唤醒/加载副作用；`enable_thinking` 模板生成精确 reasoning map 与 `qwen-chat-template` compat。 |
| `46bde88a1` | `compaction.modelOverrides` 以精确 provider/modelId 逐字段覆盖 reserveTokens/keepRecentTokens，接入手动、阈值、overflow 与 /context，保留后续 `6fcb48670` 的模型输出上限语义。**差异**：非法值仍沿用 Tack 既有 settings 的容错回退，上游逐字段报错校验未单独实现。 |

### 第八批 session-entry 事件流落地（未发布）

本批补上游生命周期回归所需的宿主/API 前置，并用真实 demo 扩展验证客户端可见
顺序；仍没有实现完整的扩展命令 runtime/reload 面。

| 上游 commit / 来源 | Tack 落地 |
| --- | --- |
| `6aab551a7`（startup 部分）+ `beffcde92` | tack-RPC 升级 3.2，Rust/TypeScript/Python SDK 新增 `session/appendEntry`。TUI、print 与 RPC 持久化 custom entry；JSON print 在启动 handler 前输出 session header，随后在 `agent_start` 前输出 `entry_appended`。RPC startup/new/fork/clone/switch 轨迹证明 handler 追加的每个 entry 恰好到达一次，且先于命令成功响应。session 生命周期完成请求会等待 handler 副作用（v3.1 保留通知回退），直接 await 单行写入消除响应/事件队列竞态，生命周期回调期间不持有 session 状态锁。 |

**未覆盖：**由扩展 `/renew`/`ctx.newSession()` 命令驱动的 print replacement，
以及 empty-bind reload（`e03dca7a3`）。它们需要完整的扩展命令 context 与
reload/资源重建面；不重建扩展而重复发一次 `sessionStart` 并不等价。

### 第九批 recovery 与小型兼容落地（未发布）

本批把 session 持久化移到 `message_end` 之前，overflow recovery 接入持久化
source id，并关闭独立的 settings/补全差距；不声称完成完整 actionable boundary
项目。

| 上游 commit / 来源 | Tack 落地 |
| --- | --- |
| `466db0fec` + `02726086d` | `SessionHooks`/RPC/ACP/remote 在发布 `message_end` 前持久化消息并返回 source entry id。`tack_session::omit_recovery_attempt` 按 source id 定位 assistant（含 replacement 与 retained-tail provenance），先验证所有目标，再在 compaction 前 omit 匹配的 tool results，raw history 保持不变。agent-loop 测试固定 finalize → omit → compact 顺序；length 截断的 tool call 改为生成失败 result 并自然续行，不写 omission。覆盖 v3/v4 reopen、replacement、重复消息、无关 result 与缺少 provenance。**未覆盖：**provider 内部 transient retry（当前不会持久化这些 attempt）、上游 pre-prompt recovery driver，以及 actionable turn/agent-settle boundary drafts。 |
| `46bde88a1` | 普通与精确模型的 compaction token 设置改为惰性逐字段校验：仅接受非负 safe integer，先校验普通字段，拒绝非法当前 override entry，其他模型/字段不受影响。错误贯穿 TUI、print、RPC、ACP 与 remote，不再静默回退。 |
| `d7951ec36` + `36af9dc48` | slash 自动补全移植上游 fuzzy scorer 与最终两段式 skill 行为：裸名命中在前，只有全名命中的 skill 在后，同时保留 `/skill` 列表与 `/skbra` 简写。覆盖 research-idea 对 deep-research、普通命令/template、显式 skill 查询及稳定前缀排序。 |

## 2026-09-21 对齐批次（基线 4a6ed0194 → 10d1ad621）

160 个上游 commit 经逐条审阅（报告由 `scripts/upstream-delta.sh` 生成）。

### 已落地

| 上游 commit | 内容 | Tack 落地 |
| --- | --- | --- |
| `890f92088` + `af7359b90` | 未知 provider 默认非 strict 工具；Cerebras 排除 | `0741270`：`compat.rs` 默认改 false；catalog.json 为 593 个 openai-completions 模型补齐显式 `supportsStrictMode`（508 个缺失按上游白名单规则写入）；附单测+目录集成测试 |
| `e5d18382a` + `e98f287ee` + `c37b0e03b` | 重试 Cloudflare 520 / Azure 峰值错误；backoff 60s 上限 | `ba52cba`：RETRYABLE_PATTERNS 两条 + `retry_delay_ms`（`RetryPolicy.max_agent_delay_ms`，`settings.retry` 新增 `maxAgentDelayMs`） |
| `4bd3f48df` | GLM-5.2 在 Mistral 走 `reasoning_effort` | `c33d2c0`：`uses_reasoning_effort` 加 `zai-glm-5-2` 分支 |
| `1283afd0d` | Anthropic 重命名模型的 thinking 回放 | `8cfd6ea`：message_start 不再覆盖 `output.model`，异名记 `response_model`；fallback 定价未移植（Tack 无 server-side fallback） |
| `8a7b0c03d` | Bedrock 1h 缓存写计费 | `ae0d0fe`：metadata `cacheDetails` 按 `ttl=="1h"` 累加进 `cache_write_1h`（计价侧本已就绪） |
| `0c7bb7c5c` | Responses 错误带 provider 名 | `bfef4a8` |
| `8bdcd4498` | 尾部超大 tool result 的 compaction 切点 | `c198896`：`find_cut_point` 找不到时回退最后切点 |
| `acaa253cc` | 扩展工具参数 schema 校验 | `e8d38a5`：`ToolSpec::validate_parameters()` + 握手后过滤畸形工具 |
| `b6419322e` | 坏 frontmatter 模板不再静默丢弃 | `875a3b4`：`tracing::warn!`（未搬 ResourceDiagnostic 体系） |
| `59eb4c393` | copy 快捷键描述 | `d588e4b`：中英文案 |
| `9b791a4cc` | 精确 session id 免全量扫描 | `8189909`：`--session-id` 改用 header-only `find_session_by_id` |
| `3349e1db1` + `60e7e76bd` + `6dff740fa` | 剪贴板三件套 | `127f25e` + `6c67877`：新 `tack-tui/src/clipboard.rs`（原生后端 pbcopy/wl-copy/xclip/xsel/termux/PowerShell 分派、失败描述性错误、OSC 52 仅远程/无显示兕底、WSL 优先 Windows 剪贴板）；tack-app 各调用点 notice 失败 |
| `661619e87` + `0e283203c`（依附于 overflow.ts 整体） | overflow 检测 + compact-and-retry | `ac119b5` + `c7e1af5`：新 `tack-ai/src/overflow.rs`（错误模式/静默超窗/length-stop 三检测 + `is_recoverable_length`）；agent-loop 溢出恢复——新 `AgentHooks::compact_for_overflow`，`SessionHooks`/`RpcCompactionHooks`/`HostCompactionHooks` 三处实现（各提取 `run_compaction` 核心），预算镜像 `_overflowRecoveryAttempted`（新用户消息/成功响应重置） |
| `0e283203c` 遗留 | length-stop 恢复接线 | `a53d3c4`：agent-loop Length 分支——`is_recoverable_length`（输出低于请求期望上限判为 context 压力/服务端截断；期望上限取调用方 `max_tokens` override 优先于模型默认，用户手配小上限属真实触顶不压缩）命中则走 compact-and-retry（共用 overflow 恢复预算），二次 length 或真顶到上限则走既有 fail-truncated-tool-calls 路径；附四测试 |
| `bfa686240` | CJK 标点文件补全 | `a03b825`：CJK 标点（U+3001–303F + 上游显式全角列表）作为 token 分隔符，`compute()`、`refresh_autocomplete` 性能门控、`ext_trigger_token`（插件触发器）三处同步同一判定；CJK 文字保持词字符 |

### 已核对，无需改动

| 上游 commit | 结论 |
| --- | --- |
| `a8b3dd199` signal 杀 shell | `tack-tools/bash.rs` 已有同等语义与测试 |
| `de2de549b` compaction 取消竞态 | 架构已覆盖（lineage 校验 + 双重 stale 守卫 + tokio 取消语义） |
| `dd01f5b24` recent session 发现 | 已纯 stat 排序，比上游修后更轻 |
| `e86102f18` Codex Off effort | 现行行为已与上游修后一致 |
| `9e05370b2` 中途 system message | 折叠回放等同上游默认行为；provider 原生透传（缓存优化）为独立增强 |
| `47a18e37b` GIF 魔数 | Tack 全部按扩展名识别图片，结构性免疫 |
| `590144609` fuzzy 延迟 | 无逐字符正则打分，结构性免疫 |
| `faa9863cb` 队列消息 input 钩 | tack-ext 无 input 拦截事件，结构性免疫 |
| `509ee2bd0` user bash hook fail-closed | 无扩展路由攻击面 |
| `46c9de402` 事件反注册 | 静态订阅模型，无快照逼历问题 |
| `465853498` + `13784598d` thinking drop notices | Tack 无此 notice 功能面 |
| `fe219d7f8` bash 耗时格式 | Tack 不显示耗时 |
| `fde6d778f` summary 点击折叠 | Tack summary 永远全文渲染，无对应交互面 |
| `b03a367a4` Anthropic fallback 配置 | Tack 无 server-side fallback，无可配置对象 |
| `e4c75a732` + `16292398a` forced system prompt | 上游净 diff 为零（revert）；Tack 无此前身设计 |
| `21b8cc1a4` stale 图片转换 | 渲染时同步转换，无异步缓存竞态 |
| `b7f788194` remote prompt 事件 | Tack remote client 无 one-shot prompt-then-exit 路径 |
| `60740991c` Node 模块缓存、`40c256ccc` jiti 懒加载 | TS/Node 特有 |
| bug reporting 一组（`3c75b2747`/`d1230ea20`/`d875512cc`/`c7cdb460a`/`1e0fe2049`/`63787ee6b`） | 功能整体不存在，暂缓；引入时需重开评估 |
| `7e1950768` experimental micro agent | 非主线对齐项 |
| `bdee230f1` 图像生成模型目录 | Tack 无图像生成功能 |
| `fa0e1f48a` LaTeX | Tack 无 LaTeX 渲染 |
| `d92eb8d4b` Fireworks deferred tool loading | Tack fireworks 走 openai-completions；运行时已支持 tool_references，目录随下次刷新 |
| `3390bd936` cache warming 迟到守卫 | 依附于 warming 特性（未移植），移植时需一并带上 |

### 跟踪中 / 待办

- **Pico/Pico5 设计文档流（37 commits）+ chord（operation log）+ durable + `10d1ad621`**：观察中，沿用 v4 fork-policy 口径「借鉴设计、不盲目跟随」。若 Tack 立项持久化 agent 运行时，回读最终收敛的 Pico5 文档（pico→pico3 中间稿已废）；chord 的 operation log 方向优于 delta 快照。
- **模型目录常规刷新**：本批次已补 508 个显式 `supportsStrictMode`；其余待下次 npm 全量刷新（deepseek/openai-codex/fireworks/google/vercel/kimi-coding、新增 meta/radius、anthropic `promptCache`/`inputLimits`、google thinking levels from models.dev）。
- **待移植（按优先级）**：
  1. session affinity headers（`bbb61e34a` OpenRouter 默认开 + `6671c6047` Baseten + `561a2e066` OpenCode 头）——compat 字段 + 头部发射，缓存命中收益。**已落地（第七批）**。**kimi-coding 已评估排除（2026-09-22）**：Moonshot 缓存是 org 级共享前缀缓存（官方文档明确「同一组织内共享」），不认任何亲和头，服务端无 per-replica 路由问题；其真正缺口是顶层 `cache_control`，见「顺带修复」`5954421`
  2. `16235fd93` Gemini 关 thinking 改走 thinkingLevelMap clamp（已落地第七批）
  3. ~~`f5c946480` image input limits~~ **暂缓（2026-09-22 评估）**：上游该特性本身「数据先行、执行半置」——真正执行的只有 `images.resize` profile 覆盖，且目前上游目录所有模型都是默认 profile（2000x2000/4.5MB/jpeg80），与 Tack 现有固定行为等价；`maxPerMessage`/`maxPerRequest`/`maxRequestBytes` 上游也没有执行层。完整移植需先给 `AgentTool::execute` 加执行上下文（对齐上游 `ExtensionContext` 的 model/cwd），跨 5 crate + 插件协议的 ABI 演进，单独立项。Tack 的 Model 反序列化不拒未知字段，未来 npm 刷新带进 `inputLimits` 数据安全。依赖项落地后再回评
  4. ~~length-stop 恢复接线~~ → 已落地 `a53d3c4`（见「已落地」表）
  5. `1e39862f6` llama.cpp `/props` thinking 探测（已落地第七批）
  6. `46bde88a1` 每模型 compaction 预算（settings `modelOverrides`，已落地第七批；非法值严格报错校验仍有差异）
  7. `dfbf793b7` 会话选择器后台渐进加载
  8. `b73412a37` Meta provider + Muse OAuth（独立特性）
  9. `4d38031fb` radius 基线目录（小）
  10. `c596d09d9` + `3390bd936` prompt cache warming（中-大，成本优化）
  11. TUI：~~`bfa686240` CJK 标点文件补全~~（已落地 `a03b825`）、`d7951ec36` skill 补全裸名打分、`803f0e906` WezTerm 全屏图（先实测确认命中）
- **独立基建待办**：`AgentTool::execute` 执行上下文（对齐上游 `ExtensionContext`：model/cwd 等）——image resize profile、read 的 ctx.cwd 等多个上游特性的共同前提，跨 tack-agent-core/tack-tools/tack-ext/tack-ext-wasm/tack-app，需插件协议一并演进
- **evals 借鉴**：`42cd371ba` TUI footer eval 思路（验证文档实际注入模型上下文）、`f3564a1d4` 文档导航可达性静态检查——可低成本移植进 evals/docs-audit。

### 顺带修复（非上游项）

- `5954421`：**kimi-coding prompt caching 修复（领先上游项）**。Moonshot Messages
  API 只在请求**顶层**认 `cache_control`（消息体内标记被忽略；缺省只读 5m
  缓存、不写入），Tack 与上游 TS 此前都只在消息体内打点 → kimi 缓存写入
  完全失效、cacheRead 折扣吃不到。修复：`AnthropicCompat.top_level_cache_control`
  开启时顶层发 `cache_control`（Short→5m / Long→`ttl:1h`，复用 CacheRetention
  映射）、消息体内不再打点；catalog kimi-coding 4 个模型已标记。验证方式：
  连跑两轮会话看响应 `usage.cache_read_input_tokens` 转为非零。**后续
  （2026-10，Moonshot 计费拆分）**：Moonshot 上下文缓存升级把 Cache Write
  单独计费（5m 写入价 = 输入价，1h = 2 倍，命中 = 1/10）。第一方 k3 模型
  （`kimi-coding`/`moonshotai`/`moonshotai-cn`）catalog 补 `cacheWrite` =
  输入价（原为 0）；顶层 Short 标记显式钉 `"ttl":"5m"`——Moonshot
  Messages 契约只文档化了带 ttl 的标记。
- `4635473`：extension_tests 三个 demo-plugin 测试共享 `TACK_AGENT_DIR` 环境变量、并行下互踩（预先存在的 flake，基线可复现），加 ENV_LOCK 串行化。

## 2026-09-08 对齐批次（基线 → 4a6ed0194）

### 已落地

| 上游 commit | 内容 | Tack 落地 |
| --- | --- | --- |
| `96617628e` | mistral-medium-* 全系用 `reasoning_effort`（前缀匹配） | `tack-ai/src/api/mistral_conversations.rs` `uses_reasoning_effort` 改为 `starts_with("mistral-medium-")`，附前缀匹配单测 |
| `fcff255b0` | 内置工具默认 `{ type: "json_schema", strict: "prefer" }` | `tack-agent-core::AgentTool` 新增 `constrained_sampling()` 默认方法；read/bash/edit/write 返回 prefer；`tack-tools` 附测试 |
| `7d8ab31a4` | Copilot `gpt-*` 前缀一律走 Responses API | `tack-ai/catalog.json` 的 github-copilot 从 npm 0.85.1 数据刷新（10 → 28 个模型），并叠加新路由规则（`gpt-6-astra` 从 completions 改路由到 responses；规则：`gpt-`/`grok-`/`oswe`/`mai-` → `openai-responses`） |
| `e687434a6` + `47acd8e6c` | 树导航并发守卫：streaming/compaction 期间拒绝；对话框关闭后复查 | RPC：`RpcState.is_compacting` 新字段，`fork` 命令检查 `is_streaming`/`is_compacting`；compaction 全程置位（含错误路径）；TUI：`/fork`、`/tree`、`/rewind` 打开时检查 `running`，fork 确认时复查；附 RPC 测试 |
| `ab9e6f89b` | Alt+滚轮 ×5 加速（SGR button bit 3 = Alt） | `tack-tui/terminal.rs` 补上被丢弃的 crossterm 鼠标 modifiers 映射；`scroll_view.rs` Alt 时滚动 3→15 行 |
| `caf6dfe73` | 剪贴板操作移出 UI 线程 | `tack-app/tui/input.rs` Ctrl+V 图片/文本剪贴板读取包进 `spawn_blocking`（arboard + CLI 两条路径） |

### 已核对，无需改动

| 上游 commit | 内容 | 结论 |
| --- | --- | --- |
| `b2602be77` | EventStream 队列 `shift()` O(n²) 修复 | Rust 侧 tokio channel 无此问题；现有 `remove(0)` 调用点均为有界小队列 |
| `9841914c7` | 鼠标 hover 不改变列表选中项 | Tack `terminal.rs` 直接丢弃 `MouseEventKind::Moved`，hover 根本到不了组件层，结构性免疫 |
| v4 fork-policy（`6dfc66d32`、`85186f823`、`41218b394` 等） | 保留命名空间投影、lane state 重置 | 属于 agent harness 存储 v4（lanes/named branches），Tack 会话格式仍是 v3；v3 `forkFrom` 语义（拷贝全非 header 条目 + parentSession 指针）经逐项核对与 `fork_from_in`/`sqlite_backend::fork_session` 一致 |

### 借鉴落地的模式

- **后端一致性测试套件**（`packages/agent/.../testing/conformance/session-repo.ts`
  的模式）：新增 `tack-session/tests/backend_conformance.rs`，同一场景跑
  JSONL 与 SQLite 两个后端，比较结构指纹（id 剔除、parentId 归一为位置
  索引、嵌套 timestamp 递归擦除）。覆盖 append/重开、branch+path、fork
  隔离性、非法 id 拒绝，4 用例。
  TS 侧的 WP08 批次还计划把 SQLite 后端纳入同一契约——Tack 先行覆盖了。

### 跟踪中 / 待办

- [x] **存储 v4 迁移**（2026-09 落地，含写路径闭环）：`tack_session::v4` +
  `v4_bridge` 模块、`fork_policy`（详见 `crates/tack-session/V4_NOTES.md`）——
  事务日志格式（header/write 类型字段级对齐上游 codec/types）、两遍流式
  legacy v3→v4 迁移（原文件留 .bak、加密保持加密）、fork-policy 命名空间
  投影规则逐项移植（`tack.lane.state` 重置、丢弃 `tack.result`/`tack.op.*`/
  `tack.pending.*`、未知 `tack.*` 保留命名空间报错）。**`SessionBackend::JsonlV4`
  为默认后端**：新会话原生写 v4，打开 v3/v2/v1 文件透明迁移，`sessionBackend:
  "v3"` 为逃生门。写路径映射：每条 entry 一个事务（entry + tip + 镜像值 +
  usage ledger 行），v3 变更条目（model_change/thinking/session_info/label）
  存为 custom 条目并镜像当前值。有意取舍：迁移条目 id 用 8 字符 uuid-v4
  （非 uuidv7）；打开时立即迁移（上游延迟到首次 commit）；未知/未来条目类型
  保留为 custom 条目而非上游的报错（Tack 不丢数据原则）；无 id 坏行跳过。
  **遗留**：WP07（SQLite v4 后端、repo 级会话列表）与 operation/pending
  运行时未实现；fork 从内存当前状态出发（未做 WP08 两遍流式扫描）。
- [ ] **模型目录常规刷新**：`catalog.json` 是手工转换快照（本次 Copilot
  刷新后 1130+ 模型）。建议每次对齐时检查 npm `@earendil-works/pi-ai`
  新版并全量刷新，而不是只补单个 provider。
- [x] **文档 evals**（`9211da172`）（2026-09 落地）：`evals/docs-audit/`——
  `static_check.sh` 无模型交叉校验（settings 键/hook 事件/CLI flags/
  features.* 键 vs 代码）+ 3 个 agent 驱动深度审计任务（确定性锚点生成
  期望值，文档或代码漂移时任务自动翻转预期）。

## 历史基线

| 日期 | 上游 commit | 备注 |
| --- | --- | --- |
| 2026-09-08 | `4a6ed0194` | mistral/strict sampling/Copilot 目录/导航守卫/TUI 细节 + 一致性测试套件 |
| 2026-09-21 | `10d1ad621` | strict 默认值/retry/overflow 恢复/剪贴板/小修复批；Pico5 观察中 |
