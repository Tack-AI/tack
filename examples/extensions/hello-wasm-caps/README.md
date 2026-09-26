# hello-wasm-caps

Like [hello-wasm](../hello-wasm/), but demonstrates **WASM capability
grants**: the sandbox is full by default (no fs/network/env), and
`extension.json` explicitly widens it:

```json
"capabilities": {
  "fs": [{ "host": "data", "guest": "/data", "access": "read-only" }]
}
```

The extension's `data/` directory (relative `host` resolves against the
extension dir) is preopened into the guest as its first preopen (fd 3). The
`readfile` tool opens `hello.txt` relative to it and returns the content.
Every grant is audit-logged by the host at load time (`wasm fs grant ...`
warnings).

It registers two tools (`ext__hello-wasm-caps__ping`,
`ext__hello-wasm-caps__readfile`) and one slash command
(`/hello-wasm-caps`).

## Try it

```sh
tack ext install examples/extensions/hello-wasm-caps
tack --verbose   # watch the "wasm fs grant" audit line at startup
# in the TUI: ask the model to call the readfile tool
```

To see the sandbox boundary: delete the `capabilities` block (or point
`guest` elsewhere) and the same `readfile` call answers
`read failed: errno ...` — there is no ambient authority to fall back on.

## Manifest capability fields

| Field | Notes |
|---|---|
| `capabilities.fs[].host` | Host directory. Relative resolves against the extension dir; must exist. |
| `capabilities.fs[].guest` | Path the guest sees (cosmetic for WASI p1 — guests address preopens by fd, in declaration order: 3, 4, ...). Required. |
| `capabilities.fs[].access` | `"read-only"` (default) \| `"read-write"`. |
| `capabilities.env` | `{"K": "V"}` injects literal values; `["K"]` passes the host's current value through (unset vars skipped). |
| `capabilities.args` | argv visible to the guest. |
| `capabilities.network` | `{tcp, udp, dns}` flags — inert for WASI p1 guests today (no socket ABI in wasmtime-wasi p1); forward-compatible. |

Trust model: user-dir extensions are trusted like any installed program;
project extensions (`.pi/extensions/`) only load under project trust — so
their grants are gated too. Grants can only ever be what the manifest
declares; malformed entries are skipped with a warning, never widened.
