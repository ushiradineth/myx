use crate::*;
use serde_json::json;

fn ctx_row() -> LibItem {
    LibItem::ctx(
        "Chill Vibes".into(),
        "you · 142".into(),
        "spotify:playlist:1".into(),
    )
}

// ------------------------------------------------------ startup ordering

#[test]
fn authentication_finishes_before_terminal_initialization() {
    let order = std::cell::RefCell::new(Vec::new());

    let result = auth_then_terminal(
        || {
            order.borrow_mut().push("auth");
            Ok("authenticated")
        },
        || {
            order.borrow_mut().push("terminal");
            Ok("terminal")
        },
    )
    .expect("startup succeeds");

    assert_eq!(result, ("authenticated", "terminal"));
    assert_eq!(*order.borrow(), ["auth", "terminal"]);
}

#[test]
fn authentication_failure_never_initializes_terminal() {
    let terminal_initialized = std::cell::Cell::new(false);

    let result: Result<((), ())> = auth_then_terminal(
        || anyhow::bail!("daemon setup requires authorization"),
        || {
            terminal_initialized.set(true);
            Ok(())
        },
    );

    assert!(result.is_err());
    assert!(!terminal_initialized.get());
}

#[test]
fn active_scrub_rejects_stale_engine_position() {
    assert!(!should_apply_engine_position(true, Some(42_000)));
    assert!(should_apply_engine_position(true, None));
    assert!(should_apply_engine_position(false, Some(42_000)));
}

// -------------------------------------------------------- context_target

#[test]
fn attach_uses_daemon_transport_modes_and_preserves_duplicate_queue_entries() {
    use myx::domain::{QueueEntry, QueueSource};
    let mut snapshot = Snapshot::starting("test-profile".into(), "test-device".into());
    snapshot.volume = 0;
    snapshot.shuffle = true;
    snapshot.equalizer.gains_db[3] = -4;
    snapshot.queue.upcoming = vec![
        QueueEntry {
            uid: "first".into(),
            source: QueueSource::Queue,
            uri: "spotify:track:a".into(),
            title: Some("A".into()),
        },
        QueueEntry {
            uid: "second".into(),
            source: QueueSource::Queue,
            uri: "spotify:track:a".into(),
            title: Some("A".into()),
        },
    ];
    for repeat in [RepeatMode::Off, RepeatMode::One, RepeatMode::All] {
        snapshot.repeat = repeat;
        let transport = Transport::from_snapshot(&snapshot);
        assert_eq!(
            transport.volume, 0,
            "attach must not replace mute with a saved default"
        );
        assert!(transport.shuffle);
        assert_eq!(transport.repeat, repeat);
        assert_eq!(transport.equalizer, snapshot.equalizer);
        assert_eq!(transport.queue, ["A", "A"]);
        assert_eq!(transport.queue_uris, ["spotify:track:a", "spotify:track:a"]);
    }
}

#[test]
fn attach_preserves_observed_playback_states_without_inventing_metadata() {
    use myx::domain::{ItemKind, PositionAnchor};
    let mut snapshot = Snapshot::starting("test-profile".into(), "test-device".into());
    assert!(PlaybackState::from_snapshot(&snapshot, 100).now.is_none());
    let mut track =
        myx::domain::TrackMetadata::pending(ItemKind::Episode, "spotify:episode:a".into());
    track.duration_ms = Some(100_000);
    snapshot.track = Some(track);
    for status in [
        PlaybackStatus::Starting,
        PlaybackStatus::Playing,
        PlaybackStatus::Paused,
        PlaybackStatus::Buffering,
        PlaybackStatus::Stopped,
        PlaybackStatus::Disconnected,
        PlaybackStatus::Error,
    ] {
        snapshot.playback = status;
        snapshot.position = PositionAnchor {
            position_ms: 10_000,
            timestamp_ms: 100,
            speed: if status == PlaybackStatus::Playing {
                1.0
            } else {
                0.0
            },
        };
        let state = PlaybackState::from_snapshot(&snapshot, 1100);
        let now = state.now.unwrap();
        assert_eq!(now.uri, "spotify:episode:a");
        assert!(now.title.is_empty(), "a URI is not a title");
        assert_eq!(now.is_playing, status == PlaybackStatus::Playing);
        assert_eq!(
            now.position_ms,
            if status == PlaybackStatus::Playing {
                11_000
            } else {
                10_000
            }
        );
    }
}

#[test]
fn context_target_accepts_context_rows() {
    let (uri, name) = context_target(&ctx_row()).expect("playlist is a context");
    assert_eq!(uri, "spotify:playlist:1");
    assert_eq!(name, "Chill Vibes");
}

#[test]
fn context_target_accepts_synthesized_play_row() {
    // "▶︎ Play X" rows carry the context URI, so P works inside a drill-in.
    let row = LibItem::play("▶︎ Play Chill Vibes".into(), "spotify:playlist:1".into());
    assert_eq!(
        context_target(&row).map(|(u, _)| u),
        Some("spotify:playlist:1".to_string())
    );
}

#[test]
fn context_target_rejects_tracks_and_headers() {
    let track = LibItem::track("Song".into(), "Artist".into(), "spotify:track:9".into());
    assert!(context_target(&track).is_none());
    assert!(context_target(&LibItem::header("Songs")).is_none());
}

// --------------------------------------------------- parse_playlist_track

#[test]
fn parses_an_items_entry() {
    // The shape /playlists/{id}/items actually serves today.
    let it = json!({"added_at": "2024-01-01T00:00:00Z", "is_local": false, "item": {
        "name": "Coffee",
        "uri": "spotify:track:429NtPmr12aypzFH3FkN9l",
        "type": "track",
        "artists": [{"name": "beabadoobee"}]
    }});
    let li = parse_playlist_track(&it).expect("valid item");
    assert_eq!(li.name, "Coffee");
    assert_eq!(li.subtitle, "beabadoobee");
    assert_eq!(li.uri, "spotify:track:429NtPmr12aypzFH3FkN9l");
    assert!(li.is_track());
}

#[test]
fn still_parses_legacy_track_entry() {
    // Older /tracks shape, kept working through the API migration.
    let it = json!({"track": {
        "name": "Sailor Song",
        "uri": "spotify:track:abc",
        "artists": [{"name": "Gigi Perez"}]
    }});
    let li = parse_playlist_track(&it).expect("valid track");
    assert_eq!(li.name, "Sailor Song");
    assert_eq!(li.subtitle, "Gigi Perez");
}

#[test]
fn skips_null_entries() {
    // Real playlists contain these for items pulled from the catalogue.
    // Must yield None (skipped) rather than panic or abort the page.
    assert!(parse_playlist_track(&json!({ "item": null })).is_none());
    assert!(parse_playlist_track(&json!({ "track": null })).is_none());
    assert!(parse_playlist_track(&json!({})).is_none());
}

#[test]
fn collection_rows_keep_original_positions_through_null_filtering_and_sorting() {
    let result = crate::api::fetch_detail_with(
        |path| {
            if path.contains("fields=snapshot_id") {
                Ok(json!({"snapshot_id":"same"}))
            } else {
                Ok(json!({"items":[
                    {"item":{"name":"Z first","uri":"spotify:track:0123456789abcdefghijkl"}},
                    {"item":null},
                    {"item":{"name":"A duplicate","uri":"spotify:track:0123456789abcdefghijkl"}}
                ],"next":null,"total":3}))
            }
        },
        "spotify:playlist:0123456789abcdefghijkl",
        "Source",
    )
    .unwrap();
    let mut rows = result.items;
    assert!(rows[0].is_play());
    assert_eq!(rows[0].source_position, None);
    assert_eq!(rows[1].source_position, Some(0));
    assert_eq!(rows[2].source_position, Some(2));
    sort_list(&mut rows, SortMode::Title);
    assert_eq!(rows[1].name, "A duplicate");
    assert_eq!(rows[1].source_position, Some(2));
    assert_eq!(rows[2].source_position, Some(0));
}

#[test]
fn album_rows_keep_original_positions_before_filtering() {
    let result = crate::api::fetch_detail_with(
        |_| {
            Ok(json!({"items":[null,
            {"name":"B","uri":"spotify:track:1123456789abcdefghijkl"},
            {"name":"A","uri":"spotify:track:0123456789abcdefghijkl"}
        ],"next":null,"total":3}))
        },
        "spotify:album:0123456789abcdefghijkl",
        "Source",
    )
    .unwrap();
    assert_eq!(result.items[1].source_position, Some(1));
    assert_eq!(result.items[2].source_position, Some(2));
}

#[test]
fn skips_entry_without_uri() {
    let it = json!({"item": {"name": "No URI", "artists": [{"name": "X"}]}});
    assert!(parse_playlist_track(&it).is_none());
}

#[test]
fn missing_artists_yields_empty_artist_not_skip() {
    let it = json!({"item": {"name": "Untitled", "uri": "spotify:track:z"}});
    let li = parse_playlist_track(&it).expect("still playable without artists");
    assert_eq!(li.subtitle, "");
}

#[test]
fn total_prefers_items_over_legacy_tracks() {
    // Live /me/playlists shape: `items.total`, no `tracks` object at all.
    assert_eq!(
        playlist_total(&json!({"items": {"href": "…", "total": 155}})),
        Some(155)
    );
    assert_eq!(playlist_total(&json!({"tracks": {"total": 42}})), Some(42));
    assert_eq!(
        playlist_total(&json!({"items": {"total": 7}, "tracks": {"total": 9}})),
        Some(7)
    );
    assert_eq!(playlist_total(&json!({"name": "no counts"})), None);
}

#[test]
fn admits_local_files_and_episodes() {
    // Documents current behaviour: both parse as ordinary tracks.
    let local = json!({"is_local": true, "item": {
        "name": "Demo.mp3", "uri": "spotify:local:::Demo:180", "artists": [{"name": "Me"}]
    }});
    assert!(parse_playlist_track(&local).is_some());

    let episode = json!({"item": {
        "name": "Ep 12", "uri": "spotify:episode:e1", "type": "episode", "artists": []
    }});
    let li = parse_playlist_track(&episode).expect("episodes are admitted today");
    assert_eq!(li.subtitle, "");
}

// ------------------------------------------------------ playlist_subtitle

#[test]
fn subtitle_puts_count_before_owner() {
    // Count leads so it survives tail-first truncation in a narrow pane.
    assert_eq!(
        playlist_subtitle("ImLordVisssh", Some(155)),
        "155 · ImLordVisssh"
    );
    assert_eq!(playlist_subtitle("you", None), "you");
    assert_eq!(playlist_subtitle("", Some(12)), "12");
    assert_eq!(playlist_subtitle("", None), "");
}

// -------------------------------------------------------- meta_is_current

#[test]
fn stale_metadata_replies_are_dropped() {
    let a = "spotify:track:AAA";
    let b = "spotify:track:BBB";
    // Waiting on B: B's reply applies, A's late reply does not.
    assert!(meta_is_current(Some(b), b));
    assert!(!meta_is_current(Some(b), a));
    // Nothing outstanding -> accept (the guard only drops provable mismatches).
    assert!(meta_is_current(None, a));
}

// ------------------------------------------------------------ enter_label

#[test]
fn enter_label_matches_context_target() {
    let track = LibItem::track("Song".into(), "Artist".into(), "spotify:track:9".into());
    assert_eq!(enter_label(Some(&ctx_row())), "open");
    assert_eq!(enter_label(Some(&track)), "select");
    assert_eq!(enter_label(Some(&LibItem::header("Songs"))), "select");
    assert_eq!(enter_label(None), "select");

    // The invariant the footer relies on: Enter says "open" for exactly
    // the rows P can play.
    for row in [ctx_row(), track, LibItem::header("Songs")] {
        let opens = enter_label(Some(&row)) == "open";
        assert_eq!(opens, context_target(&row).is_some() && !row.is_play());
    }
}

#[test]
fn quitting_does_not_wait_for_an_outstanding_artwork_download() {
    let (release, blocked) = flume::bounded::<()>(1);
    let (finished, completion) = flume::bounded(1);
    let worker = std::thread::spawn(move || {
        let result = run_ui_runtime(async {
            let (started, observed) = tokio::sync::oneshot::channel();
            tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = blocked.recv();
            });
            observed.await.unwrap();
            Ok(())
        });
        finished.send(result.is_ok()).unwrap();
    });
    let result = completion.recv_timeout(Duration::from_millis(500));
    release.send(()).unwrap();
    worker.join().unwrap();
    assert!(
        result.unwrap(),
        "UI runtime shutdown must not block terminal restoration"
    );
}
