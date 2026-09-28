//! tack-ext: subprocess plugin host for tack. Plugins are executables
//! speaking NDJSON (see `protocol`) over stdio; the host spawns one process
//! per plugin, forwards agent events, and bridges tool calls / UI dialogs.

pub mod hooks;
pub mod process;
pub mod protocol;
pub mod provider_events;
pub mod rpc3;
pub mod tool;

pub use hooks::{ExtHooks, FailMode};
pub use process::{HostServices, PluginPeer, PluginProcess};
pub use protocol::*;
pub use provider_events::{EventSink, ExtNotifyProvider};
pub use tool::ExtTool;
