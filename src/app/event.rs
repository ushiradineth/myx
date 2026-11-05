//! Engine events and metadata replies, applied to `App`.

use crate::*;

pub(crate) fn handle_engine_event(
    app: &mut App,
    ev: EngineEvent,
    meta_tx: &flume::Sender<Reply<TrackMeta>>,
) {
    // Position ticks would bury everything else in the log.
    if !matches!(ev, EngineEvent::PositionCorrection { .. }) {
        liblog(format!("engine: {ev:?}"));
    }
    match ev {
        EngineEvent::ProcessGroups(_) => {}
        EngineEvent::ReadModel(snapshot) => {
            let authority_changed = app.session.observe_authority(
                &snapshot,
                &mut app.browse,
                &mut app.search,
                &mut app.view,
            );
            // A coalesced scrub belongs to the previous owner/account too;
            // never flush that pending command into the replacement authority.
            if authority_changed {
                app.playback.seek_target = None;
            }
            let mut transport = Transport::from_snapshot(&snapshot);
            if !authority_changed && app.transport.queue_revision == snapshot.queue_revision {
                transport.queue_offset = app.transport.queue_offset;
                transport.queue = std::mem::take(&mut app.transport.queue);
                transport.queue_uris = std::mem::take(&mut app.transport.queue_uris);
            } else if app.view.mode == RightView::Queue {
                let _ = app
                    .svc
                    .engine
                    .request_queue_page(Some(snapshot.queue_revision), 0);
            }
            app.transport = transport;
            app.playback
                .apply_snapshot(&snapshot, myx::domain::playback::now_ms());
            if authority_changed {
                // Canonical playback remains the daemon's; only viewer work and
                // account-derived catalog rows are discarded. The frame loop
                // demand-loads the selected section, not every user collection.
                app.status = "daemon authority changed; reloading current section…".into();
                app.art_repaint = ArtRepaint::Wipe;
                if let Some(now) = app.playback.now.as_mut() {
                    now.cover = None;
                }
                if let Some(track) = snapshot.track.as_ref() {
                    request_artwork(
                        app,
                        TrackMetadata {
                            uri: track.uri.clone(),
                            title: track.title.clone().unwrap_or_default(),
                            artist: track
                                .artists
                                .as_ref()
                                .map(|artists| artists.join(", "))
                                .unwrap_or_default(),
                            album: track.album.clone().unwrap_or_default(),
                            duration_ms: track.duration_ms.unwrap_or(0).min(u64::from(u32::MAX))
                                as u32,
                            cover_url: track.artwork_url.clone(),
                        },
                        meta_tx,
                    );
                }
            }
            if let Some(message) = snapshot
                .control_outcome
                .as_ref()
                .filter(|_| snapshot.control_pending || snapshot.control_error.is_some())
                .or(snapshot.message.as_ref())
            {
                app.status = message.clone();
            }
        }
        EngineEvent::TrackChanged { uri } => {
            app.session.artwork_request.cancel();
            app.session.lyrics_request.cancel();
            app.session.lyrics_key = None;
            app.session.lyrics_error = None;
            app.view.lyrics.clear();
            app.view.lyrics_synced = false;
            app.status = "loading track…".to_string();
            // The daemon resolves metadata. Keep the stale-artwork guard local.
            app.session.pending_meta = Some(uri);
        }
        EngineEvent::Metadata(meta) => {
            if !meta_is_current(app.session.pending_meta.as_deref(), &meta.uri) {
                return;
            }
            app.session.pending_meta = Some(meta.uri.clone());
            let lyrics_key = (
                meta.uri.clone(),
                meta.title.clone(),
                meta.artist.clone(),
                meta.album.clone(),
                meta.duration_ms,
            );
            if app
                .session
                .lyrics_key
                .as_ref()
                .is_some_and(|key| key != &lyrics_key)
            {
                app.session.lyrics_request.cancel();
                app.session.lyrics_key = None;
                app.session.lyrics_error = None;
                app.view.lyrics.clear();
                app.view.lyrics_synced = false;
            }
            // Establish the model synchronously, before the following Playing
            // event. Cover HTTP must not lose the first playhead/paused update.
            // Metadata/artwork supplements never replace the canonical clock.
            app.playback.apply_sdk_metadata(&meta);
            if app
                .transport
                .canonical
                .track
                .as_ref()
                .is_some_and(|track| track.uri == meta.uri)
            {
                app.playback
                    .apply_snapshot(&app.transport.canonical, myx::domain::playback::now_ms());
            }
            request_artwork(app, meta, meta_tx);
        }
        EngineEvent::QueueChanged { entries } => {
            // An empty SDK queue is authoritative too, not a failed API fetch.
            let previous = app
                .transport
                .queue_uris
                .iter()
                .zip(&app.transport.queue)
                .map(|(uri, title)| (uri.as_str(), title.as_str()));
            let known_tracks = app
                .browse
                .details
                .iter()
                .flat_map(|detail| detail.items.iter())
                .chain(app.search.search_results.iter())
                .chain(
                    Section::ALL
                        .into_iter()
                        .flat_map(|section| app.browse.library.items(section)),
                )
                .filter(|item| item.is_track())
                .map(|item| (item.uri.as_str(), item.name.as_str()));
            let (titles, uris) = queue_display(entries, previous.chain(known_tracks));
            app.transport.queue = titles;
            app.transport.queue_uris = uris;
        }
        EngineEvent::BackendStatus { message } => app.status = message,
        EngineEvent::Playing { position_ms, .. } => {
            app.transport.playback_started = true;
            if let Some(n) = app.playback.now.as_mut() {
                n.is_playing = true;
            }
            app.playback.set_local_position(position_ms, true);
        }
        EngineEvent::Paused { position_ms, .. } => {
            if let Some(n) = app.playback.now.as_mut() {
                n.is_playing = false;
            }
            app.playback.set_local_position(position_ms, true);
        }
        EngineEvent::Stopped => {
            app.session.artwork_request.cancel();
            app.session.lyrics_request.cancel();
            app.session.lyrics_key = None;
            app.view.lyrics.clear();
            // Discard cover work still in flight for the stopped track.
            app.session.pending_meta = Some(String::new());
            app.playback.now = None;
            app.transport.playback_started = false;
        }
        EngineEvent::PositionCorrection { position_ms, .. } => {
            app.playback.set_local_position(position_ms, true);
        }
        EngineEvent::Reconnecting => {
            app.playback.freeze();
            app.status = "connection dropped — reconnecting…".to_string();
        }
        EngineEvent::Reconnected => {
            app.status = "reconnected".to_string();
        }
        EngineEvent::Authentication {
            logged_in,
            is_active,
        } => {
            if !logged_in || !is_active {
                app.playback.freeze();
                app.status = if logged_in {
                    "playback is not locally active"
                } else {
                    "daemon authorization required"
                }
                .into();
            }
        }
        EngineEvent::EndOfTrack { .. } => {}
        EngineEvent::NativeQueue(_)
        | EngineEvent::Options { .. }
        | EngineEvent::Volume { .. }
        | EngineEvent::Context { .. }
        | EngineEvent::PositionAnchor { .. }
        | EngineEvent::AvailableActions { .. }
        | EngineEvent::Buffering { .. }
        | EngineEvent::LocallyInactive => {
            // SDK observations are reduced by the daemon. Only ReadModel is
            // authoritative here; inactive/buffering must never synthesize Stop.
        }
    }
}

fn request_artwork(app: &mut App, meta: TrackMetadata, tx: &flume::Sender<Reply<TrackMeta>>) {
    let tx = tx.clone();
    let webapi = app.svc.webapi.clone();
    let id = app.session.artwork_request.start();
    // Public image downloads/decoding stay outside the UI loop; an authority
    // replacement invalidates this generation even when the track URI is equal.
    tokio::task::spawn_blocking(move || {
        let _ = tx.send(Reply {
            id,
            result: Ok(fetch_sdk_artwork(&webapi, meta)),
        });
    });
}

fn queue_display<'a>(
    entries: Vec<(String, String)>,
    known: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> (Vec<String>, Vec<String>) {
    if entries
        .iter()
        .all(|(title, uri)| !title.is_empty() && title != uri)
    {
        return entries.into_iter().unzip();
    }
    // Deferred SDK queue entries contain only URIs. Reuse Web API metadata
    // already in memory, retaining previous labels after leaving a playlist.
    // Only enrich labels: SDK order, duplicate tracks and URIs are authoritative.
    let names: std::collections::HashMap<_, _> = known
        .into_iter()
        .filter(|(uri, title)| !title.is_empty() && title != uri)
        .collect();
    entries
        .into_iter()
        .map(|(mut title, uri)| {
            if title.is_empty() || title == uri {
                if let Some(known) = names.get(uri.as_str()) {
                    title = (*known).to_owned();
                } else {
                    title = "Loading metadata…".into();
                }
            }
            (title, uri)
        })
        .unzip()
}

/// Is this metadata reply the one we are still waiting for?
///
/// `None` means nothing specific was requested (e.g. a path that predates the
/// guard), so accept — the guard only ever discards a reply we can prove is for
/// a different track.
pub(crate) fn meta_is_current(pending: Option<&str>, meta_uri: &str) -> bool {
    pending.is_none_or(|p| p == meta_uri)
}

pub(crate) fn apply_meta(app: &mut App, meta: TrackMeta) {
    // Metadata fetches run on independent blocking tasks, so skipping quickly
    // (n/b) can land an earlier track's reply after a later one. Applying it
    // would replace the whole NowPlaying — title, artist and cover — with the
    // wrong track's data.
    if !meta_is_current(app.session.pending_meta.as_deref(), &meta.uri) {
        return;
    }

    if meta.image.image.is_none() && meta.image.url.is_some() {
        liblog("cover: no decoded artwork for current track");
    }

    let cover = meta
        .image
        .image
        .as_ref()
        .map(|img| Cover::from_image(img.clone(), app.svc.picker.clone()))
        .or_else(|| {
            app.playback
                .now
                .as_mut()
                .filter(|now| now.uri == meta.uri)
                .and_then(|now| now.cover.take())
        });
    // A different cover encodes to a different symbol, so the diff emits it on
    // its own — no wipe, which would flash a blank box between the two covers.
    app.art_repaint = ArtRepaint::Draw;

    app.playback.now = Some(NowPlaying {
        uri: meta.uri,
        title: meta.title,
        artist: meta.artist,
        album: meta.album,
        duration_ms: meta.duration_ms,
        position_ms: app
            .playback
            .now
            .as_ref()
            .map(|n| n.position_ms)
            .unwrap_or(0),
        position_at: Instant::now(),
        is_playing: app
            .playback
            .now
            .as_ref()
            .map(|n| n.is_playing)
            .unwrap_or(app.transport.playback_started),
        cover,
    });

    if let Some(theme) = meta.theme {
        app.theme.start_fade(theme);
    }
}

/// Does this row carry a playable context URI, and under what name?
///
/// Context rows (playlist / album / artist) and the synthesized "▶︎ Play X"
/// rows both do; headers and tracks do not. Kept pure and free-standing so it
/// is unit-testable without a daemon connection. `enter_label` shares this predicate so Enter opens exactly the rows
/// `P` plays.
pub(crate) fn context_target(item: &LibItem) -> Option<(String, String)> {
    (!item.is_header() && !item.is_track()).then(|| (item.uri.clone(), item.name.clone()))
}

/// Enter opens context rows and plays everything else.
pub(crate) fn enter_label(item: Option<&LibItem>) -> &'static str {
    match item {
        Some(i) if !i.is_track() && !i.is_header() => "open",
        _ => "select",
    }
}

/// `P` / `S`: play the highlighted context from anywhere — library section,
/// search results, or inside a drill-in (`cur_items` resolves all three).
pub(crate) fn play_selected_context(app: &mut App, shuffle: bool) {
    let Some(item) = app.cur_items().get(app.browse.selected).cloned() else {
        return;
    };
    match context_target(&item) {
        Some((uri, name)) => app.play_context_row(uri, name, shuffle),
        None => app.status = "not a playlist, album, or artist".to_string(),
    }
}

#[cfg(test)]
mod artwork_tests {
    use super::meta_is_current;

    #[test]
    fn delayed_artwork_is_rejected_after_track_change_or_stop() {
        let old_uri = "spotify:track:old";
        assert!(meta_is_current(Some(old_uri), old_uri));
        assert!(!meta_is_current(Some("spotify:track:new"), old_uri));
        assert!(!meta_is_current(Some(""), old_uri));
        assert!(meta_is_current(None, old_uri));
    }
}

#[cfg(test)]
mod queue_tests {
    use super::queue_display;

    #[test]
    fn pending_queue_tracks_reuse_known_titles_without_reordering_or_deduplication() {
        let entries = vec![
            ("SDK title".into(), "spotify:track:a".into()),
            ("spotify:track:b".into(), "spotify:track:b".into()),
            ("".into(), "spotify:track:c".into()),
            ("spotify:track:b".into(), "spotify:track:b".into()),
            ("spotify:track:d".into(), "spotify:track:d".into()),
        ];
        let (titles, uris) = queue_display(
            entries,
            [
                ("spotify:track:a", "Library title"),
                ("spotify:track:b", "Cached title"),
                ("spotify:track:b", "Playlist title"),
                ("spotify:track:c", "Search title"),
                ("spotify:track:d", ""),
            ],
        );
        assert_eq!(
            titles,
            [
                "SDK title",
                "Playlist title",
                "Search title",
                "Playlist title",
                "Loading metadata…"
            ]
        );
        assert_eq!(
            uris,
            [
                "spotify:track:a",
                "spotify:track:b",
                "spotify:track:c",
                "spotify:track:b",
                "spotify:track:d"
            ]
        );
        let (preserved, _) = queue_display(
            vec![("spotify:track:b".into(), "spotify:track:b".into())],
            uris.iter()
                .zip(&titles)
                .map(|(uri, title)| (uri.as_str(), title.as_str())),
        );
        assert_eq!(preserved, ["Playlist title"]);
        assert_eq!(
            queue_display(Vec::new(), [("spotify:track:a", "Known")]),
            (Vec::new(), Vec::new())
        );
    }
}
