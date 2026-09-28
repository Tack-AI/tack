# tack extension examples

The examples moved with the plugin system redesign (tack-RPC v3, see
`docs/plugin-roadmap.md`):

- **Rust SDK**: `crates/tack-ext-sdk/examples/hello_rpc3.rs` and the
  built-in `crates/tack-ext-sdk/src/bin/tack-v3-demo-plugin.rs`.
- **TypeScript / Python SDKs**: `sdk/typescript`, `sdk/python`.
- **WASM carrier (hand-written WAT protocol references)**: the
  `hello-wasm/` and `hello-wasm-caps/` directories here.
- **Scaffolding**: `tack ext new <dir> <rust|ts|python>` generates a
  starter plugin; `tack ext inspect|dev|test` drives it.

The old v1/v2 examples (hello-js, git-checkpoint, handoff,
protected-paths) were removed with the v1/v2 protocol.
