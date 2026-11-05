//! Managed Soloist transport. Only the worker touches the runtime or the socket.
//! Protocol: https://developer.spotify.com/documentation/soloist/reference/websocket-api

use std::collections::VecDeque;
use std::io::{self, Read};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Result};
use serde_json::{json, Value};
use tungstenite::{protocol::WebSocketConfig, Message, WebSocket};

use super::protocol::{ObservedMetadata as TrackMetadata, PlayerEvent as EngineEvent};
use crate::audio::VisBands;
use crate::config::SoloistConfig;
use crate::domain::{PositionAnchor, QueueEntry, QueueSnapshot, QueueSource, RepeatMode};

const WAITING: u8 = 0;
const READY: u8 = 1;
const DISCONNECTED: u8 = 2;
const STOPPED: u8 = 3;
const MAILBOX_CAPACITY: usize = 64;
const POLL: Duration = Duration::from_secs(5);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const SEEK_TIMEOUT: Duration = Duration::from_secs(15);

pub struct RuntimePlayer {
    pub bands: Arc<Mutex<VisBands>>,
    spectrum_enabled: Arc<AtomicBool>,
    device_id: Arc<Mutex<Option<String>>>,
    equalizer: crate::audio::equalizer::EqualizerControl,
    audio: soloist_runtime::pcm::AudioControl,
    playback_gate: PlaybackGate,
    commands: flume::Sender<Command>,
    stopping: Arc<AtomicBool>,
    stop_pending: Arc<AtomicBool>,
    phase: Arc<AtomicU8>,
    worker: Mutex<Option<JoinHandle<Result<()>>>>,
}

#[derive(Debug)]
enum Command {
    Context(String, u32, bool),
    Play,
    Pause,
    Stop,
    Toggle,
    Next,
    Prev,
    Shuffle(bool),
    Repeat(RepeatMode),
    Volume(u8),
    Seek(u32),
    Add(String),
    Queue,
    State,
}

/// Bounded mailbox coalescing with no retained work across authentication loss.
struct Intents;

impl Intents {
    fn drain(
        &self,
        rx: &flume::Receiver<Command>,
        authenticated: bool,
        stop_pending: &AtomicBool,
    ) -> VecDeque<Command> {
        let mut batch = VecDeque::new();
        for cmd in rx.try_iter().take(MAILBOX_CAPACITY) {
            match cmd {
                Command::Stop => {
                    batch.clear();
                    batch.push_back(Command::Stop);
                }
                cmd @ Command::Context(..) if authenticated => {
                    // A newer selection replaces older selections, not volume
                    // or repeat preferences. Stop remains an ordering barrier.
                    batch.retain(|queued| {
                        matches!(
                            queued,
                            Command::Stop | Command::Volume(_) | Command::Repeat(_)
                        )
                    });
                    batch.push_back(cmd);
                }
                cmd if authenticated => {
                    let replace_last = matches!(
                        (batch.back(), &cmd),
                        (Some(Command::Volume(_)), Command::Volume(_))
                            | (Some(Command::Seek(_)), Command::Seek(_))
                            | (Some(Command::Shuffle(_)), Command::Shuffle(_))
                            | (Some(Command::Repeat(_)), Command::Repeat(_))
                            | (Some(Command::Play), Command::Play)
                            | (Some(Command::Pause), Command::Pause)
                            | (Some(Command::Queue), Command::Queue)
                            | (Some(Command::State), Command::State)
                    );
                    if replace_last {
                        batch.pop_back();
                    }
                    if matches!(
                        (batch.back(), &cmd),
                        (Some(Command::Toggle), Command::Toggle)
                    ) {
                        batch.pop_back();
                    } else {
                        batch.push_back(cmd);
                    }
                }
                // Loads and controls accepted just before authentication was
                // lost must never become delayed work on a later session.
                _ => {}
            }
        }
        if stop_pending.swap(false, Ordering::AcqRel) {
            batch.clear();
            batch.push_back(Command::Stop);
        }
        batch
    }
}

/// Serializes owner verification and SDK audibility before publishing to PCM.
/// Neither observation alone authorizes audible output.
#[derive(Clone)]
struct PlaybackGate {
    control: soloist_runtime::pcm::AudioControl,
    state: Arc<Mutex<PlaybackGateState>>,
}

#[derive(Default)]
struct PlaybackGateState {
    verified: bool,
    audible: bool,
    #[cfg(test)]
    applied_active: bool,
}

impl PlaybackGate {
    fn new(control: soloist_runtime::pcm::AudioControl) -> Self {
        control.set_active(false);
        Self {
            control,
            state: Arc::new(Mutex::new(PlaybackGateState::default())),
        }
    }

    fn set_active(&self, active: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.audible = active;
        self.apply(&mut state);
    }

    fn set_verified(&self, verified: bool, stopping: &AtomicBool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Check cancellation under the publication lock so an owner update
        // racing shutdown cannot restore verification after revocation.
        state.verified = verified && !stopping.load(Ordering::Acquire);
        self.apply(&mut state);
    }

    fn apply(&self, state: &mut PlaybackGateState) {
        let active = state.verified && state.audible;
        self.control.set_active(active);
        #[cfg(test)]
        {
            // Observe the exact activation value published to the private PCM
            // control without exposing its internals or starting native audio.
            state.applied_active = active;
        }
    }

    fn reset(&self) {
        self.control.reset();
    }
}

struct WorkerControl<'a> {
    stop: &'a AtomicBool,
    stop_pending: &'a AtomicBool,
    phase: &'a AtomicU8,
    audio: &'a PlaybackGate,
    device_id: &'a Mutex<Option<String>>,
}

// Cancelling asynchronous startup must still stop a worker in Runtime::start.
struct StartupGuard {
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
}
impl Drop for StartupGuard {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.stopping.store(true, Ordering::Release);
            let _ = worker.join();
        }
    }
}

impl RuntimePlayer {
    pub async fn start(
        config: SoloistConfig,
        tx: flume::Sender<super::protocol::PlayerEvent>,
        volume: u8,
    ) -> Result<Self> {
        let bands = VisBands::shared();
        let spectrum_enabled = Arc::new(AtomicBool::new(false));
        let device_id = Arc::new(Mutex::new(None));
        let worker_device_id = device_id.clone();
        let equalizer = crate::audio::equalizer::shared_equalizer();
        let audio = soloist_runtime::pcm::AudioControl::default();
        let playback_gate = PlaybackGate::new(audio.clone());
        let worker_audio = playback_gate.clone();
        let processor = crate::audio::managed_processor(
            equalizer.clone(),
            bands.clone(),
            spectrum_enabled.clone(),
        );
        let (commands, rx) = flume::bounded(MAILBOX_CAPACITY);
        let (started, ready) = tokio::sync::oneshot::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let phase = Arc::new(AtomicU8::new(WAITING));
        let stop_pending = Arc::new(AtomicBool::new(false));
        let worker_stop_pending = stop_pending.clone();
        let worker_stop = stopping.clone();
        let worker_phase = phase.clone();
        let worker = thread::Builder::new()
            .name("myx-soloist".into())
            .spawn(move || {
                let mut started = Some(started);
                let result = run(
                    config,
                    volume.min(100),
                    rx,
                    &tx,
                    &WorkerControl {
                        stop: &worker_stop,
                        stop_pending: &worker_stop_pending,
                        phase: &worker_phase,
                        audio: &worker_audio,
                        device_id: &worker_device_id,
                    },
                    &mut started,
                    processor,
                );
                worker_audio.set_active(false);
                *worker_device_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                emit(
                    &tx,
                    EngineEvent::Authentication {
                        logged_in: false,
                        is_active: false,
                    },
                );
                worker_phase.store(STOPPED, Ordering::Release);
                if let Some(started) = started.take() {
                    let _ = started.send(Err(anyhow!(
                        "Soloist startup failed; check provisioning and the local endpoint"
                    )));
                }
                if let Err(error) = &result {
                    status(&tx, soloist_runtime::startup_diagnostic(error));
                }
                emit(&tx, EngineEvent::LocallyInactive);
                result
            })
            .map_err(|_| anyhow!("Soloist worker could not start"))?;
        let mut guard = StartupGuard {
            stopping: stopping.clone(),
            worker: Some(worker),
        };
        if !matches!(ready.await, Ok(Ok(()))) {
            stopping.store(true, Ordering::Release);
            let worker = guard.worker.take().expect("startup worker owned");
            let _ = tokio::task::spawn_blocking(move || worker.join()).await;
            bail!("Soloist startup failed; check provisioning and the local endpoint");
        }
        let worker = guard.worker.take().expect("startup worker owned");
        Ok(Self {
            bands,
            spectrum_enabled,
            device_id,
            equalizer,
            audio,
            playback_gate,
            commands,
            stopping,
            stop_pending,
            phase,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn has_local_dsp(&self) -> bool {
        self.audio.is_ready()
    }
    /// Authorize PCM only after the owner verifies this SDK device and service.
    /// SDK availability and playback/auth observations remain independent.
    pub fn set_verified(&self, verified: bool) {
        self.playback_gate.set_verified(verified, &self.stopping);
    }
    /// The owner enables telemetry only while a spectrum subscriber is attached.
    pub fn set_spectrum_enabled(&self, enabled: bool) {
        self.spectrum_enabled.store(enabled, Ordering::Release);
        if !enabled {
            let mut bands = self
                .bands
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *bands = VisBands::new();
        }
    }
    pub fn audio_stats(&self) -> Option<soloist_runtime::pcm::AudioStats> {
        Some(self.audio.stats())
    }
    pub fn observed_playing(&self) -> bool {
        self.playback_gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .audible
    }
    pub fn flush_audio(&self) {
        self.audio.reset();
    }
    pub fn set_equalizer(&self, settings: crate::audio::EqualizerSettings) {
        *self
            .equalizer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings.normalized();
    }

    fn submit(&self, command: Command) -> Result<()> {
        ensure!(!self.stopping.load(Ordering::Acquire), "Soloist is stopped");
        let phase = self.phase.load(Ordering::Acquire);
        ensure!(phase != STOPPED, "Soloist worker is stopped");
        ensure!(
            phase != DISCONNECTED || matches!(&command, Command::Stop),
            "Soloist is reconnecting; retry the command once connected"
        );
        ensure!(
            phase == READY || matches!(&command, Command::Stop),
            "Soloist is waiting for authentication; only Stop can be queued"
        );
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                flume::TrySendError::Full(_) => {
                    anyhow!("Soloist command mailbox is full; retry shortly")
                }
                flume::TrySendError::Disconnected(_) => anyhow!("Soloist worker is stopped"),
            })
    }

    pub fn play_context(&self, uri: String, shuffle: bool) -> Result<()> {
        self.play_context_at(uri, None, 0, shuffle)
    }

    pub fn play_context_at(
        &self,
        uri: String,
        track: Option<String>,
        position: u32,
        shuffle: bool,
    ) -> Result<()> {
        validate_uri(&uri, false)?;
        ensure!(
            track.is_none(),
            "Soloist selected-context playback must be resolved through the owned-device Web API"
        );
        ensure!(
            position == 0 || is_item(&uri),
            "Soloist positioned context playback requires a track URI"
        );
        self.submit(Command::Context(uri, position, shuffle))
    }

    pub fn play_track_at(&self, uri: String, position: u32) -> Result<()> {
        validate_uri(&uri, false)?;
        ensure!(
            is_item(&uri),
            "Soloist direct playback requires a track or episode URI"
        );
        self.play_context_at(uri, None, position, false)
    }
    pub fn play(&self) -> Result<()> {
        self.submit(Command::Play)
    }
    pub fn pause(&self) -> Result<()> {
        self.submit(Command::Pause)
    }
    pub fn toggle(&self) -> Result<()> {
        self.submit(Command::Toggle)
    }
    pub fn next(&self) -> Result<()> {
        self.submit(Command::Next)
    }
    pub fn prev(&self) -> Result<()> {
        self.submit(Command::Prev)
    }
    pub fn shuffle(&self, on: bool) -> Result<()> {
        self.submit(Command::Shuffle(on))
    }
    pub fn set_repeat(&self, mode: RepeatMode) -> Result<()> {
        self.submit(Command::Repeat(mode))
    }
    pub fn set_volume(&self, volume: u16) -> Result<()> {
        self.submit(Command::Volume(volume_percent(volume)))
    }
    pub fn seek(&self, position: u32) -> Result<()> {
        self.submit(Command::Seek(position))
    }
    pub fn add_to_queue(&self, uri: String) -> Result<()> {
        validate_uri(&uri, true)?;
        self.submit(Command::Add(uri))
    }
    pub fn request_queue(&self) -> Result<()> {
        self.submit(Command::Queue)
    }
    pub fn request_state(&self) -> Result<()> {
        self.submit(Command::State)
    }

    /// Validated public discovery identity of this owned SDK instance.
    pub fn device_id(&self) -> Result<String> {
        ensure!(!self.stopping.load(Ordering::Acquire), "Soloist is stopped");
        self.device_id
            .lock()
            .map_err(|_| anyhow!("Soloist identity lock failed"))?
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("Soloist owned SDK device identity is unavailable"))
    }

    /// The SDK has pause, not a destructive stop command. Keep the backend
    /// available after media Stop so the next play request can still work.
    pub fn stop(&self) {
        if self.submit(Command::Stop).is_err() {
            // Stop must not silently disappear behind a full mailbox of loads.
            // This fallback is consumed by the same worker, never by the UI.
            self.stop_pending.store(true, Ordering::Release);
        }
    }

    /// Blocking ownership barrier. Call off the UI thread during app shutdown.
    pub fn shutdown(&self) -> Result<()> {
        self.stopping.store(true, Ordering::Release);
        self.set_verified(false);
        self.playback_gate.set_active(false);
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| anyhow!("Soloist shutdown lock failed"))?;
        if let Some(worker) = worker.take() {
            worker
                .join()
                .map_err(|_| anyhow!("Soloist worker panicked during shutdown"))??;
        }
        Ok(())
    }
}

impl Drop for RuntimePlayer {
    fn drop(&mut self) {
        // The worker observes cancellation and drops its owned runtime even if
        // the caller cannot perform the explicit blocking shutdown barrier.
        self.stopping.store(true, Ordering::Release);
        self.set_verified(false);
        self.playback_gate.set_active(false);
        if let Ok(worker) = self.worker.get_mut() {
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        }
    }
}

fn validate_uri(uri: &str, track_only: bool) -> Result<()> {
    let mut parts = uri.split(':');
    let namespace = parts.next();
    let kind = parts.next();
    let id = parts.next().unwrap_or("");
    ensure!(
        namespace == Some("spotify")
            && (if track_only {
                kind == Some("track")
            } else {
                matches!(
                    kind,
                    Some("track" | "album" | "playlist" | "artist" | "episode" | "show")
                )
            })
            && id.len() == 22
            && id.bytes().all(|b| b.is_ascii_alphanumeric())
            && parts.next().is_none(),
        "Soloist requires a supported Spotify URI with a 22-character alphanumeric ID"
    );
    Ok(())
}

fn is_item(uri: &str) -> bool {
    uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:")
}
fn volume_percent(volume: u16) -> u8 {
    ((u32::from(volume) * 100 + 32767) / 65535) as u8
}

fn endpoint_address(endpoint: &str) -> Result<SocketAddr> {
    // Parse authority ourselves to exclude DNS, userinfo, queries and implicit ports.
    let address = endpoint.strip_prefix("ws://").ok_or_else(|| {
        anyhow!("Soloist endpoint must use ws:// and a loopback IP with an explicit nonzero port")
    })?;
    let authority = address.strip_suffix('/').unwrap_or(address);
    ensure!(
        !authority.contains(['/', '?', '#', '@']),
        "Soloist endpoint must contain only a loopback IP and port"
    );
    let address: SocketAddr = authority.parse().map_err(|_| {
        anyhow!("Soloist endpoint requires a loopback IP and an explicit nonzero port")
    })?;
    ensure!(
        address.ip().is_loopback() && address.port() != 0,
        "Soloist endpoint requires a loopback IP and a nonzero port"
    );
    Ok(address)
}

fn runtime_config(config: &SoloistConfig, volume: u8) -> Result<soloist_runtime::Config> {
    config.validate()?;
    let bundle = crate::runtime_bundle::discover()?;
    let choose = |explicit: &Option<std::path::PathBuf>,
                  packaged: Option<&std::path::PathBuf>,
                  fallback: &str|
     -> Result<std::path::PathBuf> {
        if explicit.is_some() {
            config.resolve(explicit.as_ref(), fallback)
        } else if let Some(path) = packaged {
            Ok(path.clone())
        } else {
            config.resolve(None, fallback)
        }
    };
    let library_path = if let Some(path) = &config.library_path {
        path.clone()
    } else if let Some(bundle) = bundle
        .as_ref()
        .filter(|_| config.library_manifest.is_none())
    {
        bundle.library_path.clone()
    } else {
        let path = config.resolve(
            config.library_manifest.as_ref(),
            "assets/arm-libraries.json",
        )?;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|_| anyhow!("Soloist ARM library manifest is unavailable"))?
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow!("Soloist ARM library manifest could not be read"))?;
        ensure!(
            bytes.len() <= 65536,
            "Soloist ARM library manifest is too large"
        );
        parse_manifest(&bytes)?
    };
    ensure!(
        !library_path.is_empty()
            && library_path
                .split(':')
                .all(|p| p.starts_with('/') && !p.contains(['\0', '\n', '\r'])),
        "Soloist library_path must contain absolute nonempty paths"
    );
    let audio_tools = choose(
        &config.audio_tools,
        bundle.as_ref().map(|bundle| &bundle.audio_tools),
        "deps/audio",
    )?;
    let audio_modules = if config.audio_tools.is_none() {
        bundle.as_ref().map(|bundle| bundle.audio_modules.clone())
    } else {
        let modules = audio_tools.join("modules");
        modules.is_dir().then_some(modules)
    };
    let runtime = soloist_runtime::Config {
        sdk: config.resolve(config.sdk.as_ref(), "assets/soloist")?,
        runner: choose(
            &config.runner,
            bundle.as_ref().map(|bundle| &bundle.runner),
            "deps/elfuse/bin/elfuse",
        )?,
        sysroot: choose(
            &config.sysroot,
            bundle.as_ref().map(|bundle| &bundle.sysroot),
            "assets/root",
        )?,
        library_path,
        audio_tools,
        audio_modules,
        api_key: match &config.api_key {
            Some(key) => soloist_runtime::ApiKey::Inline(key.clone()),
            None => soloist_runtime::ApiKey::File(
                config.resolve(config.api_key_file.as_ref(), "api-key")?,
            ),
        },
        data_dir: config.resolve(config.data_dir.as_ref(), "data")?,
        cache_dir: config.resolve(config.cache_dir.as_ref(), "cache")?,
        device_name: config.device_name.clone(),
        initial_volume: volume,
        expected_sdk_sha256: config.expected_sdk_sha256.clone(),
    };
    if let Some(bundle) = bundle {
        for path in [&runtime.sdk, &runtime.data_dir, &runtime.cache_dir] {
            bundle.ensure_external_user_path(path)?;
        }
        if let soloist_runtime::ApiKey::File(path) = &runtime.api_key {
            bundle.ensure_external_user_path(path)?;
        }
    }
    Ok(runtime)
}

fn parse_manifest(bytes: &[u8]) -> Result<String> {
    let manifest: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow!("Soloist ARM library manifest is invalid"))?;
    ensure!(
        manifest["arch"] == "aarch64-linux",
        "Soloist library manifest must target aarch64-linux"
    );
    manifest["libraryPath"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Soloist library manifest has no libraryPath"))
}

struct Peer(WebSocket<TcpStream>);
impl Peer {
    fn connect(endpoint: &str) -> Result<Self> {
        let address = endpoint_address(endpoint)?;
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))
            .map_err(|_| anyhow!("Soloist local connection failed"))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(8 * 1024 * 1024))
            .max_frame_size(Some(8 * 1024 * 1024));
        let (socket, _) = tungstenite::client::client_with_config(endpoint, stream, Some(config))
            .map_err(|_| anyhow!("Soloist WebSocket handshake failed"))?;
        socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(200)))?;
        Ok(Self(socket))
    }

    fn send(&mut self, value: Value) -> Result<()> {
        self.0
            .send(Message::Text(value.to_string().into()))
            .map_err(|_| anyhow!("Soloist command transport failed"))
    }
    fn command(&mut self, name: &str) -> Result<()> {
        self.send(command(name))
    }
    fn active(&mut self, value: Value) -> Result<()> {
        self.command("activate")?;
        self.send(value)
    }
    fn receive(&mut self) -> Result<Option<Value>> {
        match self.0.read() {
            Ok(Message::Text(text)) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|_| anyhow!("Soloist sent an invalid JSON event")),
            Ok(Message::Close(_)) => bail!("Soloist control connection closed"),
            Ok(Message::Ping(_)) => {
                self.0
                    .flush()
                    .map_err(|_| anyhow!("Soloist heartbeat transport failed"))?;
                Ok(None)
            }
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(_) => bail!("Soloist control connection failed"),
        }
    }
}

fn command(name: &str) -> Value {
    json!({"type":"command", "command":name})
}
fn field_command(name: &str, field: &str, value: impl Into<Value>) -> Value {
    let mut value_command = command(name);
    value_command[field] = value.into();
    value_command
}
fn queue_query() -> Value {
    field_command("get_queue", "limit", 0)
}
fn emit(tx: &flume::Sender<EngineEvent>, event: EngineEvent) {
    // Never block the owner thread on a slow or shutting-down UI.
    let _ = tx.try_send(event);
}
fn status(tx: &flume::Sender<EngineEvent>, message: &'static str) {
    emit(
        tx,
        EngineEvent::BackendStatus {
            message: message.into(),
        },
    );
}

fn observe_authentication(
    value: &Value,
    logged_in: bool,
    is_active: bool,
    tx: &flume::Sender<EngineEvent>,
) {
    if matches!(
        value["type"].as_str(),
        Some(
            "auth_state"
                | "playback_state"
                | "track_changed"
                | "playback_changed"
                | "device_changed"
                | "context_changed"
                | "position_sync"
        )
    ) {
        emit(
            tx,
            EngineEvent::Authentication {
                logged_in,
                is_active: logged_in && is_active,
            },
        );
    }
}

#[derive(Default)]
struct Playback {
    active: bool,
    status: String,
    metadata: Option<TrackMetadata>,
    context: Option<String>,
    position: u32,
    published: Option<(String, String)>,
    published_metadata: Option<TrackMetadata>,
}

impl Playback {
    fn locally_inactive(&mut self, tx: &flume::Sender<EngineEvent>) {
        self.active = false;
        self.status.clear();
        self.published = None;
        self.published_metadata = None;
        emit(tx, EngineEvent::LocallyInactive);
    }

    fn reset_connection(&mut self) {
        self.active = false;
        self.status.clear();
        self.context = None;
        self.published = None;
        self.published_metadata = None;
    }

    fn update(&mut self, value: &Value, tx: &flume::Sender<EngineEvent>) -> bool {
        let kind = value["type"].as_str().unwrap_or("");
        if !matches!(
            kind,
            "playback_state"
                | "track_changed"
                | "playback_changed"
                | "device_changed"
                | "context_changed"
                | "position_sync"
                | "options_changed"
                | "volume_changed"
        ) {
            return false;
        }
        let was_active = self.active;
        if kind == "playback_state" {
            self.active = value["is_active"].as_bool().unwrap_or(false);
        } else if let Some(active) = value["is_active"].as_bool() {
            self.active = active;
        }
        if !self.active {
            self.locally_inactive(tx);
            return false;
        }
        let previous_uri = self.metadata.as_ref().map(|m| m.uri.clone());
        if value.get("item").is_some() {
            let next = metadata(&value["item"]);
            if previous_uri.as_deref() != next.as_ref().map(|m| m.uri.as_str()) {
                self.position = 0;
                self.status.clear();
            }
            self.metadata = next;
        }
        if let Some(context) = value.get("context") {
            self.context = context["uri"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
        }
        if let Some(state) = value["status"]
            .as_str()
            .filter(|s| matches!(*s, "idle" | "paused" | "playing" | "buffering"))
        {
            self.status = state.into();
        }
        let position = value["position"]["position_ms"]
            .as_u64()
            .map(|n| n.min(u64::from(u32::MAX)) as u32);
        if let Some(position) = position {
            self.position = position;
        }
        if !was_active && self.active && kind == "device_changed" {
            // A device role change alone does not confirm local playback of
            // the cached global track. Wait for a fresh playback notification.
            self.status.clear();
        }
        if self.status == "idle" {
            if self.published.take().is_some() || self.metadata.is_some() {
                emit(tx, EngineEvent::Stopped);
            }
            self.metadata = None;
            self.position = 0;
            self.published = None;
            self.published_metadata = None;
        } else if let Some(item) = &self.metadata {
            if self.published_metadata.as_ref() != Some(item) {
                if self.published_metadata.as_ref().map(|m| &m.uri) != Some(&item.uri) {
                    emit(
                        tx,
                        EngineEvent::TrackChanged {
                            uri: item.uri.clone(),
                        },
                    );
                }
                emit(tx, EngineEvent::Metadata(item.clone()));
                self.published_metadata = Some(item.clone());
            }
            let key = (item.uri.clone(), self.status.clone());
            if self.published.as_ref() != Some(&key) {
                let event = match self.status.as_str() {
                    "playing" => Some(EngineEvent::Playing {
                        uri: item.uri.clone(),
                        position_ms: self.position,
                    }),
                    "paused" => Some(EngineEvent::Paused {
                        uri: item.uri.clone(),
                        position_ms: self.position,
                    }),
                    "buffering" => Some(EngineEvent::Buffering {
                        uri: item.uri.clone(),
                        position_ms: self.position,
                    }),
                    _ => None,
                };
                if let Some(event) = event {
                    emit(tx, event);
                    self.published = Some(key);
                }
            }
            if let Some(anchor) = position_anchor(&value["position"]) {
                emit(
                    tx,
                    EngineEvent::PositionAnchor {
                        uri: item.uri.clone(),
                        anchor,
                    },
                );
            } else if position.is_some()
                && value["position"].get("timestamp_ms").is_none()
                && value["position"].get("speed").is_none()
            {
                emit(
                    tx,
                    EngineEvent::PositionCorrection {
                        uri: item.uri.clone(),
                        position_ms: self.position,
                    },
                );
            }
        }
        observe_details(value, tx);
        previous_uri.as_deref() != self.metadata.as_ref().map(|m| m.uri.as_str())
    }

    fn playing(&self, uri: &str) -> bool {
        self.active
            && self.status == "playing"
            && self.metadata.as_ref().is_some_and(|m| m.uri == uri)
    }
}

fn position_anchor(position: &Value) -> Option<PositionAnchor> {
    let speed = position["speed"].as_f64()?;
    if !speed.is_finite() || speed < 0.0 {
        return None;
    }
    Some(PositionAnchor {
        position_ms: position["position_ms"].as_u64()?,
        timestamp_ms: position["timestamp_ms"].as_u64()?,
        speed,
    })
}

fn observe_details(value: &Value, tx: &flume::Sender<EngineEvent>) {
    if let Some(percent) = value["volume"].as_u64().filter(|percent| *percent <= 100) {
        emit(
            tx,
            EngineEvent::Volume {
                percent: percent as u8,
            },
        );
    }
    let options = &value["options"];
    if let (Some(shuffle), Some(repeat)) =
        (options["shuffle"].as_bool(), options["repeat"].as_str())
    {
        let repeat = match repeat {
            "off" => Some(RepeatMode::Off),
            "track" => Some(RepeatMode::One),
            "context" => Some(RepeatMode::All),
            _ => None,
        };
        if let Some(repeat) = repeat {
            emit(tx, EngineEvent::Options { shuffle, repeat });
        }
    }
    if let Some(context) = value.get("context") {
        let uri = context["uri"]
            .as_str()
            .filter(|uri| validate_uri(uri, false).is_ok())
            .map(str::to_owned);
        emit(tx, EngineEvent::Context { uri });
    }
    if let Some(actions) = value["available_actions"].as_object() {
        const ALLOWED: [&str; 10] = [
            "pause",
            "play",
            "seek",
            "seek_forward",
            "seek_backward",
            "skip_next",
            "skip_prev",
            "add_to_queue",
            "shuffle",
            "set_repeat",
        ];
        let names = ALLOWED
            .into_iter()
            .filter(|name| actions.get(*name).is_some_and(Value::is_object))
            .map(str::to_owned)
            .collect();
        let step = |name: &str| {
            actions
                .get(name)
                .and_then(|value| value["step_ms"].as_u64())
                .filter(|step| *step > 0)
        };
        emit(
            tx,
            EngineEvent::AvailableActions {
                actions: names,
                seek_forward_step_ms: step("seek_forward"),
                seek_backward_step_ms: step("seek_backward"),
            },
        );
    }
}

fn metadata(item: &Value) -> Option<TrackMetadata> {
    let uri = item["uri"].as_str()?;
    if validate_uri(uri, false).is_err() || !is_item(uri) {
        return None;
    }
    let decorations = &item["decorations"];
    let parent = &decorations["parent"]["entity"]["decorations"];
    let artist = decorations["creators"]
        .as_array()
        .map(|creators| {
            creators
                .iter()
                .filter_map(|creator| creator["entity"]["decorations"]["identity"]["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let cover = |entity: &Value| {
        entity["visual_identity"]["cover"]
            .as_array()
            .and_then(|covers| {
                covers.iter().find_map(|cover| {
                    cover["url"]
                        .as_str()
                        .filter(|url| url.starts_with("https://"))
                        .map(str::to_owned)
                })
            })
    };
    Some(TrackMetadata {
        uri: uri.into(),
        title: decorations["identity"]["name"]
            .as_str()
            .unwrap_or("")
            .into(),
        artist,
        album: parent["identity"]["name"].as_str().unwrap_or("").into(),
        duration_ms: decorations["playback"]["duration_ms"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        cover_url: cover(decorations).or_else(|| cover(parent)),
    })
}

fn native_queue(value: &Value) -> QueueSnapshot {
    let entries = |name: &str| {
        value[name]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let item = &entry["item"];
                let uri = item["uri"].as_str()?;
                if validate_uri(uri, false).is_err() || !is_item(uri) {
                    return None;
                }
                Some(QueueEntry {
                    uid: entry["uid"]
                        .as_str()
                        .filter(|uid| !uid.is_empty())
                        .unwrap_or(uri)
                        .to_owned(),
                    source: match entry["source"].as_str() {
                        Some("context") => QueueSource::Context,
                        Some("queue") => QueueSource::Queue,
                        Some("autoplay") => QueueSource::Autoplay,
                        _ => QueueSource::Unknown,
                    },
                    uri: uri.to_owned(),
                    title: item["decorations"]["identity"]["name"]
                        .as_str()
                        .filter(|title| !title.is_empty())
                        .map(str::to_owned),
                })
            })
            .collect()
    };
    QueueSnapshot {
        previous: entries("previous"),
        upcoming: entries("upcoming"),
        complete_catalog_context: false,
    }
}

struct DeferredSeek {
    uri: String,
    position: u32,
    deadline: Instant,
}

#[derive(Default)]
struct Session {
    playback: Playback,
    seek: Option<DeferredSeek>,
    repeat: RepeatMode,
    // The SDK uses the same event for broadcasts and query responses, without
    // correlation IDs. Coalesce all-available requests, never infer catalog completeness.
    queue_query_pending: bool,
}

impl Session {
    fn repeat_commands(&self) -> [Value; 2] {
        if self.repeat == RepeatMode::One {
            return [
                field_command("set_repeat_context", "enabled", false),
                field_command("set_repeat_track", "enabled", true),
            ];
        }
        [
            field_command(
                "set_repeat_track",
                "enabled",
                self.repeat == RepeatMode::One,
            ),
            field_command(
                "set_repeat_context",
                "enabled",
                self.repeat == RepeatMode::All,
            ),
        ]
    }

    fn options(&self, peer: &mut Peer, shuffle: bool) -> Result<()> {
        peer.send(field_command("set_shuffle", "enabled", shuffle))?;
        for command in self.repeat_commands() {
            peer.send(command)?;
        }
        Ok(())
    }

    fn cancel_pending(&mut self) {
        self.seek = None;
    }

    fn defer_seek(&mut self, uri: &str, position: u32) {
        self.seek = (position != 0).then(|| DeferredSeek {
            uri: uri.into(),
            position,
            deadline: Instant::now() + SEEK_TIMEOUT,
        });
    }

    fn load(&mut self, peer: &mut Peer, uri: &str, position: u32, shuffle: bool) -> Result<()> {
        peer.command("activate")?;
        self.options(peer, shuffle)?;
        peer.send(field_command("play", "uri", uri))?;
        self.defer_seek(uri, position);
        // A later state confirmation, never the previous track's playing flag,
        // authorizes the deferred seek.
        self.playback.status.clear();
        Ok(())
    }

    fn handle(
        &mut self,
        cmd: Command,
        peer: &mut Peer,
        tx: &flume::Sender<EngineEvent>,
    ) -> Result<()> {
        match cmd {
            Command::Context(context, position, shuffle) => {
                self.seek = None;
                // Cached Connect state can survive a runtime restart without a
                // loaded local audio context. Fresh requests always need a URI.
                self.load(peer, &context, position, shuffle)?;
            }
            Command::Play => {
                peer.active(command("play"))?;
            }
            Command::Pause => {
                self.seek = None;
                peer.active(command("pause"))?;
            }
            Command::Stop => {
                self.cancel_pending();
                if self.playback.active {
                    peer.command("pause")?;
                }
                emit(tx, EngineEvent::Stopped);
            }
            Command::Toggle => {
                let name = if self.playback.active && self.playback.status == "playing" {
                    "pause"
                } else {
                    "play"
                };
                if name == "pause" {
                    self.seek = None;
                }
                peer.active(command(name))?;
            }
            Command::Next => {
                self.seek = None;
                peer.active(command("skip_next"))?;
            }
            Command::Prev => {
                self.seek = None;
                peer.active(command("skip_prev"))?;
            }
            Command::Seek(position) => {
                self.seek = None;
                peer.active(field_command("seek", "position_ms", position))?;
            }
            Command::Volume(volume) => {
                peer.send(field_command("set_volume", "volume", volume))?;
            }
            Command::Repeat(mode) => {
                self.repeat = mode;
                peer.command("activate")?;
                for command in self.repeat_commands() {
                    peer.send(command)?;
                }
            }
            Command::Shuffle(on) => {
                peer.command("activate")?;
                peer.send(field_command("set_shuffle", "enabled", on))?;
            }
            Command::Add(uri) => {
                peer.active(field_command("add_to_queue", "uri", uri))?;
                // Refresh after the write even if an earlier query is in flight.
                // Only a native queue event can confirm what the SDK retained.
                self.queue_query_pending = false;
                self.request_native_queue(peer)?;
            }
            Command::Queue => {
                self.request_native_queue(peer)?;
            }
            Command::State => peer.command("get_state")?,
        }
        Ok(())
    }

    fn request_native_queue(&mut self, peer: &mut Peer) -> Result<()> {
        if !self.queue_query_pending {
            peer.send(queue_query())?;
            self.queue_query_pending = true;
        }
        Ok(())
    }

    fn observe(
        &mut self,
        value: &Value,
        peer: &mut Peer,
        tx: &flume::Sender<EngineEvent>,
    ) -> Result<()> {
        // Neither command acceptance nor an unrelated notification confirms a
        // pending load. Only actual playback events can release a deferred seek.
        if !matches!(
            value["type"].as_str(),
            Some(
                "playback_state"
                    | "track_changed"
                    | "playback_changed"
                    | "device_changed"
                    | "context_changed"
                    | "position_sync"
                    | "queue_changed"
                    | "options_changed"
                    | "volume_changed"
            )
        ) {
            return Ok(());
        }
        // Publish the device role before item observations so reducers can
        // reject global state without rejecting the first local snapshot.
        observe_authentication(
            value,
            true,
            value["is_active"].as_bool().unwrap_or(self.playback.active),
            tx,
        );
        let was_active = self.playback.active;
        if value["is_active"] == false && value["type"] == "queue_changed" {
            self.playback.locally_inactive(tx);
        }
        self.playback.update(value, tx);
        if !self.playback.active {
            self.queue_query_pending = false;
            self.seek = None;
            return Ok(());
        }
        if !was_active {
            self.request_native_queue(peer)?;
        }
        if let Some(repeat) = value["options"]["repeat"].as_str() {
            self.repeat = match repeat {
                "off" => RepeatMode::Off,
                "track" => RepeatMode::One,
                "context" => RepeatMode::All,
                _ => self.repeat,
            };
        }
        if value["type"] == "queue_changed" {
            if self.queue_query_pending {
                self.queue_query_pending = false;
                emit(tx, EngineEvent::NativeQueue(native_queue(value)));
            } else {
                // Broadcasts contain at most ten entries. Refresh on the same
                // worker rather than replacing the native queue with a preview.
                self.request_native_queue(peer)?;
            }
        }
        if value["type"] != "queue_changed" {
            if let Some(seek) = &self.seek {
                if Instant::now() >= seek.deadline {
                    self.seek = None;
                    status(tx, "Soloist timed out waiting for the requested track to play; deferred seek cancelled");
                } else if self.playback.playing(&seek.uri) {
                    peer.active(field_command("seek", "position_ms", seek.position))?;
                    self.seek = None;
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self, tx: &flume::Sender<EngineEvent>) {
        if self
            .seek
            .as_ref()
            .is_some_and(|seek| Instant::now() >= seek.deadline)
        {
            self.seek = None;
            status(tx, "Soloist timed out waiting for the requested track to play; deferred seek cancelled");
        }
    }

    fn disconnected(&mut self) {
        self.queue_query_pending = false;
        self.playback.reset_connection();
        self.seek = None;
    }
}

fn run(
    config: SoloistConfig,
    volume: u8,
    rx: flume::Receiver<Command>,
    tx: &flume::Sender<EngineEvent>,
    control: &WorkerControl<'_>,
    started: &mut Option<tokio::sync::oneshot::Sender<Result<()>>>,
    processor: Box<dyn soloist_runtime::pcm::AudioProcessor>,
) -> Result<()> {
    ensure!(
        cfg!(all(target_os = "macos", target_arch = "aarch64")),
        "Managed Soloist requires macOS ARM64"
    );
    let runtime_config = runtime_config(&config, volume)
        .map_err(|error| error.context(soloist_runtime::StartupStage::Configuration))?;
    let owned = soloist_runtime::Runtime::start_processed(
        runtime_config,
        control.audio.control.clone(),
        processor,
    )?;
    let endpoint = owned.endpoint();
    emit(tx, EngineEvent::ProcessGroups(owned.process_groups()));
    // Retain only the validated public discovery ID, never the pairing body.
    *control
        .device_id
        .lock()
        .map_err(|_| anyhow!("Soloist identity lock failed"))? = Some(owned.device_id()?);
    let mut runtime = owned;
    let result = transport(&endpoint, &mut runtime, volume, rx, tx, control, started)
        .map_err(|error| error.context(soloist_runtime::StartupStage::Transport));
    // Always check owned cleanup, even after a transport failure.
    let cleanup = runtime
        .shutdown()
        .map_err(|_| anyhow!("Soloist owned runtime cleanup failed"));
    emit(tx, EngineEvent::ProcessGroups(Vec::new()));
    if cleanup.is_err() {
        status(tx, "Soloist owned runtime cleanup failed");
    }
    result.and(cleanup)
}

fn transport(
    endpoint: &str,
    runtime: &mut soloist_runtime::Runtime,
    volume: u8,
    rx: flume::Receiver<Command>,
    tx: &flume::Sender<EngineEvent>,
    control: &WorkerControl<'_>,
    started: &mut Option<tokio::sync::oneshot::Sender<Result<()>>>,
) -> Result<()> {
    let WorkerControl {
        stop,
        stop_pending,
        phase,
        audio,
        ..
    } = control;
    let mut session = Session::default();
    let intents = Intents;
    let mut failures = 0u32;
    let mut reconnecting = false;
    let mut initial_volume = Some(volume);
    let mut diagnostics = soloist_runtime::SdkDiagnostics::default();
    while !stop.load(Ordering::Acquire) {
        if !runtime.is_running() {
            bail!("Soloist owned runtime exited");
        }
        let connection = Peer::connect(endpoint);
        if let Ok(mut peer) = connection {
            let connected = Instant::now();
            let mut authenticated = false;
            let mut saw_auth = false;
            let mut poll_at = Instant::now() + POLL;
            phase.store(WAITING, Ordering::Release);
            let mut runtime_exited = false;
            let connection_result = (|| -> Result<()> {
                peer.command("get_auth_state")?;
                while !stop.load(Ordering::Acquire) {
                    let observed_diagnostics = runtime.sdk_diagnostics();
                    if observed_diagnostics != diagnostics {
                        diagnostics = observed_diagnostics;
                        emit(
                            tx,
                            EngineEvent::BackendStatus {
                                message: diagnostics.safe_message(),
                            },
                        );
                    }
                    if !runtime.is_running() {
                        runtime_exited = true;
                        bail!("Soloist owned runtime exited");
                    }
                    ensure!(
                        saw_auth || connected.elapsed() < AUTH_TIMEOUT,
                        "Soloist did not send initial authentication state"
                    );
                    if Instant::now() >= poll_at {
                        peer.command("get_auth_state")?;
                        if authenticated {
                            peer.command("get_state")?;
                        }
                        poll_at = Instant::now() + POLL;
                    }
                    // Drain even without authentication: discard stale loads and
                    // controls while preserving Stop as an ordering barrier.
                    let batch = intents.drain(&rx, authenticated, stop_pending);
                    for cmd in batch {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        if authenticated {
                            let flush = matches!(
                                &cmd,
                                Command::Context(..)
                                    | Command::Pause
                                    | Command::Stop
                                    | Command::Toggle
                                    | Command::Seek(_)
                                    | Command::Next
                                    | Command::Prev
                            );
                            session.handle(cmd, &mut peer, tx)?;
                            if flush {
                                session.playback.status.clear();
                                audio.set_active(false);
                                audio.reset();
                            }
                        } else if matches!(cmd, Command::Stop) {
                            session.cancel_pending();
                            emit(tx, EngineEvent::Stopped);
                        }
                    }
                    if authenticated && !stop.load(Ordering::Acquire) {
                        session.tick(tx);
                    }
                    if let Some(value) = peer.receive()? {
                        if value["type"] == "auth_state" {
                            if let Some(logged_in) = value["logged_in"].as_bool() {
                                let auth_changed = !saw_auth || logged_in != authenticated;
                                let newly_authenticated = logged_in && !authenticated;
                                saw_auth = true;
                                authenticated = logged_in;
                                phase.store(
                                    if logged_in { READY } else { WAITING },
                                    Ordering::Release,
                                );
                                if let Some(started) = started.take() {
                                    let _ = started.send(Ok(()));
                                }
                                if reconnecting {
                                    emit(tx, EngineEvent::Reconnected);
                                    reconnecting = false;
                                }
                                if !logged_in {
                                    audio.set_active(false);
                                    session.playback.locally_inactive(tx);
                                    session.playback.reset_connection();
                                    session.queue_query_pending = false;
                                    session.seek = None;
                                    if auth_changed {
                                        status(tx, "Soloist is waiting for authentication");
                                    }
                                } else if value["is_active"] == false {
                                    audio.set_active(false);
                                    session.playback.locally_inactive(tx);
                                    session.queue_query_pending = false;
                                }
                                if newly_authenticated {
                                    status(tx, "Soloist is authenticated");
                                    if let Some(volume) = initial_volume.take() {
                                        peer.send(field_command("set_volume", "volume", volume))?;
                                    }
                                    peer.command("get_state")?;
                                    session.request_native_queue(&mut peer)?;
                                }
                                observe_authentication(
                                    &value,
                                    logged_in,
                                    value["is_active"]
                                        .as_bool()
                                        .unwrap_or(session.playback.active),
                                    tx,
                                );
                            }
                        } else if authenticated {
                            if value["type"] == "error" {
                                session.seek = None;
                                status(
                                    tx,
                                    "Soloist rejected a command; playback was not confirmed",
                                );
                            } else {
                                // command_result is intentionally ignored by the
                                // state reducer: acceptance is not playback.
                                session.observe(&value, &mut peer, tx)?;
                                audio.set_active(
                                    session.playback.active && session.playback.status == "playing",
                                );
                            }
                        }
                    }
                    // A server that immediately accepts then drops sockets must
                    // still exhaust the retry budget. Reset only after stability.
                    if saw_auth && connected.elapsed() >= Duration::from_secs(30) {
                        failures = 0;
                    }
                }
                Ok(())
            })();
            ensure!(!runtime_exited, "Soloist owned runtime exited");
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if connection_result.is_err() {
                audio.set_active(false);
                session.disconnected();
                emit(
                    tx,
                    EngineEvent::Authentication {
                        logged_in: false,
                        is_active: false,
                    },
                );
            }
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        phase.store(DISCONNECTED, Ordering::Release);
        if !reconnecting {
            emit(tx, EngineEvent::Reconnecting);
            reconnecting = true;
        }
        // Do not replay controls submitted against the old authenticated state.
        let mut discarded = false;
        for _ in 0..MAILBOX_CAPACITY {
            if rx.try_recv().is_err() {
                break;
            }
            discarded = true;
        }
        if discarded {
            status(
                tx,
                "Soloist connection lost; queued commands were cancelled, retry after reconnecting",
            );
        }
        failures += 1;
        ensure!(failures <= 6, "Soloist reconnect attempts exhausted");
        let delay = Duration::from_millis((250u64 << (failures - 1)).min(4000));
        let until = Instant::now() + delay;
        while Instant::now() < until && !stop.load(Ordering::Acquire) {
            thread::sleep(
                Duration::from_millis(50).min(until.saturating_duration_since(Instant::now())),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "spotify:track:0123456789abcdefghijkl";
    const B: &str = "spotify:track:abcdefghijkl0123456789";

    fn item(uri: &str) -> Value {
        json!({"uri":uri,"decorations": {
            "identity":{"name":"Song"},
            "creators":[{"entity":{"decorations":{"identity":{"name":"Artist"}}}}],
            "parent":{"entity":{"decorations":{"identity":{"name":"Album"},"visual_identity":{"cover":[{"url":"https://example.test/cover"}]}}}},
            "playback":{"duration_ms":210000}
        }})
    }

    #[test]
    fn endpoints_are_literal_loopback_only_without_credentials_or_query() {
        for valid in [
            "ws://127.0.0.1:1234",
            "ws://127.9.8.7:1/",
            "ws://[::1]:65535",
        ] {
            assert!(endpoint_address(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "ws://localhost:1234",
            "wss://127.0.0.1:1234",
            "ws://192.168.1.1:1234",
            "ws://127.0.0.1:0",
            "ws://127.0.0.1",
            "ws://user@127.0.0.1:1234",
            "ws://127.0.0.1:1234?key=x",
            "ws://127.0.0.1:1234/#fragment",
            "ws://127.0.0.1:1234/path",
            "ws://[::ffff:127.0.0.1]:1234",
        ] {
            assert!(endpoint_address(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn uri_validation_rejects_paths_and_wrong_namespace_or_id() {
        assert!(validate_uri(A, true).is_ok());
        assert!(validate_uri("spotify:playlist:0123456789abcdefghijkl", false).is_ok());
        assert!(validate_uri("spotify:artist:0123456789abcdefghijkl", false).is_ok());
        for invalid in [
            "spotify:track:short",
            "https://open.spotify.com/track/0123456789abcdefghijkl",
            "spotify:user:0123456789abcdefghijkl",
            "spotify:track:0123456789abcdefghijk_",
            "spotify:track:0123456789abcdefghijkl:extra",
        ] {
            assert!(validate_uri(invalid, false).is_err());
        }
        assert!(validate_uri("spotify:episode:0123456789abcdefghijkl", true).is_err());
    }

    #[test]
    fn parses_official_decorations_and_nested_queue_items() {
        let metadata = metadata(&item(A)).unwrap();
        assert_eq!(
            (
                metadata.title.as_str(),
                metadata.artist.as_str(),
                metadata.album.as_str()
            ),
            ("Song", "Artist", "Album")
        );
        assert_eq!(metadata.duration_ms, 210000);
        assert_eq!(
            metadata.cover_url.as_deref(),
            Some("https://example.test/cover")
        );
        let queue = native_queue(&json!({"upcoming":[{"item":item(A)}, {"uri":B}]}));
        assert_eq!(queue.upcoming.len(), 1);
        assert_eq!(queue.upcoming[0].uri, A);
        assert_eq!(queue.upcoming[0].title.as_deref(), Some("Song"));
        assert!(metadata_from_empty().is_none());
    }

    fn metadata_from_empty() -> Option<TrackMetadata> {
        metadata(&json!({"uri":""}))
    }

    #[test]
    fn metadata_precedes_playback_and_is_deduplicated() {
        let (tx, rx) = flume::unbounded();
        let mut state = Playback::default();
        let event = json!({"type":"playback_state","item":item(A),"status":"playing","is_active":true,"position":{"position_ms":42}});
        assert!(state.update(&event, &tx));
        let events: Vec<_> = rx.drain().collect();
        assert!(matches!(&events[0], EngineEvent::TrackChanged { uri } if uri == A));
        assert!(matches!(&events[1], EngineEvent::Metadata(_)));
        assert!(matches!(
            &events[2],
            EngineEvent::Playing {
                position_ms: 42,
                ..
            }
        ));
        state.update(&event, &tx);
        assert!(rx
            .drain()
            .all(|event| matches!(event, EngineEvent::PositionCorrection { .. })));
    }

    #[test]
    fn inactive_global_playback_and_acknowledgements_never_claim_local_play() {
        let (tx, rx) = flume::unbounded();
        let mut state = Playback::default();
        state.update(
            &json!({"type":"playback_state","item":item(A),"status":"playing","is_active":false}),
            &tx,
        );
        state.update(
            &json!({"type":"command_result","command":"play","is_active":true,"status":"playing"}),
            &tx,
        );
        assert!(!state.playing(A));
        assert!(rx.drain().all(|event| !matches!(
            event,
            EngineEvent::Playing { .. } | EngineEvent::Paused { .. }
        )));
        state.update(&json!({"type":"device_changed","is_active":true}), &tx);
        assert!(
            !state.playing(A),
            "activation does not confirm cached global playback"
        );
        state.update(
            &json!({"type":"playback_state","item":item(A),"status":"playing","is_active":true}),
            &tx,
        );
        assert!(state.playing(A));
        state.update(&json!({"type":"device_changed","is_active":false}), &tx);
        assert!(rx
            .drain()
            .any(|event| matches!(event, EngineEvent::LocallyInactive)));
    }

    #[test]
    fn reacquiring_the_same_track_republishes_metadata_and_buffering_freezes_progress() {
        let (tx, rx) = flume::unbounded();
        let mut state = Playback::default();
        let playing =
            json!({"type":"playback_state","item":item(A),"status":"playing","is_active":true});
        state.update(&playing, &tx);
        rx.drain().for_each(drop);
        state.update(&json!({"type":"device_changed","is_active":false}), &tx);
        assert!(rx
            .drain()
            .any(|event| matches!(event, EngineEvent::LocallyInactive)));
        state.update(&playing, &tx);
        let events: Vec<_> = rx.drain().collect();
        assert!(matches!(
            events.first(),
            Some(EngineEvent::TrackChanged { .. })
        ));
        assert!(matches!(events.get(1), Some(EngineEvent::Metadata(_))));
        state.update(
            &json!({"type":"playback_changed","status":"buffering"}),
            &tx,
        );
        assert!(rx
            .drain()
            .any(|event| matches!(event, EngineEvent::Buffering { .. })));
        assert!(!state.playing(A));
    }

    #[test]
    fn track_change_requires_fresh_playing_confirmation_for_deferred_seek() {
        let (tx, _) = flume::unbounded();
        let mut state = Playback::default();
        state.update(
            &json!({"type":"playback_state","item":item(A),"status":"playing","is_active":true}),
            &tx,
        );
        state.update(&json!({"type":"track_changed","item":item(B)}), &tx);
        assert!(!state.playing(B));
        state.update(&json!({"type":"playback_changed","status":"playing"}), &tx);
        assert!(state.playing(B));
        state.reset_connection();
        assert!(!state.playing(B));
    }

    #[test]
    fn command_mapping_and_volume_conversion_use_sdk_fields() {
        assert_eq!(
            field_command("seek", "position_ms", 500u32),
            json!({"type":"command","command":"seek","position_ms":500})
        );
        assert_eq!(
            field_command("set_shuffle", "enabled", true),
            json!({"type":"command","command":"set_shuffle","enabled":true})
        );
        assert_eq!(
            field_command("set_repeat_context", "enabled", false)["enabled"],
            false
        );
        assert_eq!(
            (
                volume_percent(0),
                volume_percent(32768),
                volume_percent(u16::MAX)
            ),
            (0, 50, 100)
        );
        for volume in 0..=u16::MAX {
            assert!(volume_percent(volume) <= 100);
        }
    }

    #[test]
    fn disconnect_cancels_deferred_seek_and_queue_query_but_not_repeat_preference() {
        let mut session = Session {
            repeat: RepeatMode::All,
            queue_query_pending: true,
            ..Session::default()
        };
        session.defer_seek(A, 5000);
        session.disconnected();
        assert!(session.seek.is_none());
        assert!(!session.queue_query_pending);
        assert_eq!(session.repeat, RepeatMode::All);
        assert!(!session.playback.active);
    }

    #[test]
    fn manifest_and_auth_pending_controls_fail_without_sensitive_details() {
        assert_eq!(
            parse_manifest(br#"{"arch":"aarch64-linux","libraryPath":"/lib:/other"}"#).unwrap(),
            "/lib:/other"
        );
        assert!(
            parse_manifest(br#"{"arch":"x86_64-linux","libraryPath":"PRIVATE-SENTINEL"}"#)
                .unwrap_err()
                .to_string()
                .find("PRIVATE-SENTINEL")
                .is_none()
        );
        let (engine, rx) = test_engine(WAITING);
        assert!(engine.pause().is_err());
        assert!(engine.set_volume(100).is_err());
        engine.phase.store(READY, Ordering::Release);
        for _ in 0..MAILBOX_CAPACITY {
            engine.play_track_at(A.into(), 0).unwrap();
        }
        assert!(engine.play_track_at(A.into(), 0).is_err());
        engine.phase.store(WAITING, Ordering::Release);
        engine.stop();
        assert!(engine.stop_pending.load(Ordering::Acquire));
        assert!(!engine.stopping.load(Ordering::Acquire));
        let intents = Intents;
        let batch = intents.drain(&rx, false, &engine.stop_pending);
        assert!(matches!(batch.front(), Some(Command::Stop)));
        assert_eq!(batch.len(), 1);
        assert!(intents.drain(&rx, true, &engine.stop_pending).is_empty());
        engine.phase.store(READY, Ordering::Release);
        engine.play_track_at(B.into(), 0).unwrap();
        engine.shutdown().unwrap();
        assert!(engine.play_track_at(A.into(), 0).is_err());
    }

    fn test_engine(phase: u8) -> (RuntimePlayer, flume::Receiver<Command>) {
        let (commands, rx) = flume::bounded(MAILBOX_CAPACITY);
        let audio = soloist_runtime::pcm::AudioControl::default();
        let engine = RuntimePlayer {
            bands: VisBands::shared(),
            spectrum_enabled: Arc::new(AtomicBool::new(false)),
            device_id: Arc::new(Mutex::new(None)),
            equalizer: crate::audio::equalizer::shared_equalizer(),
            playback_gate: PlaybackGate::new(audio.clone()),
            audio,
            commands,
            stopping: Arc::new(AtomicBool::new(false)),
            stop_pending: Arc::new(AtomicBool::new(false)),
            phase: Arc::new(AtomicU8::new(phase)),
            worker: Mutex::new(None),
        };
        (engine, rx)
    }

    #[test]
    fn selected_context_and_invalid_position_requests_are_rejected_before_mailbox() {
        let context = "spotify:playlist:0123456789abcdefghijkl";
        let episode = "spotify:episode:0123456789abcdefghijkl";
        for phase in [WAITING, READY] {
            let (player, commands) = test_engine(phase);
            for uri in [context, A, episode] {
                for selected in [A, B, episode, "invalid"] {
                    assert!(player
                        .play_context_at(uri.into(), Some(selected.into()), 0, false)
                        .is_err());
                }
            }
            assert!(player
                .play_context_at(context.into(), None, 1234, false)
                .is_err());
            assert!(commands.is_empty());
            if phase == WAITING {
                assert!(player
                    .play_context_at(context.into(), None, 0, true)
                    .is_err());
                assert!(player.play_track_at(A.into(), 1234).is_err());
                assert!(commands.is_empty());
                continue;
            }
            player
                .play_context_at(context.into(), None, 0, true)
                .unwrap();
            assert!(
                matches!(commands.try_recv().unwrap(), Command::Context(uri, 0, true) if uri == context)
            );
            player.play_track_at(A.into(), 1234).unwrap();
            assert!(
                matches!(commands.try_recv().unwrap(), Command::Context(uri, 1234, false) if uri == A)
            );
        }
    }

    #[test]
    fn sdk_playing_requires_verification_without_changing_availability_or_commands() {
        let (player, commands) = test_engine(READY);
        let worker_gate = player.playback_gate.clone();
        let dsp_ready = player.has_local_dsp();
        assert!(!worker_gate.state.lock().unwrap().verified);
        assert!(!worker_gate.state.lock().unwrap().applied_active);

        worker_gate.set_active(true);
        assert!(worker_gate.state.lock().unwrap().audible);
        assert!(!worker_gate.state.lock().unwrap().applied_active);
        player.set_verified(true);
        assert!(worker_gate.state.lock().unwrap().applied_active);
        player.set_verified(true);
        assert!(worker_gate.state.lock().unwrap().applied_active);
        assert_eq!(player.has_local_dsp(), dsp_ready);
        assert_eq!(player.phase.load(Ordering::Acquire), READY);
        assert!(commands.is_empty());
    }

    #[test]
    fn verification_loss_mutes_and_sdk_inactivity_prevents_reactivation() {
        let (player, commands) = test_engine(READY);
        let worker_gate = player.playback_gate.clone();
        worker_gate.set_active(true);
        player.set_verified(true);
        assert!(worker_gate.state.lock().unwrap().applied_active);

        player.set_verified(false);
        assert!(!worker_gate.state.lock().unwrap().applied_active);
        worker_gate.set_active(true);
        assert!(!worker_gate.state.lock().unwrap().applied_active);
        worker_gate.set_active(false);
        player.set_verified(true);
        assert!(!worker_gate.state.lock().unwrap().audible);
        assert!(!worker_gate.state.lock().unwrap().applied_active);

        worker_gate.set_active(true);
        assert!(worker_gate.state.lock().unwrap().applied_active);
        worker_gate.set_active(false); // The same invalidation used on disconnect.
        player.set_verified(true);
        assert!(!worker_gate.state.lock().unwrap().applied_active);
        assert!(commands.is_empty());
    }

    #[test]
    fn shutdown_invalidates_audibility_and_cannot_be_reverified() {
        let (player, _) = test_engine(READY);
        player.playback_gate.set_active(true);
        player.set_verified(true);
        assert!(player.playback_gate.state.lock().unwrap().applied_active);
        player.shutdown().unwrap();
        player.set_verified(true);
        let state = player.playback_gate.state.lock().unwrap();
        assert!(!state.verified);
        assert!(!state.audible);
        assert!(!state.applied_active);
    }

    #[test]
    fn stop_is_a_playback_intent_and_shutdown_owns_worker_cancellation() {
        let (engine, rx) = test_engine(WAITING);
        let cancelled = engine.stopping.clone();
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = completed.clone();
        *engine.worker.lock().unwrap() = Some(thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !cancelled.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::park_timeout(Duration::from_millis(1));
            }
            ensure!(
                cancelled.load(Ordering::Acquire),
                "worker was not cancelled"
            );
            worker_completed.store(true, Ordering::Release);
            Ok(())
        }));
        engine.stop();
        assert!(matches!(rx.try_recv(), Ok(Command::Stop)));
        assert!(!engine.stopping.load(Ordering::Acquire));
        assert!(engine.play_track_at(A.into(), 0).is_err());
        engine.phase.store(READY, Ordering::Release);
        engine.play_track_at(A.into(), 0).unwrap();
        engine.shutdown().unwrap();
        assert!(engine.stopping.load(Ordering::Acquire));
        assert!(completed.load(Ordering::Acquire));
        assert!(engine.worker.lock().unwrap().is_none());
        assert!(engine.play_track_at(A.into(), 0).is_err());
        engine.shutdown().unwrap();
    }

    #[test]
    fn waiting_rejects_every_control_except_stop_without_enqueue() {
        let (player, commands) = test_engine(WAITING);
        assert!(player.play_track_at(A.into(), 1234).is_err());
        assert!(player
            .play_context("spotify:playlist:0123456789abcdefghijkl".into(), true)
            .is_err());
        assert!(player
            .play_context_at(
                "spotify:album:0123456789abcdefghijkl".into(),
                None,
                0,
                false
            )
            .is_err());
        assert!(player.play().is_err());
        assert!(player.pause().is_err());
        assert!(player.toggle().is_err());
        assert!(player.next().is_err());
        assert!(player.prev().is_err());
        assert!(player.shuffle(true).is_err());
        assert!(player.set_repeat(RepeatMode::All).is_err());
        assert!(player.set_volume(100).is_err());
        assert!(player.seek(5000).is_err());
        assert!(player.add_to_queue(B.into()).is_err());
        assert!(player.request_queue().is_err());
        assert!(player.request_state().is_err());
        assert!(commands.is_empty());
        player.stop();
        assert!(matches!(commands.try_recv(), Ok(Command::Stop)));
        assert!(commands.is_empty());
    }

    #[test]
    fn ready_accepted_loads_and_controls_are_discarded_after_auth_loss_without_replay() {
        let (player, commands) = test_engine(READY);
        player.play_track_at(A.into(), 1234).unwrap();
        player
            .play_context("spotify:playlist:0123456789abcdefghijkl".into(), true)
            .unwrap();
        player.play().unwrap();
        player.pause().unwrap();
        player.seek(5000).unwrap();
        player.set_volume(100).unwrap();
        player.set_repeat(RepeatMode::All).unwrap();
        player.shuffle(true).unwrap();
        player.add_to_queue(B.into()).unwrap();
        player.request_queue().unwrap();
        player.request_state().unwrap();
        assert!(!commands.is_empty());
        player.phase.store(WAITING, Ordering::Release);
        let intents = Intents;
        assert!(intents
            .drain(&commands, false, &player.stop_pending)
            .is_empty());
        assert!(commands.is_empty());
        player.phase.store(READY, Ordering::Release);
        assert!(intents
            .drain(&commands, true, &player.stop_pending)
            .is_empty());
        player.play_track_at(B.into(), 0).unwrap();
        let batch = intents.drain(&commands, true, &player.stop_pending);
        assert_eq!(batch.len(), 1);
        assert!(matches!(batch.front(), Some(Command::Context(uri, 0, false)) if uri == B));
    }

    #[test]
    fn stop_cancels_queued_loads_and_controls_with_or_without_authentication() {
        for authenticated in [false, true] {
            let (player, commands) = test_engine(READY);
            player.play_track_at(A.into(), 0).unwrap();
            player.set_volume(100).unwrap();
            player.set_repeat(RepeatMode::All).unwrap();
            player.seek(5000).unwrap();
            if !authenticated {
                player.phase.store(WAITING, Ordering::Release);
            }
            player.stop();
            let intents = Intents;
            let batch = intents.drain(&commands, authenticated, &player.stop_pending);
            assert_eq!(batch.len(), 1);
            assert!(matches!(batch.front(), Some(Command::Stop)));
            player.phase.store(READY, Ordering::Release);
            assert!(intents
                .drain(&commands, true, &player.stop_pending)
                .is_empty());
        }
    }

    #[test]
    fn ready_commands_are_drained_in_bounded_coalesced_batches() {
        let (tx, rx) = flume::unbounded();
        let stop_pending = AtomicBool::new(false);
        let intents = Intents;
        for _ in 0..MAILBOX_CAPACITY * 2 {
            tx.send(Command::Next).unwrap();
        }
        let batch = intents.drain(&rx, true, &stop_pending);
        assert_eq!(rx.len(), MAILBOX_CAPACITY);
        assert_eq!(batch.len(), MAILBOX_CAPACITY);
        assert!(batch.iter().all(|command| matches!(command, Command::Next)));
        intents.drain(&rx, true, &stop_pending);
        for position in 0..MAILBOX_CAPACITY {
            tx.send(Command::Seek(position as u32)).unwrap();
        }
        let batch = intents.drain(&rx, true, &stop_pending);
        assert_eq!(batch.len(), 1);
        assert!(
            matches!(batch.front(), Some(Command::Seek(position)) if *position == (MAILBOX_CAPACITY - 1) as u32)
        );
        tx.send(Command::Context(A.into(), 0, false)).unwrap();
        tx.send(Command::Context(B.into(), 0, false)).unwrap();
        let batch = intents.drain(&rx, true, &stop_pending);
        assert_eq!(batch.len(), 1);
        assert!(matches!(batch.front(), Some(Command::Context(uri, _, _)) if uri == B));
    }

    #[test]
    fn ready_load_replacement_preserves_preferences_and_stop_ordering_barrier() {
        let (tx, rx) = flume::bounded(MAILBOX_CAPACITY);
        let stop_pending = AtomicBool::new(false);
        for command in [
            Command::Volume(10),
            Command::Context(A.into(), 0, false),
            Command::Repeat(RepeatMode::All),
            Command::Seek(1000),
            Command::Context(B.into(), 0, true),
        ] {
            tx.send(command).unwrap();
        }
        let intents = Intents;
        let batch = intents.drain(&rx, true, &stop_pending);
        assert_eq!(batch.len(), 3);
        assert!(matches!(batch[0], Command::Volume(10)));
        assert!(matches!(batch[1], Command::Repeat(RepeatMode::All)));
        assert!(matches!(&batch[2], Command::Context(uri, 0, true) if uri == B));
        for command in [
            Command::Context(A.into(), 0, false),
            Command::Stop,
            Command::Volume(20),
            Command::Context(B.into(), 0, false),
        ] {
            tx.send(command).unwrap();
        }
        let batch = intents.drain(&rx, true, &stop_pending);
        assert_eq!(batch.len(), 3);
        assert!(matches!(batch[0], Command::Stop));
        assert!(matches!(batch[1], Command::Volume(20)));
        assert!(matches!(&batch[2], Command::Context(uri, 0, false) if uri == B));
    }

    #[test]
    fn runtime_credentials_select_inline_or_default_file_without_fallback() {
        let base = std::env::temp_dir().join("myx-credential-selection");
        let mut config = SoloistConfig {
            directory: Some(base.clone()),
            library_path: Some(base.to_string_lossy().into_owned()),
            api_key: Some("test-key".into()),
            ..SoloistConfig::default()
        };
        assert!(matches!(runtime_config(&config, 25).unwrap().api_key,
            soloist_runtime::ApiKey::Inline(key) if key == "test-key"));
        config.api_key = None;
        assert!(matches!(runtime_config(&config, 25).unwrap().api_key,
            soloist_runtime::ApiKey::File(path) if path == base.join("api-key")));
        config.api_key_file = Some(base.join("custom-key"));
        assert!(matches!(runtime_config(&config, 25).unwrap().api_key,
            soloist_runtime::ApiKey::File(path) if path == base.join("custom-key")));
        config.api_key = Some("PRIVATE-SENTINEL".into());
        let error = runtime_config(&config, 25).err().unwrap().to_string();
        assert!(error.contains("api_key"));
        assert!(!error.contains("PRIVATE-SENTINEL"));
    }

    #[test]
    fn cached_context_always_requires_an_explicit_uri_load() {
        for uri in [A, "spotify:playlist:0123456789abcdefghijkl"] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let reader = thread::spawn(move || {
                let mut socket =
                    WebSocket::from_raw_socket(socket, tungstenite::protocol::Role::Server, None);
                loop {
                    let message = socket.read().unwrap();
                    let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                    if value["command"] == "play" {
                        return value["uri"].as_str().map(str::to_owned);
                    }
                }
            });
            let mut peer = Peer(WebSocket::from_raw_socket(
                stream,
                tungstenite::protocol::Role::Client,
                None,
            ));
            let (tx, _rx) = flume::unbounded();
            let mut session = Session::default();
            session.playback.update(
                &json!({"type":"playback_state","is_active":true,"status":"playing",
                    "item":item(A),"context":{"uri":uri}}),
                &tx,
            );
            session
                .handle(Command::Context(uri.into(), 0, false), &mut peer, &tx)
                .unwrap();
            assert_eq!(reader.join().unwrap().as_deref(), Some(uri));
        }
    }

    #[test]
    fn stop_cancels_deferred_seek_without_fabricating_playback_state() {
        let mut session = Session::default();
        session.defer_seek(A, 5000);
        session.playback.active = true;
        session.playback.status = "playing".into();
        session.cancel_pending();
        assert!(session.seek.is_none());
        assert!(session.playback.active);
        assert_eq!(session.playback.status, "playing");
    }

    #[test]
    fn authentication_observations_follow_auth_and_state_but_never_acknowledgements() {
        let (tx, rx) = flume::unbounded();
        observe_authentication(
            &json!({"type":"command_result","logged_in":true,"is_active":true}),
            true,
            true,
            &tx,
        );
        assert!(rx.is_empty());
        for (kind, logged_in, active, expected) in [
            ("auth_state", false, true, (false, false)),
            ("auth_state", true, false, (true, false)),
            ("playback_state", true, true, (true, true)),
            ("device_changed", true, false, (true, false)),
        ] {
            observe_authentication(&json!({"type":kind}), logged_in, active, &tx);
            assert!(
                matches!(rx.recv().unwrap(), EngineEvent::Authentication { logged_in, is_active }
                if (logged_in, is_active) == expected)
            );
        }
    }

    #[test]
    fn repeat_modes_serialize_native_track_and_context_flags() {
        for (mode, track, context) in [
            (RepeatMode::Off, false, false),
            (RepeatMode::One, true, false),
            (RepeatMode::All, false, true),
        ] {
            let session = Session {
                repeat: mode,
                ..Session::default()
            };
            let mut expected = [
                json!({"type":"command","command":"set_repeat_track","enabled":track}),
                json!({"type":"command","command":"set_repeat_context","enabled":context}),
            ];
            if mode == RepeatMode::One {
                expected.reverse();
            }
            assert_eq!(session.repeat_commands(), expected);
            let (player, commands) = test_engine(READY);
            player.set_repeat(mode).unwrap();
            assert!(
                matches!(commands.recv().unwrap(), Command::Repeat(received) if received == mode)
            );
        }
    }

    #[test]
    fn public_identity_fails_closed_and_spectrum_detachment_clears_telemetry() {
        let (player, _) = test_engine(READY);
        assert!(player.device_id().is_err());
        *player.device_id.lock().unwrap() = Some("validated-sdk-id".into());
        assert_eq!(player.device_id().unwrap(), "validated-sdk-id");
        assert!(!player.spectrum_enabled.load(Ordering::Acquire));
        player.set_spectrum_enabled(true);
        player.bands.lock().unwrap().values.fill(1.0);
        player.bands.lock().unwrap().is_active = true;
        player.set_spectrum_enabled(false);
        let bands = player.bands.lock().unwrap();
        assert!(!bands.is_active);
        assert!(bands.values.iter().all(|value| *value == 0.0));
        drop(bands);
        player.shutdown().unwrap();
        assert!(player.device_id().is_err());
    }

    #[test]
    fn native_queue_preserves_both_orders_duplicates_sources_and_uri_fallback() {
        let queue = native_queue(&json!({
            "previous": [
                {"uid":"recent","source":"context","item":item(A)},
                {"uid":"older","source":"autoplay","item":item(B)}
            ],
            "upcoming": [
                {"uid":"duplicate","source":"queue","item":item(A)},
                {"uid":"duplicate","source":"context","item":item(A)},
                {"source":"autoplay","item":item(B)}
            ]
        }));
        assert_eq!(
            queue
                .previous
                .iter()
                .map(|entry| entry.uid.as_str())
                .collect::<Vec<_>>(),
            ["recent", "older"]
        );
        assert_eq!(
            queue
                .upcoming
                .iter()
                .map(|entry| entry.uid.as_str())
                .collect::<Vec<_>>(),
            ["duplicate", "duplicate", B]
        );
        assert_eq!(
            queue
                .upcoming
                .iter()
                .map(|entry| &entry.source)
                .collect::<Vec<_>>(),
            [
                &QueueSource::Queue,
                &QueueSource::Context,
                &QueueSource::Autoplay
            ]
        );
        assert!(!queue.complete_catalog_context);
    }

    fn mock_peer() -> (Peer, WebSocket<TcpStream>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        (
            Peer(WebSocket::from_raw_socket(
                stream,
                tungstenite::protocol::Role::Client,
                None,
            )),
            WebSocket::from_raw_socket(socket, tungstenite::protocol::Role::Server, None),
        )
    }

    fn read_command(server: &mut WebSocket<TcpStream>) -> Value {
        serde_json::from_str(server.read().unwrap().to_text().unwrap()).unwrap()
    }

    #[test]
    fn add_writes_once_then_refreshes_without_synthetic_queue_or_ack_authority() {
        for query_in_flight in [false, true] {
            let (mut peer, mut server) = mock_peer();
            let (tx, rx) = flume::unbounded();
            let mut session = Session {
                queue_query_pending: query_in_flight,
                ..Session::default()
            };
            session.playback.active = true;
            session.playback.status = "paused".into();
            let (player, commands) = test_engine(READY);
            player.add_to_queue(A.into()).unwrap();
            session
                .handle(commands.try_recv().unwrap(), &mut peer, &tx)
                .unwrap();
            assert!(rx.is_empty(), "a queue write is not an observation");
            assert!(session.queue_query_pending);
            session
                .observe(
                    &json!({"type":"command_result","command":"add_to_queue","success":true}),
                    &mut peer,
                    &tx,
                )
                .unwrap();
            assert!(rx.is_empty(), "SDK acceptance is not a native queue");
            session.tick(&tx);
            session.handle(Command::State, &mut peer, &tx).unwrap();
            assert_eq!(read_command(&mut server), command("activate"));
            assert_eq!(
                read_command(&mut server),
                json!({"type":"command","command":"add_to_queue","uri":A})
            );
            assert_eq!(read_command(&mut server), queue_query());
            assert_eq!(read_command(&mut server), command("get_state"));

            // The native result deliberately omits the requested A. Never merge
            // intent into observations, and retain native identities and order.
            let native = json!({"type":"queue_changed",
            "previous":[{"uid":"history","source":"context","item":item(A)}],
            "upcoming":[
                {"uid":"first","source":"queue","item":item(B)},
                {"uid":"second","source":"context","item":item(B)}
            ]});
            session.observe(&native, &mut peer, &tx).unwrap();
            match rx.try_recv().unwrap() {
                EngineEvent::NativeQueue(queue) => {
                    assert_eq!(queue, native_queue(&native));
                    assert_eq!(queue.upcoming.len(), 2);
                    assert!(queue.upcoming.iter().all(|entry| entry.uri == B));
                }
                _ => panic!("expected only the native queue observation"),
            }
            assert!(rx.is_empty());
            assert!(!session.queue_query_pending);
            session.disconnected();
            session.tick(&tx);
            session.handle(Command::State, &mut peer, &tx).unwrap();
            assert_eq!(read_command(&mut server), command("get_state"));
            assert!(rx.is_empty(), "disconnect must not fabricate queue state");
        }
    }

    #[test]
    fn shuffle_serializes_native_setting_without_local_reordering() {
        let (mut peer, mut server) = mock_peer();
        let (tx, rx) = flume::unbounded();
        let mut session = Session::default();
        for enabled in [true, false] {
            session
                .handle(Command::Shuffle(enabled), &mut peer, &tx)
                .unwrap();
            assert_eq!(read_command(&mut server), command("activate"));
            assert_eq!(
                read_command(&mut server),
                field_command("set_shuffle", "enabled", enabled)
            );
            assert!(rx.is_empty());
        }
    }

    #[test]
    fn queue_broadcast_triggers_all_available_query_and_only_query_response_publishes() {
        let (mut peer, mut server) = mock_peer();
        let (tx, rx) = flume::unbounded();
        let mut session = Session::default();
        session.playback.active = true;
        let entry = json!({"uid":"native","source":"queue","item":item(A)});
        session
            .observe(
                &json!({"type":"queue_changed","previous":[],"upcoming":vec![entry.clone();10]}),
                &mut peer,
                &tx,
            )
            .unwrap();
        assert!(
            rx.is_empty(),
            "a capped broadcast is not the native queue snapshot"
        );
        let query: Value = serde_json::from_str(server.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(
            query,
            json!({"type":"command","command":"get_queue","limit":0})
        );
        assert!(session.queue_query_pending);
        session.observe(&json!({"type":"queue_changed","previous":[entry.clone()],"upcoming":vec![entry;24]}), &mut peer, &tx).unwrap();
        assert!(matches!(rx.recv().unwrap(), EngineEvent::NativeQueue(queue)
            if queue.upcoming.len() == 24 && queue.previous.len() == 1 && !queue.complete_catalog_context));
        assert!(!session.queue_query_pending);
        session.playback.active = false;
        session
            .observe(
                &json!({"type":"queue_changed","upcoming":[{"item":item(B)}]}),
                &mut peer,
                &tx,
            )
            .unwrap();
        assert!(
            rx.is_empty(),
            "global queue updates must not replace the last local queue"
        );
    }

    #[test]
    fn sdk_options_volume_actions_and_position_are_typed_not_raw_payloads() {
        let (tx, rx) = flume::unbounded();
        let mut state = Playback::default();
        for (wire, expected) in [
            ("off", RepeatMode::Off),
            ("track", RepeatMode::One),
            ("context", RepeatMode::All),
        ] {
            state.update(&json!({"type":"playback_state","is_active":true,"status":"playing","item":item(A),
                "context":{"uri":"spotify:playlist:0123456789abcdefghijkl"},
                "volume":42,"options":{"shuffle":true,"repeat":wire,"modes":{"PRIVATE-SENTINEL":"secret"}},
                "position":{"position_ms":1200,"timestamp_ms":1700000000123u64,"speed":1.5},
                "available_actions":{"pause":{},"seek_forward":{"step_ms":15000},"seek_backward":{"step_ms":5000},"PRIVATE-SENTINEL":{}}
            }), &tx);
            let events: Vec<_> = rx.drain().collect();
            assert!(events.iter().any(|event| matches!(event, EngineEvent::Options { shuffle:true, repeat } if *repeat == expected)));
            assert!(events
                .iter()
                .any(|event| matches!(event, EngineEvent::Volume { percent: 42 })));
            assert!(events.iter().any(|event| matches!(event, EngineEvent::Context { uri:Some(uri) } if uri.starts_with("spotify:playlist:"))));
            assert!(events.iter().any(|event| matches!(event, EngineEvent::PositionAnchor { uri, anchor }
                if uri == A && *anchor == PositionAnchor { position_ms:1200, timestamp_ms:1700000000123, speed:1.5 })));
            assert!(events.iter().any(|event| matches!(event, EngineEvent::AvailableActions {
                actions, seek_forward_step_ms:Some(15000), seek_backward_step_ms:Some(5000)
            } if actions == &vec!["pause".to_owned(), "seek_forward".to_owned(), "seek_backward".to_owned()])));
            assert!(!serde_json::to_string(&events)
                .unwrap()
                .contains("PRIVATE-SENTINEL"));
        }
        state.update(
            &json!({"type":"options_changed","options":{"shuffle":false,"repeat":"off"}}),
            &tx,
        );
        state.update(&json!({"type":"volume_changed","volume":75}), &tx);
        let events: Vec<_> = rx.drain().collect();
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Options {
                shuffle: false,
                repeat: RepeatMode::Off
            }
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, EngineEvent::Volume { percent: 75 })));
    }

    #[test]
    fn inactive_global_state_cannot_replace_local_item_context_position_or_options() {
        let (tx, rx) = flume::unbounded();
        let mut state = Playback::default();
        state.update(
            &json!({"type":"playback_state","is_active":true,"status":"playing",
            "item":item(A),"context":{"uri":A},"position":{"position_ms":42}}),
            &tx,
        );
        rx.drain().for_each(drop);
        state.update(&json!({"type":"playback_state","is_active":false,"status":"playing",
            "item":item(B),"context":{"uri":B},"volume":99,"options":{"shuffle":true,"repeat":"track"},
            "position":{"position_ms":9999,"timestamp_ms":9999,"speed":2.0}}), &tx);
        assert_eq!(state.metadata.as_ref().unwrap().uri, A);
        assert_eq!(state.context.as_deref(), Some(A));
        assert_eq!(state.position, 42);
        assert!(rx
            .drain()
            .all(|event| matches!(event, EngineEvent::LocallyInactive)));
        state.update(
            &json!({"type":"command_result","is_active":true,"status":"playing","item":item(B)}),
            &tx,
        );
        assert!(rx.is_empty());
        assert!(!state.playing(A));
        assert!(!state.playing(B));
    }

    #[test]
    fn direct_episode_play_uses_sdk_playable_uri_path_and_state_query_is_explicit() {
        let (player, commands) = test_engine(READY);
        let episode = "spotify:episode:0123456789abcdefghijkl";
        player.play_track_at(episode.into(), 1234).unwrap();
        assert!(
            matches!(commands.recv().unwrap(), Command::Context(uri, 1234, false) if uri == episode)
        );
        player.request_state().unwrap();
        assert!(matches!(commands.recv().unwrap(), Command::State));
    }

    #[test]
    fn only_local_idle_stops_and_invalid_anchors_do_not_fabricate_corrections() {
        let (tx, rx) = flume::unbounded();
        let mut state = Playback::default();
        let playing =
            json!({"type":"playback_state","is_active":true,"status":"playing","item":item(A)});
        state.update(&playing, &tx);
        rx.drain().for_each(drop);
        state.update(&json!({"type":"position_sync","position":{"position_ms":10,"timestamp_ms":123,"speed":-1.0}}), &tx);
        assert!(rx.is_empty());
        state.update(
            &json!({"type":"playback_state","is_active":false,"status":"idle","item":item(B)}),
            &tx,
        );
        assert_eq!(state.metadata.as_ref().unwrap().uri, A);
        assert!(rx
            .drain()
            .all(|event| matches!(event, EngineEvent::LocallyInactive)));
        state.update(&playing, &tx);
        rx.drain().for_each(drop);
        state.update(&json!({"type":"playback_changed","status":"idle"}), &tx);
        assert!(rx
            .drain()
            .any(|event| matches!(event, EngineEvent::Stopped)));
        assert!(state.metadata.is_none());
    }
}
