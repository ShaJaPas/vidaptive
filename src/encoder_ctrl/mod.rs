//! Encoder-side latency safeguards and α optimization.

mod alpha;
mod safeguards;

pub(crate) use alpha::AlphaController;
pub(crate) use safeguards::Safeguards;
