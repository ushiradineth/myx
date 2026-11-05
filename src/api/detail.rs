//! Strict collection detail, with a separately reported unavailable Popular endpoint.
use super::*;
use crate::*;

pub(crate) struct DetailResult {
    pub(crate) uri: String,
    pub(crate) title: String,
    pub(crate) items: Vec<LibItem>,
    pub(crate) notice: Option<String>,
}

pub(crate) fn spawn_detail_fetch(
    webapi: Arc<Mutex<WebApi>>,
    uri: String,
    name: String,
    id: u64,
    tx: flume::Sender<Reply<DetailResult>>,
) {
    tokio::task::spawn_blocking(move || {
        let result = connection_of(&webapi)
            .and_then(|connection| fetch_detail_blocking(&connection, &uri, &name));
        let _ = tx.send(Reply { id, result });
    });
}

/// Revision and content reads must bypass the owner's cache: comparing two
/// cached revisions cannot detect a concurrent reorder between content pages.
pub(crate) fn playlist_contents(
    mut fetch: impl FnMut(&str) -> FetchResult<serde_json::Value>,
    id: &str,
) -> FetchResult<Vec<serde_json::Value>> {
    let path = format!("{API}/playlists/{id}?fields=snapshot_id");
    let before = fetch(&path)?["snapshot_id"]
        .as_str()
        .ok_or(ApiError::InvalidResponse)?
        .to_owned();
    let values = myx::webapi::catalog::collect_pages(
        &mut fetch,
        &format!("{API}/playlists/{id}/items?limit=50"),
        None,
    )?;
    let after = fetch(&path)?;
    if after["snapshot_id"].as_str() != Some(before.as_str()) {
        return Err(ApiError::CollectionChanged);
    }
    Ok(values)
}

pub(crate) fn artist_top_tracks(
    mut fetch: impl FnMut(&str) -> FetchResult<serde_json::Value>,
    id: &str,
) -> FetchResult<Vec<LibItem>> {
    let value = fetch(&format!("{API}/artists/{id}/top-tracks"))?;
    Ok(value["tracks"]
        .as_array()
        .ok_or(ApiError::InvalidResponse)?
        .iter()
        .filter_map(track_row)
        .collect())
}

pub(crate) fn artist_albums(
    fetch: impl FnMut(&str) -> FetchResult<serde_json::Value>,
    id: &str,
) -> FetchResult<Vec<LibItem>> {
    let mut albums = myx::webapi::catalog::collect_pages(
        fetch,
        &format!("{API}/artists/{id}/albums?include_groups=album,single&limit=10"),
        None,
    )?;
    // Distinct editions are distinct catalog entries. Never deduplicate by display name.
    albums.sort_by(|left, right| {
        right["release_date"]
            .as_str()
            .cmp(&left["release_date"].as_str())
    });
    Ok(albums
        .iter()
        .filter_map(|value| {
            let mut row = context_row(value)?;
            row.subtitle = value["release_date"].as_str().unwrap_or("").into();
            Some(row)
        })
        .collect())
}

pub(crate) fn fetch_detail_blocking(
    connection: &Connection,
    uri: &str,
    name: &str,
) -> FetchResult<DetailResult> {
    fetch_detail_with(|path| get_json(connection, path), uri, name)
}

pub(crate) fn fetch_detail_with(
    mut fetch: impl FnMut(&str) -> FetchResult<serde_json::Value>,
    uri: &str,
    name: &str,
) -> FetchResult<DetailResult> {
    let mut parts = uri.split(':');
    if parts.next() != Some("spotify") {
        return Err(ApiError::InvalidRequest);
    }
    let kind = parts.next().ok_or(ApiError::InvalidRequest)?;
    let id = parts.next().ok_or(ApiError::InvalidRequest)?;
    let mut items = vec![LibItem::play(format!("▶︎ Play {name}"), uri.into())];
    let mut notice = None;
    match kind {
        "artist" => {
            match artist_top_tracks(&mut fetch, id) {
                Ok(tracks) => {
                    if !tracks.is_empty() {
                        items.push(LibItem::header("Popular"));
                        items.extend(tracks);
                    }
                }
                Err(error @ (ApiError::Restricted | ApiError::NotFound)) => {
                    notice = Some(format!("Popular unavailable: {error}"));
                    items.push(LibItem::header("Popular unavailable for this account"));
                }
                Err(error) => return Err(error),
            }
            let albums = artist_albums(&mut fetch, id)?;
            if !albums.is_empty() {
                items.push(LibItem::header("Albums"));
                items.extend(albums);
            }
        }
        "album" => items.extend(
            myx::webapi::catalog::collect_pages(
                &mut fetch,
                &format!("{API}/albums/{id}/tracks?limit=50"),
                None,
            )?
            .iter()
            .enumerate()
            .filter_map(|(position, track)| Some(track_row(track)?.at_source_position(position))),
        ),
        "playlist" => items.extend(
            playlist_contents(&mut fetch, id)?
                .iter()
                .enumerate()
                .filter_map(|(position, entry)| {
                    Some(parse_playlist_track(entry)?.at_source_position(position))
                }),
        ),
        _ => return Err(ApiError::InvalidRequest),
    }
    if items.len() == 1 {
        items.push(LibItem::header("This collection is empty"));
    }
    Ok(DetailResult {
        uri: uri.into(),
        title: name.into(),
        items,
        notice,
    })
}
