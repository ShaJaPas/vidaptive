#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

extern crate alloc;

mod cc;
pub mod config;
mod encoder_ctrl;
mod metrics;
mod session;
mod transport;
mod types;

pub use cc::{CongestionController, Copa, CopaConfig, NoopCc};
pub use config::{ConfigError, VidaptiveConfig};
pub use session::Vidaptive;
pub use transport::filler::{BacklogFiller, DEFAULT_DUMMY_PACKET_SIZE, DummyFiller};
pub use types::{
    AckInfo, CaptureAdvice, EncodedFrame, EncoderAdvice, LostPacket, MediaPacket, SentPacket,
    TransmitAction, VidaptiveInput, VidaptiveOutput, chunk_payload,
};
