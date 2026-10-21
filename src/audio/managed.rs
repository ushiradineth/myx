//! Reuse normal playback's EQ and FFT on managed Soloist's native PCM frames.
use super::{
    equalizer::{EqualizerControl, FilterBank},
    visualizer::{SpectrumAnalyzer, VisBands},
};
use soloist_runtime::pcm::{AudioProcessor, BLOCK_SAMPLES, CHANNELS, SAMPLE_RATE};
use std::sync::{Arc, Mutex};

pub(crate) struct ManagedProcessor {
    settings: EqualizerControl,
    filters: FilterBank,
    spectrum: SpectrumAnalyzer,
    bands: Arc<Mutex<VisBands>>,
    scratch: [f64; BLOCK_SAMPLES],
}
impl ManagedProcessor {
    pub(crate) fn new(settings: EqualizerControl, bands: Arc<Mutex<VisBands>>) -> Self {
        let initial = *settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self {
            filters: FilterBank::new(initial),
            settings,
            spectrum: SpectrumAnalyzer::new(bands.clone(), SAMPLE_RATE as f32),
            bands,
            scratch: [0.0; BLOCK_SAMPLES],
        }
    }
}
impl AudioProcessor for ManagedProcessor {
    fn process(&mut self, samples: &mut [f32]) {
        assert!(samples.len() <= BLOCK_SAMPLES && samples.len().is_multiple_of(CHANNELS));
        if let Ok(settings) = self.settings.try_lock() {
            self.filters.update(*settings);
        }
        let scratch = &mut self.scratch[..samples.len()];
        for (out, input) in scratch.iter_mut().zip(samples.iter()) {
            *out = f64::from(*input);
        }
        self.filters.process_interleaved(scratch);
        self.spectrum.process(scratch);
        for (out, input) in samples.iter_mut().zip(scratch.iter()) {
            *out = *input as f32;
        }
        if let Ok(mut bands) = self.bands.try_lock() {
            bands.is_active = true;
        }
    }
    fn reset(&mut self) {
        self.filters.reset_states();
        self.spectrum.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::equalizer::shared_equalizer;
    #[test]
    fn bypass_is_exact_and_stereo_isolated_with_live_post_eq_spectrum() {
        let settings = shared_equalizer();
        let bands = VisBands::shared();
        let mut processor = ManagedProcessor::new(settings.clone(), bands.clone());
        let mut samples = [0.0; BLOCK_SAMPLES];
        for (i, frame) in samples.chunks_exact_mut(2).enumerate() {
            frame[0] = (i as f32 * 0.1).sin() * 0.1;
        }
        let original = samples;
        processor.process(&mut samples);
        assert_eq!(samples, original);
        settings.lock().unwrap().gains_db[5] = 6;
        for _ in 0..8 {
            samples = original;
            processor.process(&mut samples);
        }
        assert!(samples.chunks_exact(2).all(|frame| frame[1] == 0.0));
        assert!(samples
            .iter()
            .all(|sample| sample.is_finite() && sample.abs() <= 1.0));
        assert!(bands.lock().unwrap().values.iter().any(|band| *band > 0.0));
        assert!(bands.lock().unwrap().is_active);
        processor.reset();
        let bands = bands.lock().unwrap();
        assert!(!bands.is_active);
        assert!(bands.values.iter().all(|band| *band == 0.0));
    }
}
