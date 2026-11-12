//! Public discography is availability-filtered paging, not a playlist snapshot.
use crate::*;
use serde_json::{json, Value};

fn album(id: usize) -> Value {
    json!({"name":"Same title", "uri":format!("spotify:album:{id}"),
        "release_date":format!("2026-01-{:02}", id % 28 + 1)})
}

#[test]
fn discography_follows_next_despite_changing_totals_and_short_available_pages() {
    for totals in [[35, 34, 36, 36], [35, 35, 35, 35], [8, 8, 8, 8]] {
        let mut calls = 0;
        let rows = artist_albums(
            |path| {
                assert!(path.starts_with("/v1/artists/test/albums?"));
                let page = calls;
                calls += 1;
                let count = [10, 7, 10, 5][page];
                Ok(json!({"items":(0..count).map(|id| album(page * 10 + id)).collect::<Vec<_>>(),
                    "total":totals[page], "next":if page < 3 {
                        Some(format!("https://api.spotify.com/v1/artists/test/albums?offset={}", calls * 10))
                    } else { None }}))
            },
            "test",
        )
        .expect("provider EOF, not total equality, completes discography");
        assert_eq!(calls, 4);
        assert_eq!(rows.len(), 32);
        assert!(rows
            .windows(2)
            .all(|pair| pair[0].subtitle >= pair[1].subtitle));
        assert!(rows.iter().any(|row| row.uri == "spotify:album:34"));
    }
}

#[test]
fn discography_has_no_arbitrary_collection_cap_or_title_deduplication() {
    let mut offset = 0;
    let rows = artist_albums(
        |path| {
            assert!(path.contains("limit=10"));
            let end = (offset + 10).min(5107);
            let items: Vec<_> = (offset..end).map(album).collect();
            offset = end;
            Ok(json!({"items":items,"total":5107,"next":if end < 5107 {
            Some(format!("/v1/artists/test/albums?limit=10&offset={end}"))
        } else {None}}))
        },
        "test",
    )
    .unwrap();
    assert_eq!(rows.len(), 5107);
    assert_eq!(offset, 5107);
}

#[test]
fn discography_later_errors_never_return_partial_success() {
    for error in [
        ApiError::Restricted,
        ApiError::Quota,
        ApiError::Unauthorized,
        ApiError::Offline,
        ApiError::RateLimited {
            retry_after_secs: Some(5),
        },
    ] {
        let mut calls = 0;
        let result = artist_albums(
            |_| {
                calls += 1;
                if calls == 2 {
                    return Err(error.clone());
                }
                Ok(
                    json!({"items":[album(1)],"total":35,"next":"/v1/artists/test/albums?offset=10"}),
                )
            },
            "test",
        );
        assert!(matches!(result, Err(actual) if actual == error));
        assert_eq!(calls, 2);
    }
}

#[test]
fn discography_retains_origin_cycle_and_progress_safety() {
    for next in [
        "https://evil.test/v1/artists/test/albums",
        "/v1/artists/test/../albums",
        "https://api.spotify.com:443/v1/artists/test/albums",
        "/v1/artists/test/albums?include_groups=album,single&limit=10",
    ] {
        let mut calls = 0;
        let result = artist_albums(
            |_| {
                calls += 1;
                Ok(json!({"items":[album(1)],"total":35,"next":next}))
            },
            "test",
        );
        assert!(matches!(
            result,
            Err(ApiError::InvalidRequest | ApiError::CollectionChanged)
        ));
        assert_eq!(calls, 1);
    }
    assert!(matches!(
        artist_albums(
            |_| Ok(json!({"items":[],"total":35,
        "next":"/v1/artists/test/albums?offset=10"})),
            "test"
        ),
        Err(ApiError::CollectionChanged)
    ));
}

#[test]
fn malformed_discography_is_not_successfully_empty() {
    for page in [
        json!({}),
        json!({"items":null,"next":null}),
        json!({"items":[],"next":false}),
        json!({"items":[],"total":-1,"next":null}),
        json!({"items":[],"total":"35","next":null}),
    ] {
        assert!(matches!(
            artist_albums(|_| Ok(page.clone()), "test"),
            Err(ApiError::InvalidResponse)
        ));
    }
}

#[test]
fn popular_access_failure_is_honest_without_erasing_discography_or_fabricating_tracks() {
    for error in [ApiError::Restricted, ApiError::NotFound] {
        let mut calls = 0;
        let detail = fetch_detail_with(
            |path| {
                calls += 1;
                assert!(!path.contains("search"));
                if path.ends_with("/top-tracks") {
                    return Err(error.clone());
                }
                Ok(json!({"items":[album(1)],"total":8,"next":null}))
            },
            "spotify:artist:test",
            "Test",
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert!(detail.notice.unwrap().contains("Popular unavailable"));
        assert!(detail.items.iter().any(|row| row.uri == "spotify:album:1"));
        assert!(!detail.items.iter().any(LibItem::is_track));
    }
}

#[test]
fn discography_total_tolerance_does_not_relax_playlist_completeness() {
    let mut calls = 0;
    let result = playlist_contents(
        |path| {
            calls += 1;
            if path.contains("snapshot_id") {
                return Ok(json!({"snapshot_id":"stable"}));
            }
            Ok(json!({"items":[{"item":{"uri":"spotify:track:test"}}],"total":2,"next":null}))
        },
        "test",
    );
    assert!(matches!(result, Err(ApiError::CollectionChanged)));
    assert_eq!(calls, 2);
}
