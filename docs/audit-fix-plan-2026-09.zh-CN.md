# 2026-09 全面审计修复计划（已完成 + 回归审查）

**[English](audit-fix-plan-2026-09.md) | 简体中文**

来源：全仓库 7 路并行静态审计（~12 万行），高严重度条目已经父代理逐一读码复核。
修复分 6 批（A=TUI、B=tack-app 顶层、C=rpc/acp/remote/shell_hooks、D=tack-ai、E=tack-tools、F=底层 crate）
在 worktree 中并行完成，全部合入 main。验收：`cargo check --workspace --all-targets` 0 error、
`cargo fmt --all --check` 干净、各批 cargo test 全绿（含新增回归测试 30+ 个）。

## 回归审查（第二轮，4 路并行精读 diff + 父代理复核）

修复批本身再经一轮“只找新引入 bug”的审查，发现 17 项并已**全部修复**（b6a9c38..e30ac6e，8 commit）：

- ☑ R-01 `hooks.rs` 重入守卫 `?` 丢弃已物化的 history 优化 → 改返回 optimized（ff0c80a）
- ☑ R-02 `hooks.rs` compact 期间 fork/resume 会把压缩落到新分支 → 快照 leaf 可达性守卫（ff0c80a）
- ☑ R-03 `agent_loop` 并发 emit 之间 drained update 仍可能越过 superseding 事件 → drain 互斥锁 + 确定性测试（b6a9c38）
- ☑ R-04 remote F32 的 host 全局重绑误伤其他 session → run 时按 session.model 现解析适配器/auth（8719ac9）
- ☑ R-05 权限询问被断开/收割关闭误报 "denied by user" → 区分显式 Deny 与无应答关闭（8719ac9）
- ☑ R-06 RPC MCP 池：崩溃连接无限复用/部分失败按完整指纹缓存/OAuth token 过期 401 → 活性检查 + 仅完整集入池 + token 指纹（2dc2175）
- ☑ R-07 anthropic/openai_responses/codebuddy 缺终态解析兜底（代理省略 block-end 时工具参数缺尾） → 终止时统一重解析（3e6227a）
- ☑ R-08 F41a 把 IncompleteMessage 类瞬态错误排除出重试 → is_request() 恢复重试（3e6227a）
- ☑ R-09 fork 分支摘要复用已被 Esc 取消的 self.cancel → 独立 token（aba8a8c）
- ☑ R-10 BangDone 无 session 守卫（跨会话污染/静默丢失） → session_id 守卫（aba8a8c）
- ☑ R-11 compacting 期间带附件 prompt 被静默丢弃 → 文本入队 + 附件计数提示（aba8a8c）
- ☑ R-12 manual compact 落地守卫（会话替换/血统移动）且不再搁浅已排队 prompt（aba8a8c）
- ☑ R-13 全屏搜索流式期间每 delta 全量重算 → 内容驱动重算 200ms 节流（aba8a8c）
- ☑ R-14 mermaid 超预算条目插入即逐出还清空其他条目 → 超预算不插入（aba8a8c）
- ☑ R-15 mcp-serve 截断破坏 tool_use/tool_result 配对（provider 持续 400） → 按 user 边界截断（30db732）
- ☑ R-16 wasm/v1 stderr 转发对持续 IO 错误 100% CPU 空转 → 仅超限行续排、真错误退出（e30ac6e）
- — R-17 wasm watchdog 对非协议阻塞 WASI 调用（长 sleep/preopen 读）不 re-arm：属设计取舍，已在 DEFAULT_MAX_EXECUTION 文档注明并可 per-plugin opt-out


## P0 — 高严重度：全部已修 ☑

### TUI 主循环阻塞 / UX（Batch A）
- ☑ F01 `notice()` 不再 `line_cache.clear()`（push 保持索引对齐）→ MCP sampling/通知不再全量重渲染 — `976e981`
- ☑ F02 `/compact` 改 spawn + Esc 可取消 + 结果经 AppEvent 落地 — `80bf45f`
- ☑ F03 `!cmd` 改 spawn + 可取消 + partial output 流式进工具卡 — `13d5269`
- ☑ F04 MCP 连接建立移入 spawned run 任务（提交 prompt 不再冻结） — `93e7465`
- ☑ F05 fork/tree 分支摘要改 spawn（两个调用点都改） — `0386d6b`
- ☑ F06 插件 ui.select/confirm/input 对话框占用时 decline（不再顶掉权限框）；exec 后台化 — `64f73df`

### 内存/资源泄漏
- ☑ F07 remote 按连接跟踪 Attach/Detach，断开统一释放，会话可被 reaper 回收 — `41ee263`
- ☑ F08 RPC MCP 连接按指纹缓存复用（不再每 prompt mem::forget 堆子进程） — `2e52515`+`9487802`
- ☑ F09 audit sink 加 connect 10s/总 30s 超时 + buffer 上限 10_000 — `bce164a`
- ☑ F10 RemoteClient Drop 时 abort read-pump（无循环引用） — `682795d`
- ☑ F11 wasm fuel 仅按协议字节流动（CountingReader/Writer）补充 + 默认 10min 无进展 watchdog（deadline 随协议 I/O re-arm，不是生命周期上限） — `23cd65e`+`3b72201`
- ☑ F12 logs tail 反向窗口读取；follow 按 offset 增量（修掉跨 tick 半行丢失） — `cf0c1ce`

### CPU 热点
- ☑ F13 7 个适配器流式工具参数解析节流到 DeltaCoalescer 窗口（事件序列逐字节不变） — `514709e`+`4544f37`
- ☑ F14 codebuddy 接入 DeltaCoalescer（50ms/4KB 合并） — `4544f37`
- ☑ F15 edit 模糊匹配：spawn_blocking + 复杂度熔断 2M + 相同行短路 — `ab948c6`
- ☑ F16 grep 仅 context>0 缓存命中文件 + 64MB 缓存上限 + limit clamp 10_000 — `1679999`

## P1 — 中严重度：全部已修 ☑

- ☑ F17 remote 权限询问：10min 超时 + cancel 联动 + 0 连接直接 deny — `41ee263`
- ☑ F18 codebuddy abort 结算 stray_bridge_calls + 5min CLI idle watchdog — `3d003cf`
- ☑ F19 插件 exec 超时杀进程树收尸；open_browser 后台 wait — `2887808`
- ☑ F20 browser 渲染超时/失败分支 kill 后 wait — `e6eb9a0`
- ☑ F21 microcompact spill 目录保留最近 100 个 — `b62f55d`
- ☑ F22 accumulator persist 路径每任务固定 + 逐出注册表时清理 — `f8fdffd`
- ☑ F23 mermaid 缓存改 128MB 字节预算 LRU — `0c86034`
- ☑ F24 mcp-serve 串行 prompt（run_lock）+ 历史 200 条截断 — `b069670`
- ☑ F25 eval stderr 有界 64KiB 尾部收集 — `4aa1075`
- ☑ F26/F28 tools_manager/self_update/catalog_refresh 阻塞段 spawn_blocking + 下载 256MiB 上限 — `198b2ba`
- ☑ F27 subagent git 命令 tokio::process 化 + with_cwd（TUI/print 调用点已接） — `5e17478`+`743f948`
- ☑ F29 read 工具文本/图片同步段移入 spawn_blocking — `3f6f389`
- ☑ F30 全屏右键粘贴 spawn_blocking — `b254173`
- ☑ F31 shell_hooks stdin 写入与输出收集并发（管道死锁消除） — `236b045`
- ☑ F32 remote set_model 跨 api 重建 provider 适配器 + auth 重解析 — `dbb1db9`
- ☑ F33 remote cancel token 在 Prompt 分支锁内注册（Abort 不再丢） — `e19045b`
- ☑ F34 rpc get_state 读真实 is_compacting — `2e52515`
- ☑ F35 acp 同 session 并发 prompt 拒绝（in_flight guard） — `a473298`
- ☑ F36 Mistral 工具 id 恒产 9 字符 — `d8e6da1`
- ☑ F37 权限 glob 加载时预编译 — `304bec7`
- ☑ F38 web_fetch 逐跳校验 + resolve_to_addrs 固定已验证 IP（DNS rebinding 堵死） — `ffaece4`
- ☑ F39 atomic_write tmp 随机后缀；auth/mcp_oauth 凭证走 atomic_write_private(0600)+进程内互斥 — `b622f46`
- ☑ F40 UUID variant 位；google urlencoding 按 UTF-8 字节 — `6712349`+`f5c4bb3`
- ☑ F41 send_with_retry 按错误类别重试；codex 设备轮询 transient 容错 — `d2585f3`+`ecde429`
- ☑ F42 connection_id 原子计数器；unix socket 陈旧文件处理 + 0600 — `74ed204`
- ☑ F43 v4 usage_ids HashSet（O(1) 唯一性检查） — `34dcd04`
- ☑ F44 SessionManager entry_ids 增量缓存 — `2a429a4`
- ☑ F45 /resume v4 扫描 memchr 预过滤 + 免深克隆 — `1f54417`
- ☑ F46 全屏工具缓存逐出改 &str 键 — `a1e9297`+`f1abe6d`
- ☑ F47 args 指纹跨 delta 缓存（append-only 快速路径） — `3d5b12c`
- ☑ F48 hooks 预扫描单遍化 + 字节预算 + compact 不再持 session 锁跨网络调用 — `b62f55d`
- ☑ F49 stats/v4store 查表借用键零克隆 — `d40ed09`+`34dcd04`
- ☑ F50 branch_summary HashMap 索引 — `1399b2c`
- ☑ F51 executor 输出 bounded(64) 背压 + spill 256MB 熔断 — `be5341a`+`f8fdffd`
- ☑ F52 @image 20MB 上限 + 拒绝非常规文件（命名管道不再卡死） — `a884df5`
- ☑ F53 emit drain 期间 emit_update 一律进邮箱（乱序消除+回归测试） — `6b10f6f`

## P2 — 低严重度：全部已修 ☑

- ☑ F54 overlay margin saturating_mul；cursor_visual_col 死代码删除 — `213b546`
- ☑ F55 OSC 11 查询 unix 改读 /dev/tty（不再吞按键） — `fc35bd2`
- ☑ F56 wasm stderr 16MB 行上限；shutdown 写 2s 短超时 — `6faaf87`+`23cd65e`
- ☑ F57 未知工具 title 大 args 走字段摘要 — `233ff89`
- ☑ F58 主题选择 owned String（Box::leak 消除） — `9f55cb9`
- ☑ F59 全屏搜索随转录增长重算 + 去每帧克隆 — `a1e9297`
- ☑ F60 ext 面板字段级借用渲染 — `233ff89`
- ☑ F61 edit 精确匹配计入重叠出现（"aa" in "aaa" 不唯一） — `5cc3805`

## 不修复（设计取舍/协议限制/理论项）
- `manager.rs:1039`+`context.rs:17` entries() 全量克隆：API 设计如此，调用方有 revision 缓存兜底。
- `acp/agent.rs` sessions map 只插不删：ACP v1 无 session/close，协议限制。
- `screen_alt.rs` kitty 每帧重传：全屏模式当前不展示图片，无可达影响。
- `editor.rs:121-125` 粘贴标记全局替换：触发条件苛刻的已知边角。
- `syntax.rs:186` 64-bit 指纹理论碰撞：实际不可触发。
- `vertex_adc.rs` token mint 竞态：两 token 均有效，无害。
- SSE 默认无读超时：保持 TS 对齐语义，`httpIdleTimeoutMs` 可配；codebuddy 侧已由 F18 watchdog 兜底。
- `atomic_write.rs` Windows 回退先删后改名：std 能力内固有窗口。
- `tack-ai/images.rs:138` 非流式 body 不与 cancel 竞速：有超时兜底，影响低。

## 行为注意点（合并后）
- `WasmLimits::default().max_execution`：None → Some(10min)，且 deadline 随协议 I/O 进展 re-arm（挂起看门狗，长生命周期插件不受影响；可显式 None 覆盖）。
- settings `microcompact.maxChars` 等阈值现按**字节**解释（默认值已 4x 缩放，key 名不变）。
- `tack-ext` `read_line_bounded` 签名变更（新增 OverCap 参数，私有→pub）。
- `tack-session` 新增 pub 导出 `V4FileSummary`/`scan_v4_file_summary`。
- TUI Esc 在 idle 时也会 cancel 当前 token（支撑 /compact、!cmd 取消）。
- RPC 模式下 MCP 连接按 specs+模型指纹复用，指纹变化才重建（旧连接 drop 杀子进程）。
