//! Dummy padding for backlog periods.

mod dummy;

pub use dummy::{DEFAULT_DUMMY_PACKET_SIZE, DummyFiller};

use bytes::Bytes;
use std::time::Instant;

/// Produces dummy filler packets when the pacer is ready but the video queue is empty.
pub trait BacklogFiller {
    /// Maximum filler payload size in bytes.
    fn max_packet_size(&self) -> usize;

    /// Returns the next filler packet if one is available.
    fn next_packet(&mut self, now: Instant) -> Option<Bytes>;

    /// Called after the filler packet was handed to the application for send.
    fn on_packet_sent(&mut self, seq: u64, len: usize, now: Instant);
}
