# Compatibility & Versioning Policy

**English | [简体中文](compatibility.zh-CN.md)**

This document defines which Tack interfaces are stable, how they are
versioned, and what guarantees integrators (editor plugins, remote clients,
extension authors, hook writers) can rely on. It is the normative reference
when deciding whether a change is "breaking".

Audience: Tack contributors and anyone building against Tack's external
surfaces. Code paths cited below are evidence of the *current* state; the
*commitment* column is the forward-looking contract.

---

## 1. Stability tiers

Tack interfaces fall into three tiers:

| Tier | Meaning | Interfaces |
|---|---|---|
| **Stable / append-only** | Never removed; new capabilities are added in a backward-compatible way. Old data stays readable forever. | Session file read path (formats v1–v4), `settings.json` keys, CLI flags |
| **Versioned protocols** | Carry an explicit version number (or inherit one from an upstream spec). Breaking changes bump the version and follow the protocol-bump procedure in §4.3. | CBOR remote protocol (tack-protocol), extension protocol (tack-ext / tack-ext-wasm), RPC mode, ACP, MCP, Claude-Code-compatible hooks |
| **Internal / no guarantee** | May change in any release without notice. Do not build against these. | Rust crate APIs of all workspace crates, internal agent event types, TUI internals |

The Rust workspace crates are **not published to crates.io** and expose no
semver commitment (§6). "Tack" as a versioned product means the `tack`
binary and its externally observable behavior.

---

## 2. Interface inventory

### 2.1 Session storage format (tier: stable / append-only)

**Current state (evidence).**

- The current on-disk format is **transactional format v4**: a JSONL
  transaction log with a `{"kind":"header","v":4,"storage_version":1,...}`
  first line. Format constants live in
  `crates/tack-session/src/v4/types.rs` (`V4_FORMAT_VERSION = 4`,
  `V4_STORAGE_VERSION = 1`); the normative wire-format specification is
  `docs/session-v4-protocol.md`, and format decisions and deliberate
  divergences from upstream TS pi are documented in
  `crates/tack-session/V4_NOTES.md`.
- **Version probing** is first-line sniffing in
  `crates/tack-session/src/v4/codec.rs` (`parse_session_header`): a v4
  storage header is recognized by `kind == "header" && v == 4`; a legacy
  header by `type == "session" && version == 3`. v1 files carry no version
  field and v2/v3 share the legacy header shape.
- **Transparent migration on open** is implemented in two places:
  - legacy chain v1→v2→v3, in place, in
    `crates/tack-session/src/manager.rs` (`migrate_to_current`,
    `CURRENT_SESSION_VERSION = 3` in `crates/tack-session/src/entry.rs`);
  - streaming legacy v3→v4 in `crates/tack-session/src/v4/migrate.rs`
    (`migrate_v3_to_v4`): two bounded-memory passes, crash-safe (original
    copied to `<path>.bak`, new content staged in a temp file and
    atomically renamed), encryption state preserved.
- **Write paths.** The default backend `SessionBackend::JsonlV4` writes
  only v4. The legacy v3 write path is retained as byte-compatible escape
  hatch: `sessionBackend: "v3"` in settings selects it
  (`SessionBackend::from_setting`, `crates/tack-session/src/manager.rs`).
  Opening a v4 file with the v3 backend is a hard error, not silent
  corruption (`SessionError::V4FileWithV3Backend`).
- **`pi.*` interoperability.** Upstream pi writes the same v4 layout under
  the `pi.` namespace prefix. Tack never writes `pi.*`, but opening a
  pi-written session honors its rows as read-only fallbacks (branch tip,
  lane configuration, session name, entry labels; `tack.*` rows win once
  present), and forks apply upstream's own projection rules to `pi.*`
  namespaces (`crates/tack-session/src/fork_policy.rs`,
  `crates/tack-session/src/v4/store.rs`).
- Unknown or future entry types — and retained records whose payload no
  longer fits the typed schema (unknown message roles, missing fields) —
  are preserved as v4 *custom* entries on migration rather than rejected
  or aborted — Tack's no-data-loss principle
  (`crates/tack-session/src/v4/migrate.rs`).
- `crates/tack-session/src/sqlite_backend.rs` is an **experimental** backend
  (`sessionBackend: "sqlite"`), not covered by the guarantees below.

**Commitment.**

- The read path for every session format ever written (v1, v2, v3, v4) is
  **permanent**. Migration code is never removed: session files are user
  data, and a file written by any historical Tack (or TS pi) release must
  remain openable. This is the strongest guarantee in this document.
- New sessions are written in the current format only. Within a format,
  evolution is **append-only**: new entry types and new optional fields may
  be added; existing fields never change meaning. Alignment fixes that
  bring the wire shape closer to the documented protocol — e.g. a root
  `branchSummary.fromId` serialized as `null` in v4 message payloads
  (the v3 `"root"` sentinel stays confined to v3 records) — are bug
  fixes, not format changes: files written before the fix remain
  readable.
- If a future v5 format is ever introduced, the v1–v4 read/migrate chain
  is extended, not replaced, and a byte-compatible write-path escape hatch
  equivalent to `sessionBackend: "v3"` ships in the same release that
  flips the default.
- The `sessionBackend: "v3"` legacy write path is retained as long as
  interop with tools that parse v3 JSONL byte-for-byte is a stated
  supported scenario; its removal (if ever) is a breaking change under §4
  and requires a deprecation window.

### 2.2 CBOR remote protocol — `tack-protocol` (tier: versioned)

**Current state (evidence).**

- Framed CBOR: 4-byte big-endian length prefix + CBOR payload
  (`crates/tack-protocol/src/framing.rs`), 16 MiB max frame
  (`DEFAULT_MAX_FRAME_LENGTH`), CBOR nesting capped at 128 levels
  (`MAX_CBOR_NESTING_DEPTH`). Byte-compatible with TS pi's
  `packages/protocol/src/framing.ts` / `codec.ts`.
- **Version**: `PROTOCOL_VERSION: u32 = 1` in
  `crates/tack-protocol/src/schemas.rs`. Schemas match upstream
  `packages/protocol/src/schemas.ts` field-for-field.
- **Version negotiation exists**: `RemoteClient::connect`
  (`crates/tack-protocol/src/client.rs`) sends `Hello { version }` and
  inspects the server hello. A server speaking a **newer** protocol is
  rejected with the reserved `ProtocolErrorCode::Version` error and an
  "upgrade the client" message; an **older** server is accepted — the wire
  format is additive, unknown fields are ignored, and unknown enum
  variants decode into an `Unknown` catch-all. Versions are ordered
  integers, not semver.

**Commitment.**

- Protocol v1 evolves additively: new optional fields, new command/event
  variants (with catch-all tolerance) are v1-compatible changes and ship
  in minor releases.
- A wire-breaking change requires the protocol-bump procedure (§4.3): a
  new `PROTOCOL_VERSION` constant, version-negotiated dual-stack
  operation during the transition, and a clear error for mismatched peers
  — never silent misbehavior.

### 2.3 RPC mode (tier: versioned — de facto v1)

**Current state (evidence).**

- `tack rpc` (also `--mode rpc`, `crates/tack-app/src/main.rs`): JSONL
  commands on stdin, JSONL responses + events on stdout, wire-compatible
  with TS pi's `pi --mode rpc`
  (`crates/tack-app/src/rpc/mod.rs`).
- The command set is defined in the `handle_command` match and the
  introspectable `RPC_COMMANDS` constant (34 commands: `prompt`, `steer`,
  `abort`, `fork`, `compact`, `bash`, `get_state`, `get_commands`, …);
  unknown commands get a structured `unknown or unsupported command`
  error.
- **There is no protocol version field** in the RPC wire format today;
  clients discover capabilities via `get_commands`.

**Commitment.**

- The existing RPC wire shape (command names, `id`/`type`/`command`/
  `success` response envelope, event stream) is stable for the v1 line.
- New commands and new optional command/response fields are additive and
  ship in minor releases; clients should tolerate unknown event types.
- Renaming or removing a command, or changing a response field's meaning,
  is breaking (§4) and requires either a deprecation window or an explicit
  RPC protocol version field introduced by the same change.

### 2.4 ACP — Agent Client Protocol (tier: versioned, upstream-following)

**Current state (evidence).** `crates/tack-app/src/acp/` implements the
[Agent Client Protocol](https://agentclientprotocol.com) via the
`agent-client-protocol` crate at **0.9.5** (`crates/tack-app/Cargo.toml`,
with `unstable_session_model`, `unstable_session_usage`,
`unstable_session_info_update` features) and negotiates
`agent_client_protocol::ProtocolVersion::LATEST`
(`crates/tack-app/src/acp/agent.rs`). The protocol version is defined by the
upstream crate/spec, not by tack.

**Commitment.** Tack tracks stable upstream ACP releases and negotiates
the version per the ACP spec. ACP-side breaking changes arrive only via
upstream crate upgrades, are called out in the CHANGELOG, and are tracked
in `docs/upstream-alignment.md` (§5).

### 2.5 Extension protocols — tack-RPC & the WASM carrier (tier: versioned)

**Current state (evidence).**

- Plugins (process or WASI carrier) speak **tack-RPC v3**: JSON-RPC 2.0
  over NDJSON stdio, defined by the OpenRPC document
  `protocol/tack-rpc.openrpc.json` (the single source of truth; Rust types
  in `crates/tack-ext/src/rpc3.rs` and the TS/Python SDK types are
  generated from it by `cargo run -p xtask -- codegen`, freshness-checked
  in CI).
- **Version handshake**: the host sends `initialize` with a semver
  `protocolVersion` ("3.0.0"); compatibility requires the same major
  version and a minor not newer than the host's
  (`crates/tack-ext/src/v3/mod.rs`).
- The v1/v2 NDJSON protocol (`{"type":"request|response|event"}`) was
  **removed** in the v3 release: the host no longer speaks it, and the
  lifecycle surface was deliberately reduced (`session/*` is
  `session/get` + `session/sendUserMessage`; plugin-registered shortcuts
  and `ui.set_status` were dropped; lifecycle event names are now
  camelCase).
- Lockfiles: `extensions-lock.json` v1 is upgraded in memory on read
  (bare-name keys become `name@user`, `store: false`) and rewritten as v2
  on the next install/upgrade. The legacy flat `extensions/<name>/`
  layout keeps loading as `name@user`; new installs land in the versioned
  store (`extensions/store/<source>/<name>/<version>/`).
- **Level-2 MCP server plugins** (`extension.json` `carrier: "mcp"` +
  one `mcpServer` entry, same shape as `mcp.json`): the host connects to
  the declared MCP server (stdio / Streamable HTTP / legacy SSE) and
  adapts its tools, resource meta-tools, and prompt tools into the
  plugin's capability list. No tack-RPC wire change: the adaptation is
  host-internal (`PluginConnection`), and plugin policy/interception
  treat the tools exactly like tack-RPC plugin tools.
- **WIT component WASM carrier** (`carrier: "wasm"`, auto-detected from
  the module format): components export `tack:plugin/tools` and/or
  `tack:plugin/hooks` from
  [`protocol/wit/tack-plugin.wit`](../protocol/wit/tack-plugin.wit)
  (`tack:plugin@0.3.0`). Payloads are JSON strings carrying the rpc3
  types — the OpenRPC document stays the single schema source. The WIT
  package version is the contract's version handle (additive = minor
  bump; breaking = new package version). The WASI-stdio core-module
  carrier remains supported as the debug carrier.
- **Enterprise plugin policy** (managed settings `pluginPolicy`):
  `managedPluginsOnly`, the `allowedSources` source allow-list
  (git/hostPattern/local), and per-plugin `enabled` (managed wins over
  the user/project layers) plus narrow-only `tools`/`mcpServers`
  intersections. Enforced at install time (before clone/network and
  before activation) and at load time (discovery filter + registration
  narrowing); blocked plugins stay visible as rows, and decisions are
  audit-logged with rule and layer. The key landed in its final form —
  no v1→v2 migration is planned.

**Commitment.**

- tack-RPC is additive within a major version: new methods, notifications,
  and optional fields are minor-release changes on both sides (peers must
  tolerate unknown methods/notifications and unknown fields).
- A breaking wire change bumps the major protocol version and follows
  §4.3; the semver handshake (same major, peer minor ≤ host minor) is the
  compatibility mechanism and must keep working for mixed-version pairs.
- The WIT package follows the same additive rule within
  `tack:plugin@0.x`: new interfaces or new functions on `host` are
  additive (guests export subsets, and imports they don't use are not
  required); removing or retyping an export is breaking and bumps the
  package.

### 2.6 Hooks (tier: versioned, upstream-following)

**Current state (evidence).** Tack hooks are **Claude Code compatible**
(`docs/hooks.md`): the nested event config schema
(`matcher` + `hooks[]`), handler types `command` / `prompt` / `agent`, the
command-handler stdin JSON (`session_id`, `transcript_path`, `cwd`,
`hook_event_name`, …), the verdict-JSON stdout schema
(`decision`, `hookSpecificOutput.permissionDecision`,
`updatedInput` partial-merge semantics, `additionalContext`,
`systemMessage`, `continue`/`stopReason`), and the exit-code conventions
(exit 2 = block with stderr as reason; other non-zero = warning).
The Tack legacy flat hook format is auto-accepted. All hook failures are
fail-open.

**Commitment.** The compatible target is Claude Code's stable hook schema
(command handler I/O contract and settings shape). Tack follows upstream
Claude Code schema evolution: upstream additions are adopted additively;
divergences are documented in `docs/hooks.md`. Changes to this surface
follow the upstream-tracking policy in §5.

### 2.7 MCP — Model Context Protocol (tier: versioned, upstream-following)

**Current state (evidence).** MCP client and server support lives in
`crates/tack-app/src/mcp_config.rs`, `mcp_serve.rs`, `mcp_elicitation.rs`,
`mcp_oauth.rs`, `mcp_sampling.rs`, built on the `rmcp` crate at **3.1.4**
(root `Cargo.toml`). Transport support references spec **2024-11-05**
(legacy SSE) and streamable HTTP (`mcp_config.rs`); the wire protocol
version is negotiated per the MCP spec by the upstream crate.

**Commitment.** Same as ACP: follow stable upstream `rmcp`/MCP spec
releases, negotiate versions per spec, note upgrades in the CHANGELOG and
`docs/upstream-alignment.md`.

### 2.8 `settings.json` keys (tier: stable / append-only)

**Current state (evidence).** Settings are global
`~/.tack/agent/settings.json` + project `.pi/settings.json`, deep-merged,
project wins (`crates/tack-app/src/settings.rs`). The loader explicitly
**preserves unknown keys** and ignores them in typed accessors — the
additive convention is structural, not just aspirational. There is no
key-rename alias/migration machinery today; keys have only ever been
added (see CHANGELOG history, e.g. `memoryDirectory`,
`microcompact.minSavingsChars`, `sessionBackend`).

**Commitment.**

- Existing keys never change meaning, type, or default in a
  backward-incompatible way. New keys are additive and optional with a
  documented default.
- If a key must be renamed, the old key is kept working as a deprecated
  alias for at least one minor cycle with a warning, and the alias
  mapping is documented here and in `docs/configuration.md`.
- Removing a setting's effect is a breaking change under §4.

### 2.9 CLI flags (tier: stable / append-only)

**Current state (evidence).** Flags are defined with clap in
`crates/tack-app/src/main.rs` (`--print`, `--continue`, `--resume`,
`--session`, `--model`, `--mode`, subcommands `rpc`/`acp`/`serve`/…).
`--mode rpc` is a documented alias for the `rpc` subcommand.

**Commitment.** Existing flags and subcommands keep their meaning; new
flags are additive. Renaming or removing a flag, or changing a flag's
value semantics, is breaking (§4) and follows the deprecation-window
rule (alias + warning for one minor cycle where technically feasible).

### 2.10 Rust crate APIs (tier: internal / no guarantee)

**Current state (evidence).** The workspace's nine crates
(`tack-ai`, `tack-agent-core`, `tack-session`, `tack-tools`, `tack-app`,
`tack-protocol`, `tack-tui`, `tack-ext`, `tack-ext-wasm`) are consumed via path
dependencies only; no crate declares `publish` and none is published to
crates.io.

**Commitment.** None. Public Rust items of the workspace crates may change
in any release. If a crate is ever published to crates.io, it gets its own
semver policy at that time; this document will be updated first.

---

## 3. Version numbering

**Current state.** The whole workspace shares one version in
`Cargo.toml` → `workspace.package.version` (single source of truth;
`1.0.0` at the time of writing). Releases are tagged `tack-vX.Y.Z` and
the tag must match the workspace version exactly — the release workflow
fails otherwise (`docs/release.md`). User-facing changes are recorded in
`CHANGELOG.md` under `## [x.y.z]` headers (parsed by `/changelog` and the
startup "what's new" notice).

**Commitment (post-1.0 semantics).**

| Bump | Contents |
|---|---|
| **PATCH** | Bug fixes; behavior-preserving changes. |
| **MINOR** | Features; additive changes to any stable or versioned interface (new settings keys, new CLI flags, new RPC commands, new optional protocol fields, new protocol variants with catch-all tolerance). |
| **MAJOR** | Any breaking change as defined in §4.1. |

Deprecation warnings (§4.2) ship in MINOR releases; the actual removal
ships in the following MAJOR (or a later MINOR when the change is
tier-appropriate and the window was clearly announced — when in doubt,
MAJOR).

---

## 4. Breaking-change policy

### 4.1 What counts as breaking

- Dropping the ability to read or migrate any historical session file
  format (v1–v4). **This one is never allowed** (§2.1).
- Removing or renaming a `settings.json` key, CLI flag, or RPC command, or
  changing the meaning/type of an existing one.
- Changing an existing field's semantics in the CBOR protocol, extension
  protocol, or hook I/O contract.
- Bumping `PROTOCOL_VERSION` of the CBOR or extension protocol (the bump
  itself is allowed and expected; it is a breaking change that must follow
  §4.3 and a MAJOR version bump).
- Removing the `sessionBackend: "v3"` byte-compatible write path.
- Changing defaults in a way that silently alters behavior of existing
  integrations (judgment call; when in doubt, treat as breaking).

Explicitly **not** breaking: adding settings keys, CLI flags, RPC
commands, protocol methods/events/optional fields; changing anything in
the internal tier (§2.10); upstream-driven changes in ACP/MCP negotiated
per spec (§5).

### 4.2 Deprecation window

When a stable-tier surface must change in a breaking way:

1. **Release N (MINOR)**: the old behavior keeps working; a deprecation
   warning is emitted (stderr/log/CLI notice as appropriate) and the
   CHANGELOG entry is marked **DEPRECATED**.
2. **Release N+1 or later**: removal lands, called out prominently in the
   CHANGELOG under a "Breaking" heading.

The window is at least one full minor cycle. Security-driven removals may
shorten the window but must still ship with a CHANGELOG "Breaking" entry
and migration instructions.

### 4.3 Protocol bump procedure (adding a v2)

For the CBOR remote protocol or the extension protocol:

1. Define the new version constant (`PROTOCOL_VERSION = 2`) alongside the
   old one; keep the v1 codec paths compiled in.
2. **Dual-stack transition**: the newer side must speak v1 to a v1 peer
   for at least one minor cycle, selecting the version via the existing
   hello/initialize handshake (both protocols already exchange versions on
   connect — `crates/tack-protocol/src/client.rs`,
   `crates/tack-ext/src/process.rs`).
3. **Version detection + clear errors**: a peer that cannot be served must
   get an explicit version error (`ProtocolErrorCode::Version` / the
   "upgrade the host/client" messages), never a hang or a decode failure.
4. Ship the bump in a MAJOR release with a CHANGELOG "Breaking" entry and
   an update to this document.

---

## 5. Upstream-following interfaces

ACP (§2.4), MCP (§2.7), Claude-Code hooks (§2.6), and the wire-level parity
targets (TS pi's session formats, RPC wire shape, CBOR framing) are
**compatibility-with-an-external-ecosystem** surfaces. The policy for them:

- Follow the upstream's **stable** releases (agent-client-protocol crate,
  rmcp crate / MCP spec, Claude Code hook schema, TS pi). Do not track
  upstream nightlies.
- Alignment state and upstream baselines are tracked in
  `docs/upstream-alignment.md`; every upstream sync updates that file.
- Upstream-driven breaking changes on these surfaces are adopted
  deliberately, noted in the CHANGELOG, and where the spec supports
  negotiation (ACP, MCP) Tack negotiates rather than hard-failing.

---

## 6. Distribution & crates.io

Tack is distributed as a single binary (GitHub Releases, `tack update`
self-update — see `docs/release.md` and
`crates/tack-app/src/self_update.rs`). No workspace crate is published to
crates.io (no `publish` field in any `Cargo.toml`; path-only internal
dependencies), so there is **no semver commitment for Rust APIs** (§2.10).
The version number's compatibility meaning applies to the surfaces in §2.1
through §2.9 only.

---

## 7. Maintaining this document

- **Whoever changes an interface updates this document in the same PR.**
  A PR that touches any surface listed in §2 without a matching
  compatibility assessment is incomplete.
- When in doubt about whether a change is breaking, apply §4.1 and
  escalate to a MAJOR bump rather than rationalizing it as MINOR.
- The machine-checkable parts of this policy are enforced where possible:
  `evals/docs-audit/static_check.sh` cross-checks documented settings
  keys, hook events, CLI flags, and `features.*` keys against the code.
- Every release, the release manager verifies: CHANGELOG "Breaking"
  entries ⇔ MAJOR bump; DEPRECATED entries older than one minor cycle are
  either removed or explicitly re-justified.
