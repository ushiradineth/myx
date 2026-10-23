//! Managed PCM equalizer and demand-driven spectrum analysis.

pub mod equalizer;
pub(crate) mod managed;
pub mod visualizer;

pub use equalizer::{
    EqualizerPreset, EqualizerSettings, EQ_FREQUENCIES_HZ, MAX_EQ_GAIN_DB, MIN_EQ_GAIN_DB,
    NUM_EQ_BANDS,
};
pub use visualizer::{VisBands, NUM_BANDS};
