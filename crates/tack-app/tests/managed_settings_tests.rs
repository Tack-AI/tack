//! Managed settings layer: managed file wins over global/project, deny lists
//! union across layers, features can be forced either direction.
//!
//! Own test binary: TACK_AGENT_DIR/TACK_MANAGED_SETTINGS are
//! process-global, and parallel tests within one binary would race them.
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use tack_app::settings::Settings;

#[test]
fn managed_layer_wins_and_denies_union() {
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agent");
    let managed = tmp.path().join("managed.json");
    let cwd = tmp.path().join("work");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{
            "sandbox": "off",
            "features": { "cron": false },
            "permissions": { "deny": ["Bash(user-level *)"] }
        }"#,
    )
    .unwrap();
    std::fs::write(
        &managed,
        r#"{
            "features": { "sandbox": true, "lsp": false, "cron": true },
            "disableBypass": true,
            "lockedProvider": "anthropic",
            "permissions": { "deny": ["Bash(org-level *)"] }
        }"#,
    )
    .unwrap();

    unsafe {
        std::env::set_var("TACK_AGENT_DIR", &agent_dir);
        std::env::set_var("TACK_MANAGED_SETTINGS", &managed);
    }
    let settings = Settings::load(&cwd, &agent_dir);

    assert!(settings.managed_active);
    assert!(
        settings.sandbox,
        "managed forces sandbox on over global off"
    );
    assert!(!settings.features.lsp, "managed forces lsp off");
    assert!(
        settings.features.cron,
        "managed forces cron ON over global off"
    );
    assert!(settings.disable_bypass);
    assert_eq!(settings.locked_provider.as_deref(), Some("anthropic"));
    // Deny rules union across layers.
    assert!(
        settings
            .permission_deny
            .iter()
            .any(|r| r.contains("user-level"))
    );
    assert!(
        settings
            .permission_deny
            .iter()
            .any(|r| r.contains("org-level"))
    );

    // Without a managed file: global values alone.
    unsafe {
        std::env::set_var(
            "TACK_MANAGED_SETTINGS",
            tmp.path().join("does-not-exist.json"),
        );
    }
    let plain = Settings::load(&cwd, &agent_dir);
    assert!(!plain.managed_active);
    assert!(!plain.sandbox, "global off restored without managed file");
    assert!(!plain.features.cron, "global cron=false restored");
    assert!(plain.features.lsp);
    assert!(plain.locked_provider.is_none());

    // The env override resolves the managed path in this (debug) test
    // binary. Release builds ignore TACK_MANAGED_SETTINGS entirely
    // (managed_settings_env_override in settings.rs gates on
    // cfg!(debug_assertions)) so the managed control plane — sole source
    // of pluginPolicy / disableBypass / lockedProvider — cannot be
    // disengaged by a process env var; that path is compile-time gated
    // and not exercisable from a debug test binary.
    if !cfg!(debug_assertions) {
        panic!("tests must run in a debug build for the env override to apply");
    }
    assert_eq!(
        tack_app::settings::managed_settings_path(),
        tmp.path().join("does-not-exist.json")
    );
}
