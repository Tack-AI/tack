# Session storage v4 — design notes

> The normative wire-format specification lives in
> [`docs/session-v4-protocol.md`](../../docs/session-v4-protocol.md); this
> file records the design decisions and deliberate divergences behind it.

Tracking issue: `docs/upstream-alignment.md` → "存储 v4 迁移" (largest known
upstream gap). Upstream references (local checkout of TS pi):

- `packages/agent/src/harness/session/jsonl/{codec,io,repo,storage,types,legacy-v3,fork}.ts`
- `packages/agent/src/harness/session/fork-policy.ts`, `commit.ts`, `values.ts`
- work packages 06 (session/branch/lane separation), 07 (SQLite ownership),
  08 (named-branch streaming forks)

## Format decisions

The v4 file is a **transaction log**, replacing v3's one-entry-per-line tree:

- Line 1: header `{v:4, kind:"header", id, storageVersion:1, createdAt,
  cwd, parentSessionId?, legacyParentSessionPath?, nextSeq?}` — field names
  are byte-aligned with upstream `JsonlStorageHeader`.
- Every later line is one **transaction**: a single write object or an array
  of writes (upstream `serializeJsonlTransaction`). Write kinds are
  field-aligned with upstream `CommittedWrite`:
  - `{kind:"entry", ...Entry, seq, timestamp}` — Entry variants
    `message` / `compaction` / `branch_summary` / `custom` with the upstream
    field names (`parentId`, `retainedTail`, `tokensBefore`, `fromHook`,
    `customType`, `fromId: string|null`, millisecond numeric `timestamp`).
  - `{kind:"usage", id, seq, usage, entryId?, adjustment, details?}`
  - `{kind:"value", op:"set"|"delete", seq, namespace, key, value?}`
  - `{kind:"list", op:"append"|"delete", seq, namespace, key, value?}`
- `seq` is a session-wide monotonic sequence (≥1) assigned by the store at
  commit time; `timestamp` is epoch **millis** (v3 used ISO strings).
- Encryption: the v3 per-line scheme is kept — the header stays plaintext,
  transaction lines are encrypted whole with `tack-enc:v1:` when a session
  key is installed. A v4 file therefore round-trips through the same
  `crypto` helpers; encrypted v3 sessions migrate to encrypted v4 sessions.
- Current-state values live in namespaces. Reserved `tack.*` namespaces used
  here: `tack.session.name`, `tack.entry.label/{entryId}`,
  `tack.branch.tip/{branch}`, `tack.lane.config/{lane}`,
  `tack.lane.state/{lane}`. `tack.result`, `tack.op.*`, `tack.pending.*` are
  recognized (and excluded from forks) but never written by Tack yet.
  (Upstream TS pi writes the same layout under `pi.*`; Tack renamed the
  namespaces. `pi.*` rows are never WRITTEN, but they are honored as
  read-only fallbacks on open and get upstream fork semantics when a
  pi-written session crosses a fork — see the pi-interop section below.)

## Branch/lane separation (WP06/08, minimal viable semantics)

- A **branch** is a named path through the immutable entry tree:
  `tack.branch.tip/{branch}` holds its tip entry id (or null).
- A **lane** = branch + `tack.lane.config/{branch}` (`LaneConfiguration`:
  model, thinkingLevel, activeToolNames) + `tack.lane.state/{branch}`
  (`{currentOperationId, lastOperationId, inbox}`). Lanes are created
  atomically (`V4Store::create_lane` commits tip+config+idle state in one
  transaction), matching upstream's "supported writers create lane state
  atomically".
- `V4Store::append_entry(branch, entry)` extends a branch tip atomically
  (entry write + tip set in one transaction).
- No operation/drive runtime is implemented (that is the agent harness's
  job, out of tack-session's scope); lane state is always the idle value.

## Fork (WP08 semantics, in-memory source)

`run_v4_fork` implements the upstream fork contract:

- `ForkOptions::{Branch{branch, entry_id, position}, Tree}`; branch scope
  validates a *complete configured lane* (tip + config + state) and entry
  ancestry membership; `position: Before` may yield a null destination tip.
- All namespace knowledge lives in `fork_policy.rs` — a closed classifier
  ported function-by-function from `fork-policy.ts`:
  `tack.session.name` copied; `tack.entry.label` copied iff its entry is
  copied; `tack.branch.tip`/`tack.lane.config` kept for the named branch only
  (tree: all); `tack.lane.state` always rewritten to fresh idle;
  `tack.result`, `tack.op.*`, `tack.pending.*` excluded; **any other `tack`/`tack.*`
  namespace with current surviving state fails the fork**
  (`UnknownReservedNamespace`) instead of being silently copied or dropped;
  application namespaces are copied on tree scope and excluded on branch
  scope. Usage rows are never copied.
- Copied writes keep their source `seq`; the destination header records the
  source's `next_seq` high-water mark, like upstream `runJsonlFork`.
- Upstream `pi.*` namespaces cross forks under upstream's OWN rules
  (the same closed classifier under the `pi` prefix): `pi.session.name`
  copied, `pi.entry.label` follows copied entries, `pi.branch.tip`/
  `pi.lane.config` scoped like their tack counterparts, `pi.lane.state`
  reset to idle, `pi.result`/`pi.op.*`/`pi.pending.*` excluded on both
  scopes, and any other `pi`/`pi.*` namespace fails the fork. Without
  this, tree-forking a pi-written session would copy stale
  operation/pending runtime state into the destination.
- Tradeoff: Tack forks from the already-loaded in-memory state (equivalent
  to upstream's *memory* backend path) instead of the two-scan streaming
  JSONL procedure. This matches Tack's existing whole-file model
  (`SessionManager` loads entire sessions); the observable result is
  identical because the in-memory map *is* the current state. The streaming
  v3→v4 migration below IS two-pass/bounded-memory, as upstream requires.

## Legacy v3 → v4 migration (`migrate.rs`, port of `legacy-v3.ts`)

Opening a v3 file through `V4Store::open` transparently rewrites it to v4
(upstream defers the rewrite to the first commit; Tack's v3
`SessionManager::open` already rewrites migrated files eagerly, so eager
migration matches local precedent and keeps `open` total):

- **Streaming two-pass, bounded memory.** Pass 1 scans line by line
  building only a structural index (id/parent/type/metadata per record —
  no payloads), deriving session name, labels, branch tip, lane config and
  imported usage. Pass 2 re-streams the file, materializing retained
  entries; compaction `retainedTail`s are rebuilt from a cache of only the
  messages the tails actually reference (upstream's
  `collectRequiredTailMessageIds`).
- Entry mapping: `message`, `custom`,
  `custom_message` (→ `message` with role `custom`), `branch_summary`,
  `compaction` are **retained** with freshly minted ids. The v3 change
  records `model_change`, `thinking_level_change`, `session_info`, `label`
  are ALSO retained — as custom entries (`custom_type` = the v3 type name,
  `data` = the record fields; label `targetId`s resolve through the id
  mapping) — because Tack's session context rebuilds model/thinking
  state from these entries, and folding them away would lose it on
  resume. They are additionally folded into derived values (lane config,
  session name, labels), so v4-native consumers see the same state.
  `active_tools_change` is the only discarded kind (Tack never writes it).
- Unknown/extension record types are retained as custom entries too
  (`custom_type` = original type, `data` = all non-structural fields) —
  Tack's standing no-silent-data-loss rule (upstream's migrator rejects
  them outright). The same fallback applies to RETAINED kinds whose
  payload no longer fits the typed `SessionEntry` schema (unknown
  message roles, missing fields): preserved as custom entries rather
  than aborting the migration (upstream passes such payloads through
  verbatim). When such a record feeds a compaction tail, its raw
  `message` payload still lands in the tail on a best-effort basis.
  Records without an `id` are skipped (payloads survive
  in the `.bak`) so a malformed line can never lock a user out of the
  whole session on open; a non-string `parentId` is a hard error
  (re-rooting would silently corrupt the tree; upstream fails the same
  lookup).
- Derived values: `tack.session.name`, `tack.entry.label/{mappedId}` (latest
  label wins, cleared labels dropped), `tack.branch.tip/main` (mapped final
  entry), and — only when both model and thinking level are recoverable by
  walking the final entry's ancestry — `tack.lane.config/main` plus idle
  `tack.lane.state/main`.
- Imported usage (assistant/toolResult message usage + compaction and
  branch-summary LLM usage) is preserved as one
  `{kind:"usage", adjustment:true, details:{source:"v3-import"}}` row.
- Timestamps are parsed leniently (upstream `Date.parse` tolerance):
  RFC-3339, then RFC-2822, then date-only; anything else falls back to
  the previous record's timestamp (seeded with the header `createdAt`) —
  one bad timestamp never aborts the migration (upstream propagates NaN,
  which JSON renders `null`; tack's `u64` timestamps carry the nearest
  known time instead). Empty-string session names are not written and
  empty-string labels count as cleared (upstream's truthiness rules).
- Compactions WITHOUT a `retainedTail` checkpoint require a reachable
  `firstKeptEntryId` on the parent ancestry — a missing boundary or a
  null-parent compaction fails the migration
  (`MissingCompactionBoundary` / `CompactionBoundaryNotOnBranch`) instead
  of silently producing an empty tail (upstream throws in the same
  situations). Compactions that DO carry a checkpoint keep the
  checkpoint tail, with `systemMessage` prepended when present (the
  checkpoint is NOT byte-verbatim in that case — see the system-message
  section below).
- The original file is kept as `<path>.bak`; the new file is written to a
  temp file and atomically renamed (same crash-safety pattern as the v3
  migration; the Windows delete-then-rename retry is acceptable because
  the `.bak` already exists, and unix builds propagate the atomic
  rename's error instead). Pass 2 re-verifies the header identity before
  streaming, like upstream's changed-source guard. Encryption state is
  preserved: undecryptable input fails with
  `Encrypted`; output transaction lines are encrypted iff a session key is
  installed. Imported usage sums the optional `cacheWrite1h`/`reasoning`
  token classes too, matching upstream `addUsage`.

## Deliberate divergences from upstream

- **Entry ids**: upstream mints `uuidv7(timestamp)` for migrated entries;
  Tack mints its existing 8-char uuid-v4 ids (`generate_id`) — the format
  does not require time-ordered ids and Tack avoids enabling new uuid
  features.
- **Unknown v3 record types** (TS extension entries): upstream's migrator
  rejects them; Tack preserves them as custom entries with their full
  payload (see above), mirroring `SessionLine::Unknown`'s
  preserve-don't-crash philosophy.
- **First-line header requirement**: v4 open requires the header on line 1
  (upstream behavior); v3 `SessionManager::open` tolerated leading junk
  lines.
- **Eager migration on open** instead of upstream's lazy
  upgrade-on-first-commit (see above). `V4Store::was_legacy_v3()` reports
  whether the opened file was migrated in this call.
- v1/v2 sessions: migrate to v3 first via `SessionManager::open`, then to
  v4 — the v4 migrator accepts only version-3 headers, like upstream.
- No SQLite v4 backend, no operation/pending state, no repo-level session
  listing (WP07): out of scope for this change; the existing v3 SQLite
  backend is untouched.

## pi-written session interoperability (read fallback)

Reserved namespaces differ on disk (`tack.*` vs upstream `pi.*`), so a
pi-written session carries no `tack.*` rows. To keep such sessions
resumable, `V4Store` honors the upstream rows as READ-ONLY fallbacks:

- `branch_tip` falls back to `pi.branch.tip/{branch}` (so resume starts
  from pi's tip), `lane_config` to `pi.lane.config/{lane}` (so the
  `LaneTracker` seeds model/thinking state), `session_name` to
  `pi.session.name`, and `get_label` to `pi.entry.label/{entryId}`.
  Once a `tack.*` row exists it wins — the fallback only applies while
  the tack row is absent. The lenient whole-file scanners (session
  listing/search) likewise recognize `pi.session.name`.
- Writes NEVER go to `pi.*`: the first tack append starts moving
  `tack.branch.tip/main` while pi's row goes stale. A tack-appended pi
  session reopened IN PI therefore resumes from that stale pi tip — a
  known cross-implementation limitation (mirroring it would require
  dual-writing both namespaces).
- `has_complete_lane` and lane-state reads remain `tack.*`-only, so
  branch-scope forks of pi lanes still reject (tree scope works and
  applies the pi fork rules above).

## Transcript system messages (upstream #9548, aligned 2026-09)

Upstream #9548 ("Mid conversation system messages") moved the system
prompt and tool declarations INTO the transcript: `Message` gained a
`{role:"system", content, sections?, toolsAdded?, toolsRemoved?,
timestamp}` variant, and v3 compaction entries gained a `systemMessage`
field holding the replayed prompt/tool state at the boundary.
`addedToolNames` was removed from `ToolResultMessage` in the same change.
Tack aligns:

- `tack_ai::SystemMessage` / `Message::System` / `AgentMessage::System` are
  wire-compatible (round-trip fields incl. `providerThinkingLevel`,
  `diagnostics`, `deferred` on assistant messages; sections preserve
  insertion order via `IndexMap`, matching JS `Map` semantics).
- `tack_ai::transcript` ports upstream `utils/transcript.ts` + `text.ts`
  (replay helpers); byte-level ground truth generated from the upstream
  sources lives in `crates/tack-ai/tests/fixtures/transcript_ground_truth.json`.
- The agent loop records prompt/tool state changes as system messages
  (`system_state_update` + `declare_tool_changes`, ports of upstream
  `_preparePromptAndToolLoadout`/`declareToolChanges`). Tack stores its
  monolithic prompt in ONE section named `system-prompt`; foreign sections
  (e.g. upstream's `preamble`/`tools`/`rules`) are nulled by the next
  update rather than merged.
- Providers keep the collapsed view (`Context.system_prompt` + current
  tool list); system roles never reach provider converters. Anthropic's
  in-place `tool_addition`/`tool_removal` anchoring is NOT ported (the
  top-level tool list always carries the full current set).
- `AgentToolResult.added_tool_names` survives as a tack-internal,
  never-serialized channel for the tool_search tool; the loop records the
  activation as a `toolsAdded` system message instead.
- v3 compaction: `append_compaction` records the replayed `systemMessage`
  (serialized WITH `role:"system"`, like upstream), context building
  projects it before the summary, kept-range system messages are skipped,
  and compaction never summarizes system messages.
- **Divergence**: upstream's v3→v4 migration DROPS `systemMessage`; Tack
  preserves it by prepending the replayed system message to the v4
  compaction's `retainedTail` (`compaction_tail_with_system`) — readable
  by both implementations (v4 readers project `summary + retainedTail`).
  This applies on BOTH the checkpoint path and the rebuilt-tail path
  (earlier versions only preserved it on the checkpoint path). Live v4
  writes do the same for compactions recorded after the change.
- **branchSummary `fromId`**: inside retained tails (and everywhere else
  a `branchSummary` MESSAGE appears) a root source is `fromId: null`,
  the v4/upstream wire shape — the v3 ENTRY-level `"root"` sentinel is
  mapped at the projection boundary (`BranchSummaryMessage.from_id` is
  `Option<String>`).
- **Key order**: Tack serializes system messages in interface declaration
  order (`role, content, sections?, toolsAdded?, toolsRemoved?,
  timestamp`). Upstream itself is inconsistent: `createInitialSystemMessage`
  /`getCurrentSystemMessage` put `timestamp` last; `withToolChanges` (the
  agent-loop declare path) puts it before the tool fields. Alignment is
  therefore defined as byte-level modulo object key order (semantic JSON
  equality) — any JSON parser reads both identically.

## SessionManager write path (v4 is the default backend)

`SessionBackend::{JsonlV4 (default), Jsonl ("v3" escape hatch), Sqlite}`
via `sessionBackend`. The in-memory entry model is unchanged — only
persistence branches:

- **create/start_file**: `V4Store::create` + a null `tack.branch.tip/main`
  write (lane config/state are only written once both a model and a
  thinking level are known, mirroring the migration rule).
- **append** (`v4_bridge::live_append_writes`): one transaction per entry
  = the entry + the branch-tip move + mirrored values (session name,
  label set/delete, lane config refresh) + a usage ledger row when the
  entry carries LLM usage (assistant turns, compaction/branch-summary
  passes). `model_change`/`thinking_level_change`/`session_info`/`label`
  are stored as custom entries (same shapes the migration produces, so
  there is exactly one read path) AND mirrored into current-state values.
- **open**: first-line sniff — v4 headers open directly; v3 headers
  migrate in place (with `.bak`); v1/v2 go through the legacy v3 rewrite
  first. `sessionBackend: "v3"` keeps the legacy behavior and rejects v4
  files with `SessionError::FormatV4`.
- **branch()**: commits a `tack.branch.tip/main` value write.
- **fork_from_in**: v4 sources go through `run_v4_fork` (Tree scope +
  projection); the v3 escape hatch keeps the legacy verbatim-copy fork.
- `V4Store::create` encrypts its initial transaction when a session key
  is installed, matching the append path (never degrade to plaintext).
- A missing final newline is treated as a torn tail and dropped on v4
  open (upstream `line.terminated` rule) — real writers always terminate
  lines, but hand-edited files lose an unterminated last line.
