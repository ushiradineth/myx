//! Demand-driven collections and first-page Home previews. Failures never publish partial rows.
use super::*;
use crate::*;

pub(crate) struct LibraryReply {
    pub(crate) section: Section,
    pub(crate) reply: Reply<Vec<LibItem>>,
}

pub(crate) fn spawn_library_fetch(
    webapi: Arc<Mutex<WebApi>>,
    section: Section,
    id: u64,
    tx: flume::Sender<LibraryReply>,
) {
    tokio::task::spawn_blocking(move || {
        let result =
            connection_of(&webapi).and_then(|connection| fetch_section(&connection, section));
        let _ = tx.send(LibraryReply {
            section,
            reply: Reply { id, result },
        });
    });
}

pub(crate) fn track_row(track: &serde_json::Value) -> Option<LibItem> {
    Some(LibItem::track(
        track["name"].as_str()?.into(),
        track["artists"]
            .as_array()
            .map(|artists| {
                artists
                    .iter()
                    .filter_map(|artist| artist["name"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .or_else(|| track["show"]["publisher"].as_str().map(String::from))
            .unwrap_or_default(),
        track["uri"].as_str()?.into(),
    ))
}

pub(crate) fn context_row(value: &serde_json::Value) -> Option<LibItem> {
    Some(LibItem::ctx(
        value["name"].as_str()?.into(),
        value["artists"][0]["name"].as_str().unwrap_or("").into(),
        value["uri"].as_str()?.into(),
    ))
}

pub(crate) fn fetch_section(
    connection: &Connection,
    section: Section,
) -> FetchResult<Vec<LibItem>> {
    fetch_section_with(|path| get_json(connection, path), section)
}

fn home_group(
    page: serde_json::Value,
    title: &str,
    parse_row: impl FnMut(&serde_json::Value) -> Option<LibItem>,
) -> FetchResult<Vec<LibItem>> {
    let items = page["items"].as_array().ok_or(ApiError::InvalidResponse)?;
    match page.get("next") {
        Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::String(_)) => true,
        _ => return Err(ApiError::InvalidResponse),
    };
    let group: Vec<_> = items.iter().filter_map(parse_row).collect();
    if group.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows = vec![LibItem::header(title)];
    rows.extend(group);
    Ok(rows)
}

pub(crate) fn fetch_section_with(
    mut fetch: impl FnMut(&str) -> FetchResult<serde_json::Value>,
    section: Section,
) -> FetchResult<Vec<LibItem>> {
    let mut collect = |path: &str, container: Option<&str>| {
        myx::webapi::catalog::collect_pages(&mut fetch, path, container)
    };
    let rows = match section {
        Section::Home => {
            let mut rows = home_group(
                fetch(&format!("{API}/me/player/recently-played?limit=50"))?,
                "Recently Played",
                |entry| track_row(&entry["track"]),
            )?;
            for (title, kind) in [
                ("Your Top Tracks", "tracks"),
                ("Your Top Artists", "artists"),
            ] {
                rows.extend(home_group(
                    fetch(&format!("{API}/me/top/{kind}?limit=50"))?,
                    title,
                    if kind == "tracks" {
                        track_row
                    } else {
                        context_row
                    },
                )?);
            }
            rows
        }
        Section::Recent => collect(&format!("{API}/me/player/recently-played?limit=50"), None)?
            .iter()
            .filter_map(|entry| track_row(&entry["track"]))
            .collect(),
        Section::Playlists => collect(&format!("{API}/me/playlists?limit=50"), None)?
            .iter()
            .filter_map(|value| {
                Some(LibItem::ctx(
                    value["name"].as_str()?.into(),
                    playlist_subtitle(
                        value["owner"]["display_name"].as_str().unwrap_or(""),
                        playlist_total(value),
                    ),
                    value["uri"].as_str()?.into(),
                ))
            })
            .collect(),
        Section::Albums => collect(&format!("{API}/me/albums?limit=50"), None)?
            .iter()
            .filter_map(|entry| context_row(&entry["album"]))
            .collect(),
        Section::Artists => collect(
            &format!("{API}/me/following?type=artist&limit=50"),
            Some("artists"),
        )?
        .iter()
        .filter_map(context_row)
        .collect(),
        Section::Liked => {
            let tracks: Vec<_> = collect(&format!("{API}/me/tracks?limit=50"), None)?
                .iter()
                .enumerate()
                .filter_map(|(position, entry)| {
                    let track = myx::player::intents::liked_track(entry)?;
                    Some(track_row(track)?.at_source_position(position))
                })
                .collect();
            if tracks.is_empty() {
                Vec::new()
            } else {
                let mut rows = vec![
                    LibItem::play("▶︎  Play Liked Songs".into(), "myx:action:liked-play".into()),
                    LibItem::header("Songs"),
                ];
                rows.extend(tracks);
                rows
            }
        }
    };
    Ok(rows)
}

pub(crate) fn parse_playlist_track(entry: &serde_json::Value) -> Option<LibItem> {
    track_row(if entry["item"].is_object() {
        &entry["item"]
    } else {
        &entry["track"]
    })
}

pub(crate) fn playlist_total(value: &serde_json::Value) -> Option<u64> {
    value["items"]["total"]
        .as_u64()
        .or_else(|| value["tracks"]["total"].as_u64())
}

pub(crate) fn playlist_subtitle(owner: &str, total: Option<u64>) -> String {
    match total {
        Some(count) if owner.is_empty() => count.to_string(),
        Some(count) => format!("{count} · {owner}"),
        None => owner.into(),
    }
}
