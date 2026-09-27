# Tack Architecture

**English | [简体中文](architecture.zh-CN.md)**

Tack is a Rust reimplementation of the TypeScript pi coding-agent core: headless
at its core, with an interactive TUI, ACP (Agent Client Protocol) editor
integration, and RPC and remote-session services.

## 1. Overall Architecture

Crates are layered with an acyclic dependency direction: `tack-ai ← tack-agent-core ← tack-tools`,
`tack-ai ← tack-session`, `tack-app → all crates`.

```mermaid
flowchart TB
    subgraph App["tack-app (binary)"]
        CLI[CLI / clap arg parsing]
        TUI[interactive TUI<br/>tui/]
        PRINT[print mode<br/>one-shot headless run]
        ACP[acp subcommand<br/>ACP server over stdio]
        RPC[rpc subcommand<br/>JSONL protocol over stdio]
        SERVE[serve subcommand<br/>CBOR remote sessions]
        EXTHOST[extension_host<br/>plugin discovery & lifecycle]
        MISC[settings / skills /<br/>system_prompt / auth]
    end

    subgraph Core["Core layer"]
        AGENT[tack-agent-core<br/>agent_loop / AgentEvent /<br/>AgentHooks / Extension / AgentTool]
        SESSION[tack-session<br/>JSONL v3 session persistence (tree)<br/>compaction context compression]
        TOOLS[tack-tools<br/>read / bash / edit / write /<br/>grep / find / ls / MCP]
    end

    subgraph AI["tack-ai (model layer)"]
        MSG[unified message model<br/>types / transform]
        ADAPTERS[API adapters<br/>Anthropic / OpenAI Completions /<br/>OpenAI Responses / Azure / Codex /<br/>Google / Vertex / Mistral / Bedrock / tack-messages]
        OAUTH[oauth<br/>PKCE / device flow<br/>AuthResolver proactive refresh]
        REG[providers registry<br/>40 providers / 1130-model catalog]
    end

    subgraph Infra["Infrastructure layer"]
        PROTO[tack-protocol<br/>CBOR schemas / framing<br/>RemoteClient]
        TUILIB[tack-tui<br/>terminal UI library<br/>components / markdown / syntax highlighting<br/>main-screen + alt-screen renderers]
        EXT[tack-ext<br/>subprocess plugin protocol<br/>NDJSON over stdio]
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

Key design decisions:

- **Streaming**: `EventStream<T, R>` (tokio mpsc event queue + oneshot final
  result); provider errors are always delivered **in-band** (`Error` event +
  `stop_reason: Error`), never thrown.
- **Tools**: the parameter boundary is `serde_json::Value` + `schemars` schema;
  validation failures become in-band error tool results. `execution_mode()`
  controls parallel/serial batches; file modifications serialize on a shared lock.
- **Hooks**: a single `AgentHooks` trait object (transform_context / before_tool_call /
  after_tool_call / steering / follow-up / next-turn).
- **Cancellation**: one `tokio_util::CancellationToken` per prompt turn; bash
  kills the whole process tree.
- **Windows**: bash resolves Git Bash (legacy WSL bash is rejected), same as TS pi.

## 2. Agent Loop (tack-agent-core)

`run_loop` is a direct port of TS `packages/agent/src/agent-loop.ts`: an outer
follow-up loop plus an inner tool-call/steering loop, with matching event
ordering and error semantics.

```mermaid
flowchart TD
    A([agent_loop entry]) --> B[emit AgentStart / TurnStart<br/>prompt message into context]
    B --> C[hooks.steering_messages<br/>drain queued user interjections]
    C --> D{inner loop<br/>tool calls or<br/>pending messages?}
    D -- yes --> E[inject pending messages<br/>emit MessageStart/End]
    E --> F[stream_assistant_response<br/>hooks.transform_context<br/>convert_to_llm → LLM streaming call]
    F --> G{stop_reason?}
    G -- Error/Aborted --> H[emit TurnEnd<br/>return]
    G -- normal --> I{any tool_calls?}
    I -- yes --> J{stop_reason == Length?<br/>args may be truncated}
    J -- yes --> K[fail all tool calls<br/>without executing]
    J -- no --> L[execute_tool_calls<br/>before/after hooks<br/>parallel or serial per execution_mode]
    K --> M[tool results into context]
    L --> M
    I -- no --> N[emit TurnEnd]
    M --> N
    N --> O[hooks.prepare_next_turn<br/>may update model / thinking_level]
    O --> P{hooks.should_stop_after_turn?}
    P -- yes --> Q([return new_messages])
    P -- no --> C
    D -- no --> R{hooks.follow_up_messages<br/>any follow-up messages?}
    R -- yes --> C
    R -- no --> S[emit AgentEnd<br/>send result via oneshot]
    S --> Q

    style F fill:#fef7e0
    style L fill:#e6f4ea
```

## 3. Streaming Event Primitives (tack-ai stream)

Every LLM call and agent run is exposed through an `EventStream`: the producer
pushes events from a dedicated tokio task, the consumer pulls them one by one
via `next()`, and finally calls `result()` for the final result.

```mermaid
sequenceDiagram
    participant C as Consumer (TUI/RPC/ACP)
    participant S as EventStream<br/>(mpsc + oneshot)
    participant P as Producer task<br/>(provider adapter)
    participant LLM as LLM API (SSE)

    C->>S: call stream()
    activate P
    P->>LLM: HTTP request (SSE)
    loop each SSE chunk
        LLM-->>P: delta
        P->>S: push(TextDelta / ThinkingDelta /<br/>ToolcallDelta ...)
        S-->>C: next() → event
    end
    alt normal completion
        P->>S: push(Done) + end(message)
    else failure (in-band error, not thrown)
        P->>S: push(Error) + end(message<br/>stop_reason=Error)
    end
    deactivate P
    S-->>C: result() → AssistantMessage
```

## 4. Provider Adapter Layer (tack-ai)

The unified message model is the hub: each API adapter handles bidirectional
conversion between the unified model and the vendor wire format, and the
registry provides the model catalog and credential resolution.

```mermaid
flowchart LR
    subgraph Unified["Unified message model"]
        M[Message / AssistantMessage<br/>Context / ToolDefinition<br/>Usage / StopReason]
    end

    subgraph Adapters["API adapters (api/)"]
        AM[anthropic_messages]
        OC[openai_completions]
        ORS[openai_responses<br/>+ Azure + Codex]
        GG[google_generative_ai<br/>+ vertex / ADC]
        MI[mistral_conversations]
        BD[bedrock_converse_stream<br/>SigV4 + event-stream]
        PM[tack_messages]
    end

    subgraph Ext["External APIs"]
        E1[(Anthropic)]
        E2[(OpenAI-compatible)]
        E3[(Google)]
        E4[(AWS Bedrock)]
        E5[(other 40 providers)]
    end

    subgraph Auth["Credentials"]
        AK[API key resolution<br/>--api-key → models.json → env]
        OA[AuthResolver<br/>proactive refresh on expiry<br/>in-process double-checked lock]
    end

    M --> Adapters
    Adapters --> Ext
    AK --> Adapters
    OA --> Adapters
```

## 5. Session Persistence & Branching (tack-session)

Sessions are JSONL v3 files (byte-compatible with TS pi); entries form a
**tree** via `parent_id`; `leaf_id` points to the tip of the current branch.
branch/fork simply moves the leaf pointer.

```mermaid
flowchart TD
    subgraph File["session.jsonl (one SessionLine per line)"]
        H[header<br/>version=3, session_id]
        E1[entry: user message]
        E2[entry: assistant message]
        E3[entry: tool_result]
        E4[entry: user message B]
        E5[entry: assistant B]
        E6[entry: compaction<br/>summary + cut point]
        E7[entry: model_change / label /<br/>branch_summary / session_info]
    end

    H --> E1
    E1 -->|parent_id| E2
    E2 --> E3
    E3 --> E4
    E4 --> E5
    E3 -.branch.-> E6

    subgraph Ops["SessionManager operations"]
        OP1[create / open /<br/>continue_recent / fork_from]
        OP2[append_* family<br/>append and update leaf_id]
        OP3[branch(entry_id)<br/>rewind leaf, create new branch]
        OP4[build_session_context<br/>rebuild context along the parent chain]
    end

    File --> Ops
```

## 6. Context Compaction (tack-session compaction)

```mermaid
flowchart TD
    A[end of each turn] --> B[calculate_context_tokens<br/>estimate context usage from usage]
    B --> C{should_compact?<br/>exceeds the context_window<br/>threshold}
    C -- no --> Z([continue])
    C -- yes --> D[find_cut_point<br/>find a cut point along turn boundaries]
    D --> E[extract_file_ops<br/>collect files read/modified before the cut]
    E --> F[serialize_conversation<br/>serialize the messages to compact]
    F --> G[LLM generates summary<br/>including the file-ops list]
    G --> H[append_compaction<br/>write the compaction entry]
    H --> I[build_context_entries<br/>summary + post-cut messages<br/>as the new context]
    I --> Z

    style G fill:#fef7e0
```

## 7. Tool Execution (tack-tools)

```mermaid
flowchart LR
    subgraph Builtin["Built-in tools"]
        T1[read]
        T2[bash]
        T3[edit<br/>diff validation]
        T4[write]
        T5[grep / find / ls<br/>gitignore-aware]
    end

    subgraph MCP["MCP tools"]
        M1[mcp / mcp_sse<br/>external MCP server]
    end

    subgraph Exec["Execution layer"]
        V[schemars arg validation<br/>failure → in-band error result]
        L[file-modification serial lock<br/>read/bash may run in parallel]
        BE[BashExecutor trait]
        LOC[LocalBashExecutor<br/>local shell<br/>kills the process tree on cancel]
        ACPT[ACP terminal executor<br/>client terminal capability]
    end

    LLM[LLM tool_call] --> V
    V --> Builtin & MCP
    Builtin --> L
    T2 --> BE
    BE --> LOC
    BE -.ACP mode.-> ACPT
    L --> R[ToolResultMessage<br/>truncation truncate.rs]
```

## 8. Run Modes & Protocols (tack-app)

One binary, five entry points, all sharing the same agent core:

```mermaid
flowchart TD
    BIN[tack binary] --> MODE{entry point}
    MODE -->|no prompt, in a terminal| TI[interactive TUI<br/>tack-app/tui + tack-tui]
    MODE -->|"-p prompt"| PM[print mode<br/>stream output to stdout]
    MODE -->|"acp"| AC[ACP server<br/>stdio JSON-RPC<br/>Zed and other editors]
    MODE -->|"rpc"| RP[RPC mode<br/>JSONL commands/events<br/>wire-compatible with pi --mode rpc]
    MODE -->|"serve --listen"| SV[remote session service<br/>CBOR frames, protocol v1]

    AC --> SESS1[session/new · load · prompt<br/>cancel · request_permission<br/>terminal/*]
    RP --> SESS2[prompt / steer / abort /<br/>set_model / compact / fork ...]
    SV --> SESS3[list / create / attach /<br/>prompt / steer / abort<br/>snapshot + SessionProgress]

    TI & PM & AC & RP & SV --> CORE[agent_loop + SessionManager]
```

## 9. Extension System (tack-ext + extension_host)

Plugins come in two carriers: executable subprocesses by default
(`carrier: "process"`), or WASI p1 modules in a wasmtime sandbox
(`carrier: "wasm"`, tack-ext-wasm). Both carriers run **the same NDJSON
protocol** — the WASM carrier connects the module's stdin/stdout to an
in-memory duplex pipe handed to tack-ext's transport-agnostic `PluginPeer`,
with an identical handshake (`protocol: 2`), 30s call timeout, and fail-fast
dead-plugin semantics. WASM guests have no capabilities by default (no
fs/network/env vars); fuel / epoch wall-clock / memory limits are clamped by
the host to the values declared in the manifest. The host spawns one carrier
instance per plugin, fans out agent events, and bridges tool calls and UI
dialogs.

```mermaid
sequenceDiagram
    participant H as extension_host (tack-app)
    participant P as PluginPeer (tack-ext)
    participant X as Plugin (subprocess / WASM guest)

    H->>H: discovery: ~/.tack/agent/extensions/*<br/>settings.extensionPaths<br/>project .pi/extensions/* (trust-gated)
    H->>P: spawn (extension.json manifest)<br/>process: spawn subprocess, stdio NDJSON<br/>wasm: duplex pipe → WASI stdio, instantiate + _start
    Note over X: wasm carrier = wasmtime sandbox: no fs/net/env<br/>fuel + epoch wall clock + hard memory cap<br/>handshake protocol 2 (process: 1)
    P->>X: initialize (PROTOCOL_VERSION)
    X-->>P: register (hooks / tools / events)
    loop during agent run
        H->>X: agent event fan-out (ExtHooks)
        X-->>H: tool call results / hook rewrites
        X->>H: UI/exec requests (ExtUiRequest)
        H-->>X: replies resolved by the TUI main loop
    end
```

## 10. TUI Rendering (tack-tui + tack-app/tui)

```mermaid
flowchart TD
    subgraph Input["Input"]
        ED[editor component<br/>multiline / history / kill-ring / undo<br/>/command + @file autocomplete<br/>external editor / clipboard images]
        KB[keys<br/>Kitty keyboard protocol<br/>graceful degradation]
    end

    subgraph Render["Rendering (two modes)"]
        MS[main-screen mode screen_main<br/>scrollback + differential line updates]
        FS[full-screen mode screen_alt<br/>alt-screen / mouse scroll<br/>drag-select → OSC 52 copy<br/>search / jump-to-prompt]
    end

    subgraph Comp["Components (components/)"]
        MD[markdown<br/>pulldown-cmark streaming render]
        SY[syntax<br/>syntect highlighting]
        TC[tool cards<br/>colored diffs / streaming bash output]
        IMG[Kitty/iTerm2 inline images]
        TH[dark/light themes + user themes]
    end

    Input --> LOOP[TUI main loop<br/>consumes the AgentEvent stream]
    LOOP --> Render
    Render --> Comp
```

## 11. Cancellation & Permissions

```mermaid
flowchart LR
    subgraph Cancel["Cancellation"]
        CT[CancellationToken<br/>one per prompt turn]
        CT --> AB1[interrupt the LLM stream]
        CT --> AB2[bash kills the process tree<br/>Windows: taskkill /F /T]
        CT --> AB3[stop_reason = Aborted<br/>returned in-band]
    end

    subgraph Perm["Permission modes (Shift+Tab cycles)"]
        P1[ask: read-only tools pass<br/>edits/commands prompt]
        P2[acceptEdits: file edits pass<br/>commands still prompt]
        P3[plan: read-only<br/>bash/edit/write/MCP all blocked]
        P4[bypass: no prompts]
        P1 --> P2 --> P3 --> P4 --> P1
    end
```
