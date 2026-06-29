//! Pacer queue, pacing gate, and bytes-in-flight accounting.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use bytes::Bytes;
use core::time::Duration;
use std::time::Instant;

use crate::types::{MediaPacket, PacketMeta};

#[derive(Debug, Clone)]
struct QueuedPacket {
    frame_id: u64,
    payload: Bytes,
    fin: bool,
    counts_as_video: bool,
    queued_at: Instant,
    at_head_at: Option<Instant>,
}

/// Media queue and pacing state.
///
/// Accepts pre-formed wire packets; does not split or packetize payloads.
#[derive(Debug)]
pub(crate) struct Pacer {
    queue: Vec<QueuedPacket>,
    in_flight: BTreeMap<u64, PacketMeta>,
    next_seq: u64,
    next_send_at: Option<Instant>,
    bytes_in_flight: u64,
}

impl Pacer {
    /// Creates an empty pacer.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            queue: Vec::new(),
            in_flight: BTreeMap::new(),
            next_seq: 1,
            next_send_at: None,
            bytes_in_flight: 0,
        }
    }

    /// Enqueues pre-formed packets for one logical frame.
    pub(crate) fn enqueue_packets(
        &mut self,
        frame_id: u64,
        packets: impl IntoIterator<Item = MediaPacket>,
        now: Instant,
    ) {
        for packet in packets {
            if packet.payload.is_empty() {
                continue;
            }
            self.queue.push(QueuedPacket {
                frame_id,
                payload: packet.payload,
                fin: packet.fin,
                counts_as_video: packet.counts_as_video,
                queued_at: now,
                at_head_at: None,
            });
        }
        self.mark_queue_head(now);
    }

    /// Marks the head-of-line packet's service-time start if not already set.
    pub(crate) fn mark_queue_head(&mut self, now: Instant) {
        if let Some(packet) = self.queue.first_mut()
            && packet.at_head_at.is_none()
        {
            packet.at_head_at = Some(now);
        }
    }

    /// Whether any media packet is waiting in the queue.
    #[must_use]
    pub(crate) fn has_media(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Age of the oldest queued packet, if any.
    #[must_use]
    pub(crate) fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.queue.first().map(|packet| {
            let start = packet.at_head_at.unwrap_or(packet.queued_at);
            now.saturating_duration_since(start)
        })
    }

    /// Total bytes currently in flight on the network.
    #[must_use]
    pub(crate) const fn bytes_in_flight(&self) -> u64 {
        self.bytes_in_flight
    }

    /// Updates the pacing deadline after a send of `len` bytes at `pacing_rate`.
    pub(crate) fn record_send(&mut self, len: usize, pacing_rate: u64, sent_at: Instant) {
        if pacing_rate == 0 {
            self.next_send_at = Some(sent_at);
            return;
        }

        let interval = pacing_interval(len, pacing_rate);
        self.next_send_at = Some(sent_at + interval);
    }

    /// Returns `true` when pacing allows a send at `now`.
    #[must_use]
    pub(crate) fn pacing_ready(&self, now: Instant) -> bool {
        match self.next_send_at {
            Some(deadline) => now >= deadline,
            None => true,
        }
    }

    /// Pacing wait deadline when not yet ready.
    #[must_use]
    pub(crate) fn pacing_wait_until(&self) -> Option<Instant> {
        self.next_send_at
    }

    /// Dequeues the next media packet and registers it in flight.
    pub(crate) fn pop_packet(&mut self, now: Instant) -> Option<(PacketMeta, Bytes)> {
        if self.queue.is_empty() {
            return None;
        }

        let packet = self.queue.remove(0);
        let at_head_at = packet.at_head_at.unwrap_or(packet.queued_at);
        self.mark_queue_head(now);

        let seq = self.next_seq;
        self.next_seq += 1;

        let meta = PacketMeta {
            seq,
            frame_id: packet.frame_id,
            is_filler: false,
            len: packet.payload.len(),
            queued_at: packet.queued_at,
            at_head_at,
            fin: packet.fin,
            counts_as_video: packet.counts_as_video,
        };

        self.in_flight.insert(seq, meta.clone());
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(meta.len as u64);

        Some((meta, packet.payload))
    }

    /// Registers a filler packet as in flight.
    pub(crate) fn register_filler(&mut self, seq: u64, len: usize, now: Instant) -> PacketMeta {
        let meta = PacketMeta {
            seq,
            frame_id: 0,
            is_filler: true,
            len,
            queued_at: now,
            at_head_at: now,
            fin: true,
            counts_as_video: false,
        };
        self.in_flight.insert(seq, meta.clone());
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(len as u64);
        self.next_seq = self.next_seq.max(seq + 1);
        meta
    }

    /// Completes a packet by sequence number; returns metadata for service-time tracking.
    #[must_use]
    pub(crate) fn complete_packet(&mut self, seq: u64) -> Option<PacketMeta> {
        let meta = self.in_flight.remove(&seq)?;
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(meta.len as u64);
        Some(meta)
    }

    /// Drops in-flight state without completing service-time (loss path).
    pub(crate) fn drop_packet(&mut self, seq: u64) -> Option<PacketMeta> {
        let meta = self.in_flight.remove(&seq)?;
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(meta.len as u64);
        Some(meta)
    }

    /// Allocates the next sequence number for a filler packet.
    #[must_use]
    pub(crate) fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new()
    }
}

fn pacing_interval(len: usize, pacing_rate: u64) -> Duration {
    if pacing_rate == 0 {
        return Duration::ZERO;
    }
    let nanos = (len as u128)
        .saturating_mul(1_000_000_000)
        .saturating_div(pacing_rate as u128);
    Duration::from_nanos(nanos.min(u64::MAX as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::chunk_payload;

    #[test]
    fn enqueues_preformed_packets() {
        let mut pacer = Pacer::new();
        let now = Instant::now();
        let packets = chunk_payload([1u8; 250], 100);
        pacer.enqueue_packets(1, packets, now);
        assert!(pacer.has_media());

        let (meta, payload) = pacer.pop_packet(now).expect("packet");
        assert_eq!(payload.len(), 100);
        assert_eq!(meta.len, 100);
        assert!(!meta.fin);

        let (meta, payload) = pacer.pop_packet(now).expect("packet");
        assert_eq!(payload.len(), 100);
        assert!(!meta.fin);

        let (meta, payload) = pacer.pop_packet(now).expect("packet");
        assert_eq!(payload.len(), 50);
        assert!(meta.fin);
        assert!(!pacer.has_media());
    }
}
