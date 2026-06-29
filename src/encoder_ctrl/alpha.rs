//! Percentile-based α optimizer.

use core::time::Duration;

use alloc::vec::Vec;

use crate::config::VidaptiveConfig;
use crate::metrics::{FrameTimingBuffer, percentile_linear};
use crate::types::FrameServiceSample;

/// Computes smoothed α and target bitrate from recent service-time samples.
#[derive(Debug, Clone)]
pub(crate) struct AlphaController {
    target_service_time: Duration,
    latency_percentile: f64,
    alpha_ewma: f64,
    alpha: f64,
}

impl AlphaController {
    /// Creates an optimizer from configuration.
    #[must_use]
    pub(crate) fn new(config: &VidaptiveConfig) -> Self {
        Self {
            target_service_time: config.target_service_time(),
            latency_percentile: config.latency_percentile(),
            alpha_ewma: config.alpha_ewma(),
            alpha: 1.0,
        }
    }

    /// Current smoothed α.
    #[must_use]
    pub(crate) fn alpha(&self) -> f64 {
        self.alpha
    }

    /// Recomputes α from samples and returns `(alpha, target_bitrate_bps)`.
    pub(crate) fn update(
        &mut self,
        samples: &FrameTimingBuffer,
        pacing_rate: u64,
        paused: bool,
    ) -> (f64, u64) {
        if paused || pacing_rate == 0 {
            return (self.alpha, 0);
        }

        let raw_alpha = compute_alpha(samples, self.target_service_time, self.latency_percentile);

        self.alpha = ewma(self.alpha, raw_alpha, self.alpha_ewma);
        let target_bitrate_bps = ((self.alpha * pacing_rate as f64 * 8.0).round()) as u64;
        (self.alpha, target_bitrate_bps)
    }
}

fn compute_alpha(
    samples: &FrameTimingBuffer,
    target_service_time: Duration,
    latency_percentile: f64,
) -> f64 {
    let values: Vec<f64> = samples.iter().filter_map(normalized_service_time).collect();

    if values.is_empty() {
        return 1.0;
    }

    let mut sorted = values;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));

    let p_secs = duration_secs(target_service_time);
    let x = percentile_linear(&sorted, latency_percentile).max(f64::EPSILON);
    (p_secs / x).clamp(0.05, 1.0)
}

fn normalized_service_time(sample: &FrameServiceSample) -> Option<f64> {
    if sample.target_bitrate_bps == 0 {
        return None;
    }
    let di = duration_secs(sample.service_time);
    let tri_bytes_per_sec = sample.target_bitrate_bps as f64 / 8.0;
    let ratio = sample.cc_rate_bytes_per_sec as f64 / tri_bytes_per_sec;
    Some(di * ratio)
}

fn duration_secs(d: Duration) -> f64 {
    d.as_secs_f64() + f64::from(d.subsec_nanos()) / 1_000_000_000.0
}

fn ewma(prev: f64, next: f64, factor: f64) -> f64 {
    prev * (1.0 - factor) + next * factor
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn high_service_times_reduce_alpha() {
        let config = VidaptiveConfig::default();
        let mut controller = AlphaController::new(&config);
        let mut samples = FrameTimingBuffer::new(config.optimizer_window());
        let now = Instant::now();

        for _ in 0..10 {
            samples.push(FrameServiceSample {
                service_time: Duration::from_millis(80),
                target_bitrate_bps: 8_000_000,
                cc_rate_bytes_per_sec: 1_000_000,
                at: now,
            });
        }

        let (alpha, _) = controller.update(&samples, 1_000_000, false);
        assert!(alpha < 1.0);
    }
}
