//! No-op `ext_headless` replacement for builds without the `ext` feature
//! (aliased as `crate::ext_headless` from lib.rs).

use std::sync::Arc;

/// Headless host services (real impl degrades plugin UI/exec requests for
/// print/rpc/acp modes). Without extensions no plugin exists to serve.
#[derive(Debug)]
pub struct HeadlessExtServices {
    _private: (),
}

impl HeadlessExtServices {
    pub fn new(
        _mode: &'static str,
        _trusted: bool,
        _bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
    ) -> Arc<Self> {
        Arc::new(HeadlessExtServices { _private: () })
    }

    /// Remote-mode constructor (real impl routes plugin UI to clients).
    /// Without extensions no plugin exists to serve, so the bridge is
    /// accepted and dropped.
    pub fn new_with_remote(
        _mode: &'static str,
        _trusted: bool,
        _bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
        _remote: Arc<crate::remote::ext_bridge::RemoteExtBridge>,
    ) -> Arc<Self> {
        Arc::new(HeadlessExtServices { _private: () })
    }
}
