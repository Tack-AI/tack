//! tack-RPC v3: the schema-first host↔plugin protocol.
//!
//! - [`peer`]: transport-agnostic JSON-RPC 2.0 peer (NDJSON framing,
//!   both-directions requests, cancellation, timeouts, dead-peer
//!   semantics) shared by the host and the SDK.
//! - [`host`]: the host-side typed client (handshake + capability
//!   namespace calls) built on the generated [`crate::rpc3`] types.
//!
//! The protocol's single source of truth is
//! `protocol/tack-rpc.openrpc.json`; see `docs/plugin-roadmap.md`.

pub mod host;
pub mod peer;
pub mod process;

pub use host::HostClient;
pub use peer::{JsonRpcPeer, PeerError, PeerHandler};
pub use process::V3Process;

/// The tack-RPC protocol version this crate speaks (semver; the major
/// version must match on both sides of the handshake).
pub const PROTOCOL_VERSION: &str = "3.0.0";

/// Major/minor of a semver-ish `"x.y.z"` string (a missing patch is
/// tolerated; anything unparseable is None).
fn major_minor(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

/// Handshake version check: the peer's major must equal ours and its
/// minor must not exceed ours (a newer-minor peer may rely on behavior
/// this side does not implement).
pub fn protocol_compatible(peer_version: &str) -> bool {
    let Some((peer_major, peer_minor)) = major_minor(peer_version) else {
        return false;
    };
    let Some((our_major, our_minor)) = major_minor(PROTOCOL_VERSION) else {
        return false;
    };
    peer_major == our_major && peer_minor <= our_minor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_compatibility_rules() {
        assert!(protocol_compatible("3.0.0"));
        assert!(!protocol_compatible("2.0.0"), "older major rejected");
        assert!(!protocol_compatible("4.0.0"), "newer major rejected");
        assert!(!protocol_compatible("3.1.0"), "newer minor rejected");
        assert!(!protocol_compatible("garbage"));
    }
}
