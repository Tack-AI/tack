# Tack Extensions (tack-RPC v3)

**English | [简体中文](extensions.zh-CN.md)**

> This document describes the extension system as of the tack-RPC v3
> redesign (see [plugin-roadmap.md](plugin-roadmap.md) for the why and the
> phases). The protocol's single source of truth is the OpenRPC document
> [`protocol/tack-rpc.openrpc.json`](../protocol/tack-rpc.openrpc.json) —
> this page covers the surrounding machinery (manifest, identity, store,
> install, marketplace, dev tooling). The pre-v3 NDJSON protocol was
> removed in the same release.

## 1. The three levels

| Level | Form | Best for |
|---|---|---|
| **1 — declarative bundle** | `extension.json` + hooks/MCP/skills files, no code | guardrails, context, tool wiring |
| **2 — MCP server plugin** | `extension.json` declaring MCP servers | tool contribution from the MCP ecosystem |
| **3 — tack-RPC plugin** | process or WASM-carrier executable speaking tack-RPC v3 | interception, lifecycle, widgets, session control, approval, config, metrics |

Level 3 is what this document covers. Plugins are built with an SDK
(Rust `tack-ext-sdk`, `@tack/plugin` for TypeScript, `tack-plugin` for
Python) — plugin code never sees a JSON-RPC envelope. Scaffold one with
`tack ext new <dir> <rust|ts|python>`.

## 2. extension.json reference

```jsonc
{
  "name": "my-ext",                    // required; id segment (see §3)
  "version": "1.2.0",                  // optional semver; becomes the store version
  "command": "node",                   // process carrier: executable to spawn
  "args": ["plugin.js"],               // path-like entries resolve against the ext dir
  "env": { "FOO": "bar" },             // explicit env (sensitive host vars are stripped)
  "carrier": "process",                // "process" (default) | "wasm"
  "module": "plugin.wat",              // wasm carrier: module file (.wasm/.wat)
  "limits": { "maxFuel": 100000000, "maxMemoryBytes": 16777216, "maxExecutionMs": 60000 },
  "capabilities": {                    // wasm carrier: explicit sandbox grants
    "fs": [{ "host": "data", "guest": "/data", "access": "read-only" }],
    "env": { "LITERAL": "1" },
    "args": ["--verbose"]
  },
  "failMode": "closed",                // hook failures block (default "open": let through)
  "hooks": "hooks/hooks.json",         // bundle: Claude-format hook declarations
  "mcpServers": "mcp.json",            // bundle: file path or inline server map
  "skills": ["skills/"]                // bundle: skill directories
}
```

A bundle-only manifest (no `command`/`module`) is legal: it contributes
declarative resources without running a plugin process.

## 3. Identity, store, enable/disable

Every plugin has a stable id **`name@source`**:

- `source` is the marketplace name for catalog installs, `user` for
  direct installs, `project` for `.pi/extensions`, `local` for
  `extensionPaths` checkouts.
- Name grammar: 1–64 chars of `[a-z0-9.-]` (no `..`, no leading/trailing
  `.`); source: `[A-Za-z0-9_-]`. Ids are safe filesystem segments by
  construction.

```text
~/.tack/agent/extensions/
├── store/<source>/<name>/<version>/   # versioned installs
├── data/<source>/<name>/              # writable per-plugin data root
└── <name>/                            # legacy flat installs (pre-v3, still loaded)
```

The active version of a store plugin is `local` when present, else the
highest semver directory.

Enable/disable without uninstalling, persisted in settings
(`plugins."<id>".enabled`):

```sh
tack ext disable review@acme
tack ext enable review@acme     # bare "review" works when unambiguous
```

`tack ext list` shows `id`, state (active/disabled), version, layout
(store/legacy), and directory.

## 4. Install, upgrade, verify

```sh
tack ext install <git-url>[#<ref>] | <dir>   # install; #ref pins a tag/branch/sha
tack ext install <plugin>@<marketplace>      # resolved through a catalog (§5)
tack ext install --local <dir>               # install into the project's .pi/extensions
tack ext upgrade [id]                        # re-fetch; no-op when the commit is unchanged
tack ext remove <id>                         # remove store versions + lock entry
tack ext verify                              # audit installed checkouts against the lockfile
```

`--local` installs stay flat under `.pi/extensions` (project-trust gated)
and out of the lockfile and store.

Installs are **atomic**: the source is staged into a sibling scratch
directory, the manifest is parsed and re-read (byte-identical — TOCTOU
package-swap defense), then the staging is renamed into
`store/<source>/<name>/<version>` (an existing version is swapped out to
a backup and rolled back on failure). Git runs scrubbed
(`GIT_TERMINAL_PROMPT=0`, no inherited `GIT_*` config, piped stdio,
timeouts). Superseded versions are pruned; `local` is never pruned.

The store version is: the manifest's `version` when it's a semver, else
the install ref stripped of a leading `v` when that's a semver, else
`local`.

### Lockfile v2

`~/.tack/agent/extensions-lock.json` pins every store install:

```jsonc
{
  "version": 2,
  "plugins": {
    "review@acme": {
      "source": "https://git.acme.com/review.git",
      "rev": "v1.4.2",
      "resolvedCommit": "<sha>",
      "installedAt": 1718000000,
      "version": "1.4.2",
      "store": true,
      "marketplace": "acme"
    }
  }
}
```

A v1 lockfile (bare-name keys, no `store`/`version` fields) is upgraded
in memory on read (`name` ⇒ `name@user`, `store: false`) and rewritten
as v2 on the next install/upgrade. Startup checks (`extensionLockRequired`,
default on) skip a plugin whose checkout drifted from the pinned commit
(HEAD mismatch or dirty tree); `ext verify` reports
ok/changed/not-a-git-repo/missing per plugin and exits non-zero on any
change.

## 5. Marketplaces

A marketplace is a named JSON catalog (`{"name": ..., "plugins":
{"<name>": {"source": ..., "rev": ...}}}`) registered under
`~/.tack/agent/marketplaces/`:

```sh
tack ext marketplace add acme ./acme-marketplace.json   # or an https URL
tack ext marketplace list                               # registered catalogs
tack ext marketplace list acme                          # plugins in a catalog
tack ext marketplace remove acme
```

`<plugin>@<marketplace>` resolves the source through the catalog and
installs under the marketplace key (the plugin id becomes
`<plugin>@<marketplace>`). A catalog entry's optional `"rev"` pins the
git ref.

Catalogs may carry an ed25519 signature (TOFU key pinning): registration
requires `--public-key <hex>` once; the key is pinned to
`<name>.key` and every later resolve/install re-verifies. The signed
payload is the catalog with the top-level `signature` key removed,
reserialized with serde_json (BTreeMap key order).

## 6. Developer tooling

```sh
tack ext new my-plugin rust        # scaffold (rust | ts | python)
tack ext inspect my-plugin         # handshake + dump declared capabilities
tack ext test my-plugin            # run my-plugin/plugin.scenario.json
tack ext dev my-plugin             # run and stream plugin logs (Ctrl-C to stop)
tack ext dev my-plugin s.json      # run a scenario interactively
```

Scenarios are JSON and run against the real protocol (the plugin is
spawned and handshake-done first):

```jsonc
{
  "initialize": { "trusted": true, "config": {} },   // optional overrides
  "steps": [
    { "call": "tools/execute", "params": { "name": "hello.echo", "toolCallId": "c-1",
        "arguments": {"text": "hi"} },
      "expect": { "content": [ { "type": "text" } ] } },
    { "call": "hooks/beforeToolCall", "params": { "toolCall": { "toolCallId": "c-2",
        "toolName": "bash", "arguments": {"command": "rm -rf /"} } },
      "expectError": -32001 },
    { "notify": "events/lifecycle", "params": { "event": "turnStart", "payload": {} } },
    { "expectHostRequest": "ui/select", "respond": "b" },
    { "sleepMs": 50 }
  ]
}
```

`expect` is a recursive subset match; `expectError` asserts the JSON-RPC
error code; `expectHostRequest` scripts the plugin→host answer queue
(dialogs, exec); a failing step exits non-zero.

## 7. Run modes

Plugins load in every run mode. Non-TUI modes degrade deterministically:
`ui/select|confirm|input` answer `ERR_CAPABILITY_NOT_GRANTED`,
`session/*` and `host/registerProvider` answer `ERR_METHOD_NOT_FOUND`,
`ui/notify` goes to the log, `exec/run` stays trust-gated. A plugin
learns the mode and the available surfaces from the initialize payload's
`mode` and `capabilities`.

## 8. Security model (unchanged principles)

- **Crash isolation**: one carrier per plugin; a dead plugin fails its
  pending calls immediately and its widgets vanish.
- **Project trust**: project-local extensions load only after trust;
  `exec/run` is trust-gated.
- **Credential hygiene**: sensitive host env vars are stripped from
  plugin children unless the manifest re-declares them.
- **WASM sandbox**: no preopens/env/network by default; capability
  grants are explicit and audit-logged.
- **Supply chain**: commit pinning + drift skip, ed25519 catalog
  signatures, atomic installs.
- **Fail-open hooks**: hook failures warn and let the call through
  (`failMode: "closed"` flips that per plugin).
