use myx::domain::{PlaybackStatus, Snapshot};
use myx::player::reducer;
use myx::soloist::protocol::PlayerEvent;

#[test]
fn observed_owned_play_requires_pcm_and_pause_freezes_position() {
    let uri = "spotify:track:0123456789abcdefghijkl".to_owned();
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    snapshot.web_api_ready = true;
    snapshot.owned_device_verified = true;
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Authentication {
            logged_in: true,
            is_active: true,
        },
        false,
    );
    reducer::observe(
        &mut snapshot,
        PlayerEvent::TrackChanged { uri: uri.clone() },
        false,
    );
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Playing {
            uri: uri.clone(),
            position_ms: 10,
        },
        false,
    );
    assert_ne!(snapshot.playback, PlaybackStatus::Playing);
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Playing {
            uri: uri.clone(),
            position_ms: 25,
        },
        true,
    );
    assert_eq!(snapshot.playback, PlaybackStatus::Playing);
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Paused {
            uri,
            position_ms: 30,
        },
        true,
    );
    assert_eq!(snapshot.position.speed, 0.0);
    assert_eq!(snapshot.position.at(u64::MAX, None), 30);
}

#[test]
fn disconnect_retains_track_but_never_advances_or_authorizes_remote_controls() {
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    reducer::observe(
        &mut snapshot,
        PlayerEvent::TrackChanged {
            uri: "spotify:episode:0123456789abcdefghijkl".into(),
        },
        false,
    );
    reducer::observe(&mut snapshot, PlayerEvent::Reconnecting, true);
    assert!(snapshot.track.is_some());
    assert_eq!(snapshot.position.speed, 0.0);
    assert!(!snapshot.owned_device_verified);
    assert_eq!(snapshot.playback, PlaybackStatus::Disconnected);
}

#[test]
fn device_takeover_cannot_leave_a_playing_export() {
    let uri = "spotify:track:0123456789abcdefghijkl".to_owned();
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    snapshot.web_api_ready = true;
    snapshot.owned_device_verified = true;
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Authentication {
            logged_in: true,
            is_active: true,
        },
        true,
    );
    reducer::observe(
        &mut snapshot,
        PlayerEvent::TrackChanged { uri: uri.clone() },
        true,
    );
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Playing {
            uri,
            position_ms: 10,
        },
        true,
    );
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Authentication {
            logged_in: true,
            is_active: false,
        },
        true,
    );
    assert_eq!(snapshot.playback, PlaybackStatus::Disconnected);
    assert_eq!(snapshot.position.speed, 0.0);
}

#[test]
fn explicit_cold_resume_uses_observed_item_position_and_modes_not_stale_client_intent() {
    use myx::domain::{ItemKind, PlayerCommand, TrackMetadata};
    let uri = "spotify:track:0123456789abcdefghijkl".to_owned();
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    snapshot.track = Some(TrackMetadata::pending(ItemKind::Track, uri.clone()));
    snapshot.position.position_ms = 42_000;
    snapshot.shuffle = true;
    let previous = PlayerCommand::PlayContext {
        uri: "spotify:album:0123456789abcdefghijkl".into(),
        track: None,
        position_ms: 0,
        shuffle: false,
    };
    match myx::player::controller::restored_command(Some(previous), &snapshot) {
        PlayerCommand::PlayContext {
            track,
            position_ms,
            shuffle,
            ..
        } => {
            assert_eq!(track, Some(uri));
            assert_eq!(position_ms, 42_000);
            assert!(shuffle);
        }
        _ => panic!("restoration lost its context"),
    }
    assert_eq!(snapshot.playback, PlaybackStatus::Starting);
}

#[test]
fn verification_can_finish_after_the_sdk_play_observation_without_losing_the_transition() {
    let uri = "spotify:track:0123456789abcdefghijkl".to_owned();
    let mut snapshot = Snapshot::starting("/config".into(), "Device".into());
    let mut pending = reducer::PendingPlayback::default();
    reducer::observe(
        &mut snapshot,
        PlayerEvent::TrackChanged { uri: uri.clone() },
        false,
    );
    snapshot.locally_active = true;
    let observed = PlayerEvent::Playing {
        uri,
        position_ms: 120,
    };
    pending.note(&observed);
    reducer::observe(&mut snapshot, observed, false);
    assert!(!pending.confirm(&mut snapshot, true));
    snapshot.owned_device_verified = true;
    assert!(!pending.confirm(&mut snapshot, false));
    assert!(pending.confirm(&mut snapshot, true));
    assert_eq!(snapshot.playback, PlaybackStatus::Playing);
    assert!(!pending.is_pending());
}
