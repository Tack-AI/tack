# TS pi → Tack 上游对齐追踪

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
  1. session affinity headers（`bbb61e34a` OpenRouter 默认开 + `6671c6047` Baseten + `561a2e066` OpenCode 头）——compat 字段 + 头部发射，缓存命中收益。**kimi-coding 已评估排除（2026-09-22）**：Moonshot 缓存是 org 级共享前缀缓存（官方文档明确「同一组织内共享」），不认任何亲和头，服务端无 per-replica 路由问题；其真正缺口是顶层 `cache_control`，见「顺带修复」`5954421`
  2. `16235fd93` Gemini 关 thinking 改走 thinkingLevelMap clamp
  3. ~~`f5c946480` image input limits~~ **暂缓（2026-09-22 评估）**：上游该特性本身「数据先行、执行半置」——真正执行的只有 `images.resize` profile 覆盖，且目前上游目录所有模型都是默认 profile（2000x2000/4.5MB/jpeg80），与 Tack 现有固定行为等价；`maxPerMessage`/`maxPerRequest`/`maxRequestBytes` 上游也没有执行层。完整移植需先给 `AgentTool::execute` 加执行上下文（对齐上游 `ExtensionContext` 的 model/cwd），跨 5 crate + 插件协议的 ABI 演进，单独立项。Tack 的 Model 反序列化不拒未知字段，未来 npm 刷新带进 `inputLimits` 数据安全。依赖项落地后再回评
  4. ~~length-stop 恢复接线~~ → 已落地 `a53d3c4`（见「已落地」表）
  5. `1e39862f6` llama.cpp `/props` thinking 探测
  6. `46bde88a1` 每模型 compaction 预算（settings `modelOverrides`）
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
  连跑两轮会话看响应 `usage.cache_read_input_tokens` 转为非零。
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
