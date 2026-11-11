//! Sole per-user owner of authorization, SDK, processed audio and native media.
mod api_gate;
mod lifecycle;
mod playback;
mod queue;
pub mod server;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
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
    Command(PlayerCommand, u64, flume::Sender<Result<(), IpcError>>),
    RetryServices,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
    changed: Condvar,
    shutdown: Arc<AtomicBool>,
    api: Mutex<Option<ApiClient>>,
    api_gate: Arc<api_gate::ApiGate>,
    publisher: Mutex<Option<crate::outputs::now_playing::Publisher>>,
    device: Mutex<Option<String>>,
    subscriptions: Mutex<HashMap<String, (Instant, bool)>>,
    oauth: AtomicBool,
    generation: AtomicU64,
    commands: flume::Sender<OwnerMessage>,
    control: OnceLock<playback::Control>,
    lifetime: OnceLock<std::fs::File>,
    native_queue: Mutex<queue::NativeQueue>,
    observations: AtomicU64,
}

impl Shared {
    fn api_call(
        &self,
        generation: u64,
        method: HttpMethod,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ApiError> {
        let _permit = self.api_gate.enter(Duration::from_secs(3))?;
        let mut api = self
            .api
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.shutdown.load(Ordering::Acquire)
            || self.generation.load(Ordering::Acquire) != generation
        {
            return Err(ApiError::Busy);
        }
        let result = api
            .as_mut()
            .ok_or(ApiError::Unauthorized)?
            .request(method, path, body);
        drop(api);
        if self.generation.load(Ordering::Acquire) != generation {
            return Err(if method == HttpMethod::Get {
                ApiError::Busy
            } else {
                ApiError::AmbiguousWrite
            });
        }
        result
    }
    fn api_failure(&self, error: &ApiError) {
        if !matches!(
            error,
            ApiError::Unauthorized
                | ApiError::Offline
                | ApiError::Server
                | ApiError::InvalidResponse
                | ApiError::AmbiguousWrite
                | ApiError::RateLimited { .. }
                | ApiError::Quota
        ) {
            return;
        }
        self.update(|snapshot| {
            snapshot.catalog_available = false;
            if *error == ApiError::Unauthorized {
                snapshot.web_api_ready = false;
            }
            reducer::readiness(snapshot);
            snapshot.message = Some(error.to_string());
        });
    }
    fn update(&self, edit: impl FnOnce(&mut Snapshot)) {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        edit(&mut snapshot);
        snapshot.sequence = snapshot.sequence.saturating_add(1);
        self.changed.notify_all();
        let value = snapshot.clone();
        drop(snapshot);
        if let Some(publisher) = self
            .publisher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            publisher.submit(value);
        }
    }
    fn snapshot(&self) -> Snapshot {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        snapshot.output_error = self
            .publisher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|publisher| publisher.error());
        snapshot.spectrum_subscribers = self
            .subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|(until, enabled)| *enabled && *until > Instant::now())
            .count()
            .min(u32::MAX as usize) as u32;
        snapshot
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
            Operation::QueuePage {
                revision,
                offset,
                limit,
                previous,
                metadata,
            } => {
                let generation = self.generation.load(Ordering::Acquire);
                let mut page = self
                    .native_queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .page(revision, offset, limit, previous)?;
                let authority = self.snapshot();
                page.session_id = authority.session_id;
                page.auth_revision = authority.auth_revision;
                if metadata {
                    // At most two bounded batch requests, only for the requested viewport.
                    for (kind, endpoint) in [("track", "tracks"), ("episode", "episodes")] {
                        let ids: Vec<_> = page
                            .entries
                            .iter()
                            .filter(|entry| {
                                entry.title.is_none()
                                    && crate::player::intents::valid_uri(&entry.uri, &[kind])
                            })
                            .filter_map(|entry| entry.uri.rsplit(':').next())
                            .collect();
                        if ids.is_empty() {
                            continue;
                        }
                        let response = self.api_call(
                            generation,
                            HttpMethod::Get,
                            &format!("/v1/{endpoint}?ids={}", ids.join(",")),
                            None,
                        );
                        if self.shutdown.load(Ordering::Acquire)
                            || self.generation.load(Ordering::Acquire) != generation
                        {
                            return Err(IpcError::new(
                                ErrorCode::CollectionChanged,
                                "Queue authority changed",
                            ));
                        }
                        if let Ok(value) = response {
                            if let Some(items) = value[endpoint].as_array() {
                                let mut queue = self
                                    .native_queue
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                for item in items {
                                    if let (Some(uri), Some(title)) = (
                                        item["uri"].as_str(),
                                        item["name"].as_str().filter(|title| !title.is_empty()),
                                    ) {
                                        queue.remember(uri.to_owned(), title.to_owned());
                                        for entry in &mut page.entries {
                                            if entry.uri == uri {
                                                entry.title = Some(title.to_owned());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Do not return old coordinates after metadata HTTP raced a queue update.
                self.native_queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .page(Some(page.revision), offset, limit, previous)?;
                if self.generation.load(Ordering::Acquire) != generation {
                    return Err(IpcError::new(
                        ErrorCode::CollectionChanged,
                        "Queue authority changed",
                    ));
                }
                Ok(Payload::QueuePage(page))
            }
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
                let generation = self.generation.load(Ordering::Acquire);
                let result = self.api_call(generation, method, &path, body);
                if self.generation.load(Ordering::Acquire) != generation {
                    return Err(IpcError::new(if method == HttpMethod::Get { ErrorCode::SetupRequired } else { ErrorCode::WriteUncertain },
                        "Authorization changed during this operation; reconcile state before retrying"));
                }
                match result {
                    Ok(value) => Ok(Payload::Json(value)),
                    Err(error) => {
                        self.api_failure(&error);
                        Err(api_error(error))
                    }
                }
            }
            Operation::Player(command) => {
                let (reply, result) = flume::bounded(1);
                self.commands
                    .try_send(OwnerMessage::Command(
                        command,
                        self.generation.load(Ordering::Acquire),
                        reply,
                    ))
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
        let state = self.snapshot();
        if state.readiness == ServiceState::Starting && !state.web_api_ready {
            return Err(IpcError::new(
                ErrorCode::Busy,
                "Initial authorization checks are still running",
            ));
        }
        if self.oauth.swap(true, Ordering::AcqRel) {
            return Err(IpcError::new(
                ErrorCode::Busy,
                "An authorization flow is already pending",
            ));
        }
        let flow = (|| {
            let current = Config::load_path(&PathBuf::from(self.snapshot().config_path))?;
            let id = std::env::var("MYX_CLIENT_ID")
                .ok()
                .filter(|id| !id.is_empty())
                .or(current.client_id)
                .context("Configure a Web API client_id")?;
            AuthorizationFlow::begin(id, cache_path()?)
        })();
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
                        let mut api = shared
                            .api
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if auth.commit_cache().is_err() {
                            drop(api);
                            shared.update(|snapshot| snapshot.message=Some("Authorization cache commit failed; inspect the private cache before retrying setup".into()));
                            shared.oauth.store(false,Ordering::Release);
                            return;
                        }
                         shared.generation.fetch_add(1, Ordering::AcqRel);
                         let queue_revision = shared.native_queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear_authority();
                        shared.update(|snapshot| {
                            snapshot.auth_revision = snapshot.auth_revision.saturating_add(1);
                            snapshot.owned_device_verified = false;
                             snapshot.catalog_available = false;
                             snapshot.queue = Default::default();
                             snapshot.queue_revision = queue_revision;
                             snapshot.queue_previous_total = 0;
                             snapshot.queue_upcoming_total = 0;
                             snapshot.options_observed_revision = 0;
                             snapshot.volume_observed_revision = 0;
                             snapshot.control_pending = false;
                            reducer::readiness(snapshot);
                        });
                        *shared
                            .device
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                        *api = Some(ApiClient::new(auth));
                        drop(api);
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
    let retry_after_secs = match &error {
        ApiError::RateLimited { retry_after_secs } => *retry_after_secs,
        _ => None,
    };
    let code = match error {
        ApiError::Busy => ErrorCode::Busy,
        ApiError::Unauthorized => ErrorCode::Unauthorized,
        ApiError::Restricted | ApiError::DeviceNotOwned => ErrorCode::Restricted,
        ApiError::NotFound => ErrorCode::NotFound,
        ApiError::Quota => ErrorCode::Quota,
        ApiError::RateLimited { .. } => ErrorCode::RateLimited,
        ApiError::InvalidRequest => ErrorCode::InvalidRequest,
        ApiError::AmbiguousWrite => ErrorCode::WriteUncertain,
        ApiError::CollectionChanged => ErrorCode::CollectionChanged,
        _ => ErrorCode::Unavailable,
    };
    IpcError {
        code,
        message: error.to_string(),
        retry_after_secs,
    }
}

pub fn run() -> Result<()> {
    config::get().validate()?;
    let path = Config::path().context("Resolve daemon configuration")?;
    if let Some(bundle) = crate::runtime_bundle::discover()? {
        let settings = config::get();
        let soloist = &settings.soloist;
        let mut user_paths = vec![
            path.clone(),
            cache_path()?,
            RuntimePaths::for_user()?.directory,
            soloist.directory()?,
            soloist.resolve(soloist.sdk.as_ref(), "assets/soloist")?,
            soloist.resolve(soloist.data_dir.as_ref(), "data")?,
            soloist.resolve(soloist.cache_dir.as_ref(), "cache")?,
        ];
        if soloist.api_key.is_none() {
            user_paths.push(soloist.resolve(soloist.api_key_file.as_ref(), "api-key")?);
        }
        if settings.now_playing.enabled {
            if let Some(output) = &settings.now_playing.path {
                user_paths.push(soloist.resolve(Some(output), "")?);
            }
        }
        for path in user_paths {
            bundle.ensure_external_user_path(&path)?;
        }
    }
    lifecycle::install();
    let (commands, receiver) = flume::bounded(64);
    let shared = Arc::new(Shared {
        snapshot: Mutex::new(Snapshot::starting(
            path.to_string_lossy().into_owned(),
            config::get().soloist.device_name.clone(),
        )),
        changed: Condvar::new(),
        shutdown: Arc::new(AtomicBool::new(false)),
        api: Mutex::new(None),
        api_gate: Arc::new(api_gate::ApiGate::default()),
        publisher: Mutex::new(None),
        device: Mutex::new(None),
        subscriptions: Mutex::new(HashMap::new()),
        oauth: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        commands,
        control: OnceLock::new(),
        lifetime: OnceLock::new(),
        native_queue: Mutex::new(queue::NativeQueue::default()),
        observations: AtomicU64::new(0),
    });
    shared.update(|snapshot| snapshot.volume = config::get().soloist.initial_volume);
    let handler_shared = shared.clone();
    let mut server = server::Server::bind(
        RuntimePaths::for_user()?,
        path.to_string_lossy().into_owned(),
        Arc::new(move |operation| handler_shared.handle(operation)),
    )?;
    // Clone the actual locked open file description, never reopen its pathname.
    shared
        .lifetime
        .set(server.publisher_lock()?)
        .map_err(|_| anyhow::anyhow!("Daemon lifetime lock already installed"))?;
    shared
        .control
        .set(playback::Control::start(shared.clone())?)
        .map_err(|_| anyhow::anyhow!("Control worker already installed"))?;
    let owner_shared = shared.clone();
    if config::get().now_playing.enabled {
        let soloist = &config::get().soloist;
        let protected = vec![
            path.clone(),
            cache_path()?,
            soloist.directory()?,
            soloist.resolve(soloist.api_key_file.as_ref(), "api-key")?,
            soloist.resolve(soloist.data_dir.as_ref(), "data")?,
            soloist.resolve(soloist.cache_dir.as_ref(), "cache")?,
            RuntimePaths::for_user()?.directory,
        ];
        let publisher = crate::outputs::now_playing::Publisher::start_owned(
            &config::get().now_playing,
            &protected,
            shared.snapshot(),
            server.publisher_lock()?,
        );
        *shared
            .publisher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(publisher);
    }
    let result = crate::media::run(shared.shutdown.clone(), move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        runtime.block_on(owner(owner_shared, receiver))
    });
    shared.shutdown.store(true, Ordering::Release);
    shared.changed.notify_all();
    let final_output = shared
        .publisher
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
        .map(|publisher| publisher.finish())
        .unwrap_or(Ok(()));
    server.stop();
    result.and(final_output)
}

async fn start_player(
    shared: &Shared,
    events: &flume::Sender<PlayerEvent>,
) -> Option<Arc<RuntimePlayer>> {
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
            Some(Arc::new(player))
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
    let maintaining = Arc::new(AtomicBool::new(false));
    let mut next_maintenance = Instant::now() + Duration::from_secs(30);
    let mut last_media = u64::MAX;
    let mut last_persist = Instant::now();
    let mut pending_playback = reducer::PendingPlayback::default();
    let mut sdk_authenticated = false;
    let mut pcm_baseline = 0;
    while !shared.shutdown.load(Ordering::Acquire) && !lifecycle::signalled() {
        let wants_spectrum = shared.has_spectrum();
        if let Some(player) = &player {
            if let Some(stats) = player.audio_stats() {
                let ready = sdk_authenticated && player.has_local_dsp();
                if !ready {
                    pcm_baseline = stats.processed_frames;
                }
                shared.update(|snapshot| {
                    snapshot.audio = Some(audio_telemetry(stats));
                    if snapshot.soloist_ready != ready {
                        snapshot.soloist_ready = ready;
                        reducer::readiness(snapshot);
                    }
                });
            }
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
                Ok(OwnerMessage::Command(command, generation, reply)) => {
                    // Timed-out clients must not leave commands queued across setup.
                    if reply.is_disconnected() { continue; }
                    if shared.generation.load(Ordering::Acquire) != generation {
                        let _ = reply.try_send(Err(IpcError::new(ErrorCode::SetupRequired, "Authorization changed before command acceptance")));
                        continue;
                    }
                    let command = controller::prepare_command(command, &shared.snapshot(), last_intent.clone(), restore_pending);
                    let proposed_intent = playback::is_load(&command).then(|| command.clone());
                    let stops_restore = matches!(command, PlayerCommand::Stop);
                    let result = if let Some(player) = &player {
                        shared.control.get().ok_or_else(|| IpcError::new(ErrorCode::Unavailable, "Control worker unavailable"))
                            .and_then(|control| control.enqueue(&shared, player.clone(), command, generation))
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
                observe_pcm_boundary(
                    &event, shared.snapshot().track.is_none(), &mut pcm_baseline,
                    || player.as_ref().and_then(|player| player.audio_stats()).map_or(0, |stats| stats.processed_frames),
                    || { if let Some(player) = &player { player.flush_audio(); } },
                );
                match &event {
                    PlayerEvent::Authentication { logged_in, .. } => sdk_authenticated = *logged_in,
                    PlayerEvent::Reconnecting => sdk_authenticated = false,
                    _ => {},
                }
                pending_playback.note(&event);
                let previous = shared.snapshot();
                if matches!(event, PlayerEvent::Authentication { logged_in: false, .. }) && (previous.soloist_ready || previous.owned_device_verified) {
                     shared.generation.fetch_add(1, Ordering::AcqRel);
                     let queue_revision = shared.native_queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear_authority();
                     shared.update(|snapshot| {
                         snapshot.queue = Default::default(); snapshot.queue_revision = queue_revision;
                         snapshot.queue_previous_total = 0; snapshot.queue_upcoming_total = 0;
                         snapshot.options_observed_revision = 0; snapshot.volume_observed_revision = 0;
                         snapshot.control_pending = false;
                         snapshot.control_error = Some(ErrorCode::SetupRequired);
                         snapshot.control_outcome = Some("Control cancelled after SDK authorization changed".into());
                     });
                    *shared.device.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                }
                let event = if let PlayerEvent::NativeQueue(queue) = event {
                    if previous.locally_active {
                        let mut native = shared.native_queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        native.observe(queue);
                        let mut prefix = previous.clone();
                        native.prefix(&mut prefix);
                        drop(native);
                        shared.update(|snapshot| {
                            snapshot.queue = prefix.queue;
                            snapshot.queue_revision = prefix.queue_revision;
                            snapshot.queue_previous_total = prefix.queue_previous_total;
                            snapshot.queue_upcoming_total = prefix.queue_upcoming_total;
                            snapshot.queue_observed_revision = snapshot.queue_observed_revision.saturating_add(1);
                        });
                    }
                    shared.observations.fetch_add(1, Ordering::AcqRel);
                    shared.changed.notify_all();
                    continue;
                } else { event };
                let pcm = player.as_ref().and_then(|player| player.audio_stats()).is_some_and(|stats| stats.processed_frames > pcm_baseline);
                shared.update(|snapshot| {
                    if snapshot.locally_active {
                        match &event {
                            PlayerEvent::Options { .. } => snapshot.options_observed_revision = snapshot.options_observed_revision.saturating_add(1),
                            PlayerEvent::Volume { .. } => snapshot.volume_observed_revision = snapshot.volume_observed_revision.saturating_add(1),
                            _ => {}
                        }
                    }
                    reducer::observe(snapshot, event, pcm);
                    if player.as_ref().is_some_and(|player| !player.has_local_dsp()) {
                        snapshot.soloist_ready = false; reducer::readiness(snapshot);
                    }
                });
                shared.observations.fetch_add(1, Ordering::AcqRel);
                shared.changed.notify_all();
            },
            event = media_received.recv_async() => if let (Ok(event), Some(player)) = (event, player.as_ref()) {
                let name=match &event { MediaControlEvent::Play=>"play",MediaControlEvent::Pause=>"pause",MediaControlEvent::Toggle=>"toggle",
                    MediaControlEvent::Stop=>"stop",MediaControlEvent::Next=>"next",MediaControlEvent::Previous=>"previous",_=>"other" };
                shared.update(|snapshot| {snapshot.media_event_count=snapshot.media_event_count.saturating_add(1);snapshot.last_media_command=Some(name.into());});
                let generation = shared.generation.load(Ordering::Acquire);
                let snapshot = shared.snapshot();
                if let Some(command) = media_command(event, &snapshot) {
                    let command = controller::prepare_command(command, &snapshot, last_intent.clone(), restore_pending);
                    let proposed = playback::is_load(&command).then(|| command.clone());
                    let stopping = matches!(command, PlayerCommand::Stop);
                    if shared.control.get().is_none_or(|control| control.enqueue(&shared, player.clone(), command, generation).is_err()) { shared.update(|snapshot| snapshot.message = Some("Native media command requires ready owned services".into())); }
                    else {
                        if let Some(intent) = proposed { last_intent = Some(intent); restore_pending = false; }
                        if stopping { restore_pending = false; }
                    }
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
        if !pending_playback.is_pending()
            && snapshot.playback == PlaybackStatus::Disconnected
            && snapshot.owned_device_verified
            && snapshot.locally_active
            && player
                .as_ref()
                .is_some_and(|player| player.has_local_dsp() && player.observed_playing())
        {
            if let Some(track) = &snapshot.track {
                pending_playback.note(&PlayerEvent::Playing {
                    uri: track.uri.clone(),
                    position_ms: snapshot
                        .observed_position
                        .unwrap_or(snapshot.position)
                        .at(now_ms(), track.duration_ms)
                        .min(u64::from(u32::MAX)) as u32,
                });
            }
        }
        if Instant::now() >= next_maintenance && !maintaining.swap(true, Ordering::AcqRel) {
            next_maintenance = Instant::now() + Duration::from_secs(30);
            maintain_api(shared.clone(), maintaining.clone());
        }
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
                .and_then(|player| player.audio_stats())
                .is_some_and(|stats| stats.processed_frames > pcm_baseline);
            if pcm {
                shared.update(|snapshot| {
                    pending_playback.confirm(snapshot, true);
                });
            }
        }
        if snapshot.soloist_ready
            && (!snapshot.owned_device_verified || !snapshot.catalog_available)
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
        snapshot.control_pending = false;
    });
    media.update(&shared.snapshot());
    Ok(())
}

fn verify_device(shared: Arc<Shared>, pending: Arc<AtomicBool>, id: String) {
    pending.store(true, Ordering::Release);
    let generation = shared.generation.load(Ordering::Acquire);
    std::thread::spawn(move || {
        let response = shared.api_call(generation, HttpMethod::Get, "/v1/me/player/devices", None);
        if shared.generation.load(Ordering::Acquire) != generation {
            pending.store(false, Ordering::Release);
            return;
        }
        let verified = match response {
            Ok(value) => controller::verify_owned_device(&value, &id),
            Err(error) => {
                shared.api_failure(&error);
                shared.update(|snapshot| {
                    snapshot.message = Some(format!("Owned-device verification: {error}"))
                });
                pending.store(false, Ordering::Release);
                return;
            }
        };
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
        shared.update(|snapshot| {
            if shared.generation.load(Ordering::Acquire) != generation {
                return;
            }
            observe_device_verification(snapshot, verified);
        });
        pending.store(false, Ordering::Release);
    });
}

fn observe_device_verification(snapshot: &mut Snapshot, verified: bool) {
    snapshot.catalog_available = true;
    snapshot.owned_device_verified = verified && snapshot.soloist_ready;
    reducer::readiness(snapshot);
    if !verified {
        snapshot.message = Some(
            "Owned Soloist device not found in this Web API account; reconcile authentication"
                .into(),
        );
    } else if snapshot.readiness == ServiceState::Ready {
        snapshot.message = None;
        if !snapshot.control_pending && snapshot.control_error == Some(ErrorCode::SetupRequired) {
            snapshot.control_error = None;
            snapshot.control_outcome = None;
        }
    }
    if verified
        && snapshot.track.is_some()
        && snapshot.metadata_status == MetadataStatus::Unavailable
    {
        snapshot.metadata_status = MetadataStatus::Loading;
    }
}

fn maintain_api(shared: Arc<Shared>, pending: Arc<AtomicBool>) {
    let generation = shared.generation.load(Ordering::Acquire);
    std::thread::spawn(move || {
        let Ok(_permit) = shared.api_gate.enter(Duration::from_secs(3)) else {
            pending.store(false, Ordering::Release);
            return;
        };
        if shared.generation.load(Ordering::Acquire) != generation {
            pending.store(false, Ordering::Release);
            return;
        }
        let result = shared
            .api
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .map(ApiClient::maintain);
        if let Some(result) = result {
            shared.update(|snapshot| {
                if shared.generation.load(Ordering::Acquire) != generation {
                    return;
                }
                let previously_failed = !snapshot.web_api_ready;
                snapshot.web_api_ready = result.is_ok();
                reducer::readiness(snapshot);
                match result {
                    Err(error) => {
                        snapshot.catalog_available = false;
                        reducer::readiness(snapshot);
                        snapshot.message = Some(error.to_string());
                    }
                    Ok(()) if previously_failed && snapshot.readiness == ServiceState::Ready => {
                        snapshot.message = None
                    }
                    _ => {}
                }
            });
        }
        pending.store(false, Ordering::Release);
    });
}

fn enrich(shared: Arc<Shared>, pending: Arc<AtomicBool>, uri: String) {
    pending.store(true, Ordering::Release);
    let generation = shared.generation.load(Ordering::Acquire);
    std::thread::spawn(move || {
        let mut parts = uri.split(':');
        let kind = parts.nth(1).unwrap_or("");
        let id = parts.next().unwrap_or("");
        let endpoint = match kind {
            "track" => "tracks",
            "episode" => "episodes",
            _ => "",
        };
        let result = if !endpoint.is_empty()
            && id.len() == 22
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            Some(shared.api_call(
                generation,
                HttpMethod::Get,
                &format!("/v1/{endpoint}/{id}"),
                None,
            ))
        } else {
            None
        };
        if shared.generation.load(Ordering::Acquire) != generation {
            pending.store(false, Ordering::Release);
            return;
        }
        let value = match result {
            Some(Ok(value)) => Some(value),
            Some(Err(error)) => {
                shared.api_failure(&error);
                None
            }
            None => None,
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

fn audio_telemetry(
    stats: soloist_runtime::pcm::AudioStats,
) -> crate::domain::playback::AudioTelemetry {
    crate::domain::playback::AudioTelemetry {
        timing: Some(crate::domain::playback::AudioTimingTelemetry {
            max_capture_gap_micros: stats.timing.max_capture_gap_micros,
            max_write_gap_micros: stats.timing.max_write_gap_micros,
            max_dispatch_micros: stats.timing.max_dispatch_micros,
            max_transfer_micros: stats.timing.max_transfer_micros,
            max_wait_micros: Some(stats.timing.max_wait_micros),
            max_iteration_gap_micros: Some(stats.timing.max_iteration_gap_micros),
            last_underflow_capture_gap_micros: stats.timing.last_underflow_capture_gap_micros,
            last_underflow_pending_frames: stats.timing.last_underflow_pending_frames,
            last_underflow_write_credit_frames: stats.timing.last_underflow_write_credit_frames,
            observed_underflows: stats.timing.observed_underflows,
        }),
        processed_frames: stats.processed_frames,
        peak_pending_frames: stats.peak_pending_frames,
        estimated_latency_micros: stats.estimated_latency_micros,
        latency_valid: stats.latency_valid,
        dropped_frames: stats.dropped_frames,
        capture_hole_frames: Some(stats.capture_hole_frames),
        playback_underflows: Some(stats.playback_underflows),
        worker_wakes: stats.worker_wakes,
        resets: stats.resets,
        fft_frames: stats.fft_frames,
        fft_compute_micros: stats.fft_compute_micros,
        peak_fft_compute_micros: stats.peak_fft_compute_micros,
        recovery_attempts: stats.recovery_attempts,
        health: stats.health,
    }
}

fn observe_pcm_boundary(
    event: &PlayerEvent,
    missing_track: bool,
    baseline: &mut u64,
    processed_frames: impl FnOnce() -> u64,
    flush: impl FnOnce(),
) {
    // Per-item qualification needs new PCM even when the SDK stream continues.
    if matches!(event, PlayerEvent::TrackChanged { .. })
        || (matches!(event, PlayerEvent::Playing { .. }) && missing_track)
    {
        *baseline = processed_frames();
    }
    if matches!(
        event,
        PlayerEvent::Stopped | PlayerEvent::Reconnecting | PlayerEvent::LocallyInactive
    ) {
        flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_readiness_clears_stale_setup_error_but_not_write_uncertainty() {
        for (pending, error, expected) in [
            (false, ErrorCode::SetupRequired, None),
            (
                true,
                ErrorCode::SetupRequired,
                Some(ErrorCode::SetupRequired),
            ),
            (
                false,
                ErrorCode::WriteUncertain,
                Some(ErrorCode::WriteUncertain),
            ),
        ] {
            let mut snapshot = fixture().snapshot();
            snapshot.owned_device_verified = false;
            snapshot.control_pending = pending;
            snapshot.control_error = Some(error);
            snapshot.control_outcome = Some("earlier control error".into());
            observe_device_verification(&mut snapshot, false);
            assert_eq!(snapshot.control_error, Some(error));
            observe_device_verification(&mut snapshot, true);
            assert_eq!(snapshot.readiness, ServiceState::Ready);
            assert_eq!(snapshot.control_error, expected);
            if expected.is_none() {
                assert!(snapshot.control_outcome.is_none());
            }
        }
    }

    #[test]
    fn natural_track_boundary_requalifies_pcm_without_flushing_the_stream() {
        let event = PlayerEvent::TrackChanged {
            uri: "spotify:track:next".into(),
        };
        let mut baseline = 0;
        let mut flushes = 0;
        observe_pcm_boundary(&event, false, &mut baseline, || 1234, || flushes += 1);
        assert_eq!(baseline, 1234);
        assert_eq!(flushes, 0, "metadata is not a PCM discontinuity");
        let playing = PlayerEvent::Playing {
            uri: "spotify:track:next".into(),
            position_ms: 0,
        };
        observe_pcm_boundary(&playing, false, &mut baseline, || 1300, || flushes += 1);
        assert_eq!(baseline, 1234);
        observe_pcm_boundary(&playing, true, &mut baseline, || 1400, || flushes += 1);
        assert_eq!(baseline, 1400, "Stop-to-Play must exclude old PCM");
        for event in [
            PlayerEvent::Stopped,
            PlayerEvent::Reconnecting,
            PlayerEvent::LocallyInactive,
        ] {
            observe_pcm_boundary(
                &event,
                false,
                &mut baseline,
                || panic!("no new item"),
                || flushes += 1,
            );
        }
        assert_eq!(flushes, 3);
    }
    pub(super) fn fixture() -> Arc<Shared> {
        let mut snapshot = Snapshot::starting("/unused/test/config".into(), "Device".into());
        snapshot.web_api_ready = true;
        snapshot.catalog_available = true;
        snapshot.soloist_ready = true;
        snapshot.native_media_ready = true;
        snapshot.owned_device_verified = true;
        snapshot.locally_active = true;
        snapshot.playback = PlaybackStatus::Playing;
        snapshot.position.speed = 1.0;
        reducer::readiness(&mut snapshot);
        let (commands, _) = flume::bounded(1);
        Arc::new(Shared {
            snapshot: Mutex::new(snapshot),
            changed: Condvar::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            api: Mutex::new(None),
            api_gate: Arc::new(api_gate::ApiGate::default()),
            publisher: Mutex::new(None),
            device: Mutex::new(Some("owned".into())),
            subscriptions: Mutex::new(HashMap::new()),
            oauth: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            commands,
            control: OnceLock::new(),
            lifetime: OnceLock::new(),
            native_queue: Mutex::new(queue::NativeQueue::default()),
            observations: AtomicU64::new(0),
        })
    }
    #[test]
    fn transient_catalog_failure_does_not_invalidate_previously_owned_local_audio() {
        let owner = fixture();
        owner.api_failure(&ApiError::RateLimited {
            retry_after_secs: Some(300),
        });
        let snapshot = owner.snapshot();
        assert_eq!(snapshot.readiness, ServiceState::Degraded);
        assert!(!snapshot.catalog_available);
        assert!(snapshot.owned_device_verified);
        assert_eq!(snapshot.playback, PlaybackStatus::Playing);
        assert_eq!(snapshot.position.speed, 1.0);
    }
    #[test]
    fn a_restricted_resource_is_not_global_logout_and_revocation_is_explicit() {
        let owner = fixture();
        owner.api_failure(&ApiError::Restricted);
        assert_eq!(owner.snapshot().readiness, ServiceState::Ready);
        owner.api_failure(&ApiError::Unauthorized);
        assert!(!owner.snapshot().web_api_ready);
        assert_eq!(owner.snapshot().readiness, ServiceState::Degraded);
        assert!(owner.snapshot().owned_device_verified);
    }

    #[test]
    fn detached_worker_retains_the_actual_server_lock_until_its_last_shared_owner_exits() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::in_directory(root.path().canonicalize().unwrap().join("owner"));
        let shared = fixture();
        let released = Arc::downgrade(&shared);
        let handler: Arc<server::Handler> = Arc::new(|_| Ok(Payload::Accepted));
        let server =
            server::Server::bind(paths.clone(), "/test/config".into(), handler.clone()).unwrap();
        shared
            .lifetime
            .set(server.publisher_lock().unwrap())
            .unwrap();
        let detached = shared.clone();
        let (release, received) = flume::bounded::<()>(1);
        let worker = std::thread::spawn(move || {
            received.recv().unwrap();
            drop(detached);
        });
        drop(shared);
        drop(server);
        assert!(
            server::Server::bind(paths.clone(), "/test/config".into(), handler.clone()).is_err()
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(released.strong_count(), 0);
        // Closing the last descriptor and observing an uncontended Darwin
        // flock need not happen in the same scheduler tick. Still require real
        // reacquisition within a deadline; never release another owner's lock.
        let started = Instant::now();
        let mut probes = 0;
        loop {
            match server::Server::bind(paths.clone(), "/test/config".into(), handler.clone()) {
                Ok(_replacement) => {
                    if probes > 0 {
                        eprintln!(
                            "Owner lock release observed after {probes} contention probes ({:?})",
                            started.elapsed()
                        );
                    }
                    break;
                }
                Err(error)
                    if started.elapsed() < Duration::from_secs(2)
                        && error.chain().any(|cause| {
                            cause
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
                        }) =>
                {
                    probes += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!(
                    "server ownership was not released after the last worker exited: {error:#}"
                ),
            }
        }
    }

    #[test]
    fn typed_queue_pages_keep_uid_source_and_duplicates_without_growing_status() {
        use crate::domain::{QueueEntry, QueueSnapshot, QueueSource};
        let shared = fixture();
        let entries: Vec<_> = (0..5107)
            .map(|i| QueueEntry {
                uid: format!("native-{i}"),
                source: QueueSource::Queue,
                uri: "spotify:episode:0123456789abcdefghijkl".into(),
                title: None,
            })
            .collect();
        let mut native = shared.native_queue.lock().unwrap();
        native.observe(QueueSnapshot {
            upcoming: entries.clone(),
            previous: vec![],
            complete_catalog_context: true,
        });
        let mut prefix = shared.snapshot();
        native.prefix(&mut prefix);
        drop(native);
        shared.update(|s| *s = prefix);
        let Payload::Snapshot(status) = shared.handle(Operation::Status).unwrap() else {
            panic!("status")
        };
        assert_eq!(status.queue.upcoming.len(), 10);
        assert_eq!(status.queue_upcoming_total, 5107);
        assert!(!status.queue.complete_catalog_context);
        let Payload::QueuePage(page) = shared
            .handle(Operation::QueuePage {
                revision: Some(status.queue_revision),
                offset: 5090,
                limit: 50,
                previous: false,
                metadata: false,
            })
            .unwrap()
        else {
            panic!("queue page")
        };
        assert_eq!(page.entries, entries[5090..]);
        assert_eq!(page.session_id, status.session_id);
        assert!(!page.complete_catalog_context);
        assert!(serde_json::to_vec(&page).unwrap().len() < crate::ipc::protocol::MAX_FRAME);
        shared
            .native_queue
            .lock()
            .unwrap()
            .observe(QueueSnapshot::default());
        let error = shared
            .handle(Operation::QueuePage {
                revision: Some(page.revision),
                offset: 0,
                limit: 50,
                previous: false,
                metadata: false,
            })
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::CollectionChanged);
    }
}
