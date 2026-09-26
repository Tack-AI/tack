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
    pub fn new(_mode: &'static str, _trusted: bool) -> Arc<Self> {
        Arc::new(HeadlessExtServices { _private: () })
    }
}
