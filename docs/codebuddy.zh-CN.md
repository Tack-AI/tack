# CodeBuddy 使用指南

**[English](codebuddy.md) | 简体中文**

把 `codebuddy` CLI 作为 Tack 的一等 provider 使用：TUI、工具、技能、扩展都
留在 Tack 这边，CodeBuddy 只负责模型推理（由你本机已安装的 `codebuddy` CLI
执行）。无需 Node SDK、无需本地 HTTP 翻译层、无需额外凭据配置。

> 实现细节（工具桥、会话同步等）见 [features.md](features.zh-CN.md) 的
> 「CodeBuddy 原生 provider」一节；本文只讲怎么用。

## 前置条件

1. **安装 codebuddy CLI** 并确认在 PATH 上：

   ```bash
   which codebuddy        # 或 Windows: where codebuddy
   ```

   不在 PATH 上时，用 `CODEBUDDY_PATH` 指向二进制（也接受 `.cmd`/`.bat`/
   `.py` 脚本）：

   ```bash
   export CODEBUDDY_PATH=/path/to/codebuddy
   ```

2. **登录态可用**——Tack 不保管 CodeBuddy 凭据，完全复用 CLI 自己的
   登录态。以下任一方式让 `codebuddy` 在终端里可用即可：

   ```bash
   codebuddy login                              # 浏览器登录（推荐）
   export CODEBUDDY_API_KEY="your-api-key"      # 或 API key
   export CODEBUDDY_INTERNET_ENVIRONMENT=ioa    # 腾讯 iOA 内网 + codebuddy login
   ```

   判断标准很简单：**`codebuddy` 在你的终端能跑，Tack 里就能用。**

## 快速上手

```text
tack
/model            # 选择 codebuddy/ 前缀的模型
```

首次启动时 Tack 会启动一次 CLI 握手来发现模型列表（结果缓存到
`~/.tack/agent/codebuddy-models.json`，之后启动直接读缓存、后台再刷新）；
发现失败且无缓存时回退为单个 `codebuddy/default` 透传模型，`/model` 里
仍然能选。

想跳过每次 `/model`，在 `~/.tack/agent/settings.json` 里设默认：

```json
{
  "defaultProvider": "codebuddy",
  "defaultModel": "hy3-preview-agent-ioa"
}
```

工具、技能、扩展、`/compact`、steer 等行为与其他 provider 完全一致——
工具调用仍由 Tack 的权限/UI 执行，CodeBuddy 侧只做规划与调用。

## 思考级别（thinking）

Tack 的思考级别会映射为 CodeBuddy 的 `--effort`：

| Tack 级别 | CodeBuddy effort |
|-----------|------------------|
| off | 不传（CLI 默认） |
| minimal / low | low |
| medium | medium |
| high | high |
| xhigh / max | xhigh |

在 TUI 里用 `--thinking` 启动参数或运行中切换思考级别均可。

**注意**：`--effort` 是 CLI 进程启动参数，运行中切换思考级别会重启该会话的
CLI 进程（历史经转写重放保留，但 CLI 侧缓存重置）。切换模型则不同——通过
`set_model` 控制请求热切换，会话与 CLI 缓存都保留。

## 会话隔离（与独立 codebuddy 的区别）

Tack 启动的 codebuddy 会话与你的个人 CodeBuddy 配置**隔离**（对齐 TS 参考
插件 pi-codebuddy-sdk 的默认行为）：

- `--setting-sources none`：不加载用户/项目/本地设置（AGENTS.md 等已经由
  Tack 自己的系统提示词携带）；
- `--strict-mcp-config`：你在 codebuddy 配置里登记的 MCP 服务器**不会**进入
  会话——模型只能调用 Tack 桥接过去的工具（Tack 的工具以 SDK MCP server
  形式经 `sdkMcpServers` 声明，走 `mcp_message` 控制帧，与用户 MCP 配置无关）；
- CLI 内建工具（Read/Write/Bash 等）整体禁用，避免绕过 Tack 的权限与 UI；
- Tack 的系统提示词**替换** CodeBuddy 默认身份（`--system-prompt`），模型
  表现为 Tack 而不是独立的 CodeBuddy Code；
- 禁用了 CLI 的自动更新（防会话中途重启断连）、自动记忆、自动压缩（上下文
  管理由 Tack 负责）与后台任务。

模型元数据（上下文窗口、最大输出、是否支持思考/图片）初始按模型 id 估计
（CLI 的模型列表不携带能力信息）：gemini 1M ctx，claude/gpt 200K，其余
128K；gpt 16K max output，其余 8K。**估计不准时会自动修正**：CLI 的 result
事件携带真实服役参数（`modelUsage.*.contextWindow/maxOutputTokens`），
Tack 学习后写入注册表并持久化到 `~/.tack/agent/codebuddy-models.json`
（当前会话需重新 `/model` 选择或重启后生效）。也可以手动覆盖——在
`models.json` 里写同名 provider 条目（provider 级字段 api/baseUrl 仍由
内建 provider 接管，仅模型元数据生效）：

```json
{
  "providers": {
    "codebuddy": {
      "models": [
        { "id": "hy3-preview-agent-ioa", "contextWindow": 1048576, "maxTokens": 32768 }
      ]
    }
  }
}
```

模型列表本身也会缓存到该文件：启动时先同步注册缓存（`/model` 立即可见），
再后台握手发现刷新；发现失败时保留缓存，仅有 CLI 无缓存时回退
`codebuddy/default` 透传模型。

## 已知行为

- **压缩/改史/切换思考级别**：重写 CodeBuddy 原生会话文件（JSONL）后以
  `--resume` 重启 CLI——多轮结构与 CLI 侧缓存保留（对齐参考插件的会话
  重建机制；写文件/校验失败时回退扁平转写重放）。**切换模型不重启**：
  走 `set_model` 控制请求热切换（失败时回退上述重建）。
- **并行工具调用的参数兜底**：CLI 的 stream_event 重放对并行 tool_use
  是有损的（多个块共用同一 content index，input_json delta 可能交错或
  整个不流式下发）。工具边界处 Tack 会以 CLI 随后派发的 MCP
  `tools/call` 帧为准改写参数（帧来自 CLI 的完整 assistant 消息），并按
  帧配对应答（不受参数归一化或同名并行乱序影响）；块起始时也会用
  `content_block.input` 做种子参数（对齐参考插件）。
- **turn 内 API 重试**：CLI 重试会重发 `message_start` 并复用 content
  index；Tack 会把上一次尝试遗留的未闭合 text/thinking 块就地收尾
  （补发 End 事件），避免块悬挂与 delta 错路由（思考流串字）。
- **abort（Esc）**：生成阶段打断（无工具调用在途）不重建会话——中断 turn
  的残余消息排干后续跑；工具执行中打断会换新 session id 重建（被 kill 的
  旧进程可能还在写旧会话文件，避免孤儿写入撞车）。
- **usage/费用**：取自 CLI 的 result 事件；订阅制下成本为 0，token 计数正常。
- **rate limit**：限流事件在 TUI 出内联警告 + 桌面通知（`notifications`
  设置控制），headless 模式记日志。
- **会话清理**：`/new`、RPC/print 会话结束时会关闭对应的 codebuddy CLI
  进程；空闲超过 2 小时的会话也会被自动回收。

## 故障排查

打开 provider 调试日志：

```bash
RUST_LOG=tack_ai::codebuddy=debug tack
```

协议层的坑（并行工具调用丢参数、turn 内重试、抓包方法等）见
[codebuddy-pitfalls.md](codebuddy-pitfalls.zh-CN.md)。

常见问题：

- **`/model` 里没有 codebuddy 条目**：`which codebuddy` 是否成功？未安装时
  provider 整体不注册。设置 `CODEBUDDY_PATH` 后重启。
- **只有 `codebuddy/default`**：模型发现握手失败（CLI 过老或网络问题）。
  升级 codebuddy CLI 后重启 Tack；default 模型可直接用（CLI 侧自选模型）。
- **工具调用报权限/超时**：Tack 侧的行为，与其他 provider 相同，与
  CodeBuddy 无关；检查 Tack 的权限设置。
- **Windows**：npm 全局安装的 `codebuddy.cmd` shim 经 `cmd /c` 启动，会话
  回收时整树清理；超长系统提示词自动改走 stdin 注入（不受 Windows 命令行
  长度限制影响）。

## ask_codebuddy 工具（委托子任务）

安装了 codebuddy CLI 时，Tack 自动注册内建工具 `ask_codebuddy`：把一
个聚焦的子任务委托给一次独立的 CodeBuddy 调用（第二意见、代码评审、
架构问题、调试假设），full 模式下也可以自主执行。

- **干净会话**：被委托的调用只看到 prompt，看不到当前对话——prompt 必
  须自包含（问题、相关文件路径、关注点写清楚）。
- **mode**：`read`（默认，只读探索，禁止 Write/Edit/Bash 等）、`full`
  （允许写入与 bash，无 Tack 回执，慎用）、`none`（纯通用知识，无文件
  访问）。这里用的是 CodeBuddy 自己的内建工具，与 Tack 的工具/权限无
  关。
- **model / thinking**：可选覆盖模型与思考级别（映射 `--effort`）。
- 执行有 10 分钟上限；Esc 会中断委托调用；返回正文 + 工具使用摘要。

## 与 TS 插件 pi-codebuddy-sdk 的差异

Tack 的对齐目标是**线上行为一致**（启动参数、流式、effort、参数容错），
架构差异导致以下插件能力形态不同：

- **`codebuddy-sdk.json` 配置文件**：Tack 不读它（覆盖项用 models.json
  的同名 codebuddy 条目代替，见「模型元数据」一节）；
- **AskCodebuddy 委托工具**：以 Tack 内建工具 `ask_codebuddy` 提供（见
  上节），暂不支持共享当前对话历史（干净会话）；
- **跨进程 resume**：Tack 会话与 codebuddy 会话 id 的映射持久化在
  `~/.tack/agent/codebuddy-sessions.json`——重启 Tack 后首个分叉重建
  会复用上次的 codebuddy 会话文件（新进程直接走原生重建，不会先白起
  一个新 CLI）。
