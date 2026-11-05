//! Managed PCM equalizer and demand-driven spectrum analysis.

pub mod equalizer;
pub(crate) mod managed;
pub mod visualizer;

pub use equalizer::{
    EqualizerPreset, EqualizerSettings, EQ_FREQUENCIES_HZ, MAX_EQ_GAIN_DB, MIN_EQ_GAIN_DB,
    NUM_EQ_BANDS,
};
pub use soloist_runtime::pcm::AudioProcessor;
pub use visualizer::{VisBands, NUM_BANDS};

/// The production managed PCM processor, also used by credential-free native
/// fixtures. Settings and telemetry are shared with the owning player actor.
pub fn managed_processor(
    settings: std::sync::Arc<std::sync::Mutex<EqualizerSettings>>,
    bands: std::sync::Arc<std::sync::Mutex<VisBands>>,
    spectrum_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Box<dyn AudioProcessor> {
    Box::new(managed::ManagedProcessor::new(
        settings,
        bands,
        spectrum_enabled,
    ))
}
