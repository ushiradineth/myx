//! Bounded local clients. The daemon owns authorization, SDK and playback.

mod connection;
pub use connection::Connection;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};

use crate::audio::{EqualizerSettings, VisBands};
use crate::cli::{Cli, DaemonCommand, Mode, ServiceCommand};
use crate::domain::{PlayerCommand, RepeatMode, Snapshot};
use crate::ipc::protocol::{Operation, Payload, QueuePage};
use crate::soloist::protocol::{ObservedMetadata, PlayerEvent};
use connection::CONTROL_TIMEOUT;

const EVENT_LIMIT: usize = 64;

/// A catalog client handle with no HTTP client, tokens or independent auth flow.
pub struct WebApi {
    connection: Connection,
}
impl WebApi {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }
    pub fn connection(&self) -> Connection {
        self.connection.clone()
    }
    pub fn is_available(&self) -> bool {
        self.connection
            .snapshot()
            .is_ok_and(|snapshot| snapshot.web_api_ready)
    }
}

pub struct Player {
    connection: Connection,
    pub bands: Arc<Mutex<VisBands>>,
    cancelled: Arc<AtomicBool>,
    spectrum: Arc<AtomicBool>,
    events: Arc<EventSink>,
    queue_requests: flume::Sender<(Option<u64>, usize)>,
    queue_result: Arc<Mutex<Option<Result<QueuePage>>>>,
}

/// All producers on this handle share a gate. Even an unbounded UI channel
/// receives at most 64 queued events from us, with no blocking sends.
struct EventSink {
    sender: flume::Sender<PlayerEvent>,
    gate: Mutex<()>,
}
impl EventSink {
    fn send(&self, events: Vec<PlayerEvent>) -> bool {
        let Ok(_gate) = self.gate.lock() else {
            return false;
        };
        let limit = self
            .sender
            .capacity()
            .unwrap_or(EVENT_LIMIT)
            .min(EVENT_LIMIT);
        if self.sender.len().saturating_add(events.len()) > limit {
            return false;
        }
        for event in events {
            if self.sender.try_send(event).is_err() {
                return false;
            }
        }
        true
    }
    fn status(&self, message: &'static str) {
        if !self.send(vec![PlayerEvent::BackendStatus {
            message: message.into(),
        }]) {
            // Void legacy controls must still surface errors if the UI stalls.
            eprintln!("myx: {message}");
        }
    }
}

impl Player {
    pub fn connect(connection: Connection, events: flume::Sender<PlayerEvent>) -> Result<Self> {
        let initial = connection.snapshot()?;
        let bands = VisBands::shared();
        let cancelled = Arc::new(AtomicBool::new(false));
        let spectrum = Arc::new(AtomicBool::new(false));
        let events = Arc::new(EventSink {
            sender: events,
            gate: Mutex::new(()),
        });
        let watcher = (
            connection.clone(),
            bands.clone(),
            cancelled.clone(),
            spectrum.clone(),
            events.clone(),
        );
        let (queue_requests, queue_received) = flume::bounded::<(Option<u64>, usize)>(1);
        let queue_result = Arc::new(Mutex::new(None));
        let queue_connection = connection.clone();
        let queue_cancelled = cancelled.clone();
        let queue_output = queue_result.clone();
        thread::Builder::new()
            .name("myx-queue-view".into())
            .spawn(move || {
                while !queue_cancelled.load(Ordering::Acquire) {
                    let Ok((revision, offset)) =
                        queue_received.recv_timeout(Duration::from_millis(100))
                    else {
                        continue;
                    };
                    let result = queue_connection.queue_page(revision, offset, 10, false, true);
                    if !queue_cancelled.load(Ordering::Acquire) {
                        *queue_output
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
                    }
                }
            })
            .map_err(|_| anyhow::anyhow!("Cannot start queue viewport worker"))?;
        thread::Builder::new()
            .name("myx-watch".into())
            .spawn(move || {
                watch_loop(
                    watcher.0, watcher.1, watcher.2, watcher.3, watcher.4, initial,
                );
            })
            .map_err(|_| anyhow::anyhow!("Cannot start daemon watcher"))?;
        Ok(Self {
            connection,
            bands,
            cancelled,
            spectrum,
            events,
            queue_requests,
            queue_result,
        })
    }

    pub fn set_spectrum_enabled(&self, enabled: bool) {
        self.spectrum.store(enabled, Ordering::Release);
        if !enabled {
            clear_bands(&self.bands);
        }
    }

    fn command(&self, command: PlayerCommand) -> Result<()> {
        accepted(
            self.connection
                .request(Operation::Player(command), CONTROL_TIMEOUT)?,
        )
    }
    pub fn play_context(&self, uri: impl Into<String>, shuffle: bool) -> Result<()> {
        self.play_context_at(uri.into(), None, 0, shuffle)
    }
    pub fn play_context_at(
        &self,
        context: String,
        track: Option<String>,
        position_ms: u32,
        shuffle: bool,
    ) -> Result<()> {
        self.command(PlayerCommand::PlayContext {
            uri: context,
            track,
            position_ms,
            shuffle,
        })
    }
    pub fn play_tracks(
        &self,
        tracks: Vec<String>,
        start: Option<String>,
        position_ms: u32,
        shuffle: bool,
    ) -> Result<()> {
        self.command(PlayerCommand::PlayTracks {
            uris: tracks,
            start,
            position_ms,
            shuffle,
        })
    }
    pub fn play_track_at(&self, uri: String, position_ms: u32) -> Result<()> {
        self.play_context_at(uri, None, position_ms, false)
    }
    pub fn play_context_at_position(
        &self,
        context: String,
        track: Option<String>,
        row_position: u32,
        position_ms: u32,
        shuffle: bool,
    ) -> Result<()> {
        self.command(PlayerCommand::PlayContextAt {
            uri: context,
            track,
            row_position,
            position_ms,
            shuffle,
        })
    }
    pub fn play_tracks_at_index(
        &self,
        uris: Vec<String>,
        start_index: u32,
        position_ms: u32,
        shuffle: bool,
    ) -> Result<()> {
        self.command(PlayerCommand::PlayTracksAt {
            uris,
            start_index,
            position_ms,
            shuffle,
        })
    }
    pub fn play_liked(
        &self,
        start: Option<String>,
        source_position: Option<u32>,
        position_ms: u32,
        shuffle: bool,
    ) -> Result<()> {
        self.command(PlayerCommand::PlayLiked {
            start,
            source_position,
            position_ms,
            shuffle,
        })
    }
    pub fn play(&self) -> Result<()> {
        self.command(PlayerCommand::Play)
    }
    pub fn pause(&self) -> Result<()> {
        self.command(PlayerCommand::Pause)
    }
    pub fn toggle(&self) -> Result<()> {
        self.command(PlayerCommand::Toggle)
    }
    pub fn next(&self) -> Result<()> {
        self.command(PlayerCommand::Next)
    }
    pub fn prev(&self) -> Result<()> {
        self.command(PlayerCommand::Previous)
    }
    pub fn seek(&self, position_ms: u32) -> Result<()> {
        self.command(PlayerCommand::Seek { position_ms })
    }
    pub fn shuffle(&self, on: bool) -> Result<()> {
        self.command(PlayerCommand::Shuffle { enabled: on })
    }
    pub fn set_repeat(&self, mode: RepeatMode) -> Result<()> {
        self.command(PlayerCommand::Repeat { mode })
    }
    pub fn repeat(&self, on: bool) -> Result<()> {
        self.set_repeat(if on { RepeatMode::All } else { RepeatMode::Off })
    }
    pub fn set_volume(&self, volume: u16) -> Result<()> {
        let percent = ((u32::from(volume) * 100 + 32767) / 65535) as u8;
        self.command(PlayerCommand::Volume { percent })
    }
    pub fn set_equalizer(&self, settings: EqualizerSettings) {
        if self.command(PlayerCommand::Equalizer { settings }).is_err() {
            self.events
                .status("Equalizer update failed; inspect daemon status");
        }
    }
    pub fn stop(&self) {
        if self.command(PlayerCommand::Stop).is_err() {
            self.events
                .status("Playback stop failed; inspect daemon status");
        }
    }
    pub fn request_queue(&self) -> Result<()> {
        self.command(PlayerCommand::Queue)
    }
    /// Nonblocking viewport demand. One active request and one queued page at most.
    pub fn request_queue_page(&self, revision: Option<u64>, offset: usize) -> Result<()> {
        self.queue_requests
            .try_send((revision, offset))
            .map_err(|_| anyhow::anyhow!("Queue viewport request is busy"))
    }
    pub fn take_queue_page(&self) -> Option<Result<QueuePage>> {
        self.queue_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
    pub fn add_to_queue(&self, uri: String) -> Result<()> {
        self.command(PlayerCommand::Add { uri })
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.spectrum.store(false, Ordering::Release);
        clear_bands(&self.bands);
        // The watcher owns its subscription ID and does the final Unsubscribe
        // after its bounded in-flight Watch. Never join or stop owner/audio.
    }
}

fn accepted(payload: Payload) -> Result<()> {
    ensure!(
        matches!(payload, Payload::Accepted),
        "Invalid daemon control response"
    );
    Ok(())
}

fn clear_bands(bands: &Mutex<VisBands>) {
    if let Ok(mut bands) = bands.lock() {
        *bands = VisBands::new();
    }
}

fn update_bands(
    bands: &Mutex<VisBands>,
    snapshot: &Snapshot,
    enabled: &AtomicBool,
    connected: bool,
) {
    if let Ok(mut bands) = bands.try_lock() {
        *bands = VisBands::new();
        // Load under the bands lock: disabling and clearing cannot race with
        // an older watcher demand value that would restore stale telemetry.
        if connected && enabled.load(Ordering::Acquire) {
            if let Some(values) = &snapshot.spectrum {
                for (target, value) in bands.values.iter_mut().zip(values) {
                    *target = if value.is_finite() {
                        value.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                }
                bands.is_active = bands.values.iter().any(|value| *value > 0.0);
                bands.peak_envelope = bands.values.iter().copied().fold(1e-6, f32::max);
            }
        }
    }
}

fn watch_loop(
    connection: Connection,
    bands: Arc<Mutex<VisBands>>,
    cancelled: Arc<AtomicBool>,
    spectrum: Arc<AtomicBool>,
    events: Arc<EventSink>,
    initial: Snapshot,
) {
    let client_id = format!("{:032x}", rand::random::<u128>());
    let mut latest = initial;
    let mut delivered: Option<Snapshot> = None;
    let mut connected = true;
    while !cancelled.load(Ordering::Acquire) && !events.sender.is_disconnected() {
        update_bands(&bands, &latest, &spectrum, connected);
        if connected {
            let batch = snapshot_events(delivered.as_ref(), &latest);
            // Very small bounded channels can still consume the authoritative
            // model. Legacy supplemental events are optional there.
            let batch = if events
                .sender
                .capacity()
                .is_some_and(|capacity| capacity < batch.len())
            {
                vec![PlayerEvent::ReadModel(Box::new(latest.clone()))]
            } else {
                batch
            };
            if events.send(batch) {
                delivered = Some(latest.clone());
            }
        }
        let result = if connected {
            connection.watch(
                latest.sequence,
                &client_id,
                spectrum.load(Ordering::Acquire),
            )
        } else {
            // A new owner may restart its sequence at zero. Always obtain a
            // fresh status before resubscribing; commands are never replayed.
            connection.refresh_subscription()
        };
        match result {
            Ok(snapshot) => {
                if !connected || snapshot.session_id != latest.session_id {
                    delivered = None;
                }
                connected = true;
                latest = snapshot;
            }
            Err(_) => {
                clear_bands(&bands);
                if connected {
                    events
                        .status("Daemon disconnected; reconnecting to the selected configuration");
                }
                connected = false;
                // Short sleeps make cancellation responsive without a spin loop.
                for _ in 0..10 {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
        // A server may return immediately for telemetry or unchanged status.
        // Keep a bounded request rate even when the receiving UI is stalled.
        thread::sleep(Duration::from_millis(20));
    }
    clear_bands(&bands);
    let _ = connection.request(Operation::Unsubscribe { client_id }, Duration::from_secs(2));
}

fn snapshot_events(previous: Option<&Snapshot>, snapshot: &Snapshot) -> Vec<PlayerEvent> {
    let mut events = vec![PlayerEvent::ReadModel(Box::new(snapshot.clone()))];
    let track_changed = previous.and_then(|s| s.track.as_ref()).map(|t| &t.uri)
        != snapshot.track.as_ref().map(|t| &t.uri);
    if let Some(track) = &snapshot.track {
        if track_changed {
            events.push(PlayerEvent::TrackChanged {
                uri: track.uri.clone(),
            });
        }
        if previous.and_then(|s| s.track.as_ref()) != Some(track) {
            // Missing canonical fields stay empty, never inferred from URI.
            events.push(PlayerEvent::Metadata(ObservedMetadata {
                uri: track.uri.clone(),
                title: track.title.clone().unwrap_or_default(),
                artist: track
                    .artists
                    .as_ref()
                    .map(|artists| artists.join(", "))
                    .unwrap_or_default(),
                album: track.album.clone().unwrap_or_default(),
                duration_ms: track.duration_ms.unwrap_or(0).min(u64::from(u32::MAX)) as u32,
                cover_url: track.artwork_url.clone(),
            }));
        }
    }
    // ReadModel owns playback/anchors, including inactive and buffering states.
    // Legacy position events would replace canonical timestamp/speed on attach.
    events
}

pub fn handle_management(cli: &Cli) -> Result<bool> {
    if let Mode::Config(command) = cli.mode {
        let source = if command == crate::cli::ConfigCommand::Init && cli.output.is_some() {
            cli.output.clone()
        } else {
            crate::config::Config::path()
        }
        .context("Resolve selected configuration path")?;
        crate::config::setup(command, &source, cli.output.as_deref())?;
        println!("Configuration operation completed (static validation only; no services started)");
        return Ok(true);
    }
    if cli.mode == Mode::Tui {
        return Ok(false);
    }
    if cli.mode == Mode::Daemon(DaemonCommand::Run) {
        crate::daemon::run()?;
        return Ok(true);
    }
    // Inspection/stop/uninstall must remain available when a user is fixing
    // invalid config. Validate replacements before stopping an existing owner.
    if matches!(
        cli.mode,
        Mode::Daemon(DaemonCommand::Start | DaemonCommand::Restart)
            | Mode::Service(ServiceCommand::Install)
    ) {
        crate::config::get().validate()?;
    }
    let config = crate::config::Config::path().context("Resolve selected configuration path")?;
    match cli.mode {
        Mode::Daemon(DaemonCommand::Start) => {
            Connection::ensure(&config)?;
            println!("Daemon started");
        }
        Mode::Daemon(DaemonCommand::Stop) => {
            stop_owner(Connection::selected(&config)?)?;
            println!("Daemon stopped");
        }
        Mode::Daemon(DaemonCommand::Restart) => {
            stop_owner(Connection::selected(&config)?)?;
            Connection::ensure(&config)?;
            println!("Daemon restarted with the selected configuration");
        }
        Mode::Daemon(DaemonCommand::Status) | Mode::Service(ServiceCommand::Status) => {
            let service = crate::service::status()?;
            let snapshot = Connection::selected(&config)?.status_any()?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "online": snapshot.is_some(), "service": service, "snapshot": snapshot,
                    }))?
                );
            } else {
                println!(
                    "Service: {} / {}",
                    if service.installed {
                        "installed"
                    } else {
                        "not installed"
                    },
                    if service.loaded {
                        "loaded"
                    } else {
                        "not loaded"
                    }
                );
                if let Some(snapshot) = snapshot {
                    println!(
                        "Daemon: online (pid {}, {:?}, {:?})",
                        snapshot.pid, snapshot.readiness, snapshot.playback
                    );
                    println!("Configuration: {}", snapshot.config_path.escape_default());
                    println!(
                        "Web API: {}; Soloist: {}",
                        if snapshot.web_api_ready {
                            "ready"
                        } else {
                            "setup required"
                        },
                        if snapshot.soloist_ready {
                            "ready"
                        } else {
                            "setup required"
                        }
                    );
                    if snapshot.rename_requires_restart {
                        println!("Device rename pending explicit restart");
                    }
                } else {
                    println!("Daemon: offline");
                }
            }
        }
        Mode::Service(ServiceCommand::Install) => {
            crate::service::install(
                &std::env::current_exe().context("Locate myx executable")?,
                &config,
            )?;
            println!("Service registered; activation and configuration changes are pending an explicit daemon restart");
        }
        Mode::Service(ServiceCommand::Uninstall) => {
            crate::service::uninstall()?;
            println!("Myx service uninstalled");
        }
        Mode::Tui | Mode::Daemon(DaemonCommand::Run) | Mode::Config(_) => unreachable!(),
    }
    Ok(true)
}

fn stop_owner(connection: Connection) -> Result<()> {
    // Boot out a registered supervisor first so it cannot respawn the owner
    // after Shutdown. Only our recognized service is touched by this API.
    crate::service::stop()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let snapshot = match connection.status_any_until(deadline) {
        Ok(snapshot) => snapshot,
        Err(error) if stopping_error(&error) => None,
        Err(error) => return Err(error),
    };
    if let Some(snapshot) = &snapshot {
        let active = Connection::selected(std::path::Path::new(&snapshot.config_path))?;
        match active.request(Operation::Shutdown, CONTROL_TIMEOUT) {
            Ok(payload) => accepted(payload)?,
            Err(error) if stopping_error(&error) => {}
            Err(error) => return Err(error),
        }
    }
    loop {
        // Do not race a dying socket with status handshakes. An acknowledged
        // Shutdown completes only when the server releases its lifetime lock.
        if connection.owner_released()? {
            match connection.status_any_until(deadline)? {
                None => return Ok(()),
                Some(current)
                    if snapshot
                        .as_ref()
                        .is_none_or(|old| current.session_id != old.session_id) =>
                {
                    bail!("Another daemon started while stopping; inspect daemon status before retrying");
                }
                _ => {}
            }
        }
        ensure!(
            Instant::now() < deadline,
            "Daemon did not stop within fifteen seconds"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn stopping_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::ipc::protocol::IpcError>()
        .is_some_and(|error| error.code == crate::ipc::protocol::ErrorCode::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_stop_tolerates_only_an_owner_already_stopping() {
        use crate::ipc::protocol::{ErrorCode, IpcError};
        assert!(stopping_error(&anyhow::Error::new(IpcError::new(
            ErrorCode::Unavailable,
            "Daemon is stopping"
        ))));
        assert!(!stopping_error(&anyhow::Error::new(IpcError::new(
            ErrorCode::ConfigMismatch,
            "Wrong profile"
        ))));
        assert!(!stopping_error(&anyhow::anyhow!(
            "unrelated transport failure"
        )));
    }

    #[test]
    fn metadata_changes_use_canonical_fields_even_for_same_uri() {
        let mut snapshot = Snapshot::starting("/config".into(), "myx".into());
        snapshot.track = Some(crate::domain::TrackMetadata::pending(
            crate::domain::ItemKind::Track,
            "spotify:track:identifier".into(),
        ));
        let events = snapshot_events(None, &snapshot);
        assert!(events
            .iter()
            .any(|e| matches!(e, PlayerEvent::Metadata(m) if m.title.is_empty())));
        let previous = snapshot.clone();
        snapshot.track.as_mut().unwrap().title = Some("Canonical title".into());
        let events = snapshot_events(Some(&previous), &snapshot);
        assert!(!events
            .iter()
            .any(|e| matches!(e, PlayerEvent::TrackChanged { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, PlayerEvent::Metadata(m) if m.title == "Canonical title")));
        assert!(!snapshot_events(Some(&snapshot), &snapshot)
            .iter()
            .any(|e| matches!(e, PlayerEvent::Metadata(_))));
    }
}
