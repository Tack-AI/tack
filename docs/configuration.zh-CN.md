# Tack 配置参考

**[English](configuration.md) | 简体中文**

Every settings.json key, environment variable, CLI flag, and on-disk file —
with types, defaults, and precedence.

## 配置层级与优先级

Tack 从四级读取配置，**后加载的层级覆盖先加载的**（部分键有特殊规则，见下）：

| 层级 | 位置 | 说明 |
|---|---|---|
| **Managed**（组织强制） | Linux `/etc/tack/managed-settings.json` · macOS `/Library/Application Support/tack/managed-settings.json` · Windows `%ProgramData%\tack\managed-settings.json`（`TACK_MANAGED_SETTINGS` 可覆盖路径，仅限开发/测试构建——release 构建会忽略该变量） | 最高优先级。深度合并时最后应用。 |
| **Global**（用户） | `~/.tack/agent/settings.json` | 用户默认配置。 |
| **Project**（项目） | `<project>/.pi/settings.json` | 仅当项目被信任时加载（`/trust`）。 |

特殊合并规则（**不是**简单的后者覆盖）：

- **`features.*`**：project 只能**关**不能开（AND 合并）；managed 可以双向强制。
- **`sandbox`**：global 可以开，project 只能显式关，managed 双向。
- **`permissions.allow/deny`**：三层 **UNION 合并**——任何一层加规则都生效，组织的 deny 列表不可被丢弃。
- **`hooks.*`**：深度合并（同名事件的项目 hooks 覆盖全局列表）。

## settings.json 全键参考

### 模型与 provider

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `defaultProvider` | string | `"anthropic"` | 默认 provider |
| `defaultModel` | string | provider 默认 | 默认模型 id |
| `scopedModels` | string[] | `[]` | ctrl+p 循环的模型范围（"provider/id"） |
| `fallbackModels` | string[] | `[]` | fallback 链（"provider/id" 有序）。主模型重试耗尽后遇 429/过载/5xx/超时自动降级 |
| `transport` | string | `"auto"` | Codex 传输：`auto`（WS 优先 SSE 回退）\| `sse` \| `websocket` |
| `modelCatalogRefresh` | bool | `false` | 启动时从 npm 上的 `@earendil-works/pi-ai` 拉取最新模型目录（后台执行，失败仅记日志）。手动 `/models refresh` 不受此开关限制；已拉取的目录缓存总是会被加载 |
| `backgroundAutoWake` | bool | `true` | 后台任务在 agent 空闲时完成：自动开一轮让 agent 读输出接着干（与 cron 触发同一模式）。设为 `false` 则只在 transcript 出通知，不自动唤醒 |
| `httpIdleTimeoutMs` | number | — | HTTP 读空闲超时 |
| `retry` | object | `{enabled:true, maxRetries:3, baseDelayMs:2000, maxAgentDelayMs:60000}` | 瞬态错误自动重试（`maxAgentDelayMs` 为单次退避上限） |
| `compaction` | object | `{enabled:true, reserveTokens:…, keepRecentTokens:…, goalRecitation:true, minKeptTurns:2}` | 自动上下文压缩。`goalRecitation`：压缩后把摘要的 Goal/Next Steps 作为尾部用户消息复述进上下文（仅 LLM 侧副本，append-only 不破坏缓存）；`minKeptTurns`：压缩切点保证的最少保留轮数 |
| `microcompact` | object | `{enabled:true, maxChars:20000, keepRecent:3, minSavingsChars:8000}` | 老超长 tool result 原地裁剪（全文落盘可回读）。`minSavingsChars`：总回收量低于该值不改写历史（保护提示缓存前缀）。`enabled:false` 会连带关闭下面三项历史优化 |
| `maskDuplicateReads` | bool | `true` | 同一文件被 read 两次且内容逐字节一致时，把较早的结果替换为指针（较新副本仍在上下文中，零信息损失） |
| `toolResultMaxChars` | number | `60000` | 任意 tool result（含最新几轮）的硬上限，超出截断并把全文落盘 `<id>.full.log`；`0` 关闭 |
| `rulesMaxChars` | number | `40000` | 单个 AGENTS.md/CLAUDE.md 上下文文件的系统提示预算，超出截断并标注磁盘路径；`0` 不限 |

### 预算

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `tokenBudget` | number | — | 会话总 token 预算 |
| `tokenBudgetAction` | string | `"warn"` | 超限动作：`warn`（TUI 警告）\| `pause`（turn 边界停 run）\| `downgrade`（切换便宜模型） |
| `budgetDowngradeModel` | string | fallbackModels 末位 | downgrade 目标模型（"provider/id"） |
| `subagents.maxConcurrent` | number | `0`（不限） | 子代理并发上限：同时运行的子代理循环数，超出排队的共享信号量 |
| `subagents.budgetTokens` | number | `0`（不限） | 子代理共享 token 预算：本次会话所有子代理（含后台）累计用量上限，超出后拒绝新的子代理调用（子代理不落会话文件，预算钩子看不到它们的用量，故单独管控） |
| `subagents.inheritPlugins` | string | `"hooks"` | 子代理循环继承哪些插件面：`none`（仅内置工具 + deny 规则）\| `hooks`（插件钩子桥——beforeToolCall 拦截、上下文变换、结果补丁——跟随子代理，堵住护栏绕过缺口）\| `full`（钩子 + 插件工具；agent 定义的 `tools` 白名单仍会收窄合并后的集合）。与普通设置键一样分层，managed 层可钉死 |

### 功能开关（features.*）

全部默认 **true**（含 `sandbox`；不再需要显式开启）。关闭的功能对 agent 完全不可见：工具不注册、系统提示零痕迹、子系统不启动。

| 键 | 控制内容 |
|---|---|
| `features.lsp` | lsp 工具（diagnostics/definition/references/implementation/symbols/rename/hover/code_actions/workspace_symbols/incoming_calls/outgoing_calls）+ edit/write 诊断反馈 + language server 进程 |
| `features.checkpoints` | turn 文件快照 + git 基线 + `/checkpoints` 回滚 |
| `features.backgroundTasks` | bash `run_in_background` + `bash_output`/`bash_wait`/`kill_shell` + 后台子代理 |
| `features.memory` | `memory` 工具 + MEMORY.md 系统提示注入（project/user 双作用域）+ `/memory` |
| `features.shellHooks` | `hooks.*` 里的 shell 命令（配了也不执行） |
| `features.cron` | 定时任务（`/cron` + 触发） |
| `features.sandbox` | OS 沙箱（等价于 `sandbox` 键） |

旧键映射：`lspDisabled: true` ≡ `features.lsp: false`；`checkpointsDisabled: true` ≡ `features.checkpoints: false`（新键优先）。

### LSP

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `lspServers` | object | 内置表 | 扩展名 → server 覆盖：`{"rs": "rust-analyzer", "ts": {"command": "typescript-language-server", "args": ["--stdio"]}}` |
| `lspEditFeedback` | bool | `true` | edit/write 结果附诊断摘要 |

内置 server 探测：`rs`→rust-analyzer、`ts/tsx/js/jsx/mts/cts/mjs/cjs`→typescript-language-server --stdio、`py/pyi`→pyright-langserver --stdio、`go`→gopls、`c/h/cpp/cc/cxx/hpp/hh`→clangd。二进制不在 PATH 时该扩展名静默禁用（`tack doctor` 可见）。

### 权限（permissions.*）

```json
"permissions": {
  "allow": ["Bash(npm run *)", "Edit(src/**)", "WebFetch"],
  "deny": ["Bash(rm -rf *)", "Edit(**/.env)", "Edit(**/secrets/**)"]
}
```

- 语法：`Tool(pattern)` 或裸 `Tool`（该工具全部调用）。
- bash 命令：`*` 通配（首尾锚定）；文件路径：glob（`**` 跨目录）。
- **deny 永远优先**，且在 headless 模式（print/rpc/serve/CI）同样生效。
- TUI 里选 "always" 的答复持久化到 `~/.tack/agent/permissions.json` 的 `allowAlways` 数组，重启后仍生效。插件工具（`ext__*`）会同时记录加载时的插件版本（`extToolVersions` 映射），只有该版本仍在加载时条目才生效——`tack ext upgrade` 会使过期批准失效。
- 不可信内容防线（见 features 文档）会临时绕过 allow 规则强制弹窗。

### 沙箱

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `sandbox` | string/bool | `"on"` | `"off"`/`false` 关闭 OS 沙箱（默认开启：bash 写入限制在工作区） |
| `sandboxNetwork` | bool | `true` | 沙箱内允许网络 |
| `sandboxMaxProcesses` | number | — | Windows Job Object 进程数上限 |
| `sandboxMaxMemoryMb` | number | — | Windows Job Object 内存上限（MB） |

后端：Linux bubblewrap（只读系统 + 工作区可写）、macOS seatbelt、Windows Job Objects（进程树杀 + 资源限额，非文件系统隔离）。无后端平台降级运行并警告一次。

**可写集合** = 主 cwd + `additionalDirs`/`--add-dir` 的每个目录 + 内置允许（macOS/Linux：`/tmp`、`/private/tmp`、`/private/var/folders`、`/dev`）。集合外的写入在沙箱内报 EPERM，输出会附带当前可写根列表的提示。

**沙箱内的 CARGO_HOME**：默认 `~/.cargo` 通常不在可写集合里，沙箱内任何要写 registry 的 cargo 命令都会 EPERM。executor 因此自动回退——未显式设置 `CARGO_HOME` 且 `~/.cargo` 不可写时，注入 `CARGO_HOME=$TMPDIR/tack-cargo-home`（各后端都可写，OS 清理后 cargo 自动重新拉取）。要让沙箱内 cargo 使用**真实的** `~/.cargo`（与终端里的 cargo 共享缓存），把它的**绝对路径**加进 `additionalDirs`（设置解析不做 `~` 展开）：

```json
{ "additionalDirs": ["/home/you/.cargo"] }
```

`~/.cargo` 被可写集合覆盖后自动回退不再触发，cargo 直接用默认路径。注意：settings 启动时加载，改动需**重启**生效；`additionalDirs` 同时是 multi-root 机制（目录会出现在系统提示、加载其 AGENTS.md、并入 LSP roots），且沙箱对该目录的写保护随之消失——这正是目的，但意味着 agent 的 bash 可任意改动它。只 `export CARGO_HOME=~/.cargo` **无效**：显式值优先于自动回退，而目录不可写时照样 EPERM。

### Hooks（hooks.*）

**Claude Code 兼容**的生命周期钩子（命令 / LLM 评估 handler、12 事件、
`updatedInput` 改参数、`permissionDecision` 权限裁决、`additionalContext`
注入、managed hooks）。完整协议见 [hooks.md](hooks.zh-CN.md)。

```json
"hooks": {
  "PreToolUse": [{"matcher": "bash", "hooks": [{"type": "command", "command": "check.sh"}]}],
  "SessionStart": [{"command": "cat .pi/context.md"}]
}
```

相关设置：`managedHooksOnly: true` 只执行 `~/.tack/agent/managed-hooks.json`。

### 凭据与安全

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `credentialStore` | string | `"auto"` | 凭据存储：`auto`（keyring 可用则用，否则文件）\| `keyring`（强制 OS 凭据库）\| `file`（auth.json 明文） |
| `defaultProjectTrust` | string | `"ask"` | 项目信任：`ask` \| `always` \| `never` |

### 可观测性

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `observability.enabled` | bool | `false` | 结构化 JSONL trace 到 `~/.tack/agent/logs/` |
| `observability.level` | string | `"info"` | 文件层日志级别 |

环境变量 `TACK_TRACE_FILE=1` / `TACK_TRACE_LEVEL=debug` 优先。写盘前自动脱敏（token/authorization/Bearer/URL 敏感参数）。

### Web

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `webRender` | string | `"auto"` | headless 渲染：`auto`（JS 空壳自动回退）\| `always` \| `off` |
| `webSearch.provider` | string | `"bing"` | 搜索后端：`bing` \| `duckduckgo` \| `brave` \| `tavily` \| `exa`。两个免 key 的抓取后端（`bing`/`duckduckgo`）失败时互相回退 |
| `webSearch.apiKey` | string | — | 后端 API key（或用 `BRAVE_API_KEY`/`TAVILY_API_KEY`/`EXA_API_KEY` 环境变量） |

### 会话与历史

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `sessionBackend` | string | `"v4"` | 会话存储：`v4`（事务日志，打开旧 v3/v2/v1 会话时透明迁移并留 .bak）\| `v3`（legacy JSONL，与旧版 TS pi 字节兼容；拒绝打开 v4 文件）\| `sqlite`（实验性；跨会话搜索已覆盖 sqlite 会话） |
| `sessionEncryption` | bool | `false` | 会话静态加密：entry 行 AES-256-GCM 加密落盘（密钥存 OS 凭据库，首次自动生成） |
| `steeringMode` | string | `"all"` | steering 投递：`all` \| `one-at-a-time` |
| `followUpMode` | string | — | follow-up 投递：同上 |
| `doubleEscapeAction` | string | `"tree"` | 空编辑器双击 Esc：`tree` \| `fork` \| `none` |
| `treeFilterMode` | string | `"default"` | /tree 过滤：`default` \| `no-tools` \| `user-only` \| `labeled-only` \| `all` |
| `additionalDirs` | string[] | `[]` | 多工作区：额外工作目录（加载其 AGENTS.md、沙箱可写合并——可用来放行 `~/.cargo` 等，见「沙箱」一节；须绝对路径） |

### TUI 外观与行为

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `theme` | string | 终端检测 | 主题名（内置 dark/light + catppuccin-mocha/-latte、tokyo-night、gruvbox-dark/-light、nord、dracula、one-dark、solarized-dark/-light、kanagawa、monokai、rose-pine/-dawn；外加 themes/ 目录自定义；支持 `"light/dark"` 自动配对，如 `"catppuccin-latte/catppuccin-mocha"`） |
| `tuiMode` | string | `"regular"` | `regular` \| `fullscreen`（alt-screen） |
| `language` | string | LANG 环境变量 | 界面语言：`en` \| `zh`——TUI 全量文案（斜杠命令帮助、对话框、通知/错误、状态栏、footer）双语覆盖；日志/调试输出、命令名、协议值不翻译 |
| `mermaid` | string | `"image"` | mermaid 渲染：`image` \| `off`（需要编译时启用 `mermaid` cargo feature，默认不启用；未启用时代码块按普通代码渲染） |
| `terminal.showImages` | bool | `true` | 内联图片 |
| `terminal.imageWidthCells` | number | — | 图片宽度（单元格） |
| `terminal.clearOnShrink` | bool | `false` | 终端缩小时清屏 |
| `terminal.hyperlinks` | bool \| `"auto"` | `"auto"` | OSC 8 超链接能力覆盖；`auto` 保留探测（优先于 `TACK_HYPERLINKS`） |
| `terminal.images` | `"kitty"` \| `"iterm2"` \| `false` \| `"auto"` | `"auto"` | 内联图片协议能力覆盖（优先于 `TACK_IMAGE_PROTOCOL`） |
| `terminal.trueColor` | bool \| `"auto"` | `"auto"` | 24-bit 色彩能力覆盖（优先于 `TACK_TRUE_COLOR`） |
| `fullscreenCopyOnSelect` | bool | `true` | 全屏拖选自动 OSC 52 复制；`false` 时选区保持高亮，`Ctrl+X` 复制选区（无选区则复制最后一条 assistant 消息） |
| `images.blockImages` | bool | `false` | 不向 LLM 发送图片 |
| `imageProtocol` | string | 自动探测 | 图片协议：`kitty` \| `iterm2` \| `half-block` |
| `editorPaddingX` | number (0-3) | `0` | 编辑器左右边距 |
| `markdown.codeBlockIndent` | string | — | 代码块额外缩进 |
| `autocompleteMaxVisible` | number (3-20) | — | 补全列表可见条数 |
| `fullscreenExitOutput` | string | — | 退出全屏时 `"transcript"` 打印会话记录 |
| `externalEditorCommand` | string | `$EDITOR` | 外部编辑器命令 |
| `hideThinkingBlock` | bool | `false` | 隐藏思考块 |
| `collapseChangelog` | bool | `false` | 启动 changelog 只显示一行 |
| `quietStartup` | bool | `false` | 跳过 banner + changelog |
| `showCacheMissNotices` | bool | `false` | 提示词缓存未命中通知 |
| `cacheRetention` | `"short"` \| `"long"` \| `"off"` | `"short"` | 提示词缓存保留时长：`short`=5 分钟写入（提供商默认）；`long`=1 小时写入（OpenAI 为 24h，写入费更高；Kimi/Anthropic 协议发 `ttl:"1h"`，Moonshot OpenAI 协议发 `prompt_cache_options`）；`off`=不发缓存标记（支持处只读）。未设置时回退 `TACK_CACHE_RETENTION` 环境变量（`long` 生效），再回退 `short`。注意 Kimi 的缓存 TTL 首次写入后锁定，中途切换需等旧条目过期才生效 |
| `enableSkillCommands` | bool | `true` | `/skill:<name>` 补全 |
| `defaultTools` | string[] | `[]`（全部） | 内置工具 allowlist（与 features.* 取交集）。可选的 `powershell` 工具（Windows）默认关闭，仅在此显式列出时注册——如 `["read", "powershell", "edit", "write"]` 替换 bash，或同时列出两者。优先 `pwsh.exe`，回退 `powershell.exe`，启动参数 `-NoProfile -NonInteractive -ExecutionPolicy Bypass`；非 Windows 平台注册后执行会报错。权限规则写法 `PowerShell(...)`，通配语义同 `Bash(...)` |
| `updateRepo` | string | `"Tack-AI/tack"` | 自更新 GitHub 仓库 |
| `updateCheck` | bool | `true` | TUI 启动时后台检查新版本（结果缓存 `update-check.json`，24h TTL），有更新在 footer 与聊天区提示。`--offline`/`TACK_OFFLINE` 时跳过；只检查不安装 |
| `notifications` | bool | `true` | 桌面通知（OSC 9 / OSC 777 escape）：权限弹窗弹出、agent run 完成/出错、后台任务完成时通知（按事件源 5s 节流）。终端不支持则静默无效 |
| `shellPath` | string | 自动探测 | bash 工具使用的 shell 路径覆盖（默认按 Git Bash → `where bash` 顺序探测，Windows） |
| `memoryDirectory` | string | `~/.tack/agent/memory` | 持久记忆 user 作用域根目录（project 作用域在其下 `projects/<仓库>/`）；`TACK_MEMORY_DIR` 环境变量优先 |
| `appendSystemPrompt` | string | — | 追加到系统提示末尾的文本（等价 `--append-system-prompt` flag） |

### 扩展资源目录

| 键 | 说明 |
|---|---|
| `skills` | 额外 skills 目录（string[]） |
| `prompts` | 额外 prompt 模板目录 |
| `themes` | 额外主题目录 |

另：`extensionLockRequired`（bool，默认 `true`）——扩展供应链锁定：用户目录下经 `ext install` 安装的插件在启动时校验 git HEAD 与 `extensions-lock.json` 中的 resolvedCommit，不一致则 warn 并跳过该插件；设为 `false` 时仅 warn 仍加载。无 lock 条目的插件不受影响（见 docs/extensions.zh-CN.md §1c）。managed 层可双向强制该值。

另：`plugins`（对象）——按插件 id（`name@source`，见 docs/plugin-roadmap.zh-CN.md）设置的插件级配置。目前唯一的插件级键是 `enabled`（bool，默认 `true`）：被禁用的插件保持安装但不会被拉起（其元数据仍在 `ext list` 中可见）。可用 `tack ext enable|disable <id>` 管理，也可直接编辑配置文件。

另：`pluginMarketplaces`（对象）——由后台启动同步保持新鲜的策展市场目录。键为市场名；值为来源字符串或对象 `{"source", "ref"?, "path"?, "publicKey"?}`（git 仓库 URL、https `.json` 目录或本地文件/目录）。**仅从全局层与 managed 层读取**（目录可通过 `installed-by-default` 推送代码，项目层不得重定向——与 `updateRepo` 同规则）。见 docs/extensions.zh-CN.md §5.2。

### Managed 层专属键

仅 managed-settings.json 生效：

| 键 | 说明 |
|---|---|
| `disableBypass` | 禁用 bypass 权限模式（/mode 循环跳过 + hook 层兜底降级） |
| `lockedProvider` | 锁定 provider（启动和 /model 都拦截） |
| `lockedModel` | 锁定模型 id |
| `auditSink` | 审计上报：`{"url": "…", "token": "…", "intervalMs": 5000}`——trace 事件批量 POST（换行分隔 JSON，Bearer 鉴权）。设置后强制开启 observability |
| `pluginPolicy` | 企业插件策略：`managedPluginsOnly`、`allowedSources`（git/hostPattern/local 来源白名单）、按插件的 `enabled`（压过用户/项目层）、只收窄的 `tools`/`mcpServers` 交集、`provider` 桥服务闸门（置 `false` 时 provider-stream 插件在加载时被策略阻止），以及 `hooks` 能力闸门（置 `false` 时在注册时剥离插件的 hook 桥——无工具调用拦截——其工具/命令仍照常加载）。安装时与加载时双重执行；决策记入审计日志。见 docs/extensions.zh-CN.md §9 |

### MCP（mcp.json）

每个 server 条目除 `command`/`args`/`env`（stdio）或 `url`/`headers`（HTTP）外：

| 键 | 说明 |
|---|---|
| `oauth` | `true` 或 `{"clientId": "…", "scopes": ["…"]}`——远程 server 的 OAuth 2.1 授权（PKCE + 动态注册，令牌缓存 `mcp-tokens.json` 并自动续期） |

settings.json 另有 `mcpDeferThreshold`（number，默认 0=关闭）：工具总数超阈值时 MCP 工具延迟加载，agent 通过 `tool_search` 按需激活（见 features 文档）。

settings.json 的另外两个 MCP 开关：

| 键 | 说明 |
|---|---|
| `mcpSampling` | boolean，默认 `false`。为 `true` 时允许 MCP server 反向请求 LLM 补全（`sampling/createMessage`）：用当前会话的 provider/model 在隔离上下文中执行（server 提供的消息按 `<untrusted_content>` 处理、不进主会话；拒绝 tools/toolChoice 与 audio），usage 计入会话并写日志。默认关闭时不声明该能力。 |
| `mcpElicitation` | boolean，默认 `true`。MCP server 可向用户请求结构化输入（`elicitation/create`）：TUI 逐字段弹文本输入框（按 schema 做 string/number/integer/boolean/enum 类型转换，Esc 取消）；`tack serve` 会把表单转发给具备 dialog 能力的远程客户端（无可用客户端时 decline）；print/rpc/acp 自动 decline；URL 模式一律 decline。 |

## 环境变量

| 变量 | 说明 |
|---|---|
| `TACK_AGENT_DIR` | agent 目录覆盖（默认 `~/.tack/agent`） |
| `TACK_MANAGED_SETTINGS` | managed settings 文件路径覆盖（仅限开发/测试构建；release 构建忽略） |
| `TACK_TRACE_FILE` | =1 开启 JSONL trace 导出 |
| `TACK_TRACE_LEVEL` | trace 文件级别（默认 info） |
| `TACK_BROWSER` | headless 渲染的浏览器可执行文件路径 |
| `TACK_REMOTE_TOKEN` | serve/client 的共享 token（等价 --auth-token） |
| `TACK_PROVIDER` / `TACK_MODEL` | ACP 模式的 provider/model 覆盖（`--provider`/`--model` 优先于它们） |
| `TACK_THINKING` | ACP 模式 thinking level（`--thinking` 优先） |
| `TACK_IMAGE_PROTOCOL` | 图片协议强制（kitty/iterm2/half-block） |
| `TACK_HYPERLINKS` | OSC 8 超链接能力覆盖：`1` \| `0` \| `auto`（`terminal.hyperlinks` 设置优先） |
| `TACK_IMAGE_PROTOCOL` | 图片协议能力覆盖：`kitty` \| `iterm2` \| `none` \| `auto`（`terminal.images` 设置优先） |
| `TACK_TRUE_COLOR` | 24-bit 色彩能力覆盖：`1` \| `0` \| `auto`（`terminal.trueColor` 设置优先） |
| `TACK_UPDATE_REPO` | 自更新仓库覆盖 |
| `TACK_MODEL_CATALOG_REGISTRY` | 模型目录刷新的 npm registry 基址（默认 registry.npmjs.org） |
| `TACK_OFFLINE` | 离线模式（禁 fd/rg 下载、分享、OAuth） |
| `TACK_CACHE_RETENTION` | `long` 时启用长缓存写入（1h；OpenAI 为 24h）；`cacheRetention` 设置优先 |
| `ANTHROPIC_BASE_URL` / `ANTHROPIC_MODEL` | Anthropic 兼容端点代理 |
| `CODEBUDDY_PATH` | codebuddy CLI 路径覆盖（不在 PATH 时；接受 `.cmd`/`.bat`/`.py`）。用法详见 [codebuddy.md](codebuddy.zh-CN.md) |
| `BRAVE_API_KEY` / `TAVILY_API_KEY` / `EXA_API_KEY` | 搜索后端 key |
| 各 provider 标准 key | `ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`GEMINI_API_KEY` 等（见 providers 表） |
| `LANG` | `zh*` 时 TUI 默认中文（`language` 设置优先） |

## CLI 标志

主命令（`tack [flags] [prompt | @file …]`）：

| 标志 | 说明 |
|---|---|
| `-p, --print <PROMPT>` | headless 单发模式 |
| `--provider / --model / --api-key` | 覆盖 provider/模型/凭据 |
| `-c, --continue` / `-r, --resume` / `--session / --session-id / --fork` / `--name` / `--no-session` / `--session-dir` | 会话控制 |
| `--models a,b,c` | ctrl+p 循环范围 |
| `-t, --tools` / `--exclude-tools` / `--no-tools` / `--no-builtin-tools` | 工具过滤（与 features.* 取交集） |
| `--no-skills` / `--no-context-files` / `--no-prompt-templates` / `--no-themes` | 资源开关 |
| `--skill <dir>` / `--add-dir <dir>` | 额外 skills 目录 / 额外工作目录（可重复） |
| `--mode text\|json\|rpc` | 输出模式 |
| `--tui-mode regular\|fullscreen` / `--use-theme <name>` | TUI 覆盖 |
| `--thinking off\|minimal\|low\|medium\|high\|xhigh\|max` | 思考级别 |
| `--append-system-prompt <text|path>` | 追加系统提示（可重复） |
| `--system-prompt` / `--prompt-template "name args"` | 系统提示覆盖 / 模板展开 |
| `--export <path>` | 结束时复制会话文件 |
| `--verbose` / `--offline` | 调试日志 / 离线 |
| `-a, --approve` / `--no-approve` | 项目信任覆盖 |
| `--list-models [pattern]` | 列出模型后退出 |

子命令：

| 子命令 | 说明 |
|---|---|
| `tack acp` | ACP server（Zed 等编辑器） |
| `tack rpc` | JSONL RPC（stdin 命令 / stdout 事件） |
| `tack mcp-serve` | MCP server（把 agent 暴露给其他 MCP 客户端） |
| `tack serve` | 远程会话宿主（CBOR；`--listen`、`--auth-token`/`--auth-token-file`、`--tls`/`--tls-cert`/`--tls-key`、`--allow-no-auth`）。`--tls` 对 `tcp:` 与 `ws:` 监听都生效（后者即 wss）。`--allow-no-auth` 允许非 loopback 监听不带 token 启动（危险：能连到端口的人即可在你机器上执行命令；loopback/unix 监听不需要） |
| `tack client` | 连接 serve（`--addr`、`--auth-token`、`--tls`/`--tls-ca`/`--tls-insecure`；`--addr ws:`/`wss:` 走 WebSocket，`wss:` 隐含 `--tls`） |
| `tack stats` | 跨会话用量报表：分 provider×model 的 token/成本估算、按天时间序列（`--since/--until/--json/--dir`） |
| `tack doctor` | 环境自检（shell/git/LSP/沙箱/浏览器/凭据/MCP/fd+rg；`--json` 机器可读输出，`--bundle [PATH]` 打包诊断 tar.gz：报告 + 脱敏 settings.json + crash.log，默认写到当前目录时间戳文件） |
| `tack logs` | trace 查看（`--tail/--level/--target/--follow`） |
| `tack eval <dir>` | 评测（`--runs/--filter/--report/--baseline`） |
| `tack login / logout / auth-status` | 凭据管理（OAuth 流程或 `--api-key`） |
| `tack fork <file>` / `tack compact` | 会话分叉 / 手动压缩 |
| `tack ext install/list/remove` | 扩展管理 |
| `tack update` | 自更新（`--check`/`--force`） |

## Agent 目录磁盘文件全览

```
~/.tack/agent/
  settings.json          # 全局配置
  auth.json              # 凭据（keyring 模式下只有 {"type":"keyring"} 占位）
  models.json            # 自定义 provider
  keybindings.json       # 键位覆盖
  mcp.json               # 全局 MCP servers
  mcp-tokens.json        # MCP OAuth 令牌缓存（0600，含 refresh_token/client_id）
  permissions.json       # allow-always 持久化（{"allowAlways": [...], "extToolVersions": {...}}）
  cron.json              # 定时任务
  catalog.json           # 刷新拉取的模型目录缓存（/models reset 删除）
  catalog.meta.json      # 目录来源与抓取时间（version/providers/models/fetchedAt）
  update-check.json      # TUI 启动更新检查缓存（checked_at/latest，24h TTL）
  trust.json             # 项目信任决定
  serve-cert.pem / serve-key.pem   # serve --tls 自签证书（自动生成）
  AGENTS.md / rules/     # 全局规则
  skills/ prompts/ themes/ agents/ # 资源目录（agents = 自定义子代理）
  memory/                # 持久记忆（MEMORY.md 索引 + 每条一文件）
  sessions/--<cwd>--/*.jsonl       # 会话（或 sqlite 模式的 sessions.db）
  checkpoints/<session-id>/turn-N/ # 文件快照（meta.json + blobs + state.json）
  plans/plan-<ts>.md     # plan mode 落盘的计划
  logs/tack-<day>.jsonl # 结构化 trace（observability）
  microcompact/<id>.log  # 裁剪掉的超长 tool 输出全文
  microcompact/<id>.full.log  # 硬上限截断前的 tool 输出全文
  bin/                   # 托管的 fd/rg
```

## 项目目录

```
<project>/.pi/
  settings.json   # 项目配置（需信任；只能禁用 features、可增权限/规则）
  mcp.json        # 项目 MCP servers（需信任）
  agents/         # 项目级自定义子代理（需信任，同名覆盖全局）
  skills/ prompts/ themes/
  AGENTS.md       # 项目规则（祖先扫描）
```
