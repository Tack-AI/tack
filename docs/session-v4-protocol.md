# Message v4 JSONL Protocol Specification

**English | [简体中文](session-v4-protocol.zh-CN.md)**

This document defines the wire format of **message v4** session files and is
the conformance reference for readers and writers. It describes only "what a
valid v4 file looks like and what readers and writers must obey"; it does not
cover design trade-offs (see `crates/tack-session/V4_NOTES.md`) or the
legacy-format migration process (the §12 appendix gives only recognition
rules).

- Format version: `v = 4`; storage layout version: `storageVersion = 1`
- Reference implementation: `crates/tack-session/src/v4/{types,codec,store,fork}.rs`,
  `crates/tack-session/src/fork_policy.rs`

---

## 1. Terminology and File Model

A **session file** is a line-oriented JSONL text file, UTF-8 encoded.

Logically, the file is a **transaction log**:

- Line 1 is the **header** (§3);
- each subsequent line is a **transaction**: either a single **write**
  object, or an array of writes (all writes inside an array belong to the
  same transaction; an empty array is legal).

A reader folds the transaction log into the session's current state by
**replay**:

1. **entry set**: immutable events (messages, compaction checkpoints, …),
   forming a tree (or a forest of trees) via `id`/`parentId`, globally
   ordered by `seq`;
2. **usage ledger**: the append-only sequence of usage rows;
3. **scalar current values**: the latest value at each `(namespace, key)`
   address;
4. **list current contents**: at each `(namespace, key)` address, the
   sequence of elements `append`ed after the last `delete` (ascending
   `seq`).

## 2. Line Format

- Every line is terminated by `\n`. Trailing whitespace may be stripped
  before parsing.
- **Torn tail**: if the final line of the file lacks the `\n` terminator,
  that line must be discarded and excluded from replay (real writers always
  terminate lines; this rule tolerates crash truncation).
- Apart from the torn tail, any line that fails to parse (invalid JSON,
  invalid write, validation violation) means **the entire file is
  corrupt** — the reader must reject the file as a whole and must not skip
  the line and continue.
- Empty lines (whitespace only) are illegal.

## 3. Header

Line 1 must be the header object:

```json
{"v":4,"kind":"header","id":"s1","storageVersion":1,"createdAt":1767225600000,"cwd":"/work","parentSessionId":"s0","nextSeq":15}
```

| Field | Type | Required | Constraints and meaning |
|---|---|---|---|
| `v` | int | ✓ | Always `4` |
| `kind` | string | ✓ | Always `"header"` |
| `id` | string | ✓ | Session id |
| `storageVersion` | int | ✓ | ≥ 1; this specification defines only `1`; other values must be refused on open |
| `createdAt` | int | ✓ | ≥ 0, creation time (Unix epoch milliseconds) |
| `cwd` | string | ✓ | Working directory the session belongs to |
| `parentSessionId` | string | — | Source session id of a fork |
| `legacyParentSessionPath` | string | — | File path retained when the parent session id is unresolvable (migration fallback) |
| `nextSeq` | int | — | ≥ 1; sequence-number high-water mark (§5.2) |

Recognition: `kind == "header" && v == 4` and all type constraints in the
table above hold. **Unrecognized fields must be ignored** (forward
compatibility). The header always appears in plaintext (§9 encryption
applies only to transaction lines).

## 4. Write Types

Each write is a JSON object discriminated by the `kind` field; there are 4
kinds: `entry`, `usage`, `value`, `list`. An unknown `kind` is file
corruption.

### 4.1 `entry` — Immutable Events

An entry write is a flattened structure: `{kind:"entry", ...Entry}`.
Entries are discriminated by `type` and carry common structural fields
(`EntryBase`):

| Field | Type | Meaning |
|---|---|---|
| `id` | string | entry id (same namespace as usage-row ids) |
| `parentId` | string \| null | parent entry; `null` denotes a tree root |
| `seq` | int | session-level sequence number (§5.1) |
| `timestamp` | int | ≥ 0, commit time (epoch milliseconds) |

An unknown `type` is file corruption. There are 4 entry types:

#### 4.1.1 `message`

```json
{"kind":"entry","type":"message","id":"e1","parentId":null,"seq":2,"timestamp":1767225601000,
 "message":{"role":"user","content":"hi","timestamp":1767225600999}}
```

| Field | Type | Meaning |
|---|---|---|
| `message` | AgentMessage | message payload (see §4.1.5) |
| `terminate` | `true` | optional; lane-termination marker, present only when `true` |

#### 4.1.2 `compaction`

```json
{"kind":"entry","type":"compaction","id":"c1","parentId":"e9","seq":10,"timestamp":1767225610000,
 "summary":"...","retainedTail":[{"role":"user","content":"kept","timestamp":1}],
 "tokensBefore":12000,"fromHook":false}
```

| Field | Type | Meaning |
|---|---|---|
| `summary` | string | compaction summary |
| `retainedTail` | AgentMessage[] | materialized retained messages (the checkpoint is self-contained; readers need not consult the entry tree) |
| `tokensBefore` | int | context token count before compaction |
| `details` | any | optional extension payload |
| `usage` | Usage | optional; usage of this compaction LLM call (§4.2.1) |
| `fromHook` | bool | whether produced by a hook |

Context projection rule: `summary` first, `retainedTail` after; together
they replace the compacted history.

#### 4.1.3 `branch_summary`

```json
{"kind":"entry","type":"branch_summary","id":"b1","parentId":"e9","seq":11,"timestamp":1767225611000,
 "fromId":"e5","summary":"...","fromHook":false}
```

| Field | Type | Meaning |
|---|---|---|
| `fromId` | string \| null | the entry the summary branches from; `null` = tree root |
| `summary` | string | summary text (an entry whose summary is the empty string projects no context message) |
| `details` / `usage` / `fromHook` | — | same as 4.1.2 |

#### 4.1.4 `custom`

```json
{"kind":"entry","type":"custom","id":"x1","parentId":"e9","seq":12,"timestamp":1767225612000,
 "customType":"model_change","data":{"provider":"anthropic","modelId":"claude"}}
```

| Field | Type | Meaning |
|---|---|---|
| `customType` | string | application-defined type tag |
| `data` | any | optional application payload |

The storage layer does not interpret custom entries; whether they are
projected into context is up to the application. customType values used by
tack convention (readers should preserve them verbatim, with no obligation
to interpret): `model_change` (`{provider, modelId}`),
`thinking_level_change` (`{thinkingLevel}`), `session_info` (`{name}`),
`label` (`{targetId, label}`).

#### 4.1.5 AgentMessage

The `message` payload and the `retainedTail` elements are `AgentMessage`: a
message union discriminated by `role` (`user` / `assistant` / `toolResult`
/ `system` / `custom` / `bashExecution` / `branchSummary` /
`compactionSummary`), whose content blocks, usage, and timestamp schemas
are defined in `crates/tack-agent-core/src/message.rs` (identical to
upstream pi's message wire format; not elaborated here).

### 4.2 `usage` — Usage Ledger Row

```json
{"kind":"usage","id":"u1","seq":13,"usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"entryId":"e2","adjustment":false}
```

| Field | Type | Meaning |
|---|---|---|
| `id` | string | row id (same namespace as entry ids) |
| `seq` | int | session-level sequence number |
| `usage` | Usage | see §4.2.1 |
| `entryId` | string | optional; the owning entry |
| `adjustment` | bool | `true` = ledger correction (e.g. a format-import row), `false` = new consumption |
| `details` | any | optional extension payload (import rows use `{source:"v3-import"}` by convention) |

Ledger rows do not enter the entry tree; session-level usage totals = the
sum of the ledger rows' `usage`. The `usage` field inside an entry payload
is just data and does not enter the totals.

#### 4.2.1 Usage

```json
{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"cacheWrite1h":0,"reasoning":0,
 "totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}}
```

Integer fields are token counts; `cacheWrite1h` and `reasoning` are
optional (absent = that category is not tracked); `cost.*` are
floating-point amounts.

### 4.3 `value` — Scalar Current State

```json
{"kind":"value","op":"set","seq":14,"namespace":"tack.branch.tip","key":"main","value":"e9"}
{"kind":"value","op":"delete","seq":15,"namespace":"tack.entry.label","key":"e3"}
```

| Field | Type | Meaning |
|---|---|---|
| `op` | `"set"` \| `"delete"` | other values are file corruption |
| `seq` | int | session-level sequence number |
| `namespace` | string | namespace (§7) |
| `key` | string | address key; the empty string denotes a singleton address |
| `value` | any | `set` only; any JSON value |

The current value of an address is decided by the last set/delete in replay
(after a delete the address has no current value; a later set restores it).

### 4.4 `list` — List Current State

```json
{"kind":"list","op":"append","seq":16,"namespace":"app.log","key":"k","value":{"n":1}}
{"kind":"list","op":"delete","seq":17,"namespace":"app.log","key":"k"}
```

Fields as in 4.3. `append` appends one element; `delete` clears the entire
list. **Live elements** = all elements `append`ed after the last `delete`
(ascending `seq`); if never `delete`d, all `append`s.

## 5. Invariants

### 5.1 Sequence Numbers (seq)

- `seq` is a **session-level** integer sequence starting at 1, assigned by
  the writer at commit time; multiple writes within one transaction take
  consecutive values.
- During replay, the `seq` of all writes (including entry/usage/value/list)
  must be **globally strictly increasing**.
- Every write has `seq ≥ 1`.

### 5.2 `nextSeq` High-Water Mark

Snapshot-style rewrites (processes like fork that copy only live writes)
write `nextSeq` into the target header. After opening, the next allocatable
seq is `max(highest seq seen + 1, header.nextSeq)`. This guarantees that
the rewritten file never reuses sequence numbers already consumed by the
source file.

### 5.3 id Uniqueness and Parent Integrity

- All entry/usage `id`s are globally unique (one namespace across both
  kinds).
- An entry's `parentId` is `null`, or points to an already-replayed entry;
  a parent entry appearing earlier within the same transaction is legal
  (parent seq is therefore always less than child seq).

### 5.4 Timestamps

- `timestamp` appears only on entry writes, in epoch milliseconds.
  usage/value/list carry no timestamp.
- Timestamps inside message payloads (AgentMessage) are likewise epoch
  milliseconds, independent of the entry's commit timestamp.

### 5.5 Replay Validation Checklist

The reader must perform the following checks in transaction order; any
violation means file corruption:

1. the line is valid JSON, and is an object or an array of objects;
2. every write's `kind` and `op` are enum values of this specification,
   `seq ≥ 1`;
3. `seq` is globally strictly increasing (§5.1);
4. ids are unique and parents are complete (§5.3);
5. the header `storageVersion` is supported (§3).

Read-only scenarios (listing, search, statistics) may use **lenient
replay**: skip unparseable lines and skip invariant checks — but the result
of a lenient read must never be used for write-back.

## 6. Writer Rules

1. All changes are appended as one transaction per line; a multi-write
   transaction must be persisted atomically (indivisible across two
   transactions).
2. When creating a file, write the header first, then the initial
   transaction (which may be empty, i.e. header only).
3. Snapshot-style rewrites (fork, any whole-file rewrite) must: write a
   temporary file first, then atomically rename; and write the correct
   `nextSeq` into the header (§5.2).
4. An entry's `parentId` must satisfy §5.3 at write time.

## 7. Namespaces

Current state is addressed by `namespace` strings. Namespaces fall into two
categories:

- **Reserved namespaces**: prefix `tack.` (tack) or `pi.` (upstream pi).
  The two have the same layout with different prefixes; implementations
  **must not** write to the other's reserved prefix (read-fallback rules in
  §7.2).
- **Application namespaces**: any other string, interpreted by the
  application itself.

### 7.1 Reserved Address Table

| namespace (`tack.`/`pi.` prefix) | key | value type | meaning |
|---|---|---|---|
| `.session.name` | `""` | string | session display name |
| `.entry.label` | entryId | string | entry label |
| `.branch.tip` | branch name | string \| null | entry id of the branch tip; null = empty branch |
| `.lane.config` | lane name | LaneConfiguration | lane configuration |
| `.lane.state` | lane name | LaneState | lane runtime state |
| `.result` | operationId | OperationResultRecord | result of a finished operation |
| `.op.*` | implementation-defined | operation runtime state |
| `.pending.*` | implementation-defined | to-be-materialized/pending state |

```
LaneConfiguration = {"model":{"provider":string,"modelId":string},
                     "thinkingLevel":string,"activeToolNames":string[]}
LaneState         = {"currentOperationId":string|null,
                     "lastOperationId":string|null,
                     "inbox":[{"entryId":string,"kind":string}]}
idle LaneState    = {"currentOperationId":null,"lastOperationId":null,"inbox":[]}
```

**branch**: a named path on the entry tree (§1); `.branch.tip/{branch}`
points to its tip; the main branch name is `"main"` by convention. A
**complete lane** = the branch tip row + lane config + lane state under the
same name, all three present; lane-supporting writers should create all
three atomically in one transaction.

### 7.2 Cross-Prefix Read Fallback (tack Implementation)

When tack opens a session containing only `pi.*` rows, the following reads
fall back to `pi.*` rows when `tack.*` rows are missing: branch tip, lane
config, session name, entry label; once a `tack.*` row exists it wins. tack
never writes `pi.*` — in a mixed file the pi-prefixed tip row goes stale as
tack appends, and handing the file back to pi will fall back to that old
tip (a declared cross-implementation limitation).

## 8. Fork Contract

fork projects a source session into a new v4 file (the source is not
modified):

- Copied writes **retain their original seq**; the target header records
  `parentSessionId = source id` and the source's high-water `nextSeq`
  (§5.2).
- Entry selection:
  - **`tree` scope**: copy all entries;
  - **`branch` scope**: the source branch must be a **complete lane**
    (§7.1); only the ancestor chain of its tip is copied; `entryId`
    defaults to the tip, and `position:"before"` cuts to that entry's
    parent (may yield a null tip).
- Current-state projection is a **closed classifier** — the same set of
  rules (table below) applies to the `tack.*` and `pi.*` prefixes; usage
  ledger rows are never copied; deleted (non-live) historical rows do not
  participate in projection.

| namespace | tree scope | branch scope |
|---|---|---|
| `*.session.name` | copy | copy |
| `*.entry.label` | only if its entry is copied | same |
| `*.branch.tip` | all branches as-is | only that branch, value rewritten to the cut point (may be null) |
| `*.lane.config` | all lanes | only that lane |
| `*.lane.state` | all lanes, values reset to idle | only that lane, value reset to idle |
| `*.result`, `*.op.*`, `*.pending.*` | **excluded** | **excluded** |
| other namespaces starting with `tack`/`tack.` or `pi`/`pi.` | **fork fails** (fail-closed) | same |
| application namespaces | copy | excluded |

Fail-closed is part of this contract: an implementation that introduces a
new namespace under a reserved prefix must simultaneously declare its fork
semantics in the classifier; otherwise, forking a session containing live
state in that namespace must fail, rather than the state being silently
copied or dropped.

## 9. Encryption Layer (tack Extension)

Encryption is a **transparent line-level wrapper** for the protocol:

- The header is always plaintext; transaction lines may be encrypted whole
  as `tack-enc:v1:<payload>` (algorithm and key derivation in
  `crates/tack-session/src/crypto.rs`).
- The reader first recognizes and decrypts per line, then parses per this
  specification; an encrypted line that cannot be decrypted (no key, wrong
  key, tampered ciphertext) must make the entire file fail as "encryption
  unreadable", not be skipped or treated as corruption.
- When a writer enables encryption, all transaction lines (including the
  initial transaction and the products of snapshot rewrites) must be
  encrypted; if encryption fails, the transaction must not be persisted —
  never downgrade to plaintext.
- A reader without the key may treat `tack-enc:v1:` lines as unparseable
  lines: per §2, this means the entire file is unusable (for
  implementations like pi that have no encryption concept, i.e. "file
  corrupt").

## 10. Versioning and Extension Rules

- Dual versioning with `v` / `storageVersion`: beyond the format (v4),
  layout changes are expressed via `storageVersion`; unsupported versions
  must be refused on open (§3).
- **Unrecognized fields** on objects must be ignored (header, writes,
  entries, payloads alike).
- Unrecognized `kind`, `type`, `op` enum values are **file corruption**
  (they cannot be safely ignored).
- New reserved namespaces must register fork semantics under §8's
  fail-closed rule.

## 11. Conformance Requirements

**Conforming reader**: satisfies the §2 line rules, §3 header recognition,
the §5.5 validation checklist, §5.2 `nextSeq` advancement, and the §8 fork
projection table; may declare encryption capability if it supports §9.

**Conforming writer**: satisfies all §5 invariants, the §6 write rules, and
the §8 snapshot-rewrite requirements; does not write to the other's
reserved prefix (§7).

## 12. Appendix

### A. Complete Example

```jsonl
{"v":4,"kind":"header","id":"s1","storageVersion":1,"createdAt":1767225600000,"cwd":"/work"}
{"kind":"value","op":"set","seq":1,"namespace":"tack.branch.tip","key":"main","value":null}
[{"kind":"entry","type":"message","id":"a1b2c3d4","parentId":null,"seq":2,"timestamp":1767225601000,"message":{"role":"user","content":"hi","timestamp":1767225600999}},{"kind":"value","op":"set","seq":3,"namespace":"tack.branch.tip","key":"main","value":"a1b2c3d4"}]
[{"kind":"entry","type":"custom","id":"e5f6a7b8","parentId":"a1b2c3d4","seq":4,"timestamp":1767225602000,"customType":"model_change","data":{"provider":"anthropic","modelId":"claude"}},{"kind":"value","op":"set","seq":5,"namespace":"tack.branch.tip","key":"main","value":"e5f6a7b8"}]
[{"kind":"entry","type":"custom","id":"c9d0e1f2","parentId":"e5f6a7b8","seq":6,"timestamp":1767225603000,"customType":"thinking_level_change","data":{"thinkingLevel":"high"}},{"kind":"value","op":"set","seq":7,"namespace":"tack.branch.tip","key":"main","value":"c9d0e1f2"},{"kind":"value","op":"set","seq":8,"namespace":"tack.lane.config","key":"main","value":{"model":{"provider":"anthropic","modelId":"claude"},"thinkingLevel":"high","activeToolNames":[]}},{"kind":"value","op":"set","seq":9,"namespace":"tack.lane.state","key":"main","value":{"currentOperationId":null,"lastOperationId":null,"inbox":[]}}]
{"kind":"usage","id":"d3e4f5a6","seq":10,"usage":{"input":12,"output":4,"cacheRead":0,"cacheWrite":0,"totalTokens":16,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"entryId":"a1b2c3d4","adjustment":false}
```

Line by line: ① header; ② empty main branch tip at creation (bare-object
transaction); ③ user message + tip move (array transaction); ④ model
change (custom entry + tip move); ⑤ thinking change — with model and
thinking both present, lane config and idle lane state are mirrored in the
same transaction; ⑥ one usage ledger row.

### B. Recognizing Non-v4 Files

A first line `{"type":"session","version":3,...}` is a legacy v3 file
(earlier versions are chain-upgraded through v3 first). v3 and v4 are
**different formats**: v3 has one entry record per line, ISO-string
timestamps, and no transaction/namespace concepts. The v3 → v4 migration
rules and implementation live in `crates/tack-session/V4_NOTES.md` and
`src/v4/migrate.rs`, outside this specification.

### C. Reference Implementations

| Implementation | Location |
|---|---|
| tack (Rust) | `crates/tack-session/src/v4/{types,codec,store,fork}.rs`, `src/fork_policy.rs`, `src/v4_bridge.rs` |
| upstream pi (TypeScript) | `packages/agent/src/harness/session/jsonl/*.ts`, `session/{commit,fork-policy,values}.ts` |

---

*Protocol version: format v4 / storage v1. When modifying this
specification, also update `crates/tack-session/V4_NOTES.md` and
`docs/compatibility.md` §2.1.*
