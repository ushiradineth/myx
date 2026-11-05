//! MAC2 deterministic fixtures. No account, daemon, browser or native SDK.
use crate::*;
use serde_json::{json, Value};

#[test]
fn more_than_five_thousand_liked_tracks_are_delivered_only_after_the_last_page() {
    let mut offset = 0;
    let mut calls = 0;
    let rows = fetch_section_with(|path| {
        assert!(path.starts_with("/v1/me/tracks?"));
        let end = (offset + 50).min(5107);
        let items: Vec<_> = (offset..end).map(|id| json!({"track": {
            "uri":format!("spotify:track:{id:022}"),"name":format!("Track {id}"),"artists":[{"name":"Artist"}]
        }})).collect();
        offset = end;
        calls += 1;
        Ok(json!({"items":items,"total":5107,"next":if end < 5107 {Some(format!("/v1/me/tracks?offset={end}"))} else {None}}))
    }, Section::Liked).unwrap();
    assert_eq!(calls, 103);
    assert_eq!(rows.iter().filter(|row| row.is_track()).count(), 5107);
    assert_eq!(
        rows.last().unwrap().uri,
        format!("spotify:track:{:022}", 5106)
    );
    assert_eq!(rows.last().unwrap().source_position, Some(5106));
}

#[test]
fn liked_headers_and_unavailable_rows_do_not_change_authoritative_selection() {
    let entries = vec![
        json!({"track":{"name":"Z","uri":"spotify:track:0123456789abcdefghijkl"}}),
        Value::Null,
        json!({"track":null}),
        json!({"track":{"name":"Unavailable","uri":"spotify:track:1123456789abcdefghijkl","is_playable":false}}),
        json!({"track":{"name":"A duplicate","uri":"spotify:track:0123456789abcdefghijkl"}}),
    ];
    let mut rows = fetch_section_with(
        |_| Ok(json!({"items":entries,"next":null,"total":5})),
        Section::Liked,
    )
    .unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows[0].is_play());
    assert!(rows[1].is_header());
    assert_eq!(rows[0].source_position, None);
    assert_eq!(rows[1].source_position, None);
    sort_list(&mut rows, SortMode::Title);
    let selected = &rows[2];
    assert_eq!(selected.name, "A duplicate");
    assert_eq!(selected.source_position, Some(4));
    let command = myx::domain::PlayerCommand::PlayLiked {
        start: Some(selected.uri.clone()),
        source_position: selected.source_position,
        position_ms: 0,
        shuffle: true,
    };
    let myx::player::intents::LoadPlan::WebApi { body, .. } =
        myx::player::intents::resolve_liked(&command, &entries).unwrap()
    else {
        panic!("expected full source");
    };
    assert_eq!(
        body["uris"],
        json!([
            "spotify:track:0123456789abcdefghijkl",
            "spotify:track:0123456789abcdefghijkl"
        ])
    );
    assert_eq!(body["offset"], json!({"position":1}));
}

#[test]
fn page_two_restriction_is_a_typed_failure_not_an_empty_or_partial_collection() {
    let mut calls = 0;
    let result = fetch_section_with(
        |_| {
            calls += 1;
            if calls == 2 {
                return Err(ApiError::Restricted);
            }
            Ok(
                json!({"items":[{"track":{"name":"A","uri":"spotify:track:a"}}],"total":2,"next":"/v1/me/tracks?offset=1"}),
            )
        },
        Section::Liked,
    );
    assert!(matches!(result, Err(ApiError::Restricted)));
    assert_eq!(calls, 2);
}

#[test]
fn successfully_empty_is_loaded_but_an_error_never_replaces_valid_rows() {
    let empty = fetch_section_with(
        |_| Ok(json!({"items":[],"total":0,"next":null})),
        Section::Liked,
    )
    .unwrap();
    assert!(empty.is_empty());
    let mut library = Library::default();
    let id = library.request(Section::Liked, false).unwrap();
    assert!(library.request(Section::Liked, false).is_none());
    library.receive(
        Section::Liked,
        Reply {
            id,
            result: Ok(empty),
        },
    );
    assert!(library.is_loaded(Section::Liked));
    assert!(library.error(Section::Liked).is_none());
    library.set(
        Section::Liked,
        vec![LibItem::track(
            "A".into(),
            "Artist".into(),
            "spotify:track:a".into(),
        )],
    );
    let id = library.request(Section::Liked, true).unwrap();
    library.receive(
        Section::Liked,
        Reply {
            id,
            result: Err(ApiError::Quota),
        },
    );
    assert_eq!(library.items(Section::Liked)[0].name, "A");
    assert_eq!(library.error(Section::Liked), Some(&ApiError::Quota));
    assert!(library.request(Section::Liked, false).is_none());
}

#[test]
fn home_requests_its_recent_and_user_top_groups_without_loading_other_user_collections() {
    let mut paths = Vec::new();
    fetch_section_with(
        |path| {
            paths.push(path.to_owned());
            Ok(json!({"items":[],"total":0,"next":null}))
        },
        Section::Home,
    )
    .unwrap();
    assert_eq!(
        paths,
        [
            "/v1/me/player/recently-played?limit=50",
            "/v1/me/top/tracks?limit=50",
            "/v1/me/top/artists?limit=50"
        ]
    );
    let rows = fetch_section_with(|_| Ok(json!({"artists":{"items":[{"name":"Artist","uri":"spotify:artist:a"}],"next":null}})), Section::Artists).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uri, "spotify:artist:a");
}

#[test]
fn playlist_revision_change_rejects_even_a_same_length_reordered_collection() {
    let mut revisions = 0;
    let result = playlist_contents(
        |path| {
            if path.contains("fields=snapshot_id") {
                revisions += 1;
                return Ok(json!({"snapshot_id":if revisions == 1 {"before"} else {"reordered"}}));
            }
            Ok(
                json!({"items":[{"item":{"uri":"spotify:track:b"}},{"item":{"uri":"spotify:track:a"}}],"total":2,"next":null}),
            )
        },
        "test",
    );
    assert!(matches!(result, Err(ApiError::CollectionChanged)));
    assert_eq!(revisions, 2);
}

#[test]
fn playlist_requires_authorized_snapshot_reads_and_preserves_duplicates() {
    assert!(matches!(
        playlist_contents(|_| Err(ApiError::Restricted), "test"),
        Err(ApiError::Restricted)
    ));
    let result = playlist_contents(|path| {
        if path.contains("fields=snapshot_id") { return Ok(json!({"snapshot_id":"stable"})); }
        Ok(json!({"items":[{"item":{"uri":"spotify:track:a"}},{"item":{"uri":"spotify:track:a"}}],"total":2,"next":null}))
    }, "test").unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], result[1]);
}

#[test]
fn stale_success_and_errors_cannot_finish_a_new_query_detail_menu_artwork_or_lyric_visit() {
    for _ in ["query", "detail", "menu", "artwork", "lyrics"] {
        let mut gate = RequestGate::default();
        let old = gate.start();
        let current = gate.start();
        assert!(!gate.finish(old));
        assert!(gate.pending());
        assert!(gate.finish(current));
        assert!(!gate.finish(current), "duplicate deliveries are stale too");
        let reopened = gate.start();
        gate.cancel();
        assert!(
            !gate.finish(reopened),
            "closing a view invalidates its replies"
        );
    }
    let mut library = Library::default();
    let old = library.request(Section::Albums, false).unwrap();
    let current = library.request(Section::Albums, true).unwrap();
    assert!(!library.receive(
        Section::Albums,
        Reply {
            id: old,
            result: Err(ApiError::Unauthorized)
        }
    ));
    assert!(library.receive(
        Section::Albums,
        Reply {
            id: current,
            result: Ok(Vec::new())
        }
    ));
    assert!(library.error(Section::Albums).is_none());
}

#[test]
fn ipc_classifications_survive_anyhow_context_without_rendering_secret_messages() {
    use myx::ipc::protocol::{ErrorCode, IpcError};
    for (code, expected) in [
        (ErrorCode::Unauthorized, ApiError::Unauthorized),
        (ErrorCode::Restricted, ApiError::Restricted),
        (ErrorCode::Quota, ApiError::Quota),
        (ErrorCode::Unavailable, ApiError::Offline),
        (ErrorCode::WriteUncertain, ApiError::AmbiguousWrite),
        (ErrorCode::CollectionChanged, ApiError::CollectionChanged),
    ] {
        let error = anyhow::Error::new(IpcError {
            code,
            message: "PRIVATE-SENTINEL".into(),
            retry_after_secs: None,
        })
        .context("PRIVATE-SENTINEL");
        let classified = fetch_error(error);
        assert_eq!(classified, expected);
        assert!(!classified.to_string().contains("PRIVATE-SENTINEL"));
    }
    let classified = fetch_error(anyhow::Error::new(IpcError {
        code: ErrorCode::RateLimited,
        message: "PRIVATE-SENTINEL".into(),
        retry_after_secs: Some(90),
    }));
    assert_eq!(
        classified,
        ApiError::RateLimited {
            retry_after_secs: Some(90)
        }
    );
}

#[test]
fn membership_unknown_is_not_false_and_ambiguous_writes_are_not_repeated() {
    assert_eq!(
        write_error(anyhow::anyhow!("PRIVATE-SENTINEL: reply lost")),
        ApiError::AmbiguousWrite
    );
    assert_eq!(
        write_error(anyhow::Error::new(ApiError::Restricted)),
        ApiError::Restricted
    );
    assert_eq!(parse_contains(&json!([false])), Ok(false));
    assert_eq!(parse_contains(&json!([true])), Ok(true));
    for malformed in [
        Value::Null,
        json!([]),
        json!([null]),
        json!({"error":"restricted"}),
    ] {
        assert_eq!(parse_contains(&malformed), Err(ApiError::InvalidResponse));
    }
    for write_result in [Ok(Value::Null), Err(ApiError::AmbiguousWrite)] {
        let writes = std::cell::Cell::new(0);
        let reads = std::cell::Cell::new(0);
        let result = reconcile_membership(
            || {
                writes.set(writes.get() + 1);
                write_result
            },
            || {
                reads.set(reads.get() + 1);
                Ok(true)
            },
            true,
        );
        assert_eq!(result, Ok(()));
        assert_eq!((writes.get(), reads.get()), (1, 1));
    }
    assert_eq!(
        reconcile_membership(|| Err(ApiError::AmbiguousWrite), || Ok(false), true),
        Err(ApiError::AmbiguousWrite)
    );
    assert_eq!(
        reconcile_membership(|| Ok(Value::Null), || Err(ApiError::Restricted), false),
        Err(ApiError::AmbiguousWrite)
    );
    assert_eq!(
        reconcile_membership(
            || Err(ApiError::Restricted),
            || panic!("no write accepted; no reconciliation needed"),
            true
        ),
        Err(ApiError::Restricted)
    );
}

#[test]
fn immediate_menu_does_not_offer_a_mutation_for_unknown_membership() {
    let menu = build_action_menu(
        None,
        &LibItem::track("A".into(), "Artist".into(), "spotify:track:a".into()),
    )
    .unwrap();
    assert!(menu
        .items
        .iter()
        .all(|item| !matches!(item.kind, ActionKind::ToggleLike { .. })));
}

#[test]
fn broad_search_reports_the_real_provider_offset_bound_and_keeps_reachable_rows() {
    let mut calls = 0;
    let result = search_with(|path| {
        calls += 1;
        let url = reqwest::Url::parse(&format!("https://api.spotify.com{path}")).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let kind = &params["type"];
        let container = format!("{kind}s");
        let offset = params.get("offset").map(|s| s.parse::<usize>().unwrap()).unwrap_or(0);
        let total = if kind == "track" { 12000 } else { 0 };
        let items: Vec<_> = if total == 0 { Vec::new() } else { (offset..offset + 10).map(|id| json!({"name":format!("A {id}"),"uri":format!("spotify:track:{id}")})).collect() };
        Ok(json!({(container):{"items":items,"offset":offset,"total":total,"next":if total > offset + 10 { Some(format!("/v1/search?q=broad&type={kind}&limit=10&offset={}", offset + 10)) } else { None }}}))
    }, "broad").unwrap();
    assert_eq!(calls, 104);
    assert_eq!(
        result.rows.iter().filter(|row| row.is_track()).count(),
        1010
    );
    assert!(result.notice.unwrap().contains("partial"));
}

#[test]
fn search_access_restrictions_are_explicit_and_do_not_erase_authorized_groups() {
    let result = search_with(|path| {
        if !path.contains("type=track&") { return Err(ApiError::Restricted); }
        Ok(json!({"tracks":{"items":[{"name":"A","uri":"spotify:track:a"}],"total":1,"next":null}}))
    }, "query").unwrap();
    assert_eq!(result.rows.iter().filter(|row| row.is_track()).count(), 1);
    assert!(result.notice.unwrap().contains("partial search results"));
    assert!(matches!(
        search_with(|_| Err(ApiError::Unauthorized), "query"),
        Err(ApiError::Unauthorized)
    ));
    assert!(matches!(
        search_with(|_| Err(ApiError::Restricted), "query"),
        Err(ApiError::Restricted)
    ));
}

#[test]
fn add_to_playlist_requires_a_server_revision_and_does_not_guess_from_existing_membership() {
    assert!(
        confirm_playlist_write(Ok(json!({"snapshot_id":"revision"})))
            .unwrap()
            .refresh_library
    );
    for value in [Value::Null, json!({}), json!({"snapshot_id":""})] {
        assert!(matches!(
            confirm_playlist_write(Ok(value)),
            Err(ApiError::AmbiguousWrite)
        ));
    }
    assert!(matches!(
        confirm_playlist_write(Err(ApiError::AmbiguousWrite)),
        Err(ApiError::AmbiguousWrite)
    ));
    assert!(matches!(
        confirm_playlist_write(Err(ApiError::Restricted)),
        Err(ApiError::Restricted)
    ));
}

#[test]
fn artist_restriction_keeps_authorized_discography_without_search_substitution() {
    let mut paths = Vec::new();
    let detail = fetch_detail_with(|path| {
        paths.push(path.to_owned());
        if path.contains("/top-tracks") { return Err(ApiError::Restricted); }
        Ok(json!({"items":[{"name":"Album","uri":"spotify:album:a","release_date":"2026"}],"total":1,"next":null}))
    }, "spotify:artist:a", "Artist").unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths[0].ends_with("/artists/a/top-tracks"));
    assert!(paths.iter().all(|path| !path.contains("/search")));
    assert!(detail.notice.unwrap().contains("Popular unavailable"));
    assert!(detail.items.iter().any(|row| row.uri == "spotify:album:a"));
    assert!(matches!(
        fetch_detail_with(|_| Err(ApiError::Quota), "spotify:artist:a", "Artist"),
        Err(ApiError::Quota)
    ));
}

#[test]
fn album_detail_uses_all_direct_track_pages_not_a_nested_album_first_page() {
    let mut calls = 0;
    let detail = fetch_detail_with(|path| {
        assert!(path.starts_with("/v1/albums/a/tracks?"));
        calls += 1;
        Ok(json!({"items":[{"name":format!("Track {calls}"),"uri":format!("spotify:track:{calls}")}],"total":2,
            "next":if calls == 1 {Some("/v1/albums/a/tracks?offset=1")} else {None}}))
    }, "spotify:album:a", "Album").unwrap();
    assert_eq!(calls, 2);
    assert_eq!(detail.items.iter().filter(|row| row.is_track()).count(), 2);
}

fn authority_views(snapshot: &Snapshot) -> (SessionState, BrowseState, SearchState, ViewState) {
    let session = SessionState {
        authority: Some(CatalogAuthority::from_snapshot(snapshot)),
        pending_meta: Some("spotify:track:old".into()),
        search_error: Some(ApiError::Restricted),
        menu_error: Some(ApiError::Unauthorized),
        lyrics_error: Some(ApiError::Offline),
        search_notice: Some("old search notice".into()),
        mutation_uncertain: true,
        menu_item: Some(LibItem::track(
            "Old account track".into(),
            "Old artist".into(),
            "spotify:track:old".into(),
        )),
        lyrics_key: Some((
            "spotify:track:old".into(),
            "Old title".into(),
            "Old artist".into(),
            "Old album".into(),
            100,
        )),
        ..Default::default()
    };
    let browse = BrowseState {
        library: Library::default(),
        section: Section::Albums,
        selected: 4,
        sort: SortMode::Title,
        details: vec![Detail {
            context_uri: "spotify:playlist:old".into(),
            title: "Old playlist".into(),
            items: vec![LibItem::header("old detail rows")],
            parent_selected: 2,
        }],
    };
    let mut search = SearchState {
        input_mode: true,
        input: Default::default(),
        searching: true,
        in_flight: true,
        search_results: vec![LibItem::header("old search rows")],
    };
    search.input.insert_str("user-entered query");
    let view = ViewState {
        mode: RightView::Lyrics,
        zen: true,
        lyrics: vec![(0, "old lyrics".into())],
        lyrics_synced: true,
        actions: Some(ActionMenu {
            title: "old account menu".into(),
            selected: 0,
            items: Vec::new(),
        }),
        equalizer: Some(EqualizerOverlay { selected_band: 3 }),
    };
    (session, browse, search, view)
}

#[test]
fn oauth_replacement_and_owner_restart_clear_all_old_account_views_without_resetting_generations() {
    let mut attached = Snapshot::starting("test-profile".into(), "test-device".into());
    attached.session_id = "owner-a".into();
    attached.auth_revision = 3;
    for (session_id, revision) in [("owner-a", 4), ("owner-b", 3), ("owner-b", 0)] {
        let (mut session, mut browse, mut search, mut view) = authority_views(&attached);
        let mut old_sections = Vec::new();
        for section in Section::ALL {
            browse
                .library
                .set(section, vec![LibItem::header("old account rows")]);
            old_sections.push((section, browse.library.request(section, true).unwrap()));
        }
        let old_requests: Vec<_> = [
            &mut session.search_request,
            &mut session.detail_request,
            &mut session.menu_request,
            &mut session.action_request,
            &mut session.artwork_request,
            &mut session.lyrics_request,
        ]
        .into_iter()
        .map(RequestGate::start)
        .collect();
        let mut changed = attached.clone();
        changed.session_id = session_id.into();
        changed.auth_revision = revision;
        changed.track = Some(myx::domain::TrackMetadata::pending(
            myx::domain::ItemKind::Track,
            "spotify:track:new".into(),
        ));
        assert!(session.observe_authority(&changed, &mut browse, &mut search, &mut view));
        assert_eq!(
            session.authority,
            Some(CatalogAuthority::from_snapshot(&changed))
        );
        assert_eq!(session.pending_meta.as_deref(), Some("spotify:track:new"));
        for (section, old_id) in old_sections {
            assert!(browse.library.items(section).is_empty());
            assert!(!browse.library.is_loaded(section));
            assert!(browse.library.error(section).is_none());
            // The first request under the new authority must not reuse old_id.
            let new_id = browse.library.request(section, false).unwrap();
            assert_ne!(new_id, old_id);
            assert!(!browse.library.receive(
                section,
                Reply {
                    id: old_id,
                    result: Ok(vec![LibItem::header("stale success")])
                }
            ));
            assert!(!browse.library.receive(
                section,
                Reply {
                    id: old_id,
                    result: Err(ApiError::Quota)
                }
            ));
            assert!(browse.library.receive(
                section,
                Reply {
                    id: new_id,
                    result: Ok(Vec::new())
                }
            ));
        }
        for (gate, old_id) in [
            &mut session.search_request,
            &mut session.detail_request,
            &mut session.menu_request,
            &mut session.action_request,
            &mut session.artwork_request,
            &mut session.lyrics_request,
        ]
        .into_iter()
        .zip(old_requests)
        {
            assert!(!gate.pending());
            let new_id = gate.start();
            assert_ne!(new_id, old_id);
            assert!(
                !gate.finish(old_id),
                "an old-account success or error is stale"
            );
            assert!(gate.finish(new_id));
        }
        assert!(browse.details.is_empty());
        assert!(browse.section == Section::Albums && browse.sort == SortMode::Title);
        assert_eq!(browse.selected, 0);
        assert!(search.search_results.is_empty());
        assert!(!search.searching && !search.input_mode && !search.in_flight);
        assert_eq!(
            search.query(),
            "user-entered query",
            "viewer input is not account data"
        );
        assert!(view.actions.is_none() && view.lyrics.is_empty() && !view.lyrics_synced);
        assert!(view.mode == RightView::Lyrics && view.zen);
        assert_eq!(view.equalizer.as_ref().unwrap().selected_band, 3);
        assert!(
            session.search_error.is_none()
                && session.menu_error.is_none()
                && session.lyrics_error.is_none()
        );
        assert!(
            session.menu_item.is_none()
                && session.lyrics_key.is_none()
                && session.search_notice.is_none()
        );
        assert!(!session.mutation_uncertain);
    }
}

#[test]
fn same_authority_read_models_leave_pending_requests_rows_and_viewer_navigation_intact() {
    let mut snapshot = Snapshot::starting("test-profile".into(), "test-device".into());
    snapshot.session_id = "stable-owner".into();
    snapshot.auth_revision = 7;
    let (mut session, mut browse, mut search, mut view) = authority_views(&snapshot);
    let id = session.search_request.start();
    let section_id = browse.library.request(Section::Albums, false).unwrap();
    snapshot.sequence += 1;
    snapshot.volume = 52;
    assert!(!session.observe_authority(&snapshot, &mut browse, &mut search, &mut view));
    assert!(session.search_request.finish(id));
    assert!(browse.library.receive(
        Section::Albums,
        Reply {
            id: section_id,
            result: Ok(Vec::new())
        }
    ));
    assert_eq!(browse.details[0].title, "Old playlist");
    assert_eq!(search.search_results[0].name, "old search rows");
    assert!(view.actions.is_some() && !view.lyrics.is_empty());
}

#[test]
fn authority_reset_keeps_the_selected_section_and_leaves_other_sections_unrequested() {
    let mut snapshot = Snapshot::starting("test-profile".into(), "test-device".into());
    snapshot.session_id = "owner".into();
    let (mut session, mut browse, mut search, mut view) = authority_views(&snapshot);
    snapshot.auth_revision += 1;
    assert!(session.observe_authority(&snapshot, &mut browse, &mut search, &mut view));
    // The production demand loop requests browse.section only; reset itself
    // neither marks sections requested nor starts background collection fetches.
    let id = browse.library.request(browse.section, false).unwrap();
    assert!(browse.section == Section::Albums);
    assert!(browse.library.request(Section::Albums, false).is_none());
    assert!(!browse.library.is_loaded(Section::Home));
    assert!(browse.library.receive(
        Section::Albums,
        Reply {
            id,
            result: Ok(Vec::new())
        }
    ));
    assert!(
        !session.search_request.pending()
            && !session.detail_request.pending()
            && !session.menu_request.pending()
    );
}
