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
| **2 — MCP server plugin** | `extension.json` with `carrier: "mcp"` declaring one MCP server | tool contribution from the MCP ecosystem, with plugin identity |
| **3 — tack-RPC plugin** | process or WASM-carrier executable speaking tack-RPC v3 | interception, lifecycle, widgets, session control, approval, config, metrics |

Level 3 is what most of this document covers. Plugins are built with an SDK
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
  "carrier": "process",                // "process" (default) | "wasm" | "mcp"
  "module": "plugin.wat",              // wasm carrier: module file (.wasm/.wat,
                                       //   core module or WIT component — auto-detected)
  "mcpServer": {                       // mcp carrier (Level 2): ONE MCP server entry
    "command": "node",                 //   (same shape as an mcp.json server:
    "args": ["server.js"],             //   command/args/env, or url/headers/type/oauth)
    "env": { "DEBUG": "1" }
  },
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

## 2.1 Level 2: MCP server plugins (`carrier: "mcp"`)

A Level-2 plugin's `extension.json` declares exactly one MCP server — the
server **is** the plugin; no tack-RPC process is ever spawned. Any
existing MCP server qualifies (stdio, Streamable HTTP, or legacy SSE,
same entry shape as `mcp.json`). The host connects at load time and
adapts the probe into the plugin model:

- **Capabilities**: the server's tools become the plugin's advertised
  tools; when it has resources, `list_resources` / `read_resource`
  meta-tools join; its prompts join as `prompt__<name>` tools — the same
  surfaces as config-file MCP servers.
- **Identity**: agent-facing tool names are `ext__<plugin-id>__<tool>`
  (the plugin-id prefix is sanitized); attribution, `ext list`, policy,
  and hook interception treat them exactly like any other plugin tool.
  Interception is uniform: another plugin's `hooks/beforeToolCall` sees
  these calls like any other.
- **Untrusted-content defense**: tool results are wrapped in
  `<untrusted_content>` and set the session's untrusted flag (permission
  elevation), identical to config-file MCP tools.
- **stdio resolution**: the server runs with the extension directory as
  cwd; a path-like relative `command` (containing `/` or starting with
  `.`) resolves against it. Arguments are never rewritten (npm package
  names like `@scope/pkg` contain `/` but are not paths).
- **Sampling / elicitation**: elicitation follows the run mode (TUI
  prompts, headless modes decline). Sampling is **not** wired for plugin
  connections — the session model does not exist at load time — so a
  server's sampling request gets method-not-found (documented Level-2
  limitation).
- **Failure as data**: a server that fails to connect or probe lands in
  `ext list` with its error (`LoadedPlugin.error`), like any other
  plugin. Shutdown cancels the connection (server child killed).

Capabilities MCP cannot express (hooks, widgets, session control, …)
are never declared; a host bug calling one gets
`ERR_CAPABILITY_NOT_GRANTED`.

## 2.2 WASM carriers: WASI stdio and WIT component

`carrier: "wasm"` covers two module formats, auto-detected from the
module bytes (or the `(component` text form):

- **WASI-stdio core module** (the debug carrier): the plugin speaks the
  same tack-RPC v3 NDJSON protocol over WASI stdin/stdout; host-side it
  shares the JSON-RPC peer with the process carrier. Manifest
  `capabilities` (fs preopens, env, args) are honored here.
- **WIT component** (the distribution carrier, `tack:plugin@0.3.0` —
  [`protocol/wit/tack-plugin.wit`](../protocol/wit/tack-plugin.wit)):
  the component exports `tack:plugin/tools` and/or `tack:plugin/hooks`
  with JSON-string payloads carrying the rpc3 data types (the OpenRPC
  schema stays the single source of truth; wit-bindgen handles the
  string framing in any guest language). The world imports no WASI
  interfaces — the sandbox (no fs, no env, no network) is structural,
  and WASI capability grants are ignored with a warning. Calls are
  synchronous in 0.3.0 (per-call fuel, wall-clock, and memory limits,
  same ceilings as the stdio carrier); a trapping call kills the plugin.

The component's capability list is probed at spawn: `tools.list` (rpc3
`ToolSpec[]` JSON) plus the presence of `hooks.before-tool-call`.
Subsets are valid plugins — a hooks-only component exports just
`tack:plugin/hooks`. See
[`examples/extensions/hello-component/`](../examples/extensions/hello-component/)
for a hand-written component-WAT reference (guest SDKs come from
wit-bindgen toolchains, not from us).

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
  grants are explicit and audit-logged. WIT component plugins are
  capability-free by construction (the world has no WASI).
- **Untrusted content**: MCP tool results (config-file servers and
  Level-2 MCP plugins alike) are wrapped and flagged for the permission
  layer.
- **Supply chain**: commit pinning + drift skip, ed25519 catalog
  signatures, atomic installs.
- **Fail-open hooks**: hook failures warn and let the call through
  (`failMode: "closed"` flips that per plugin).

## 9. Enterprise policy (managed `pluginPolicy`)

Organizations constrain the plugin surface through the **managed
settings layer** (the highest-authority settings file — see
[configuration.md](configuration.md); users and projects cannot set or
weaken policy):

```jsonc
// /etc/tack/managed-settings.json (Linux; path varies by OS)
{
  "pluginPolicy": {
    // Only plugins explicitly named in `plugins` may load at all.
    "managedPluginsOnly": false,
    // Where plugin bits may come from. Empty/absent = no restriction.
    "allowedSources": [
      // Exact git URL; an optional "ref" only allows installs pinned
      // to exactly that ref (an unpinned clone is denied).
      { "type": "git", "url": "https://git.acme.com/tack/plugins.git", "ref": "main" },
      // Regex matched against the source URL's host (https and
      // scp-style git@host: sources).
      { "type": "hostPattern", "pattern": "^(.+\\.)?acme\\.com$" },
      // Local-directory sources must be this path or live beneath it.
      { "type": "local", "path": "/opt/acme/tack-ext" }
    ],
    "plugins": {
      "review@acme": {
        // Managed `enabled` wins over the user/project layers in both
        // directions (force-on or force-off).
        "enabled": true,
        // Narrow-only intersections with what the plugin registers —
        // they shrink the surface, never expand it:
        "mcpServers": ["jira"],      // bundle MCP servers kept
        "tools": ["create_ticket"]   // registered tools kept
      }
    }
  }
}
```

The policy is enforced **twice**:

1. **At install time** — the source is checked against `allowedSources`
   before any clone or network access; after the manifest is parsed
   (the plugin id is known) but before activation, `managedPluginsOnly`
   membership and a managed `enabled: false` deny the install. Denials
   name the rule and the layer they came from. `tack ext upgrade`
   re-checks the locked source before re-fetching.
2. **At load time** (the backstop) — the discovered set is filtered
   before any carrier starts: unlisted plugins under
   `managedPluginsOnly`, and plugins whose locked source (store
   installs) or directory (user/project/extensionPaths checkouts)
   matches no `allowedSources` rule, so a hand-edited store or
   lockfile cannot smuggle in an unapproved source. Tool and MCP-server
   narrowing is applied at registration, so every downstream consumer
   is compliant by construction.

Policy-blocked plugins are **rows, not absences**: `tack ext list`
shows them as `policy-blocked (<reason>)`. Every policy decision —
deny, filter, narrow, `enabled` override — is audit-logged as a
structured tracing event (target `plugin_policy`) naming the rule and
the origin layer; with a managed `auditSink` configured those events
are shipped to the organization collector. `tack ext enable|disable`
warns when the managed layer pins the opposite value.
