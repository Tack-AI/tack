# hello-wasm

A tack-ext plugin running in the **WASM sandbox carrier** (`carrier: "wasm"`):
a hand-written WASI p1 module (`plugin.wat`) speaking the exact same NDJSON
protocol as subprocess plugins, but fully sandboxed by wasmtime — no file
system, no network, no environment, with fuel/memory limits from
`extension.json`.

It registers one tool (`ext__hello-wasm__ping`) and one slash command
(`/hello-wasm`).

## Try it

```sh
tack ext install examples/extensions/hello-wasm   # or copy to ~/.tack/agent/extensions/
tack
# in the TUI: /hello-wasm, or ask the model to call the ping tool
```

## Writing a real plugin

`plugin.wat` is a protocol reference (WAT is painful for real logic). A real
plugin is any language compiling to `wasm32-wasip1` — e.g. Rust:

```sh
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1 --release
# extension.json: { "name": "my-ext", "carrier": "wasm", "module": "target/wasm32-wasip1/release/my_ext.wasm" }
```

The protocol is identical to subprocess plugins (see `docs/extensions.md`):
read lines on stdin, write NDJSON envelopes on stdout. The only differences
are the sandbox (no ambient fs/net/env) and `initialize.payload.protocol: 2`.

## Manifest fields

| Field | Notes |
|---|---|
| `carrier` | `"wasm"` selects the wasmtime carrier (default `"process"`). |
| `module` | `.wasm` or `.wat` file, resolved against the extension dir. |
| `limits.maxFuel` | CPU instruction budget (default 1e9). |
| `limits.maxMemoryBytes` | Linear memory cap (default 256MB). |
| `limits.maxExecutionMs` | Wall-clock cap for the guest (default: none). |
