//! Read-only daemon catalog scenarios. These require an explicitly selected,
//! authorized test profile and never read an OAuth cache in the client.

use crate::*;

fn connection() -> Connection {
    let path = std::env::var_os("MYX_TEST_CONFIG")
        .map(std::path::PathBuf::from)
        .expect("MYX_TEST_CONFIG must select an authorized test daemon profile");
    let connection = Connection::ensure(&path).expect("connect test daemon");
    connection
        .prepare(false)
        .expect("test daemon must already be authorized");
    connection
}

const ARTIST_ID: &str = "3uwAm6vQy7kWPS2bciKWx9";
const ARTIST_NAME: &str = "girl in red";

#[test]
#[ignore = "requires an explicitly authorized test daemon and Spotify catalog"]
fn live_liked_songs_page_all_the_way_through() {
    let connection = connection();
    let client = http_client();
    let mut url = Some(format!("{API}/me/tracks?limit=50"));
    let mut tracks = 0;
    let mut total = None;
    while let Some(page) = url.take() {
        let value = get_json(&client, &page, &connection).expect("liked catalog page");
        total.get_or_insert(value["total"].as_u64().expect("liked total"));
        tracks += value["items"].as_array().expect("liked items").len();
        url = value["next"].as_str().map(String::from);
    }
    assert_eq!(Some(tracks as u64), total, "pagination dropped tracks");
}

#[test]
#[ignore = "requires an explicitly authorized test daemon and Spotify catalog"]
fn live_artist_track_results_are_not_empty() {
    let rows = artist_top_tracks(&http_client(), &connection(), ARTIST_ID, ARTIST_NAME);
    assert!(!rows.is_empty(), "artist track results unavailable");
    assert!(rows.iter().all(|row| row.is_track()));
}

#[test]
#[ignore = "requires an explicitly authorized test daemon and Spotify catalog"]
fn live_artist_albums_page_past_the_first_ten() {
    let rows = artist_albums(&http_client(), &connection(), ARTIST_ID);
    assert!(rows.len() > 10, "expected a paged discography");
    assert!(rows.iter().all(|row| !row.is_track() && !row.is_header()));
    let years: Vec<&str> = rows.iter().map(|row| row.subtitle.as_str()).collect();
    assert!(years.windows(2).all(|pair| pair[0] >= pair[1]));
}

#[test]
#[ignore = "requires an explicitly authorized test daemon and Spotify catalog"]
fn live_artist_detail_has_both_sections() {
    let (_, items) = fetch_detail_blocking(
        &connection(),
        &format!("spotify:artist:{ARTIST_ID}"),
        ARTIST_NAME,
    );
    let headers: Vec<&str> = items
        .iter()
        .filter(|item| item.is_header())
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(headers, ["Popular", "Albums"]);
    assert!(items.len() > 20);
}
