//! Enterprise plugin policy end-to-end (P5): the managed settings
//! layer's `pluginPolicy` gates installs (before any clone/network
//! access and again before activation) and filters the load set.
//!
//! Own test binary: TACK_MANAGED_SETTINGS is process-global, and
//! parallel tests within one binary would race it (same pattern as
//! managed_settings_tests.rs) — so this is one sequential test.
#![cfg(feature = "ext")]
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use std::sync::Arc;

use tack_app::extension_host::ExtensionManager;
use tack_ext::v3::PeerHandler;

struct NoopServices;

#[async_trait::async_trait]
impl PeerHandler for NoopServices {}

#[tokio::test(flavor = "multi_thread")]
async fn managed_policy_gates_install_and_load() {
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agent");
    let cwd = tmp.path().join("work");
    let managed = tmp.path().join("managed.json");
    let approved_root = tmp.path().join("approved");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&approved_root).unwrap();

    // An approvable plugin source under the approved root, and the same
    // plugin outside it.
    let good_src = approved_root.join("good");
    let bad_src = tmp.path().join("elsewhere").join("good");
    for dir in [&good_src, &bad_src] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("extension.json"),
            r#"{"name": "good", "command": "definitely-not-a-real-command-xyz"}"#,
        )
        .unwrap();
    }
    let blocked_src = approved_root.join("blocked");
    std::fs::create_dir_all(&blocked_src).unwrap();
    std::fs::write(blocked_src.join("extension.json"), r#"{"name": "blocked"}"#).unwrap();

    std::fs::write(
        &managed,
        format!(
            r#"{{
                "pluginPolicy": {{
                    "allowedSources": [
                        {{ "type": "local", "path": "{}" }}
                    ],
                    "plugins": {{
                        "blocked@user": {{ "enabled": false }}
                    }}
                }}
            }}"#,
            approved_root.display()
        ),
    )
    .unwrap();
    unsafe {
        std::env::set_var("TACK_MANAGED_SETTINGS", &managed);
    }

    // 1. Install-time source check: a source outside the allow-list is
    //    denied BEFORE anything is copied, and the denial names the
    //    rule and its origin layer.
    let err = tack_app::extension_host::install_extension(
        bad_src.to_str().unwrap(),
        &cwd,
        &agent_dir,
        false,
    )
    .unwrap_err();
    let message = format!("{err}");
    assert!(
        message.contains("blocked by managed plugin policy"),
        "{message}"
    );
    assert!(message.contains(managed.to_str().unwrap()), "{message}");
    assert!(message.contains("allowedSources"), "{message}");
    assert!(
        !tack_app::extension_host::list_extensions(&cwd, &agent_dir)
            .iter()
            .any(|info| info.id == "good@user"),
        "a denied install leaves nothing behind"
    );

    // 2. The same plugin from an approved source installs.
    tack_app::extension_host::install_extension(
        good_src.to_str().unwrap(),
        &cwd,
        &agent_dir,
        false,
    )
    .unwrap();

    // 3. A managed `enabled: false` plugin is denied after staging
    //    (before activation) and the staging directory is cleaned up.
    let err = tack_app::extension_host::install_extension(
        blocked_src.to_str().unwrap(),
        &cwd,
        &agent_dir,
        false,
    )
    .unwrap_err();
    assert!(
        format!("{err}").contains("disabled by managed policy"),
        "{err}"
    );
    let store = agent_dir.join("extensions").join("store").join("user");
    let leftovers: Vec<_> = std::fs::read_dir(&store)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(".staging"))
        .collect();
    assert!(leftovers.is_empty(), "staging cleaned up: {leftovers:?}");

    // 4. Load: the installed plugin passes the policy filter (the
    //    carrier itself fails to spawn, which is a normal load error,
    //    not a policy block).
    let manager = ExtensionManager::load(
        &cwd,
        &agent_dir,
        "tui",
        Arc::new(NoopServices),
        false,
        Default::default(),
    )
    .await;
    let good = manager
        .plugins
        .iter()
        .find(|p| p.id.to_string() == "good@user")
        .expect("good@user discovered");
    assert!(good.policy_block.is_none(), "{good:?}");
    assert!(good.error.is_some(), "bogus command fails the spawn");

    // 5. Tighten the policy: managedPluginsOnly + the managed layer
    //    force-enables good@user over the user's explicit disable.
    //    A legacy flat plugin not in the managed map is filtered out.
    let flat = agent_dir.join("extensions").join("sketchy");
    std::fs::create_dir_all(&flat).unwrap();
    std::fs::write(
        flat.join("extension.json"),
        r#"{"name": "sketchy", "command": "definitely-not-a-real-command-xyz"}"#,
    )
    .unwrap();
    tack_app::extension_host::set_plugin_enabled(&agent_dir, "good@user", false).unwrap();
    std::fs::write(
        &managed,
        r#"{
            "pluginPolicy": {
                "managedPluginsOnly": true,
                "plugins": {
                    "good@user": { "enabled": true }
                }
            }
        }"#,
    )
    .unwrap();

    let manager = ExtensionManager::load(
        &cwd,
        &agent_dir,
        "tui",
        Arc::new(NoopServices),
        false,
        Default::default(),
    )
    .await;
    let good = manager
        .plugins
        .iter()
        .find(|p| p.id.to_string() == "good@user")
        .expect("good@user discovered");
    assert!(
        good.enabled,
        "managed enabled=true wins over the user disable"
    );
    assert!(good.policy_block.is_none(), "{good:?}");
    let sketchy = manager
        .plugins
        .iter()
        .find(|p| p.id.to_string() == "sketchy@user")
        .expect("sketchy@user discovered");
    let reason = sketchy.policy_block.as_deref().unwrap_or("");
    assert!(reason.contains("managedPluginsOnly"), "{reason}");
    assert!(sketchy.handle.is_none(), "filtered plugins never spawn");

    // 6. `tack ext list` mirrors the same policy state (rows, not
    //    absences).
    let listed = tack_app::extension_host::list_extensions(&cwd, &agent_dir);
    let sketchy = listed
        .iter()
        .find(|info| info.id == "sketchy@user")
        .expect("listed");
    assert!(sketchy.policy_block.is_some(), "{sketchy:?}");
    let good = listed.iter().find(|info| info.id == "good@user").unwrap();
    assert!(good.enabled, "list shows the managed enabled override");
    assert!(good.policy_block.is_none());
}
