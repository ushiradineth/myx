//! EQ and demand-driven FFT on managed Soloist's native PCM frames.
use super::{
    equalizer::{EqualizerControl, FilterBank},
    visualizer::{SpectrumAnalyzer, VisBands},
};
use soloist_runtime::pcm::{AudioProcessor, BLOCK_SAMPLES, CHANNELS, SAMPLE_RATE};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) struct ManagedProcessor {
    settings: EqualizerControl,
    filters: FilterBank,
    spectrum: SpectrumAnalyzer,
    spectrum_enabled: Arc<AtomicBool>,
    scratch: [f64; BLOCK_SAMPLES],
    spectrum_requested: bool,
}
impl ManagedProcessor {
    pub(crate) fn new(
        settings: EqualizerControl,
        bands: Arc<Mutex<VisBands>>,
        spectrum_enabled: Arc<AtomicBool>,
    ) -> Self {
        let initial = *settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self {
            filters: FilterBank::new(initial),
            settings,
            spectrum: SpectrumAnalyzer::new(bands, SAMPLE_RATE as f32),
            spectrum_enabled,
            scratch: [0.0; BLOCK_SAMPLES],
            spectrum_requested: false,
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
        if self.spectrum_enabled.load(Ordering::Acquire) {
            self.spectrum_requested = true;
            self.spectrum.process(scratch, &self.spectrum_enabled);
        } else if self.spectrum_requested {
            self.spectrum_requested = !self.spectrum.reset();
        }
        for (out, input) in samples.iter_mut().zip(scratch.iter()) {
            *out = *input as f32;
        }
    }
    fn reset(&mut self) {
        self.filters.reset_states();
        self.spectrum_requested = !self.spectrum.reset();
    }
    fn fft_metrics(&self) -> (u64, u64, u64) {
        (
            self.spectrum.fft_frames() as u64,
            self.spectrum.fft_compute_micros(),
            self.spectrum.peak_fft_compute_micros(),
        )
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
        let mut processor = ManagedProcessor::new(
            settings.clone(),
            bands.clone(),
            Arc::new(AtomicBool::new(true)),
        );
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

    #[test]
    fn spectrum_demand_defaults_off_and_detachment_clears_bands_without_bypassing_eq() {
        let settings = shared_equalizer();
        settings.lock().unwrap().gains_db[5] = 6;
        let bands = VisBands::shared();
        let enabled = Arc::new(AtomicBool::new(false));
        let mut processor = ManagedProcessor::new(settings, bands.clone(), enabled.clone());
        let mut input = [0.0; BLOCK_SAMPLES];
        for (i, frame) in input.chunks_exact_mut(CHANNELS).enumerate() {
            frame[0] = (i as f32 * 0.1).sin() * 0.1;
        }
        let initial_update = bands.lock().unwrap().updated_at;
        let mut samples = input;
        processor.process(&mut samples);
        assert_ne!(
            samples, input,
            "EQ still processes audio without spectrum demand"
        );
        assert_eq!(processor.spectrum.fft_frames(), 0);
        assert!(!bands.lock().unwrap().is_active);
        assert!(bands
            .lock()
            .unwrap()
            .values
            .iter()
            .all(|value| *value == 0.0));
        enabled.store(true, Ordering::Release);
        for _ in 0..8 {
            samples = input;
            processor.process(&mut samples);
        }
        assert!(bands.lock().unwrap().is_active);
        assert!(bands
            .lock()
            .unwrap()
            .values
            .iter()
            .any(|value| *value > 0.0));
        let fft_frames = processor.spectrum.fft_frames();
        assert!(fft_frames > 0);
        enabled.store(false, Ordering::Release);
        processor.process(&mut samples);
        assert_eq!(processor.spectrum.fft_frames(), fft_frames);
        let bands = bands.lock().unwrap();
        assert!(!bands.is_active);
        assert!(bands.values.iter().all(|value| *value == 0.0));
        assert!(bands.updated_at >= initial_update);
    }
}
