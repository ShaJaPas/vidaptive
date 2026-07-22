//! Vidaptive session: CC, pacer, filler, safeguards, and α control.

use alloc::collections::BTreeMap;
use core::time::Duration;
use std::time::Instant;

use crate::cc::CongestionController;
use crate::config::VidaptiveConfig;
use crate::encoder_ctrl::{AlphaController, Safeguards};
use crate::metrics::{FrameTimingBuffer, VideoBitrateWindow};
use crate::transport::Pacer;
use crate::transport::filler::BacklogFiller;
use crate::types::{
    AckInfo, CaptureAdvice, EncodedFrame, EncoderAdvice, FrameServiceSample, LostPacket,
    SentPacket, TransmitAction, VidaptiveInput, VidaptiveOutput,
};

/// Main Vidaptive session wiring CC, pacer, filler, safeguards, and α control.
pub struct Vidaptive<CC, F>
where
    CC: CongestionController,
    F: BacklogFiller,
{
    cc: CC,
    filler: F,
    config: VidaptiveConfig,
    pacer: Pacer,
    safeguards: Safeguards,
    alpha: AlphaController,
    samples: FrameTimingBuffer,
    encoder: EncoderAdvice,
    cached_target_bitrate_bps: u64,
    next_capture_at: Option<Instant>,
    frame_targets: BTreeMap<u64, FrameEncodeContext>,
    frames_in_flight: BTreeMap<u64, FrameInFlight>,
    held_capture_at: Option<Instant>,
    last_optimizer_tick: Instant,
    video_bitrate: VideoBitrateWindow,
}

#[derive(Debug, Clone, Copy)]
struct FrameEncodeContext {
    target_bitrate_bps: u64,
    cc_rate_bytes_per_sec: u64,
}

#[derive(Debug, Clone, Copy)]
struct FrameInFlight {
    head_at: Instant,
    target_bitrate_bps: u64,
    cc_rate_bytes_per_sec: u64,
}

impl<CC, F> Vidaptive<CC, F>
where
    CC: CongestionController,
    F: BacklogFiller,
{
    /// Creates a session from a congestion controller, filler, and configuration.
    #[must_use]
    pub fn new(cc: CC, filler: F, config: VidaptiveConfig) -> Self {
        let samples = FrameTimingBuffer::new(config.optimizer_window());
        let video_bitrate = VideoBitrateWindow::new(config.optimizer_window());
        let pacing = cc.cc_rate();
        let cached_target_bitrate_bps = pacing.saturating_mul(8);
        let encoder = EncoderAdvice {
            target_bitrate_bps: cached_target_bitrate_bps,
            pause: false,
            alpha: 1.0,
        };
        Self {
            cc,
            filler,
            pacer: Pacer::new(),
            safeguards: Safeguards::new(&config),
            alpha: AlphaController::new(&config),
            samples,
            encoder,
            cached_target_bitrate_bps,
            config,
            next_capture_at: None,
            frame_targets: BTreeMap::new(),
            frames_in_flight: BTreeMap::new(),
            held_capture_at: None,
            last_optimizer_tick: Instant::now(),
            video_bitrate,
        }
    }

    /// Returns a reference to the congestion controller.
    #[must_use]
    pub const fn congestion_controller(&self) -> &CC {
        &self.cc
    }

    /// Returns a mutable reference to the congestion controller.
    pub const fn congestion_controller_mut(&mut self) -> &mut CC {
        &mut self.cc
    }

    /// Returns a reference to the backlog filler.
    #[must_use]
    pub const fn filler(&self) -> &F {
        &self.filler
    }

    /// Returns a mutable reference to the backlog filler.
    pub const fn filler_mut(&mut self) -> &mut F {
        &mut self.filler
    }

    /// Enqueues pre-formed wire packets for one logical frame.
    pub fn enqueue_packets(&mut self, frame: EncodedFrame, now: Instant) {
        self.frame_targets.insert(
            frame.id,
            FrameEncodeContext {
                target_bitrate_bps: frame.target_bitrate,
                cc_rate_bytes_per_sec: self.cc.cc_rate(),
            },
        );
        self.pacer.enqueue_packets(frame.id, frame.packets, now);
        self.refresh_encoder_state(now);
    }

    /// Guidance for whether to encode a newly captured camera frame.
    pub fn advise_capture(&mut self, captured_at: Instant, now: Instant) -> CaptureAdvice {
        self.advance_capture_schedule(captured_at);

        if self.encoder.pause {
            self.held_capture_at = Some(captured_at);
            return CaptureAdvice::Hold;
        }

        if let Some(held) = self.held_capture_at.take() {
            let age = now.saturating_duration_since(held);
            if age <= self.config.frame_interval() / 2 {
                return CaptureAdvice::EncodeHeld { captured_at: held };
            }
            return CaptureAdvice::SkipHeld { captured_at: held };
        }

        CaptureAdvice::Encode
    }

    /// Returns the next wire action, if any.
    #[must_use]
    pub fn poll_transmit(&mut self, now: Instant) -> Option<TransmitAction> {
        self.poll_transmit_inner(now)
    }

    /// Whether the pacer has queued media packets waiting to be sent.
    ///
    /// Callers can use this to avoid polling for fillers when they intend to
    /// skip them anyway — `poll_transmit` pre-registers a filler in the pacer
    /// (incrementing `bytes_in_flight` and the pacer's seq) before returning
    /// `SendFiller`, so skipping the send would leak a phantom packet that
    /// inflates `bytes_in_flight` forever and diverges the seq counters.
    #[must_use]
    pub fn has_media(&self) -> bool {
        self.pacer.has_media()
    }

    /// Number of packets currently waiting in the pacer queue.
    #[must_use]
    pub fn pacer_queue_len(&self) -> usize {
        self.pacer.queue_len()
    }

    /// Age of the oldest packet still waiting in the pacer queue, if any.
    #[must_use]
    pub fn pacer_oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.pacer.oldest_queued_age(now)
    }

    /// Bytes currently in flight on the network (pacer accounting).
    #[must_use]
    pub fn bytes_in_flight(&self) -> u64 {
        self.pacer.bytes_in_flight()
    }

    /// Whether the safeguards have paused the encoder (head-of-line queue too old).
    #[must_use]
    pub fn encoder_paused(&self) -> bool {
        self.encoder.pause
    }

    /// Returns current encoder advice (bitrate, pause, α).
    #[must_use]
    pub fn poll_encoder(&mut self, _now: Instant) -> EncoderAdvice {
        self.encoder
    }

    /// Handles one or more network ACKs in a single feedback event.
    pub fn on_network_acks(&mut self, acks: &[AckInfo], now: Instant) {
        self.cc.on_packets_acked(acks);
        for ack in acks {
            if let Some(meta) = self.pacer.complete_packet(ack.seq) {
                self.on_packet_completed(meta);
            }
        }
        self.refresh_encoder_state(now);
    }

    /// Handles a network ACK.
    pub fn on_network_ack(&mut self, ack: AckInfo, now: Instant) {
        self.on_network_acks(core::slice::from_ref(&ack), now);
    }

    /// Handles a network loss notification.
    pub fn on_network_loss(&mut self, loss: LostPacket, now: Instant) {
        self.cc.on_packet_lost(&loss);
        if let Some(meta) = self.pacer.drop_packet(loss.seq) {
            self.on_packet_completed(meta);
        }
        self.refresh_encoder_state(now);
    }

    /// Periodic tick: optimizer, eviction, safeguards.
    pub fn tick(&mut self, now: Instant) {
        self.samples.evict(now);
        self.video_bitrate.evict(now);
        if now.saturating_duration_since(self.last_optimizer_tick) >= self.config.optimizer_window()
        {
            self.run_optimizer(now);
        }
        self.refresh_encoder_state(now);
    }

    /// Consolidated event handler.
    #[must_use]
    pub fn handle(&mut self, input: VidaptiveInput, now: Instant) -> VidaptiveOutput {
        match input {
            VidaptiveInput::PacketsEnqueued(frame) => self.enqueue_packets(frame, now),
            VidaptiveInput::Ack(ack) => self.on_network_ack(ack, now),
            VidaptiveInput::Loss(loss) => self.on_network_loss(loss, now),
            VidaptiveInput::Tick => self.tick(now),
        }

        VidaptiveOutput {
            transmit: self.poll_transmit(now),
            encoder: self.poll_encoder(now),
        }
    }

    fn poll_transmit_inner(&mut self, now: Instant) -> Option<TransmitAction> {
        if !self.pacer.pacing_ready(now) {
            return Some(TransmitAction::WaitUntil(
                self.pacer.pacing_wait_until().unwrap_or(now),
            ));
        }

        let bytes_in_flight = self.pacer.bytes_in_flight();
        if !self.cc.can_send(bytes_in_flight) {
            return Some(TransmitAction::Idle);
        }

        if self.pacer.has_media() {
            return self.send_packet(now);
        }

        if self.should_send_filler(now) {
            return self.send_filler(now);
        }

        Some(TransmitAction::Idle)
    }

    fn send_packet(&mut self, now: Instant) -> Option<TransmitAction> {
        let (meta, payload) = self.pacer.pop_packet(now)?;

        self.frames_in_flight
            .entry(meta.frame_id)
            .or_insert_with(|| {
                let ctx =
                    self.frame_targets
                        .get(&meta.frame_id)
                        .copied()
                        .unwrap_or(FrameEncodeContext {
                            target_bitrate_bps: self.cached_target_bitrate_bps,
                            cc_rate_bytes_per_sec: self.cc.cc_rate(),
                        });
                FrameInFlight {
                    head_at: meta.at_head_at,
                    target_bitrate_bps: ctx.target_bitrate_bps,
                    cc_rate_bytes_per_sec: ctx.cc_rate_bytes_per_sec,
                }
            });

        // After pop, bif already includes this packet. If cwnd still has room
        // and the pacer has no more media, this send was application-limited.
        let app_limited = !self.pacer.has_media() && self.cc.can_send(self.pacer.bytes_in_flight());
        let sent = SentPacket {
            seq: meta.seq,
            len: meta.len,
            sent_at: now,
            app_limited,
        };
        self.cc.on_packet_sent(&sent);
        self.pacer.record_send(meta.len, self.cc.pacing_rate(), now);
        if meta.counts_as_video {
            self.video_bitrate.record(meta.len as u64, now);
        }

        if meta.fin {
            self.complete_frame_service(meta.frame_id, now);
        }

        Some(TransmitAction::SendPacket { payload })
    }

    fn send_filler(&mut self, now: Instant) -> Option<TransmitAction> {
        let payload = self.filler.next_packet(now)?;
        let len = payload.len();
        let seq = self.pacer.next_seq();
        let _meta = self.pacer.register_filler(seq, len, now);

        // Fillers intentionally probe spare capacity — never app-limited.
        let sent = SentPacket {
            seq,
            len,
            sent_at: now,
            app_limited: false,
        };
        self.cc.on_packet_sent(&sent);
        self.pacer.record_send(len, self.cc.pacing_rate(), now);
        self.filler.on_packet_sent(seq, len, now);

        Some(TransmitAction::SendFiller { payload })
    }

    fn should_send_filler(&self, now: Instant) -> bool {
        if self.encoder.pause {
            return false;
        }

        if self.cc.pacing_rate() == 0 {
            return false;
        }

        if self.video_bitrate.bitrate_bps(now) >= self.config.max_video_bitrate() {
            return false;
        }

        if let Some(next_capture) = self.next_capture_at {
            let until = next_capture
                .checked_sub(self.config.filler_withhold_before_frame())
                .unwrap_or(next_capture);
            if now >= until && now < next_capture {
                return false;
            }
        }

        true
    }

    fn advance_capture_schedule(&mut self, captured_at: Instant) {
        self.next_capture_at = Some(captured_at + self.config.frame_interval());
    }

    fn complete_frame_service(&mut self, frame_id: u64, now: Instant) {
        let Some(inflight) = self.frames_in_flight.remove(&frame_id) else {
            return;
        };
        self.frame_targets.remove(&frame_id);

        let service_time = now.saturating_duration_since(inflight.head_at);
        self.samples.push(FrameServiceSample {
            service_time,
            target_bitrate_bps: inflight.target_bitrate_bps,
            cc_rate_bytes_per_sec: inflight.cc_rate_bytes_per_sec,
            at: now,
        });
    }

    fn on_packet_completed(&mut self, meta: crate::types::PacketMeta) {
        if meta.is_filler {
            return;
        }
        if meta.fin {
            self.frames_in_flight.remove(&meta.frame_id);
            self.frame_targets.remove(&meta.frame_id);
        }
    }

    fn run_optimizer(&mut self, now: Instant) {
        let paused = self.safeguards.is_paused();
        let (alpha, target_bitrate_bps) =
            self.alpha.update(&self.samples, self.cc.cc_rate(), paused);
        self.encoder.alpha = alpha;
        self.cached_target_bitrate_bps = if paused { 0 } else { target_bitrate_bps };
        self.last_optimizer_tick = now;
    }

    fn refresh_encoder_state(&mut self, now: Instant) {
        let oldest = self.pacer.oldest_queued_age(now);
        self.safeguards.update(oldest, !self.pacer.has_media());
        let was_paused = self.encoder.pause;
        let now_paused = self.safeguards.is_paused();

        if now_paused && !was_paused {
            // Entering pause: drop stale high target so unpause cannot
            // instantly restore a pre-congestion 10 Mbps cache.
            self.cached_target_bitrate_bps = 0;
        } else if !now_paused && was_paused {
            // Leaving pause: recompute α·cc_rate immediately instead of
            // waiting up to optimizer_window with a zero/stale cache.
            self.run_optimizer(now);
        }

        self.encoder.pause = now_paused;
        self.encoder.target_bitrate_bps = if now_paused {
            0
        } else {
            self.cached_target_bitrate_bps
        };
        self.encoder.alpha = self.alpha.alpha();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DummyFiller;
    use crate::cc::NoopCc;
    use crate::types::chunk_payload;
    use core::time::Duration;

    fn frame(
        id: u64,
        captured_at: Instant,
        target_bitrate: u64,
        data_len: usize,
        chunk_size: usize,
    ) -> EncodedFrame {
        EncodedFrame {
            id,
            captured_at,
            target_bitrate,
            packets: chunk_payload(vec![0u8; data_len], chunk_size),
        }
    }

    #[test]
    fn sends_dummy_when_queue_empty() {
        let cc = NoopCc::new(100_000, 100_000, Duration::from_millis(50));
        let config = VidaptiveConfig::default();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let now = Instant::now();

        let action = session.poll_transmit(now).expect("action");
        assert!(matches!(action, TransmitAction::SendFiller { .. }));
    }

    #[test]
    fn pauses_encoder_when_queue_backlogged() {
        let cc = NoopCc::new(1, 1_000_000, Duration::from_millis(50));
        let config = VidaptiveConfig::builder()
            .tau_pause(Duration::from_millis(1))
            .build()
            .unwrap();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let t0 = Instant::now();
        session.enqueue_packets(frame(1, t0, 500_000, 2_000, 500), t0);

        let later = t0 + Duration::from_millis(10);
        session.refresh_encoder_state(later);
        assert!(session.encoder.pause);
        assert_eq!(session.encoder.target_bitrate_bps, 0);
        assert_eq!(session.cached_target_bitrate_bps, 0);
    }

    #[test]
    fn unpause_recomputes_target_instead_of_restoring_stale_cache() {
        let cc = NoopCc::new(100_000, 100_000, Duration::from_millis(50));
        let config = VidaptiveConfig::builder()
            .tau_pause(Duration::from_millis(1))
            .optimizer_window(Duration::from_secs(10))
            .build()
            .unwrap();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let t0 = Instant::now();

        // Seed a high cached target, then enter pause via backlog.
        session.cached_target_bitrate_bps = 10_000_000;
        session.encoder.target_bitrate_bps = 10_000_000;
        session.encoder.pause = false;
        session.enqueue_packets(frame(1, t0, 10_000_000, 2_000, 500), t0);
        let paused_at = t0 + Duration::from_millis(10);
        session.refresh_encoder_state(paused_at);
        assert!(session.encoder.pause);
        assert_eq!(session.cached_target_bitrate_bps, 0);

        // Simulate queue drain: empty pacer → safeguards clear pause → optimizer runs.
        session.pacer = Pacer::new();
        let resumed_at = paused_at + Duration::from_millis(1);
        session.refresh_encoder_state(resumed_at);
        assert!(!session.encoder.pause);
        // Fresh optimizer pass — not the stale 10 Mbps.
        assert_ne!(session.encoder.target_bitrate_bps, 10_000_000);
        assert_eq!(
            session.encoder.target_bitrate_bps,
            session.cached_target_bitrate_bps
        );
    }

    #[test]
    fn holds_capture_while_paused() {
        let cc = NoopCc::new(1, 1_000_000, Duration::from_millis(50));
        let config = VidaptiveConfig::builder()
            .tau_pause(Duration::from_millis(1))
            .build()
            .unwrap();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let t0 = Instant::now();
        session.enqueue_packets(frame(1, t0, 500_000, 2_000, 500), t0);
        let later = t0 + Duration::from_millis(10);
        session.refresh_encoder_state(later);
        assert!(session.encoder.pause);

        let advice = session.advise_capture(t0, later);
        assert_eq!(advice, CaptureAdvice::Hold);
    }

    #[test]
    fn encodes_held_capture_on_resume_within_half_frame() {
        let cc = NoopCc::new(100_000, 100_000, Duration::from_millis(50));
        let config = VidaptiveConfig::default();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let t0 = Instant::now();

        session.encoder.pause = true;
        let _ = session.advise_capture(t0, t0);

        session.encoder.pause = false;
        let resume = t0 + Duration::from_millis(10);
        let advice = session.advise_capture(resume, resume);
        assert_eq!(advice, CaptureAdvice::EncodeHeld { captured_at: t0 });
    }

    #[test]
    fn skips_stale_held_capture_on_resume() {
        let cc = NoopCc::new(100_000, 100_000, Duration::from_millis(50));
        let config = VidaptiveConfig::default();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let t0 = Instant::now();

        session.encoder.pause = true;
        let _ = session.advise_capture(t0, t0);

        session.encoder.pause = false;
        let resume = t0 + Duration::from_millis(20);
        let advice = session.advise_capture(resume, resume);
        assert_eq!(advice, CaptureAdvice::SkipHeld { captured_at: t0 });
    }

    #[test]
    fn target_bitrate_only_changes_on_tick() {
        let cc = NoopCc::new(100_000, 100_000, Duration::from_millis(50));
        let config = VidaptiveConfig::builder()
            .optimizer_window(Duration::from_millis(100))
            .build()
            .unwrap();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let t0 = Instant::now();
        let initial = session.encoder.target_bitrate_bps;

        session.tick(t0 + Duration::from_millis(150));
        let after_tick = session.encoder.target_bitrate_bps;

        session.enqueue_packets(
            frame(1, t0, after_tick, 100, 100),
            t0 + Duration::from_millis(160),
        );
        assert_eq!(session.poll_encoder(t0).target_bitrate_bps, after_tick);
        assert_ne!(initial, 0);
    }

    #[test]
    fn filler_withhold_follows_capture_schedule_not_enqueue() {
        let cc = NoopCc::new(100_000, 100_000, Duration::from_millis(50));
        let config = VidaptiveConfig::default();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let t0 = Instant::now();

        let _ = session.advise_capture(t0, t0);

        session.enqueue_packets(
            frame(1, t0, 1_000_000, 100, 100),
            t0 + Duration::from_millis(20),
        );

        let during_withhold = t0 + Duration::from_nanos(25_000_000);
        assert!(!session.should_send_filler(during_withhold));

        let before_withhold = t0 + Duration::from_millis(20);
        assert!(session.should_send_filler(before_withhold));
    }

    #[test]
    fn stops_filler_above_max_video_bitrate() {
        let cc = NoopCc::new(10_000_000, 10_000_000, Duration::from_millis(50));
        let config = VidaptiveConfig::builder()
            .optimizer_window(Duration::from_secs(1))
            .build()
            .unwrap();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let t0 = Instant::now();

        session.video_bitrate.record(1_500_000, t0);
        assert!(!session.should_send_filler(t0));
    }

    #[test]
    fn parity_packets_do_not_count_toward_video_bitrate() {
        let cc = NoopCc::new(10_000_000, 10_000_000, Duration::from_millis(50));
        let config = VidaptiveConfig::builder()
            .optimizer_window(Duration::from_secs(1))
            .build()
            .unwrap();
        let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
        let t0 = Instant::now();

        session.enqueue_packets(
            EncodedFrame {
                id: 1,
                captured_at: t0,
                target_bitrate: 1_000_000,
                packets: vec![
                    crate::types::MediaPacket::video(
                        bytes::Bytes::from(vec![0u8; 1_000_000]),
                        false,
                    ),
                    crate::types::MediaPacket::non_video(
                        bytes::Bytes::from(vec![0u8; 500_000]),
                        true,
                    ),
                ],
            },
            t0,
        );

        let action = session.poll_transmit(t0).expect("send data");
        assert!(matches!(action, TransmitAction::SendPacket { .. }));
        let _ = session.poll_transmit(t0);
        assert!(session.should_send_filler(t0));
    }
}
