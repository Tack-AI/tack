# Tack 架构设计

**[English](architecture.md) | 简体中文**

Tack 是 TypeScript pi 编码代理核心的 Rust 重实现：无头（headless）为核心，附带
交互式 TUI、ACP（Agent Client Protocol）编辑器集成、RPC 与远程会话服务。

## 1. 整体架构

crate 分层，依赖方向无环：`tack-ai ← tack-agent-core ← tack-tools`，`tack-ai ← tack-session`，
`tack-app → 所有 crate`。

```mermaid
flowchart TB
    subgraph App["tack-app（二进制）"]
        CLI[CLI / clap 参数解析]
        TUI[交互式 TUI<br/>tui/]
        PRINT[print 模式<br/>一次性无头执行]
        ACP[acp 子命令<br/>ACP server over stdio]
        RPC[rpc 子命令<br/>JSONL 协议 over stdio]
        SERVE[serve 子命令<br/>CBOR 远程会话]
        EXTHOST[extension_host<br/>插件发现与生命周期]
        MISC[settings / skills /<br/>system_prompt / auth]
    end

    subgraph Core["核心层"]
        AGENT[tack-agent-core<br/>agent_loop / AgentEvent /<br/>AgentHooks / Extension / AgentTool]
        SESSION[tack-session<br/>JSONL v3 会话持久化（树结构）<br/>compaction 上下文压缩]
        TOOLS[tack-tools<br/>read / bash / edit / write /<br/>grep / find / ls / MCP]
    end

    subgraph AI["tack-ai（模型层）"]
        MSG[统一消息模型<br/>types / transform]
        ADAPTERS[API 适配器<br/>Anthropic / OpenAI Completions /<br/>OpenAI Responses / Azure / Codex /<br/>Google / Vertex / Mistral / Bedrock / tack-messages]
        OAUTH[oauth<br/>PKCE / device flow<br/>AuthResolver 主动刷新]
        REG[providers 注册表<br/>40 providers / 1130 模型目录]
    end

    subgraph Infra["基础设施层"]
        PROTO[tack-protocol<br/>CBOR schemas / framing<br/>RemoteClient]
        TUILIB[tack-tui<br/>终端 UI 库<br/>组件 / markdown / 语法高亮<br/>主屏 + alt-screen 渲染器]
        EXT[tack-ext<br/>子进程插件协议<br/>NDJSON over stdio]
    end

    CLI --> TUI & PRINT & ACP & RPC & SERVE
    TUI --> TUILIB
    PRINT & ACP & RPC & SERVE --> AGENT
    TUI --> AGENT
    EXTHOST --> EXT
    AGENT --> MSG
    TOOLS --> AGENT
    SESSION --> AGENT
    AGENT --> ADAPTERS
    ADAPTERS --> MSG
    ADAPTERS --> OAUTH
    REG --> ADAPTERS
    SERVE --> PROTO
    EXT -.NDJSON.-> EXTHOST

    style App fill:#e8f0fe
    style Core fill:#e6f4ea
    style AI fill:#fef7e0
    style Infra fill:#fce8e6
```

关键设计决策：

- **流式**：`EventStream<T, R>`（tokio mpsc 事件队列 + oneshot 最终结果），
  provider 错误一律**带内**传递（`Error` 事件 + `stop_reason: Error`），不抛出。
- **工具**：参数边界为 `serde_json::Value` + `schemars` schema；校验失败转为带内
  错误 tool result。`execution_mode()` 控制并行/串行批次；文件修改在共享锁上串行。
- **Hooks**：单一 `AgentHooks` trait 对象（transform_context / before_tool_call /
  after_tool_call / steering / follow-up / next-turn）。
- **取消**：每个 prompt 回合一个 `tokio_util::CancellationToken`；bash 杀整棵进程树。
- **Windows**：bash 解析 Git Bash（拒绝旧版 WSL bash），与 TS pi 一致。

## 2. Agent 循环（tack-agent-core）

`run_loop` 直接移植 TS `packages/agent/src/agent-loop.ts`：外层 follow-up 循环 +
内层 tool-call/steering 循环，事件顺序与错误语义保持一致。

```mermaid
flowchart TD
    A([agent_loop 入口]) --> B[emit AgentStart / TurnStart<br/>prompt 消息入 context]
    B --> C[hooks.steering_messages<br/>取排队中的用户插话]
    C --> D{内层循环<br/>有 tool call 或<br/>pending 消息?}
    D -- 是 --> E[注入 pending 消息<br/>emit MessageStart/End]
    E --> F[stream_assistant_response<br/>hooks.transform_context<br/>convert_to_llm → LLM 流式调用]
    F --> G{stop_reason?}
    G -- Error/Aborted --> H[emit TurnEnd<br/>返回]
    G -- 正常 --> I{有 tool_calls?}
    I -- 有 --> J{stop_reason == Length?<br/>参数可能被截断}
    J -- 是 --> K[全部 tool call 判失败<br/>不执行]
    J -- 否 --> L[execute_tool_calls<br/>before/after hooks<br/>按 execution_mode 并行或串行]
    K --> M[tool results 入 context]
    L --> M
    I -- 无 --> N[emit TurnEnd]
    M --> N
    N --> O[hooks.prepare_next_turn<br/>可更新 model / thinking_level]
    O --> P{hooks.should_stop_after_turn?}
    P -- 是 --> Q([返回 new_messages])
    P -- 否 --> C
    D -- 否 --> R{hooks.follow_up_messages<br/>有后续消息?}
    R -- 是 --> C
    R -- 否 --> S[emit AgentEnd<br/>oneshot 发送结果]
    S --> Q

    style F fill:#fef7e0
    style L fill:#e6f4ea
```

## 3. 流式事件原语（tack-ai stream）

每个 LLM 调用和 agent 运行都通过 `EventStream` 暴露：生产者在独立 tokio task 中
推事件，消费者逐条 `next()`，最后 `result()` 取最终结果。

```mermaid
sequenceDiagram
    participant C as 消费者（TUI/RPC/ACP）
    participant S as EventStream<br/>(mpsc + oneshot)
    participant P as 生产者 task<br/>(provider 适配器)
    participant LLM as LLM API (SSE)

    C->>S: 调用 stream()
    activate P
    P->>LLM: HTTP 请求（SSE）
    loop 每个 SSE 块
        LLM-->>P: delta
        P->>S: push(TextDelta / ThinkingDelta /<br/>ToolcallDelta ...)
        S-->>C: next() → 事件
    end
    alt 正常结束
        P->>S: push(Done) + end(message)
    else 失败（带内错误，不抛出）
        P->>S: push(Error) + end(message<br/>stop_reason=Error)
    end
    deactivate P
    S-->>C: result() → AssistantMessage
```

## 4. Provider 适配层（tack-ai）

统一消息模型为枢纽：各 API 适配器负责「统一模型 ↔ 厂商线格式」双向转换，
注册表提供模型目录与凭据解析。

```mermaid
flowchart LR
    subgraph Unified["统一消息模型"]
        M[Message / AssistantMessage<br/>Context / ToolDefinition<br/>Usage / StopReason]
    end

    subgraph Adapters["API 适配器（api/）"]
        AM[anthropic_messages]
        OC[openai_completions]
        ORS[openai_responses<br/>+ Azure + Codex]
        GG[google_generative_ai<br/>+ vertex / ADC]
        MI[mistral_conversations]
        BD[bedrock_converse_stream<br/>SigV4 + event-stream]
        PM[tack_messages]
    end

    subgraph Ext["外部 API"]
        E1[(Anthropic)]
        E2[(OpenAI 兼容)]
        E3[(Google)]
        E4[(AWS Bedrock)]
        E5[(其他 40 providers)]
    end

    subgraph Auth["凭据"]
        AK[API key 解析<br/>--api-key → models.json → env]
        OA[AuthResolver<br/>过期主动刷新<br/>进程内双检锁]
    end

    M --> Adapters
    Adapters --> Ext
    AK --> Adapters
    OA --> Adapters
```

## 5. 会话持久化与分支（tack-session）

会话为 JSONL v3 文件（与 TS pi 字节兼容），条目以 `parent_id` 构成**树**；
`leaf_id` 指向当前分支末梢。branch/fork 即移动 leaf 指针。

```mermaid
flowchart TD
    subgraph File["session.jsonl（每行一条 SessionLine）"]
        H[header<br/>version=3, session_id]
        E1[entry: user message]
        E2[entry: assistant message]
        E3[entry: tool_result]
        E4[entry: user message B]
        E5[entry: assistant B]
        E6[entry: compaction<br/>摘要 + cut point]
        E7[entry: model_change / label /<br/>branch_summary / session_info]
    end

    H --> E1
    E1 -->|parent_id| E2
    E2 --> E3
    E3 --> E4
    E4 --> E5
    E3 -.branch.-> E6

    subgraph Ops["SessionManager 操作"]
        OP1[create / open /<br/>continue_recent / fork_from]
        OP2[append_* 系列<br/>追加并更新 leaf_id]
        OP3[branch(entry_id)<br/>回溯 leaf，产生新分支]
        OP4[build_session_context<br/>沿 parent 链还原上下文]
    end

    File --> Ops
```

## 6. 上下文压缩（tack-session compaction）

```mermaid
flowchart TD
    A[每个回合结束] --> B[calculate_context_tokens<br/>从 usage 估算上下文占用]
    B --> C{should_compact?<br/>超过 context_window<br/>阈值}
    C -- 否 --> Z([继续])
    C -- 是 --> D[find_cut_point<br/>沿 turn 边界找切割点]
    D --> E[extract_file_ops<br/>收集切割点前读/改过的文件]
    E --> F[serialize_conversation<br/>序列化待压缩消息]
    F --> G[LLM 生成摘要<br/>含文件操作清单]
    G --> H[append_compaction<br/>写 compaction 条目]
    H --> I[build_context_entries<br/>摘要 + 切割点后消息<br/>作为新上下文]
    I --> Z

    style G fill:#fef7e0
```

## 7. 工具执行（tack-tools）

```mermaid
flowchart LR
    subgraph Builtin["内置工具"]
        T1[read]
        T2[bash]
        T3[edit<br/>diff 校验]
        T4[write]
        T5[grep / find / ls<br/>gitignore 感知]
    end

    subgraph MCP["MCP 工具"]
        M1[mcp / mcp_sse<br/>外部 MCP server]
    end

    subgraph Exec["执行层"]
        V[schemars 参数校验<br/>失败 → 带内错误 result]
        L[文件修改串行锁<br/>read/bash 可并行]
        BE[BashExecutor trait]
        LOC[LocalBashExecutor<br/>本地 shell<br/>取消时杀进程树]
        ACPT[ACP terminal executor<br/>客户端终端能力]
    end

    LLM[LLM tool_call] --> V
    V --> Builtin & MCP
    Builtin --> L
    T2 --> BE
    BE --> LOC
    BE -.ACP 模式.-> ACPT
    L --> R[ToolResultMessage<br/>截断 truncate.rs]
```

## 8. 运行模式与协议（tack-app）

一个二进制，五种入口，共享同一 agent 核心：

```mermaid
flowchart TD
    BIN[tack 二进制] --> MODE{入口}
    MODE -->|无 prompt，终端中| TI[交互式 TUI<br/>tack-app/tui + tack-tui]
    MODE -->|"-p prompt"| PM[print 模式<br/>流式输出到 stdout]
    MODE -->|"acp"| AC[ACP server<br/>stdio JSON-RPC<br/>Zed 等编辑器]
    MODE -->|"rpc"| RP[RPC 模式<br/>JSONL 命令/事件<br/>与 pi --mode rpc 线兼容]
    MODE -->|"serve --listen"| SV[远程会话服务<br/>CBOR 帧，protocol v1]

    AC --> SESS1[session/new · load · prompt<br/>cancel · request_permission<br/>terminal/*]
    RP --> SESS2[prompt / steer / abort /<br/>set_model / compact / fork ...]
    SV --> SESS3[list / create / attach /<br/>prompt / steer / abort<br/>快照 + SessionProgress]

    TI & PM & AC & RP & SV --> CORE[agent_loop + SessionManager]
```

## 9. 扩展系统（tack-ext + extension_host）

插件有两种载体：默认是可执行子进程（`carrier: "process"`），也可以是
wasmtime 沙箱中的 WASI p1 模块（`carrier: "wasm"`，tack-ext-wasm）。两种载体
跑**同一份 NDJSON 协议**——WASM 载体把模块的 stdin/stdout 接到内存 duplex
pipe，交给 tack-ext 传输无关的 `PluginPeer`，握手（`protocol: 2`）、30s 调用
超时、死插件 fail-fast 语义与子进程完全一致。WASM guest 默认无任何能力
（无 fs/网络/环境变量），fuel / epoch 墙钟 / 内存上限由 host 钳制 manifest
声明值。host 每插件起一个载体实例，转发 agent 事件并桥接 tool 调用与 UI
对话框。

```mermaid
sequenceDiagram
    participant H as extension_host (tack-app)
    participant P as PluginPeer (tack-ext)
    participant X as 插件（子进程 / WASM guest）

    H->>H: 发现：~/.tack/agent/extensions/*<br/>settings.extensionPaths<br/>项目 .pi/extensions/*（信任门控）
    H->>P: spawn（extension.json manifest）<br/>process：起子进程，stdio NDJSON<br/>wasm：duplex pipe → WASI stdio，instantiate + _start
    Note over X: wasm 载体 = wasmtime 沙箱：无 fs/net/env<br/>fuel + epoch 墙钟 + 内存硬上限<br/>握手 protocol 2（process 为 1）
    P->>X: initialize（PROTOCOL_VERSION）
    X-->>P: register（hooks / tools / events）
    loop agent 运行期间
        H->>X: agent 事件扇出（ExtHooks）
        X-->>H: tool 调用结果 / hook 改写
        X->>H: UI/exec 请求（ExtUiRequest）
        H-->>X: TUI 主循环解析后回包
    end
```

## 10. TUI 渲染（tack-tui + tack-app/tui）

```mermaid
flowchart TD
    subgraph Input["输入"]
        ED[editor 组件<br/>多行 / 历史 / kill-ring / 撤销<br/>/命令 + @文件 自动补全<br/>外部编辑器 / 剪贴板图片]
        KB[keys<br/>Kitty keyboard 协议<br/>优雅降级]
    end

    subgraph Render["渲染（两种模式）"]
        MS[主屏模式 screen_main<br/>滚动回退 + 差分行更新]
        FS[全屏模式 screen_alt<br/>alt-screen / 鼠标滚动<br/>拖拽选择 → OSC 52 复制<br/>搜索 / prompt 跳转]
    end

    subgraph Comp["组件（components/）"]
        MD[markdown<br/>pulldown-cmark 流式渲染]
        SY[syntax<br/>syntect 高亮]
        TC[工具卡片<br/>彩色 diff / bash 流式输出]
        IMG[Kitty/iTerm2 内联图片]
        TH[主题 dark/light + 用户主题]
    end

    Input --> LOOP[TUI 主循环<br/>消费 AgentEvent 流]
    LOOP --> Render
    Render --> Comp
```

## 11. 取消与权限

```mermaid
flowchart LR
    subgraph Cancel["取消"]
        CT[CancellationToken<br/>每 prompt 回合一个]
        CT --> AB1[LLM 流中断]
        CT --> AB2[bash 杀进程树<br/>Windows: taskkill /F /T]
        CT --> AB3[stop_reason = Aborted<br/>带内返回]
    end

    subgraph Perm["权限模式（Shift+Tab 循环）"]
        P1[ask：只读工具放行<br/>编辑/命令弹窗]
        P2[acceptEdits：文件编辑放行<br/>命令仍弹窗]
        P3[plan：只读<br/>bash/edit/write/MCP 全拦]
        P4[bypass：不弹窗]
        P1 --> P2 --> P3 --> P4 --> P1
    end
```
