//! No-op `ext_provider_bridge` replacement for builds without the `ext`
//! feature (aliased as `crate::ext_provider_bridge` from lib.rs). Only the
//! state type exists so service/manager signatures compile unchanged;
//! without extensions no plugin exists to serve a provider.

use std::sync::Arc;

/// Provider bridge state (the real impl tracks plugin connections,
/// in-flight stream sinks, and provider registrations).
#[derive(Debug, Default)]
pub struct ProviderBridgeState {
    _private: (),
}

impl ProviderBridgeState {
    pub fn shared() -> Arc<Self> {
        Arc::new(ProviderBridgeState { _private: () })
    }
}
