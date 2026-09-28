# hello-component

A tack plugin as a **WIT component** (`tack:plugin@0.3.0`, hand-written
component WAT) — the sandboxed distribution carrier added in the plugin
redesign P4. Same `carrier: "wasm"` manifest as the WASI-stdio carrier;
the host detects the component format and drives it via the
`tack:plugin/tools` + `tack:plugin/hooks` exports instead of a JSON-RPC
stdio protocol. See `protocol/wit/tack-plugin.wit` for the contract.

Point tack at it via the `extensionPaths` setting (or drop it into a
project's `.pi/extensions/`):

```jsonc
// ~/.tack/agent/settings.json
{ "extensionPaths": ["path/to/examples/extensions/hello-component"] }
```

Then call the `ext__hello-component_local__hello` tool, or inspect the
plugin with `tack ext list`. Real-world guests are wit-bindgen-generated
(Rust/Go/JS/Python); the hand-written WAT here doubles as the carrier's
test fixture.
