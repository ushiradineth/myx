//! One bounded serial writer for both SDK commands and owned-device Web API writes.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{api_error, Shared};
use crate::domain::{PlaybackStatus, PlayerCommand, Snapshot};
use crate::ipc::protocol::{ErrorCode, IpcError};
use crate::player::intents::{self, LoadPlan, Qualification};
use crate::soloist::RuntimePlayer;
use crate::webapi::{catalog::collect_pages, ApiError, HttpMethod};

struct Intent<P> {
    id: u64,
    generation: u64,
    command: PlayerCommand,
    player: P,
}

struct Mailbox<P> {
    queue: Mutex<VecDeque<Intent<P>>>,
    changed: Condvar,
    next: AtomicU64,
    latest_load: AtomicU64,
}

impl<P> Default for Mailbox<P> {
    fn default() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            changed: Condvar::new(),
            next: AtomicU64::new(0),
            latest_load: AtomicU64::new(0),
        }
    }
}

pub(super) struct Control(Arc<Mailbox<Arc<RuntimePlayer>>>);

pub(super) fn is_load(command: &PlayerCommand) -> bool {
    matches!(
        command,
        PlayerCommand::PlayContext { .. }
            | PlayerCommand::PlayContextAt { .. }
            | PlayerCommand::PlayTracks { .. }
            | PlayerCommand::PlayTracksAt { .. }
            | PlayerCommand::PlayLiked { .. }
    )
}

impl Control {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<Self> {
        let mailbox = Arc::new(Mailbox::default());
        let worker = mailbox.clone();
        std::thread::Builder::new()
            .name("myx-control".into())
            .spawn(move || run_worker(shared, worker, execute))?;
        Ok(Self(mailbox))
    }

    pub fn enqueue(
        &self,
        shared: &Shared,
        player: Arc<RuntimePlayer>,
        command: PlayerCommand,
        generation: u64,
    ) -> Result<(), IpcError> {
        self.0
            .enqueue_at_generation(shared, player, command, generation)
    }
}

fn run_worker<P>(
    shared: Arc<Shared>,
    worker: Arc<Mailbox<P>>,
    execute: impl Fn(&Shared, &Mailbox<P>, &Intent<P>) -> Result<bool, IpcError>,
) {
    // Shared retains a clone of the actual open daemon lifetime lock. A
    // timed-out HTTP call may finish after owner/audio shutdown, never
    // after a replacement owner acquires this same session lock.
    let mut unsettled = false;
    let mut generation = shared.generation.load(Ordering::Acquire);
    while !shared.shutdown.load(Ordering::Acquire) {
        let intent = {
            let mut queue = worker
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if queue.is_empty() {
                queue = worker
                    .changed
                    .wait_timeout(queue, Duration::from_secs(1))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            queue.pop_front()
        };
        let Some(intent) = intent else { continue };
        if cancelled(&shared, &worker, &intent) {
            shared.update(|s| {
                        if s.control_id == intent.id {
                            s.control_pending = false;
                            s.control_error = Some(ErrorCode::Busy);
                            s.control_outcome = Some("Queued control cancelled by a newer load, authorization change or shutdown".into());
                        }
                    });
            continue;
        }
        if generation != intent.generation {
            generation = intent.generation;
            unsettled = false;
        }
        shared.update(|s| {
            if s.control_id != intent.id {
                return;
            }
            s.control_pending = true;
            s.control_error = None;
            s.control_outcome = Some("Executing queued control; awaiting observations".into());
        });
        let result = if unsettled
            && !matches!(
                intent.command,
                PlayerCommand::Stop | PlayerCommand::Equalizer { .. }
            ) {
            Err(IpcError::new(
                ErrorCode::WriteUncertain,
                "Previous write is unconfirmed; explicitly Stop and observe before another control",
            ))
        } else {
            execute(&shared, &worker, &intent)
        };
        if matches!(intent.command, PlayerCommand::Stop) && matches!(result, Ok(true)) {
            unsettled = false;
        } else if result
            .as_ref()
            .is_err_and(|e| e.code == ErrorCode::WriteUncertain)
        {
            unsettled = true;
        }
        if shared.generation.load(Ordering::Acquire) != intent.generation {
            continue;
        }
        shared.update(|s| {
            if s.control_id != intent.id {
                return;
            }
            s.control_pending = false;
            match result {
                Ok(observed) => {
                    if !observed && !matches!(intent.command, PlayerCommand::Queue) {
                        s.control_error = Some(ErrorCode::Timeout);
                    }
                    s.control_outcome = Some(if observed {
                        "Matching native state observed (not load/offset qualification)".into()
                    } else {
                        "Command sent; matching state was not observed in time. A new selection can replace it".into()
                    });
                }
                Err(error) => {
                    s.control_error = Some(error.code);
                    s.control_outcome = Some(error.to_string());
                }
            }
        });
    }
}

impl<P> Mailbox<P> {
    #[cfg(test)]
    fn enqueue(&self, shared: &Shared, player: P, command: PlayerCommand) -> Result<(), IpcError> {
        self.enqueue_at_generation(
            shared,
            player,
            command,
            shared.generation.load(Ordering::Acquire),
        )
    }

    fn enqueue_at_generation(
        &self,
        shared: &Shared,
        player: P,
        command: PlayerCommand,
        generation: u64,
    ) -> Result<(), IpcError> {
        // Validate descriptors before accepting; resolution failures are asynchronous.
        intents::plan(&command).map_err(api_error)?;
        if shared.shutdown.load(Ordering::Acquire)
            || shared.generation.load(Ordering::Acquire) != generation
        {
            return Err(IpcError::new(
                ErrorCode::SetupRequired,
                "Authorization changed before command acceptance",
            ));
        }
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if is_load(&command) {
            // Only collapse consecutive loads. A pause/seek/option is an ordering barrier.
            while queue.back().is_some_and(|intent| is_load(&intent.command)) {
                queue.pop_back();
            }
        }
        if queue.len() >= 64 {
            return Err(IpcError::new(
                ErrorCode::Busy,
                "Player command queue is busy",
            ));
        }
        let id = self.next.fetch_add(1, Ordering::AcqRel) + 1;
        if (is_load(&command) && queue.is_empty()) || matches!(command, PlayerCommand::Stop) {
            self.latest_load.store(id, Ordering::Release);
        }
        shared.update(|s| {
            s.control_id = id;
            s.control_pending = true;
            s.control_error = None;
            s.control_outcome =
                Some("Control queued (acceptance is not playback confirmation)".into());
        });
        queue.push_back(Intent {
            id,
            generation,
            command,
            player,
        });
        drop(queue);
        self.changed.notify_one();
        Ok(())
    }
}

fn cancelled<P>(shared: &Shared, mailbox: &Mailbox<P>, intent: &Intent<P>) -> bool {
    shared.shutdown.load(Ordering::Acquire)
        || shared.generation.load(Ordering::Acquire) != intent.generation
        || (is_load(&intent.command) && mailbox.latest_load.load(Ordering::Acquire) > intent.id)
}

fn check<P>(shared: &Shared, mailbox: &Mailbox<P>, intent: &Intent<P>) -> Result<(), IpcError> {
    if cancelled(shared, mailbox, intent) {
        Err(IpcError::new(
            ErrorCode::Busy,
            "Control cancelled or superseded before its next write",
        ))
    } else {
        Ok(())
    }
}

/// All source reads retain original rows. Playlist detail responses are explicitly
/// uncached by the owner ApiClient, including both revision checks.
fn resolve(
    mut load: LoadPlan,
    mut fetch: impl FnMut(&str) -> Result<Value, ApiError>,
) -> Result<LoadPlan, ApiError> {
    load = match load {
        LoadPlan::ResolveLiked { command } => {
            let rows = collect_pages(&mut fetch, "/v1/me/tracks?limit=50", None)?;
            intents::resolve_liked(&command, &rows)?
        }
        LoadPlan::ResolveContext { command } => {
            let uri = match &command {
                PlayerCommand::PlayContext { uri, .. }
                | PlayerCommand::PlayContextAt { uri, .. } => uri,
                _ => return Err(ApiError::InvalidRequest),
            };
            let mut parts = uri.split(':');
            let kind = parts.nth(1).ok_or(ApiError::InvalidRequest)?;
            let id = parts.next().ok_or(ApiError::InvalidRequest)?;
            let playlist = kind == "playlist";
            let revision_path = format!("/v1/playlists/{id}?fields=snapshot_id");
            let before = if playlist {
                Some(revision(fetch(&revision_path)?)?)
            } else {
                None
            };
            let path = if playlist {
                format!("/v1/playlists/{id}/items?limit=50")
            } else {
                format!("/v1/albums/{id}/tracks?limit=50")
            };
            let rows = collect_pages(&mut fetch, &path, None)?;
            if let Some(before) = before {
                if before != revision(fetch(&revision_path)?)? {
                    return Err(ApiError::CollectionChanged);
                }
            }
            intents::resolve_context(&command, &rows)?
        }
        load => load,
    };
    Ok(load)
}

fn revision(value: Value) -> Result<String, ApiError> {
    value["snapshot_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(ApiError::InvalidResponse)
}

fn owned(shared: &Shared) -> Result<String, IpcError> {
    let snapshot = shared.snapshot();
    if !snapshot.soloist_ready || !snapshot.owned_device_verified {
        return Err(IpcError::new(
            ErrorCode::SetupRequired,
            "Verified owned SDK device is required",
        ));
    }
    shared
        .device
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .ok_or_else(|| {
            IpcError::new(
                ErrorCode::SetupRequired,
                "Owned SDK public device identity is unavailable",
            )
        })
}

fn api_write(
    shared: &Shared,
    mailbox: &Mailbox<Arc<RuntimePlayer>>,
    intent: &Intent<Arc<RuntimePlayer>>,
    method: HttpMethod,
    path: &str,
    body: Option<Value>,
) -> Result<(), IpcError> {
    check(shared, mailbox, intent)?;
    owned(shared)?;
    shared
        .api_call(intent.generation, method, path, body)
        .map(|_| ())
        .map_err(api_error)
}

fn wait<P>(
    shared: &Shared,
    mailbox: &Mailbox<P>,
    intent: &Intent<P>,
    after: u64,
    predicate: impl Fn(&Snapshot) -> bool,
) -> Result<bool, IpcError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut snapshot = shared
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        // Delivery uncertainty is handled by the write itself, not this wait.
        // A new absolute selection may replace an accepted command's observation
        // wait, but Stop reconciliation remains an ordering barrier.
        if shared.shutdown.load(Ordering::Acquire)
            || shared.generation.load(Ordering::Acquire) != intent.generation
        {
            return Err(IpcError::new(
                ErrorCode::WriteUncertain,
                "Write observation interrupted; reconcile before retrying",
            ));
        }
        if !matches!(intent.command, PlayerCommand::Stop)
            && mailbox.latest_load.load(Ordering::Acquire) > intent.id
        {
            return Err(IpcError::new(
                ErrorCode::Busy,
                "Command observation superseded by a newer selection",
            ));
        }
        if shared.observations.load(Ordering::Acquire) > after && predicate(&snapshot) {
            return Ok(true);
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(false);
        };
        snapshot = shared
            .changed
            .wait_timeout(snapshot, left.min(Duration::from_millis(100)))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

fn sdk(result: anyhow::Result<()>) -> Result<(), IpcError> {
    result.map_err(|_| {
        IpcError::new(
            ErrorCode::Unavailable,
            "SDK command was not accepted; inspect observed state",
        )
    })
}

fn apply_volume<P>(
    shared: &Shared,
    mailbox: &Mailbox<P>,
    intent: &Intent<P>,
    percent: u8,
    submit: impl FnOnce() -> Result<(), IpcError>,
) -> Result<bool, IpcError> {
    let snapshot = shared.snapshot();
    // Repeated keys can queue the same absolute target before the viewer sees
    // its first update. An unchanged SDK volume need not emit another event.
    if snapshot.locally_active
        && snapshot.volume_observed_revision > 0
        && snapshot.volume == percent
    {
        return Ok(true);
    }
    let after = shared.observations.load(Ordering::Acquire);
    submit()?;
    wait(shared, mailbox, intent, after, |s| {
        s.volume_observed_revision > snapshot.volume_observed_revision && s.volume == percent
    })
}

fn load_observed(
    snapshot: &Snapshot,
    before: &Snapshot,
    body: &Value,
    expected_uri: Option<&str>,
) -> bool {
    if !snapshot.locally_active
        || !matches!(
            snapshot.playback,
            PlaybackStatus::Playing | PlaybackStatus::Paused
        )
    {
        return false;
    }
    let Some(track) = &snapshot.track else {
        return false;
    };
    let target = body["uris"]
        .as_array()
        .and_then(|uris| {
            uris.get(body["offset"]["position"].as_u64().unwrap_or(0) as usize)
                .and_then(Value::as_str)
        })
        .or_else(|| body["offset"]["uri"].as_str())
        .or(expected_uri);
    if target.is_some_and(|uri| uri != track.uri) {
        return false;
    }
    if let Some(requested) = body["position_ms"].as_u64() {
        let observed = snapshot
            .observed_position
            .unwrap_or(snapshot.position)
            .position_ms;
        if observed.abs_diff(requested) > 1000 {
            return false;
        }
    }
    if body["context_uri"]
        .as_str()
        .is_some_and(|uri| snapshot.context_uri.as_deref() != Some(uri))
    {
        return false;
    }
    // An old playing model plus an unrelated volume observation is not a load receipt.
    snapshot.track.as_ref().map(|t| &t.uri) != before.track.as_ref().map(|t| &t.uri)
        || snapshot.observed_position != before.observed_position
        || snapshot.context_uri != before.context_uri
}

fn apply_options(
    shared: &Shared,
    mailbox: &Mailbox<Arc<RuntimePlayer>>,
    intent: &Intent<Arc<RuntimePlayer>>,
    shuffle: bool,
    repeat: crate::domain::RepeatMode,
    web_api_device: Option<&str>,
) -> Result<bool, IpcError> {
    for repeat_option in [true, false] {
        check(shared, mailbox, intent)?;
        owned(shared)?;
        let before = shared.snapshot();
        if before.options_observed_revision > 0
            && if repeat_option {
                before.repeat == repeat
            } else {
                before.shuffle == shuffle
            }
        {
            continue;
        }
        let after = shared.observations.load(Ordering::Acquire);
        if let Some(device) = web_api_device {
            let path = if repeat_option {
                let state = match repeat {
                    crate::domain::RepeatMode::Off => "off",
                    crate::domain::RepeatMode::One => "track",
                    crate::domain::RepeatMode::All => "context",
                };
                format!("/v1/me/player/repeat?device_id={device}&state={state}")
            } else {
                format!("/v1/me/player/shuffle?device_id={device}&state={shuffle}")
            };
            api_write(shared, mailbox, intent, HttpMethod::Put, &path, None)?;
        } else if repeat_option {
            sdk(intent.player.set_repeat(repeat))?;
        } else {
            sdk(intent.player.shuffle(shuffle))?;
        }
        if !wait(shared, mailbox, intent, after, |s| {
            s.options_observed_revision > before.options_observed_revision
                && if repeat_option {
                    s.repeat == repeat
                } else {
                    s.shuffle == shuffle
                }
        })? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn execute(
    shared: &Shared,
    mailbox: &Mailbox<Arc<RuntimePlayer>>,
    intent: &Intent<Arc<RuntimePlayer>>,
) -> Result<bool, IpcError> {
    let player = &intent.player;
    check(shared, mailbox, intent)?;
    if let PlayerCommand::Equalizer { settings } = &intent.command {
        player.set_equalizer(*settings);
        shared.update(|s| s.equalizer = settings.normalized());
        return Ok(true);
    }
    if matches!(intent.command, PlayerCommand::Stop) {
        let after = shared.observations.load(Ordering::Acquire);
        player.stop();
        return wait(shared, mailbox, intent, after, |s| {
            matches!(s.playback, PlaybackStatus::Paused | PlaybackStatus::Stopped)
        });
    }
    let device = owned(shared)?;
    let load = intents::plan(&intent.command).map_err(api_error)?;
    let load = load
        .map(|load| {
            resolve(load, |path| {
                if cancelled(shared, mailbox, intent) {
                    return Err(ApiError::Busy);
                }
                shared.api_call(intent.generation, HttpMethod::Get, path, None)
            })
        })
        .transpose()
        .map_err(api_error)?;
    check(shared, mailbox, intent)?;
    let snapshot = shared.snapshot();
    let explicit_play = load.is_some()
        || matches!(intent.command, PlayerCommand::Play)
        || (matches!(intent.command, PlayerCommand::Toggle)
            && snapshot.playback != PlaybackStatus::Playing);
    if !snapshot.locally_active {
        if !explicit_play {
            return Err(IpcError::new(
                ErrorCode::Restricted,
                "Control requires locally active playback",
            ));
        }
        // API loads have one documented owned-device transfer. Direct SDK
        // playables/Play use their single SDK activation route instead.
        // Never transfer on login, attach or verification.
        if matches!(load.as_ref(), Some(LoadPlan::WebApi { .. })) {
            let after = shared.observations.load(Ordering::Acquire);
            api_write(
                shared,
                mailbox,
                intent,
                HttpMethod::Put,
                "/v1/me/player",
                Some(json!({"device_ids":[device],"play":false})),
            )?;
            if !wait(shared, mailbox, intent, after, |s| s.locally_active)? {
                return Err(IpcError::new(
                    ErrorCode::Timeout,
                    "Owned-device activation has not been observed; no playback write sent",
                ));
            }
        }
    }
    let snapshot = shared.snapshot();
    let after = shared.observations.load(Ordering::Acquire);
    if let Some(load) = load {
        match load {
            LoadPlan::SdkPlayable {
                uri,
                position_ms,
                shuffle,
            } => {
                shared.update(|s| s.load_requires_live_qualification = false);
                check(shared, mailbox, intent)?;
                sdk(player.play_context_at(uri.clone(), None, position_ms, shuffle))?;
                let target = json!({"uris":[uri],"position_ms":position_ms});
                if !wait(shared, mailbox, intent, after, |s| {
                    load_observed(s, &snapshot, &target, None)
                })? {
                    return Ok(false);
                }
                apply_options(shared, mailbox, intent, shuffle, snapshot.repeat, None)
            }
            LoadPlan::WebApi {
                body,
                shuffle,
                qualification,
                expected_uri,
            } => {
                shared.update(|s| {
                    s.load_requires_live_qualification =
                        qualification == Qualification::ExplicitArrayOffsetRequiresLive
                });
                let target = body.clone();
                api_write(
                    shared,
                    mailbox,
                    intent,
                    HttpMethod::Put,
                    &format!("/v1/me/player/play?device_id={device}"),
                    Some(body),
                )?;
                // A 204 is not Playing. Wait for local state before options;
                // never mix a queued SDK write with a later HTTP load.
                if !wait(shared, mailbox, intent, after, |s| {
                    load_observed(s, &snapshot, &target, expected_uri.as_deref())
                })? {
                    return Ok(false);
                }
                apply_options(
                    shared,
                    mailbox,
                    intent,
                    shuffle,
                    snapshot.repeat,
                    Some(&device),
                )
            }
            _ => unreachable!("source resolution returns an executable load"),
        }
    } else {
        if snapshot.track.is_none()
            && matches!(
                intent.command,
                PlayerCommand::Pause
                    | PlayerCommand::Next
                    | PlayerCommand::Previous
                    | PlayerCommand::Seek { .. }
            )
        {
            return Err(IpcError::new(
                ErrorCode::Restricted,
                "This control requires an observed local item",
            ));
        }
        let action = match &intent.command {
            PlayerCommand::Play => Some("play"),
            PlayerCommand::Pause => Some("pause"),
            PlayerCommand::Toggle => Some(if snapshot.playback == PlaybackStatus::Playing {
                "pause"
            } else {
                "play"
            }),
            PlayerCommand::Next => Some("skip_next"),
            PlayerCommand::Previous => Some("skip_prev"),
            PlayerCommand::Seek { .. } => Some("seek"),
            PlayerCommand::Shuffle { .. } => Some("shuffle"),
            PlayerCommand::Repeat { .. } => Some("set_repeat"),
            PlayerCommand::Add { .. } => Some("add_to_queue"),
            _ => None,
        };
        if action.is_some_and(|action| {
            snapshot
                .available_actions
                .as_ref()
                .is_some_and(|actions| !actions.iter().any(|a| a == action))
        }) {
            return Err(IpcError::new(
                ErrorCode::Restricted,
                "This control is not currently available for the observed item",
            ));
        }
        match &intent.command {
            PlayerCommand::Play => sdk(player.play())?,
            PlayerCommand::Pause => sdk(player.pause())?,
            PlayerCommand::Toggle => {
                if snapshot.playback == PlaybackStatus::Playing {
                    sdk(player.pause())?
                } else {
                    sdk(player.play())?
                }
            }
            PlayerCommand::Next => sdk(player.next())?,
            PlayerCommand::Previous => sdk(player.prev())?,
            PlayerCommand::Seek { position_ms } => sdk(player.seek(*position_ms))?,
            PlayerCommand::Volume { percent } if *percent <= 100 => {
                return apply_volume(shared, mailbox, intent, *percent, || {
                    sdk(player.set_volume(((u32::from(*percent) * 65535) / 100) as u16))
                });
            }
            PlayerCommand::Shuffle { enabled } => sdk(player.shuffle(*enabled))?,
            PlayerCommand::Repeat { mode } => sdk(player.set_repeat(*mode))?,
            PlayerCommand::Queue => sdk(player.request_queue())?,
            PlayerCommand::Add { uri } if intents::valid_uri(uri, &["episode"]) => {
                api_write(
                    shared,
                    mailbox,
                    intent,
                    HttpMethod::Post,
                    &format!("/v1/me/player/queue?device_id={device}&uri={uri}"),
                    None,
                )?;
                sdk(player.request_queue())?;
                return wait(shared, mailbox, intent, after, |s| {
                    s.queue_observed_revision > snapshot.queue_observed_revision
                        && s.queue_revision != snapshot.queue_revision
                });
            }
            PlayerCommand::Add { uri } => sdk(player.add_to_queue(uri.clone()))?,
            _ => {
                return Err(IpcError::new(
                    ErrorCode::InvalidRequest,
                    "Invalid playback control",
                ))
            }
        }
        wait(shared, mailbox, intent, after, |s| match &intent.command {
            PlayerCommand::Shuffle { enabled } => {
                s.options_observed_revision > snapshot.options_observed_revision
                    && s.shuffle == *enabled
            }
            PlayerCommand::Repeat { mode } => {
                s.options_observed_revision > snapshot.options_observed_revision
                    && s.repeat == *mode
            }
            PlayerCommand::Pause => s.playback == PlaybackStatus::Paused,
            PlayerCommand::Play => s.playback == PlaybackStatus::Playing,
            PlayerCommand::Toggle => {
                s.playback
                    == if snapshot.playback == PlaybackStatus::Playing {
                        PlaybackStatus::Paused
                    } else {
                        PlaybackStatus::Playing
                    }
            }
            PlayerCommand::Next | PlayerCommand::Previous => {
                s.track.as_ref().map(|t| &t.uri) != snapshot.track.as_ref().map(|t| &t.uri)
                    || s.queue_revision != snapshot.queue_revision
            }
            PlayerCommand::Queue => s.queue_observed_revision > snapshot.queue_observed_revision,
            PlayerCommand::Add { .. } => s.queue_revision != snapshot.queue_revision,
            PlayerCommand::Seek { .. } => s.observed_position != snapshot.observed_position,
            _ => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "spotify:track:0123456789abcdefghijkl";

    #[test]
    fn repeated_volume_target_does_not_write_or_fence_later_controls() {
        let shared = super::super::tests::fixture();
        shared.update(|s| {
            s.locally_active = true;
            s.volume = 50;
            s.volume_observed_revision = 1;
        });
        let mailbox = Arc::new(Mailbox::default());
        for _ in 0..3 {
            mailbox
                .enqueue(&shared, (), PlayerCommand::Volume { percent: 55 })
                .unwrap();
        }
        mailbox.enqueue(&shared, (), PlayerCommand::Pause).unwrap();
        let writes = AtomicU64::new(0);
        run_worker(shared.clone(), mailbox, |state, input, intent| {
            if let PlayerCommand::Volume { percent } = intent.command {
                let result = apply_volume(state, input, intent, percent, || {
                    if writes.fetch_add(1, Ordering::AcqRel) == 0 {
                        state.update(|s| {
                            s.volume = percent;
                            s.volume_observed_revision += 1;
                        });
                        state.observations.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(())
                });
                if !matches!(result, Ok(true)) {
                    state.shutdown.store(true, Ordering::Release);
                }
                assert!(
                    matches!(result, Ok(true)),
                    "duplicate volume must already be satisfied"
                );
                result
            } else {
                state.shutdown.store(true, Ordering::Release);
                Ok(true)
            }
        });
        assert_eq!(writes.load(Ordering::Acquire), 1);
        assert_eq!(shared.snapshot().control_error, None);
        assert!(!shared.snapshot().control_pending);
    }

    #[test]
    fn volume_default_or_inactive_state_is_not_confirmation() {
        for (active, revision) in [(true, 0), (false, 1)] {
            let shared = super::super::tests::fixture();
            shared.update(|s| {
                s.locally_active = active;
                s.volume = 100;
                s.volume_observed_revision = revision;
            });
            let mailbox = Mailbox::default();
            mailbox
                .enqueue(&shared, (), PlayerCommand::Volume { percent: 100 })
                .unwrap();
            let intent = mailbox.queue.lock().unwrap().pop_front().unwrap();
            let result = apply_volume(&shared, &mailbox, &intent, 100, || {
                Err(IpcError::new(ErrorCode::Unavailable, "not submitted"))
            });
            assert!(matches!(result, Err(error) if error.code == ErrorCode::Unavailable));
        }
    }

    #[test]
    fn liked_resolution_fetches_all_pages_and_keeps_duplicate_exact_offset() {
        let command = PlayerCommand::PlayLiked {
            start: Some(A.into()),
            source_position: Some(5106),
            position_ms: 42,
            shuffle: true,
        };
        let mut calls = 0;
        let resolved = resolve(intents::plan(&command).unwrap().unwrap(), |path| {
            calls += 1;
            Ok(if path.contains("offset=") {
                json!({"items":[{"track":{"uri":A}}],"total":5107,"next":null})
            } else {
                json!({"items":vec![json!({"track":{"uri":A}});5106],"total":5107,"next":"/v1/me/tracks?offset=5106"})
            })
        }).unwrap();
        assert_eq!(calls, 2);
        let LoadPlan::WebApi {
            body,
            qualification,
            ..
        } = resolved
        else {
            panic!("expected API")
        };
        assert_eq!(body["uris"].as_array().unwrap().len(), 5107);
        assert_eq!(body["offset"]["position"], 5106);
        assert_eq!(body["position_ms"], 42);
        assert_eq!(
            qualification,
            Qualification::ExplicitArrayOffsetRequiresLive
        );
    }

    #[test]
    fn fresh_playlist_checks_surround_full_unfiltered_pages_and_reject_changes() {
        let command = PlayerCommand::PlayContextAt {
            uri: "spotify:playlist:0123456789abcdefghijkl".into(),
            track: Some(A.into()),
            row_position: 2,
            position_ms: 7,
            shuffle: false,
        };
        for changed in [false, true] {
            let mut paths = Vec::new();
            let result = resolve(intents::plan(&command).unwrap().unwrap(), |path| {
                paths.push(path.to_owned());
                Ok(if path.contains("fields=") {
                    json!({"snapshot_id":if changed && paths.len() > 1 { "new" } else { "old" }})
                } else if path.contains("offset=") {
                    json!({"items":[{"item":{"uri":A}}],"total":3,"next":null})
                } else {
                    json!({"items":[null,{"item":{"uri":A}}],"total":3,"next":"/v1/playlists/0123456789abcdefghijkl/items?offset=2"})
                })
            });
            assert_eq!(paths.len(), 4);
            assert_eq!(paths.first(), paths.last());
            if changed {
                assert!(matches!(result, Err(ApiError::CollectionChanged)));
            } else {
                let LoadPlan::WebApi { body, .. } = result.unwrap() else {
                    panic!("expected API")
                };
                assert_eq!(body["offset"]["position"], 2);
            }
        }
    }

    #[test]
    fn source_errors_do_not_fall_back_to_an_sdk_route() {
        let command = PlayerCommand::PlayLiked {
            start: None,
            source_position: None,
            position_ms: 0,
            shuffle: false,
        };
        assert!(matches!(
            resolve(intents::plan(&command).unwrap().unwrap(), |_| Err(
                ApiError::Offline
            )),
            Err(ApiError::Offline)
        ));
    }

    fn liked() -> PlayerCommand {
        PlayerCommand::PlayLiked {
            start: None,
            source_position: None,
            position_ms: 0,
            shuffle: false,
        }
    }

    #[test]
    fn mailbox_coalesces_only_superseded_loads_and_keeps_control_order() {
        let shared = super::super::tests::fixture();
        let mailbox = Mailbox::default();
        mailbox.enqueue(&shared, (), liked()).unwrap();
        mailbox.enqueue(&shared, (), liked()).unwrap();
        mailbox.enqueue(&shared, (), PlayerCommand::Pause).unwrap();
        mailbox.enqueue(&shared, (), liked()).unwrap();
        let queue = mailbox.queue.lock().unwrap();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.iter().map(|i| i.id).collect::<Vec<_>>(), [2, 3, 4]);
        assert!(!cancelled(&shared, &mailbox, &queue[0]));
        assert!(matches!(queue[1].command, PlayerCommand::Pause));
    }

    #[test]
    fn account_replacement_shutdown_and_newer_load_cancel_before_a_write() {
        let shared = super::super::tests::fixture();
        let mailbox = Mailbox::default();
        mailbox.enqueue(&shared, (), liked()).unwrap();
        let running = mailbox.queue.lock().unwrap().pop_front().unwrap();
        assert!(check(&shared, &mailbox, &running).is_ok());
        mailbox.enqueue(&shared, (), liked()).unwrap();
        assert!(check(&shared, &mailbox, &running).is_err());
        let current = mailbox.queue.lock().unwrap().pop_front().unwrap();
        shared.generation.fetch_add(1, Ordering::AcqRel);
        assert!(check(&shared, &mailbox, &current).is_err());
        shared
            .generation
            .store(current.generation, Ordering::Release);
        shared.shutdown.store(true, Ordering::Release);
        assert!(check(&shared, &mailbox, &current).is_err());
    }

    #[test]
    fn serial_coordinator_never_replays_uncertain_write_or_turns_acceptance_into_playing() {
        let shared = super::super::tests::fixture();
        shared.update(|s| {
            s.playback = PlaybackStatus::Paused;
            s.position.speed = 0.0;
        });
        let mailbox = Arc::new(Mailbox::default());
        mailbox.enqueue(&shared, (), PlayerCommand::Pause).unwrap();
        mailbox
            .enqueue(&shared, (), PlayerCommand::Volume { percent: 31 })
            .unwrap();
        mailbox.enqueue(&shared, (), PlayerCommand::Stop).unwrap();
        mailbox
            .enqueue(&shared, (), PlayerCommand::Volume { percent: 32 })
            .unwrap();
        let (sent, received) = flume::unbounded();
        let state = shared.clone();
        let worker = std::thread::spawn(move || {
            run_worker(state, mailbox, |state, _, intent| {
                sent.send(intent.id).unwrap();
                match intent.id {
                    1 => Err(api_error(ApiError::AmbiguousWrite)),
                    3 => Ok(true), // Explicit Stop reconciliation seam.
                    4 => {
                        state.shutdown.store(true, Ordering::Release);
                        Ok(false)
                    }
                    _ => panic!("uncertain writes must fence later controls"),
                }
            })
        });
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 4);
        worker.join().unwrap();
        assert_eq!(shared.snapshot().playback, PlaybackStatus::Paused);
        assert_eq!(shared.snapshot().position.speed, 0.0);
        assert!(!shared.snapshot().control_pending);
    }

    #[test]
    fn observation_timeout_does_not_fence_a_new_explicit_selection() {
        let shared = super::super::tests::fixture();
        let mailbox = Arc::new(Mailbox::default());
        mailbox.enqueue(&shared, (), PlayerCommand::Pause).unwrap();
        mailbox.enqueue(&shared, (), liked()).unwrap();
        let (sent, received) = flume::unbounded();
        let state = shared.clone();
        let input = mailbox.clone();
        let worker = std::thread::spawn(move || {
            run_worker(state, input, |state, _, intent| {
                sent.send(intent.id).unwrap();
                if intent.id == 2 {
                    state.shutdown.store(true, Ordering::Release);
                }
                Ok(false)
            });
        });
        assert_eq!(received.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
        let second = received.recv_timeout(Duration::from_millis(500));
        shared.shutdown.store(true, Ordering::Release);
        mailbox.changed.notify_all();
        worker.join().unwrap();
        assert_eq!(
            second.unwrap(),
            2,
            "a missing observation is not ambiguous delivery"
        );
        assert_eq!(shared.snapshot().control_error, Some(ErrorCode::Timeout));
    }

    #[test]
    fn newer_selection_supersedes_an_old_observation_wait_without_replaying_it() {
        let shared = super::super::tests::fixture();
        let mailbox = Arc::new(Mailbox::default());
        mailbox.enqueue(&shared, (), liked()).unwrap();
        let (sent, received) = flume::unbounded();
        let state = shared.clone();
        let input = mailbox.clone();
        let worker = std::thread::spawn(move || {
            run_worker(state, input, |state, input, intent| {
                sent.send(intent.id).unwrap();
                if intent.id == 1 {
                    wait(state, input, intent, 0, |_| false)
                } else {
                    state.shutdown.store(true, Ordering::Release);
                    Ok(true)
                }
            });
        });
        assert_eq!(received.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
        mailbox.enqueue(&shared, (), liked()).unwrap();
        let second = received.recv_timeout(Duration::from_millis(500));
        shared.shutdown.store(true, Ordering::Release);
        mailbox.changed.notify_all();
        shared.changed.notify_all();
        worker.join().unwrap();
        assert_eq!(
            second.unwrap(),
            2,
            "new selection must not wait for the five-second observation deadline"
        );
        assert_eq!(shared.snapshot().control_error, None);
    }

    #[test]
    fn source_resolution_wait_does_not_lock_observations_or_status_and_is_supersedable() {
        let shared = super::super::tests::fixture();
        let mailbox = Arc::new(Mailbox::default());
        mailbox.enqueue(&shared, (), liked()).unwrap();
        let (entered, started) = flume::bounded(1);
        let (release, released) = flume::bounded(1);
        let (done, finished) = flume::bounded(2);
        let state = shared.clone();
        let input = mailbox.clone();
        let worker = std::thread::spawn(move || {
            run_worker(state, input, |state, input, intent| {
                if intent.id == 1 {
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    let cancelled = check(state, input, intent).is_err();
                    done.send(cancelled).unwrap();
                    Err(IpcError::new(ErrorCode::Busy, "Superseded"))
                } else {
                    done.send(true).unwrap();
                    state.shutdown.store(true, Ordering::Release);
                    Ok(false)
                }
            })
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        shared.update(|s| s.volume = 47);
        assert_eq!(shared.snapshot().volume, 47);
        mailbox.enqueue(&shared, (), liked()).unwrap();
        release.send(()).unwrap();
        assert!(finished.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(finished.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn selected_context_receipt_requires_the_resolved_row_not_just_its_context() {
        use crate::domain::{ItemKind, TrackMetadata};
        let context = "spotify:playlist:0123456789abcdefghijkl";
        let selected = "spotify:track:1123456789abcdefghijkl";
        let command = PlayerCommand::PlayContextAt {
            uri: context.into(),
            track: None,
            row_position: 1,
            position_ms: 0,
            shuffle: false,
        };
        let rows = vec![
            json!({"track":{"uri":A}}),
            json!({"track":{"uri":selected}}),
        ];
        let LoadPlan::WebApi {
            body, expected_uri, ..
        } = intents::resolve_context(&command, &rows).unwrap()
        else {
            panic!("expected resolved context");
        };
        assert_eq!(expected_uri.as_deref(), Some(selected));
        assert!(
            body.get("expected_uri").is_none(),
            "internal receipt metadata is not a provider field"
        );
        let before = Snapshot::starting("/config".into(), "myx".into());
        let mut after = before.clone();
        after.locally_active = true;
        after.playback = PlaybackStatus::Playing;
        after.context_uri = Some(context.into());
        after.track = Some(TrackMetadata::pending(ItemKind::Track, A.into()));
        assert!(!load_observed(
            &after,
            &before,
            &body,
            expected_uri.as_deref()
        ));
        after.track = Some(TrackMetadata::pending(ItemKind::Track, selected.into()));
        assert!(load_observed(
            &after,
            &before,
            &body,
            expected_uri.as_deref()
        ));
    }

    #[test]
    fn periodic_progress_does_not_confirm_a_same_item_load_at_another_position() {
        use crate::domain::{ItemKind, PositionAnchor, TrackMetadata};
        let mut before = Snapshot::starting("/config".into(), "myx".into());
        before.locally_active = true;
        before.playback = PlaybackStatus::Playing;
        before.track = Some(TrackMetadata::pending(ItemKind::Track, A.into()));
        before.observed_position = Some(PositionAnchor {
            position_ms: 40_000,
            timestamp_ms: 1000,
            speed: 1.0,
        });
        let mut after = before.clone();
        after.observed_position = Some(PositionAnchor {
            position_ms: 41_000,
            timestamp_ms: 2000,
            speed: 1.0,
        });
        assert!(!load_observed(
            &after,
            &before,
            &json!({"uris":[A],"position_ms":0}),
            None
        ));
    }

    #[test]
    fn old_playing_or_metadata_and_http_ack_are_not_a_new_load_observation() {
        use crate::domain::{ItemKind, TrackMetadata};
        let mut before = Snapshot::starting("/config".into(), "myx".into());
        before.locally_active = true;
        before.playback = PlaybackStatus::Playing;
        before.track = Some(TrackMetadata::pending(ItemKind::Track, A.into()));
        let mut after = before.clone();
        let body = json!({"uris":[A],"position_ms":42});
        assert!(!load_observed(&after, &before, &body, None));
        after.track.as_mut().unwrap().title = Some("Enriched title".into());
        assert!(!load_observed(&after, &before, &body, None));
        after.observed_position = Some(crate::domain::PositionAnchor {
            position_ms: 42,
            timestamp_ms: 12345,
            speed: 1.25,
        });
        assert!(load_observed(&after, &before, &body, None));
        assert_eq!(after.observed_position.unwrap().speed, 1.25);
        assert_eq!(after.observed_position.unwrap().timestamp_ms, 12345);
    }
}
