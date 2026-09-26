//! Process-wide rustls crypto provider.
//!
//! Workspace reqwest is built with `rustls-no-provider` (its `rustls` feature
//! forces the aws-lc-rs C toolchain), so *someone* must install a
//! `CryptoProvider` before the first TLS client is constructed or rustls
//! panics. Call [`ensure_ring_provider`] at process startup and before
//! building ad-hoc reqwest clients in library code.

/// Install ring as the process-default rustls `CryptoProvider`.
///
/// Idempotent and race-safe: if another provider is already installed the
/// error is ignored (the existing provider wins).
pub fn ensure_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// Belt and suspenders: run at load time of any binary/test that links tack-ai,
// so even reqwest clients built outside the ensured call sites (tests,
// examples, downstream users) never hit the no-provider panic.
#[ctor::ctor(unsafe)]
fn install_ring_provider_at_load() {
    ensure_ring_provider();
}
