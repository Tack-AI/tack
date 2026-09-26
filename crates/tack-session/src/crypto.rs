//! Session-at-rest encryption (settings `sessionEncryption: true`).
//!
//! Entry lines (everything after the plaintext header) are encrypted
//! AES-256-GCM with a process-global key (the host resolves it from the OS
//! keyring and calls `set_session_key`). Encrypted lines are
//! `tack-enc:v1:<base64(nonce‖ciphertext)>`; plaintext sessions stay
//! readable, and a mixed file decrypts the encrypted lines and passes the
//! rest through — so turning the flag on/off never bricks history.

use std::sync::OnceLock;

use aes_gcm::aead::{Aead as _, KeyInit as _};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;

const PREFIX: &str = "tack-enc:v1:";

static SESSION_KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Install the process-global session key (host: resolve from OS keyring).
///
/// The key is installed ONCE per process: there is no re-keying support —
/// existing ciphertext is only decryptable with the key that produced it,
/// and every read path (`decrypt_line`, `SessionLine::parse`, `open`) uses
/// this one global key. Later calls are therefore ignored rather than
/// swapping the key under live sessions. Re-installing the SAME key is a
/// harmless no-op; a DIFFERENT key logs a warning, because the caller's
/// key rotation / profile switch silently did not take effect.
pub fn set_session_key(key: [u8; 32]) {
    if let Err(returned) = SESSION_KEY.set(key) {
        let installed = SESSION_KEY.get().expect("set failed => a key is installed");
        if *installed == returned {
            tracing::debug!("session key re-installed with the identical value (no-op)");
        } else {
            tracing::warn!(
                "session key already installed; ignoring a DIFFERENT key — \
                 session encryption keeps using the original key (no re-keying support)"
            );
        }
    }
}

pub fn session_key() -> Option<[u8; 32]> {
    SESSION_KEY.get().copied()
}

pub fn is_encrypted_line(line: &str) -> bool {
    line.starts_with(PREFIX)
}

/// True when the content carries an encrypted line that cannot be
/// decrypted (no key installed, wrong key, or tampered ciphertext). `open`
/// rejects such files with `SessionError::Encrypted` instead of silently
/// dropping the encrypted history.
pub(crate) fn has_undecryptable_encrypted_line(content: &str) -> bool {
    content
        .lines()
        .map(str::trim)
        .any(|line| is_encrypted_line(line) && decrypt_line(line).is_none())
}

/// Encrypt one plaintext JSON line. None when no key is installed.
#[allow(deprecated)] // aes-gcm 0.10 pins generic-array 0.14 (from_slice deprecation is cosmetic)
pub fn encrypt_line(plaintext: &str) -> Option<String> {
    let key = SESSION_KEY.get()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext.as_bytes()).ok()?;
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ciphertext);
    Some(format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(blob)
    ))
}

/// Decrypt an encrypted line; None when it isn't encrypted or the key is
/// missing/wrong (callers skip the line).
#[allow(deprecated)]
pub fn decrypt_line(line: &str) -> Option<String> {
    let blob = line.strip_prefix(PREFIX)?;
    let key = SESSION_KEY.get()?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(blob.trim())
        .ok()?;
    if blob.len() < 13 {
        return None;
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .ok()?;
    String::from_utf8(plaintext).ok()
}

/// Test-only helpers: the SESSION_KEY OnceLock is process-global, so
/// every test in this binary must install the SAME fixed key — that way
/// no test depends on execution order or on which test ran first.
#[cfg(test)]
pub(crate) const TEST_KEY: [u8; 32] = [42u8; 32];

#[cfg(test)]
pub(crate) fn install_test_key() {
    set_session_key(TEST_KEY);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn roundtrip_and_prefix_detection() {
        install_test_key();
        let line =
            r#"{"type":"message","id":"m1","message":{"role":"user","content":"secret plan"}}"#;
        let encrypted = encrypt_line(line).unwrap();
        assert!(is_encrypted_line(&encrypted));
        assert!(
            !encrypted.contains("secret plan"),
            "plaintext leaked: {encrypted}"
        );
        assert_eq!(decrypt_line(&encrypted).as_deref(), Some(line));
    }

    #[test]
    fn non_encrypted_lines_pass_through() {
        assert!(!is_encrypted_line(r#"{"type":"message"}"#));
        assert!(decrypt_line(r#"{"type":"message"}"#).is_none());
    }

    /// The OnceLock semantics are deliberate: a different key never
    /// replaces the installed one (no re-keying support — a warning is
    /// logged), while re-installing the identical key is a no-op.
    #[test]
    fn set_session_key_ignores_later_different_keys() {
        install_test_key();
        set_session_key(TEST_KEY); // identical: no-op
        set_session_key([9u8; 32]); // different: ignored (+ warning)
        assert_eq!(session_key(), Some(TEST_KEY));
    }

    /// A tampered ciphertext fails AEAD. (A genuinely wrong/missing key
    /// cannot be exercised here: the OnceLock keeps the first installed
    /// key for the whole test binary — see TEST_KEY.)
    #[test]
    fn tampered_ciphertext_drops_line() {
        install_test_key();
        let encrypted = encrypt_line("hello").unwrap();
        let mut blob = encrypted.clone();
        let pos = blob.len() - 4;
        blob.replace_range(
            pos..pos + 1,
            if blob.as_bytes()[pos] == b'A' {
                "B"
            } else {
                "A"
            },
        );
        assert!(decrypt_line(&blob).is_none());
    }
}

#[cfg(test)]
mod e2e_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::entry::SessionLine;

    /// Encrypted lines written to a session file parse back through the
    /// normal SessionLine::parse path (what SessionManager::open uses).
    #[test]
    fn encrypted_session_lines_parse_back() {
        install_test_key(); // same fixed key as every test in this binary
        let entry_json = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-29T00:00:00Z","message":{"role":"user","content":"top secret","timestamp":1}}"#;
        let encrypted = encrypt_line(entry_json).unwrap();
        let parsed = SessionLine::parse(&encrypted).expect("decrypts");
        let SessionLine::Entry(crate::SessionEntry::Message { message, .. }) = parsed else {
            panic!("expected message entry");
        };
        let tack_agent_core::AgentMessage::User(u) = message else {
            panic!()
        };
        let tack_ai::UserContent::Text(text) = u.content else {
            panic!()
        };
        assert_eq!(text, "top secret");
    }
}
