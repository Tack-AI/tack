//! tack-ext: the tack-RPC v3 plugin host. Plugins are executables (or
//! WASI modules via tack-ext-wasm) speaking JSON-RPC 2.0 over NDJSON
//! stdio; the host spawns one carrier per plugin, forwards agent events,
//! and bridges tool calls / hooks / UI dialogs.
//!
//! The protocol's single source of truth is
//! `protocol/tack-rpc.openrpc.json` (generated types in [`rpc3`]); the
//! redesign rationale and plugin levels are in `docs/plugin-roadmap.md`.

pub mod hooks;
pub mod plugin_id;
pub mod process;
pub mod provider_events;
pub mod rpc3;
pub mod tool;
pub mod v3;

pub use hooks::{ExtHooks, FailMode};
pub use plugin_id::PluginId;
pub use provider_events::{EventSink, ExtNotifyProvider};
pub use tool::ExtTool;
pub use v3::{HostClient, JsonRpcPeer, PeerError, PeerHandler, PluginConnection, V3Process};
