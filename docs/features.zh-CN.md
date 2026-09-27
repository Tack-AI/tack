# Tack 功能指南

**[English](features.md) | 简体中文**

超出 TS pi 的功能逐项说明：是什么、怎么用、怎么关（全部可用 `features.*`
或对应设置关闭，详见 [configuration.md](configuration.zh-CN.md)）。

## Agent 执行

### 模型目录在线刷新

内嵌目录是构建时快照，新模型要等 Tack 发版。`/models refresh` 从 npm 上发布的
`@earendil-works/pi-ai` 包拉取最新目录（tarball 内的 `dist/providers/data/*.json`，
即内嵌目录的同源数据），转换为 Tack 格式后立即生效并缓存到
`~/.tack/agent/catalog.json`（每次启动加载，含 headless 模式）：

```
/models            # 查看当前目录来源（内嵌/已刷新）
/models refresh    # 立即刷新（受 --offline / TACK_OFFLINE 限制）
/models reset      # 删除缓存，回到内嵌目录（重启后生效）
```

设置 `"modelCatalogRefresh": true` 后每次 TUI 启动后台自动刷新（不阻塞、失败不致命）；
registry 可用 `TACK_MODEL_CATALOG_REGISTRY` 覆盖。带截断防护（<20 provider 或
<50 model 的结果拒绝安装），未知 provider 的数据文件跳过。

### Provider/模型发现（首次使用引导）

新用户不知道有哪些 provider、每个 provider 有什么模型、怎么配 key：

- `tack providers`：全部 provider 一览——认证状态（✓/未配置）、模型数、
  启用方式（env var 名或 `tack login --provider <id>`）；含 models.json 自定义
  provider；结尾汇总多少个 provider 就绪
- `tack models [pattern]`（= `--list-models`）：按 provider 分组的模型目录，
  provider 头带认证标记；子串过滤 id/名称
- TUI 内 `/providers`：同样的 provider 一览；`/model` 浏览选择模型
- TUI 启动时若没有任何凭据（含 models.json/env/云端 ambient 链），直接在
  对话里给出四步引导（providers → login → env var → /model），而不是等第一次
  提交才报 "no API key"
- print 模式缺 key 的报错同样附上 `tack providers` / `login` 提示

### CodeBuddy 原生 provider（`codebuddy/*`）

把 `codebuddy` CLI 作为一等 provider 接入（tack-ai `codebuddy-stream` 适配器，
无需 Node SDK、无需本地 HTTP 翻译层）。**使用指南见
[codebuddy.md](codebuddy.zh-CN.md)**（安装、登录、思考级别、会话隔离、故障排查）。
实现要点：

- **stream-json 长驻会话**：以 `codebuddy -p --input-format stream-json
  --output-format stream-json` 启动长驻进程（`CODEBUDDY_PATH` 或 PATH 探测），
  经 `control_request initialize` 握手；模型目录从握手响应发现并在启动时注入
  （失败回退单个 `default` 模型），CLI 侧 auth（`codebuddy login`）直接复用
- **工具桥（SDK MCP / parked tools/call）**：Tack 的工具经 **SDK MCP server**
  暴露给模型——`initialize` 握手声明 `sdkMcpServers: ["tack"]` 后，CLI 以
  `mcp_message` control 帧跑 MCP JSON-RPC（initialize/tools/list/tools/call），
  Tack 用 `control_response`（`response.response.mcp_response`）应答。SDK MCP
  工具是 CLI 的 native 工具（不像 HTTP MCP 那样被 defer），因此 `--tools ""`
  禁用内建工具的同时模型仍能看到 `mcp__tack__*` 的 schema；模型的 tool_use 结束本次
  LLM 响应并把 CLI 侧的 tools/call **挂起**，Tack 的 agent loop 用自己的权限/UI
  执行工具，下一次 stream 调用解析挂起的 MCP 请求、CLI 会话原生续跑——CLI 侧
  缓存不丢，且没有前缀哈希续接之类的脆弱机制。注意 CLI 会把**并行** tool_use
  块复用同一个 content index 且只发一个 `content_block_stop`（Tack 按 index
  停掉所有活跃块）；工具轮结束后 CLI 按块重放完整 assistant 消息（stale echo，
  下一轮首个 stream_event 前整行跳过）
- **会话同步**：provider 按语义指纹（去时间戳）比对已同步消息；压缩/改史导致
  分叉时重开 CLI 会话并以扁平转写重放（功能正确，失去 CLI 侧缓存）
- 图片块原生映射（base64）；usage 取 result 事件（订阅制，成本为 0）。
  启动参数/环境对齐 TS 参考插件（pi-codebuddy-sdk → @tencent-ai/agent-sdk）：
  `--tools ""` 禁用 CLI 内建工具、`--strict-mcp-config` 忽略用户 MCP 配置、
  `--permission-mode bypassPermissions`（工具门禁由 Tack 负责）、
  `--setting-sources none` 隔离用户/项目设置、`--system-prompt` 替换
  CodeBuddy 默认身份、`--allowedTools mcp__tack` 只允许桥接工具；
  `--include-partial-messages` 打开 `stream_event` 增量流式（旧 CLI 自动回退
  整段 assistant 消息）；pi 思考级别映射为 `--effort`
  （minimal/low→low、medium→medium、high→high、xhigh/max→xhigh，
  模型的 thinkingLevelMap 优先）；模型元数据按 id 估计（gemini 1M ctx、
  claude/gpt 200K、默认 128K/8K）；工具参数别名容错
  （`file_path`→`path`、`old_string`→`oldText` 等，bash 默认 120s 超时）；
  环境变量禁后台任务/自动更新/自动记忆/自动压缩
  （`CODEBUDDY_CODE_DISABLE_BACKGROUND_TASKS`、`DISABLE_AUTOUPDATER`、
  `CODEBUDDY_DISABLE_AUTO_MEMORY`、`DISABLE_AUTO_COMPACT`）；abort 时优雅解决
  挂起的 MCP 调用并标记下轮重建会话

前置条件：`codebuddy` CLI 已安装并登录。Windows 同样支持：PATH 探测用
`where`（npm 全局安装的 `codebuddy.cmd` shim 经 `cmd /c` 启动，会话回收时
`taskkill /T /F` 整树清理避免 node 孤儿进程）；`CODEBUDDY_PATH` 也可直接指向
`.cmd`/`.bat`/`.py` 脚本。取代旧的 OpenAI shim 桥插件方案（无需 Node、
无需本地 HTTP 翻译层）。

### 后台任务（background tasks）

`bash` 工具加 `run_in_background: true` 立即返回任务 id，不阻塞：

- `bash_output`：读输出（omit task_id 列出所有任务）；
- `bash_wait`：阻塞到任务完成（或 timeout，默认 600s），完成瞬间即返回——下一步依赖任务结果时用它，不要在前台 `sleep`（sleep 期间无法被完成通知打断）；
- `kill_shell`：杀整个进程树；
- 完成时 TUI 出通知并自动 steering 给 agent（"任务 bg3 完成了"），agent 可以接着读结果继续干活；
- agent 空闲时任务完成：自动唤醒开新一轮（与 cron 触发同一模式），不需要人催。关闭：`"backgroundAutoWake": false`。

典型用法：dev server、`cargo build`、长测试。后台任务跨 run 存活（TUI/RPC 会话级），进程退出即回收。

关闭：`"features": {"backgroundTasks": false}` —— bash 的 schema 里 `run_in_background` 参数整个消失，模型不知道它的存在。

### 后台子代理（fire-and-forget subagent）

`subagent` 工具加 `run_in_background: true`：派一个完整子代理去后台跑长任务，进同一个任务管理器（`bash_output` 可看实时进度——逐条记录工具调用，`bash_wait` 可阻塞等它完成），完成通知走同一通道。

```json
{"task": "重构 src/auth 模块并跑测试", "description": "auth refactor", "run_in_background": true}
```

### 模型 fallback 链

```json
"fallbackModels": ["anthropic/k3", "openai/gpt-5.2"]
```

主模型在 provider 级重试耗尽后仍遇 429/过载/5xx/超时 → 自动切下一个模型重试当前 turn，TUI 通知并更新当前模型显示。401 等认证错误**不**降级（换模型没用）。子代理有自己的显式模型选择，不继承链。

### 预算熔断

```json
"tokenBudget": 500000,
"tokenBudgetAction": "pause",          // warn(默认) | pause | downgrade
"budgetDowngradeModel": "openai/gpt-5-mini"  // downgrade 目标，默认 fallbackModels 末位
```

`pause` 在 turn 边界停掉 run（预算警告里说明如何继续）；`downgrade` 只触发一次并切到预算模型。`/cost` 随时查看分模型 token/成本明细和预算进度。

## 代码感知

### LSP 集成

`lsp` 工具的操作：

```
diagnostics        （默认）文件的编译/类型错误；省略 path = 所有有问题的文件。
                   后台类型检查（rust-analyzer flycheck）未完成时会明确标注
                   “analysis still running”，不会把语法级干净误报为类型检查通过
definition         path + line + column（或 name）→ 定义位置
references         path + line + column（或 name）→ 所有引用
implementation     path + line + column（或 name）→ trait/接口实现
symbols            path → 文件符号大纲（类/函数树）
workspace_symbols  path + query → 全项目符号搜索（query 为子串，空 = 全部）
hover              path + line + column（或 name）→ 类型/签名信息（markdown，截断到 2000 字符）
incoming_calls     path + line + column（或 name）→ 谁调用了这个符号（call hierarchy）
outgoing_calls     path + line + column（或 name）→ 这个符号调用了谁
code_actions       path [+ line + column] → 列出 quickfix；apply=<index> + line + column → 应用（落盘，先过 checkpoint）
rename             path + line + column + new_name → 全项目重命名（直接落盘，先过 checkpoint）
```

支持 `name=<符号>` 代替 line/column：经 workspace/symbol 精确匹配定位，多个候选时会列出位置让你用 path+line+column 消歧；省略 path 时查询所有已启动的 server。

code_actions 的上下文诊断自动限定在光标位置的范围内；服务端返回的 Command 类动作标记为不可应用（只应用携带 WorkspaceEdit 的动作）。

多根会话：`--add-dir` 的额外目录（存在且不重复时）作为 workspaceFolders 一并发给语言 server，跨目录依赖可解析。每 server 最多同时打开 64 个文档，超限后最久未同步的文档自动 didClose（缓存诊断一并丢弃）。

edit/write 的结果自动附该文件最新诊断（`lspEditFeedback: false` 可关），诊断行带来源与代码（如 `[rustc E0308]`），并汇总其他文件的错误计数（如 “3 error(s) in 2 other file(s): tui/mod.rs (2), chat.rs (1)”）——改 enum/签名导致的跨文件破坏不用等 build 就能看见。server 按需惰性启动并保持热连接；首次查询要等索引。语言 server 按扩展名自动识别，`lspServers` 可覆盖。

已知的 rust-analyzer 行为（已内建应对）：先发空诊断再发真诊断（客户端有 settle 循环，按文件维度等待）；首次分析完成前 definition 返回空（warmup 门）；分析进行中 rename 返回 ContentModified（自动重试）；类型诊断来自后台 flycheck（通过 serverStatus/progress 跟踪，诊断反馈据此区分“语法级干净”与“类型检查完成”）。

server 进程崩溃后透明重启（每扩展名最多 3 次，超出后标记失败避免崩溃循环）；单个请求硬超时 60s，杜绝挂死 tool call。

关闭：`"features": {"lsp": false}`。

### 文件 checkpoint 回滚

每个用户 turn 开始时：

1. git 仓库内：快照所有 dirty/untracked 文件（git 基线）；
2. turn 内 edit/write 工具写文件前快照原内容。

`/checkpoints` 列出各 turn 改了哪些文件；`/checkpoints restore <turn>` 回滚：

- edit/write 改的文件 → 从 blob 精确恢复；
- bash 改的已跟踪文件（`sed -i`、`cargo fmt`）→ `git checkout` 恢复；
- bash 新建的文件 → 删除；
- **turn 开始前就是 dirty 的文件 → 恢复到当时的 dirty 内容**（不是 HEAD）。

与 `/rewind` 互补：rewind 只回退对话树，checkpoints 恢复文件。

关闭：`"features": {"checkpoints": false}`。

### 自定义子代理

`.pi/agents/*.md`（项目级，需信任）或 `~/.tack/agent/agents/*.md`（全局）：

```markdown
---
name: reviewer
description: 代码审查——实现完功能后调用
tools: read, grep, find, ls, lsp
model: anthropic/k3
---

你是一个严谨的代码审查者。……（正文成为子代理的系统提示）
```

然后 `subagent` 工具用 `agent: "reviewer"` 调用：独立系统提示 + 工具白名单 + 模型覆盖。同名项目级覆盖全局。

### Worktree 隔离

`subagent` 加 `isolation: "worktree"`：自动 `git worktree add` 临时分支，子代理在其中改文件，结果附分支名 + 路径 + diff stat。多个子代理并行改同一代码库互不踩踏；父代理自行决定 merge 还是丢弃。

### 并发上限与共享预算

```jsonc
// settings.json
{ "subagents": { "maxConcurrent": 4, "budgetTokens": 200000 } }
```

- `maxConcurrent`：同时运行的子代理循环数上限（信号量），并行批次 + 后台子代理共用；超出的排队等待（可取消）。
- `budgetTokens`：本次会话所有子代理（含 fire-and-forget 后台）累计 token 用量上限，超限后新的 `subagent` 调用直接报错。子代理不写会话文件，全局 `tokenBudget` 钩子看不到它们的用量，所以需要这份独立账本；工具结果带 `usageTokens`/`budgetTokensUsed` 让父代理感知委派成本。

两个键都是 `0` = 不限（默认）。

### 受控 git 工具

内置 `git` 工具：agent 用 `command: "status"` / `"add -A"` / `"commit -m msg"` 结构化调用 git，工具自己解析子命令，权限系统按子命令分类而不是匹配自由文本（bash 通配符易被 `git -C`、alias、引号绕过）：

- 纯读子命令（status/log/diff/show/blame/reflog/rev-list/ls-tree/ls-remote/branch 列表形式/`config --get` 等显式读形式/…）在 plan 模式、ask 模式下免提示，`Git` 规则免提示；其余子命令走正常权限询问。
- 引号感知解析：shell 元字符只在**引号外**被拒绝，所以 `commit -m "fix: a > b \`x\` $(y)"` 这类提交信息可以正常写；解析后的每个参数在执行前重新加单引号转义，引号内容对 shell 完全惰性。
- `commands: ["status", "log --oneline -10"]` 批量形式：多条调用一次执行（按顺序，遇错即止）。只读分类要求**每条**都只读；权限规则对批量按条目匹配——deny 命中任意一条即拒，allow 要求全部命中。
- 校验拒绝未加引号的 shell 元字符、全局选项（`-C`/`--git-dir`）和不在白名单上的子命令——同时阻断了 config alias（`alias.x = !shell`）注入路径。执行走 bash 执行器，沙箱策略与 ACP client terminal 对齐。
- 权限规则写法与 bash 一致：`permissions.allow: ["Git(push *)"]`、`permissions.deny: ["Git(reset --hard*)"]`（通配符匹配，deny 优先）。

bash 仍然可用（临时目录、非 git 任务），但系统提示会引导优先用 git 工具。

### 上下文占用可视化

`/context`：展示当前上下文窗口里各段占多少 token——系统提示、工具 schema、用户消息、助手消息、thinking、tool results（含最大的几个工具按体积排序），并用 provider 报告的真实值校准估算，给出窗口占比与自动压缩触发余量。压缩变贵变慢之前就能看出是哪段在膨胀。`/todo` 查看会话任务列表（`/todo clear` 清空），与 TUI 面板同一状态。

## 记忆与会话

### 持久记忆

`memory` 工具（save/delete/list）以 **Claude Code 式 auto memory** 写两个作用域：

- **project**（默认）：本仓库的约定/决策/环境怪癖，存 `<memory根>/projects/<编码仓库路径>/`。
  以主工作树根为 key，**同一仓库的所有 worktree 共享**；非 git 目录回退 cwd。
- **user**：跨项目的用户偏好/工作方式，存 `<memory根>/`（默认 `~/.tack/agent/memory/`，
  `memoryDirectory` 设置或 `TACK_MEMORY_DIR` 环境变量可改，env 优先）。

两个作用域的 MEMORY.md 索引都自动注入**每个**新会话的系统提示（TUI/RPC/ACP/print
统一走 `assemble_system_prompt`）。`/memory` 分节查看，`/memory forget <name> [project|user]`
删除（不指定作用域时先 project 后 user），`/memory edit [<name>|user|project]` 用外部编辑器
改记忆文件（改完自动重建索引；不带名字直接编辑该作用域的 MEMORY.md）。

与 Claude Code 对齐的索引约束：条目部分超 **200 行 / 25KB** 时注入截断并附明确警告；
用量 ≥80% 时系统提示提醒 agent 合并精简；**写入导致索引超限会显式报错**（而不是静默
截断未来会话可见的内容），报错文案指导先压缩再重试。记忆文件 frontmatter 带
`modified`（ISO 8601）时间戳。

agent 被明确告知不要记 repo 里已有答案的东西（代码结构、CLAUDE.md 内容、git 历史）。

关闭：`"features": {"memory": false}`（索引注入同时消失）。

### 会话存储 v4

对齐上游 TS pi 的 WP01–WP08 存储演进（`tack_session::v4` + `v4_bridge` 模块，详见
crates/tack-session/V4_NOTES.md）。**v4 是默认写路径**（`sessionBackend` 默认 `v4`）：

- **事务日志格式**：首行 header（字段级对齐上游 `JsonlStorageHeader`），后续每行一个事务（`entry/usage/value/list` 写类型对齐上游 `CommittedWrite`），`seq` 全会话单调；branch/lane 分离与命名分支（WP06/08 最小可用语义）。
- **打开即迁移**：v3/v2/v1 会话文件打开时透明迁移到 v4（两遍流式、留 `.bak`、原子发布、加密会话保持加密、失败不动原文件）。`sessionBackend: "v3"` 保留旧写路径作逃生门（并拒绝打开 v4 文件）。
- **写路径映射**：每条 entry 一个事务 = entry + branch tip 移动 + 镜像值（session name/label/lane config）+ usage ledger 行（assistant/compaction/branch_summary 的 LLM 用量）；`model_change`/`thinking_level_change`/`session_info`/`label` 存为 custom 条目并镜像当前值——会话上下文重建（模型/思考级别）与 v4 原生工具（fork 投影）两边都正确。
- **fork-policy 投影规则**在生产路径生效：`tack fork` 经 `run_v4_fork` 产出 v4 分叉（lane state 重置、usage ledger 不拷贝、`parentSessionId` 指针）；未知 `tack.*` 保留命名空间报错而非静默拷贝。
- **不丢数据**：未知/未来条目类型（TS 扩展写入的）迁移时保留为 custom 条目（上游直接报错）；无 id 的损坏行跳过（负载留在 .bak），绝不让一行坏数据锁死整个会话。
- SQLite 后端不变（上游 WP07 尚未对齐）。

### 跨会话搜索

`/search <query>`：全文检索所有历史会话的 user/assistant 消息，按时间倒序给出片段，配合 `/resume` 打开。"上次那个问题怎么解决的" 直接可查。

### 会话静态加密

`"sessionEncryption": true`——会话文件的 entry 行以 AES-256-GCM 加密落盘（`tack-enc:v1:` 前缀，header 行保持明文以便列表/搜索）。密钥存在 OS 凭据库（首次使用自动生成），密钥不可用时加密行静默跳过。合规场景（笔记本丢失/共享目录）开启。

### 定时任务

```
/cron add "every 10m" 检查 cargo test 有没有挂
/cron add "*/5 * * * *" 汇总一下当前进度
/cron            列出任务（含下次触发时间）
/cron pause|resume|remove <id>
```

任务持久化在 `~/.tack/agent/cron.json`；触发时 agent 空闲则直接开跑，运行中则排队为 steering。**关闭期间错过的任务下次启动补发一次**，然后恢复节奏。

关闭：`"features": {"cron": false}`。

## 安全纵深

防御模型三层（详见 configuration.md 的配置细节）：

| 层 | 控制 | 配置 |
|---|---|---|
| 能力 | 功能在不在 | `features.*` |
| 许可 | 这次调用要不要问 | `permissions.allow/deny` + 权限模式 |
| 围限 | 跑起来能碰什么 | `sandbox` |

### 声明式权限

```json
"permissions": {
  "allow": ["Bash(cargo *)", "Bash(npm run test*)", "Edit(src/**)"],
  "deny":  ["Bash(git push *)", "Edit(**/.env*)", "Bash(rm -rf *)"]
}
```

deny 无条件优先（bypass 模式也拦），headless 模式（CI）同样生效。TUI 权限弹窗选 "always" 会持久化（重启不丢）。

### 不可信内容防线

web_fetch/web_search/MCP 的结果：

1. 被 `<untrusted_content source="…">` 包裹——模型被告知里面是数据不是指令；
2. 本 run 内见过不可信内容后，bash/edit/write **强制弹窗**（allow 规则和 allow-always 缓存都失效），新用户 prompt 时复位；
3. bypass 模式不受限（用户显式选择的豁免）。

这是针对"网页里嵌一句 ignore previous instructions → agent 拿 allow-always 的 bash 执行"这条链的专门防御。

### OS 沙箱

默认**开启**（`"sandbox": "off"` 可显式关闭）：bash（含后台任务）进入：

- **Linux** bubblewrap：只读系统、工作区可写、`sandboxNetwork: false` 断网；
- **macOS** seatbelt：同类策略（sandbox-exec profile）；
- **Windows** Job Objects：进程树可靠终止 + `sandboxMaxProcesses`/`sandboxMaxMemoryMb` 资源限额（**注意：不是文件系统隔离**）。

**边界须知**：沙箱限制的是**写**，不限制**读**——沙箱内进程仍能读取 `~/.ssh`、云凭据等文件，且 `sandboxNetwork` 默认 `true`。要防"读取密钥并外发"这条 exfiltration 链，必须同时设 `sandboxNetwork: false`（或在权限层对敏感命令弹窗）。另外文件工具（read/edit/write）本身不做工作目录围栏，边界完全在权限层——headless/auto-approve 部署时请注意这一点。

无后端平台降级运行并在日志警告一次。LSP/编辑工具不走沙箱（它们走权限系统）。

### serve 远程安全

```
tack serve --listen tcp:0.0.0.0:7749 --auth-token $(openssl rand -hex 32) --tls
```

- `--auth-token`/`--auth-token-file`：hello 帧带 token，SHA256 常量时间比较；
- `--tls`：首次启动自动生成自签证书（agent 目录 serve-cert.pem/serve-key.pem，0600），客户端 `--tls --tls-ca <cert>` 或 `--tls-insecure`；
- 非 loopback 绑定且无 token/无 TLS 时打印醒目警告。

`--listen ws:ADDR`（WebSocket 传输 + 内嵌零依赖浏览器客户端，HTTP `GET /` 返回页面）同样受 `--auth-token` 保护（hello 帧带 token）；`--tls` 现在对 `ws:` 监听同样生效——复用同一份自签证书直接提供 wss://（客户端 `tack client --addr wss:host:port` 或 `ws:` + `--tls`），不再需要外挂 TLS 终止反代。

Web 客户端支持：权限弹窗（allow once/always/deny，并行工具调用排队逐个弹）、权限模式切换（ask/acceptEdits/plan/bypass，经新增的 `set_mode` 命令）、模型与 thinking 级别选择、会话 list/create/attach/switch。协议扩展全部增量（新 optional 字段 + `serde(other)` 未知消息兑底），且远程会话默认 bypass 模式——不会应答的旧客户端永远收不到新的 `permission_request` 事件，保持与 TS `@earendil-works/pi-protocol` v1 线兼容。

### Managed settings（组织强制）

组织级策略文件（路径见 configuration.md）可以：强制 sandbox 开、强制开关 features、禁用 bypass 模式、锁定 provider/模型、追加不可移除的 deny 规则。配合项目信任机制，企业内部署时用户侧和仓库侧都无法绕过组织策略。

### 凭据 keyring

`credentialStore: "auto"`（默认）：OS 凭据库（DPAPI/Keychain/libsecret）可用时 token/密钥存那里，auth.json 只留占位符；不可用（headless Linux）自动回退文件。`"file"` 锁死旧行为。已有文件凭据无缝继续工作。

### Trace 脱敏

observability 的 JSONL 日志写盘前自动 redact：敏感字段名（token/key/authorization/secret/password/credential/cookie）、`Bearer …` 值、URL 敏感查询参数全部替换为 `***`。

## 生态互操作

### Hooks（13 个事件）

PreToolUse / PermissionRequest / PostToolUse / UserPromptSubmit / SessionStart /
SessionEnd / PreCompact / PostCompact / Stop / SubagentStart / SubagentStop /
Interrupt / Notification。Claude Code 兼容：嵌套
`{matcher, hooks}` 配置（旧扁平格式自动兼容）、正则 matcher、
command/prompt/agent 三种 handler、stdout JSON verdict 协议（block /
permissionDecision / updatedInput 改写 / additionalContext 注入）。
`SubagentStart` 在子代理启动前发射（payload 带 `agent_type`/`prompt`/
`description`/`isolation`/`background`，matcher 按子代理名匹配）：block
verdict 拒绝启动（工具返回错误、零 token 消耗，后台子代理同步 gate
不会注册注定失败的任务），`additionalContext` 追加进子代理 task。
零扩展进程成本——settings.json 里直接写 shell 命令；企业可用
`managed-hooks.json` + `managedHooksOnly` 锁定只跑托管 hooks。详见
[hooks.md](hooks.zh-CN.md)。

### 扩展系统（bundle / marketplace / WASM 载体）

- **Extension bundle**：`extension.json` 除插件进程外可声明 `hooks`
  （Claude 格式，并入会话 hooks）、`mcpServers`（并入 MCP 连接）、
  `skills`（并入技能发现）；bundle-only 清单（无 command/module）也合法。
- **Marketplace**：`tack ext marketplace add/list/remove` 注册 JSON catalog
  （本地文件或 URL，缓存于 `~/.tack/agent/marketplaces/`），
  `tack ext install <plugin>@<marketplace>` 经目录解析安装。
- **WASM 载体**（tack-ext v2）：`carrier: "wasm"` + `module` + `limits` 把插件跑成
  wasmtime 沙箱里的 WASI p1 模块——默认无 fs/网络/环境变量，fuel / epoch
  墙钟 / 内存硬上限由 host 钳制。线协议与子进程载体逐字节相同（复用同一个
  `PluginPeer`），握手 `protocol: 2`；示例 `examples/extensions/hello-wasm/`
  （手写 WAT 的协议参考实现）。

详见 [plugin-system.md](plugin-system.zh-CN.md)、[extensions.md](extensions.zh-CN.md)、
[extensions-v2.md](extensions-v2.zh-CN.md)。

### MCP server 模式

`tack mcp-serve`（stdio）：其他 agent/IDE 可以把 Tack 当 MCP server 调用。

| 工具 | 说明 |
|---|---|
| `prompt(text)` | 跑完整 coding agent（上下文在服务存活期内持续累积） |
| `get_session_stats()` | token/成本累计 |
| `read_context(maxMessages?)` | 最近消息摘要 |
| `list_available_tools()` | 可用工具清单 |
| `reset_session()` | 清空上下文 |

### MCP client OAuth 2.1

mcp.json 的远程（HTTP）server 加 `"oauth": true` 或 `{"clientId": "…", "scopes": […]}` 即启用：元数据发现 → 动态客户端注册（或配置的 clientId）→ PKCE 浏览器授权（loopback 回调，支持手动粘贴）。令牌缓存 `~/.tack/agent/mcp-tokens.json`（0600），过期自动用 refresh_token 续期。缓存按 **server 名**（mcp.json 里的键名）索引——TUI 里授权过的 server，同名配置在 print/rpc/serve 等 headless 模式直接命中缓存。TUI 交互授权；print/rpc/serve 只用缓存/续期令牌，绝不弹浏览器。

### MCP sampling 与 elicitation（server 反向请求）

除工具/资源/提示外，MCP server 还可以反向请求客户端：

- **Sampling**（`sampling/createMessage`）：server 请求一次 LLM 补全。settings.json 设 `"mcpSampling": true` 开启（**默认关闭**，不声明能力）。开启后用当前会话的 provider/model 执行，安全防线与 MCP 工具结果一致：server 提供的 system prompt/messages 全部包裹 `<untrusted_content>` 并加防护前缀，只在隔离的子调用上下文中使用，绝不进主会话；拒绝 tools/toolChoice（不给 server 驱动本地工具的旁路）与 audio；`modelPreferences.hints` 忽略（始终用当前模型）。每次调用写 tracing 日志，token usage 计入会话（TUI 底部统计 + 提示）。
- **Elicitation**（`elicitation/create`）：server 向用户请求结构化输入。`"mcpElicitation": true`（**默认开启**）。TUI 按 server 给的 JSON schema 逐字段弹输入框（string/number/integer/boolean/enum 自动类型转换，必填为空会重问，Esc = cancel）；print/rpc/acp/serve 等 headless 模式自动 decline；URL 模式（server 指定的浏览器流程）一律 decline。决策逻辑（模式 × 开关 → 弹窗/拒绝）在 `mcp_elicitation::elicitation_decision`，有单测。

### 工具 schema 懒加载（tool search）

`mcpDeferThreshold: N`——工具总数超过 N 时，MCP 工具不直接进模型工具表（零 schema 开销），而是进一个 agent 不可见的延迟池；agent 用 `tool_search` 按能力检索，命中的工具从下一步起可调用。N=0（默认）关闭。

### Eval 回归评测

```
evals/
  examples/
    fix-typo/
      task.json   {"prompt": "…", "setup": "…", "verify": "…", "timeoutSecs": 300}
  solutions/    # selftest.sh 的参考解（无需模型即可自测任务有效性）
```

```
tack eval evals/examples --runs 3 --report report.json --baseline baseline.json
```

每个任务：setup（可选）→ headless 跑 agent → verify（exit 0 = 通过）。报告含通过率/token/成本；`--baseline` 对比旧报告，▲▼ 标记升降。`examples/ci/tack-eval.yml` 是现成的 GitHub Action 回归门禁（▼ 即 fail）。

**文档审计**（`evals/docs-audit/`）：`static_check.sh` 无需模型即可跑——交叉校验 docs 记录的 settings 键 / hook 事件 / CLI flags / `features.*` 键与代码实现是否漂移（文档有而代码无 = FAIL；已知豁免带原因清单）。另有 3 个 agent 驱动的深度审计任务（sandbox 默认值、权限 deny 优先、hooks verdict 协议），用确定性锚点生成期望值，文档或代码任一侧漂移时任务自动翻转预期，定义无需手改。

## 运维工具

- **`tack stats`**：跨会话用量报表——扫描 agent 目录全部项目会话，聚合分 provider×model 的 token（input/output/cache read/cache write/thinking）与按 catalog 定价估算的成本、按天时间序列；`--since 7d`/`--until`/`--json`/`--dir <path>`。只读防御式解析：v3/v4 两种会话格式都支持（v4 以 usage ledger 为准不重复计数），加密 entry（无 key 时）与损坏行跳过计数，SQLite 后端经 public API 纳入；catalog 缺定价的模型标 `-` 不计入总成本。
- **`tack doctor`**：逐项自检（shell/git/5 种 LSP server/沙箱后端/headless 浏览器/凭据/MCP server 命令/fd+rg），每项带修复建议，硬失败退出码 1。报 bug 前先跑它。`--json` 输出机器可读报告（含版本/平台/TERM 等元数据）；`--bundle [PATH]` 生成报 bug 用 tar.gz（doctor 报告 text+json、脱敏后的 settings.json、crash.log；auth.json 永不包含），默认文件名带时间戳。
- **`tack logs`**：`--tail 100 --level warn --target tack_tools::lsp --follow`；TUI 内 `/trace [level] [target]`。
- **结构化 trace**：`observability.enabled` 或 `TACK_TRACE_FILE=1` 开 JSONL 导出（`~/.tack/agent/logs/`），脱敏见上。
- **Managed 审计 sink**：managed-settings.json 里的 `auditSink: {"url": "https://…", "token": "…", "intervalMs": 5000}`——trace 事件批量化 HTTP 上报（Bearer 鉴权、50 条或间隔触发、失败丢弃不阻塞，本地 JSONL 保留）。仅 managed 层可设，设置后强制开启 observability。

## TUI 体验

- **启动更新提示**：TUI 启动后后台检查新版本（结果缓存 24h，离线跳过），有更新时 footer 显示 `↑ vX.Y.Z` 并在聊天区提示 `tack update`。关闭：`"updateCheck": false`。
- **Ctrl+R 历史反向搜索**：编辑器内 Ctrl+R 进入增量搜索——输入即过滤 prompt 历史（新→旧，大小写不敏感），↑/↓/重复 Ctrl+R 循环命中，Enter 接受、Esc 取消恢复草稿。（原 Ctrl+R 的 dequeue 绑定迁移为 Alt+↑。）
- **桌面通知**：权限弹窗弹出、run 完成/出错、后台任务完成时发 OSC 9 / OSC 777 终端通知（按事件源 5s 节流，终端不支持则静默无效）。关闭：`"notifications": false`。
- **plan mode 闭环**：`/mode plan` 下 agent 拿到 `exit_plan_mode` 工具——计划落盘 `~/.tack/agent/plans/` 并弹审批框：Yes→acceptEdits（改动自由、命令仍问）、Always→bypass、No→留在 plan 模式修订。
- **ask_user 工具**：agent 可以在 run 中途暂停，向用户提结构化问题（`ask_user`，每次 1–4 个）——多选项选择题（2–4 个带描述的选项，外加“其他…”自由输入出口；`multi_select: true` 时变为可多选的勾选对话框，回答是全部选中标签）或省略选项时的纯自由输入。TUI 逐题弹对话框（Esc 取消整批，工具结果会提示模型自行决策）。headless 模式（print/rpc/acp/serve）和 subagent 注册的是无交互 handler 的工具：调用后返回带内的“无交互用户”消息，模型据此自行判断继续，不会永久阻塞（与 MCP elicitation decline 同一策略）。
- **多工作区**：`--add-dir <path>`（可重复）或 `additionalDirs`——额外目录的 AGENTS.md 进上下文，系统提示列出在 scope 的目录，沙箱可写集合合并。
- **i18n**：`language: "zh"`（或 LANG=zh*）——TUI 用户可见文案全量双语（斜杠命令帮助与描述、对话框标题/按钮/提示、通知/错误、状态栏、footer、/settings /model /providers /trust /resume 等界面）；日志/调试输出、命令名、协议值（权限模式、思考级别）不翻译；缺译自动回落英文。
