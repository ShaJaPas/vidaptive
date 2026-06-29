//! Sliding-window video send bitrate for the filler cap.

use alloc::vec::Vec;
use core::time::Duration;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
struct ByteSample {
    at: Instant,
    bytes: u64,
}

/// Bytes sent over a fixed-duration window; bitrate = `8 · bytes / window`.
#[derive(Debug, Clone)]
pub(crate) struct VideoBitrateWindow {
    window: Duration,
    samples: Vec<ByteSample>,
}

impl VideoBitrateWindow {
    /// Creates a window of the given duration.
    #[must_use]
    pub(crate) const fn new(window: Duration) -> Self {
        Self {
            window,
            samples: Vec::new(),
        }
    }

    /// Records video payload bytes sent at `now`.
    pub(crate) fn record(&mut self, bytes: u64, now: Instant) {
        self.evict(now);
        self.samples.push(ByteSample { at: now, bytes });
    }

    /// Drops samples older than `now - window`.
    pub(crate) fn evict(&mut self, now: Instant) {
        let Some(cutoff) = now.checked_sub(self.window) else {
            return;
        };
        self.samples.retain(|sample| sample.at >= cutoff);
    }

    /// Measured video bitrate in bits per second over the full window duration.
    ///
    /// Always divides by the configured window length (not elapsed since the last
    /// sample), which avoids startup spikes when only a short burst has been sent.
    #[must_use]
    pub(crate) fn bitrate_bps(&self, now: Instant) -> u64 {
        let window_secs = self.window.as_secs_f64();
        if window_secs <= f64::EPSILON {
            return 0;
        }

        let total_bytes: u64 = match now.checked_sub(self.window) {
            Some(cutoff) => self
                .samples
                .iter()
                .filter(|sample| sample.at >= cutoff)
                .map(|sample| sample.bytes)
                .sum(),
            None => self.samples.iter().map(|sample| sample.bytes).sum(),
        };

        ((total_bytes as f64) * 8.0 / window_secs) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_burst_does_not_spike_bitrate() {
        let window = Duration::from_secs(1);
        let mut tracker = VideoBitrateWindow::new(window);
        let t0 = Instant::now();

        // 1.5 MB in 1 ms would be ~12 Gbps if divided by elapsed; window caps at 12 Mbps scale.
        tracker.record(1_500_000, t0);

        let bps = tracker.bitrate_bps(t0 + Duration::from_millis(1));
        assert_eq!(bps, 12_000_000);
    }

    #[test]
    fn evicts_samples_outside_window() {
        let window = Duration::from_millis(100);
        let mut tracker = VideoBitrateWindow::new(window);
        let t0 = Instant::now();

        tracker.record(10_000, t0);
        let mid = t0 + Duration::from_millis(150);
        tracker.evict(mid);
        assert_eq!(tracker.bitrate_bps(mid), 0);
    }
}
