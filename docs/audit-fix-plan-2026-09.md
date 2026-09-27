# 2026-09 Full Audit Fix Plan (Completed + Regression Review)

**English | [简体中文](audit-fix-plan-2026-09.zh-CN.md)**

Source: a 7-way parallel static audit of the whole repo (~120k lines); high-severity items were re-verified line by line against the code by the parent agent.
Fixes were done in 6 batches (A=TUI, B=tack-app top level, C=rpc/acp/remote/shell_hooks, D=tack-ai, E=tack-tools, F=lower-level crates)
in parallel worktrees, all merged into main. Acceptance: `cargo check --workspace --all-targets` with 0 errors,
`cargo fmt --all --check` clean, and per-batch cargo test all green (including 30+ new regression tests).

## Regression review (second round: 4-way parallel close reading of the diffs + parent-agent re-verification)

The fix batches themselves went through another review round that "only looks for newly introduced bugs"; 17 items were found and have **all been fixed** (b6a9c38..e30ac6e, 8 commits):

- ☑ R-01 `hooks.rs` reentrancy guard `?` dropped the materialized history optimization → return optimized instead (ff0c80a)
- ☑ R-02 `hooks.rs` fork/resume during compact could land the compaction on the new branch → snapshot leaf-reachability guard (ff0c80a)
- ☑ R-03 `agent_loop` drained updates between concurrent emits could still overtake superseding events → drain mutex + deterministic test (b6a9c38)
- ☑ R-04 remote F32's host-global rebinding hurt other sessions → resolve adapter/auth per session.model at run time (8719ac9)
- ☑ R-05 permission prompts closed by disconnect/reaping were misreported as "denied by user" → distinguish explicit Deny from no-answer closure (8719ac9)
- ☑ R-06 RPC MCP pool: crashed connections reused indefinitely / partial failures cached under the full fingerprint / OAuth token expiry → 401 → liveness check + only complete sets enter the pool + token fingerprint (2dc2175)
- ☑ R-07 anthropic/openai_responses/codebuddy lacked terminal-state parse fallback (tool args truncated when a proxy omits block-end) → unified re-parse on termination (3e6227a)
- ☑ R-08 F41a excluded IncompleteMessage-class transient errors from retry → is_request() restores retry (3e6227a)
- ☑ R-09 fork branch summary reused self.cancel already cancelled by Esc → independent token (aba8a8c)
- ☑ R-10 BangDone had no session guard (cross-session pollution/silent loss) → session_id guard (aba8a8c)
- ☑ R-11 prompts with attachments during compacting were silently dropped → enqueue text + attachment-count notice (aba8a8c)
- ☑ R-12 manual compact landing guard (session replacement/lineage moves) and no longer strands already-queued prompts (aba8a8c)
- ☑ R-13 fullscreen search recomputed everything per delta during streaming → content-driven recompute with 200ms throttle (aba8a8c)
- ☑ R-14 mermaid over-budget entries were evicted on insert while also clearing other entries → don't insert when over budget (aba8a8c)
- ☑ R-15 mcp-serve truncation broke tool_use/tool_result pairing (provider kept returning 400) → truncate at user-message boundaries (30db732)
- ☑ R-16 wasm/v1 stderr forwarding spun at 100% CPU on persistent IO errors → only requeue over-limit lines, exit on real errors (e30ac6e)
- — R-17 wasm watchdog doesn't re-arm on non-protocol blocking WASI calls (long sleep/preopen reads): a design trade-off, documented in DEFAULT_MAX_EXECUTION and opt-out per plugin


## P0 — High severity: all fixed ☑

### TUI main-loop blocking / UX (Batch A)
- ☑ F01 `notice()` no longer `line_cache.clear()` (push keeps index alignment) → MCP sampling/notifications no longer trigger full re-render — `976e981`
- ☑ F02 `/compact` moved to spawn + Esc-cancellable + result lands via AppEvent — `80bf45f`
- ☑ F03 `!cmd` moved to spawn + cancellable + partial output streams into the tool card — `13d5269`
- ☑ F04 MCP connection establishment moved into the spawned run task (submitting a prompt no longer freezes) — `93e7465`
- ☑ F05 fork/tree branch summary moved to spawn (both call sites) — `0386d6b`
- ☑ F06 plugin ui.select/confirm/input dialogs decline while occupied (no longer shove aside the permission dialog); exec backgrounded — `64f73df`

### Memory/resource leaks
- ☑ F07 remote tracks Attach/Detach per connection, releases uniformly on disconnect, sessions can be reaped by the reaper — `41ee263`
- ☑ F08 RPC MCP connections cached and reused by fingerprint (no more mem::forget of subprocess heaps per prompt) — `2e52515`+`9487802`
- ☑ F09 audit sink gets connect 10s/total 30s timeouts + buffer cap 10_000 — `bce164a`
- ☑ F10 RemoteClient aborts the read-pump on Drop (no reference cycle) — `682795d`
- ☑ F11 wasm fuel refilled only by protocol byte flow (CountingReader/Writer) + default 10min no-progress watchdog (deadline re-arms with protocol I/O; not a lifetime cap) — `23cd65e`+`3b72201`
- ☑ F12 logs tail reads via reverse window; follow increments by offset (fixes half-line loss across ticks) — `cf0c1ce`

### CPU hotspots
- ☑ F13 7 adapters' streaming tool-arg parsing throttled to the DeltaCoalescer window (event sequence byte-for-byte unchanged) — `514709e`+`4544f37`
- ☑ F14 codebuddy wired into DeltaCoalescer (50ms/4KB coalescing) — `4544f37`
- ☑ F15 edit fuzzy match: spawn_blocking + complexity circuit breaker at 2M + identical-line short-circuit — `ab948c6`
- ☑ F16 grep caches hit files only when context>0 + 64MB cache cap + limit clamp 10_000 — `1679999`

## P1 — Medium severity: all fixed ☑

- ☑ F17 remote permission prompts: 10min timeout + cancel propagation + 0 connections means immediate deny — `41ee263`
- ☑ F18 codebuddy abort settles stray_bridge_calls + 5min CLI idle watchdog — `3d003cf`
- ☑ F19 plugin exec timeout kills the process tree and reaps; open_browser waits in the background — `2887808`
- ☑ F20 browser render timeout/failure branches wait after kill — `e6eb9a0`
- ☑ F21 microcompact spill directory keeps the most recent 100 — `b62f55d`
- ☑ F22 accumulator persist path fixed per task + cleaned up on registry eviction — `f8fdffd`
- ☑ F23 mermaid cache changed to a 128MB byte-budget LRU — `0c86034`
- ☑ F24 mcp-serve serializes prompts (run_lock) + history truncated to 200 entries — `b069670`
- ☑ F25 eval stderr collected as a bounded 64KiB tail — `4aa1075`
- ☑ F26/F28 tools_manager/self_update/catalog_refresh blocking segments moved to spawn_blocking + 256MiB download cap — `198b2ba`
- ☑ F27 subagent git commands converted to tokio::process + with_cwd (TUI/print call sites wired) — `5e17478`+`743f948`
- ☑ F29 read tool text/image sync segments moved into spawn_blocking — `3f6f389`
- ☑ F30 fullscreen right-click paste moved to spawn_blocking — `b254173`
- ☑ F31 shell_hooks stdin write concurrent with output collection (pipe deadlock eliminated) — `236b045`
- ☑ F32 remote set_model rebuilds the provider adapter across apis + re-resolves auth — `dbb1db9`
- ☑ F33 remote cancel token registered inside the Prompt branch lock (Abort no longer lost) — `e19045b`
- ☑ F34 rpc get_state reads the real is_compacting — `2e52515`
- ☑ F35 acp rejects concurrent prompts on the same session (in_flight guard) — `a473298`
- ☑ F36 Mistral tool ids always generated at 9 chars — `d8e6da1`
- ☑ F37 permission globs precompiled at load time — `304bec7`
- ☑ F38 web_fetch per-hop validation + resolve_to_addrs pins verified IPs (DNS rebinding closed) — `ffaece4`
- ☑ F39 atomic_write tmp files get random suffixes; auth/mcp_oauth credentials go through atomic_write_private(0600) + in-process mutex — `b622f46`
- ☑ F40 UUID variant bits; google urlencoding by UTF-8 bytes — `6712349`+`f5c4bb3`
- ☑ F41 send_with_retry retries by error class; codex device polling tolerates transient errors — `d2585f3`+`ecde429`
- ☑ F42 connection_id atomic counter; stale unix socket file handling + 0600 — `74ed204`
- ☑ F43 v4 usage_ids HashSet (O(1) uniqueness checks) — `34dcd04`
- ☑ F44 SessionManager entry_ids incremental cache — `2a429a4`
- ☑ F45 /resume v4 scanning memchr prefilter + no deep clones — `1f54417`
- ☑ F46 fullscreen tool cache eviction switched to &str keys — `a1e9297`+`f1abe6d`
- ☑ F47 args fingerprint cached across deltas (append-only fast path) — `3d5b12c`
- ☑ F48 hooks prescan single-pass + byte budget + compact no longer holds the session lock across network calls — `b62f55d`
- ☑ F49 stats/v4store table lookups use borrowed keys, zero clones — `d40ed09`+`34dcd04`
- ☑ F50 branch_summary HashMap index — `1399b2c`
- ☑ F51 executor output bounded(64) backpressure + spill 256MB circuit breaker — `be5341a`+`f8fdffd`
- ☑ F52 @image 20MB cap + reject non-regular files (named pipes no longer hang) — `a884df5`
- ☑ F53 emit_update during emit drain always goes to the mailbox (ordering fixed + regression test) — `6b10f6f`

## P2 — Low severity: all fixed ☑

- ☑ F54 overlay margin saturating_mul; cursor_visual_col dead code removed — `213b546`
- ☑ F55 OSC 11 query on unix reads /dev/tty instead (no longer swallows keystrokes) — `fc35bd2`
- ☑ F56 wasm stderr 16MB line cap; shutdown write gets a 2s short timeout — `6faaf87`+`23cd65e`
- ☑ F57 unknown-tool titles with large args use field summaries — `233ff89`
- ☑ F58 theme selection uses owned String (Box::leak eliminated) — `9f55cb9`
- ☑ F59 fullscreen search recomputes as the transcript grows + per-frame clones removed — `a1e9297`
- ☑ F60 ext panel renders via field-level borrows — `233ff89`
- ☑ F61 edit exact match counts overlapping occurrences ("aa" in "aaa" is not unique) — `5cc3805`

## Won't fix (design trade-offs / protocol limits / theoretical items)
- `manager.rs:1039`+`context.rs:17` entries() full clone: the API is designed this way; callers have the revision cache as a backstop.
- `acp/agent.rs` sessions map inserts but never removes: ACP v1 has no session/close — protocol limitation.
- `screen_alt.rs` kitty retransmit every frame: fullscreen mode currently shows no images — no reachable impact.
- `editor.rs:121-125` global replacement of paste markers: a known corner with harsh trigger conditions.
- `syntax.rs:186` 64-bit fingerprint theoretical collision: not triggerable in practice.
- `vertex_adc.rs` token mint race: both tokens are valid — harmless.
- SSE has no read timeout by default: keeps TS-aligned semantics; `httpIdleTimeoutMs` is configurable; the codebuddy side is covered by the F18 watchdog.
- `atomic_write.rs` Windows fallback deletes before renaming: an inherent window within std's capabilities.
- `tack-ai/images.rs:138` non-streaming body doesn't race against cancel: timeouts are the backstop; low impact.

## Behavior notes (post-merge)
- `WasmLimits::default().max_execution`: None → Some(10min), and the deadline re-arms with protocol I/O progress (a hang watchdog; long-lived plugins unaffected; explicit None overrides).
- settings thresholds like `microcompact.maxChars` are now interpreted in **bytes** (defaults scaled 4x; key names unchanged).
- `tack-ext` `read_line_bounded` signature change (new OverCap parameter, private → pub).
- `tack-session` gains pub exports `V4FileSummary`/`scan_v4_file_summary`.
- TUI Esc cancels the current token even when idle (supports /compact, !cmd cancellation).
- In RPC mode, MCP connections are reused by specs+model fingerprint and rebuilt only when the fingerprint changes (dropping the old connection kills its subprocess).
