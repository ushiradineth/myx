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
use rand::seq::SliceRandom;
use serde_json::{json, Value};
use tungstenite::{protocol::WebSocketConfig, Message, WebSocket};

use super::protocol::{ObservedMetadata as TrackMetadata, PlayerEvent as EngineEvent};
use crate::audio::VisBands;
use crate::config::SoloistConfig;
use crate::domain::RepeatMode;

const WAITING: u8 = 0;
const READY: u8 = 1;
const DISCONNECTED: u8 = 2;
const STOPPED: u8 = 3;
const MAILBOX_CAPACITY: usize = 64;
const WINDOW: usize = 4;
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
    Context(String, Option<String>, u32, bool),
    Tracks(Vec<String>, Option<String>, u32, bool),
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
}

impl Command {
    fn is_load(&self) -> bool {
        matches!(self, Self::Context(..) | Self::Tracks(..))
    }
}

/// One deferred load while unauthenticated, never an accumulating playback queue.
#[derive(Default)]
struct Intents {
    load: Option<Command>,
}

impl Intents {
    fn drain(
        &mut self,
        rx: &flume::Receiver<Command>,
        authenticated: bool,
        stop_pending: &AtomicBool,
    ) -> VecDeque<Command> {
        let mut batch = VecDeque::new();
        if authenticated {
            if let Some(load) = self.load.take() {
                batch.push_back(load);
            }
        }
        for cmd in rx.try_iter().take(MAILBOX_CAPACITY) {
            match cmd {
                Command::Stop => {
                    self.load = None;
                    batch.clear();
                    batch.push_back(Command::Stop);
                }
                cmd if cmd.is_load() => {
                    if authenticated {
                        // A newer selection replaces older selections, not volume
                        // or repeat preferences. Stop remains an ordering barrier.
                        batch.retain(|queued| {
                            matches!(
                                queued,
                                Command::Stop | Command::Volume(_) | Command::Repeat(_)
                            )
                        });
                        batch.push_back(cmd);
                    } else {
                        self.load = Some(cmd);
                    }
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
                // Controls accepted just before authentication was lost must
                // not become delayed controls on a later authenticated session.
                _ => {}
            }
        }
        if stop_pending.swap(false, Ordering::AcqRel) {
            self.cancel();
            batch.clear();
            batch.push_back(Command::Stop);
        }
        batch
    }

    fn cancel(&mut self) {
        self.load = None;
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
        let processor = Box::new(crate::audio::managed::ManagedProcessor::new(
            equalizer.clone(),
            bands.clone(),
            spectrum_enabled.clone(),
        )) as Box<dyn soloist_runtime::pcm::AudioProcessor>;
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
                if result.is_err() {
                    status(
                        &tx,
                        "Soloist worker stopped after a runtime or transport failure",
                    );
                }
                emit(&tx, EngineEvent::Stopped);
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
        self.has_local_dsp().then(|| self.audio.stats())
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
            phase == READY || command.is_load() || matches!(&command, Command::Stop),
            "Soloist is waiting for authentication; only explicit playback loads or Stop can be queued"
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
        if let Some(track) = &track {
            validate_uri(track, true)?;
        }
        ensure!(
            position == 0 || track.is_some() || is_item(&uri),
            "Soloist positioned context playback requires a track URI"
        );
        self.submit(Command::Context(uri, track, position, shuffle))
    }

    pub fn play_tracks(
        &self,
        tracks: Vec<String>,
        start: Option<String>,
        position: u32,
        shuffle: bool,
    ) -> Result<()> {
        ensure!(!tracks.is_empty(), "Soloist requires at least one track");
        for uri in &tracks {
            validate_uri(uri, true)?;
        }
        if let Some(start) = &start {
            validate_uri(start, true)?;
            ensure!(
                tracks.contains(start),
                "Soloist start track is not in the supplied list"
            );
        }
        self.submit(Command::Tracks(tracks, start, position, shuffle))
    }

    pub fn play_track_at(&self, uri: String, position: u32) -> Result<()> {
        self.play_tracks(vec![uri], None, position, false)
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
    let library_path = if let Some(path) = &config.library_path {
        path.clone()
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
    Ok(soloist_runtime::Config {
        sdk: config.resolve(config.sdk.as_ref(), "assets/soloist")?,
        runner: config.resolve(config.runner.as_ref(), "deps/elfuse/bin/elfuse")?,
        sysroot: config.resolve(config.sysroot.as_ref(), "assets/root")?,
        library_path,
        audio_tools: config.resolve(config.audio_tools.as_ref(), "deps/audio")?,
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
    })
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
            .max_message_size(Some(131072))
            .max_frame_size(Some(131072));
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
        ) {
            return false;
        }
        let was_active = self.active;
        if kind == "playback_state" {
            self.active = value["is_active"].as_bool().unwrap_or(false);
        } else if let Some(active) = value["is_active"].as_bool() {
            self.active = active;
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
            .and_then(|n| u32::try_from(n).ok());
        if let Some(position) = position {
            self.position = position;
        }
        if !was_active && self.active && kind == "device_changed" {
            // A device role change alone does not confirm local playback of
            // the cached global track. Wait for a fresh playback notification.
            self.status.clear();
        }
        if !self.active || self.status == "idle" || self.metadata.is_none() {
            if (was_active && !self.active) || self.published.take().is_some() {
                emit(tx, EngineEvent::Stopped);
            }
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
                    "paused" | "buffering" => Some(EngineEvent::Paused {
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
            if position.is_some() {
                emit(
                    tx,
                    EngineEvent::PositionCorrection {
                        uri: item.uri.clone(),
                        position_ms: self.position,
                    },
                );
            }
        }
        previous_uri.as_deref() != self.metadata.as_ref().map(|m| m.uri.as_str())
    }

    fn playing(&self, uri: &str) -> bool {
        self.active
            && self.status == "playing"
            && self.metadata.as_ref().is_some_and(|m| m.uri == uri)
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

fn queue_entries(value: &Value) -> Vec<(String, String)> {
    value["upcoming"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let item = &entry["item"];
            let uri = item["uri"].as_str()?;
            validate_uri(uri, false).ok()?;
            Some((
                uri.into(),
                item["decorations"]["identity"]["name"]
                    .as_str()
                    .unwrap_or("")
                    .into(),
            ))
        })
        .collect()
}

struct DeferredSeek {
    uri: String,
    position: u32,
    deadline: Instant,
}

#[derive(Debug, PartialEq, Eq)]
struct PendingTrack {
    uri: String,
    order: usize,
}

#[derive(Default)]
struct Session {
    playback: Playback,
    pending: VecDeque<PendingTrack>,
    next_order: usize,
    sent: VecDeque<String>,
    explicit: bool,
    target: Option<String>,
    seek: Option<DeferredSeek>,
    repeat: RepeatMode,
    queue: Vec<(String, String)>,
    loaded_locally: bool,
}

impl Session {
    fn context_repeat(&self) -> bool {
        self.repeat == RepeatMode::All && !self.explicit
    }

    fn repeat_commands(&self) -> [Value; 2] {
        [
            field_command(
                "set_repeat_track",
                "enabled",
                self.repeat == RepeatMode::One,
            ),
            field_command("set_repeat_context", "enabled", self.context_repeat()),
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
        self.pending.clear();
        self.sent.clear();
        self.target = None;
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
        self.loaded_locally = true;
        self.target = is_item(uri).then(|| uri.into());
        self.defer_seek(uri, position);
        // A later state confirmation, never the previous track's playing flag,
        // authorizes the refill and deferred seek.
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
            Command::Context(context, track, position, shuffle) => {
                self.pending.clear();
                self.sent.clear();
                self.explicit = false;
                self.target = None;
                self.seek = None;
                // Cached Connect state can survive a runtime restart without a
                // loaded local audio context. Fresh requests always need a URI.
                let matching = self.loaded_locally
                    && track.is_some()
                    && self.playback.active
                    && self.playback.context.as_deref() == Some(&context)
                    && track.as_ref().is_none_or(|uri| {
                        self.playback
                            .metadata
                            .as_ref()
                            .is_some_and(|m| &m.uri == uri)
                    });
                if matching {
                    peer.command("activate")?;
                    self.options(peer, shuffle)?;
                    peer.command("play")?;
                    if let Some(uri) = self.playback.metadata.as_ref().map(|m| m.uri.clone()) {
                        self.defer_seek(&uri, position);
                    }
                    self.playback.status.clear();
                } else if let Some(track) = track {
                    status(tx, "Soloist cannot start a context at a track; resuming the explicit track without its context position");
                    self.load(peer, &track, position, false)?;
                } else {
                    self.load(peer, &context, position, shuffle)?;
                }
            }
            Command::Tracks(tracks, start, position, shuffle) => {
                self.next_order = tracks.len();
                let mut tracks = ordered_tracks(tracks, start.as_deref(), shuffle);
                let first = tracks
                    .pop_front()
                    .ok_or_else(|| anyhow!("Soloist track list is empty"))?;
                self.pending = tracks;
                self.sent.clear();
                self.explicit = true;
                if self.repeat == RepeatMode::All {
                    status(tx, "Soloist cannot repeat explicit track lists; list repeat is disabled, and the context repeat preference is retained");
                }
                if !self.pending.is_empty() || !self.queue.is_empty() {
                    status(tx, "Soloist streams explicit lists through a small queue window; existing SDK queue entries cannot be cleared");
                }
                self.load(peer, &first.uri, position, false)?;
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
                self.publish_queue(tx);
                if self.playback.active {
                    peer.command("pause")?;
                }
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
                if mode == RepeatMode::All && self.explicit {
                    status(tx, "Soloist cannot repeat explicit track lists; list repeat is disabled, and the context repeat preference is retained");
                }
                peer.command("activate")?;
                for command in self.repeat_commands() {
                    peer.send(command)?;
                }
            }
            Command::Shuffle(on) => {
                peer.command("activate")?;
                if self.explicit {
                    if on {
                        self.pending.make_contiguous().shuffle(&mut rand::rng());
                    } else {
                        self.pending
                            .make_contiguous()
                            .sort_by_key(|track| track.order);
                    }
                    peer.send(field_command("set_shuffle", "enabled", false))?;
                    if !self.sent.is_empty() || !on {
                        status(tx, "Soloist list shuffle applies to unsent tracks; already queued tracks keep their order");
                    }
                } else {
                    peer.send(field_command("set_shuffle", "enabled", on))?;
                }
            }
            Command::Add(uri) => {
                self.pending.push_back(PendingTrack {
                    uri,
                    order: self.next_order,
                });
                self.next_order = self.next_order.saturating_add(1);
                self.publish_queue(tx);
            }
            Command::Queue => {
                peer.send(field_command("get_queue", "limit", 64))?;
            }
        }
        Ok(())
    }

    fn publish_queue(&self, tx: &flume::Sender<EngineEvent>) {
        let mut entries: Vec<_> = self
            .queue
            .iter()
            .map(|(uri, title)| {
                (
                    if title.is_empty() {
                        uri.clone()
                    } else {
                        title.clone()
                    },
                    uri.clone(),
                )
            })
            .collect();
        entries.extend(
            self.pending
                .iter()
                .map(|track| (track.uri.clone(), track.uri.clone())),
        );
        emit(tx, EngineEvent::QueueChanged { entries });
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
            )
        ) {
            return Ok(());
        }
        let changed = self.playback.update(value, tx);
        observe_authentication(value, true, self.playback.active, tx);
        if changed || value["type"] == "track_changed" {
            if let Some(uri) = self.playback.metadata.as_ref().map(|m| &m.uri) {
                if let Some(index) = self.sent.iter().position(|queued| queued == uri) {
                    drop(self.sent.drain(..=index));
                    self.target = Some(uri.clone());
                }
            }
        }
        if value["type"] == "queue_changed" {
            self.queue = queue_entries(value);
            self.publish_queue(tx);
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

    fn tick(&mut self, peer: &mut Peer, tx: &flume::Sender<EngineEvent>) -> Result<()> {
        if self
            .seek
            .as_ref()
            .is_some_and(|seek| Instant::now() >= seek.deadline)
        {
            self.seek = None;
            status(tx, "Soloist timed out waiting for the requested track to play; deferred seek cancelled");
        }
        if let Some(uri) = self.next_refill() {
            // At most one queue write per worker iteration; no list-sized burst.
            peer.send(field_command("add_to_queue", "uri", uri.clone()))?;
            self.queue.push((uri, String::new()));
            self.publish_queue(tx);
        }
        Ok(())
    }

    fn next_refill(&mut self) -> Option<String> {
        let may_refill = self.playback.active
            && self.playback.status == "playing"
            && self
                .target
                .as_ref()
                .is_none_or(|uri| self.playback.playing(uri));
        if may_refill && self.sent.len() < WINDOW {
            if let Some(track) = self.pending.pop_front() {
                let uri = track.uri;
                // Reserve before the write: an ambiguous failed write must not
                // be retried and duplicated after reconnect.
                self.sent.push_back(uri.clone());
                return Some(uri);
            }
        }
        None
    }

    fn disconnected(&mut self, tx: &flume::Sender<EngineEvent>) {
        self.loaded_locally = false;
        self.playback.reset_connection();
        self.seek = None;
        // A write can have succeeded before disconnect. Replaying it would
        // duplicate playback or queue entries, so require a new explicit load.
        if !self.pending.is_empty() || !self.sent.is_empty() {
            status(tx, "Soloist connection lost; unsent list tracks cancelled to avoid duplicate queue entries");
        }
        self.pending.clear();
        self.sent.clear();
        self.target = None;
        // Losing the socket does not change the SDK's track-only context. Keep
        // rejecting list repeat until a fresh context load changes that mode.
        self.queue.clear();
    }
}

fn ordered_tracks(
    tracks: Vec<String>,
    start: Option<&str>,
    shuffle: bool,
) -> VecDeque<PendingTrack> {
    let index = start
        .and_then(|uri| tracks.iter().position(|track| track == uri))
        .unwrap_or(0);
    let mut tracks: VecDeque<_> = tracks
        .into_iter()
        .enumerate()
        .skip(index)
        .map(|(order, uri)| PendingTrack { uri, order })
        .collect();
    if shuffle && tracks.len() > 1 {
        tracks.make_contiguous()[1..].shuffle(&mut rand::rng());
    }
    tracks
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
        .map_err(|_| anyhow!("Soloist runtime configuration is invalid or incomplete"))?;
    let owned = soloist_runtime::Runtime::start_processed(
        runtime_config,
        control.audio.control.clone(),
        processor,
    )
    .map_err(|_| anyhow!("Soloist managed runtime failed to start; check provisioned assets"))?;
    let endpoint = owned.endpoint();
    // Retain only the validated public discovery ID, never the pairing body.
    *control
        .device_id
        .lock()
        .map_err(|_| anyhow!("Soloist identity lock failed"))? = Some(owned.device_id()?);
    let mut runtime = owned;
    let result = transport(&endpoint, &mut runtime, volume, rx, tx, control, started);
    // Always check owned cleanup, even after a transport failure.
    let cleanup = runtime
        .shutdown()
        .map_err(|_| anyhow!("Soloist owned runtime cleanup failed"));
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
    let mut intents = Intents::default();
    let mut failures = 0u32;
    let mut reconnecting = false;
    let mut initial_volume = Some(volume);
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
                    // Drain a bounded batch even before authentication, so Stop
                    // can cancel the one latest deferred selection immediately.
                    let batch = intents.drain(&rx, authenticated, stop_pending);
                    for cmd in batch {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        if authenticated {
                            let flush = matches!(
                                &cmd,
                                Command::Context(..)
                                    | Command::Tracks(..)
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
                            session.publish_queue(tx);
                        }
                    }
                    if authenticated && !stop.load(Ordering::Acquire) {
                        session.tick(&mut peer, tx)?;
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
                                    if session.playback.active {
                                        emit(tx, EngineEvent::Stopped);
                                    }
                                    session.playback.reset_connection();
                                    session.loaded_locally = false;
                                    session.seek = None;
                                    if auth_changed {
                                        status(tx, "Soloist is waiting for authentication");
                                    }
                                } else if newly_authenticated {
                                    status(tx, "Soloist is authenticated");
                                    if let Some(volume) = initial_volume.take() {
                                        peer.send(field_command("set_volume", "volume", volume))?;
                                    }
                                    peer.command("get_state")?;
                                    peer.send(field_command("get_queue", "limit", 64))?;
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
                                session.pending.clear();
                                session.sent.clear();
                                status(
                                    tx,
                                    "Soloist rejected a command; playback was not confirmed and pending list refills were cancelled",
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
                session.disconnected(tx);
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
        intents.cancel();
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
        assert_eq!(
            queue_entries(&json!({"upcoming":[{"item":item(A)}, {"uri":B}]})),
            vec![(A.into(), "Song".into())]
        );
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
            .any(|event| matches!(event, EngineEvent::Stopped)));
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
            .any(|event| matches!(event, EngineEvent::Stopped)));
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
            .any(|event| matches!(event, EngineEvent::Paused { .. })));
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
    fn lists_start_at_requested_item_and_keep_it_first_when_shuffled() {
        assert_eq!(
            ordered_tracks(vec![A.into(), B.into()], Some(B), false),
            VecDeque::from([PendingTrack {
                uri: B.to_owned(),
                order: 1
            }])
        );
        let shuffled = ordered_tracks(vec![A.into(), B.into(), A.into()], None, true);
        assert_eq!(shuffled.front().map(|track| track.uri.as_str()), Some(A));
        assert_eq!(shuffled.len(), 3);
    }

    #[test]
    fn refill_is_bounded_and_waits_for_the_local_target_to_play() {
        let mut session = Session {
            pending: ordered_tracks(vec![B.into(); 20], None, false),
            target: Some(A.into()),
            ..Session::default()
        };
        assert!(session.next_refill().is_none());
        session.playback.metadata = metadata(&item(B));
        session.playback.active = true;
        session.playback.status = "playing".into();
        assert!(session.next_refill().is_none());
        session.playback.metadata = metadata(&item(A));
        for _ in 0..WINDOW {
            assert_eq!(session.next_refill().as_deref(), Some(B));
        }
        assert!(session.next_refill().is_none());
        assert_eq!(session.pending.len(), 20 - WINDOW);
        session.sent.pop_front();
        assert_eq!(session.next_refill().as_deref(), Some(B));
        assert_eq!(session.sent.len(), WINDOW);
    }

    #[test]
    fn logical_queue_keeps_user_additions_after_the_pending_list() {
        let (tx, rx) = flume::unbounded();
        let mut session = Session {
            queue: vec![(A.into(), "Already queued".into())],
            pending: ordered_tracks(vec![B.into(), A.into()], None, false),
            ..Session::default()
        };
        session.pending.push_back(PendingTrack {
            uri: B.into(),
            order: 2,
        });
        session.publish_queue(&tx);
        match rx.recv().unwrap() {
            EngineEvent::QueueChanged { entries } => assert_eq!(
                entries,
                vec![
                    ("Already queued".into(), A.into()),
                    (B.into(), B.into()),
                    (A.into(), A.into()),
                    (B.into(), B.into()),
                ]
            ),
            _ => panic!("expected logical queue event"),
        }
        session.pending.make_contiguous().reverse();
        session
            .pending
            .make_contiguous()
            .sort_by_key(|track| track.order);
        assert_eq!(
            session
                .pending
                .iter()
                .map(|track| track.uri.as_str())
                .collect::<Vec<_>>(),
            vec![B, A, B]
        );
    }

    #[test]
    fn disconnect_cancels_ambiguous_queue_writes_and_deferred_seek_but_not_repeat_preference() {
        let (tx, _rx) = flume::unbounded();
        let mut session = Session {
            repeat: RepeatMode::All,
            pending: ordered_tracks(vec![B.into()], None, false),
            ..Session::default()
        };
        session.sent.push_back(A.into());
        session.defer_seek(A, 5000);
        session.disconnected(&tx);
        assert!(session.seek.is_none());
        assert!(session.pending.is_empty());
        assert!(session.sent.is_empty());
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
        for _ in 0..MAILBOX_CAPACITY {
            engine.play_track_at(A.into(), 0).unwrap();
        }
        assert!(engine.play_track_at(A.into(), 0).is_err());
        engine.stop();
        assert!(!engine.stopping.load(Ordering::Acquire));
        let mut intents = Intents::default();
        let batch = intents.drain(&rx, false, &engine.stop_pending);
        assert!(matches!(batch.front(), Some(Command::Stop)));
        assert_eq!(batch.len(), 1);
        assert!(intents.load.is_none());
        assert!(intents.drain(&rx, true, &engine.stop_pending).is_empty());
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
        engine.play_track_at(A.into(), 0).unwrap();
        engine.shutdown().unwrap();
        assert!(engine.stopping.load(Ordering::Acquire));
        assert!(completed.load(Ordering::Acquire));
        assert!(engine.worker.lock().unwrap().is_none());
        assert!(engine.play_track_at(A.into(), 0).is_err());
        engine.shutdown().unwrap();
    }

    #[test]
    fn unauthenticated_intents_keep_only_the_latest_load_and_stop_cancels_it() {
        let (tx, rx) = flume::bounded(MAILBOX_CAPACITY);
        let stop_pending = AtomicBool::new(false);
        let mut intents = Intents::default();
        tx.send(Command::Tracks(vec![A.into()], None, 0, false))
            .unwrap();
        tx.send(Command::Tracks(vec![B.into()], None, 5000, false))
            .unwrap();
        // A control queued just before auth was lost is not replayed later.
        tx.send(Command::Play).unwrap();
        assert!(intents.drain(&rx, false, &stop_pending).is_empty());
        assert!(
            matches!(&intents.load, Some(Command::Tracks(tracks, _, 5000, _)) if tracks == &vec![B.to_owned()])
        );
        tx.send(Command::Stop).unwrap();
        let batch = intents.drain(&rx, false, &stop_pending);
        assert_eq!(batch.len(), 1);
        assert!(matches!(batch.front(), Some(Command::Stop)));
        assert!(intents.load.is_none());
        assert!(intents.drain(&rx, true, &stop_pending).is_empty());

        tx.send(Command::Context(A.into(), None, 0, false)).unwrap();
        assert!(intents.drain(&rx, false, &stop_pending).is_empty());
        // Stop queued immediately before auth succeeds still wins over the
        // deferred selection, so no SDK play command escapes the auth boundary.
        tx.send(Command::Stop).unwrap();
        let batch = intents.drain(&rx, true, &stop_pending);
        assert_eq!(batch.len(), 1);
        assert!(matches!(batch.front(), Some(Command::Stop)));

        tx.send(Command::Tracks(vec![B.into()], None, 0, false))
            .unwrap();
        assert!(intents.drain(&rx, false, &stop_pending).is_empty());
        let batch = intents.drain(&rx, true, &stop_pending);
        assert!(
            matches!(batch.front(), Some(Command::Tracks(tracks, _, _, _)) if tracks == &vec![B.to_owned()])
        );
        assert!(intents.drain(&rx, true, &stop_pending).is_empty());
    }

    #[test]
    fn ready_commands_are_drained_in_bounded_coalesced_batches() {
        let (tx, rx) = flume::unbounded();
        let stop_pending = AtomicBool::new(false);
        let mut intents = Intents::default();
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
        tx.send(Command::Tracks(vec![A.into()], None, 0, false))
            .unwrap();
        tx.send(Command::Tracks(vec![B.into()], None, 0, false))
            .unwrap();
        let batch = intents.drain(&rx, true, &stop_pending);
        assert_eq!(batch.len(), 1);
        assert!(
            matches!(batch.front(), Some(Command::Tracks(tracks, _, _, _)) if tracks == &vec![B.to_owned()])
        );
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
    fn cached_context_requires_an_explicit_load_unless_resuming_a_local_selection() {
        for (loaded_locally, selected_track, expected_uri) in [
            (false, None, Some(A)),
            (false, Some(A), Some(A)),
            (true, None, Some(A)),
            (true, Some(A), None),
        ] {
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
            let mut session = Session {
                loaded_locally,
                ..Session::default()
            };
            session.playback.update(
                &json!({"type":"playback_state","is_active":true,"status":"playing",
                    "item":item(A),"context":{"uri":A}}),
                &tx,
            );
            session
                .handle(
                    Command::Context(A.into(), selected_track.map(str::to_owned), 0, false),
                    &mut peer,
                    &tx,
                )
                .unwrap();
            assert_eq!(
                reader.join().unwrap().as_deref(),
                expected_uri,
                "loaded_locally={loaded_locally}, selected_track={selected_track:?}"
            );
        }
    }

    #[test]
    fn stop_cancels_unsent_work_without_fabricating_playback_state() {
        let mut session = Session {
            pending: ordered_tracks(vec![B.into()], None, false),
            ..Session::default()
        };
        session.sent.push_back(A.into());
        session.target = Some(A.into());
        session.defer_seek(A, 5000);
        session.playback.active = true;
        session.playback.status = "playing".into();
        session.cancel_pending();
        assert!(session.pending.is_empty());
        assert!(session.sent.is_empty());
        assert!(session.target.is_none());
        assert!(session.seek.is_none());
        assert!(session.playback.active);
        assert_eq!(session.playback.status, "playing");
    }

    #[test]
    fn explicit_lists_never_repeat_the_first_sdk_track_as_a_fake_list_repeat() {
        let mut session = Session {
            repeat: RepeatMode::All,
            ..Session::default()
        };
        assert!(session.context_repeat());
        session.explicit = true;
        assert!(!session.context_repeat());
        session.cancel_pending();
        assert!(!session.context_repeat());
        let (tx, _rx) = flume::unbounded();
        session.disconnected(&tx);
        assert!(!session.context_repeat());
        // Returning to context playback restores the actual user preference.
        session.explicit = false;
        assert!(session.context_repeat());
        session.repeat = RepeatMode::Off;
        assert!(!session.context_repeat());
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
    fn repeat_modes_serialize_track_and_context_flags_and_preserve_list_limitations() {
        for explicit in [false, true] {
            for (mode, track, context) in [
                (RepeatMode::Off, false, false),
                (RepeatMode::One, true, false),
                (RepeatMode::All, false, !explicit),
            ] {
                let session = Session {
                    repeat: mode,
                    explicit,
                    ..Session::default()
                };
                assert_eq!(
                    session.repeat_commands(),
                    [
                        json!({"type":"command","command":"set_repeat_track","enabled":track}),
                        json!({"type":"command","command":"set_repeat_context","enabled":context}),
                    ]
                );
                let (player, commands) = test_engine(READY);
                player.set_repeat(mode).unwrap();
                assert!(
                    matches!(commands.recv().unwrap(), Command::Repeat(received) if received == mode)
                );
            }
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
}
