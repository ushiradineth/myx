use myx::domain::{PlaybackStatus, Snapshot};
use myx::player::reducer;
use myx::soloist::protocol::PlayerEvent;

#[test]
fn stop_then_same_item_play_restores_the_observed_item_before_pcm_confirmation() {
    for kind in ["track", "episode"] {
        let uri = format!("spotify:{kind}:0123456789abcdefghijkl");
        let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
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
                position_ms: 30_000,
            },
            true,
        );
        reducer::observe(&mut snapshot, PlayerEvent::Stopped, false);
        assert!(snapshot.track.is_none());
        assert_eq!(snapshot.playback, PlaybackStatus::Stopped);

        // A paused cache refresh must not undo the explicit stopped state.
        reducer::observe(
            &mut snapshot,
            PlayerEvent::Paused {
                uri: uri.clone(),
                position_ms: 30_000,
            },
            false,
        );
        assert!(snapshot.track.is_none());
        assert_eq!(snapshot.playback, PlaybackStatus::Stopped);

        let mut pending = reducer::PendingPlayback::default();
        let playing = PlayerEvent::Playing {
            uri: uri.clone(),
            position_ms: 30_000,
        };
        pending.note(&playing);
        reducer::observe(&mut snapshot, playing, false);
        assert_eq!(
            snapshot.track.as_ref().map(|track| track.uri.as_str()),
            Some(uri.as_str())
        );
        assert_eq!(snapshot.playback, PlaybackStatus::Buffering);
        assert!(!pending.confirm(&mut snapshot, false));
        assert!(pending.confirm(&mut snapshot, true));
        assert_eq!(snapshot.playback, PlaybackStatus::Playing);

        reducer::observe(
            &mut snapshot,
            PlayerEvent::Paused {
                uri: uri.clone(),
                position_ms: 30_500,
            },
            true,
        );
        assert_eq!(snapshot.playback, PlaybackStatus::Paused);
        assert_eq!(snapshot.position.speed, 0.0);
        assert_eq!(
            snapshot.track.as_ref().map(|track| track.uri.as_str()),
            Some(uri.as_str())
        );
    }
}

#[test]
fn missing_item_recovery_still_rejects_inactive_unowned_and_wrong_item_playback() {
    let uri = "spotify:track:0123456789abcdefghijkl".to_owned();
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    reducer::observe(&mut snapshot, PlayerEvent::Stopped, false);
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Playing {
            uri: uri.clone(),
            position_ms: 42,
        },
        true,
    );
    assert!(snapshot.track.is_none());
    assert_eq!(snapshot.playback, PlaybackStatus::Stopped);

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
        PlayerEvent::Playing {
            uri: uri.clone(),
            position_ms: 42,
        },
        true,
    );
    assert_eq!(snapshot.playback, PlaybackStatus::Buffering);
    assert_eq!(
        snapshot.metadata_status,
        myx::domain::MetadataStatus::Loading
    );
    snapshot.owned_device_verified = true;
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Playing {
            uri: "spotify:track:abcdefghijkl0123456789".into(),
            position_ms: 99,
        },
        true,
    );
    assert_eq!(snapshot.playback, PlaybackStatus::Buffering);
    assert_eq!(
        snapshot.track.as_ref().map(|track| track.uri.as_str()),
        Some(uri.as_str())
    );
}

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
        PlayerEvent::Authentication {
            logged_in: true,
            is_active: true,
        },
        false,
    );
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

#[test]
fn native_and_client_play_toggle_share_explicit_restore_policy() {
    use myx::domain::{ItemKind, PlayerCommand, TrackMetadata};
    let uri = "spotify:track:0123456789abcdefghijkl".to_owned();
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    snapshot.track = Some(TrackMetadata::pending(ItemKind::Track, uri.clone()));
    snapshot.position.position_ms = 1234;
    for command in [PlayerCommand::Play, PlayerCommand::Toggle] {
        assert!(matches!(
            myx::player::controller::prepare_command(command, &snapshot, None, true),
            PlayerCommand::PlayContext {
                position_ms: 1234,
                ..
            }
        ));
    }
    assert!(matches!(
        myx::player::controller::prepare_command(PlayerCommand::Toggle, &snapshot, None, false),
        PlayerCommand::Toggle
    ));
    assert!(matches!(
        myx::player::controller::prepare_command(PlayerCommand::Pause, &snapshot, None, true),
        PlayerCommand::Pause
    ));
}

#[test]
fn losing_account_ownership_freezes_the_previous_playing_anchor() {
    let mut snapshot = Snapshot::starting("/config".into(), "Device".into());
    snapshot.playback = PlaybackStatus::Playing;
    snapshot.soloist_ready = true;
    snapshot.position.speed = 1.0;
    snapshot.owned_device_verified = false;
    reducer::readiness(&mut snapshot);
    assert_eq!(snapshot.playback, PlaybackStatus::Disconnected);
    assert_eq!(snapshot.position.speed, 0.0);
}

#[test]
fn cold_restore_does_not_apply_a_requested_row_to_a_different_observed_item() {
    use myx::domain::{ItemKind, PlayerCommand, TrackMetadata};
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    snapshot.track = Some(TrackMetadata::pending(
        ItemKind::Track,
        "spotify:track:0123456789abcdefghijkl".into(),
    ));
    snapshot.position.position_ms = 421;
    let last = PlayerCommand::PlayContextAt {
        uri: "spotify:playlist:2123456789abcdefghijkl".into(),
        track: Some("spotify:track:1123456789abcdefghijkl".into()),
        row_position: 5106,
        position_ms: 0,
        shuffle: false,
    };
    let restored = myx::player::controller::restored_command(Some(last), &snapshot);
    assert!(matches!(restored, PlayerCommand::PlayContext {
        track: Some(track), position_ms: 421, ..
    } if track == "spotify:track:0123456789abcdefghijkl"));
    assert_eq!(
        snapshot.track.as_ref().unwrap().uri,
        "spotify:track:0123456789abcdefghijkl"
    );
    assert!(!snapshot.locally_active);
}

#[test]
fn cold_restore_follows_observed_item_after_direct_or_explicit_source_advance() {
    use myx::domain::{ItemKind, PlayerCommand, TrackMetadata};
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    snapshot.track = Some(TrackMetadata::pending(
        ItemKind::Track,
        "spotify:track:0123456789abcdefghijkl".into(),
    ));
    snapshot.position.position_ms = 421;
    for last in [
        PlayerCommand::PlayContext {
            uri: "spotify:episode:1123456789abcdefghijkl".into(),
            track: None,
            position_ms: 999,
            shuffle: false,
        },
        PlayerCommand::PlayTracks {
            uris: vec!["spotify:track:1123456789abcdefghijkl".into()],
            start: Some("spotify:track:1123456789abcdefghijkl".into()),
            position_ms: 999,
            shuffle: false,
        },
        PlayerCommand::PlayTracksAt {
            uris: vec!["spotify:track:1123456789abcdefghijkl".into()],
            start_index: 0,
            position_ms: 999,
            shuffle: false,
        },
    ] {
        let restored = myx::player::controller::restored_command(Some(last), &snapshot);
        assert!(matches!(myx::player::intents::plan(&restored).unwrap(),
            Some(myx::player::intents::LoadPlan::SdkPlayable { uri, position_ms: 421, .. })
                if uri == "spotify:track:0123456789abcdefghijkl"));
    }
}

#[test]
fn cold_restore_discards_stale_liked_coordinates_and_rejects_ambiguous_advanced_uri() {
    use myx::domain::{ItemKind, PlayerCommand, TrackMetadata};
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    snapshot.track = Some(TrackMetadata::pending(
        ItemKind::Track,
        "spotify:track:0123456789abcdefghijkl".into(),
    ));
    snapshot.position.position_ms = 421;
    let liked = PlayerCommand::PlayLiked {
        start: Some("spotify:track:1123456789abcdefghijkl".into()),
        source_position: Some(5106),
        position_ms: 999,
        shuffle: false,
    };
    assert!(
        matches!(myx::player::controller::restored_command(Some(liked), &snapshot),
        PlayerCommand::PlayLiked { start: Some(uri), source_position: None, position_ms: 421, .. }
            if uri == "spotify:track:0123456789abcdefghijkl")
    );
    let explicit = PlayerCommand::PlayTracksAt {
        uris: vec![
            "spotify:track:1123456789abcdefghijkl".into(),
            "spotify:track:0123456789abcdefghijkl".into(),
            "spotify:track:0123456789abcdefghijkl".into(),
        ],
        start_index: 0,
        position_ms: 999,
        shuffle: false,
    };
    let restored = myx::player::controller::restored_command(Some(explicit), &snapshot);
    // The observed URI alone cannot identify which duplicate occurrence advanced.
    assert!(matches!(
        myx::player::intents::plan(&restored),
        Err(myx::webapi::ApiError::InvalidRequest)
    ));
}

#[test]
fn inactive_and_buffering_observations_retain_native_queue_and_the_last_local_item() {
    use myx::domain::{QueueEntry, QueueSnapshot, QueueSource};
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    snapshot.owned_device_verified = true;
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Authentication {
            logged_in: true,
            is_active: true,
        },
        false,
    );
    let uri = "spotify:episode:0123456789abcdefghijkl".to_owned();
    reducer::observe(
        &mut snapshot,
        PlayerEvent::TrackChanged { uri: uri.clone() },
        false,
    );
    let queue = QueueSnapshot {
        upcoming: vec![QueueEntry {
            uid: "native-1".into(),
            source: QueueSource::Autoplay,
            uri: uri.clone(),
            title: None,
        }],
        previous: vec![],
        complete_catalog_context: false,
    };
    reducer::observe(
        &mut snapshot,
        PlayerEvent::NativeQueue(queue.clone()),
        false,
    );
    let anchor = myx::domain::PositionAnchor {
        position_ms: 77,
        timestamp_ms: 12345,
        speed: 1.25,
    };
    reducer::observe(
        &mut snapshot,
        PlayerEvent::PositionAnchor {
            uri: uri.clone(),
            anchor,
        },
        false,
    );
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Buffering {
            uri: uri.clone(),
            position_ms: 77,
        },
        false,
    );
    assert_eq!(snapshot.playback, PlaybackStatus::Buffering);
    assert_eq!(snapshot.observed_position, Some(anchor));
    reducer::observe(&mut snapshot, PlayerEvent::LocallyInactive, false);
    reducer::observe(
        &mut snapshot,
        PlayerEvent::NativeQueue(QueueSnapshot::default()),
        false,
    );
    reducer::observe(
        &mut snapshot,
        PlayerEvent::TrackChanged {
            uri: "spotify:track:1123456789abcdefghijkl".into(),
        },
        false,
    );
    assert_eq!(snapshot.track.unwrap().uri, uri);
    assert_eq!(snapshot.queue, queue);
    assert_eq!(snapshot.observed_position, Some(anchor));
    assert_eq!(snapshot.position.speed, 0.0);
    assert_ne!(snapshot.playback, PlaybackStatus::Stopped);
}
