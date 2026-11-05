//! Observation-only playback reduction. Intent acknowledgement cannot start the clock.
use crate::domain::{
    metadata::ItemKind, playback::now_ms, MetadataStatus, PlaybackStatus, PositionAnchor,
    ServiceState, Snapshot, TrackMetadata,
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
            | PlayerEvent::Buffering { .. }
            | PlayerEvent::LocallyInactive
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
        if let Some(anchor) = snapshot.observed_position {
            snapshot.position = anchor;
            snapshot.playback = PlaybackStatus::Playing;
            return true;
        }
        observe(snapshot, PlayerEvent::Playing { uri, position_ms }, true);
        true
    }
}

fn freeze(snapshot: &mut Snapshot) {
    let timestamp = now_ms();
    snapshot.position.position_ms = snapshot.position.at(
        timestamp,
        snapshot.track.as_ref().and_then(|track| track.duration_ms),
    );
    snapshot.position.timestamp_ms = timestamp;
    snapshot.position.speed = 0.0;
}

pub fn readiness(snapshot: &mut Snapshot) {
    if (!snapshot.owned_device_verified || !snapshot.soloist_ready)
        && snapshot.playback == PlaybackStatus::Playing
    {
        freeze(snapshot);
        snapshot.playback = PlaybackStatus::Disconnected;
    }
    snapshot.readiness = if snapshot.web_api_ready
        && snapshot.catalog_available
        && snapshot.soloist_ready
        && snapshot.native_media_ready
        && snapshot.owned_device_verified
    {
        ServiceState::Ready
    } else if snapshot.owned_device_verified && snapshot.soloist_ready {
        ServiceState::Degraded
    } else {
        ServiceState::SetupRequired
    };
}

fn begin_item(snapshot: &mut Snapshot, uri: String) {
    let kind = if uri.starts_with("spotify:episode:") {
        ItemKind::Episode
    } else {
        ItemKind::Track
    };
    snapshot.track = Some(TrackMetadata::pending(kind, uri));
    snapshot.metadata_status = MetadataStatus::Loading;
    snapshot.position = PositionAnchor::default();
    snapshot.observed_position = None;
    clear_actions(snapshot);
    snapshot.playback = PlaybackStatus::Buffering;
}

pub fn observe(snapshot: &mut Snapshot, event: PlayerEvent, processed_pcm: bool) {
    match event {
        PlayerEvent::TrackChanged { uri } => {
            if !snapshot.locally_active {
                return;
            }
            begin_item(snapshot, uri);
        }
        PlayerEvent::Playing { uri, position_ms } => {
            if !snapshot.locally_active || snapshot.track.as_ref().is_some_and(|track| track.uri != uri) {
                return;
            }
            // Stop clears the read model, while the SDK may keep the same item
            // and omit a redundant TrackChanged when it resumes.
            if snapshot.track.is_none() {
                begin_item(snapshot, uri.clone());
            }
            if snapshot.owned_device_verified && processed_pcm {
                snapshot.playback = PlaybackStatus::Playing;
                snapshot.position = snapshot.observed_position.unwrap_or(PositionAnchor {
                    position_ms: u64::from(position_ms),
                    timestamp_ms: now_ms(),
                    speed: 1.0,
                });
            } else {
                freeze(snapshot);
                snapshot.playback = PlaybackStatus::Buffering;
            }
        }
        PlayerEvent::Paused { uri, position_ms } => {
            if snapshot.locally_active && snapshot
                .track
                .as_ref()
                .is_some_and(|track| track.uri == uri)
            {
                freeze(snapshot);
                if snapshot.observed_position.is_none() {
                    snapshot.position.position_ms = u64::from(position_ms);
                }
                snapshot.playback = PlaybackStatus::Paused;
            }
        }
        PlayerEvent::PositionCorrection { uri, position_ms } => {
            // Legacy observations have no server anchor. Once one is known,
            // never replace its timestamp/speed with a local receipt time.
            if snapshot.locally_active && snapshot.observed_position.is_none() && snapshot
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
            snapshot.observed_position = None;
            snapshot.context_uri = None;
            snapshot.available_actions = None;
            snapshot.seek_forward_step_ms = None;
            snapshot.seek_backward_step_ms = None;
            snapshot.track = None;
            snapshot.metadata_status = MetadataStatus::Unavailable;
        }
        PlayerEvent::Reconnecting => {
            freeze(snapshot);
            snapshot.playback = PlaybackStatus::Disconnected;
            snapshot.locally_active = false;
            snapshot.soloist_ready = false;
            snapshot.owned_device_verified = false;
            clear_actions(snapshot);
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
                clear_actions(snapshot);
                if snapshot.playback == PlaybackStatus::Playing {
                    snapshot.playback = PlaybackStatus::Disconnected;
                }
            }
            if !logged_in {
                snapshot.owned_device_verified = false;
            }
            readiness(snapshot);
        }
        PlayerEvent::NativeQueue(mut queue) => {
            if snapshot.locally_active {
                queue.complete_catalog_context = false;
                snapshot.queue = queue;
            }
        }
        PlayerEvent::Options { shuffle, repeat } => {
            if snapshot.locally_active {
                snapshot.shuffle = shuffle;
                snapshot.repeat = repeat;
            }
        }
        PlayerEvent::Volume { percent } => {
            if snapshot.locally_active && percent <= 100 {
                snapshot.volume = percent;
            }
        }
        PlayerEvent::Context { uri } => {
            if snapshot.locally_active {
                snapshot.context_uri = uri;
            }
        }
        PlayerEvent::PositionAnchor { uri, anchor } => {
            if snapshot.locally_active && anchor.speed.is_finite() && anchor.speed >= 0.0
                && snapshot.track.as_ref().is_some_and(|track| track.uri == uri)
            {
                snapshot.observed_position = Some(anchor);
                snapshot.position = anchor;
                if snapshot.playback != PlaybackStatus::Playing || !processed_pcm
                    || !snapshot.owned_device_verified
                {
                    snapshot.position.speed = 0.0;
                }
            }
        }
        PlayerEvent::Buffering { uri, position_ms } => {
            if snapshot.locally_active && snapshot.track.as_ref().is_some_and(|track| track.uri == uri) {
                freeze(snapshot);
                if snapshot.observed_position.is_none() {
                    snapshot.position.position_ms = u64::from(position_ms);
                }
                snapshot.playback = PlaybackStatus::Buffering;
            }
        }
        PlayerEvent::LocallyInactive => {
            freeze(snapshot);
            snapshot.locally_active = false;
            clear_actions(snapshot);
            if snapshot.track.is_some() {
                snapshot.playback = PlaybackStatus::Disconnected;
            }
        }
        PlayerEvent::AvailableActions { actions, seek_forward_step_ms, seek_backward_step_ms } => {
            if snapshot.locally_active {
                snapshot.available_actions = Some(actions);
                snapshot.seek_forward_step_ms = seek_forward_step_ms;
                snapshot.seek_backward_step_ms = seek_backward_step_ms;
            }
        }
        PlayerEvent::BackendStatus { message } => snapshot.message = Some(message),
        PlayerEvent::ProcessGroups(groups) => snapshot.owned_process_groups=groups.into_iter().map(|(pid,pgid)|super::super::domain::playback::OwnedProcessGroup {pid,pgid}).collect(),
        // Catalog enrichment, not SDK decorations, owns canonical titles/art.
        PlayerEvent::Metadata(_)
        // Logical request/source lists must never masquerade as native IDs.
        | PlayerEvent::QueueChanged { .. }
        | PlayerEvent::EndOfTrack { .. }
        | PlayerEvent::Reconnected
        | PlayerEvent::ReadModel(_) => {}
    }
}

fn clear_actions(snapshot: &mut Snapshot) {
    snapshot.available_actions = None;
    snapshot.seek_forward_step_ms = None;
    snapshot.seek_backward_step_ms = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{QueueEntry, QueueSnapshot, QueueSource, RepeatMode};

    const LOCAL: &str = "spotify:track:0123456789abcdefghijkl";
    const OTHER: &str = "spotify:track:abcdefghijkl0123456789";

    fn local() -> Snapshot {
        let mut snapshot = Snapshot::starting("config".into(), "Myx".into());
        snapshot.locally_active = true;
        snapshot.soloist_ready = true;
        snapshot.owned_device_verified = true;
        snapshot.catalog_available = true;
        observe(
            &mut snapshot,
            PlayerEvent::TrackChanged { uri: LOCAL.into() },
            false,
        );
        snapshot
    }

    fn queue(uri: &str) -> QueueSnapshot {
        QueueSnapshot {
            previous: vec![],
            upcoming: vec![QueueEntry {
                uid: "sdk-uid".into(),
                source: QueueSource::Context,
                uri: uri.into(),
                title: Some("Observed".into()),
            }],
            complete_catalog_context: false,
        }
    }

    #[test]
    fn pcm_confirmation_preserves_actual_sdk_timestamp_and_nonunit_speed() {
        let mut snapshot = local();
        let mut pending = PendingPlayback::default();
        let playing = PlayerEvent::Playing {
            uri: LOCAL.into(),
            position_ms: 1200,
        };
        pending.note(&playing);
        observe(&mut snapshot, playing, false);
        let anchor = PositionAnchor {
            position_ms: 1200,
            timestamp_ms: 1700000000123,
            speed: 1.5,
        };
        observe(
            &mut snapshot,
            PlayerEvent::PositionAnchor {
                uri: LOCAL.into(),
                anchor,
            },
            false,
        );
        assert_eq!(snapshot.observed_position, Some(anchor));
        assert_eq!(snapshot.position.speed, 0.0);
        assert!(!pending.confirm(&mut snapshot, false));
        assert!(pending.confirm(&mut snapshot, true));
        assert_eq!(snapshot.position, anchor);
        assert_eq!(snapshot.playback, PlaybackStatus::Playing);
        observe(
            &mut snapshot,
            PlayerEvent::PositionCorrection {
                uri: LOCAL.into(),
                position_ms: 9999,
            },
            true,
        );
        assert_eq!(
            snapshot.position, anchor,
            "legacy correction cannot erase the SDK anchor"
        );
        assert_eq!(snapshot.position.at(anchor.timestamp_ms + 200, None), 1500);
    }

    #[test]
    fn buffering_is_distinct_from_paused_and_cancels_pending_pcm_confirmation() {
        let mut snapshot = local();
        let mut pending = PendingPlayback::default();
        pending.note(&PlayerEvent::Playing {
            uri: LOCAL.into(),
            position_ms: 42,
        });
        let event = PlayerEvent::Buffering {
            uri: LOCAL.into(),
            position_ms: 42,
        };
        pending.note(&event);
        observe(&mut snapshot, event, true);
        assert_eq!(snapshot.playback, PlaybackStatus::Buffering);
        assert_eq!(snapshot.position.speed, 0.0);
        assert!(!pending.confirm(&mut snapshot, true));
        observe(
            &mut snapshot,
            PlayerEvent::Paused {
                uri: LOCAL.into(),
                position_ms: 42,
            },
            true,
        );
        assert_eq!(snapshot.playback, PlaybackStatus::Paused);
    }

    #[test]
    fn takeover_retains_local_catalog_position_and_native_queue_without_global_leaks() {
        let mut snapshot = local();
        snapshot.track.as_mut().unwrap().title = Some("Local catalog title".into());
        snapshot.metadata_status = MetadataStatus::Ready;
        snapshot.output_error = Some("publisher error".into());
        snapshot.auth_revision = 7;
        snapshot.position = PositionAnchor {
            position_ms: 42,
            timestamp_ms: now_ms(),
            speed: 0.0,
        };
        observe(&mut snapshot, PlayerEvent::NativeQueue(queue(LOCAL)), false);
        let track = snapshot.track.clone();
        let retained_queue = snapshot.queue.clone();
        observe(&mut snapshot, PlayerEvent::LocallyInactive, false);
        let frozen = snapshot.position;
        for event in [
            PlayerEvent::TrackChanged { uri: OTHER.into() },
            PlayerEvent::Playing {
                uri: OTHER.into(),
                position_ms: 9999,
            },
            PlayerEvent::NativeQueue(queue(OTHER)),
            PlayerEvent::Context {
                uri: Some(OTHER.into()),
            },
            PlayerEvent::Volume { percent: 99 },
            PlayerEvent::PositionAnchor {
                uri: OTHER.into(),
                anchor: PositionAnchor {
                    position_ms: 9999,
                    timestamp_ms: 123,
                    speed: 2.0,
                },
            },
        ] {
            observe(&mut snapshot, event, true);
        }
        assert_eq!(snapshot.track, track);
        assert_eq!(snapshot.queue, retained_queue);
        assert_eq!(snapshot.position, frozen);
        assert_eq!(snapshot.playback, PlaybackStatus::Disconnected);
        assert_eq!(snapshot.metadata_status, MetadataStatus::Ready);
        assert_eq!(snapshot.volume, 25);
        assert_eq!(snapshot.output_error.as_deref(), Some("publisher error"));
        assert_eq!(snapshot.auth_revision, 7);
        assert!(snapshot.catalog_available);
        observe(&mut snapshot, PlayerEvent::Stopped, false);
        assert_eq!(snapshot.playback, PlaybackStatus::Stopped);
        assert!(snapshot.track.is_none());
    }

    #[test]
    fn options_actions_and_native_queue_are_observations_not_logical_request_ids() {
        let mut snapshot = local();
        observe(&mut snapshot, PlayerEvent::NativeQueue(queue(LOCAL)), false);
        let native = snapshot.queue.clone();
        observe(
            &mut snapshot,
            PlayerEvent::QueueChanged {
                entries: vec![("Requested".into(), OTHER.into())],
            },
            false,
        );
        assert_eq!(snapshot.queue, native);
        observe(
            &mut snapshot,
            PlayerEvent::Options {
                shuffle: true,
                repeat: RepeatMode::One,
            },
            false,
        );
        observe(&mut snapshot, PlayerEvent::Volume { percent: 65 }, false);
        observe(
            &mut snapshot,
            PlayerEvent::Context {
                uri: Some("spotify:playlist:0123456789abcdefghijkl".into()),
            },
            false,
        );
        observe(
            &mut snapshot,
            PlayerEvent::AvailableActions {
                actions: vec!["seek_forward".into()],
                seek_forward_step_ms: Some(15000),
                seek_backward_step_ms: None,
            },
            false,
        );
        assert!(snapshot.shuffle);
        assert_eq!(snapshot.repeat, RepeatMode::One);
        assert_eq!(snapshot.volume, 65);
        assert!(snapshot.context_uri.is_some());
        assert_eq!(
            snapshot.available_actions,
            Some(vec!["seek_forward".into()])
        );
        assert_eq!(snapshot.seek_forward_step_ms, Some(15000));
        assert!(
            snapshot.message.is_none(),
            "transient actions are not backend support banners"
        );
        observe(
            &mut snapshot,
            PlayerEvent::AvailableActions {
                actions: vec![],
                seek_forward_step_ms: None,
                seek_backward_step_ms: None,
            },
            false,
        );
        assert_eq!(snapshot.available_actions, Some(vec![]));
        assert!(snapshot.seek_forward_step_ms.is_none());
    }
}
