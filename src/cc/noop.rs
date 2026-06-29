//! Fixed-rate congestion controller for tests and bring-up.

use core::time::Duration;

use crate::cc::CongestionController;
use crate::types::{AckInfo, LostPacket, SentPacket};

/// Congestion controller with constant `pacing_rate` and `cwnd`.
#[derive(Debug, Clone)]
pub struct NoopCc {
    pacing_rate: u64,
    cwnd: u64,
    rtt: Duration,
    bytes_in_flight: u64,
}

impl NoopCc {
    /// Creates a CC that always reports the given rate and window.
    #[must_use]
    pub const fn new(pacing_rate: u64, cwnd: u64, rtt: Duration) -> Self {
        Self {
            pacing_rate,
            cwnd,
            rtt,
            bytes_in_flight: 0,
        }
    }
}

impl CongestionController for NoopCc {
    fn on_packet_sent(&mut self, packet: &SentPacket) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(packet.len as u64);
    }

    fn on_packets_acked(&mut self, acks: &[AckInfo]) {
        for ack in acks {
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(ack.len as u64);
        }
    }

    fn on_packet_lost(&mut self, packet: &LostPacket) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(packet.len as u64);
    }

    fn pacing_rate(&self) -> u64 {
        self.pacing_rate
    }

    fn cwnd(&self) -> u64 {
        self.cwnd
    }

    fn rtt(&self) -> Duration {
        self.rtt
    }

    fn can_send(&self, bytes_in_flight: u64) -> bool {
        bytes_in_flight < self.cwnd
    }
}
