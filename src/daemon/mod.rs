//! Sole per-user owner of authorization, SDK, processed audio and native media.
mod lifecycle;
pub mod server;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use souvlaki::{MediaControlEvent, SeekDirection};

use crate::config::{self, Config};
use crate::domain::{
    playback::now_ms, ItemKind, MetadataStatus, PlaybackStatus, PlayerCommand, ServiceState,
    Snapshot, TrackMetadata,
};
use crate::ipc::protocol::{ErrorCode, IpcError, Operation, Payload};
use crate::ipc::unix::RuntimePaths;
use crate::player::{controller, reducer};
use crate::soloist::{protocol::PlayerEvent, RuntimePlayer};
use crate::webapi::{ApiClient, ApiError, Authorization, AuthorizationFlow, HttpMethod};

enum OwnerMessage {
    Command(PlayerCommand, flume::Sender<Result<(), IpcError>>),
    RetryServices,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
    changed: Condvar,
    shutdown: Arc<AtomicBool>,
    api: Mutex<Option<ApiClient>>,
    device: Mutex<Option<String>>,
    subscriptions: Mutex<HashMap<String, (Instant, bool)>>,
    oauth: AtomicBool,
    generation: AtomicU64,
    commands: flume::Sender<OwnerMessage>,
}

impl Shared {
    fn update(&self, edit: impl FnOnce(&mut Snapshot)) {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        edit(&mut snapshot);
        snapshot.sequence = snapshot.sequence.saturating_add(1);
        self.changed.notify_all();
    }
    fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn status(&self) -> Snapshot {
        let mut snapshot = self.snapshot();
        if let Ok(config) = Config::load_path(&PathBuf::from(&snapshot.config_path)) {
            snapshot.configured_device_name = config.soloist.device_name;
            snapshot.rename_requires_restart =
                snapshot.configured_device_name != snapshot.effective_device_name;
        }
        snapshot
    }
    fn has_spectrum(&self) -> bool {
        let mut subscribers = self
            .subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscribers.retain(|_, (deadline, _)| *deadline > Instant::now());
        subscribers.values().any(|(_, enabled)| *enabled)
    }
    fn handle(self: &Arc<Self>, operation: Operation) -> Result<Payload, IpcError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(IpcError::new(
                ErrorCode::Unavailable,
                "Daemon is stopping; reconnect after restart",
            ));
        }
        match operation {
            Operation::Status => Ok(Payload::Snapshot(Box::new(self.status()))),
            Operation::Watch {
                after,
                client_id,
                spectrum,
            } => {
                if client_id.is_empty()
                    || client_id.len() > 64
                    || !client_id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                {
                    return Err(IpcError::new(
                        ErrorCode::InvalidRequest,
                        "Invalid subscription identity",
                    ));
                }
                {
                    let mut subscribers = self
                        .subscriptions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    subscribers.retain(|_, (until, _)| *until > Instant::now());
                    if subscribers.len() >= 32 && !subscribers.contains_key(&client_id) {
                        return Err(IpcError::new(ErrorCode::Busy, "Too many subscriptions"));
                    }
                    subscribers.insert(
                        client_id,
                        (Instant::now() + Duration::from_secs(3), spectrum),
                    );
                }
                let snapshot = self
                    .snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if snapshot.sequence <= after {
                    drop(self.changed.wait_timeout(snapshot, Duration::from_secs(1)));
                } else {
                    drop(snapshot);
                }
                Ok(Payload::Snapshot(Box::new(self.snapshot())))
            }
            Operation::Unsubscribe { client_id } => {
                self.subscriptions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&client_id);
                Ok(Payload::Accepted)
            }
            Operation::Shutdown => {
                self.shutdown.store(true, Ordering::Release);
                self.changed.notify_all();
                Ok(Payload::Accepted)
            }
            Operation::Authorize => self.authorize(),
            Operation::Catalog { method, path, body } => {
                if method != HttpMethod::Get && path.starts_with("/v1/me/player") {
                    return Err(IpcError::new(
                        ErrorCode::InvalidRequest,
                        "Playback writes must use serialized typed player commands",
                    ));
                }
                let result = {
                    let mut api = self
                        .api
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    api.as_mut()
                        .ok_or_else(|| {
                            IpcError::new(
                                ErrorCode::SetupRequired,
                                "Web API authorization is required",
                            )
                        })?
                        .request(method, &path, body)
                };
                match result {
                    Ok(value) => Ok(Payload::Json(value)),
                    Err(error) => {
                        if error == ApiError::Unauthorized {
                            self.update(|snapshot| {
                                snapshot.web_api_ready = false;
                                reducer::readiness(snapshot);
                                snapshot.message = Some(error.to_string());
                            });
                        }
                        Err(api_error(error))
                    }
                }
            }
            Operation::Player(command) => {
                let (reply, result) = flume::bounded(1);
                self.commands
                    .try_send(OwnerMessage::Command(command, reply))
                    .map_err(|_| IpcError::new(ErrorCode::Busy, "Player command queue is busy"))?;
                result.recv_timeout(Duration::from_secs(2)).map_err(|_| {
                    IpcError::new(
                        ErrorCode::Timeout,
                        "Player acknowledgement timed out; inspect state before retrying",
                    )
                })??;
                Ok(Payload::Accepted)
            }
        }
    }
    fn authorize(self: &Arc<Self>) -> Result<Payload, IpcError> {
        if self.oauth.swap(true, Ordering::AcqRel) {
            return Err(IpcError::new(
                ErrorCode::Busy,
                "An authorization flow is already pending",
            ));
        }
        let flow =
            configured_client_id().and_then(|id| AuthorizationFlow::begin(id, cache_path()?));
        let flow = match flow {
            Ok(flow) => flow,
            Err(_) => {
                self.oauth.store(false, Ordering::Release);
                return Err(IpcError::new(ErrorCode::SetupRequired, "Configure a Spotify client_id and allow the loopback OAuth callback before setup"));
            }
        };
        let url = flow.url().to_owned();
        let shared = self.clone();
        std::thread::Builder::new()
            .name("myx-oauth".into())
            .spawn(move || {
                match flow.wait(&shared.shutdown) {
                    Ok(auth) => {
                        shared.generation.fetch_add(1, Ordering::AcqRel);
                        shared.update(|snapshot| snapshot.owned_device_verified = false);
                        *shared
                            .device
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                        *shared
                            .api
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(ApiClient::new(auth));
                        shared.update(|snapshot| {
                            snapshot.web_api_ready = true;
                            snapshot.readiness = ServiceState::Starting;
                            snapshot.message = None;
                        });
                        let _ = shared.commands.try_send(OwnerMessage::RetryServices);
                    }
                    Err(_) => shared.update(|snapshot| {
                        snapshot.message = Some(
                            "Web API authorization did not complete; retry explicit setup".into(),
                        );
                        reducer::readiness(snapshot);
                    }),
                }
                shared.oauth.store(false, Ordering::Release);
            })
            .map_err(|_| {
                self.oauth.store(false, Ordering::Release);
                IpcError::new(ErrorCode::Internal, "Cannot start authorization worker")
            })?;
        Ok(Payload::Authorization { url })
    }
}

fn configured_client_id() -> Result<String> {
    std::env::var("MYX_CLIENT_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .or_else(|| config::get().client_id.clone())
        .context("Configure the required Spotify Web API client_id")
}
fn cache_path() -> Result<PathBuf> {
    Ok(crate::home_dir()
        .context("A home directory is required")?
        .join(".cache/myx/webapi.json"))
}
fn api_error(error: ApiError) -> IpcError {
    let code = match error {
        ApiError::Unauthorized => ErrorCode::Unauthorized,
        ApiError::Restricted | ApiError::DeviceNotOwned => ErrorCode::Restricted,
        ApiError::NotFound => ErrorCode::NotFound,
        ApiError::Quota => ErrorCode::Quota,
        ApiError::RateLimited { .. } => ErrorCode::RateLimited,
        ApiError::InvalidRequest => ErrorCode::InvalidRequest,
        _ => ErrorCode::Unavailable,
    };
    IpcError {
        code,
        message: error.to_string(),
    }
}

pub fn run() -> Result<()> {
    config::get().validate()?;
    lifecycle::install();
    let path = Config::path().context("Resolve daemon configuration")?;
    let (commands, receiver) = flume::bounded(64);
    let shared = Arc::new(Shared {
        snapshot: Mutex::new(Snapshot::starting(
            path.to_string_lossy().into_owned(),
            config::get().soloist.device_name.clone(),
        )),
        changed: Condvar::new(),
        shutdown: Arc::new(AtomicBool::new(false)),
        api: Mutex::new(None),
        device: Mutex::new(None),
        subscriptions: Mutex::new(HashMap::new()),
        oauth: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        commands,
    });
    shared.update(|snapshot| snapshot.volume = config::get().soloist.initial_volume);
    let handler_shared = shared.clone();
    let mut server = server::Server::bind(
        RuntimePaths::for_user()?,
        path.to_string_lossy().into_owned(),
        Arc::new(move |operation| handler_shared.handle(operation)),
    )?;
    let owner_shared = shared.clone();
    let result = crate::media::run(shared.shutdown.clone(), move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        runtime.block_on(owner(owner_shared, receiver))
    });
    shared.shutdown.store(true, Ordering::Release);
    shared.changed.notify_all();
    server.stop();
    result
}

async fn start_player(
    shared: &Shared,
    events: &flume::Sender<PlayerEvent>,
) -> Option<RuntimePlayer> {
    if !shared.snapshot().web_api_ready
        || shared.shutdown.load(Ordering::Acquire)
        || lifecycle::signalled()
    {
        return None;
    }
    shared.update(|snapshot| {
        snapshot.readiness = ServiceState::Starting;
    });
    let startup = RuntimePlayer::start(
        config::get().soloist.clone(),
        events.clone(),
        shared.snapshot().volume,
    );
    let cancellation = async {
        while !shared.shutdown.load(Ordering::Acquire) && !lifecycle::signalled() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let result = tokio::select! { result = startup => result, _ = cancellation => return None };
    match result {
        Ok(player) => {
            shared.update(|snapshot| {
                snapshot.message =
                    Some("Waiting for Soloist authentication and owned-device verification".into());
            });
            Some(player)
        }
        Err(_) => {
            shared.update(|snapshot| { snapshot.readiness = ServiceState::SetupRequired; snapshot.message = Some("Soloist SDK/key/audio setup is required; fix configuration and explicitly restart".into()); });
            None
        }
    }
}

async fn owner(shared: Arc<Shared>, commands: flume::Receiver<OwnerMessage>) -> Result<()> {
    let (events, observed) = flume::bounded(128);
    let (media_events, media_received) = flume::bounded(32);
    let mut media = crate::media::MediaOwner::new(media_events)?;
    shared.update(|snapshot| snapshot.native_media_ready = true);
    let initialized = tokio::task::spawn_blocking(|| {
        configured_client_id()
            .and_then(|id| Authorization::load_cached(id, cache_path()?))
            .map(ApiClient::new)
    })
    .await
    .context("Join required authorization initialization")?;
    match initialized {
        Ok(api) => {
            *shared
                .api
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(api);
            shared.update(|snapshot| {
                snapshot.web_api_ready = true;
            });
        }
        Err(_) => shared.update(|snapshot| {
            snapshot.readiness = ServiceState::SetupRequired;
            snapshot.message = Some(
                "Web API setup required; run an interactive client with a configured client_id"
                    .into(),
            );
        }),
    }
    let persistence_path = RuntimePaths::for_user()?.directory.join("player-v1.json");
    let saved = match crate::player::persist::SavedPlayer::load(&persistence_path) {
        Ok(saved) => saved,
        Err(_) => {
            shared.update(|snapshot| {
                snapshot.message =
                    Some("Saved playback could not be restored; its file was preserved".into())
            });
            None
        }
    };
    let mut last_intent = saved.as_ref().and_then(|saved| saved.last_intent.clone());
    if config::get().restore_on_startup {
        if let Some(saved) = &saved {
            shared.update(|snapshot| saved.apply(snapshot));
        }
    }
    let mut restore_pending = saved.is_some() && config::get().restore_on_startup;
    let mut player = start_player(&shared, &events).await;
    if let Some(player) = &player {
        player.set_equalizer(shared.snapshot().equalizer);
    }
    let mut writer = crate::player::persist::Writer::start(persistence_path)?;
    let verifying = Arc::new(AtomicBool::new(false));
    let enriching = Arc::new(AtomicBool::new(false));
    let mut next_verification = Instant::now();
    let mut last_media = u64::MAX;
    let mut last_persist = Instant::now();
    let mut pending_playback = reducer::PendingPlayback::default();
    while !shared.shutdown.load(Ordering::Acquire) && !lifecycle::signalled() {
        let wants_spectrum = shared.has_spectrum();
        if let Some(player) = &player {
            let snapshot = shared.snapshot();
            player.set_verified(
                snapshot.owned_device_verified && snapshot.soloist_ready && snapshot.locally_active,
            );
            player.set_spectrum_enabled(wants_spectrum);
        }
        let interval = if wants_spectrum
            || (pending_playback.is_pending() && shared.snapshot().owned_device_verified)
        {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        tokio::select! {
            message = commands.recv_async() => match message {
                Ok(OwnerMessage::Command(command, reply)) => {
                    // Timed-out clients must not leave commands queued across setup.
                    if reply.is_disconnected() { continue; }
                    let command = controller::prepare_command(command, &shared.snapshot(), last_intent.clone(), restore_pending);
                    let proposed_intent = matches!(command, PlayerCommand::PlayContext { .. } | PlayerCommand::PlayTracks { .. }).then(|| command.clone());
                    let stops_restore = matches!(command, PlayerCommand::Stop);
                    let result = if let Some(player) = &player {
                        let mut snapshot = shared.snapshot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        let result = controller::dispatch(player, &mut snapshot, command).map_err(|_| IpcError::new(ErrorCode::SetupRequired, "Player operation failed; inspect required services and owned-device state"));
                        snapshot.sequence = snapshot.sequence.saturating_add(1); shared.changed.notify_all(); result
                    } else { Err(IpcError::new(ErrorCode::SetupRequired, "Required player services need setup")) };
                    if result.is_ok() {
                        if let Some(intent) = proposed_intent { last_intent = Some(intent); restore_pending = false; }
                        if stops_restore { restore_pending = false; }
                    }
                    let _ = reply.try_send(result);
                }
                Ok(OwnerMessage::RetryServices) => { if player.is_none() { player = start_player(&shared, &events).await; } next_verification = Instant::now(); }
                Err(_) => break,
            },
            event = observed.recv_async() => if let Ok(event) = event {
                pending_playback.note(&event);
                if matches!(event, PlayerEvent::Authentication { logged_in: false, .. }) {
                    shared.generation.fetch_add(1, Ordering::AcqRel);
                    *shared.device.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                }
                let pcm = player.as_ref().and_then(RuntimePlayer::audio_stats).is_some_and(|stats| stats.processed_frames > 0);
                shared.update(|snapshot| {
                    reducer::observe(snapshot, event, pcm);
                    if player.as_ref().is_some_and(|player| !player.has_local_dsp()) {
                        snapshot.soloist_ready = false; reducer::readiness(snapshot);
                    }
                });
            },
            event = media_received.recv_async() => if let (Ok(event), Some(player)) = (event, player.as_ref()) {
                let mut snapshot = shared.snapshot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(command) = media_command(event, &snapshot) {
                    let command = controller::prepare_command(command, &snapshot, last_intent.clone(), restore_pending);
                    let proposed = matches!(command, PlayerCommand::PlayContext { .. } | PlayerCommand::PlayTracks { .. }).then(|| command.clone());
                    let stopping = matches!(command, PlayerCommand::Stop);
                    if controller::dispatch(player, &mut snapshot, command).is_err() { snapshot.message = Some("Native media command requires ready owned services".into()); }
                    else {
                        if let Some(intent) = proposed { last_intent = Some(intent); restore_pending = false; }
                        if stopping { restore_pending = false; }
                    }
                    snapshot.sequence = snapshot.sequence.saturating_add(1); shared.changed.notify_all();
                }
            },
            _ = tokio::time::sleep(interval) => {},
        }
        if wants_spectrum {
            if let Some(player) = &player {
                if let Ok(bands) = player.bands.try_lock() {
                    let values = bands
                        .values
                        .iter()
                        .map(|value| if value.is_finite() { *value } else { 0.0 })
                        .collect();
                    shared.update(|snapshot| snapshot.spectrum = Some(values));
                }
            }
        } else if shared.snapshot().spectrum.is_some() {
            shared.update(|snapshot| snapshot.spectrum = None);
        }
        let snapshot = shared.snapshot();
        if snapshot.sequence != last_media {
            let native_ready = media.update(&snapshot);
            if snapshot.native_media_ready != native_ready {
                shared.update(|snapshot| {
                    snapshot.native_media_ready = native_ready;
                    reducer::readiness(snapshot);
                    if !native_ready { snapshot.message = Some("Mac native media integration is unavailable; check the GUI user session".into()); }
                });
            }
            last_media = snapshot.sequence;
            if last_persist.elapsed() >= Duration::from_secs(1) {
                writer.submit(crate::player::persist::SavedPlayer::from_snapshot(
                    &snapshot,
                    last_intent.clone(),
                ));
                last_persist = Instant::now();
            }
        }
        if pending_playback.is_pending()
            && snapshot.owned_device_verified
            && snapshot.locally_active
        {
            let pcm = player
                .as_ref()
                .and_then(RuntimePlayer::audio_stats)
                .is_some_and(|stats| stats.processed_frames > 0);
            if pcm {
                shared.update(|snapshot| {
                    pending_playback.confirm(snapshot, true);
                });
            }
        }
        if snapshot.soloist_ready
            && !snapshot.owned_device_verified
            && Instant::now() >= next_verification
            && !verifying.load(Ordering::Acquire)
        {
            if let Some(id) = player.as_ref().and_then(|player| player.device_id().ok()) {
                next_verification = Instant::now() + Duration::from_secs(30);
                verify_device(shared.clone(), verifying.clone(), id);
            }
        }
        if snapshot.metadata_status == MetadataStatus::Loading && !enriching.load(Ordering::Acquire)
        {
            if let Some(track) = snapshot.track {
                enrich(shared.clone(), enriching.clone(), track.uri);
            }
        }
    }
    shared.shutdown.store(true, Ordering::Release);
    writer.submit(crate::player::persist::SavedPlayer::from_snapshot(
        &shared.snapshot(),
        last_intent,
    ));
    if writer.finish().is_err() {
        crate::liblog::liblog("Playback persistence failed; existing saved data was preserved");
    }
    if let Some(player) = player.take() {
        player.shutdown()?;
    }
    shared.update(|snapshot| {
        snapshot.playback = PlaybackStatus::Stopped;
        snapshot.position = Default::default();
        snapshot.track = None;
        snapshot.spectrum = None;
        snapshot.locally_active = false;
        snapshot.soloist_ready = false;
        snapshot.owned_device_verified = false;
    });
    media.update(&shared.snapshot());
    Ok(())
}

fn verify_device(shared: Arc<Shared>, pending: Arc<AtomicBool>, id: String) {
    pending.store(true, Ordering::Release);
    let generation = shared.generation.load(Ordering::Acquire);
    std::thread::spawn(move || {
        let verified = {
            let mut api = shared
                .api
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            api.as_mut()
                .and_then(|api| {
                    api.request(HttpMethod::Get, "/v1/me/player/devices", None)
                        .ok()
                })
                .is_some_and(|value| controller::verify_owned_device(&value, &id))
        };
        if shared.generation.load(Ordering::Acquire) != generation {
            pending.store(false, Ordering::Release);
            return;
        }
        if verified && shared.snapshot().soloist_ready {
            *shared
                .device
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id.clone());
            if let Some(api) = shared
                .api
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut()
            {
                api.set_owned_device(Some(id));
            }
        }
        shared.update(|snapshot| { if shared.generation.load(Ordering::Acquire) != generation { return; }
            snapshot.owned_device_verified = verified && snapshot.soloist_ready; reducer::readiness(snapshot);
            snapshot.message = (!verified).then(|| "Owned Soloist device not found in this Web API account; reconcile authentication".into()); });
        pending.store(false, Ordering::Release);
    });
}

fn enrich(shared: Arc<Shared>, pending: Arc<AtomicBool>, uri: String) {
    pending.store(true, Ordering::Release);
    std::thread::spawn(move || {
        let mut parts = uri.split(':');
        let kind = parts.nth(1).unwrap_or("");
        let id = parts.next().unwrap_or("");
        let endpoint = match kind {
            "track" => "tracks",
            "episode" => "episodes",
            _ => "",
        };
        let value = if !endpoint.is_empty()
            && id.len() == 22
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            shared
                .api
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut()
                .and_then(|api| {
                    api.request(HttpMethod::Get, &format!("/v1/{endpoint}/{id}"), None)
                        .ok()
                })
        } else {
            None
        };
        shared.update(|snapshot| {
            if snapshot.track.as_ref().is_none_or(|track| track.uri != uri) {
                return;
            }
            snapshot.metadata_status = MetadataStatus::Unavailable;
            if let Some(value) = value {
                let mut metadata = TrackMetadata::pending(
                    if kind == "episode" {
                        ItemKind::Episode
                    } else {
                        ItemKind::Track
                    },
                    uri.clone(),
                );
                metadata.title = value["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                metadata.duration_ms = value["duration_ms"].as_u64();
                metadata.spotify_url = value["external_urls"]["spotify"]
                    .as_str()
                    .map(str::to_owned);
                if kind == "episode" {
                    metadata.artists = value["show"]["publisher"]
                        .as_str()
                        .map(|publisher| vec![publisher.to_owned()]);
                    metadata.album = value["show"]["name"].as_str().map(str::to_owned);
                } else {
                    metadata.artists = value["artists"].as_array().map(|artists| {
                        artists
                            .iter()
                            .filter_map(|artist| artist["name"].as_str().map(str::to_owned))
                            .collect()
                    });
                    metadata.album = value["album"]["name"].as_str().map(str::to_owned);
                }
                let images = if kind == "episode" {
                    &value["images"]
                } else {
                    &value["album"]["images"]
                };
                metadata.artwork_url = largest_image(images);
                snapshot.metadata_status = if metadata.title.is_some() {
                    MetadataStatus::Ready
                } else {
                    MetadataStatus::Unavailable
                };
                snapshot.track = Some(metadata);
            }
        });
        pending.store(false, Ordering::Release);
    });
}

fn largest_image(images: &Value) -> Option<String> {
    images
        .as_array()?
        .iter()
        .filter_map(|image| {
            let url = reqwest::Url::parse(image["url"].as_str()?).ok()?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return None;
            }
            Some((
                image["width"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_mul(image["height"].as_u64().unwrap_or(0)),
                url.to_string(),
            ))
        })
        .max_by_key(|(area, _)| *area)
        .map(|(_, url)| url)
}

fn media_command(event: MediaControlEvent, snapshot: &Snapshot) -> Option<PlayerCommand> {
    Some(match event {
        MediaControlEvent::Play => PlayerCommand::Play,
        MediaControlEvent::Pause => PlayerCommand::Pause,
        MediaControlEvent::Toggle => PlayerCommand::Toggle,
        MediaControlEvent::Stop => PlayerCommand::Stop,
        MediaControlEvent::Next => PlayerCommand::Next,
        MediaControlEvent::Previous => PlayerCommand::Previous,
        MediaControlEvent::SetPosition(position) => PlayerCommand::Seek {
            position_ms: position.0.as_millis().min(u128::from(u32::MAX)) as u32,
        },
        MediaControlEvent::SeekBy(direction, duration) => {
            let position = snapshot.position.at(
                now_ms(),
                snapshot.track.as_ref().and_then(|track| track.duration_ms),
            );
            let step = duration.as_millis().min(u128::from(u32::MAX)) as u64;
            let target = if direction == SeekDirection::Forward {
                position.saturating_add(step)
            } else {
                position.saturating_sub(step)
            };
            PlayerCommand::Seek {
                position_ms: target.min(u64::from(u32::MAX)) as u32,
            }
        }
        MediaControlEvent::SetVolume(volume) if volume.is_finite() => PlayerCommand::Volume {
            percent: (volume.clamp(0.0, 1.0) * 100.0).round() as u8,
        },
        _ => return None,
    })
}
