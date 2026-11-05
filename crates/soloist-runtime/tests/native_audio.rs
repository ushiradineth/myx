//! Owner-run MAC4 scenarios. No SDK, credential, external executor or receipt.
//! `cargo test -p soloist-runtime --test native_audio -- --ignored native_audio::`
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub(crate) fn owner_tools() -> std::path::PathBuf {
    assert_eq!(
        std::env::var("MYX_NATIVE_AUDIO_ALLOW").as_deref(),
        Ok("1"),
        "explicit owner allowance MYX_NATIVE_AUDIO_ALLOW=1 required"
    );
    let fixture = std::path::PathBuf::from(
        std::env::var_os("MYX_NATIVE_AUDIO_FIXTURE_DIR")
            .expect("MYX_NATIVE_AUDIO_FIXTURE_DIR required"),
    );
    assert!(
        fixture.is_absolute() && fixture.is_dir(),
        "owner fixture directory missing"
    );
    let tools = std::path::PathBuf::from(
        std::env::var_os("MYX_AUDIO_TOOLS").expect("MYX_AUDIO_TOOLS required"),
    );
    assert!(tools.is_absolute(), "MYX_AUDIO_TOOLS must be absolute");
    for asset in ["bin/pulseaudio", "bin/pactl", "lib/libpulse.0.dylib"] {
        assert!(
            tools.join(asset).is_file(),
            "required native asset missing: {asset}"
        );
    }
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        panic!("native scenarios require macOS ARM64");
    }
    tools
}

// The named target compiles the actual runtime adapter as its crate root. Myx
// depends on the runtime library, so this thin trait adapter bridges the two
// crate identities without copying either DSP or transport implementation.
pub(crate) struct RootProcessor(pub Box<dyn myx::audio::AudioProcessor>);
impl crate::pcm::AudioProcessor for RootProcessor {
    fn process(&mut self, samples: &mut [f32]) {
        self.0.process(samples);
    }
    fn reset(&mut self) {
        self.0.reset();
    }
    fn fft_metrics(&self) -> (u64, u64, u64) {
        self.0.fft_metrics()
    }
}

fn processor(
    settings: myx::audio::EqualizerSettings,
    enabled: bool,
) -> (
    RootProcessor,
    Arc<Mutex<myx::audio::VisBands>>,
    Arc<AtomicBool>,
) {
    let bands = myx::audio::VisBands::shared();
    let demand = Arc::new(AtomicBool::new(enabled));
    (
        RootProcessor(myx::audio::managed_processor(
            Arc::new(Mutex::new(settings)),
            bands.clone(),
            demand.clone(),
        )),
        bands,
        demand,
    )
}

#[test]
#[ignore = "owner fixture required; real private PulseAudio, no SDK"]
fn negotiated_float44100_stereo_transport_50ms_pause_resume_seek_queues_shutdown_cleanup() {
    owner_tools();
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let (processor, _, _) = processor(myx::audio::EqualizerSettings::default(), false);
        crate::pulse::tests::run_route(Box::new(processor), 0.25, false);
    }
}

#[test]
#[ignore = "owner fixture required; actual production nonflat EQ through managed native PCM, no SDK"]
fn native_actual_eq_headroom_gain_and_stereo_transport() {
    owner_tools();
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let mut settings = myx::audio::EqualizerSettings::default();
        settings.gains_db[5] = 6;
        // A settled DC input has unity biquad response; automatic headroom is
        // still applied by the actual production filter bank before transport.
        let expected = 0.25 * 10.0f64.powf(settings.auto_preamp_db() / 20.0) as f32;
        let (processor, _, _) = processor(settings, true);
        crate::pulse::tests::run_route(Box::new(processor), expected, false);
    }
}

#[test]
#[ignore = "owner fixture required; real owned null sink unload/retarget, no SDK"]
fn owned_output_loss_retarget_keeps_processor_worker_and_private_server() {
    owner_tools();
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let (processor, _, _) = processor(myx::audio::EqualizerSettings::default(), false);
        crate::pulse::tests::run_route(Box::new(processor), 0.25, true);
    }
}

#[test]
#[ignore = "owner fixture required; physical CoreAudio output module reload, silence only; actual hardware hotplug needs_validation"]
fn owned_coreaudio_module_loss_reloads_without_replacing_private_server() {
    owner_tools();
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    crate::audio::tests::owned_output_module_loss_recovers();
}

#[test]
#[ignore = "owner fixture required; cancelled native startup must not connect"]
fn startup_cancel_before_native_connection() {
    owner_tools();
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    crate::pulse::tests::cancelled_startup();
}

#[test]
#[ignore = "owner fixture required; actual production DSP, no SDK"]
fn controlled_gain_frequency_headroom_bypass_stereo_actual_post_eq_fft() {
    owner_tools();
    use crate::pcm::AudioProcessor;
    let mut cut = myx::audio::EqualizerSettings::default();
    cut.gains_db[5] = -12;
    let (mut filtered, bands, demand) = processor(cut, true);
    let mut bypass = cut;
    bypass.enabled = false;
    let (mut flat, _, _) = processor(bypass, false);
    let mut filtered_energy = 0.0f64;
    let mut flat_energy = 0.0f64;
    let mut input = [0.0f32; crate::pcm::BLOCK_SAMPLES];
    for block in 0..128 {
        for (i, frame) in input.chunks_exact_mut(2).enumerate() {
            frame[0] =
                (std::f32::consts::TAU * 1000.0 * (block * 128 + i) as f32 / 44100.0).sin() * 0.5;
            frame[1] = 0.0;
        }
        let mut dry = input;
        let mut wet = input;
        flat.process(&mut dry);
        filtered.process(&mut wet);
        assert_eq!(dry, input, "bypass must be sample exact");
        assert!(
            wet.chunks_exact(2).all(|frame| frame[1] == 0.0),
            "channel leakage"
        );
        if block > 16 {
            filtered_energy += wet
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>();
            flat_energy += dry
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>();
        }
    }
    let gain_db = 10.0 * (filtered_energy / flat_energy).log10();
    assert!(
        (-12.5..-11.5).contains(&gain_db),
        "1kHz cut response {gain_db}dB"
    );
    assert!(bands.lock().unwrap().is_active);
    assert!(filtered.fft_metrics().0 > 0);
    // Compare post-EQ spectra with the same input and actual analyzer, not a
    // synthetic spectrum receipt. The -12dB center cut must reduce its peak.
    let (mut reference, reference_bands, _) = processor(bypass, true);
    for _ in 0..32 {
        let mut samples = input;
        reference.process(&mut samples);
    }
    let wet_peak = bands
        .lock()
        .unwrap()
        .values
        .iter()
        .copied()
        .fold(0.0f32, f32::max);
    let dry_peak = reference_bands
        .lock()
        .unwrap()
        .values
        .iter()
        .copied()
        .fold(0.0f32, f32::max);
    assert!(
        wet_peak < dry_peak * 0.4,
        "FFT must observe post-EQ attenuation"
    );
    demand.store(false, Ordering::Release);
    let before = filtered.fft_metrics();
    filtered.process(&mut input);
    assert_eq!(
        before,
        filtered.fft_metrics(),
        "detached FFT performed work"
    );
    filtered.reset();
    assert!(!bands.lock().unwrap().is_active);
    let mut boost = cut;
    boost.gains_db.fill(12);
    let (mut boosted, _, _) = processor(boost, false);
    for _ in 0..32 {
        let mut samples = [0.95; crate::pcm::BLOCK_SAMPLES];
        boosted.process(&mut samples);
        assert!(
            samples
                .iter()
                .all(|sample| sample.is_finite() && sample.abs() <= 1.0),
            "headroom exceeded"
        );
    }
}

#[test]
#[ignore = "owner fixture required; physical output, silence only; hotplug remains needs_validation"]
fn current_owned_output_silence_and_cleanup() {
    owner_tools();
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    crate::audio::tests::managed_pcm_opens_current_output_and_cleans_up_without_an_sdk();
}

#[test]
#[ignore = "owner fixture required; deterministic control policy, no native calls"]
fn deterministic_pause_resume_seek_gate() {
    owner_tools();
    let control = crate::pcm::AudioControl::default();
    assert_eq!(control.stats().health, 0);
    assert_eq!(control.stats().processed_frames, 0);
    control.set_active(true);
    control.set_active(true);
    control.reset();
    control.set_active(false);
    assert!(!control.is_ready());
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        assert_eq!(control.generation(), 3);
        assert!(!control.active());
    }
}

#[test]
#[ignore = "owner fixture required; deterministic retry policy, no native calls"]
fn deterministic_output_recovery_backoff_and_exhaustion() {
    owner_tools();
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    crate::audio::tests::recovery_is_bounded_and_notification_rearms();
}
