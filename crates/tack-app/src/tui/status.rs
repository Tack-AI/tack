//! Status indicators: spinner variants for working / retrying / compacting
//! (port of `status-indicator.ts`).

use tack_tui::components::loader::Loader;
use tack_tui::{Component, Line};

use super::theme::Theme;

/// What the agent is currently doing.
#[derive(Debug)]
pub struct StatusIndicator {
    loader: Loader,
    /// Retry countdown deadline (for retrying status).
    retry_until: Option<std::time::Instant>,
    label: String,
}

impl StatusIndicator {
    pub fn working() -> Self {
        let label = crate::i18n::tr("status.working");
        StatusIndicator {
            loader: Loader::new(label.clone()),
            retry_until: None,
            label,
        }
    }

    pub fn compacting() -> Self {
        let label = crate::i18n::tr("status.compacting");
        StatusIndicator {
            loader: Loader::new(label.clone()),
            retry_until: None,
            label,
        }
    }

    pub fn retrying(attempt: u32, max: u32, delay_ms: u64) -> Self {
        StatusIndicator {
            loader: Loader::new(String::new()),
            retry_until: Some(
                std::time::Instant::now() + std::time::Duration::from_millis(delay_ms),
            ),
            label: crate::i18n::trf(
                "status.retrying",
                &[("attempt", &attempt.to_string()), ("max", &max.to_string())],
            ),
        }
    }

    pub fn tick(&mut self) {
        self.loader.tick();
    }

    pub fn is_retrying(&self) -> bool {
        self.retry_until.is_some()
    }

    pub fn render(&mut self, width: u16, _theme: &Theme) -> Vec<Line> {
        let label = if let Some(until) = self.retry_until {
            let remaining = until.saturating_duration_since(std::time::Instant::now());
            crate::i18n::trf(
                "status.retry_countdown",
                &[
                    ("label", &self.label),
                    ("secs", &(remaining.as_secs_f32().ceil() as u32).to_string()),
                ],
            )
        } else {
            self.label.clone()
        };
        self.loader.set_label(label);
        self.loader.render(width)
    }
}
