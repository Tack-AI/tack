# Changelog

All notable changes to Tack are documented here. The format follows
`## [x.y.z]` version headers so `/changelog` and the startup "what's new"
notice can parse entries (same convention as TS pi).

## [1.0.8] - 2026-10-01

### Changed

- **Live footer context percentage is exact mid-stream**: once the provider
  reports the prompt side of the usage (every adapter does from the first
  `message_start`, CodeBuddy included), the footer's context indicator uses
  the real per-request size — system prompt, history and tool results
  included — instead of the stale pre-run estimate plus generated output.
  Providers that only report usage in the final chunk keep the old
  output-growth approximation.

### Fixed

- **CodeBuddy post-compaction turns stall out — early stop, "tool isn't
  available" refusals, or reasoning loops**: three compounding defects in
  the JSONL session rebuild that runs on context divergence (compaction,
  history edit):
  1. the rebuilt CLI session lost its tool list — `spawn_new` starts with
     an empty list, so the resumed CLI's `tools/list` was answered with
     `[]` for the whole post-respawn turn. The model then refused ("the
     bash tool isn't available to me right now") or spun in reasoning
     loops (observed 95+ empty responses against the real CLI). The
     current tools now ride the respawn;
  2. the projection flattened every tool call/result into `[tool:name]` /
     `[tool_result:id]` text markers, so the resumed history contained
     zero real tool structure and the model mimicked the markers as
     literal text and ended its turn (the "stuck after Compacted context"
     report). Settled tool turns now project to the CLI's NATIVE
     `reasoning` / `function_call` / `function_call_result` records with
     the same `messageId`/`conversationRequestId` linkage the CLI writes
     itself — verified against the real CLI to keep calling tools after a
     compaction-shaped rebuild (new `cb_rebuild_repro` example). Only
     dangling calls (no result in the rebuilt slice) and orphan results
     still degrade to text markers, since a resumed CLI must not see
     calls it would park on;
  3. a settled tool result trailing the last assistant message forced the
     whole rebuild down the lossy transcript-replay fallback; such tails
     now ride the rebuilt prefix natively.
- **CodeBuddy `codebuddy CLI idle: no events for 5 minutes` on parallel
  tool calls**: the CLI's `stream_event` channel can drop a parallel
  tool_use block entirely (observed on CLI 2.156.0 with hy4-preview-f:
  two parallel `bash` calls streamed as one), while the CLI still
  dispatches a `tools/call` MCP frame for every call in its complete
  assistant message and awaits the whole batch. The orphan frame was
  never answered, so the CLI went silent mid-turn and the idle watchdog
  killed it five minutes later. The tool boundary now drains the rest of
  the frame burst (short idle grace) and MATERIALIZES any unpaired frame
  as a fresh parked call — complete name + arguments ride the frame;
  only the model's tool_use id is synthesized — so every dispatched
  frame gets its result and the turn continues natively.
- **CodeBuddy: the model repeats an earlier thought verbatim after a
compaction** — the same reasoning block + text re-emitted
byte-identically (observed 3×), re-running the same file reads each
time. The transcript-replay fallback (taken when a native session
rebuild has no deliverable user tail) pasted THINKING blocks into the
flattened history verbatim, and reasoning models strongly echo their
own reasoning when it reappears as plain in-context text; each echo
then landed in the next compaction's retained tail and reinforced
itself. Thinking is no longer pasted (the summary, text, and tool
markers carry the state), the replay marker now spells out that the
history is final and must not be repeated, re-answered, or re-run, and
overflow-compaction retries carry the goal recitation: that retry path
bypasses `transform_context`, so without the recitation there was no
trailing user message to deliver and every overflow compaction fell
into the lossy paste.
- **CodeBuddy: quitting tack orphans the CLI child process**: the
  provider's CLI session registry is a process-wide static (never
  dropped) and the TUI/print/compact paths exit through
  `std::process::exit` (no destructors run), so `kill_on_drop` and the
  session `Drop` impl never fired — every quit left a `codebuddy` CLI
  process behind (visible in `ps` via its `--allowedTools mcp__tack`
  argv) until its own stdin-EOF/idle handling eventually reaped it. The
  already-existing `close_all_sessions()` sweep is now wired into every
  agent-loop mode exit: TUI, print, compact, rpc, acp, mcp-serve, eval
  and serve.

## [1.0.7] - 2026-09-30

### Fixed

- **CodeBuddy crash after compaction** (`index out of bounds: the len is 0
  but the index is 0` in `codebuddy_jsonl.rs`): once context compaction
  folded every assistant reply into the summary, a native session rebuild
  had no settled prefix — it rewrote the CLI's session file to empty and
  the post-write integrity check indexed `lines[0]` on zero lines,
  panicking the provider task (and cascading into `event stream ended
  without a final result`). Empty-prefix rebuilds now fall back to
  transcript replay, and the verifier treats an empty file with zero
  expected records as sound.
- **`openai-codex` 404 on every request**: the Codex ChatGPT backend
  serves Responses under `/codex`, but the request URL was built as
  `{baseUrl}/responses`, so `https://chatgpt.com/backend-api/responses`
  answered `404 {"detail":"Not Found"}`. Both transports (SSE and the
  WebSocket path) now resolve the endpoint like TS pi's
  `resolveCodexUrl` — appending `/codex/responses`, tolerant of custom
  base URLs that already end in `/codex` or `/codex/responses`.
- **Provider endpoint URLs aligned with TS pi across the board** (a
  follow-up audit of every adapter against the upstream SDK semantics):
  - **Google Gemini 404/duplicated version path**: catalog models carry a
    version-inclusive `baseUrl` (`.../v1beta`), but the adapter appended
    another `/v1beta`. The adapter now treats an explicit `baseUrl` as
    version-inclusive (TS sets `apiVersion=""`), prefixes bare model ids
    with `models/` like the SDK's `tModel` (resource-rooted ids pass
    through verbatim, no percent-encoding), and falls back to
    `GOOGLE_GEMINI_BASE_URL` when no base is set.
  - **Google Vertex**: custom `baseUrl` is now honored in BOTH auth modes
    as a collection-scope base (`/v1` appended only when no `v<digits>` /
    `v<digits>beta<digits>` path segment is present), `{location}`-
    templated bases are ignored (TS `resolveCustomBaseUrl`),
    `GOOGLE_VERTEX_BASE_URL` overrides the default host, the
    multi-regional `us`/`eu` locations use the
    `aiplatform.<loc>.rep.googleapis.com` host, and third-party
    `publisher/model` ids expand to `publishers/<pub>/models/<id>`. ADC
    mode now requires `GOOGLE_CLOUD_LOCATION` like TS pi (the silent
    metadata-zone / `global` fallback was removed).
  - **Anthropic**: requests go to `/v1/messages?beta=true`, matching the
    SDK's beta surface used by TS pi.
  - **Azure OpenAI**: base-URL normalization now uses real URL parsing —
    azure-host detection is a hostname suffix match (no more substring
    false positives), the `/openai/v1` rewrite drops any query string,
    invalid bases are an explicit error, and empty
    `AZURE_OPENAI_BASE_URL` / `AZURE_OPENAI_RESOURCE_NAME` /
    `AZURE_OPENAI_API_VERSION` values are treated as unset (TS `||`
    semantics). The deployment-name map skips empty keys/values and stops
    the value at a second `=` like TS.
  - **Amazon Bedrock**: a standard `bedrock-runtime.<region>.amazonaws.com`
    base URL no longer overrides a configured `AWS_REGION` /
    `AWS_DEFAULT_REGION` or an ambient `AWS_PROFILE` — the endpoint is
    re-derived from the resolved region (TS
    `shouldUseExplicitBedrockEndpoint`), fixing SigV4-signed requests
    landing on the wrong regional host. Region resolution now matches the
    TS order (bedrock-service ARN → env → pinned standard base → active
    profile's `region` in `~/.aws/config` → `us-east-1`), and non-standard
    custom bases (VPC/proxy) stay pinned.

## [1.0.6] - 2026-09-30

### Added

- **`/ext` slash command**: inspect installed plugins from inside the TUI
  (`/ext` or `/ext list`) — id, load state (active/disabled/failed/
  policy-filtered), version, directory, contributed capabilities, and
  load warnings. Read-only snapshot of the session-start load; plugin
  management stays on the `tack ext` CLI.

### Fixed

- **Extension slash-commands no longer freeze the TUI for 30s**: a
  plugin whose command handler calls back into the host (`ui/notify`,
  `ui/select`, …) deadlocked against the UI loop — the loop awaited
  `commands/invoke` inline while the plugin's host request waited for
  that same loop, so nothing rendered until the 30s request timeout
  fired and the command reported `request timed out`. The dispatcher
  now runs the invoke on a spawned task and lands the result as an app
  event, keeping the loop free to answer plugin callbacks.

## [1.0.5] - 2026-09-30

### Added

- **Plugin development guide**: new `docs/plugin-development.md` (+
  zh-CN) — a hands-on tutorial across the three carriers (process /
  WASM / MCP) and the three SDK languages (Rust / TypeScript / Python):
  scaffolding with `tack ext new`, the same guardrail plugin written
  three ways, scenarios, the `extensionPaths` dev loop, and
  distribution. README and docs/features.md are synchronized with the
  v3 redesign (they still described the removed v1/v2 protocol).
- **First-class provider bridges (plugin redesign P7).** Plugins can
  serve inference **directly** — no HTTP shim between the user and the
  model. A plugin declaring `capabilities.provider.stream` registers a
  provider with `bridge: true`; its models get the reserved api kind
  `ext-provider-bridge`, appear in `/model`, and resolve like native
  providers in **all four run modes** (print/rpc/acp previously answered
  `host/registerProvider` with `ERR_METHOD_NOT_FOUND`). The streaming
  model fits the v3 peer: `provider/stream` is a fast ack, the turn's
  events ride plugin→host `provider/streamEvent` notifications demuxed
  by `streamId` with in-band terminal semantics (the `Provider`
  contract), and abort (Esc) rides `provider/streamCancel` with a 5s
  grace period before the host synthesizes a terminal error — carrier
  death and protocol violations fail-open the same way, so a
  misbehaving plugin degrades only its own provider. Bridge providers
  manage their own credentials (no `apiKey` crosses the wire); usage
  and cost pass through from the plugin's terminal message. Carrier
  matrix: process and WASI-stdio serve streams; the WIT component and
  MCP carriers reject bridge registrations (the WIT world is
  unchanged). Managed policy narrows the capability
  (`pluginPolicy.plugins."<id>".provider: false` → policy-blocked at
  load), everything is audited under the new `plugin_provider` tracing
  target (pinned at INFO in the managed auditSink filter), and load
  telemetry counts provider-bridging plugins. SDK parity: Rust
  (`PluginBuilder::provider_stream` / `ProviderEvents` /
  `ProviderStreamCx`), TypeScript (`.providerStream`), Python
  (`.provider_stream`), plus a new `on_ready` startup hook in all three
  (the registration entry point); `ext dev` scenarios gained the
  `providerStream` step (scripted cancel races included), `ext inspect`
  prints bridge registrations, and the demo plugin fixture grew a
  deterministic fake model. See docs/plugin-system.md §3.1d.
- **Provider events (`provider/event`, P7c).** The codebuddy-only
  rate-limit notifier generalized into a provider-wide channel:
  plugins (and native providers) surface rate limits and warnings by
  provider id — TUI shows the same inline notice plus a settings-gated,
  throttled desktop notification, headless modes log, and events are
  audited under `plugin_provider`. The codebuddy provider rides the
  generalized channel unchanged in behavior.

- **Plugin approval chain (`approval/review`) wired into the host.**
  When the built-in permission flow is about to prompt a human, active
  plugins that declared `capabilities.hooks.approvalReview` get first
  crack at the decision — load order, first-claim-wins, a null result
  passes to the next reviewer. Composition: deny rules → PreToolUse
  hook decisions → mode gate → allow rules/cache → **plugin approval
  chain** → PermissionRequest hooks → user prompt, so the chain only
  ever sees calls that would prompt. Claimed `allow`/`reviewed`
  approve one-shot (nothing persists into allow-always state; the two
  actions stay distinguishable in the audit event), `askUser` defers
  to the built-in prompt, and reviewer errors — including
  `unsupported_capability` from carriers without the method — degrade
  to pass (fail-open). Claims/passes are structured tracing events
  (target `plugin_approval`) for managed `auditSink` deployments.
  Wired in the TUI and rpc surfaces; acp/remote-host prompts are a
  documented follow-up. Resolves roadmap open question #6; see
  docs/plugin-system.md §3.1c.
- **Subagent plugin inheritance (`subagents.inheritPlugins`).**
  Subagent child loops used to see no plugin surfaces at all — a
  spawned subagent ran straight past guardrail plugins' `beforeToolCall`
  verdicts. The new settings key (`none | hooks | full`, default
  `hooks`) shares the parent's loaded plugins in-process: `hooks` runs
  plugin hook bridges inside the child (after its `permissions.deny`
  rules, matching surface chain order); `full` additionally adds plugin
  tools to the child tool set (agent-definition `tools` whitelists
  still narrow the combined set); `none` restores the legacy behavior.
  The key layers like any settings key, so managed settings can pin
  it. Resolves roadmap open question #5; see docs/features.md
  "Plugin inheritance".
- **Plugin redesign P6 — distribution + observability.** Curated
  marketplace startup sync: `pluginMarketplaces` (global/managed
  settings layers) declares catalogs kept fresh in the background —
  git transport degrading to the forge's https archive, fingerprint
  short-circuit, cross-process lock, backup/rename/swap activation,
  and a failing sync never blocks startup (`tack ext marketplace sync`
  is the manual form). Catalog v2 entries gain `installation`
  (`available | not-available | installed-by-default`, the latter
  auto-installed by the sync) and an inline `manifest` for rich
  listing; unknown entry keys warn and skip. Bundle archives are the
  air-gapped unit: `tack ext bundle pack` writes a deterministic
  `<name>-<version>.tgz`, and `tack ext install <file.tgz>` extracts
  it under hostile-input rules (no links, no traversal, size caps)
  into the normal store. Load telemetry emits counts by outcome
  (`active | disabled | failed | policy-filtered`) with error classes
  (`manifest | handshake | register | policy | store`, target
  `plugin_load`) and persists `extensions/last-load.json`, which
  `tack doctor` reads to report load failures with causes, lock
  drift, policy-filtered entries, and WASM component support. The
  metrics sidecar lets Level-3 plugins emit telemetry untrusted: a
  declare-at-initialize schema (validated all-or-nothing), a
  host-provided scratch file (WASI-stdio WASM: dedicated audited
  preopen), and strict drain validation (≤64 KiB / ≤100 lines, exact
  dimension sets, finite values, dedup) before measurements enter
  telemetry with plugin attribution (target `plugin_metrics`); the
  Rust SDK gains `MetricsRecorder`. See docs/extensions.md §5, §10,
  §11.
- **Plugin redesign P5 — enterprise plugin policy.** The managed
  settings layer gains a `pluginPolicy` key: `managedPluginsOnly`
  restricts loading to plugins explicitly named in the managed
  `plugins` map; `allowedSources` is a source allow-list (`git` exact
  URL with optional `ref` pin, `hostPattern` regex over the source
  host, `local` directory roots); per-plugin `enabled` wins over the
  user/project layers in both directions, and per-plugin
  `tools`/`mcpServers` are narrow-only intersections with what the
  plugin registers. Policy is enforced twice: at install time (source
  check before any clone/network access; `managedPluginsOnly` and
  managed-disabled checks after manifest parse, before activation;
  `ext upgrade` re-checks) and at load time (discovery filter with a
  lockfile/dir origin backstop, plus registration-time narrowing).
  Blocked plugins stay visible as rows in `tack ext list`
  (`policy-blocked (<reason>)`), every decision is audit-logged with
  the rule and its origin layer (shipped via the managed `auditSink`
  when configured), and `tack ext enable|disable` warns when the
  managed layer pins the opposite value. See docs/extensions.md §9.
- **Plugin redesign P4 — Level-2 MCP server plugins and the WIT
  component WASM carrier.** An `extension.json` with `carrier: "mcp"`
  and one `mcpServer` entry (mcp.json shape: stdio, Streamable HTTP, or
  legacy SSE) makes an MCP server *the plugin* — no tack-RPC process is
  spawned. The host adapts the server's tools, `list_resources` /
  `read_resource` meta-tools, and `prompt__<name>` prompt tools into the
  plugin's capability list with full plugin identity: `ext__<plugin>__
  <tool>` naming, attribution, policy, hook interception, and the
  untrusted-content defense (results wrapped in `<untrusted_content>`,
  permission elevation) apply exactly as for config-file MCP servers.
  stdio servers run with the extension directory as cwd; elicitation
  follows the run mode (sampling is a documented non-goal for plugin
  connections).
- **WIT component WASM carrier** (`tack:plugin@0.3.0`,
  `protocol/wit/tack-plugin.wit`): `carrier: "wasm"` now auto-detects
  the module format — WIT components are driven via typed
  `tack:plugin/tools` / `tack:plugin/hooks` exports with JSON-string
  payloads (the OpenRPC schema stays the single source of truth), while
  WASI-stdio core modules keep the debug-carrier path unchanged.
  Components are capability-free by construction (the world imports no
  WASI; fs/env/args grants are ignored with a warning), calls are
  synchronous with per-call fuel/wall-clock/memory limits, and interface
  subsets are valid plugins (a hooks-only component exports just
  `tack:plugin/hooks`). New example:
  `examples/extensions/hello-component/` (hand-written component WAT).
- Plugin tools may now return image blocks to the model (previously
  text-only), for MCP and tack-RPC carriers alike.

### Changed

- **web_search defaults to Bing and the keyless backends back each
  other up.** The default `webSearch.provider` is now `bing` (was
  `duckduckgo`), and the two keyless scrape backends (`bing` /
  `duckduckgo`) automatically fall back to each other when the
  configured one is unreachable from the current network or returns an
  empty (bot-walled) page — duckduckgo.com is blocked outright on some
  networks, which previously made web_search dead on arrival there.
  Keyed backends (`brave`/`tavily`/`exa`) are unchanged.
- **Plugin system rework (redesign P3): the ExtensionManager now speaks
  tack-RPC v3** and the v1/v2 NDJSON protocol is removed. Plugins are
  identified as `name@source` (marketplace name, or reserved
  `user`/`project`/`local`); installs land in a versioned store
  (`~/.tack/agent/extensions/store/<source>/<name>/<version>/`, active =
  `local` else highest semver) with atomic stage-verify-swap-rollback
  installs and fingerprint-idempotent upgrades. The lockfile is v2 (v1
  auto-upgraded in memory: bare keys become `name@user`); legacy flat
  `extensions/<name>/` installs keep loading. Load failures are
  first-class state (`LoadedPlugin{id, enabled, error}` — broken or
  disabled plugins are visible in `ext list` instead of vanishing). New
  CLI: `tack ext enable|disable <id>` (persisted as settings
  `plugins."<id>".enabled`), `tack ext upgrade [id]`, richer `ext list`
  (id/state/version/layout). New manifest fields: `version` (semver,
  becomes the store version), `failMode` (hook failures block). Git
  installs/upgrades run with a scrubbed git environment.
- Deliberate surface reductions with the v3 switch: plugin-registered
  shortcuts, `ui.set_status`, and the v1 `session.*` control methods are
  gone (v3 keeps `session/get` + `session/sendUserMessage`); lifecycle
  event names are camelCase now (`agentStart`, `turnEnd`, …). The hidden
  `tack ext-demo-plugin` command and the v1 example extensions
  (hello-js, git-checkpoint, handoff, protected-paths) were removed; the
  WASM examples were ported to v3.

### Added

- tack-RPC v3 groundwork (plugin system redesign P0, see
  `docs/plugin-roadmap.md`): `protocol/tack-rpc.openrpc.json` is the new
  schema-first definition of the host↔plugin protocol (JSON-RPC 2.0,
  capability namespaces), and `cargo run -p xtask -- codegen` generates
  the Rust types (`tack_ext::rpc3`) from it, with a CI freshness check.
  Existing v1/v2 plugins are unaffected — the v3 protocol is not wired
  into the host yet.
- tack-RPC v3 host core and Rust SDK (redesign P1):
  `tack_ext::v3::{JsonRpcPeer, HostClient}` implements the transport-
  agnostic JSON-RPC 2.0 peer (both-directions requests, `$/cancelRequest`,
  timeouts, dead-peer semantics) plus the typed host-side client for every
  capability namespace, and the new `tack-ext-sdk` crate lets Rust plugin
  authors build a Level-3 plugin with a builder API (tools, commands,
  before/after hooks, context transform, approval review, lifecycle
  events, widgets, autocomplete, config/metrics declarations, and a typed
  host client for ui/exec/session/snapshot/config services) without ever
  seeing an envelope. See `crates/tack-ext-sdk/examples/hello_rpc3.rs`.
  The v1/v2 protocol is still the live one; the ExtensionManager switch
  and v1/v2 removal land with the loader rework.
- tack-RPC v3 language SDKs and dev tooling (redesign P2):
  - `sdk/typescript` (`@tack/plugin`) and `sdk/python` (`tack-plugin`):
    zero-dependency SDKs with the same builder/handler surface as the
    Rust SDK; their protocol types are generated from the OpenRPC schema
    by `xtask codegen` (freshness-checked in CI, plus node/python test
    jobs).
  - `tack_ext::v3::V3Process`: the v3 process carrier (spawn, sensitive
    env stripping, stderr forwarding, graceful shutdown).
  - New dev subcommands speaking v3 directly: `tack ext new <dir>
    <rust|ts|python>` (scaffold), `tack ext inspect <dir>` (handshake +
    capability dump), `tack ext dev <dir> [scenario]` (run against a JSON
    scenario, or stream plugin logs), `tack ext test <dir> [scenario]`
    (assertions with recursive subset matching and scripted
    plugin→host answers; non-zero exit on failure).

### Fixed

- **Plugin security hardening (second post-redesign review).**
  - ACP sessions ordered plugin `beforeToolCall` bridges AFTER the
    permission layer, so a plugin could rewrite a command the human had
    already approved (and past `permissions.deny`). Plugin bridges now
    run BEFORE deny rules and the prompt in ACP too — every surface
    (TUI/rpc/print/acp/subagent) sees the final post-rewrite arguments.
  - Provider-bridge trust boundary: plain (non-bridge)
    `host/registerProvider` now requires the new declared
    `capabilities.provider.register` + managed policy (previously
    ungated — any plugin, even a sandboxed one, could register HTTP-shim
    providers); registrations whose id collides with a built-in
    provider are rejected (a plugin can no longer shadow e.g.
    `anthropic` and redirect its traffic); `apiKeyEnv` is no longer
    resolved from the HOST environment for plugin-registered providers;
    plain registrations are revoked when the plugin dies or is shut
    down; `provider/event` notifications are accepted only for provider
    ids the emitting plugin actually registered. SDK parity: all three
    SDKs gained a `provider_register`/`providerRegister` builder knob.
  - The WIT component carrier now resolves the version-qualified
    interface names real toolchains emit (`tack:plugin/tools@0.3.0`,
    falling back to the bare names for hand-written guests) —
    wit-bindgen/cargo-component guests actually load now — and a guest
    built for a different WIT version gets an error naming both
    versions instead of a misleading "exports neither" failure.
  - Managed-control-plane escape hatches closed: `TACK_MANAGED_SETTINGS`
    is honored only in debug/test builds (release binaries ignore it),
    and the project `.pi/settings.json` `plugins."<id>".enabled` map is
    only read for trusted projects — an untrusted clone can no longer
    disable your guardrail plugins. Managed policy can now also strip a
    plugin's `hooks` capability (`pluginPolicy.plugins."<id>".hooks:
    false`) while keeping its tools, and the `mcpServers` narrowing
    list matches an MCP-carrier plugin by id or bare name.
  - Approval integrity: rpc mode honors PreToolUse
    `permissionDecision: "ask"`/`"allow"` verdicts (previously recorded
    into a map nobody read, letting the plugin approval chain approve a
    call a shell hook had escalated); allow-always approvals for
    `ext__*` tools now bind the loaded plugin version — a plugin
    upgrade invalidates stale approvals instead of applying them to
    unreviewed code (permissions.json gained an additive
    `extToolVersions` key; legacy ext__* entries are ignored).
  - v3 protocol robustness: plugin tool calls are no longer killed by
    the 30s default RPC timeout (they run until the turn cancels them,
    and cancellation now reaches the plugin via `$/cancelRequest`);
    graceful shutdown of a hung plugin costs ~5s instead of ~32s and
    plugins shut down concurrently; a write failure on the wire marks
    the peer dead instead of desynchronizing NDJSON framing; inbound
    requests/notifications are concurrency-capped (a flooding plugin is
    disconnected); requests with unparseable ids get `-32600` instead
    of being silently downgraded to notifications; duplicate request
    ids are rejected; plugin-provided result `details` survive on
    text-only tool results; `transformContext` serialization failure
    leaves the context unchanged instead of silently truncating it.
  - SDK robustness: Python plugins no longer die on NDJSON lines over
    64 KiB (asyncio default limit — now 16 MiB like Rust/TS); the
    TypeScript peer survives write failures (was an unhandled
    rejection = process crash) and ignores non-object lines like the
    other peers; the Rust SDK answers pre-`initialize` requests with a
    protocol error instead of panicking-and-dropping.
  - Distribution hardening: concurrent same-process installs no longer
    share one `.staging-<pid>` directory; a corrupt
    `extensions-lock.json` fails CLOSED when `extensionLockRequired`
    is set and is backed up before any rewrite (pins survive);
    marketplace http client re-validates every redirect hop (no
    https→http downgrade) and archive-fallback sync tries the
    remaining URL/ref candidates when one serves an undecodable body;
    extracted archive file modes are masked to `0o755` (no
    world-writable plugin code); plugin metrics declarations cap the
    operation count; MCP-carrier plugin servers no longer inherit the
    host's credential environment variables (parity with the process
    carrier); a WASM manifest `module` path can no longer escape the
    extension directory; `tack ext remove --local` rejects path-like
    names; `ext dev` rejects a non-object `providerStream` step
    instead of panicking.
- **Plugin security hardening (post-redesign review).**
  - Pinned marketplaces now REFUSE an unsigned replacement catalog
    (previously only warned) — a signature-stripping downgrade can no
    longer void the TOFU pin and push code via `installed-by-default`.
  - The plugin approval chain no longer approves mutating calls once
    untrusted web/MCP content entered the context (the human is asked,
    matching the distrust already applied to allow rules/allow-always),
    and a PreToolUse `permissionDecision: "ask"` verdict now forces the
    human dialog past the chain. `askUser` no longer terminates the
    chain: every reviewer is consulted, the first `allow`/`reviewed`
    claim wins.
  - Plugin `hooks/beforeToolCall` bridges now run BEFORE the permission
    layer in every surface (TUI/rpc/print/subagent), so declarative
    deny rules, the mode gate, the approval chain, and the prompt
    dialog all see the FINAL post-rewrite arguments — a plugin rewrite
    can no longer smuggle content past deny rules or an approval.
  - MCP-carrier plugins now time out like the process carrier (30s per
    call, 15s connect+initialize): a wedged MCP server fails the call
    instead of hanging the agent turn or the whole startup. ACP turns
    wrap MCP-carrier plugin (and config-file MCP) tool output as
    untrusted and re-prompt for mutating calls, closing the ACP gap in
    the prompt-injection defense.
  - Managed plugin policy: a malformed managed-settings file now logs a
    loud warning that the policy is INACTIVE (previously silent);
    managed audit targets (`plugin_policy`/`plugin_approval`/
    `plugin_metrics`/`plugin_load`) are pinned at INFO for the managed
    auditSink so a user-set log level cannot suppress them; `ext
    upgrade` re-checks the per-plugin policy gate, not just the source
    list; `hostPattern` rules match credential-embedded and `ssh://`
    git URLs (userinfo is stripped before matching); install-time deny
    audit events carry the plugin/source in `subject`.
  - The extensions lockfile is written atomically and read-modify-write
    sequences take a cross-process guard — concurrent tack processes
    (e.g. background marketplace default-installs vs `ext install`)
    can no longer tear the JSON or lose each other's entries.
  - All plugin-store git invocations (clone/checkout/rev-parse/status)
    now scrub the inherited git environment (`GIT_DIR` and friends):
    running tack from a git hook no longer misreads other repos, which
    previously recorded wrong lockfile commits and falsely reported
    checkout drift for every git-installed plugin at startup.
  - Plugin metrics scratch files are unique per live session/process
    (concurrent sessions no longer truncate or double-drain each
    other's sidecar), the shutdown drain flushes a trailing partial
    line instead of dropping it, and ACP sessions shut their plugins
    down (graceful stop + final drain) when the client connection ends.
  - Marketplace sync: tag/sha-pinned git catalogs fingerprint cheaply
    again (a sha pin needs no network round-trip at all) instead of
    full-cloning every startup; `catalog.json?token=…` URLs classify
    correctly; a missing live catalog self-heals from its `.bak`;
    stale sync locks are reclaimed race-free; `ext marketplace add`
    caps the catalog download; leaked `.staged-*.json` files no longer
    appear as phantom marketplaces.
  - Smaller correctness fixes: `tack ext bundle pack` rejects path-like
    manifest versions instead of writing outside the cwd; duplicate
    sanitized plugin tool names now warn and skip instead of silently
    shadowing; a disabled plugin with a broken manifest reports
    "disabled" instead of a spurious "failed"; the v3 peer no longer
    leaks completed incoming-request handles and reports local
    cancellation distinctly; BOM-saved component WAT routes to the
    component carrier; dead-at-handshake MCP/component carriers report
    Dead instead of registering as loaded.
- `tack ext new`'s scaffold README no longer claims the session loader
  still speaks v1/v2 (it has spoken v3 since P3); it now points at
  `extensionPaths` for the live dev loop and at the new guide.

## [1.0.4] - 2026-09-27

### Added

- `ask_user` multi-select questions: setting `multi_select: true` on a
  multiple-choice question turns the TUI dialog into a pick-several
  checkbox list (space toggles, enter confirms, at least one option
  required; no "Other…" free-text escape). The tool result reports all
  selected labels as the answer.

### Fixed

- `ask_user` now tolerates multiple-choice options sent as bare strings
  (`["A", "B"]`): argument preparation rewrites them into option objects
  (`[{"label": "A"}, ...]`) before schema validation, so a well-meant tool
  call is no longer rejected with `"A" is not of type "object"`.
- Markdown tables in the TUI now render with full box-drawing borders,
  matching TS pi: a top border (`┌─┬─┐`), a horizontal separator under the
  header and between every body row (`├─┼─┤`), and a bottom border
  (`└─┴─┘`). Previously only vertical bars and a single plain rule under
  the header were drawn, so tables had no horizontal lines. Cells are now
  padded with a space on both sides (`│ cell │`), keeping the border
  junctions aligned with the interior bars.

## [1.0.3] - 2026-09-27

### Added

- `ask_user` tool: the agent can pause mid-run and ask the user structured
  questions — 1–4 per call, multiple-choice (2–4 options with descriptions,
  plus an "Other…" free-text escape) or free-text when options are omitted.
  The TUI walks the questions one dialog at a time; Esc dismisses the batch
  and the tool result tells the model to decide on its own. Headless modes
  (print/rpc/acp/serve) and subagents register the tool without an
  interactive handler, so calling it there returns an in-band "no
  interactive user" message instead of blocking forever (same policy as MCP
  elicitation decline).

## [1.0.2] - 2026-09-27

Repository relocation plus a bilingual documentation restructure — no
functional changes. The project moved from `sufar/tack` to `Tack-AI/tack`
after the GitHub account rename.

### Changed

- `tack update` now defaults to the new `Tack-AI/tack` release repository
  (`TACK_UPDATE_REPO` and the `updateRepo` setting still override). Older
  binaries keep finding updates through GitHub's username-rename redirect.
- All repository references — README, SECURITY.md, docs, ACP registry
  metadata, CI examples — point at the new location.
- Every document under `docs/` now ships in two languages: `foo.md` is the
  English canonical, `foo.zh-CN.md` the Chinese version (previously most
  docs were Chinese-only). Cross-links, nav headers, and the
  `README.zh-CN.md` doc index were updated to match.

### Added

- `AGENTS.md`: repo guidance for AI coding agents (layout, CI-matched
  commands, testing rules, compatibility constraints).

## [1.0.1] - 2026-09-27

v4 session storage upstream-alignment fixes and pi interoperability.

### Fixed

- v3→v4 migration: a compaction's `systemMessage` is now preserved on the
  rebuilt-tail path too (previously only on checkpoint compactions).
- v3→v4 migration: the imported usage row now sums the optional
  `cacheWrite1h`/`reasoning` token classes, matching upstream `addUsage`.
- v3→v4 migration: records whose payload no longer fits the typed schema
  (unknown message roles, missing fields) are preserved as
  payload-carrying custom entries instead of aborting the whole
  migration; a record needed by a compaction tail still contributes its
  raw message payload.
- v3→v4 migration: compactions with neither a checkpoint tail nor a
  reachable `firstKeptEntryId` now fail loudly instead of silently
  producing an empty (context-truncating) tail.
- v3→v4 migration: unparseable timestamps fall back to the nearest known
  time instead of aborting; empty-string session names are skipped and
  empty-string labels count as cleared (upstream truthiness rules);
  non-string `parentId` is a hard error instead of a silent re-root;
  pass 2 re-verifies the header identity.
- v4 message payloads: `branchSummary` messages encode a root source as
  `fromId: null` (the v3 entry-level `"root"` sentinel no longer leaks
  into v4 retained tails); pre-fix files remain readable.
- Forks of pi-written sessions: upstream `pi.*` namespaces now follow
  upstream projection rules — operation/pending/result state is excluded
  and lane state resets to idle instead of crossing the fork verbatim.

### Added

- `pi.*` read fallback: opening a pi-written v4 session resumes from
  pi's branch tip, lane configuration, session name and labels
  (`tack.*` rows win once present; tack never writes `pi.*`).
- `docs/session-v4-protocol.md`: the normative message-v4 JSONL
  wire-format specification.

## [1.0.0] - 2026-09-26

First stable release. Tack is a Rust reimplementation of the TypeScript pi
coding agent — wire- and storage-compatible, with the same session files
(transparent v1–v4 migration), RPC/ACP wire protocols, provider registry and
model catalog, and CLI flags.

### Highlights

- Interactive TUI, headless print mode, ACP editor integration (Zed,
  JetBrains, …), JSONL RPC mode.
- Remote sessions: `tack serve` over TCP/WebSocket/TLS with token auth;
  attach from `tack client` or the embedded browser client.
- 42 built-in providers (40 mirroring TS pi, plus zero-config local ollama
  and llama.cpp) with the full 1130-model catalog embedded.
- Background tasks, LSP navigation & diagnostics, file checkpoints,
  persistent memory, sub-agents with worktree isolation, cross-session
  search, cron, OS sandbox, declarative permissions, MCP client and server,
  eval framework.
- Extensible via subprocess plugins (NDJSON/JSON-RPC) or sandboxed WASM,
  plus Claude-Code-compatible lifecycle hooks.
- `tack update` self-update from `tack-v*` GitHub releases; private by
  default (no telemetry).


