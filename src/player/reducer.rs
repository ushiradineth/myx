//! Observation-only playback reduction. Intent acknowledgement cannot start the clock.
use crate::domain::{
    metadata::ItemKind, playback::now_ms, MetadataStatus, PlaybackStatus, PositionAnchor,
    QueueEntry, QueueSource, ServiceState, Snapshot, TrackMetadata,
};
use crate::soloist::protocol::PlayerEvent;

#[derive(Default)]
pub struct PendingPlayback(Option<(String, u32, std::time::Instant)>);
impl PendingPlayback {
    pub fn is_pending(&self) -> bool {
        self.0.is_some()
    }
    pub fn note(&mut self, event: &PlayerEvent) {
        match event {
            PlayerEvent::Playing { uri, position_ms } => {
                self.0 = Some((uri.clone(), *position_ms, std::time::Instant::now()))
            }
            PlayerEvent::TrackChanged { .. }
            | PlayerEvent::Paused { .. }
            | PlayerEvent::Stopped
            | PlayerEvent::Reconnecting
            | PlayerEvent::Authentication {
                is_active: false, ..
            } => self.0 = None,
            _ => {}
        }
    }
    pub fn confirm(&mut self, snapshot: &mut Snapshot, processed_pcm: bool) -> bool {
        if !processed_pcm || !snapshot.owned_device_verified || !snapshot.locally_active {
            return false;
        }
        let Some((uri, position, at)) = self.0.as_ref() else {
            return false;
        };
        if snapshot
            .track
            .as_ref()
            .is_none_or(|track| track.uri != *uri)
        {
            return false;
        }
        let position_ms =
            position.saturating_add(at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32);
        let uri = uri.clone();
        self.0 = None;
        observe(snapshot, PlayerEvent::Playing { uri, position_ms }, true);
        true
    }
}

fn freeze(snapshot: &mut Snapshot) {
    snapshot.position.position_ms = snapshot.position.at(
        now_ms(),
        snapshot.track.as_ref().and_then(|track| track.duration_ms),
    );
    snapshot.position.timestamp_ms = now_ms();
    snapshot.position.speed = 0.0;
}

pub fn readiness(snapshot: &mut Snapshot) {
    snapshot.readiness = if snapshot.web_api_ready
        && snapshot.soloist_ready
        && snapshot.native_media_ready
        && snapshot.owned_device_verified
    {
        ServiceState::Ready
    } else if snapshot.locally_active && snapshot.soloist_ready {
        ServiceState::Degraded
    } else {
        ServiceState::SetupRequired
    };
}

pub fn observe(snapshot: &mut Snapshot, event: PlayerEvent, processed_pcm: bool) {
    match event {
        PlayerEvent::TrackChanged { uri } => {
            let kind = if uri.starts_with("spotify:episode:") {
                ItemKind::Episode
            } else {
                ItemKind::Track
            };
            snapshot.track = Some(TrackMetadata::pending(kind, uri));
            snapshot.metadata_status = MetadataStatus::Loading;
            snapshot.position = PositionAnchor::default();
            snapshot.playback = PlaybackStatus::Buffering;
        }
        PlayerEvent::Playing { uri, position_ms } => {
            if snapshot.locally_active
                && snapshot.owned_device_verified
                && processed_pcm
                && snapshot
                    .track
                    .as_ref()
                    .is_some_and(|track| track.uri == uri)
            {
                snapshot.playback = PlaybackStatus::Playing;
                snapshot.position = PositionAnchor {
                    position_ms: u64::from(position_ms),
                    timestamp_ms: now_ms(),
                    speed: 1.0,
                };
            } else {
                freeze(snapshot);
                snapshot.playback = PlaybackStatus::Buffering;
            }
        }
        PlayerEvent::Paused { uri, position_ms } => {
            if snapshot
                .track
                .as_ref()
                .is_some_and(|track| track.uri == uri)
            {
                snapshot.position = PositionAnchor {
                    position_ms: u64::from(position_ms),
                    timestamp_ms: now_ms(),
                    speed: 0.0,
                };
                snapshot.playback = PlaybackStatus::Paused;
            }
        }
        PlayerEvent::PositionCorrection { uri, position_ms } => {
            if snapshot
                .track
                .as_ref()
                .is_some_and(|track| track.uri == uri)
            {
                snapshot.position.position_ms = u64::from(position_ms);
                snapshot.position.timestamp_ms = now_ms();
            }
        }
        PlayerEvent::Stopped => {
            snapshot.playback = PlaybackStatus::Stopped;
            snapshot.position = PositionAnchor::default();
            snapshot.track = None;
            snapshot.metadata_status = MetadataStatus::Unavailable;
        }
        PlayerEvent::Reconnecting => {
            freeze(snapshot);
            snapshot.playback = PlaybackStatus::Disconnected;
            snapshot.locally_active = false;
            snapshot.soloist_ready = false;
            snapshot.owned_device_verified = false;
            readiness(snapshot);
        }
        PlayerEvent::Authentication {
            logged_in,
            is_active,
        } => {
            snapshot.soloist_ready = logged_in;
            snapshot.locally_active = logged_in && is_active;
            if !snapshot.locally_active {
                freeze(snapshot);
                if snapshot.playback == PlaybackStatus::Playing {
                    snapshot.playback = PlaybackStatus::Disconnected;
                }
            }
            if !logged_in {
                snapshot.owned_device_verified = false;
            }
            readiness(snapshot);
        }
        PlayerEvent::QueueChanged { entries } => {
            // Legacy transport availability is not proof of native entry IDs or a full context.
            snapshot.queue.upcoming = entries
                .into_iter()
                .enumerate()
                .map(|(index, (title, uri))| QueueEntry {
                    uid: format!("pending-{index}"),
                    source: QueueSource::Unknown,
                    title: (!title.is_empty() && title != uri).then_some(title),
                    uri,
                })
                .collect();
            snapshot.queue.complete_catalog_context = false;
        }
        PlayerEvent::BackendStatus { message } => snapshot.message = Some(message),
        // Catalog enrichment, not SDK decorations, owns canonical titles/art.
        PlayerEvent::Metadata(_)
        | PlayerEvent::EndOfTrack { .. }
        | PlayerEvent::Reconnected
        | PlayerEvent::ReadModel(_) => {}
    }
}
