//! Shared value types for transport, congestion control, and encoder control.

use alloc::vec::Vec;
use bytes::Bytes;
use core::time::Duration;
use std::time::Instant;

/// One wire-ready media packet queued in the pacer.
///
/// Vidaptive does not split payloads — packetization is the caller's responsibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPacket {
    /// On-wire payload bytes.
    pub payload: Bytes,
    /// Last packet of the logical frame (triggers service-time sampling for α).
    pub fin: bool,
    /// When `false` (e.g. FEC parity), bytes are not counted toward [`crate::config::VidaptiveConfig::max_video_bitrate`].
    pub counts_as_video: bool,
}

impl MediaPacket {
    /// Video/data packet (counts toward the video-bitrate cap).
    #[must_use]
    pub fn video(payload: Bytes, fin: bool) -> Self {
        Self {
            payload,
            fin,
            counts_as_video: true,
        }
    }

    /// Non-video packet (parity, etc.) — still paced and congestion-controlled.
    #[must_use]
    pub fn non_video(payload: Bytes, fin: bool) -> Self {
        Self {
            payload,
            fin,
            counts_as_video: false,
        }
    }
}

/// Splits a payload into fixed-size packets for tests and plain-UDP paths without FEC.
#[must_use]
pub fn chunk_payload(data: impl AsRef<[u8]>, chunk_size: usize) -> Vec<MediaPacket> {
    let data = data.as_ref();
    if data.is_empty() || chunk_size == 0 {
        return Vec::new();
    }

    let chunks: Vec<_> = data.chunks(chunk_size).collect();
    let last = chunks.len().saturating_sub(1);

    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| MediaPacket::video(Bytes::copy_from_slice(chunk), index == last))
        .collect()
}

/// A logical encoded frame: metadata plus pre-packetized wire payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Application-defined monotonic frame id.
    pub id: u64,
    /// Wall time when the frame was captured or read from the source.
    pub captured_at: Instant,
    /// Target bitrate (`tri`) the encoder was given for this frame (bits per second).
    pub target_bitrate: u64,
    /// Pre-formed packets for this frame.
    pub packets: Vec<MediaPacket>,
}

/// Metadata for a packet queued in the pacer or in flight on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PacketMeta {
    /// Monotonic send sequence number.
    pub seq: u64,
    /// Owning frame id (`0` for filler packets).
    pub frame_id: u64,
    /// `true` for backlog filler (dummy).
    pub is_filler: bool,
    /// Payload length in bytes.
    pub len: usize,
    /// Time the packet entered the pacer queue.
    pub queued_at: Instant,
    /// Time the packet became head-of-line in the pacer (service-time start).
    pub at_head_at: Instant,
    /// `true` when this is the last packet of its logical frame.
    pub fin: bool,
    /// Whether this packet counts toward the video-bitrate cap.
    pub counts_as_video: bool,
}

/// Congestion-control notification for a sent packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentPacket {
    /// Sequence number assigned at send time.
    pub seq: u64,
    /// Payload length in bytes.
    pub len: usize,
    /// Send timestamp.
    pub sent_at: Instant,
}

/// Receiver acknowledgement for a previously sent packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckInfo {
    /// Acknowledged sequence number.
    pub seq: u64,
    /// Payload length in bytes.
    pub len: usize,
    /// Time the ACK was observed at the sender.
    pub acked_at: Instant,
}

/// Loss notification for a previously sent packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostPacket {
    /// Lost sequence number.
    pub seq: u64,
    /// Payload length in bytes.
    pub len: usize,
    /// Time loss was detected.
    pub lost_at: Instant,
}

/// Advice from Vidaptive to the video encoder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderAdvice {
    /// Recommended target bitrate in bits per second (`α · CC-Rate · 8`; `0` when paused).
    pub target_bitrate_bps: u64,
    /// When `true`, the encoder should not emit new frames.
    pub pause: bool,
    /// Current headroom fraction α ∈ (0, 1].
    pub alpha: f64,
}

impl EncoderAdvice {
    /// Default advice before the first optimizer tick.
    pub const INITIAL: Self = Self {
        target_bitrate_bps: 0,
        pause: false,
        alpha: 1.0,
    };
}

/// Guidance for whether to encode a newly captured camera frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum CaptureAdvice {
    /// Encode this capture normally.
    Encode,
    /// Encoder is paused — hold the capture, do not encode yet.
    Hold,
    /// Resume: encode a previously held capture (still within Δ/2).
    EncodeHeld {
        /// Original capture timestamp of the held frame.
        captured_at: Instant,
    },
    /// Resume: discard a stale held capture and skip to the next frame.
    SkipHeld {
        /// Original capture timestamp that was too old to encode.
        captured_at: Instant,
    },
}

/// Action the application should take on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransmitAction {
    /// Send a pre-formed media packet.
    SendPacket {
        /// Payload.
        payload: Bytes,
    },
    /// Send dummy backlog filler to keep the CC feedback loop tight.
    SendFiller {
        /// Payload.
        payload: Bytes,
    },
    /// Pacing gate: do not send before this instant.
    WaitUntil(Instant),
    /// Nothing to send right now (cwnd full, filler unavailable, etc.).
    Idle,
}

/// Consolidated input event
#[derive(Debug, Clone)]
pub enum VidaptiveInput {
    /// Pre-packetized media for a frame.
    PacketsEnqueued(EncodedFrame),
    /// Network ACK.
    Ack(AckInfo),
    /// Network loss.
    Loss(LostPacket),
    /// Periodic tick (pacing, α update, safeguards).
    Tick,
}

/// Consolidated output from
#[derive(Debug, Clone, PartialEq)]
pub struct VidaptiveOutput {
    /// Next wire action, if any.
    pub transmit: Option<TransmitAction>,
    /// Current encoder advice.
    pub encoder: EncoderAdvice,
}

/// Completed frame service-time sample for the α optimizer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameServiceSample {
    /// Service time from first packet at pacer head until last packet sent.
    pub service_time: Duration,
    /// Encoder target bitrate `tri` in bits per second when the frame was encoded.
    pub target_bitrate_bps: u64,
    /// `CC-Rate` in bytes per second when the frame was encoded.
    pub cc_rate_bytes_per_sec: u64,
    /// Sample timestamp (completion time).
    pub at: Instant,
}
