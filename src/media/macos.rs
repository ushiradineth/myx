use crate::domain::{playback::now_ms, PlaybackStatus, PositionAnchor, Snapshot, TrackMetadata};
use anyhow::{Context, Result};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use std::time::Duration;

pub struct MediaOwner {
    controls: MediaControls,
    metadata: Option<Option<TrackMetadata>>,
    playback: Option<(PlaybackStatus, PositionAnchor)>,
    last_failure: Option<std::time::Instant>,
}
impl MediaOwner {
    pub fn new(events: flume::Sender<MediaControlEvent>) -> Result<Self> {
        let mut controls = MediaControls::new(PlatformConfig {
            dbus_name: "myx",
            display_name: "myx",
            hwnd: None,
        })
        .map_err(|_| anyhow::anyhow!("Native media controls require the Mac GUI user session"))?;
        controls
            .attach(Box::new(move |event| {
                let _ = events.try_send(event);
            }))
            .map_err(|_| anyhow::anyhow!("Cannot attach native media controls"))?;
        Ok(Self {
            controls,
            metadata: None,
            playback: None,
            last_failure: None,
        })
    }
    pub fn update(&mut self, snapshot: &Snapshot) -> bool {
        if self
            .last_failure
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return false;
        }
        let track = snapshot.track.as_ref();
        if self.metadata.as_ref() != Some(&snapshot.track) {
            let artists = track
                .and_then(|track| track.artists.as_ref())
                .map(|artists| artists.join(", "));
            if self
                .controls
                .set_metadata(MediaMetadata {
                    title: track.and_then(|track| track.title.as_deref()),
                    artist: artists.as_deref(),
                    album: track.and_then(|track| track.album.as_deref()),
                    cover_url: track.and_then(|track| track.artwork_url.as_deref()),
                    duration: track
                        .and_then(|track| track.duration_ms)
                        .map(Duration::from_millis),
                })
                .is_err()
            {
                self.last_failure = Some(std::time::Instant::now());
                return false;
            }
            self.metadata = Some(snapshot.track.clone());
        }
        if self.playback == Some((snapshot.playback, snapshot.position)) {
            self.last_failure = None;
            return true;
        }
        let progress = Some(MediaPosition(Duration::from_millis(
            snapshot
                .position
                .at(now_ms(), track.and_then(|track| track.duration_ms)),
        )));
        let playback = match snapshot.playback {
            PlaybackStatus::Playing => MediaPlayback::Playing { progress },
            PlaybackStatus::Paused | PlaybackStatus::Buffering | PlaybackStatus::Disconnected => {
                MediaPlayback::Paused { progress }
            }
            _ => MediaPlayback::Stopped,
        };
        if self.controls.set_playback(playback).is_err() {
            self.last_failure = Some(std::time::Instant::now());
            return false;
        }
        self.playback = Some((snapshot.playback, snapshot.position));
        self.last_failure = None;
        true
    }
}

pub fn run(
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    owner: impl FnOnce() -> Result<()> + Send + 'static,
) -> Result<()> {
    use winit::application::ApplicationHandler;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    struct Finished;
    struct Pump;
    impl ApplicationHandler<Finished> for Pump {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
        fn window_event(
            &mut self,
            _: &ActiveEventLoop,
            _: winit::window::WindowId,
            _: winit::event::WindowEvent,
        ) {
        }
        fn user_event(&mut self, event_loop: &ActiveEventLoop, _: Finished) {
            event_loop.exit();
        }
    }
    let event_loop = EventLoop::<Finished>::with_user_event()
        .with_activation_policy(ActivationPolicy::Accessory)
        .with_activate_ignoring_other_apps(false)
        .build()
        .context("Create the daemon's Mac GUI event loop")?;
    let proxy = event_loop.create_proxy();
    let worker = std::thread::Builder::new()
        .name("myx-owner".into())
        .spawn(move || {
            let result = owner();
            let _ = proxy.send_event(Finished);
            result
        })?;
    let result = event_loop
        .run_app(&mut Pump)
        .context("Run Mac media event loop");
    if result.is_err() {
        shutdown.store(true, std::sync::atomic::Ordering::Release);
    }
    let owned_result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("Daemon owner thread panicked"))?;
    result?;
    owned_result
}
