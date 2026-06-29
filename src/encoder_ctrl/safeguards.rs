//! Encoder pause when queue delay exceeds τ.

use core::time::Duration;

use crate::config::VidaptiveConfig;

/// Pause encoding when the oldest queued packet waits too long.
#[derive(Debug, Clone)]
pub(crate) struct Safeguards {
    tau_pause: Duration,
    paused: bool,
}

impl Safeguards {
    /// Creates safeguards from configuration.
    #[must_use]
    pub(crate) fn new(config: &VidaptiveConfig) -> Self {
        Self {
            tau_pause: config.tau_pause(),
            paused: false,
        }
    }

    /// Updates pause state from queue age and whether the pacer queue is empty.
    pub(crate) fn update(&mut self, oldest_queued_age: Option<Duration>, queue_empty: bool) {
        if queue_empty {
            self.paused = false;
            return;
        }

        let Some(age) = oldest_queued_age else {
            self.paused = false;
            return;
        };

        self.paused = age > self.tau_pause;
    }

    /// Whether the encoder should pause.
    #[must_use]
    pub(crate) const fn is_paused(&self) -> bool {
        self.paused
    }
}
