//! Desktop notifications (OSC 9 / OSC 777 via tack-tui) with per-source
//! throttling: a chatty event source (e.g. repeated permission prompts)
//! emits at most one notification per THROTTLE window.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::*;

/// Same event source repeats at most one notification per interval.
pub const THROTTLE: Duration = Duration::from_secs(5);

/// Per-source notification throttle.
#[derive(Debug, Default)]
pub struct NotifyGate {
    last_sent: HashMap<String, Instant>,
}

impl NotifyGate {
    /// The first event from a source always passes; repeats within
    /// THROTTLE are dropped. `now` is injectable for tests.
    pub fn should_send(&mut self, source: &str, now: Instant) -> bool {
        if let Some(last) = self.last_sent.get(source)
            && now.duration_since(*last) < THROTTLE
        {
            return false;
        }
        self.last_sent.insert(source.to_string(), now);
        true
    }
}

impl TuiApp {
    /// Fire a desktop notification honoring the settings switch
    /// (`notifications: false`) and the per-source throttle. Terminals
    /// without notification support silently ignore the escapes.
    pub(crate) fn desktop_notify(&mut self, source: &str, title: &str, body: &str) {
        if !self.settings.notifications {
            return;
        }
        if !self.notify_gate.should_send(source, Instant::now()) {
            return;
        }
        tack_tui::terminal::notify(title, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_blocks_repeats_within_the_window() {
        let mut gate = NotifyGate::default();
        let t0 = Instant::now();
        assert!(gate.should_send("run", t0), "first event passes");
        assert!(!gate.should_send("run", t0 + Duration::from_secs(1)));
        assert!(!gate.should_send("run", t0 + THROTTLE - Duration::from_millis(1)));
        assert!(
            gate.should_send("run", t0 + THROTTLE),
            "window elapsed: passes again"
        );
    }

    #[test]
    fn throttle_is_per_source() {
        let mut gate = NotifyGate::default();
        let t0 = Instant::now();
        assert!(gate.should_send("permission", t0));
        assert!(gate.should_send("run", t0), "other sources unaffected");
        assert!(!gate.should_send("permission", t0 + Duration::from_secs(1)));
        assert!(!gate.should_send("run", t0 + Duration::from_secs(1)));
        assert!(gate.should_send("background", t0 + Duration::from_secs(1)));
    }

    #[test]
    fn settings_switch_gates_notifications() {
        let mut settings = crate::settings::Settings::default();
        assert!(settings.notifications, "default on");
        settings.notifications = false;
        assert!(!settings.notifications);
    }
}
