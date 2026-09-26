# CodeBuddy provider 踩坑记录

`codebuddy` CLI 的 stream-json 协议在**工具调用参数的流式重放**上是有损的。
本文记录 Tack codebuddy provider 实际踩过的坑、定位方法和修复方案，供后续
维护者参考。修复实现见 `crates/tack-ai/src/codebuddy.rs`（0d482e0）。

## 症状（一次真实会话，deepseek-v4-pro）

- 模型发起**并行工具调用** `bash_output` + `bash`，Tack 侧两个调用的参数
  都变成 `{}`：`bash_output` 列出任务列表而不是读 bg1 的输出，`bash` 直接
  报 `invalid arguments for bash: "command" is a required property`；
- 界面里两段 thinking 文字**词级交错**，一段思维的尾巴拼进了另一段中间；
- 模型以纯文本输出 `<tool_info><available_skills>…</tool_result>`（此为
  模型自身行为，非 provider bug，见下文「不坑但像坑」）。

## 定位方法（方法论上踩的坑）

1. **先看双方的持久化记录，再猜协议**。
   - CLI 侧：`~/.codebuddy/projects/<cwd-hash>/<session>.jsonl`。`function_call`
     记录里是模型生成的**完整参数**；`function_call_result` 里是 Tack 工具
     实际执行的结果。两边一对比，"模型给对了、Tack 执行错了"立刻实锤。
   - Tack 侧：`~/.tack/agent/sessions/--<cwd>--/*.jsonl`，记录的是 provider
     交给 agent loop 的最终 `AssistantMessage`（参数已丢）。
2. **CODEBUDDY_PATH tee 抓包**：`CODEBUDDY_PATH` 指到一个透传脚本（Python
   双向 tee stdin/stdout 到文件），就能拿到 CLI 的真实 wire。这一步直接
   推翻了 mock 里的错误假设（见下）。
3. **不要相信 mock 即真实**。原 mock 假设并行调用的 `tools/call` 帧是"发一
   个、等响应、再发下一个"串行派发的；真实 CLI（2.156.0）在 `message_stop`
   之后**一次性把整批帧全部发出**。按错误假设写的 mock 会让"边界处等齐所有
   帧"的逻辑白等 5 秒。
4. **HEAD 可能本来就过不了 CI 的 `cargo fmt --all -- --check`**。改完务必
   跑 fmt；顺手把预存违规单独成一个 style commit，别和功能修复混在一起。
5. 本仓库并行会话多：提交前先核对 `git log`，别人的 commit 可能已经落在
   你开始的 HEAD 之上。

## 协议层的坑（stream_event 重放）

以下均为真实 wire（CLI 2.156.0 + deepseek-v4-pro）观察到的行为：

1. **并行 tool_use 共用同一 content index**，且只发一个
   `content_block_stop`。更糟的是 delta 顺序不保证"第一个调用的 JSON 发完再
   start 第二个"：实际见过两个块都先 start、然后两份 JSON 的 delta 全部打在
   同一个 index 上——按"最后 start 的块"路由，第一个调用收到空、第二个收到
   两份 JSON 的拼接（解析必炸）。
2. **有些调用的参数根本不走 delta**：`content_block_start.content_block.input`
   直接带全量 input，之后一个 `input_json_delta` 都没有。TS 参考实现
   （pi-codebuddy-sdk）在 start 时就用 `input` 做种子、解析失败回退种子
   （`parsePartialJson(partial, block.arguments)`）；Tack 原来硬编码 `{}`。
3. **turn 内 API 级重试**：CLI 重试时会重发 `message_start` 并**从头复用
   content index**。第一次尝试里没收到的 `content_block_stop` 的块就此悬挂，
   后续同 index 的 delta 被路由到新块（思考流串字的来源）。修复：第二个
   `message_start` 到达时把未闭合的 text/thinking 块就地收尾（补 End 事件）。
4. **`message_delta` 可能来多个且 stop_reason 互相矛盾**（实测一个
   `tool_use` 后紧跟一个 `end_turn`）。工具边界要以 `message_stop` 时的
   状态为准，别被中间的 `end_turn` 带偏。
5. **thinking 块可以永远没有 `content_block_stop`**（工具块开始了它还没
   闭合），delta 还会和工具块的 delta 交错到达。
6. **完整参数的唯一权威来源是 MCP `tools/call` 帧**（CLI 从完整的
   assistant 消息派发，在 `message_stop` 后立即全部到达）。修复方案：边界处
   有界等待收齐帧，按名配对后改写参数；配对关系记录 `mcp_request_id`，
   `resolve_parked` 按 request_id 应答——因为 `map_tool_args` 的参数归一化
   （如 bash 自动补 `timeout:120`）会让 (name, arguments) 精确匹配失效，
   同名并行调用的乱序结果会因此错配。

## 不坑但像坑

- **模型以文本吐 `<tool_info>`/`<tool_result>` 标记**：这是模型在模仿
  CodeBuddy 训练数据里的内部标记（Tack 与 TS 插件代码库里都没有这个标记），
  属于模型行为，provider 层无解。
- **CLI 会话 JSONL 里 `[tool:bash]` / `[tool_result:call_…]` 是压缩展示
  形式**，真实内容在同目录的 function_call 记录里，别被占位文本误导。
