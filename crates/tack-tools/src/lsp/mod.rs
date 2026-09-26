//! LSP integration: a minimal stdio JSON-RPC client per language server,
//! kept alive across tool calls. Documents sync via `didOpen`/full
//! `didChange`; an LRU cap `didClose`s the least-recently-synced documents
//! so long sessions don't grow server state unboundedly.
//!
//! Agent-oriented protocol subset: diagnostics (with rust-analyzer
//! serverStatus/progress tracking so "clean syntax pass" is never reported
//! as "fully type-checked"), hover, definition, references, implementation,
//! document/workspace symbols, call hierarchy (incoming/outgoing calls),
//! name-based position resolution (workspace/symbol exact match), code
//! actions (quickfixes applied as WorkspaceEdits) and rename. No
//! completion/signatureHelp/etc. — those serve human-driven editing.
//! Servers are auto-detected by file extension (default table below) and
//! can be overridden per-extension by the host app (settings `lspServers`).
//! Missing binaries disable that extension silently; a server that crashes
//! mid-session is transparently respawned (bounded restart budget).

mod client;
mod convert;
mod manager;
mod tool;

pub use client::{AnalysisState, Diag, LspClient};
pub use convert::{CallSite, Location, SymbolInfo, WorkspaceSymbol, path_to_uri};
pub use manager::{LspManager, ServerSpec, format_diagnostics, post_edit_summary};
pub use tool::{DiagnosticsTool, LspTool};
