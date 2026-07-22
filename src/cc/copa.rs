//! Copa delay-based congestion control (adapted from TQUIC).
//!
//! See <https://web.mit.edu/copa/>.

use alloc::collections::{BTreeMap, VecDeque};
use core::time::Duration;
use std::time::Instant;

use crate::cc::CongestionController;
use crate::types::{AckInfo, LostPacket, SentPacket};

// δ = 0.9 in the Vidaptive reference implementation.
const DEFAULT_DELTA: f64 = 0.9;

const SPEED_UP_THRESHOLD: u64 = 3;
const STANDING_RTT_WINDOW: Duration = Duration::from_millis(100);
const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);
const PACING_GAIN: u64 = 2;
const LOSS_RATE_THRESHOLD: f64 = 0.1;
const DEFAULT_INITIAL_RTT: Duration = Duration::from_millis(50);
const DEFAULT_PACKET_SIZE: u64 = 1_200;

/// Copa configuration.
#[derive(Debug, Clone)]
pub struct CopaConfig {
    /// Minimum cwnd in bytes.
    pub min_cwnd: u64,
    /// Initial cwnd in bytes.
    pub initial_cwnd: u64,
    /// Initial RTT guess.
    pub initial_rtt: Duration,
    /// Reference packet size for target-rate calculation.
    pub packet_size: u64,
    /// δ in slow start.
    pub slow_start_delta: f64,
    /// δ in steady state.
    pub steady_delta: f64,
    /// Use standing RTT window = srtt (true) or srtt/2 (false).
    pub use_standing_rtt: bool,
}

impl Default for CopaConfig {
    fn default() -> Self {
        Self {
            min_cwnd: 4 * DEFAULT_PACKET_SIZE,
            initial_cwnd: 80 * DEFAULT_PACKET_SIZE,
            initial_rtt: DEFAULT_INITIAL_RTT,
            packet_size: DEFAULT_PACKET_SIZE,
            slow_start_delta: DEFAULT_DELTA,
            steady_delta: DEFAULT_DELTA,
            use_standing_rtt: true,
        }
    }
}

/// Copa congestion controller.
#[derive(Debug)]
pub struct Copa {
    config: CopaConfig,
    init_time: Instant,
    cwnd: u64,
    slow_start: bool,
    mode: CompetingMode,
    delta: f64,
    velocity: Velocity,
    standing_rtt: MinMax,
    min_rtt: MinMax,
    rtt: RttEstimator,
    ack: AckState,
    increase_cwnd: bool,
    target_rate: u64,
    round: Round,
    last_sent_seq: u64,
    bytes_in_flight: u64,
    bytes_acked_total: u64,
    bytes_lost_total: u64,
    sent: BTreeMap<u64, SentRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompetingMode {
    Default,
    Competitive,
}

#[derive(Debug, Clone, Copy)]
struct SentRecord {
    sent_at: Instant,
}

#[derive(Debug, Clone, Copy)]
struct Velocity {
    direction: Direction,
    velocity: u64,
    last_cwnd: u64,
    same_direction_cnt: u64,
}

#[derive(Debug, Clone, Copy)]
struct AckState {
    now: Instant,
    newly_lost_bytes: u64,
    newly_acked_bytes: u64,
    largest_acked_seq: u64,
    min_rtt: Duration,
    last_srtt: Duration,
}

#[derive(Debug, Default, Clone, Copy)]
struct Round {
    count: u64,
    is_start: bool,
    end_seq: u64,
    last_acked_bytes: u64,
    last_lost_bytes: u64,
    loss_rate: f64,
}

#[derive(Debug, Clone)]
struct MinMax {
    window_us: u64,
    samples: VecDeque<(u64, u64)>,
    min: u64,
}

impl MinMax {
    fn new(window: Duration) -> Self {
        Self {
            window_us: window.as_micros().min(u64::MAX as u128) as u64,
            samples: VecDeque::new(),
            min: 0,
        }
    }

    fn set_window(&mut self, window: Duration) {
        self.window_us = window.as_micros().min(u64::MAX as u128) as u64;
    }

    fn update_min(&mut self, now_us: u64, sample_us: u64) {
        self.samples.push_back((now_us, sample_us));
        let cutoff = now_us.saturating_sub(self.window_us);
        while self.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            self.samples.pop_front();
        }
        self.min = self
            .samples
            .iter()
            .map(|(_, v)| *v)
            .min()
            .unwrap_or(sample_us);
    }

    const fn get(&self) -> u64 {
        self.min
    }
}

#[derive(Debug, Clone, Copy)]
struct RttEstimator {
    latest_rtt: Duration,
    smoothed_rtt: Duration,
    rttvar: Duration,
}

impl RttEstimator {
    fn new(initial: Duration) -> Self {
        Self {
            latest_rtt: initial,
            smoothed_rtt: initial,
            rttvar: initial / 2,
        }
    }

    fn on_sample(&mut self, sample: Duration) {
        self.latest_rtt = sample;
        if self.smoothed_rtt.is_zero() {
            self.smoothed_rtt = sample;
            self.rttvar = sample / 2;
            return;
        }
        let diff = sample.abs_diff(self.smoothed_rtt);
        self.rttvar = duration_mul(self.rttvar, 0.75) + duration_mul(diff, 0.25);
        self.smoothed_rtt = duration_mul(self.smoothed_rtt, 0.875) + duration_mul(sample, 0.125);
    }

    const fn smoothed_rtt(&self) -> Duration {
        self.smoothed_rtt
    }
}

fn duration_mul(d: Duration, factor: f64) -> Duration {
    Duration::from_secs_f64(d.as_secs_f64() * factor)
}

impl Copa {
    /// Creates a Copa instance.
    #[must_use]
    pub fn new(config: CopaConfig) -> Self {
        let initial_rtt = config.initial_rtt;
        let initial_cwnd = config.initial_cwnd;
        Self {
            delta: config.slow_start_delta,
            cwnd: initial_cwnd,
            rtt: RttEstimator::new(initial_rtt),
            standing_rtt: MinMax::new(STANDING_RTT_WINDOW),
            min_rtt: MinMax::new(MIN_RTT_WINDOW),
            init_time: Instant::now(),
            config,
            slow_start: true,
            mode: CompetingMode::Default,
            velocity: Velocity {
                direction: Direction::Up,
                velocity: 1,
                last_cwnd: 0,
                same_direction_cnt: 0,
            },
            ack: AckState {
                now: Instant::now(),
                newly_lost_bytes: 0,
                newly_acked_bytes: 0,
                largest_acked_seq: 0,
                min_rtt: Duration::ZERO,
                last_srtt: Duration::ZERO,
            },
            increase_cwnd: true,
            target_rate: 0,
            round: Round::default(),
            last_sent_seq: 0,
            bytes_in_flight: 0,
            bytes_acked_total: 0,
            bytes_lost_total: 0,
            sent: BTreeMap::new(),
        }
    }

    /// Current target throughput from the Copa model (bytes/sec).
    #[must_use]
    pub const fn target_rate(&self) -> u64 {
        self.target_rate
    }

    /// Whether Copa is still in slow start.
    #[must_use]
    pub const fn in_slow_start(&self) -> bool {
        self.slow_start
    }

    fn standing_rtt(&self) -> Duration {
        let rtt = Duration::from_micros(self.standing_rtt.get());
        if rtt.is_zero() {
            self.config.initial_rtt.max(Duration::from_micros(1))
        } else {
            rtt
        }
    }

    fn begin_ack(&mut self, now: Instant) {
        self.ack.now = now;
        self.ack.newly_lost_bytes = 0;
        self.ack.newly_acked_bytes = 0;
        self.ack.largest_acked_seq = 0;
        self.ack.min_rtt = Duration::ZERO;
        self.ack.last_srtt = Duration::ZERO;
    }

    fn on_ack_sample(&mut self, seq: u64, acked_bytes: u64, rtt_sample: Duration) {
        self.ack.newly_acked_bytes = self.ack.newly_acked_bytes.saturating_add(acked_bytes);
        self.ack.largest_acked_seq = self.ack.largest_acked_seq.max(seq);
        self.ack.last_srtt = self.rtt.smoothed_rtt();
        if self.ack.min_rtt.is_zero() || self.ack.min_rtt >= rtt_sample {
            self.ack.min_rtt = rtt_sample;
        }
    }

    fn end_ack(&mut self) {
        self.update_round();
        self.update_model();
    }

    fn update_round(&mut self) {
        if self.ack.largest_acked_seq >= self.round.end_seq {
            let lost = self
                .bytes_lost_total
                .saturating_sub(self.round.last_lost_bytes);
            let acked = self
                .bytes_acked_total
                .saturating_sub(self.round.last_acked_bytes);
            let denom = lost.saturating_add(acked);
            self.round.loss_rate = if denom == 0 {
                0.0
            } else {
                lost as f64 / denom as f64
            };
            self.round.last_acked_bytes = self.bytes_acked_total;
            self.round.last_lost_bytes = self.bytes_lost_total;
            self.round.count = self.round.count.saturating_add(1);
            self.round.end_seq = self.last_sent_seq;
            self.round.is_start = true;
        } else {
            self.round.is_start = false;
        }
    }

    fn update_mode(&mut self) {
        self.mode = if self.round.loss_rate >= LOSS_RATE_THRESHOLD {
            CompetingMode::Competitive
        } else {
            CompetingMode::Default
        };

        self.delta = match self.mode {
            CompetingMode::Default => {
                if self.slow_start {
                    self.config.slow_start_delta
                } else {
                    self.config.steady_delta
                }
            }
            CompetingMode::Competitive => (self.delta * 2.0).min(0.5),
        };
    }

    fn update_velocity(&mut self) {
        if self.slow_start && self.increase_cwnd {
            return;
        }

        if self.velocity.last_cwnd == 0 {
            self.velocity.last_cwnd = self.cwnd.max(self.config.min_cwnd);
            self.velocity.velocity = 1;
            self.velocity.same_direction_cnt = 0;
            return;
        }

        if !self.round.is_start {
            return;
        }

        let new_dir = if self.cwnd > self.velocity.last_cwnd {
            Direction::Up
        } else {
            Direction::Down
        };

        if new_dir != self.velocity.direction {
            self.velocity.velocity = 1;
            self.velocity.same_direction_cnt = 0;
        } else {
            self.velocity.same_direction_cnt = self.velocity.same_direction_cnt.saturating_add(1);
            if self.velocity.same_direction_cnt >= SPEED_UP_THRESHOLD {
                self.velocity.velocity = self.velocity.velocity.saturating_mul(2);
            }
        }

        if self.increase_cwnd
            && self.velocity.direction != Direction::Up
            && self.velocity.velocity > 1
        {
            self.velocity.direction = Direction::Up;
            self.velocity.velocity = 1;
        } else if !self.increase_cwnd
            && self.velocity.direction != Direction::Down
            && self.velocity.velocity > 1
        {
            self.velocity.direction = Direction::Down;
            self.velocity.velocity = 1;
        }

        self.velocity.direction = new_dir;
        self.velocity.last_cwnd = self.cwnd;
    }

    fn update_cwnd(&mut self) {
        if self.slow_start && !self.increase_cwnd {
            self.slow_start = false;
        }

        if self.slow_start {
            if self.increase_cwnd {
                self.cwnd = self.cwnd.saturating_add(self.ack.newly_acked_bytes);
            }
        } else {
            let cwnd_delta = ((self.velocity.velocity
                * self.ack.newly_acked_bytes
                * self.config.packet_size) as f64
                / (self.delta * self.cwnd as f64)) as u64;

            self.cwnd = if self.increase_cwnd {
                self.cwnd.saturating_add(cwnd_delta)
            } else {
                self.cwnd.saturating_sub(cwnd_delta)
            };

            if self.cwnd == 0 {
                self.cwnd = self.config.min_cwnd;
                self.velocity.velocity = 1;
            }
        }
    }

    fn update_model(&mut self) {
        if self.config.use_standing_rtt {
            self.standing_rtt
                .set_window(self.ack.last_srtt.max(Duration::from_micros(1)));
        } else {
            self.standing_rtt
                .set_window((self.ack.last_srtt / 2).max(Duration::from_micros(1)));
        }

        if self.ack.min_rtt.is_zero() {
            self.ack.min_rtt = if self.ack.last_srtt.is_zero() {
                self.config.initial_rtt
            } else {
                self.ack.last_srtt
            };
        }

        let elapsed = self.ack.now.saturating_duration_since(self.init_time);
        let elapsed_us = elapsed.as_micros().min(u64::MAX as u128) as u64;
        let min_sample = self.ack.min_rtt.as_micros().min(u64::MAX as u128) as u64;

        self.min_rtt.update_min(elapsed_us, min_sample);
        self.standing_rtt.update_min(elapsed_us, min_sample);

        self.update_mode();

        let min_rtt = Duration::from_micros(self.min_rtt.get());
        let standing_rtt = self.standing_rtt();
        let standing_secs = standing_rtt.as_secs_f64().max(1e-9);
        let current_rate = (self.cwnd as f64 / standing_secs) as u64;
        let queueing_delay = standing_rtt.saturating_sub(min_rtt);

        if queueing_delay.is_zero() {
            self.increase_cwnd = true;
            self.target_rate = current_rate;
        } else {
            self.target_rate = (self.config.packet_size as f64
                / self.delta
                / queueing_delay.max(Duration::from_micros(1)).as_secs_f64())
                as u64;
            self.increase_cwnd = self.target_rate >= current_rate;
        }

        self.update_velocity();
        self.update_cwnd();
    }
}

impl CongestionController for Copa {
    fn on_packet_sent(&mut self, packet: &SentPacket) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(packet.len as u64);
        self.last_sent_seq = packet.seq;
        self.sent.insert(
            packet.seq,
            SentRecord {
                sent_at: packet.sent_at,
            },
        );
    }

    fn on_packets_acked(&mut self, acks: &[AckInfo]) {
        if acks.is_empty() {
            return;
        }

        self.begin_ack(acks[0].acked_at);

        for ack in acks {
            let Some(record) = self.sent.remove(&ack.seq) else {
                continue;
            };

            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(ack.len as u64);
            self.bytes_acked_total = self.bytes_acked_total.saturating_add(ack.len as u64);

            let rtt_sample = ack.acked_at.saturating_duration_since(record.sent_at);
            self.rtt.on_sample(rtt_sample);

            self.on_ack_sample(ack.seq, ack.len as u64, rtt_sample);
        }

        self.end_ack();
    }

    fn on_packet_lost(&mut self, packet: &LostPacket) {
        if self.sent.remove(&packet.seq).is_some() {
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(packet.len as u64);
        }
        self.bytes_lost_total = self.bytes_lost_total.saturating_add(packet.len as u64);
        self.ack.newly_lost_bytes = self.ack.newly_lost_bytes.saturating_add(packet.len as u64);
    }

    fn pacing_rate(&self) -> u64 {
        let standing = self.standing_rtt();
        let secs = standing.as_secs_f64().max(1e-9);
        PACING_GAIN.saturating_mul((self.cwnd() as f64 / secs) as u64)
    }

    fn cc_rate(&self) -> u64 {
        // Throughput estimate without pacer burst gain (paper §3.1).
        let secs = self.rtt().as_secs_f64().max(1e-9);
        (self.cwnd() as f64 / secs) as u64
    }

    fn cwnd(&self) -> u64 {
        self.cwnd.max(self.config.min_cwnd)
    }

    fn rtt(&self) -> Duration {
        self.rtt.smoothed_rtt()
    }

    fn can_send(&self, bytes_in_flight: u64) -> bool {
        bytes_in_flight < self.cwnd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwnd_grows_on_ack() {
        let mut copa = Copa::new(CopaConfig::default());
        let t0 = Instant::now();
        let pkt_len = 1_200usize;

        for seq in 1..=10 {
            copa.on_packet_sent(&SentPacket {
                seq,
                len: pkt_len,
                sent_at: t0 + Duration::from_millis(seq),
                app_limited: false,
            });
        }

        let cwnd_before = copa.cwnd();
        copa.on_packet_acked(&AckInfo {
            seq: 1,
            len: pkt_len,
            acked_at: t0 + Duration::from_millis(50),
        });
        assert!(copa.cwnd() >= cwnd_before);
        assert!(copa.pacing_rate() > 0);
    }

    #[test]
    fn can_send_respects_cwnd() {
        let copa = Copa::new(CopaConfig {
            min_cwnd: 3_600,
            initial_cwnd: 3_600,
            ..CopaConfig::default()
        });
        assert!(copa.can_send(0));
        assert!(copa.can_send(3_599));
        assert!(!copa.can_send(3_600));
    }
}
