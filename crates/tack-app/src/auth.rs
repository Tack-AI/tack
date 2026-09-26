//! Credential storage: `~/.tack/agent/auth.json`, same shape as TS pi's
//! auth store (`{ provider: { "type": "api_key", "key": "..." } }`).
//!
//! OS keyring integration (settings `credentialStore`: "auto" default |
//! "keyring" | "file"): secrets go to the OS credential store (Windows
//! DPAPI / macOS Keychain / libsecret) when available; auth.json then holds
//! only a `{"type": "keyring"}` placeholder per provider. The file remains
//! the fallback when no keyring service is reachable (headless Linux).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

fn auth_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("auth.json")
}

fn read_store(agent_dir: &Path) -> BTreeMap<String, Value> {
    std::fs::read_to_string(auth_path(agent_dir))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn write_store(agent_dir: &Path, store: &BTreeMap<String, Value>) -> std::io::Result<()> {
    std::fs::create_dir_all(agent_dir)?;
    // 0600 on unix from the first byte (the temp file is created with the
    // mode, so the secret never sits on disk with broader permissions);
    // Windows relies on the profile ACL. Atomic: a crash mid-write keeps
    // the previous store instead of leaving a truncated file that loads
    // as empty defaults (dropping every credential).
    let content = serde_json::to_string_pretty(store)?;
    crate::atomic_write::atomic_write_private(&auth_path(agent_dir), &content, 0o600)
}

/// In-process serializer for read-modify-write cycles on auth.json: two
/// concurrent logins in one process must not lose each other's entry.
/// NOTE: this is only a process-local mutex — two tack PROCESSES writing
/// concurrently still race (last writer wins); the atomic write keeps the
/// file intact either way.
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---------------------------------------------------------------------
// OS keyring
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialStore {
    /// Keyring when reachable, file otherwise (default).
    Auto,
    /// Keyring only; errors surface.
    Keyring,
    /// auth.json only (previous behavior).
    File,
}

fn credential_store_setting(agent_dir: &Path) -> CredentialStore {
    let raw = std::fs::read_to_string(agent_dir.join("settings.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok());
    match raw
        .as_ref()
        .and_then(|r| r.get("credentialStore"))
        .and_then(Value::as_str)
    {
        Some("keyring") => CredentialStore::Keyring,
        Some("file") => CredentialStore::File,
        _ => CredentialStore::Auto,
    }
}

fn keyring_entry(service: &str, provider: &str) -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(service, provider)
}

fn keyring_read(service: &str, provider: &str) -> Option<Value> {
    let secret = keyring_entry(service, provider).ok()?.get_password().ok()?;
    serde_json::from_str(&secret).ok()
}

fn keyring_write(service: &str, provider: &str, credential: &Value) -> std::io::Result<()> {
    let entry = keyring_entry(service, provider)
        .map_err(|e| std::io::Error::other(format!("keyring unavailable: {e}")))?;
    let secret = serde_json::to_string(credential)?;
    entry
        .set_password(&secret)
        .map_err(|e| std::io::Error::other(format!("keyring write failed: {e}")))
}

fn keyring_delete(service: &str, provider: &str) {
    if let Ok(entry) = keyring_entry(service, provider) {
        let _ = entry.delete_credential();
    }
}

/// Store a raw credential entry (used by OAuth login/refresh).
pub fn set_credential(agent_dir: &Path, provider: &str, credential: Value) -> std::io::Result<()> {
    set_credential_with_service(agent_dir, provider, credential, "tack")
}

fn set_credential_with_service(
    agent_dir: &Path,
    provider: &str,
    credential: Value,
    service: &str,
) -> std::io::Result<()> {
    match credential_store_setting(agent_dir) {
        CredentialStore::File => return write_file_entry(agent_dir, provider, &credential),
        CredentialStore::Keyring => {
            keyring_write(service, provider, &credential)?;
            return write_file_placeholder(agent_dir, provider);
        }
        CredentialStore::Auto => {
            if keyring_write(service, provider, &credential).is_ok() {
                return write_file_placeholder(agent_dir, provider);
            }
            tracing::debug!("keyring unavailable for {provider}; falling back to auth.json");
        }
    }
    write_file_entry(agent_dir, provider, &credential)
}

fn write_file_entry(agent_dir: &Path, provider: &str, credential: &Value) -> std::io::Result<()> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = read_store(agent_dir);
    store.insert(provider.to_string(), credential.clone());
    write_store(agent_dir, &store)
}

/// The on-disk marker for a keyring-held credential (keeps list()/status
/// working; the secret itself never touches the file).
fn write_file_placeholder(agent_dir: &Path, provider: &str) -> std::io::Result<()> {
    write_file_entry(agent_dir, provider, &json!({ "type": "keyring" }))
}

/// Read a provider's raw credential entry (any type).
pub fn get_credential(agent_dir: &Path, provider: &str) -> Option<Value> {
    get_credential_with_service(agent_dir, provider, "tack")
}

fn get_credential_with_service(agent_dir: &Path, provider: &str, service: &str) -> Option<Value> {
    let entry = read_store(agent_dir).get(provider).cloned()?;
    // Placeholder → resolve from the keyring.
    if entry.get("type").and_then(Value::as_str) == Some("keyring") {
        return keyring_read(service, provider);
    }
    Some(entry)
}

/// Store an API key for a provider.
pub fn login(agent_dir: &Path, provider: &str, api_key: &str) -> std::io::Result<()> {
    set_credential(
        agent_dir,
        provider,
        json!({ "type": "api_key", "key": api_key }),
    )
}

/// Remove a provider's stored credential. Returns true if one existed.
pub fn logout(agent_dir: &Path, provider: &str) -> std::io::Result<bool> {
    // File mode never touches the OS keyring — skip the delete (it would
    // prompt/keychain-access for nothing; also keeps tests hermetic).
    if credential_store_setting(agent_dir) != CredentialStore::File {
        keyring_delete("tack", provider);
    }
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = read_store(agent_dir);
    let existed = store.remove(provider).is_some();
    write_store(agent_dir, &store)?;
    Ok(existed)
}

/// Read the stored API key for a provider, if any.
pub fn get_api_key(agent_dir: &Path, provider: &str) -> Option<String> {
    get_credential(agent_dir, provider)
        .and_then(|v| v.get("key").cloned())
        .and_then(|v| v.as_str().map(str::to_string))
}

/// Providers with stored credentials (for status display).
pub fn list(agent_dir: &Path) -> Vec<String> {
    read_store(agent_dir).into_keys().collect()
}

/// Read a provider's stored OAuth credential, if the entry is one.
pub fn get_oauth(agent_dir: &Path, provider: &str) -> Option<tack_ai::oauth::OAuthCredential> {
    // Via get_credential so keyring placeholders resolve.
    let entry = get_credential(agent_dir, provider)?;
    if entry.get("type").and_then(Value::as_str) != Some("oauth") {
        return None;
    }
    serde_json::from_value(entry).ok()
}

/// Persist an OAuth credential (wraps in `{ "type": "oauth", ... }`).
pub fn set_oauth(
    agent_dir: &Path,
    provider: &str,
    credential: &tack_ai::oauth::OAuthCredential,
) -> std::io::Result<()> {
    let mut value = serde_json::to_value(credential)?;
    value
        .as_object_mut()
        .expect("credential is an object")
        .insert("type".to_string(), Value::String("oauth".to_string()));
    set_credential(agent_dir, provider, value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Real-OS-keyring integration test. Opt-in via TACK_TEST_KEYRING=1:
    /// it writes/deletes a real keychain item, which can trigger a macOS
    /// Keychain prompt — keep `cargo test` free of keychain access by
    /// default.
    #[test]
    fn keyring_roundtrip_and_placeholder() {
        if std::env::var_os("TACK_TEST_KEYRING").is_none() {
            eprintln!("skipping: set TACK_TEST_KEYRING=1 to run the real-keyring test");
            return;
        }
        // Real OS credential store (Windows DPAPI here); unique service name,
        // cleaned up at the end.
        let tmp = tempfile::tempdir().unwrap();
        let service = format!("tack-test-{}", std::process::id());
        let credential = json!({ "type": "api_key", "key": "sk-test-secret" });

        // Force keyring store via settings.
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"credentialStore": "keyring"}"#,
        )
        .unwrap();
        if let Err(e) =
            set_credential_with_service(tmp.path(), "test-provider", credential.clone(), &service)
        {
            // OS-integration test: skip (don't fail) where the keyring is
            // unreachable — headless CI, or a process tree running under a
            // seatbelt/sandbox that blocks Keychain IPC.
            eprintln!("skipping keyring_roundtrip_and_placeholder: keyring unavailable: {e}");
            return;
        }

        // auth.json holds only the placeholder.
        let on_disk = std::fs::read_to_string(tmp.path().join("auth.json")).unwrap();
        assert!(on_disk.contains("\"keyring\""), "{on_disk}");
        assert!(
            !on_disk.contains("sk-test-secret"),
            "secret leaked to disk: {on_disk}"
        );

        // Reads resolve through the keyring.
        let read = get_credential_with_service(tmp.path(), "test-provider", &service).unwrap();
        assert_eq!(read["key"], "sk-test-secret");

        keyring_delete(&service, "test-provider");
    }

    #[cfg(unix)]
    #[test]
    fn preexisting_file_permissions_are_tightened() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"credentialStore": "file"}"#,
        )
        .unwrap();
        // Simulate a credential file created with broad permissions.
        let path = tmp.path().join("auth.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        login(tmp.path(), "p1", "sk-file").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth.json kept mode {mode:o}");
    }

    #[test]
    fn file_mode_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"credentialStore": "file"}"#,
        )
        .unwrap();
        login(tmp.path(), "p1", "sk-file").unwrap();
        assert_eq!(get_api_key(tmp.path(), "p1").as_deref(), Some("sk-file"));
        assert_eq!(list(tmp.path()), vec!["p1".to_string()]);
        assert!(logout(tmp.path(), "p1").unwrap());
        assert!(get_api_key(tmp.path(), "p1").is_none());
    }
}
