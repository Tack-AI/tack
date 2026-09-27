# Changelog

All notable changes to Tack are documented here. The format follows
`## [x.y.z]` version headers so `/changelog` and the startup "what's new"
notice can parse entries (same convention as TS pi).

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


