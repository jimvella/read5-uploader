//! WAV in, Ogg Opus out, and the optional "clean" variant of each utterance.

pub mod dsp;
pub mod opus;
pub mod process;
pub mod wav;

pub use opus::{encode_ogg_opus, EncodeParams};
pub use process::{ConditionParams, Conditioner, CHAIN_VERSION};
pub use wav::WavError;
