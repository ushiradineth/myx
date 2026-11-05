//! The host's private PCM contract. No SDK, UI, or librespot types cross it.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::sync::Mutex;
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
    /// FFT frames, cumulative transform microseconds, peak transform microseconds.
    /// These measure computation, not sample-to-display or acoustic latency.
    fn fft_metrics(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }
}

// The registration lock covers invocation and removal. The owning connection
// removes this registration before freeing the native mainloop.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) struct Wake {
    pub handle: usize,
    pub wake: unsafe extern "C" fn(*mut std::ffi::c_void),
}

#[derive(Default)]
struct TimingCounters {
    capture_gap: AtomicU64,
    write_gap: AtomicU64,
    dispatch: AtomicU64,
    transfer: AtomicU64,
    wait: AtomicU64,
    iteration_gap: AtomicU64,
    underflow_capture_gap: AtomicU64,
    underflow_pending: AtomicU64,
    underflow_credit: AtomicU64,
    underflow_count: AtomicU64,
}

/// Worker-observed progress/phase times, not hardware timing or queue occupancy.
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioTimingStats {
    pub max_capture_gap_micros: u64,
    pub max_write_gap_micros: u64,
    pub max_dispatch_micros: u64,
    pub max_transfer_micros: u64,
    pub max_wait_micros: u64,
    pub max_iteration_gap_micros: u64,
    pub last_underflow_capture_gap_micros: u64,
    pub last_underflow_pending_frames: u64,
    /// Outstanding libpulse write credit, NOT server queue occupancy.
    pub last_underflow_write_credit_frames: u64,
    pub observed_underflows: u64,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) enum TimingMetric {
    CaptureGap,
    WriteGap,
    Dispatch,
    Transfer,
    Wait,
    IterationGap,
}

#[derive(Default)]
struct State {
    active: AtomicBool,
    generation: AtomicU64,
    health: AtomicU8,
    processed: AtomicU64,
    peak_pending: AtomicU64,
    latency: AtomicU64,
    latency_valid: AtomicBool,
    dropped: AtomicU64,
    capture_holes: AtomicU64,
    playback_underflows: AtomicU64,
    wakes: AtomicU64,
    resets: AtomicU64,
    fft_frames: AtomicU64,
    fft_micros: AtomicU64,
    fft_peak_micros: AtomicU64,
    recoveries: AtomicU64,
    timing: TimingCounters,
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    wake: Mutex<Option<Wake>>,
}

#[derive(Clone, Default)]
pub struct AudioControl(Arc<State>);

#[derive(Debug, Clone, Copy)]
pub struct AudioStats {
    pub timing: AudioTimingStats,
    pub processed_frames: u64,
    pub peak_pending_frames: u64,
    /// PulseAudio's combined stream buffering estimate, not an acoustic measurement.
    pub estimated_latency_micros: u64,
    /// False until both native stream latency queries succeeded. The last
    /// estimate must not be treated as a current measurement when false.
    pub latency_valid: bool,
    /// Known local pending/paused drops; excludes PulseAudio's flushed buffers.
    pub dropped_frames: u64,
    /// Active monitor-hole frames zero-filled by the bridge; may be intentional silence.
    pub capture_hole_frames: u64,
    /// Native output-stream underflow notifications, including startup transitions.
    pub playback_underflows: u64,
    pub worker_wakes: u64,
    pub resets: u64,
    pub fft_frames: u64,
    pub fft_compute_micros: u64,
    pub peak_fft_compute_micros: u64,
    /// Owned output reloads plus bridge reopen attempts, including failures.
    pub recovery_attempts: u64,
    /// 0 starting, 1 ready, 2 unavailable/recovering, 3 stopped.
    pub health: u8,
}

impl AudioControl {
    pub fn set_active(&self, active: bool) {
        if self.0.active.swap(active, Ordering::AcqRel) != active {
            self.reset();
        }
    }
    pub fn reset(&self) {
        self.0.generation.fetch_add(1, Ordering::AcqRel);
        self.wake();
    }
    pub fn is_ready(&self) -> bool {
        self.0.health.load(Ordering::Acquire) == 1
    }
    pub fn stats(&self) -> AudioStats {
        AudioStats {
            timing: AudioTimingStats {
                max_capture_gap_micros: self.0.timing.capture_gap.load(Ordering::Relaxed),
                max_write_gap_micros: self.0.timing.write_gap.load(Ordering::Relaxed),
                max_dispatch_micros: self.0.timing.dispatch.load(Ordering::Relaxed),
                max_transfer_micros: self.0.timing.transfer.load(Ordering::Relaxed),
                max_wait_micros: self.0.timing.wait.load(Ordering::Relaxed),
                max_iteration_gap_micros: self.0.timing.iteration_gap.load(Ordering::Relaxed),
                last_underflow_capture_gap_micros: self
                    .0
                    .timing
                    .underflow_capture_gap
                    .load(Ordering::Relaxed),
                last_underflow_pending_frames: self
                    .0
                    .timing
                    .underflow_pending
                    .load(Ordering::Relaxed),
                last_underflow_write_credit_frames: self
                    .0
                    .timing
                    .underflow_credit
                    .load(Ordering::Relaxed),
                observed_underflows: self.0.timing.underflow_count.load(Ordering::Relaxed),
            },
            processed_frames: self.0.processed.load(Ordering::Relaxed),
            peak_pending_frames: self.0.peak_pending.load(Ordering::Relaxed),
            estimated_latency_micros: self.0.latency.load(Ordering::Relaxed),
            latency_valid: self.0.latency_valid.load(Ordering::Relaxed),
            dropped_frames: self.0.dropped.load(Ordering::Relaxed),
            capture_hole_frames: self.0.capture_holes.load(Ordering::Relaxed),
            playback_underflows: self.0.playback_underflows.load(Ordering::Relaxed),
            worker_wakes: self.0.wakes.load(Ordering::Relaxed),
            resets: self.0.resets.load(Ordering::Relaxed),
            fft_frames: self.0.fft_frames.load(Ordering::Relaxed),
            fft_compute_micros: self.0.fft_micros.load(Ordering::Relaxed),
            peak_fft_compute_micros: self.0.fft_peak_micros.load(Ordering::Relaxed),
            recovery_attempts: self.0.recoveries.load(Ordering::Relaxed),
            health: self.0.health.load(Ordering::Acquire),
        }
    }
    pub(crate) fn wake(&self) {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        if let Some(wake) = &*self
            .0
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            // SAFETY: removal holds this same lock before the mainloop is freed.
            unsafe { (wake.wake)(wake.handle as *mut std::ffi::c_void) };
        }
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn register_wake(&self, wake: Option<Wake>) {
        *self
            .0
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = wake;
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn dropped(&self, frames: usize) {
        self.0.dropped.fetch_add(frames as u64, Ordering::Relaxed);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn capture_hole(&self, frames: usize) {
        self.0
            .capture_holes
            .fetch_add(frames as u64, Ordering::Relaxed);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn note_timing(&self, metric: TimingMetric, micros: u64) {
        let counter = match metric {
            TimingMetric::CaptureGap => &self.0.timing.capture_gap,
            TimingMetric::WriteGap => &self.0.timing.write_gap,
            TimingMetric::Dispatch => &self.0.timing.dispatch,
            TimingMetric::Transfer => &self.0.timing.transfer,
            TimingMetric::Wait => &self.0.timing.wait,
            TimingMetric::IterationGap => &self.0.timing.iteration_gap,
        };
        counter.fetch_max(micros, Ordering::Relaxed);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn note_underflow(&self, count: u64, gap: u64, pending: usize, credit: usize) {
        self.0
            .timing
            .underflow_capture_gap
            .store(gap, Ordering::Relaxed);
        self.0
            .timing
            .underflow_pending
            .store(pending as u64, Ordering::Relaxed);
        self.0
            .timing
            .underflow_credit
            .store(credit as u64, Ordering::Relaxed);
        self.0
            .timing
            .underflow_count
            .store(count, Ordering::Relaxed);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn underflow_count(&self) -> u64 {
        self.0.playback_underflows.load(Ordering::Relaxed)
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn underflow_userdata(&self) -> *mut std::ffi::c_void {
        Arc::as_ptr(&self.0).cast_mut().cast()
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn woke(&self) {
        self.0.wakes.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn did_reset(&self) {
        self.0.resets.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn recovering(&self) {
        self.health(2);
        self.0.recoveries.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn fft(&self, frames: u64, micros: u64, peak_micros: u64) {
        self.0.fft_frames.store(frames, Ordering::Relaxed);
        self.0.fft_micros.store(micros, Ordering::Relaxed);
        self.0.fft_peak_micros.store(peak_micros, Ordering::Relaxed);
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
        if health != 1 {
            self.0.latency_valid.store(false, Ordering::Relaxed);
        }
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn latency(&self, micros: Option<u64>) {
        if let Some(micros) = micros {
            self.0.latency.store(micros, Ordering::Relaxed);
        }
        self.0
            .latency_valid
            .store(micros.is_some(), Ordering::Relaxed);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn record(&self, frames: usize, pending: usize) {
        self.0.processed.fetch_add(frames as u64, Ordering::Relaxed);
        self.0
            .peak_pending
            .fetch_max(pending as u64, Ordering::Relaxed);
    }
}

/// The registering connection must retain an AudioControl until callback removal.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) unsafe extern "C" fn playback_underflow(
    _stream: *mut std::ffi::c_void,
    userdata: *mut std::ffi::c_void,
) {
    if !userdata.is_null() {
        // SAFETY: the connection retains the Arc and unregisters before releasing it.
        // No allocation, locks, formatting, samples or panicking operations cross the ABI.
        unsafe { &*userdata.cast::<State>() }
            .playback_underflows
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn native_gap_counters_are_shared_and_do_not_change_playback() {
        let original = AudioControl::default();
        let retained = original.clone();
        let userdata = original.underflow_userdata();
        drop(original);
        retained.capture_hole(128);
        retained.capture_hole(64);
        // SAFETY: retained owns the callback's stable Arc allocation throughout.
        unsafe {
            playback_underflow(std::ptr::null_mut(), userdata);
            playback_underflow(std::ptr::null_mut(), userdata);
            playback_underflow(std::ptr::null_mut(), std::ptr::null_mut());
        }
        let stats = retained.stats();
        assert_eq!(stats.capture_hole_frames, 192);
        assert_eq!(stats.playback_underflows, 2);
        assert_eq!(stats.dropped_frames, 0);
        assert_eq!(stats.processed_frames, 0);
        assert_eq!(stats.resets, 0);
        assert!(!retained.active());
        assert_eq!(retained.generation(), 0);
    }
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
