//! Unified multi-provider LLM API.
//!
//! Rust port of `@earendil-works/pi-ai`. Contains the uniform message model,
//! the streaming event protocol, and per-API provider adapters. No agent logic.
//!
//! Key contract (mirrors pi): provider streams never fail fatally — request or
//! runtime errors are encoded in-band as an `Error` event plus a final
//! `AssistantMessage` with `stop_reason: StopReason::Error`.

pub mod api;
pub mod codebuddy;
pub(crate) mod codebuddy_jsonl;
pub mod constrained_sampling;
pub mod env_keys;
pub mod images;
pub mod json_repair;
pub mod local_providers;
pub mod oauth;
pub mod overflow;
pub mod provider;
pub mod providers;
pub mod retry;
pub mod stream;
pub mod tls;
pub mod transcript;
pub mod transform;
pub mod types;

pub use provider::{CacheRetention, Provider, StreamOptions, ToolChoice, provider_for};
pub use stream::{
    AssistantMessageEvent, AssistantMessageEventSender, AssistantMessageEventStream, EventSender,
    EventStream, event_stream,
};
pub use types::*;
