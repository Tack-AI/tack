# CodeBuddy provider 踩坑记录

**[English](codebuddy-pitfalls.md) | 简体中文**

`codebuddy` CLI 是一个 Claude-Code fork，通过有状态的 stream-json 协议驱
动，而 Tack 让**同一个** CLI 进程长驻整个会话——所以 provider 要扛起两
个难题：**工具调用参数的流式重放是有损的**，以及**跨压缩/中断/respawn
维持三个状态机一致**（tack 上下文 ↔ 存活的 CLI 进程 ↔ CLI 的 session
JSONL）。本文记录 Tack codebuddy provider 实际踩过的坑、定位方法和修复
方案，供后续维护者参考。修复实现见 `crates/tack-ai/src/codebuddy.rs` 与
`codebuddy_jsonl.rs`（0d482e0 及之后）。

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
7. **并行 tool_use 块可以整块蒸发**（CLI 2.156.0 + hy4-preview-f）：两个
   并行 `bash` 调用在流里只出现一个——第二个块在任何 `stream_event` 里
   都没出现过。但 CLI 的完整 assistant 消息里有两个，所以两个 `tools/call`
   帧都会派发，且 CLI 要等整批结果；没有 parked 对应物的帧一直无人应答，
   CLI 在 turn 中途静默，5 分钟后被 F18 idle watchdog 杀掉（报错
   `codebuddy CLI idle: no events for 5 minutes`）。修复：边界处在配对前
   先把整批帧排空（250ms 无活动宽限，每来一行重置——否则"等到 pending ≥
   parked"的循环会在同批靠后的帧到达前退出），然后把每个未配对的帧**物化**
   为新的 parked 调用：帧里带完整的 name + arguments，只有模型的 tool_use id
   无法恢复（用 `adopted-<request_id>` 合成）。定位备注：这次又是双方持久化
   记录一锤定音——CLI history 里有两个 `function_call`，Tack 会话里只有
   一个 toolCall，而第二条 `function_call_result` 的文本原文就是
   `turn aborted (CLI idle)`，正是 watchdog 自己的清理消息。

## 会话重建的坑（压缩后"卡住"，2026-09-30）

用户报告"Compacted context (115255 tokens)——然后会话好像卡住了"。实际
没有任何东西卡死：turn 已经**结束**了。下列缺陷全部住在 JSONL 会话重
建路径里（tack 上下文与 CLI 会话分叉时触发——压缩、历史编辑），靠双
方持久化记录 + CLI 自己的 trace 逐层定位：

1. **respawn 把工具列表清空了。** `spawn_new` 以 `tools: []` 起步，而
   `respawn_native`/`respawn_transcript` 用这个新会话整体替换 `*self`
   ——于是 respawn 后的整个 turn 里，CLI 的 `tools/list` MCP 请求拿到的
   是空列表。模型在没有工具时的表现覆盖整个频谱：拒绝（"the bash tool
   isn't available to me right now—only my internal reasoning tool
   is"）、幻觉出的限制（"bash 只能跑 `date +%F`"）、乃至 reasoning 死
   循环（4 分钟内 95+ 次短 reasoning 响应，每次都是一次 API 往返，永远
   不发出调用）。修复：respawn 时把 `self.tools` 带进新会话。
2. **有损文本标记投影教会模型提前收工。** 参照实现的投影把 tool
   call/result 拍平成 `[tool:name]` / `[tool_result:id]` 文本标记，重建
   历史里真实的工具结构为零。kimi-k2.8 于是用字面文本
   `"[thinking]\n继续深入。读 README…"` 回答压缩后的提示并以
   `finish_reason=stop` 收工——它在模仿标记而不是调用工具。用户视角：
   代理说了要继续干活，然后就没声了——与挂起无法区分。修复：settled 的
   工具轮次投影为 CLI **原生**的 `reasoning` / `function_call` /
   `function_call_result` 记录（格式逐字段对齐真实 CLI 会话文件，包括
   `messageId`/`conversationRequestId` 链接字段和调用上的
   `providerData.reasoning`）。CLI 会把它们重放成标准的 API
   `tool_calls`/`tool` 消息——已用真实 CLI resume 我们写的文件验证：
   立即发起工具调用并答对。只有**悬空调用**（重建切片里没有配对结果）
   和**孤儿结果**仍降级为文本标记：原生调用没有结果会让 resume 的 CLI
   永远干等。
3. **最后一条 assistant 消息之后的 settled tool result 会强制走
   transcript 兜底。** 旧的切分逻辑拒绝任何含 tool result 的尾部（新
   CLI 没有 parked 调用可以投递）。有了原生结果记录后，这类尾部随重建
   前缀走；兜底现在只在上下文结束于工具轮次中途时触发。
4. **transcript 兜底的粘贴教会模型逐字复读。** 当 native 重建没有可
   投递的 user 尾部时（任务中途的自动压缩——上下文以 tool result 结
   尾），兜底把整个上下文拍平成一条 user 消息，而且过去连 thinking
   块也逐字粘贴。模型自己的推理以纯文本形式重新出现在上下文里，是
   最強的 echo 源：一段 810 字符的 reasoning + 115 字符的文本被逐字
   节复读了三遍（每遍都重跑了同样的文件读取），而且每次复读都落入
   下一次压缩的 retained tail，自我强化。修复：thinking 不再粘贴
   （状态由摘要、text 和工具标记承载），replay 标记明确声明历史已经
   终结（"不要重复……不要重新回答……不要重跑"），且 overflow 压缩的
   retry 现在也携带 goal recitation——该路径绕过
   `transform_context`，没有 recitation 就没有可投递的尾部 user 消
   息，每次 overflow 压缩都会落入有损粘贴。
5. **空前缀重建把 provider task 搞 panic 了**（`index out of bounds:
   the len is 0 but the index is 0`，v1.0.7 修复）。当压缩把所有
   assistant 回复都折进摘要后，native 重建连一条 settled 前缀都没
   有——它把 CLI 的 session 文件重写成**空文件**，而写入后的完整性
   检查在零行文件上索引了 `lines[0]`，直接 panic 掉 provider task，
   用户侧表现为 `event stream ended without a final result`。现在空
   前缀重建回退 transcript replay（即 `no settled prefix to
   rebuild` 这条 WARN 的来源），且校验器把"零条预期记录的空文件"
   视为正常。

验证工具：`cargo run -p tack-ai --example cb_rebuild_repro -- <model>`
会用真实 CLI 跑一遍压缩形状的重建并断言模型仍会调用工具（含全新会话的
对照组）。注意运行需要可写的 `CODEBUDDY_CONFIG_DIR`——沙箱 shell 会让
CLI 静默卡死（它无法写日志/会话文件，initialize 握手之后零日志输出；
把 `~/.codebuddy` 拷到可写位置（含登录态）再把环境变量指过去）。

## 进程生命周期的坑（退出后 CLI 成为孤儿进程，2026-09-30）

有用户报告："Ctrl+C 退出 TUI 后，还有一个 `tack` 进程没退出"。那其实
**不是** tack 进程：被泄漏的 CLI 的 argv 里带着 `--allowedTools
mcp__tack`，所以 `ps aux | grep tack` 会匹配到它 —— 下结论前先用
`ps -p <pid> -o command=` 确认二进制到底是谁。

**CLI 子进程在所有退出路径上都会幸存**，三个叠加的事实：

1. 会话注册表是进程级 `static SESSIONS`
   （`OnceLock<Mutex<HashMap<…>>>`）—— Rust 永远不会 drop static；
2. TUI/print/compact 路径通过 `std::process::exit` 退出 —— 完全不运行
   任何析构函数；
3. 于是两道保险在退出时都成了死代码：`CodeBuddySession::drop`
   （`kill_tree` + `start_kill`）和 tokio 的 `kill_on_drop(true)`（只在
   `Child` 句柄真正被 drop 时才触发）。唯一的收割者是 2 小时空闲驱逐，
   而它只会在下一次 `get_or_spawn` 时运行 —— 在同一个已经死掉的进程里。

CLI 最终确实会自己死掉（管道关闭后 stdin EOF），但"最终"不是保证。
`close_all_sessions()` 早就存在（doc 注释写着 "process shutdown"）却从
未被接线；修复把每个 agent 循环模式的退出都接上了它：TUI、print、
compact 在 `std::process::exit` 前调用，rpc / acp / mcp-serve / eval /
serve 用 `close_codebuddy_after(...)` 包装（测试
`close_all_sessions_reaps_registered_cli` 覆盖）。剩余边界：被信号打死
的 `tack serve`（SIGTERM/SIGINT 默认终止）仍然无法清理 —— 那需要信号
处理器，而不是析构函数。诊断备注：TUI 双击 Ctrl+C 退出有 quit-grace
窗口，会先取消进行中的 turn，所以关机清扫不会卡在 mid-turn 持有的
session 锁上。

## 不坑但像坑

- **模型以文本吐 `<tool_info>`/`<tool_result>` 标记**：这是模型在模仿
  CodeBuddy 训练数据里的内部标记（Tack 与 TS 插件代码库里都没有这个标记），
  属于模型行为，provider 层无解。
- **CLI 会话 JSONL 里 `[tool:bash]` / `[tool_result:call_…]` 是压缩展示
  形式**，真实内容在同目录的 function_call 记录里，别被占位文本误导。
- **transcript 兜底的两条 WARN 是设计内路径，不是故障**：
  `JSONL rebuild failed (no settled prefix to rebuild; transcript
  fallback)` / `no user tail to deliver; transcript fallback` 表示
  native 重建没有可安全 resume 的材料，provider 改为重放拍平的
  transcript。对话会带着完整上下文继续——损失的只有原生工具结构和
  CLI 侧缓存。偶发（激进压缩折掉全部 assistant 轮次、CLI 首次回答前
  挂掉）属正常；只有当**每次**压缩都走兜底、且 retained tail 里明明
  有 assistant 轮次时才值得查。
- **本机已安装且已登录的 codebuddy CLI 会让
  `model_switch_rebinds_provider_adapter` 这个 TUI 测试失败**
  （tack-app）：测试假设环境里**没有** CLI；真 CLI 存在时
  `resolve_model` 会发现它，基于 bare-model 兜底写出的断言就挂了。
  属于环境问题，不是代码 bug。
