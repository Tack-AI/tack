# Tack

<p align="center">
  <img src="assets/logo.svg" alt="Tack logo — 螃蟹夹钳造型的字母 A" width="160">
</p>

**[English](README.md) | 简体中文**

[pi](https://github.com/earendil-works/pi) 编码代理的 Rust 重实现 —— 交互式
TUI、headless print 模式、[ACP (Agent Client Protocol)](https://agentclientprotocol.com)
编辑器集成（Zed、JetBrains……）、JSONL RPC 模式，以及带内嵌浏览器客户端的
TCP/WebSocket 远程会话。与 TypeScript pi 线上协议和存储格式兼容。

## 亮点

- **与 TS pi 完全对齐** —— 相同的会话文件（v1–v4 透明迁移）、相同的
  RPC/ACP 线上协议、相同的 provider 注册表与模型目录、相同的 CLI 标志。
- **不止是移植** —— 后台任务、LSP 导航与诊断、文件 checkpoint、持久记忆、
  worktree 隔离的子代理、跨会话搜索、cron、OS 沙箱、声明式权限、MCP
  client *和* server、eval 框架等：**[docs/features.md](docs/features.md)**。
- **内置 42 个 provider** —— 40 个镜像 TS pi 注册表，外加零配置本地
  **ollama** 和 **llama.cpp** —— 内嵌完整模型目录（1130 个模型：上下文
  窗口、成本、推理标志、compat 怪癖）。
- **可扩展** —— 任意语言的子进程插件（NDJSON/JSON-RPC）或沙箱化 WASM，
  外加兼容 Claude Code 的生命周期 hooks。
- **远程优先** —— `tack serve` 通过 TCP/WebSocket/TLS + token 鉴权托管
  会话；可从 `tack client` 或浏览器接入。
- **默认隐私** —— 无遥测；本地 crash.log、opt-in tracing 和
  `tack doctor` 替代（[docs/telemetry.md](docs/telemetry.md)）。

## 安装

### 预编译二进制

从最新的 [GitHub release](https://github.com/sufar/tack/releases) 下载
`tack-<target-triple>.tar.gz` / `.zip`（`tack-v*` tag；windows x64/arm64、
linux x64/arm64、macOS arm64/x64）。

> **macOS Gatekeeper**：二进制没有 Developer-ID 签名，浏览器下载的压缩包
> 会触发 "Apple 无法验证…"。解压后执行一次
> `xattr -d com.apple.quarantine tack`（或右键 → 打开）即可。

### 源码构建

```bash
git clone https://github.com/sufar/tack.git && cd tack
cargo build --release -p tack-app        # 二进制：target/release/tack
```

工具链由 `rust-toolchain.toml` 钉定（MSRV 1.85，edition 2024）。

### 自更新

```bash
tack update          # 拉取最新 release 并原子替换二进制
tack update --check  # 只检查不安装
```

仓库解析顺序：`TACK_UPDATE_REPO` → settings `updateRepo` → `sufar/tack`。
进程内下载不带 macOS 隔离标志，所以自更新不受 Gatekeeper 影响。

## 快速上手

```bash
# 1. 发现 provider 并登录（首次运行）
tack providers                   # 每个 provider：认证状态、模型数量、如何启用
tack login --provider anthropic  # OAuth 流程（或从 stdin 读 API key）
tack models claude               # 按 provider 分组浏览模型目录

# 2. 运行
tack                             # 交互式 TUI（终端中的默认模式）
tack -c                          # 恢复此目录最近的会话
tack -p "create hello.py that prints primes < 50 and run it"   # headless print 模式
export ANTHROPIC_API_KEY=...      # headless：用环境变量代替登录
```

常用标志：`--provider`、`--model`、`--api-key`、
`--thinking off|minimal|low|medium|high|xhigh|max`、`--session-dir`、
`--system-prompt`、`--append-system-prompt`、`-t/--tools`、`--mode text|json|rpc`、
`--offline`。完整标志列表与 TS 对齐说明：
[docs/configuration.md](docs/configuration.md)。

### 编辑器集成（ACP）

`tack acp` 在 stdio 上讲 ACP。Zed 配置：

```jsonc
// ~/.config/zed/settings.json
{
  "agent_servers": {
    "tack": {
      "type": "custom",
      "command": "C:/path/to/tack.exe",
      "args": ["acp"],
      "env": { "ANTHROPIC_API_KEY": "..." }
    }
  }
}
```

模式（ask/acceptEdits/plan/bypass）、模型和 thinking 等级可在客户端 UI 中
选择。JetBrains IDE 见 [docs/intellij-idea-acp.md](docs/intellij-idea-acp.md)。

### 远程会话与浏览器客户端

```bash
tack serve --listen ws:127.0.0.1:7749   # 托管会话（+ 内嵌网页 UI）
# 在浏览器打开 http://127.0.0.1:7749/
tack client --addr tcp:127.0.0.1:7749   # 或从另一个终端接入
```

`serve` 讲分帧 CBOR，与 `@earendil-works/pi-protocol` v1 线上兼容，支持
`tcp:` / `unix:` / `ws:`（WebSocket 载荷除长度前缀外逐字节相同），带
`--tls`（首次使用自动生成自签名证书对）和 `--auth-token`。内嵌网页客户端
是单个零依赖 HTML 文件：创建/接入/切换会话、流式接收回复、回答权限询问。

## 使用 Tack

### 交互式 TUI

- **编辑器**：多行、Ctrl+R 历史搜索、撤销、bracketed paste、`/command` +
  `@file` 自动补全（感知 gitignore）、外部编辑器（Ctrl+G）、剪贴板图片
  粘贴（Ctrl+V）。
- **聊天**：流式 markdown + 语法高亮、thinking 块、带彩色 diff 和流式
  bash 输出的工具卡片（Ctrl+O 展开）。
- **权限**：ask / acceptEdits / plan / bypass（Shift+Tab 循环），允许一次/
  总是允许询问（按工具+输入缓存）；`.pi/` 资源由项目信任门控（`/trust`）。
- **命令**：`/model /thinking /mode /compact /new /resume /tree /fork
  /clone /name /session /rules /todo /context /copy /export /share /import
  /login /logout /settings /fullscreen /reload /quit`，外加 prompt 模板
  作为 `/name` 命令。
- **渲染**：主屏 scrollback（默认）或全屏 alt-screen（`/fullscreen`），
  带鼠标滚动、拖拽复制、transcript 搜索（Ctrl+Shift+F）；内联图片
  （Kitty/iTerm2）、14 个内置主题 + 用户主题、跟随终端背景的
  light/dark 主题对。

### 其他子命令

```bash
tack rpc          # stdio 上的 JSONL RPC（与 pi --mode rpc 线上兼容）
tack mcp-serve    # 把 tack 自身作为 MCP server 暴露在 stdio 上
tack stats        # 跨会话 token/费用报告（--since 7d --json）
tack fork <session.jsonl>   # 把会话 fork 进此目录
tack compact      # 手动压缩最近的会话
tack doctor       # 环境自检（shell、LSP、沙箱、凭据、MCP）
tack logs         # 结构化 trace 查看器（--follow、--level、--target）
tack eval <dir>   # 无头运行 eval 任务并统计通过率
tack ext ...      # 安装/列出/移除/校验扩展，市场管理
```

RPC 命令面与 TS 完全对齐（34 个命令，`prompt`/`steer`/`follow_up`/`abort`/
`set_model`/`get_state`/`export_html`/……）；见
[docs/compatibility.md](docs/compatibility.md) §2.3。

### 会话

以事务性 v4 格式存储在 `~/.tack/agent/sessions/--<encoded-cwd>--/`
（与上游 TS pi 对齐；v1–v3 文件打开时透明迁移，`sessionBackend: "v3"`
保留旧写入路径）。树状历史，`/tree` 与 `/fork` 跳转时自动生成分支摘要。

## Provider 与认证

- **选择**：`tack --provider <id> [--model <id>]` —— provider id 与 TS pi
  完全一致（无别名）；省略模型时默认 provider 旗舰。注册表：anthropic、
  openai、openai-codex、azure、google、mistral、deepseek、openrouter、xai、
  groq、cerebras、together、fireworks、baseten、huggingface、nvidia、
  zai(-coding-cn)、moonshotai(-cn)、kimi-coding、minimax(-cn)、xiaomi、qwen、
  opencode(-go)、vercel-ai-gateway、cloudflare、github-copilot、ant-ling、
  radius、amazon-bedrock、google-vertex 等。
- **API key** 解析顺序：`--api-key` → `models.json` 的 `apiKey` →
  provider 环境变量（变量名与 TS pi 相同，如 `ZAI_API_KEY`、
  `MOONSHOT_API_KEY`、`KIMI_API_KEY`）。
- **OAuth**：`tack login --provider <id>` 在未给 `--api-key` 时运行
  provider 的流程 —— anthropic（Claude Pro/Max）、openai-codex（ChatGPT）、
  github-copilot、openrouter、kimi-coding、xai、radius。无头机器可粘贴
  重定向 code 或用 `--device-code`。过期 token 主动刷新并持久化回
  `auth.json`；`tack auth-status` 显示类型和过期时间。
- **本地零配置**：`ollama`（`OLLAMA_HOST`）和 `llama.cpp`
  （`LLAMA_CPP_HOST`）启动时探测；发现的模型注入目录，服务器在线时
  `tack --provider ollama "…"` 开箱即用。
- **自定义 provider**：`~/.tack/agent/models.json`，schema 与 TS pi 相同
  （`providers.<id>.{baseUrl, api, apiKey, headers, compat, models[]}`），
  支持 `$ENV_VAR` 插值和 `apiKey` 中的 `!command` 执行。
- **Bedrock / Vertex**：凭据链、SigV4、ADC 细节见
  [docs/providers.md](docs/providers.md)。

## MCP servers

内置 MCP 客户端（基于 `rmcp` SDK）—— 不同于 TS pi 刻意把 MCP 留给扩展：

```jsonc
// ~/.tack/agent/mcp.json（全局）或 <project>/.pi/mcp.json（项目优先）
{
  "mcpServers": {
    "everything": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-everything"] },
    "remote":     { "type": "http", "url": "http://localhost:3000/mcp",
                    "headers": { "authorization": "Bearer …" } }
  }
}
```

stdio / Streamable HTTP / 旧版 SSE 传输；工具以 `mcp__<server>__<tool>` 走
正常权限管线；资源和提示可经 `/mcp` 浏览；远程服务器支持 OAuth 2.1
（`"oauth": true`）；惰性工具 schema（`mcpDeferThreshold`）、server 反向
sampling（`mcpSampling`，默认关）和 elicitation（`mcpElicitation`）均有
加固默认值。细节：[docs/features.md](docs/features.md) §生态互操作 与
[docs/configuration.md](docs/configuration.md)。

## 扩展

动态扩展以**子进程插件**运行（`tack-ext`）：任何在 stdio 上讲换行分隔 JSON
的可执行文件，每个插件一个崩溃隔离进程 —— 或作为沙箱化 WASI 模块
（`tack-ext-wasm`）。

```bash
# 从 git URL / 本地目录 / 市场规格安装
tack ext install https://github.com/example/plugin.git
tack ext list
```

……或把带 `extension.json` 清单的目录放进
`~/.tack/agent/extensions/<name>/`：

```json
{ "name": "hello-js", "command": "node", "args": ["plugin.js"] }
```

插件可注册工具（`ext__<plugin>__<tool>`）、斜杠命令、事件处理器、UI
对话框/通知、会话控制和运行时 provider。[`examples/extensions/`](examples/extensions/)
下有现成示例（Node.js hello-world、protected-paths 防护、git checkpoint、
会话 handoff）。**完整指南：[docs/extensions.md](docs/extensions.md)**
（协议、API 参考、安全模型）与 [docs/extensions-v2.md](docs/extensions-v2.md)
（WASM 载体）。

## 配置

四层 —— managed（组织强制）→ global（`~/.tack/agent/settings.json`）→
project（`<project>/.pi/settings.json`，受信任门控）—— 深度合并，
`features.*`、`sandbox`、`permissions.*` 有特殊规则。

```jsonc
// ~/.tack/agent/settings.json
{
  "defaultProvider": "anthropic",
  "defaultModel": "claude-sonnet-4-5",
  "theme": "catppuccin-latte/catppuccin-mocha",
  "tokenBudget": 1000000,
  "features": { "sandbox": true, "cron": false }
}
```

`features.*` 开关让被禁用的功能**对 agent 不可见**（无工具 schema、无系统
提示片段、子系统不启动）。每个 settings 键、环境变量、CLI 标志及优先级
规则：**[docs/configuration.md](docs/configuration.md)** —— 资源加载顺序
（rules/skills/MCP/主题）：**[docs/directories.md](docs/directories.md)**。

## 文档

面向贡献者的入口文档为英文；深度设计文档大多为中文（语言政策见
[CONTRIBUTING.md](CONTRIBUTING.md)）。

| 文档 | 内容 |
|---|---|
| **[docs/features.md](docs/features.md)** | Tack 超出 TS pi 的全部功能 —— 每个功能怎么用、怎么关 |
| **[docs/configuration.md](docs/configuration.md)** | 配置参考：每个 settings 键、环境变量、CLI 标志、优先级 |
| **[docs/onboarding.md](docs/onboarding.md)** | 新开发者上手：源码阅读路线、功能→代码速查表、mermaid 图 |
| [docs/architecture.md](docs/architecture.md) | 内部架构（crate 分层、流式模型、hooks、渲染器） |
| [docs/providers.md](docs/providers.md) | Provider 认证深入（Bedrock SigV4、Vertex ADC）、自定义 provider schema |
| [docs/directories.md](docs/directories.md) | 目录与资源加载顺序 |
| [docs/compatibility.md](docs/compatibility.md) | 各类接口的兼容与版本政策 |
| [docs/extensions.md](docs/extensions.md) / [docs/extensions-v2.md](docs/extensions-v2.md) | 扩展开发（进程协议；WASM 载体） |
| [docs/plugin-system.md](docs/plugin-system.md) | 插件系统总览（hooks / tack-ext / WASM / bundle / 市场） |
| [docs/hooks.md](docs/hooks.md) | 生命周期 hooks（兼容 Claude Code） |
| [docs/codebuddy.md](docs/codebuddy.md) | CodeBuddy 指南（安装/登录、`/model`、排障） |
| [docs/intellij-idea-acp.md](docs/intellij-idea-acp.md) | JetBrains IDE 的 ACP 配置 |
| [docs/telemetry.md](docs/telemetry.md) | 为什么没有遥测，以及替代方案 |
| [docs/upstream-alignment.md](docs/upstream-alignment.md) | TS pi → Tack 同步跟踪（每周自动 delta 报告） |
| [docs/release.md](docs/release.md) | 发布流程（tag、跨平台构建、自更新） |

## 开发

```bash
cargo build --release -p tack-app   # 构建 CLI
cargo test --workspace            # 全量测试，无需网络
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

### 仓库布局

| Crate | 职责 |
|---|---|
| `tack-ai` | 统一消息模型 + provider 适配器（Anthropic、OpenAI、Azure、Codex、Google、Mistral、Bedrock……） |
| `tack-agent-core` | Agent 循环、`AgentTool`/`AgentHooks`/`Extension` trait、`AgentEvent` 流 |
| `tack-session` | 会话持久化（树结构）+ 上下文压缩 |
| `tack-tools` | 内置工具：read、bash、git、edit、write、grep、find、ls、web_fetch、web_search、todo |
| `tack-tui` | 终端 UI 库：样式行、组件、diff + alt-screen 渲染器 |
| `tack-protocol` | CBOR 远程会话协议：schema、分帧、`RemoteClient` |
| `tack-ext` / `tack-ext-wasm` | 扩展宿主：子进程 NDJSON 协议；沙箱化 WASM 载体 |
| `tack-app` | 二进制：TUI / print / ACP / RPC / serve / mcp-serve，skills、settings、系统提示 |

依赖方向无环：`tack-ai ← tack-agent-core ← tack-tools`、`tack-ai ← tack-session`、
`tack-app → all`。

### 设计不变量

- **流式**：`EventStream<T, R>`（tokio mpsc + oneshot 结果）；provider 错误
  永远**带内**返回（`Error` 事件 + `stop_reason: Error`），绝不抛出。
- **工具**：`serde_json::Value` 参数边界 + `schemars` schema；校验失败变为
  带内错误工具结果；文件修改在共享锁上串行化。
- **Hooks**：单一 `AgentHooks` trait 对象（transform_context /
  before_tool_call / after_tool_call / steering / follow-ups）。
- **取消**：每个 prompt 轮一个 `CancellationToken`；bash 杀掉整个进程树。
- **Windows**：bash 像 pi 的 `utils/shell.ts` 一样解析 Git Bash；可选的
  `powershell` 工具在列入 `defaultTools` 时注册。

新来乍到？从 [docs/onboarding.md](docs/onboarding.md)（源码导览）开始，
然后 [docs/architecture.md](docs/architecture.md)，提 PR 前阅读
[CONTRIBUTING.md](CONTRIBUTING.md)（conventional commits、CHANGELOG 条目、
CI 门禁）。

### 相对 TS pi 的已知差距

- 扩展提供的 TUI widget/对话框（插件已覆盖工具、命令、对话框、事件、会话
  控制、provider 注册 —— 自定义渲染组件需要声明式 v2.1/v2.2 协议，仍开放）、
  彩蛋、npm 包管理器。
- TS 模块扩展只能经 tack-ext 协议运行（无即插即用的 TS 加载器）。
- 决策性推迟：实时多客户端协作；Windows 沙箱只有资源约束（没有
  AppContainer 就没有文件系统隔离）。

## 更新日志、贡献、许可证

- 版本与变更：[CHANGELOG.md](CHANGELOG.md)
- 贡献：[CONTRIBUTING.md](CONTRIBUTING.md) · 安全：[SECURITY.md](SECURITY.md)
- 基于 [Apache-2.0](LICENSE) 许可。Tack 是 [pi](https://github.com/earendil-works/pi)
  编码代理的 Rust 重实现；pi 本身基于 [MIT](https://github.com/earendil-works/pi/blob/main/LICENSE)
  许可 © 2025 Mario Zechner。
