//! The host's private PCM contract. No SDK, UI, or librespot types cross it.
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
    Arc,
};

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: usize = 2;
pub const BLOCK_FRAMES: usize = 128;
pub const BLOCK_SAMPLES: usize = BLOCK_FRAMES * CHANNELS;

pub trait AudioProcessor: Send + 'static {
    /// Interleaved stereo float32 at SAMPLE_RATE. Never block or retain samples.
    fn process(&mut self, samples: &mut [f32]);
    fn reset(&mut self);
}

#[derive(Default)]
struct State {
    active: AtomicBool,
    generation: AtomicU64,
    health: AtomicU8,
    processed: AtomicU64,
    peak_pending: AtomicU64,
    latency: AtomicU64,
}

#[derive(Clone, Default)]
pub struct AudioControl(Arc<State>);

#[derive(Debug, Clone, Copy)]
pub struct AudioStats {
    pub processed_frames: u64,
    pub peak_pending_frames: u64,
    /// PulseAudio's combined stream buffering estimate, not an acoustic measurement.
    pub estimated_latency_micros: u64,
}

impl AudioControl {
    pub fn set_active(&self, active: bool) {
        if self.0.active.swap(active, Ordering::AcqRel) != active {
            self.reset();
        }
    }
    pub fn reset(&self) {
        self.0.generation.fetch_add(1, Ordering::AcqRel);
    }
    pub fn is_ready(&self) -> bool {
        self.0.health.load(Ordering::Acquire) == 1
    }
    pub fn stats(&self) -> AudioStats {
        AudioStats {
            processed_frames: self.0.processed.load(Ordering::Relaxed),
            peak_pending_frames: self.0.peak_pending.load(Ordering::Relaxed),
            estimated_latency_micros: self.0.latency.load(Ordering::Relaxed),
        }
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn active(&self) -> bool {
        self.0.active.load(Ordering::Acquire)
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::Acquire)
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn health(&self, health: u8) {
        self.0.health.store(health, Ordering::Release);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn record(&self, frames: usize, pending: usize, latency: Option<u64>) {
        self.0.processed.fetch_add(frames as u64, Ordering::Relaxed);
        self.0
            .peak_pending
            .fetch_max(pending as u64, Ordering::Relaxed);
        if let Some(latency) = latency {
            self.0.latency.store(latency, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn control_defaults_to_inactive_and_repeated_state_changes_are_idempotent() {
        let control = AudioControl::default();
        assert!(!control.is_ready());
        assert!(!control.0.active.load(Ordering::Acquire));
        control.set_active(true);
        assert_eq!(control.0.generation.load(Ordering::Acquire), 1);
        control.set_active(true);
        assert_eq!(control.0.generation.load(Ordering::Acquire), 1);
        control.reset();
        control.set_active(false);
        assert_eq!(control.0.generation.load(Ordering::Acquire), 3);
    }
}
