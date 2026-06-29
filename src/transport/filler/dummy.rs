//! Dummy padding (default ≤ 200 B per packet).

use bytes::Bytes;
use std::time::Instant;

use crate::transport::filler::BacklogFiller;

/// Maximum dummy payload size (default 200 B).
pub const DEFAULT_DUMMY_PACKET_SIZE: usize = 200;

/// Generates zero-filled dummy packets.
#[derive(Debug, Clone)]
pub struct DummyFiller {
    packet_size: usize,
    sent_packets: u64,
}

impl DummyFiller {
    /// Creates a filler using the default 200-byte payloads.
    #[must_use]
    pub fn new() -> Self {
        Self::with_packet_size(DEFAULT_DUMMY_PACKET_SIZE)
    }

    /// Creates a filler with a custom payload size.
    #[must_use]
    pub fn with_packet_size(packet_size: usize) -> Self {
        Self {
            packet_size,
            sent_packets: 0,
        }
    }

    /// Number of filler packets produced so far (for tests / metrics).
    #[must_use]
    pub const fn sent_packets(&self) -> u64 {
        self.sent_packets
    }
}

impl Default for DummyFiller {
    fn default() -> Self {
        Self::new()
    }
}

impl BacklogFiller for DummyFiller {
    fn max_packet_size(&self) -> usize {
        self.packet_size
    }

    fn next_packet(&mut self, _now: Instant) -> Option<Bytes> {
        Some(Bytes::from(vec![0u8; self.packet_size]))
    }

    fn on_packet_sent(&mut self, _seq: u64, _len: usize, _now: Instant) {
        self.sent_packets += 1;
    }
}
