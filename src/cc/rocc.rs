use crate::{AckInfo, CongestionController, LostPacket, SentPacket};
use alloc::collections::BTreeMap;
use core::time::Duration;
use std::time::Instant;

const ROCC_NUM_INTERVALS: usize = 16;
const ROCC_NUM_INTERVALS_MASK: usize = 15;

#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct RoccConfig {
    pub mss: u64,
    pub initial_cwnd_packets: u64,
    pub min_cwnd_packets: u64,
    /// rocc_alpha in packets (usually 2)
    pub alpha_packets: u64,
    /// rocc_loss_thresh (fraction of 1024, e.g. 64 = ~6.25%)
    pub loss_thresh: u64,
}

impl Default for RoccConfig {
    fn default() -> Self {
        Self {
            mss: 1400,
            initial_cwnd_packets: 10,
            min_cwnd_packets: 2,
            alpha_packets: 2,
            loss_thresh: 64,
        }
    }
}

#[derive(Clone, Debug)]
struct RoccInterval {
    start_time: Option<Instant>,
    bytes_acked: u64,
    bytes_lost: u64,
    app_limited: bool,
}

#[allow(missing_docs)]
pub struct Rocc {
    config: RoccConfig,
    intervals: Vec<RoccInterval>,
    intervals_head: usize,

    min_rtt: Duration,
    current_rtt: Duration,

    cwnd_bytes: u64,
    pacing_rate_bps: u64,

    sent_packets: BTreeMap<u64, Instant>,
    bytes_in_flight: u64,
}

impl Rocc {
    /// Initialize
    pub fn new(config: RoccConfig) -> Self {
        let intervals = vec![
            RoccInterval {
                start_time: None,
                bytes_acked: 0,
                bytes_lost: 0,
                app_limited: false,
            };
            ROCC_NUM_INTERVALS
        ];

        let initial_cwnd_bytes = config.initial_cwnd_packets * config.mss;

        Self {
            config,
            intervals,
            intervals_head: 0,
            min_rtt: Duration::MAX,
            current_rtt: Duration::from_millis(50),
            cwnd_bytes: initial_cwnd_bytes,
            pacing_rate_bps: (initial_cwnd_bytes as f64 / 0.05) as u64,
            sent_packets: BTreeMap::new(),
            bytes_in_flight: 0,
        }
    }

    fn process_sample(&mut self, acked_bytes: u64, lost_bytes: u64, now: Instant) {
        let is_app_limited = self.bytes_in_flight < self.cwnd_bytes;

        let min_rtt_us = if self.min_rtt == Duration::MAX {
            50_000
        } else {
            self.min_rtt.as_micros().max(1) as u64
        };

        let rtt_us = self.current_rtt.as_micros().max(1) as u64;
        let hist_us = min_rtt_us * 2;
        let interval_length_us = (2 * hist_us / ROCC_NUM_INTERVALS as u64) + 1;
        let interval_duration = Duration::from_micros(interval_length_us);

        let current_start = self.intervals[self.intervals_head].start_time;
        if current_start.is_none() || current_start.unwrap() + interval_duration < now {
            self.intervals_head = self.intervals_head.wrapping_sub(1) & ROCC_NUM_INTERVALS_MASK;
            self.intervals[self.intervals_head].start_time = Some(now);
            self.intervals[self.intervals_head].bytes_acked = acked_bytes;
            self.intervals[self.intervals_head].bytes_lost = lost_bytes;
            self.intervals[self.intervals_head].app_limited = is_app_limited;
        } else {
            self.intervals[self.intervals_head].bytes_acked += acked_bytes;
            self.intervals[self.intervals_head].bytes_lost += lost_bytes;
            self.intervals[self.intervals_head].app_limited |= is_app_limited;
        }

        let mut total_bytes_acked = 0;
        let mut total_bytes_lost = 0;
        let mut app_limited = false;
        let hist_duration = Duration::from_micros(hist_us);

        for i in 0..ROCC_NUM_INTERVALS {
            let id = (self.intervals_head + i) & ROCC_NUM_INTERVALS_MASK;
            if let Some(start) = self.intervals[id].start_time {
                total_bytes_acked += self.intervals[id].bytes_acked;
                total_bytes_lost += self.intervals[id].bytes_lost;
                app_limited |= self.intervals[id].app_limited;

                if start + hist_duration < now {
                    break;
                }
            } else {
                break;
            }
        }

        let total_bytes = total_bytes_acked + total_bytes_lost;
        let raw_loss_mode =
            total_bytes > 0 && (total_bytes_lost * 1024) > (total_bytes * self.config.loss_thresh);

        // Distinguish congestion loss (RTT increased) from random loss (RTT stable).
        // Random loss (e.g. netem, FEC-transparent) should not trigger rate reduction.
        let rtt_increased = self.min_rtt != Duration::MAX && rtt_us > min_rtt_us * 3 / 2;
        let loss_mode = raw_loss_mode && rtt_increased;

        let mut target_cwnd = total_bytes_acked + (self.config.alpha_packets * self.config.mss);

        if app_limited && !loss_mode && target_cwnd < self.cwnd_bytes {
            target_cwnd = self.cwnd_bytes;
        }

        let min_cwnd_bytes = self.config.min_cwnd_packets * self.config.mss;
        self.cwnd_bytes = target_cwnd.max(min_cwnd_bytes);

        if loss_mode {
            let current_rtt_secs = (rtt_us as f64) / 1_000_000.0;
            let factor =
                (1024.0 + 2.0 * self.config.loss_thresh as f64) / (current_rtt_secs * 2.0 * 1024.0);
            self.pacing_rate_bps = (self.cwnd_bytes as f64 * factor) as u64;
        } else {
            let min_rtt_secs = (min_rtt_us as f64) / 1_000_000.0;
            self.pacing_rate_bps = (self.cwnd_bytes as f64 / min_rtt_secs) as u64;
        }
    }
}

impl CongestionController for Rocc {
    fn on_packet_sent(&mut self, packet: &SentPacket) {
        let now = Instant::now();
        let _elapsed = now.saturating_duration_since(packet.sent_at);
        self.sent_packets.insert(packet.seq, packet.sent_at);
        self.bytes_in_flight += packet.len as u64;
    }

    fn on_packets_acked(&mut self, acks: &[AckInfo]) {
        if acks.is_empty() {
            return;
        }

        let mut total_acked_bytes = 0;
        let mut last_acked_at = acks[0].acked_at;

        for ack in acks {
            if let Some(sent_at) = self.sent_packets.remove(&ack.seq) {
                let rtt = ack.acked_at.saturating_duration_since(sent_at);
                if rtt > Duration::ZERO {
                    self.current_rtt = rtt;
                    if self.min_rtt == Duration::MAX || rtt < self.min_rtt {
                        self.min_rtt = rtt;
                    }
                }

                self.bytes_in_flight = self.bytes_in_flight.saturating_sub(ack.len as u64);
                total_acked_bytes += ack.len as u64;
                last_acked_at = last_acked_at.max(ack.acked_at);
            }
        }

        if total_acked_bytes > 0 {
            self.process_sample(total_acked_bytes, 0, last_acked_at);
        }
    }

    fn on_packet_lost(&mut self, packet: &LostPacket) {
        if self.sent_packets.remove(&packet.seq).is_some() {
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(packet.len as u64);
            self.process_sample(0, packet.len as u64, packet.lost_at);
        }
    }

    fn pacing_rate(&self) -> u64 {
        self.pacing_rate_bps
    }

    fn cc_rate(&self) -> u64 {
        // Encoder BWE: cwnd / current RTT (Vidaptive §3.1). Using min_rtt here
        // overestimates throughput under standing queue delay and drives the
        // α·cc_rate·8 target into the ceiling while the pipe is congested.
        // Pacing keeps min_rtt-based `pacing_rate()` for send timing.
        let rtt_secs = self.current_rtt.as_secs_f64().max(1e-6);
        (self.cwnd_bytes as f64 / rtt_secs) as u64
    }

    fn cwnd(&self) -> u64 {
        self.cwnd_bytes
    }

    fn rtt(&self) -> Duration {
        self.current_rtt
    }

    fn can_send(&self, bytes_in_flight: u64) -> bool {
        bytes_in_flight < self.cwnd_bytes
    }
}
