# TS pi → Tack Upstream Alignment Tracking

**English | [简体中文](upstream-alignment.zh-CN.md)**

Tracks how the evolution of [earendil-works/pi](https://github.com/earendil-works/pi)
(the TypeScript original) is aligned in Tack. Update this document after every
upstream sync.

## Current baseline

| Item | Value |
| --- | --- |
| Upstream repo | `/data/github/pi` (origin: earendil-works/pi) |
| Aligned-to commit | `10d1ad621f9dc4b6c441221f14057a770d1d0fa2` (2026-09-21, `feat: add transactional replicated state`) |
| Alignment date | 2026-09-21 |
| Tack version | after v1.0.9 (unreleased) |

## Sync process

1. (Automatic) Every Monday at 04:17 UTC,
   [.github/workflows/upstream-delta.yml](../.github/workflows/upstream-delta.yml)
   runs `scripts/upstream-delta.sh` to generate an upstream-delta review
   report: when there are new commits it appends to the tracking issue whose
   title contains `[upstream-delta]` (creating one if none exists), and the
   full report is also stored as a workflow artifact. Manual review starts
   from the report in the issue.
   To generate the report locally:
   `UPSTREAM_REPO=/path/to/pi scripts/upstream-delta.sh`
   (`UPSTREAM_REPO` points at a local clone of upstream pi; if unset it is
   cloned from GitHub).
2. Evaluate each commit in the report: land directly / borrow the design /
   no follow-up (record the reason). To see original commit details:
   `git -C "$UPSTREAM_REPO" fetch origin &&
   git -C "$UPSTREAM_REPO" log --oneline <last-baseline>..origin/HEAD`.
3. After landing items, update this document's baseline and item statuses.
4. The model catalog (`crates/tack-ai/catalog.json`) is converted from the
   published `@earendil-works/pi-ai` npm package
   (`npm pack @earendil-works/pi-ai@latest`; data in
   `dist/providers/data/*.json`). Note that routing rules merged after a
   release must be overlaid manually (see the Copilot entry from 2026-09-08).

## 2026-11-22 upstream delta triage (10d1ad621 → 9c4652688)

This is a **targeted triage, not a completed alignment batch**. The review covered
the v0.87.0–v1.6.0 changelogs, feature/fix logs, and targeted comparisons against
Tack's implementation; it did not review every commit individually. The aligned-to
baseline therefore remains `10d1ad621`. No item in this section is recorded as
landed unless it is explicitly included in a landing table.

| Item | Value |
| --- | --- |
| Review target | `9c4652688590abb836b40ebd1e4725ecf9a7c7d1` (2026-11-22; v1.6.0 plus post-release commits) |
| Delta since baseline | 1,684 commits (1,660 non-merge commits) |
| Tack reviewed at | v1.0.21, `09e887eec096a488a84108b52c49a43db8742cc1` |
| Verification level | Source/changelog comparison only; no live-provider or real-terminal verification |

Cost estimates below include focused implementation and regression tests: **S** is
a small local change, **M** crosses modules or needs platform validation, **L** is
a multi-day/cross-crate project, and **XL** should have its own design and delivery
plan.

### Sync automation blocker (fix before the next alignment batch)

`UPSTREAM_REPO=/Users/zugle/workspaces/github/pi scripts/upstream-delta.sh` was run
locally and failed with `无法解析基线 commit（得到 ''）`. Before the fix below,
the canonical English document used `Aligned-to commit`, while
`scripts/upstream-delta.sh` only grepped for `对齐到的 commit`;
`.github/workflows/upstream-delta.yml` did not override `UPSTREAM_BASE`, so its
report step had the same failure. This may explain why the alignment baseline
stopped advancing, although the workflow history was not audited as part of
this review.

The bilingual parser fix is recorded in the landed table below. Expanded report
groups and the stable-mainline versus experimental-architecture view are
implemented in the third landing table; the offline test uses real per-directory
commits rather than mocked paths.

### Immediate correctness, safety, and provider-compatibility candidates

The findings in this candidate table describe the review-time state. Their
current implementations, where landed, are recorded in the landing tables below.

| Upstream commit | Candidate | Tack finding / implementation note | Cost |
| --- | --- | --- | --- |
| `87f9a3641` + `1b2aa0ca0` | Strict final tool-call argument integrity | Streaming previews may use tolerant parsing, but malformed final JSON and Responses calls without `output_item.done` must not execute. Tack currently applies `parse_streaming_json()` to final arguments and has no `argumentsError`; preserve raw arguments for model feedback instead of executing a repaired prefix. | M |
| `f715a15c1` | Incremental streaming JSON parser | Tack's coalescer reduces parse frequency but Anthropic, OpenAI Completions/Responses, Bedrock, Mistral, and other adapters still reparse accumulated text at flush time. Build one shared incremental parser and pair it with the strict-final distinction above; do not land only the performance half. | L |
| `b82477b7b` + `7a6bd5a97` + `14b804d80` | OpenAI-compatible request compatibility | Default non-OpenAI instruction messages to `system`, omit empty `tools: []` when tools are disabled, and add `requiresNonNullAssistantContent`. Tack still emits an empty array for tool history and lacks the latter compat switch. Addresses Groq, vLLM/LiteLLM, and proxy 400s. | S–M |
| `406e719a6` + `4a6a58eb5` + `aa47cedce` | Strict/untyped and Anthropic schema handling | Fall back from strict schemas with untyped nodes; preserve root `$defs`, `definitions`, and `additionalProperties` in non-strict Anthropic schemas. Tack's `convert_tools()` currently rebuilds a narrow root and can break MCP `$ref`s or hide the extra-argument constraint. | M |
| `6fcb48670` + `441f5767c` | Summary limits and print truncation exit status | Remove fixed summary output caps so reasoning cannot consume the whole summary allowance; preserve truncated print output but warn and exit non-zero on `stopReason=length`. Tack has fixed summary budgets and currently sets print exit 1 only for `Error`. | S |
| `0a4fad548` + `d3398f826` + `73f8de80b` + `3dd803d7e` | Overflow detection and output-token clamping | Leave headroom for OpenRouter tokenizer overcount, and recognize LiteLLM, Copilot HTTP 413, and Z.AI CN overflow forms. Prevents avoidable 400s and unnecessary compaction. Tack already has summary retry and cache-disabled summaries; do not rewrite those parts. | M |
| `a00a5048d` + `25190b72c` + `4699b5b26` | Retry classification | Retry temporary in-stream provider failures, empty/partial gateway responses, and standalone socket errors while excluding permanent quota failures. Cover both the assistant classifier and initial HTTP handling; changing only one layer leaves behavior inconsistent. | M |
| `1d74741e1` + `bde882c74` + `b2cb62923` + `f10993bc7` | OAuth refresh lifecycle | Serialize refreshes across processes, persist rotated tokens even if the caller is cancelled, wait for started refreshes during shutdown, and make MCP sign-in cancellable. Tack has an in-process provider lock but no equivalent cross-process/exit lifecycle. | M–L |
| `5806068c2` + `d850edee9` + `e529a82c9` + `9ffd27cfa` | MCP OAuth storage and authorization hardening | Key credentials by server name **and URL**, support metadata URL override and RFC 9207 `iss`, retain granted scopes on step-up, and allow only HTTP(S) authorization URLs. Migrate old token entries rather than silently invalidating them. | M |

### High-value feature and architecture candidates

| Priority | Upstream commit / source | Candidate and recommended shape | Cost |
| --- | --- | --- | --- |
| High — storage compatibility | `466db0fec` | **`ContextEditEntry` plus safe context boundaries.** Implement read/projection semantics before writing: branch-relative hide/replace of model context without changing raw history, shared by compaction, branch summaries, failure recovery, and forks. Split ordinary `context` from explicit full-system `context_with_system`; Tack currently exposes full messages through its transform hook and has no context-edit projection. | L |
| High — new agent capability | `8562bcf66`; `packages/codemode/README.md` | **Non-persistent Code Mode MVP.** Model-written JavaScript composes tools with loops, branching, and concurrency; intermediate results stay in the script and only the final output enters model context. Start opt-in with QuickJS/WASM, no fs/net/env/timers, strict time/memory/output/concurrency/depth limits, and one nested executor that preserves validation, hooks, permissions, and output caps. Reuse WASM resource-limit experience, not the long-lived plugin ABI. Defer deterministic journal replay and durable recovery. | XL |
| High — latency and tool control | `e029c3ed0` + `30a1d1849` + `ddaa0a034` + `04b97ef00` | **Deferred MCP connection and layered tool selection.** Wait only for direct servers before the first prompt; wait for deferred servers when searched/called and restore their tools after reconnect. Add `+name`/`-name`, `*` patterns, and an upstream CLI switch for disabling MCP; ordinary `--tools` must not accidentally clear MCP tools. | L |
| High — project MCP configuration | `1387af7b4` + `91f9f3b5d` | Let project MCP entries override only `enabled`/`exposure`/`toolExposure` of a global server, and let MCP servers reuse a provider login through `auth.provider`. | M |
| Medium — cache and cost | `60ac3bcc3` + `fcc33744b` + `b271b0a52` + `caa6967b6` | Preserve prompt caches across GPT thinking-level changes and dynamic tool additions/removals. Use `configuration_update`, Anthropic inline additions, and Responses message-anchored additional tools as appropriate; history edits and tool redefinitions need explicit fallback behavior. | L |
| Medium — provider catalogs and billing | `ee409a85f` + `7c22a93d0` | Fetch OpenRouter's per-key model list, limits, and prices; distinguish provider-reported billed totals from catalog estimates. Tack already has long-context tier pricing, so do not duplicate it. | M |
| Medium — provider continuation | `a758f3ae4` + `49fe0a237` | **Anthropic Message Threads.** Send only new messages when valid, fall back to a full request after earlier context/tool changes, and clean up per-session resources. The benefit is less repeated upload/latency, not a cheaper token price. Cache warming and summaries must not touch the main thread. | L |
| Medium — protocol parity | `e473b5cd8` + `beffcde92` + `6aab551a7` + `e03dca7a3` | Add additive RPC prompt `started`/`queued`/`handled` disposition; make session replacement emit lifecycle events exactly once and subscribe before running startup handlers so JSON/RPC clients do not lose `session_start` events. | M–L |
| Medium — current models | `a37306d43` + `a3f265b1c` + `139707002` + `3e4f87265` + `1692bba63` + `fe8a6f5dc` + `86918f9ee` | Refresh the embedded catalog **together with protocol migrations**: Azure Foundry Chat Completions, Kimi Coding over OpenAI Completions, current Qwen gateways and Individual CN, Gemini 4/prefixed Gemini 3+, and Copilot reasoning metadata. A JSON-only refresh is insufficient; `providers.rs` still contains the old Azure ID, Kimi protocol/base URL, and Qwen hosts. | M–L |
| Medium — tool discovery | `ae823bc0b` + `cc44fa216` + `e033f255f` + `79006f061` + `6d3e342b2` | Improve `tool_search`: exact/full-name and multi-term ranking, name-over-description weighting, Unicode normalization, and searchable properties behind local `$ref`s. Tack currently indexes only lowercase name/description text. | M |
| Later — workbench | p2 command menu/projects; `4839e41d6`, `5a14e69c0` | Add Cmd/Ctrl+K over existing built-in/session/model/extension commands and a cwd/project sidebar with working/failed/unread/child status. Keep authorization and command availability host-owned; do not add a remote arbitrary-CLI endpoint. | M–L |
| Later — agent output | p2 artifacts; `6f1a96e1d`, `da57c1662` | Add versioned Markdown/static-HTML artifacts with list/read/write/edit/show and transcript references. Use a session sidecar or independent store, not a v4 format change. Start with sandboxed static rendering; defer HTML eval, free-form annotation, and shared plugin panes. | L |
| Later — remote UI foundation | Chord delta/sync design | Add monotonic progress revisions, `afterRevision`, a bounded operation log, transcript windows, and snapshot reset/gap recovery to the existing CBOR protocol. Borrow resumable watch semantics; do not replace the protocol with Chord or claim multi-writer CRDT behavior. | L |
| Later — durable waits | `473515f9c`; `packages/durable` | Prototype one explicit parked tool (webhook or human approval) with a typed signal inbox, persisted checkpoint/deadline, timeout/cancel/duplicate handling, and restart recovery. Do not attempt to freeze arbitrary bash, Tokio futures, or running subagents, and do not promise exactly-once external side effects. | L–XL |

### Low-cost TUI and platform fixes worth batching

| Upstream commit | Candidate | Tack finding |
| --- | --- | --- |
| `9e9609216` | Preserve bash output when an over-long line is followed by a blank line | `truncate_tail()` only takes an oversized line's tail while the accumulated output is empty; a trailing blank line can therefore suppress the useful text. Also review the bash footer's `endLine` versus `startLine`. |
| `6100fe5a8` + `3bae85274` | Editor Home/End and selector PageUp/PageDown | Fullscreen currently captures these keys before the editor/autocomplete/dialogs. Use Ctrl+Home/Ctrl+End for transcript boundaries and let open menus handle selection keys first. |
| `a635f390` | Stable `/resume` selection across refresh/rename/delete | Tack rebuilds its select list and resets filtering selection; preserve identity by path and define deletion fallback rather than porting upstream's progressive-loading machinery. |
| `be2744d95` | Keep Kitty images valid after full redraw | Main-screen ED2/ED3 can invalidate an image while the upload cache later emits only its placement. Invalidate/retransmit on clear or avoid the destructive clear. |
| `672000c80c` + `4622edb79` + `4464442f9` + `1c8cb1781` | Draw fullscreen images last and crop images anchored above the viewport | Separate text clearing/output from image placement, then add bounded image-height metadata/search for cropped placements. Validate on Kitty/WezTerm/Orca rather than treating this as a single-terminal fix. |
| `e4f50d76d` + `3107a58d2` | Handle external SIGTSTP through the Ctrl+Z terminal-restore path | Include terminal-ownership follow-up semantics: no listener before start, remove only after terminal restoration, and do not interfere with an external editor. |
| `31b407dc6` | Windows console-close shutdown cleanup | Route close/SIGHUP through the same bounded shutdown as normal exit. The existing Job Object handles many child processes but does not replace extension/OAuth cleanup. |
| `b7bb7ca90` | Remaining fullscreen scroll optimization | Tack already materializes a visible window and bounds caches. Only optimize the remaining full-item offsets/spans scans (view-only frames, reuse, binary search); do not port upstream's readonly-render-array breaking API. |

### Reviewed with no immediate duplicate work

- `4438c58ba` (answer every tool call after an aborted batch): Tack already writes
  `"Operation aborted"` results for unstarted calls in sequential and parallel paths.
  An optional small follow-up is to check cancellation before lookup/hooks.
- `3a7bb6f48` (streaming read): already covered by the bounded `BufReader` line-window
  implementation.
- `cb1205e46` (very many entries after compaction): Tack builds its session path
  iteratively and is not exposed to the JavaScript spread/call-stack failure.
- `51ba3ab5b` (retry abort-listener buildup): Rust cancellation tokens/futures do not
  have the Node listener-retention mechanism.
- `5b5be2c3` (very large self-rendered tool result): the upstream crash is a V8
  spread-argument limit; Tack uses Rust iteration/extend. Large-render performance is
  a separate concern, not the same crash fix.

### Explicitly deferred / do not port wholesale

- **p2:** its README calls it an experimental prototype that "will be gone soon".
  Borrow command-menu, project/sidebar, workflow, and artifact ideas only.
- **p3:** currently a headless skeleton, not a complete successor product. Tack
  already has richer process/WASM plugin carriers; dogfood new host capabilities
  incrementally instead of rewriting the core as plugins.
- **Chord / Durable:** no wholesale migration. Borrow resumable watches and explicit
  parked waits respectively. Durable Code Mode replay is a later, separate project.
- **Radius/cloud backup, generic rooms/CRDT collaboration, and remote execution
  environments:** no current alignment commitment. Existing `tack serve` remains the
  remote-session path.
- **pi-schema:** primarily solves TypeScript/CSP/TypeBox concerns; it is not an
  independent Tack product feature.
- **Agent-authored self-applied production plugins:** do not bypass Tack's install
  policy, review, and WASM capability boundaries.

### Compatibility constraints for implementation

1. **Context edits:** add v3 and v4 interoperability fixtures before writing new
   entries. Preserving an unknown entry as custom data is not enough if its context
   projection is ignored. Cover migration, fork, compaction, summaries, and usage.
2. **OAuth stores:** name → name+URL key changes need backward-compatible migration,
   cross-process read/modify/write protection, and symlink-resolved lock identity.
3. **RPC:** optional response fields such as disposition are additive; lifecycle event
   ordering is not. Add golden traces for startup/new/switch/fork/clone/reload and
   JSON print, including exactly-once subscription and handler-emitted events.
4. **Threads/cache preservation:** compaction, context edits, system/tool changes,
   summaries, and cache warming need full-request or non-continuation fallbacks plus
   session-resource cleanup.
5. **Code Mode:** nested calls must share the top-level security/permission path.
   Keep program-only structured results off the persisted wire until a versioned
   schema is designed; cap fan-out, nested calls, output, and resource use.
6. **Projects/artifacts:** use additive protocol metadata and sidecar stores. Do not
   change session v4 for UI-owned grouping, read state, or artifact versions.

### Recommended delivery order

1. Repair delta automation and add parser/grouping tests.
2. Land final-argument integrity, OpenAI/schema compatibility, summary/print fixes,
   and the low-cost TUI batch as focused PRs.
3. Harden OAuth/MCP lifecycle and shutdown behavior.
4. Deliver the shared incremental JSON parser as its own performance project.
5. Add ContextEdit read compatibility, deferred MCP startup, and provider/catalog
   migrations with interoperability tests.
6. Build the non-persistent Code Mode MVP behind a feature gate.
7. If the browser workbench is a product priority, add command menu/project sidebar,
   resumable watch, and static versioned artifacts in that order.

### First small-fix landing (Unreleased)

These low-risk items are implemented in the current working tree and covered by
focused regression tests. They are not yet in a tagged release.

| Upstream commit | Tack landing |
| --- | --- |
| Delta automation blocker | `scripts/upstream-delta.sh` accepts both `Aligned-to commit` and `对齐到的 commit`; new offline `scripts/upstream-delta.test.sh` verifies both languages and runs in the `evals-selftest` CI job. Expanded grouping is recorded in the third landing table. |
| `9e9609216` | `truncate_tail()` retains the end of an oversized line when only blank lines follow, preserves that suffix, and keeps byte/line metadata consistent; the bash footer uses the partial line's real `start_line`. Unit coverage includes 60 KB/219 KB and LF/CRLF cases, plus a real bash-tool regression test. |
| `441f5767c` | Print mode warns on stderr and exits 1 for `StopReason::Length` in both text and JSON modes while retaining partial stdout. Error behavior is unchanged. The separate `6fcb48670` summary-budget item is recorded in the second landing table below. |
| `6100fe5a8` + scoped portion of `3bae85274` | Bare Home/End reach the fullscreen editor; Ctrl+Home/Ctrl+End own transcript boundaries; open autocomplete receives PageUp/PageDown before transcript scrolling. Dialogs already had input priority. The remaining custom-selector work and upstream's `capturesViewportInput` component API are not ported. |

### Second small-fix landing (Unreleased)

These follow-up items are also implemented and covered by focused tests.

| Upstream commit | Tack landing |
| --- | --- |
| `b82477b7b` + `7a6bd5a97` + `14b804d80` | OpenAI-compatible instruction messages default to `system` outside OpenAI's own API; empty `tools: []` is omitted unless `requiresToolsWithToolHistory` opts in; `requiresNonNullAssistantContent` independently selects `""` for tool-call-only assistant replay. Explicit `supportsDeveloperRole` still overrides detection. Request-level tests cover OpenAI/Groq/OpenRouter roles, vLLM-style empty tools, both opt-ins, and null versus empty-string assistant content. |
| Scoped portion of `a635f390` | `/resume` refresh preserves the selected session path across rename and sort changes. Delete falls back to the next row, then the previous row at the end. Tack has synchronous rather than progressive loading, so upstream's pending-partial-list machinery is unnecessary. Tests use real named session files. |
| `6fcb48670` | Compaction history, split-turn prefix, and abandoned-branch summaries use `model.max_tokens` directly; a zero model limit omits the override. `reserveTokens` remains only the compaction trigger. Tests capture provider options and verify the model limit instead of a reserve-derived cap. |

### Third upstream correctness landing (Unreleased)

These provider-integrity, compatibility, retry, and reporting items are
implemented in the current working tree and covered by offline/focused tests.

| Upstream commit | Tack landing |
| --- | --- |
| Delta report grouping follow-up | Reports now group `packages/tui`, `mcp`, `codemode`, `chord`, `durable`, `apps/p2`, `apps/p2-mac`, and `apps/p3` explicitly. A second, non-overlapping view separates stable-mainline commits from commits touching p2/p3/Chord/Durable experimental paths; cross-directory experimental commits stay on the experimental side. Offline tests create real commits for each group and verify counts. |
| `87f9a3641` + `1b2aa0ca0` | Streaming arguments remain lenient previews, while completed Anthropic, Bedrock, Mistral, OpenAI Completions, and Responses calls use strict parsing plus string-literal repair. Empty/blank arguments become `{}`; non-objects and malformed JSON become `{}` plus optional camelCase `argumentsError` containing the cause and raw text. The agent returns that error as a ToolResult before preparation, hooks, or execution in both execution modes. Responses `response.completed` with a tool call missing `output_item.done` is now an in-band stream error. Existing length-stop rejection still takes precedence. |
| `406e719a6` + `4a6a58eb5` + `aa47cedce` | Strict conversion rejects nodes lacking `type`, `anyOf`, and `enum`; `prefer` falls back to non-strict and `require` retains the explicit error, while enum-only nodes remain supported. Non-strict Anthropic conversion preserves only the additional supported root fields `additionalProperties`, `$defs`, and `definitions`, keeping local `$ref` targets and extra-argument constraints without widening strict schemas. |
| `09f105957` + `0a4fad548` + `d3398f826` + `73f8de80b` + `3dd803d7e` | A shared `streamSimple`-style estimator uses the latest applicable assistant usage plus estimated trailing messages. Anthropic, Bedrock, Google, Mistral, OpenAI Completions, and Responses clamp the caller or model output cap with `max(4096, ceil(15% input))` headroom and the upstream 32768-output fallback near the context limit. Overflow detection adds LiteLLM pre-call wording, Z.AI CN wording, and anchored HTTP 413 forms; body-only 413 references and `4130` do not match, while Cerebras's private bodyless rule is limited to 400. |
| `a00a5048d` + `25190b72c` + `4699b5b26` | Retry patterns cover temporary provider/gateway failures, exact Anthropic `api_error`, incomplete responses, and standalone Node socket codes. Quota/billing/subscription-sharing and Z.AI 1308 exclusions run first; HTTP numeric and socket-code matching use ASCII token boundaries. `send_with_retry` now reads the error body and uses the same classifier as assistant errors, so permanent HTTP 429 quota failures are not retried but temporary provider bodies can recover. |

### Fourth OAuth and shutdown landing (Unreleased)

These lifecycle, credential-isolation, authorization-hardening, and shutdown
items are implemented with offline fixtures and process-level signal tests.

| Upstream commit | Tack landing |
| --- | --- |
| `bde882c74` + `b2cb62923` | Provider OAuth refresh is detached after the cross-process lock/recheck phase, so caller cancellation cannot drop a rotated grant. Conditional commit prevents stale writes after login/logout; graceful command exits await tracked refreshes. Regression tests cancel a live request and prove persistence, and verify that a late refresh cannot resurrect logout. |
| `1d74741e1` + `f10993bc7` | MCP refresh uses per-server process/local locks across reload → request → save, reuses grants changed by another process, refreshes rejected tokens before browser fallback, and tracks persistence during shutdown. Interactive discovery/registration/callback/manual paste is cancellable; `tack mcp login --timeout` bounds the whole flow (default 300 s). |
| `5806068c2` + `d850edee9` + `e529a82c9` + `9ffd27cfa` | `mcp-tokens.json` entries move atomically from legacy server-name keys to normalized name+URL keys without invalidating grants. `oauth.authServerMetadataUrl` replaces discovery (HTTPS except loopback); rmcp's RFC 9207 issuer validation receives callback/manual-paste `iss`; granted scopes survive refresh/step-up; authorization endpoints accept only HTTP(S). |
| `31b407dc6` | SIGHUP/SIGTERM and Windows `CTRL_CLOSE_EVENT` enter bounded TUI/print/RPC cleanup with a detached four-second backstop; terminal errors no longer skip extension cleanup. RPC bash executors and prompt pumps are cancelled and drained before teardown, preventing orphaned trees and late persistence. Unix subprocess tests verify graceful 129/143 exits, active-tree cleanup, and stdin-EOF 0; real Windows console validation remains platform follow-up. |

### Fifth incremental streaming-JSON landing (Unreleased)

The shared parser and adapter migration are implemented without changing the
session, RPC, ACP, or plugin wire formats.

| Upstream commit | Tack landing |
| --- | --- |
| `f715a15c1` | `ToolCallArgumentsStream` consumes each tool-argument delta once while retaining untouched raw text for the existing strict final parser. Owned snapshots use a copy budget to bound deep-cloning work, with forced EOF/error previews so trailing deltas cannot be stranded. Anthropic, Bedrock, Mistral, OpenAI Completions/Responses, tack-messages, and CodeBuddy now share this path; Responses authoritative replacement resets non-prefix state and emits only a missing suffix, while grammar/custom tools retain raw non-JSON input. Tests cover complete-document and per-prefix differential behavior, chunk/escape/Unicode boundaries, concurrent calls, replacement/reset, malformed final rejection, and trailing previews. A criterion bench compares legacy accumulated-prefix reparsing with incremental parsing, and the existing fuzz target exercises arbitrary UTF-8 split points plus strict finish. |

### Sixth context projection, MCP startup, and provider migration landing (Unreleased)

The storage-compatibility, startup-latency, and current-provider batch is
implemented with offline fixtures and mocked provider requests. Context edits
are read/projection-only: Tack does not yet originate `context_edit` entries or
split extension `context` from `context_with_system` events.

| Upstream commit / source | Tack landing |
| --- | --- |
| `466db0fec` + `d0fe21308` | `context_edit` is a typed v3 entry and a payload-compatible v4 custom entry. Latest-on-branch omit/replace projection preserves raw history and message metadata, including imported string replacements for assistant/tool results. Compaction cut selection, projected token estimates, branch summaries, and all prompt surfaces consume the projection. v3→v4 migration remaps target ids and records rebuilt-tail source ids in `details.tackRetainedTailSourceIds`, preserving post-compaction edits. Tests cover latest-wins, branch-relative behavior, array-only roles, stale usage invalidation, compaction/summary input, and checkpoint migration. |
| `e029c3ed0` + `30a1d1849` + `ddaa0a034` + `04b97ef00` | Enabled MCP servers connect in the background; direct servers alone gate the first prompt for at most 10 seconds, while `tool_search` awaits deferred servers and merges their tools into a live pool shared by TUI/print/RPC/ACP. Reconnection rebuilds the proxies. `--tools`/`--exclude-tools` accept `*` patterns, pure `+name`/`-name` modifiers work across CLI and layered `defaultTools`, non-MCP allowlists preserve unnamed MCP tools as search-only, and `--no-mcp` disables configured servers. Dynamic-pool activation, bounded direct waits, CLI filtering, ACP/RPC smoke tests, and the agent-loop activation path are covered. |
| `a37306d43` + `a3f265b1c` + `139707002` + `3e4f87265` + `1692bba63` + `fe8a6f5dc` + `86918f9ee` | Azure is the provider id for Responses and Foundry Completions, with shared endpoint normalization, deployment-name payload mapping, and old-session replay compatibility. Kimi Coding uses OpenAI Completions at `/coding/v1`; four Qwen plans use the current gateways and per-plan credential fallback; Gemini 4+/prefixed ids use `thinkingLevel`; Copilot reasoning usage accepts top-level `reasoning_tokens`. The embedded 1179-model catalog updates the Azure/Kimi/Qwen/Copilot slices. A full generated catalog refresh was not possible offline because upstream's generated provider-data files are not materialized in the local clone; unrelated provider snapshots are unchanged. |

## 2026-09-21 alignment batch (baseline 4a6ed0194 → 10d1ad621)

160 upstream commits reviewed one by one (report generated by
`scripts/upstream-delta.sh`).

### Landed

| Upstream commit | Content | Tack landing |
| --- | --- | --- |
| `890f92088` + `af7359b90` | Unknown providers default to non-strict tools; Cerebras excluded | `0741270`: `compat.rs` default changed to false; catalog.json gained explicit `supportsStrictMode` for 593 openai-completions models (508 missing ones written per upstream whitelist rules); with unit tests + catalog integration test |
| `e5d18382a` + `e98f287ee` + `c37b0e03b` | Retry Cloudflare 520 / Azure peak errors; backoff capped at 60s | `ba52cba`: two RETRYABLE_PATTERNS entries + `retry_delay_ms` (`RetryPolicy.max_agent_delay_ms`, `settings.retry` gains `maxAgentDelayMs`) |
| `4bd3f48df` | GLM-5.2 uses `reasoning_effort` on Mistral | `c33d2c0`: `uses_reasoning_effort` gains a `zai-glm-5-2` branch |
| `1283afd0d` | Thinking replay for Anthropic's renamed models | `8cfd6ea`: message_start no longer overwrites `output.model`; differing names recorded as `response_model`; fallback pricing not ported (Tack has no server-side fallback) |
| `8a7b0c03d` | Bedrock 1h cache-write billing | `ae0d0fe`: metadata `cacheDetails` accumulated into `cache_write_1h` when `ttl=="1h"` (the pricing side was already ready) |
| `0c7bb7c5c` | Responses errors carry the provider name | `bfef4a8` |
| `8bdcd4498` | Compaction cut point for oversized trailing tool results | `c198896`: `find_cut_point` falls back to the last cut point when none is found |
| `acaa253cc` | Extension tool parameter schema validation | `e8d38a5`: `ToolSpec::validate_parameters()` + filtering malformed tools after the handshake |
| `b6419322e` | Templates with bad frontmatter no longer silently dropped | `875a3b4`: `tracing::warn!` (the ResourceDiagnostic system not ported) |
| `59eb4c393` | Copy shortcut description | `d588e4b`: Chinese and English copy |
| `9b791a4cc` | Exact session id without a full scan | `8189909`: `--session-id` switched to header-only `find_session_by_id` |
| `3349e1db1` + `60e7e76bd` + `6dff740fa` | Clipboard trio | `127f25e` + `6c67877`: new `tack-tui/src/clipboard.rs` (native backend dispatch pbcopy/wl-copy/xclip/xsel/termux/PowerShell, descriptive errors on failure, OSC 52 only as a fallback for remote/no-display, WSL prefers the Windows clipboard); tack-app call sites surface failures as notices |
| `661619e87` + `0e283203c` (attached to overflow.ts as a whole) | overflow detection + compact-and-retry | `ac119b5` + `c7e1af5`: new `tack-ai/src/overflow.rs` (three detectors: error pattern / silent over-window / length-stop, plus `is_recoverable_length`); agent-loop overflow recovery — new `AgentHooks::compact_for_overflow`, implemented in three places: `SessionHooks`/`RpcCompactionHooks`/`HostCompactionHooks` (each extracting a `run_compaction` core), budget mirroring `_overflowRecoveryAttempted` (reset on new user message / successful response) |
| `0e283203c` remainder | length-stop recovery wiring | `a53d3c4`: agent-loop Length branch — `is_recoverable_length` (output below the requested expected cap is judged as context pressure / server-side truncation; the expected cap takes the caller's `max_tokens` override over the model default; a user-configured small cap counts as genuinely hitting the cap and is not compacted) goes through compact-and-retry on hit (sharing the overflow recovery budget); a second length stop or genuinely hitting the cap goes through the existing fail-truncated-tool-calls path; with four tests |
| `bfa686240` | CJK punctuation file completion | `a03b825`: CJK punctuation (U+3001–303F + upstream's explicit fullwidth list) as token separators, the same judgment synchronized in three places: `compute()`, `refresh_autocomplete` performance gating, `ext_trigger_token` (plugin triggers); CJK characters remain word characters |

### Reviewed, no changes needed

| Upstream commit | Conclusion |
| --- | --- |
| `a8b3dd199` signal kills shell | `tack-tools/bash.rs` already has equivalent semantics and tests |
| `de2de549b` compaction cancel race | covered by architecture (lineage validation + dual stale guards + tokio cancellation semantics) |
| `dd01f5b24` recent session discovery | already pure stat sorting, lighter than post-fix upstream |
| `e86102f18` Codex Off effort | current behavior already matches post-fix upstream |
| `9e05370b2` mid-session system message | collapsed replay equals upstream's default behavior; provider-native pass-through (cache optimization) is an independent enhancement |
| `47a18e37b` GIF magic bytes | Tack identifies images solely by extension, structurally immune |
| `590144609` fuzzy delay | no per-character regex scoring, structurally immune |
| `faa9863cb` queue message input hook | tack-ext has no input interception event, structurally immune |
| `509ee2bd0` user bash hook fail-closed | no extension routing attack surface |
| `46c9de402` event de-registration | static subscription model, no snapshot-iteration issue |
| `465853498` + `13784598d` thinking drop notices | Tack has no such notice feature surface |
| `fe219d7f8` bash duration format | Tack does not display durations |
| `fde6d778f` summary click-to-collapse | Tack summaries always render in full, no corresponding interaction surface |
| `b03a367a4` Anthropic fallback config | Tack has no server-side fallback, nothing configurable |
| `e4c75a732` + `16292398a` forced system prompt | upstream net diff is zero (revert); Tack never had this predecessor design |
| `21b8cc1a4` stale image conversion | converted synchronously at render time, no async cache race |
| `b7f788194` remote prompt event | Tack remote client has no one-shot prompt-then-exit path |
| `60740991c` Node module cache, `40c256ccc` jiti lazy loading | TS/Node-specific |
| bug reporting group (`3c75b2747`/`d1230ea20`/`d875512cc`/`c7cdb460a`/`1e0fe2049`/`63787ee6b`) | feature does not exist at all, deferred; needs re-evaluation when introduced |
| `7e1950768` experimental micro agent | not a mainline alignment item |
| `bdee230f1` image generation model catalog | Tack has no image generation feature |
| `fa0e1f48a` LaTeX | Tack has no LaTeX rendering |
| `d92eb8d4b` Fireworks deferred tool loading | Tack fireworks goes through openai-completions; the runtime already supports tool_references, the catalog follows on next refresh |
| `3390bd936` cache warming late guard | attached to the warming feature (not ported); must be included when it is ported |

### Tracking / TODO

- **Pico/Pico5 design doc stream (37 commits) + chord (operation log) + durable + `10d1ad621`**: under observation, following the v4 fork-policy line of "borrow the design, don't follow blindly". If Tack starts a persistent agent runtime project, re-read the final converged Pico5 docs (the pico→pico3 intermediate drafts are obsolete); chord's operation log direction is better than delta snapshots.
- **Regular model-catalog refresh**: this batch added 508 explicit `supportsStrictMode` entries; the rest waits for the next full npm refresh (deepseek/openai-codex/fireworks/google/vercel/kimi-coding, new meta/radius, anthropic `promptCache`/`inputLimits`, google thinking levels from models.dev).
- **Pending ports (by priority)**:
  1. session affinity headers (`bbb61e34a` OpenRouter default-on + `6671c6047` Baseten + `561a2e066` OpenCode headers) — compat fields + header emission, cache-hit gains. **kimi-coding evaluated and excluded (2026-09-22)**: Moonshot's cache is an org-level shared prefix cache (the official docs explicitly say "shared within the same organization"), it honors no affinity headers, and there is no server-side per-replica routing problem; its real gap is top-level `cache_control` — see "Incidental fixes" `5954421`
  2. `16235fd93` Gemini thinking-off switched to thinkingLevelMap clamp
  3. ~~`f5c946480` image input limits~~ **deferred (evaluated 2026-09-22)**: the upstream feature itself is "data first, enforcement half in place" — the only thing actually enforced is the `images.resize` profile override, and all models in the upstream catalog currently use the default profile (2000x2000/4.5MB/jpeg80), equivalent to Tack's existing fixed behavior; `maxPerMessage`/`maxPerRequest`/`maxRequestBytes` have no enforcement layer upstream either. A full port requires first adding an execution context to `AgentTool::execute` (aligning with upstream `ExtensionContext`'s model/cwd), an ABI evolution across 5 crates + the plugin protocol — a separate project. Tack's Model deserialization does not reject unknown fields, so future npm refreshes bringing in `inputLimits` data are safe. Re-evaluate after the dependency lands
  4. ~~length-stop recovery wiring~~ → landed in `a53d3c4` (see the "Landed" table)
  5. `1e39862f6` llama.cpp `/props` thinking detection
  6. `46bde88a1` per-model compaction budget (settings `modelOverrides`)
  7. `dfbf793b7` session selector background progressive loading
  8. `b73412a37` Meta provider + Muse OAuth (independent feature)
  9. `4d38031fb` radius baseline catalog (small)
  10. `c596d09d9` + `3390bd936` prompt cache warming (medium-large, cost optimization)
  11. TUI: ~~`bfa686240` CJK punctuation file completion~~ (landed in `a03b825`), `d7951ec36` skill completion bare-name scoring, `803f0e906` WezTerm fullscreen images (verify with a real test first)
- **Independent infrastructure TODO**: `AgentTool::execute` execution context (aligning with upstream `ExtensionContext`: model/cwd etc.) — the common prerequisite for several upstream features such as the image resize profile and read's ctx.cwd; spans tack-agent-core/tack-tools/tack-ext/tack-ext-wasm/tack-app and requires the plugin protocol to evolve alongside
- **evals borrowings**: `42cd371ba` TUI footer eval idea (verify docs are actually injected into the model context), `f3564a1d4` docs-navigation reachability static check — can be ported into evals/docs-audit at low cost.

### Incidental fixes (non-upstream items)

- `5954421`: **kimi-coding prompt caching fix (ahead of upstream)**. The
  Moonshot Messages API only honors `cache_control` at the request **top
  level** (markers inside message bodies are ignored; by default it only reads
  the 5m cache and never writes). Both Tack and upstream TS previously only
  marked inside message bodies → kimi cache writes were completely ineffective
  and the cacheRead discount was lost. Fix: when
  `AnthropicCompat.top_level_cache_control` is enabled, send `cache_control`
  at the top level (Short→5m / Long→`ttl:1h`, reusing the CacheRetention
  mapping) and no longer mark inside message bodies; the 4 catalog kimi-coding
  models are flagged. Verification: run two sessions back to back and watch
  the response `usage.cache_read_input_tokens` turn non-zero. **Follow-up
  (2026-10, Moonshot billing split)**: Moonshot's context-caching upgrade
  bills Cache Write separately (5m write = input price, 1h = 2×, hits =
  1/10). First-party k3 models (`kimi-coding`/`moonshotai`/`moonshotai-cn`)
  gained `cacheWrite` = input price in the catalog (was 0), and the
  top-level Short marker now pins an explicit `"ttl":"5m"` — Moonshot's
  Messages contract only documents ttl-bearing markers.
- `4635473`: three extension_tests demo-plugin tests share the
  `TACK_AGENT_DIR` environment variable and interfere with each other in
  parallel (a pre-existing flake, reproducible at baseline); serialized with
  ENV_LOCK.

## 2026-09-08 alignment batch (baseline → 4a6ed0194)

### Landed

| Upstream commit | Content | Tack landing |
| --- | --- | --- |
| `96617628e` | all mistral-medium-* use `reasoning_effort` (prefix match) | `tack-ai/src/api/mistral_conversations.rs` `uses_reasoning_effort` changed to `starts_with("mistral-medium-")`, with a prefix-match unit test |
| `fcff255b0` | built-in tools default `{ type: "json_schema", strict: "prefer" }` | `tack-agent-core::AgentTool` gains a `constrained_sampling()` default method; read/bash/edit/write return prefer; tests in `tack-tools` |
| `7d8ab31a4` | Copilot `gpt-*` prefix always routes to the Responses API | github-copilot in `tack-ai/catalog.json` refreshed from npm 0.85.1 data (10 → 28 models), with new routing rules overlaid (`gpt-6-astra` rerouted from completions to responses; rules: `gpt-`/`grok-`/`oswe`/`mai-` → `openai-responses`) |
| `e687434a6` + `47acd8e6c` | tree navigation concurrency guard: rejected during streaming/compaction; re-checked after the dialog closes | RPC: new `RpcState.is_compacting` field, `fork` command checks `is_streaming`/`is_compacting`; set for the whole compaction (including error paths); TUI: `/fork`, `/tree`, `/rewind` check `running` on open, re-check on fork confirmation; with RPC tests |
| `ab9e6f89b` | Alt+scroll wheel ×5 acceleration (SGR button bit 3 = Alt) | `tack-tui/terminal.rs` restores the dropped crossterm mouse modifiers mapping; `scroll_view.rs` scrolls 3→15 lines with Alt |
| `caf6dfe73` | clipboard operations moved off the UI thread | `tack-app/tui/input.rs` Ctrl+V image/text clipboard reads wrapped in `spawn_blocking` (both the arboard and CLI paths) |

### Reviewed, no changes needed

| Upstream commit | Content | Conclusion |
| --- | --- | --- |
| `b2602be77` | EventStream queue `shift()` O(n²) fix | tokio channels on the Rust side have no such issue; existing `remove(0)` call sites are all bounded small queues |
| `9841914c7` | mouse hover does not change list selection | Tack `terminal.rs` directly drops `MouseEventKind::Moved`; hover never reaches the component layer, structurally immune |
| v4 fork-policy (`6dfc66d32`, `85186f823`, `41218b394`, etc.) | retain namespace projection, lane state reset | belongs to agent harness storage v4 (lanes/named branches); Tack's session format is still v3; v3 `forkFrom` semantics (copy all non-header entries + parentSession pointer) verified item by item to match `fork_from_in`/`sqlite_backend::fork_session` |

### Patterns adopted by reference

- **Backend conformance test suite** (the pattern of
  `packages/agent/.../testing/conformance/session-repo.ts`): added
  `tack-session/tests/backend_conformance.rs`, running the same scenarios
  against both the JSONL and SQLite backends and comparing structural
  fingerprints (ids stripped, parentId normalized to positional indexes,
  nested timestamps recursively erased). Covers append/reopen, branch+path,
  fork isolation, invalid-id rejection; 4 cases.
  The TS side's WP08 batch also plans to bring the SQLite backend under the
  same contract — Tack covered it first.

### Tracking / TODO

- [x] **Storage v4 migration** (landed 2026-09, including write-path closure):
  `tack_session::v4` + `v4_bridge` modules, `fork_policy` (see
  `crates/tack-session/V4_NOTES.md` for details) — transaction log format
  (header/write type fields aligned field-by-field with upstream codec/types),
  two-pass streaming legacy v3→v4 migration (original file kept as .bak,
  encryption stays encrypted), fork-policy namespace projection rules ported
  item by item (`tack.lane.state` reset, `tack.result`/`tack.op.*`/
  `tack.pending.*` dropped, unknown `tack.*` preserved namespaces raise an
  error). **`SessionBackend::JsonlV4` is the default backend**: new sessions
  write v4 natively, opening v3/v2/v1 files migrates transparently,
  `sessionBackend: "v3"` is the escape hatch. Write-path mapping: one
  transaction per entry (entry + tip + mirror values + usage ledger row); v3
  mutation entries (model_change/thinking/session_info/label) are stored as
  custom entries mirroring current values. Deliberate trade-offs: migration
  entry ids use 8-char uuid-v4 (not uuidv7); migrate immediately on open
  (upstream defers to the first commit); unknown/future entry types are
  preserved as custom entries instead of upstream's error (Tack's no-data-loss
  principle); corrupt lines without ids are skipped.
  **Remaining**: WP07 (SQLite v4 backend, repo-level session list) and the
  operation/pending runtime are not implemented; forks start from in-memory
  current state (WP08 two-pass streaming scan not done).
- [ ] **Regular model-catalog refresh**: `catalog.json` is a manually
  converted snapshot (1130+ models after this Copilot refresh). Recommend
  checking for new npm `@earendil-works/pi-ai` versions and doing a full
  refresh at each alignment, rather than only patching a single provider.
- [x] **Docs evals** (`9211da172`) (landed 2026-09): `evals/docs-audit/` —
  `static_check.sh` model-free cross-validation (settings keys / hook events /
  CLI flags / features.* keys vs. code) + 3 agent-driven deep audit tasks
  (deterministic anchors generate expected values; tasks automatically flip
  expectations when docs or code drift).

## Historical baselines

| Date | Upstream commit | Notes |
| --- | --- | --- |
| 2026-09-08 | `4a6ed0194` | mistral/strict sampling/Copilot catalog/navigation guard/TUI details + conformance test suite |
| 2026-09-21 | `10d1ad621` | strict default/retry/overflow recovery/clipboard/small-fix batch; Pico5 under observation |
