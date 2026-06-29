//! Session configuration.

use core::num::NonZeroU64;
use core::time::Duration;

use thiserror::Error;

/// Tunables for a Vidaptive rate-control session.
///
/// Build with [`VidaptiveConfig::builder`]
#[derive(Debug, Clone, PartialEq)]
pub struct VidaptiveConfig {
    pub(crate) tau_pause: Duration,
    pub(crate) target_service_time: Duration,
    pub(crate) latency_percentile: f64,
    pub(crate) optimizer_window: Duration,
    pub(crate) alpha_ewma: f64,
    pub(crate) max_video_bitrate: u64,
    pub(crate) filler_withhold_before_frame: Duration,
    pub(crate) frame_interval: Duration,
}

impl VidaptiveConfig {
    /// Start building a validated configuration.
    pub fn builder() -> VidaptiveConfigBuilder {
        VidaptiveConfigBuilder::default()
    }

    /// Pacer queue pause threshold τ.
    pub const fn tau_pause(&self) -> Duration {
        self.tau_pause
    }

    /// Target frame service time **P** for the α optimizer.
    pub const fn target_service_time(&self) -> Duration {
        self.target_service_time
    }

    /// Latency percentile **λ** for service-time control.
    pub const fn latency_percentile(&self) -> f64 {
        self.latency_percentile
    }

    /// Sliding window **D** for α updates.
    pub const fn optimizer_window(&self) -> Duration {
        self.optimizer_window
    }

    /// EWMA smoothing factor for α.
    pub const fn alpha_ewma(&self) -> f64 {
        self.alpha_ewma
    }

    /// Stop sending filler above this video bitrate (bits per second).
    pub const fn max_video_bitrate(&self) -> u64 {
        self.max_video_bitrate
    }

    /// Do not send filler within this interval before the next expected frame.
    pub const fn filler_withhold_before_frame(&self) -> Duration {
        self.filler_withhold_before_frame
    }

    /// Camera frame interval **Δ** = 1 / f_max.
    pub const fn frame_interval(&self) -> Duration {
        self.frame_interval
    }
}

impl Default for VidaptiveConfig {
    fn default() -> Self {
        Self::builder()
            .build()
            .expect("default VidaptiveConfig is valid")
    }
}

/// Fluent builder for [`VidaptiveConfig`].
#[derive(Debug, Clone)]
pub struct VidaptiveConfigBuilder {
    tau_pause: Duration,
    target_service_time: Duration,
    latency_percentile: f64,
    optimizer_window: Duration,
    alpha_ewma: f64,
    max_video_bitrate: u64,
    filler_withhold_before_frame: Duration,
    frame_interval: Duration,
}

impl Default for VidaptiveConfigBuilder {
    fn default() -> Self {
        Self {
            tau_pause: Duration::from_millis(33),
            target_service_time: Duration::from_millis(33),
            latency_percentile: 0.9,
            optimizer_window: Duration::from_secs(1),
            alpha_ewma: 0.25,
            max_video_bitrate: 12_000_000,
            filler_withhold_before_frame: Duration::from_nanos(8_250_000),
            frame_interval: Duration::from_millis(33),
        }
    }
}

impl VidaptiveConfigBuilder {
    /// See [`VidaptiveConfig::tau_pause`].
    pub const fn tau_pause(mut self, value: Duration) -> Self {
        self.tau_pause = value;
        self
    }

    /// See [`VidaptiveConfig::target_service_time`].
    pub const fn target_service_time(mut self, value: Duration) -> Self {
        self.target_service_time = value;
        self
    }

    /// See [`VidaptiveConfig::latency_percentile`].
    pub const fn latency_percentile(mut self, value: f64) -> Self {
        self.latency_percentile = value;
        self
    }

    /// See [`VidaptiveConfig::optimizer_window`].
    pub const fn optimizer_window(mut self, value: Duration) -> Self {
        self.optimizer_window = value;
        self
    }

    /// See [`VidaptiveConfig::alpha_ewma`].
    pub const fn alpha_ewma(mut self, value: f64) -> Self {
        self.alpha_ewma = value;
        self
    }

    /// See [`VidaptiveConfig::max_video_bitrate`].
    pub const fn max_video_bitrate(mut self, bps: u64) -> Self {
        self.max_video_bitrate = bps;
        self
    }

    /// See [`VidaptiveConfig::max_video_bitrate`].
    pub const fn max_video_bitrate_nonzero(mut self, bps: NonZeroU64) -> Self {
        self.max_video_bitrate = bps.get();
        self
    }

    /// See [`VidaptiveConfig::filler_withhold_before_frame`].
    pub const fn filler_withhold_before_frame(mut self, value: Duration) -> Self {
        self.filler_withhold_before_frame = value;
        self
    }

    /// See [`VidaptiveConfig::frame_interval`].
    pub const fn frame_interval(mut self, value: Duration) -> Self {
        self.frame_interval = value;
        self
    }

    /// Frame rate helper: sets `frame_interval` to `1 / fps`.
    pub fn frame_rate_hz(self, fps: NonZeroU64) -> Self {
        let nanos = 1_000_000_000u64 / fps.get();
        self.frame_interval(Duration::from_nanos(nanos.max(1)))
    }

    /// Produce a validated [`VidaptiveConfig`].
    pub fn build(self) -> Result<VidaptiveConfig, ConfigError> {
        validate_duration(self.tau_pause, DurationField::TauPause)?;
        validate_duration(self.target_service_time, DurationField::TargetServiceTime)?;
        validate_duration(self.optimizer_window, DurationField::OptimizerWindow)?;
        validate_duration(self.frame_interval, DurationField::FrameInterval)?;

        if self.latency_percentile <= 0.0 || self.latency_percentile > 1.0 {
            return Err(ConfigError::InvalidPercentile {
                value: self.latency_percentile,
            });
        }
        if !(0.0..=1.0).contains(&self.alpha_ewma) {
            return Err(ConfigError::InvalidEwma {
                value: self.alpha_ewma,
            });
        }
        if self.max_video_bitrate == 0 {
            return Err(ConfigError::InvalidMaxBitrate);
        }
        if self.filler_withhold_before_frame > self.frame_interval {
            return Err(ConfigError::FillerWithholdExceedsFrameInterval {
                withhold: self.filler_withhold_before_frame,
                frame_interval: self.frame_interval,
            });
        }

        Ok(VidaptiveConfig {
            tau_pause: self.tau_pause,
            target_service_time: self.target_service_time,
            latency_percentile: self.latency_percentile,
            optimizer_window: self.optimizer_window,
            alpha_ewma: self.alpha_ewma,
            max_video_bitrate: self.max_video_bitrate,
            filler_withhold_before_frame: self.filler_withhold_before_frame,
            frame_interval: self.frame_interval,
        })
    }
}

fn validate_duration(value: Duration, field: DurationField) -> Result<(), ConfigError> {
    if value.is_zero() {
        return Err(ConfigError::ZeroDuration { field });
    }
    Ok(())
}

/// Which duration field failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationField {
    /// [`VidaptiveConfig::tau_pause`].
    TauPause,
    /// [`VidaptiveConfig::target_service_time`].
    TargetServiceTime,
    /// [`VidaptiveConfig::optimizer_window`].
    OptimizerWindow,
    /// [`VidaptiveConfig::frame_interval`].
    FrameInterval,
}

/// Invalid [`VidaptiveConfig`] field.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConfigError {
    /// `latency_percentile` must be in (0, 1].
    #[error("latency_percentile must be in (0, 1], got {value}")]
    InvalidPercentile {
        /// Rejected value.
        value: f64,
    },
    /// `alpha_ewma` must be in [0, 1].
    #[error("alpha_ewma must be in [0, 1], got {value}")]
    InvalidEwma {
        /// Rejected value.
        value: f64,
    },
    /// `max_video_bitrate` must be non-zero.
    #[error("max_video_bitrate must be non-zero")]
    InvalidMaxBitrate,
    /// A required duration was zero.
    #[error("{field:?} must be non-zero")]
    ZeroDuration {
        /// Which field.
        field: DurationField,
    },
    /// `filler_withhold_before_frame` must not exceed `frame_interval`.
    #[error(
        "filler_withhold_before_frame ({withhold:?}) must not exceed frame_interval ({frame_interval:?})"
    )]
    FillerWithholdExceedsFrameInterval {
        /// Configured withhold interval.
        withhold: Duration,
        /// Configured frame interval.
        frame_interval: Duration,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::num::NonZeroU64;

    #[test]
    fn default_builder_succeeds() {
        assert!(VidaptiveConfig::builder().build().is_ok());
    }

    #[test]
    fn rejects_zero_tau_pause() {
        let err = VidaptiveConfig::builder()
            .tau_pause(Duration::ZERO)
            .build()
            .unwrap_err();
        assert_eq!(
            err,
            ConfigError::ZeroDuration {
                field: DurationField::TauPause
            }
        );
    }

    #[test]
    fn rejects_invalid_percentile() {
        let err = VidaptiveConfig::builder()
            .latency_percentile(1.5)
            .build()
            .unwrap_err();
        assert_eq!(err, ConfigError::InvalidPercentile { value: 1.5 });
    }

    #[test]
    fn rejects_withhold_longer_than_frame_interval() {
        let err = VidaptiveConfig::builder()
            .frame_interval(Duration::from_millis(33))
            .filler_withhold_before_frame(Duration::from_millis(40))
            .build()
            .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::FillerWithholdExceedsFrameInterval { .. }
        ));
    }

    #[test]
    fn frame_rate_hz_sets_interval() {
        let config = VidaptiveConfig::builder()
            .frame_rate_hz(NonZeroU64::new(30).unwrap())
            .build()
            .unwrap();
        assert_eq!(config.frame_interval(), Duration::from_nanos(33_333_333));
    }
}
