# Vidaptive

[![Crates.io](https://img.shields.io/crates/v/vidaptive.svg)](https://crates.io/crates/vidaptive)
[![Documentation](https://docs.rs/vidaptive/badge.svg)](https://docs.rs/vidaptive)
[![License](https://img.shields.io/crates/l/vidaptive.svg)](https://crates.io/crates/vidaptive)
[![Coverage Status](https://coveralls.io/repos/github/ShaJaPas/vidaptive/badge.svg)](https://coveralls.io/github/ShaJaPas/vidaptive)

Responsive rate control for real-time video.

Vidaptive decouples **encoder output** from **wire rate**: a pluggable congestion controller and pacer decide when bytes leave the sender, while dummy padding keeps the feedback loop tight when the media queue is empty. An **α optimizer** adapts the encoder target bitrate to track link capacity.

The crate is **codec-agnostic** and **packetization-agnostic** — you enqueue pre-formed wire packets ([`MediaPacket`]). Chunking, FEC, RTP, and framing are your responsibility.

## Results vs GCC

In the Vidaptive evaluation ([WebRTC M108](https://webrtc.googlesource.com/src/), **15 cellular traces**, **20× 1080p** clips, 2 min per run, Copa as CC), the algorithm was compared head-to-head with **Google Congestion Control (GCC)**:

| Metric | GCC | Vidaptive | Δ |
|--------|-----|-----------|---|
| Video bitrate (avg) | — | — | **~1.5×** |
| VMAF (avg / P50) | 39 / 36.8 | 53.5 / 58.2 | **+40%** avg |
| SSIM (P50) | 12 dB | 13.8 dB | **+1.4 dB** |
| PSNR (median) | 37.5 dB | 38.8 dB | **+1.3 dB** |
| P95 frame latency | 3.94 s | 1.70 s | **−2.2 s (−57%)** |
| Median frame latency | 48 ms | 65 ms | +17 ms |
| Avg frame latency | 734 ms | 383 ms | **−351 ms** |

GCC ties send rate to encoder output, so it ramps slowly and under-uses the link; tail latency spikes when capacity drops. Vidaptive keeps the congestion-control loop fed (pacer + dummy padding) and sets encoder bitrate separately — **more throughput and quality**, **much lower P95 latency**, at the cost of slightly higher median delay from probing.

## Features

- **Pluggable CC** — [`CongestionController`] trait; [`Copa`] and [`NoopCc`] for tests.
- **Backlog filler** — [`DummyFiller`] (≤ 200 B) when the media queue is empty but the CC can send.
- **Pacing** — interval pacing at `CC-Rate`; cwnd gate via [`CongestionController::can_send`].
- **Encoder pause** — stop encoding when the oldest queued packet waits longer than τ (default 33 ms).
- **Held capture** — on resume, encode a recently held camera frame or skip if stale.
- **α optimizer** — percentile control on frame service times; EWMA-smoothed target bitrate `α · CC-Rate · 8`.
- **Filler policy** — withhold dummy before the next expected frame; stop dummy above a configurable video send-rate cap.
- **Pre-formed ingress** — no internal MTU split; optional `counts_as_video` for non-video packets (e.g. parity).

## When to use vidaptive

**Good fits:** live video over UDP or similar datagram transports, custom send stacks, trace simulators, any setup where you already have ACK/loss feedback for a CC.

**Poor fits:** VOD / TCP bulk transfer, systems without send-side CC feedback, drop-in WebRTC (no RTP/SRTP/codec here).

## Architecture

```text
┌─────────────┐     pre-formed        ┌──────────────────────────────┐     ┌──────┐
│ Encoder /   │     MediaPackets      │ Vidaptive                    │     │ Wire │
│ packetizer  │ ────────────────────► │ CC + pacer + α + safeguards  │ ──► │ …    │
└─────────────┘                       │ + DummyFiller for gaps       │     └──────┘
       ▲                              └──────────────────────────────┘
       │  EncoderAdvice                     ▲ ACK / loss
       └────────────────────────────────────┘
```

Vidaptive queues, paces, and congestion-controls bytes. It does not packetize or encode.

## Core concepts

### Media packet (ingress)

Each [`MediaPacket`] is one on-wire payload:

| Field | Meaning |
|-------|---------|
| `payload` | Bytes to send |
| `fin` | Last packet of the logical frame (ends service-time sample for α) |
| `counts_as_video` | When `false`, bytes are still paced and CC'd but excluded from the video-bitrate filler cap |

Use [`chunk_payload`] to split a blob into fixed-size packets for tests or simple UDP paths.

### Dummy filler

When the pacer is ready, cwnd allows sending, and the media queue is empty, [`DummyFiller`] emits small zero payloads so the CC sees continuous traffic. Dummy is suppressed when:

- the encoder is paused,
- video send rate in the optimizer window exceeds `max_video_bitrate`,
- within `filler_withhold_before_frame` of the next expected camera frame.

### Frame service time

For α: time from when the **first packet of a frame** reaches the head of the pacer queue until the **last packet** (`fin`) is sent.

### Target bitrate

Once per `optimizer_window`, Vidaptive updates:

```text
target_bitrate_bps = α · CC-Rate · 8
```

α is derived from recent frame service times and the configured percentile / target service time.

### Encoder pause & held capture

If the oldest queued packet age exceeds `tau_pause`, encoding pauses (`target_bitrate_bps = 0`). On resume:

- held capture not older than half a frame interval → [`CaptureAdvice::EncodeHeld`],
- older → [`CaptureAdvice::SkipHeld`].

## Usage

```rust,no_run
use std::time::Instant;
use vidaptive::{
    chunk_payload, CaptureAdvice, DummyFiller, EncodedFrame, NoopCc, TransmitAction,
    Vidaptive, VidaptiveConfig,
};

let cc = NoopCc::new(500_000, 500_000, std::time::Duration::from_millis(50));
let config = VidaptiveConfig::builder().build().unwrap();
let mut session = Vidaptive::new(cc, DummyFiller::new(), config);
let mut now = Instant::now();

let frame = EncodedFrame {
    id: 1,
    captured_at: now,
    target_bitrate: session.poll_encoder(now).target_bitrate_bps,
    packets: chunk_payload(vec![0u8; 4_000], 1_200),
};
session.enqueue_packets(frame, now);

match session.advise_capture(now, now) {
    CaptureAdvice::Encode => { /* encode new capture */ }
    CaptureAdvice::Hold => { /* wait */ }
    CaptureAdvice::EncodeHeld { captured_at } => { /* encode held frame */ }
    CaptureAdvice::SkipHeld { .. } => { /* drop stale hold */ }
}

loop {
    session.tick(now);

    if let Some(action) = session.poll_transmit(now) {
        match action {
            TransmitAction::SendPacket { payload } => { /* send */ }
            TransmitAction::SendFiller { payload } => { /* send dummy */ }
            TransmitAction::WaitUntil(t) => { now = t; }
            TransmitAction::Idle => {}
        }
    }

    // session.on_network_ack(ack, now);
    // session.on_network_loss(loss, now);
}
```

## Configuration reference

Build with [`VidaptiveConfig::builder()`] → … → [`.build()`](VidaptiveConfig::builder). Invalid combinations return [`ConfigError`] at build time (bad percentiles, zero durations, withhold longer than frame interval, etc.).

Default builder values (@ 30 fps):

| Parameter | Default | Role |
|-----------|---------|------|
| `tau_pause` | 33 ms | Pause encoding if oldest queued packet is older than this |
| `target_service_time` | 33 ms | Target frame service time for α |
| `latency_percentile` | 0.9 | Percentile for service-time control |
| `optimizer_window` | 1 s | α update period; video bitrate measurement window |
| `alpha_ewma` | 0.25 | EWMA smoothing for α |
| `max_video_bitrate` | 12 Mbps | Stop dummy above this video send rate |
| `filler_withhold_before_frame` | 8.25 ms | No dummy just before next capture |
| `frame_interval` | 33 ms | Expected time between camera frames |

```rust,no_run
use core::num::NonZeroU64;
use core::time::Duration;
use vidaptive::VidaptiveConfig;

let config = VidaptiveConfig::builder()
    .frame_rate_hz(NonZeroU64::new(30).unwrap())
    .tau_pause(Duration::from_millis(33))
    .latency_percentile(0.9)
    .build()
    .unwrap();
```

[`VidaptiveConfig::default()`] is equivalent to `builder().build()` with the defaults above.

### `tau_pause`

When `oldest_queued_age > tau_pause`, `EncoderAdvice.pause = true` and `target_bitrate_bps = 0`. Clears when the media queue drains. Set roughly to one frame interval at your capture rate.

### `target_service_time` and `latency_percentile`

Control the latency vs quality tradeoff in α. Higher percentile or lower target service time → more conservative α.

### `optimizer_window`

How quickly α reacts to new samples; also the window for the video-bitrate filler cap.

### `max_video_bitrate` and `filler_withhold_before_frame`

Limit dummy traffic when video already fills the link; avoid dummy bursts right before a scheduled capture.

## Consolidated event API

```rust,ignore
use vidaptive::{VidaptiveInput, VidaptiveOutput};

let output: VidaptiveOutput = session.handle(VidaptiveInput::Tick, now);
```

## Further reading

- [Vidaptive paper (arXiv)](https://arxiv.org/abs/2309.16869) — methodology and full evaluation
- [Copa](https://web.mit.edu/copa/)

## License

Licensed under the Apache License, Version 2.0 (<http://www.apache.org/licenses/LICENSE-2.0>).
