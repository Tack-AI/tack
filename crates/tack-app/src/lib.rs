//! tack-app library surface (the binary is a thin wrapper in main.rs).

pub mod acp;
pub mod agents;
pub mod ask_user;
pub mod atomic_write;
pub mod auth;
pub mod catalog_refresh;
pub mod changelog;
pub mod cli_flags;
pub mod cli_output;
pub mod cron;
pub mod debug_image;
pub mod doctor;
pub mod eval;
#[cfg(feature = "ext")]
pub mod ext_dev;
#[cfg(not(feature = "ext"))]
#[doc(hidden)]
pub mod ext_dev_stub;
#[cfg(not(feature = "ext"))]
pub use ext_dev_stub as ext_dev;
#[cfg(feature = "ext")]
pub mod ext_headless;
#[cfg(not(feature = "ext"))]
#[doc(hidden)]
pub mod ext_headless_stub;
#[cfg(not(feature = "ext"))]
pub use ext_headless_stub as ext_headless;
#[cfg(feature = "ext")]
pub mod extension_host;
#[cfg(not(feature = "ext"))]
#[doc(hidden)]
pub mod extension_host_stub;
#[cfg(not(feature = "ext"))]
pub use extension_host_stub as extension_host;
pub mod hooks;
pub mod i18n;
pub mod logs;
pub mod mcp_config;
pub mod mcp_elicitation;
pub mod mcp_oauth;
#[cfg(feature = "ext")]
pub mod mcp_plugin;
pub mod mcp_sampling;
pub mod mcp_serve;
pub mod model;
pub mod oauth_login;
pub mod observability;
pub mod permissions;
pub mod print_mode;
pub mod project_trust;
pub mod prompt_templates;
pub mod remote;
pub mod remote_client;
pub mod remote_tls;
pub mod remote_ws;
pub mod resources;
pub mod rpc;
pub mod self_update;
pub mod session_search_tool;
pub mod settings;
pub mod shell_hooks;
pub mod skills;
pub mod stats;
pub mod subagent_tool;
mod sync_process;
pub mod system_prompt;
pub mod task;
pub mod tools_manager;
pub mod tui;
