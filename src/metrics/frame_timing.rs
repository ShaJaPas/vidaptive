//! Sliding window of frame service-time samples for the α optimizer.

use alloc::vec::Vec;
use core::time::Duration;
use std::time::Instant;

use crate::types::FrameServiceSample;

/// Samples retained for the last `window` duration.
#[derive(Debug, Clone)]
pub(crate) struct FrameTimingBuffer {
    window: Duration,
    samples: Vec<FrameServiceSample>,
}

impl FrameTimingBuffer {
    /// Creates a buffer with the given retention window.
    #[must_use]
    pub(crate) const fn new(window: Duration) -> Self {
        Self {
            window,
            samples: Vec::new(),
        }
    }

    /// Appends a sample and drops entries older than the window relative to `sample.at`.
    pub(crate) fn push(&mut self, sample: FrameServiceSample) {
        self.evict_before(sample.at);
        self.samples.push(sample);
    }

    /// Removes samples older than `now - window`.
    pub(crate) fn evict(&mut self, now: Instant) {
        self.evict_before(now);
    }

    fn evict_before(&mut self, now: Instant) {
        let cutoff = now.checked_sub(self.window);
        if let Some(cutoff) = cutoff {
            self.samples.retain(|s| s.at >= cutoff);
        }
    }

    /// Iterates samples in insertion order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &FrameServiceSample> {
        self.samples.iter()
    }
}
