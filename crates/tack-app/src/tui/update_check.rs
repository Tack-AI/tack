//! TUI startup update check: a background latest-release query (never
//! blocks startup) with a 24h cache in `<agentDir>/update-check.json`.
//! Skipped entirely in offline mode (--offline / TACK_OFFLINE) and when
//! settings `"updateCheck": false`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;

/// Cache TTL: at most one network check per day.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Cached outcome of the last successful update check.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UpdateCheckCache {
    /// Unix seconds of the last successful check.
    pub checked_at: u64,
    /// Latest release version seen ("1.2.3"), when the check succeeded.
    pub latest: Option<String>,
}

impl UpdateCheckCache {
    fn path(agent_dir: &Path) -> PathBuf {
        agent_dir.join("update-check.json")
    }

    pub fn load(agent_dir: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(Self::path(agent_dir)).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn save(&self, agent_dir: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(agent_dir), json);
        }
    }

    /// Fresh = checked less than CACHE_TTL ago. A `checked_at` in the
    /// future (clock skew) counts as fresh rather than panicking.
    pub fn is_fresh(&self, now: u64) -> bool {
        now.saturating_sub(self.checked_at) < CACHE_TTL.as_secs()
    }
}

/// Current time as unix seconds (0 on clock-before-epoch, harmless here).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl TuiApp {
    /// Startup update check. A fresh cache answers from disk (no network,
    /// and a cached newer version still surfaces the hint); otherwise the
    /// query runs in a background task and the result lands as
    /// `AppEvent::UpdateAvailable`.
    pub(crate) fn start_update_check(&mut self) {
        if !self.settings.update_check
            || self.flags.offline
            || std::env::var_os("TACK_OFFLINE").is_some()
        {
            return;
        }
        let now = now_secs();
        if let Some(cache) = UpdateCheckCache::load(&self.agent_dir)
            && cache.is_fresh(now)
        {
            // Recent check on disk: no network; surface a cached hit.
            if let Some(latest) = cache.latest.as_deref()
                && crate::self_update::is_newer_version(latest, env!("CARGO_PKG_VERSION"))
            {
                self.set_update_hint(latest);
            }
            return;
        }
        let repo = crate::self_update::update_repo(self.settings.update_repo.as_deref());
        let agent_dir = self.agent_dir.clone();
        let tx = self.event_tx.clone();
        crate::task::spawn_guarded("update-check", async move {
            match crate::self_update::check_latest_release(&repo).await {
                Ok(release) => {
                    UpdateCheckCache {
                        checked_at: now_secs(),
                        latest: Some(release.version.clone()),
                    }
                    .save(&agent_dir);
                    if crate::self_update::is_newer_version(
                        &release.version,
                        env!("CARGO_PKG_VERSION"),
                    ) {
                        let _ = tx.send(AppEvent::UpdateAvailable(release.version));
                    }
                }
                Err(e) => {
                    // No cache write: a transient failure must not suppress
                    // the next launch's check for 24h.
                    tracing::debug!("startup update check failed: {e}");
                }
            }
        });
    }

    /// Surface "update available": footer hint + one chat notice
    /// (idempotent per version, so the cache path and the background
    /// result cannot double-notify).
    pub(crate) fn set_update_hint(&mut self, version: &str) {
        if self.update_hint.as_deref() == Some(version) {
            return;
        }
        self.update_hint = Some(version.to_string());
        self.notice(
            crate::i18n::t(
                self.lang,
                "msg.update_available",
                &[("version", version), ("current", env!("CARGO_PKG_VERSION"))],
            ),
            NoticeKind::Info,
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn cache_freshness_gates_on_the_ttl() {
        let now = 1_000_000u64;
        let cache = UpdateCheckCache {
            checked_at: now,
            latest: None,
        };
        assert!(cache.is_fresh(now));
        assert!(cache.is_fresh(now + CACHE_TTL.as_secs() - 1));
        assert!(!cache.is_fresh(now + CACHE_TTL.as_secs()));
        assert!(!cache.is_fresh(now + CACHE_TTL.as_secs() * 2));
        // Clock skew (checked_at in the future): fresh, never panics.
        assert!(cache.is_fresh(now - 60));
    }

    #[test]
    fn cache_roundtrips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        // Missing file: no cache.
        assert!(UpdateCheckCache::load(tmp.path()).is_none());
        let cache = UpdateCheckCache {
            checked_at: 42,
            latest: Some("1.2.3".to_string()),
        };
        cache.save(tmp.path());
        let loaded = UpdateCheckCache::load(tmp.path()).unwrap();
        assert_eq!(loaded.checked_at, 42);
        assert_eq!(loaded.latest.as_deref(), Some("1.2.3"));
        // Corrupt file: ignored, not an error.
        std::fs::write(tmp.path().join("update-check.json"), "{nope").unwrap();
        assert!(UpdateCheckCache::load(tmp.path()).is_none());
    }
}
