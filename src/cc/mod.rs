//! Pluggable congestion controllers.

mod copa;
mod noop;
mod rocc;

pub use copa::{Copa, CopaConfig};
pub use noop::NoopCc;
pub use rocc::{Rocc, RoccConfig};

use core::time::Duration;

use crate::types::{AckInfo, LostPacket, SentPacket};

/// Bandwidth estimator
pub trait CongestionController {
    /// Notify the CC that a packet was sent.
    fn on_packet_sent(&mut self, packet: &SentPacket);

    /// Notify the CC that a packet was acknowledged.
    fn on_packet_acked(&mut self, ack: &AckInfo) {
        self.on_packets_acked(core::slice::from_ref(ack));
    }

    /// Notify the CC of one or more ACKs in a single feedback event.
    fn on_packets_acked(&mut self, acks: &[AckInfo]);

    /// Notify the CC that a packet was lost.
    fn on_packet_lost(&mut self, packet: &LostPacket);

    /// Smoothed sending rate estimate (`CC-Rate`) in bytes per second.
    fn pacing_rate(&self) -> u64;

    /// CC-Rate used for encoder target bitrate selection: `cwnd / sRTT`
    /// (Vidaptive paper §3.1, line "CC-Rate, computed as the cwnd divided by
    /// smoothed RTT"). This is the **throughput** estimate without any pacer
    /// burst gain. `target = α · cc_rate()` (paper §3.3). `pacing_rate()`
    /// (which may include a burst gain) is used only by the pacer.
    fn cc_rate(&self) -> u64 {
        // Default: same as pacing_rate() (e.g. ROCC already has no gain).
        self.pacing_rate()
    }

    /// Congestion window in bytes.
    fn cwnd(&self) -> u64;

    /// Smoothed round-trip time.
    fn rtt(&self) -> Duration;

    /// Whether another `len`-byte packet may be sent given current bytes in flight.
    fn can_send(&self, bytes_in_flight: u64) -> bool;
}
