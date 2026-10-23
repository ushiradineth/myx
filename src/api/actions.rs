//! Library writes and the action menu that drives them.
//!
//! The rest of `api/` reads; this file is the half that writes — saving and
//! unsaving tracks, albums and artists, following playlists, and queue and
//! playlist adds. `build_action_menu` is the exception: it builds the menu
//! model describing those writes rather than performing one.

use super::*;
use crate::*;

pub(crate) fn spawn_action_menu(
    webapi: Arc<Mutex<WebApi>>,
    item: LibItem,
    tx: flume::Sender<ActionMenu>,
) {
    tokio::task::spawn_blocking(move || {
        if !webapi.lock().map(|w| w.is_available()).unwrap_or(false) {
            return;
        }
        if let Some(token) = connection_of(&webapi) {
            let _ = tx.send(build_action_menu(Some(&token), &item));
        }
    });
}

/// Build the context menu for `item`, checking saved/following state and
/// resolving related artist/album links up front.
pub(crate) fn build_action_menu(token: Option<&Connection>, item: &LibItem) -> ActionMenu {
    let mut parts = item.uri.split(':');
    parts.next();
    let kind = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("").to_string();
    let uri = item.uri.clone();
    // Only build the blocking client for the enriched connection path; the
    // instant (None) path runs on the async loop where dropping reqwest's inner
    // runtime would panic.
    let client = token.map(|_| http_client());
    let mut items = Vec::new();

    match kind {
        "track" => {
            let saved = token
                .map(|t| api_contains(t, &format!("spotify:track:{id}")))
                .unwrap_or(false);
            items.push(ActionItem {
                label: if saved {
                    "♥  Remove from Liked".into()
                } else {
                    "♡  Add to Liked".into()
                },
                kind: ActionKind::ToggleLike {
                    id: id.clone(),
                    saved,
                },
            });
            items.push(ActionItem {
                label: "＋  Add to Queue".into(),
                kind: ActionKind::Queue { uri: uri.clone() },
            });
            items.push(ActionItem {
                label: "≡  Add to Playlist…".into(),
                kind: ActionKind::AddToPlaylistMenu {
                    track_uri: uri.clone(),
                },
            });
            // Resolve the track's artist + album for "Go to" navigation.
            if let Some(v) = client
                .as_ref()
                .zip(token)
                .and_then(|(c, t)| get_json_cached(c, &format!("{API}/tracks/{id}"), t))
            {
                if let (Some(au), Some(an)) = (
                    v["artists"][0]["uri"].as_str(),
                    v["artists"][0]["name"].as_str(),
                ) {
                    items.push(ActionItem {
                        label: format!("→  Go to Artist ({an})"),
                        kind: ActionKind::Open {
                            uri: au.to_string(),
                            name: an.to_string(),
                        },
                    });
                }
                if let (Some(lu), Some(ln)) =
                    (v["album"]["uri"].as_str(), v["album"]["name"].as_str())
                {
                    items.push(ActionItem {
                        label: "→  Go to Album".into(),
                        kind: ActionKind::Open {
                            uri: lu.to_string(),
                            name: ln.to_string(),
                        },
                    });
                }
            }
            items.push(ActionItem {
                label: "⧉  Copy Link".into(),
                kind: ActionKind::CopyLink { uri },
            });
        }
        "artist" => {
            let following = token
                .map(|t| api_contains(t, &format!("spotify:artist:{id}")))
                .unwrap_or(false);
            items.push(ActionItem {
                label: if following {
                    "Unfollow".into()
                } else {
                    "Follow".into()
                },
                kind: ActionKind::ToggleFollowArtist { id, following },
            });
            items.push(ActionItem {
                label: "▶︎  Play".into(),
                kind: ActionKind::Play {
                    uri: uri.clone(),
                    name: item.name.clone(),
                },
            });
            items.push(ActionItem {
                label: "→  Open".into(),
                kind: ActionKind::Open {
                    uri: uri.clone(),
                    name: item.name.clone(),
                },
            });
            items.push(ActionItem {
                label: "⧉  Copy Link".into(),
                kind: ActionKind::CopyLink { uri },
            });
        }
        "album" => {
            let saved = token
                .map(|t| api_contains(t, &format!("spotify:album:{id}")))
                .unwrap_or(false);
            items.push(ActionItem {
                label: if saved {
                    "Remove from Library".into()
                } else {
                    "Save Album".into()
                },
                kind: ActionKind::ToggleSaveAlbum {
                    id: id.clone(),
                    saved,
                },
            });
            items.push(ActionItem {
                label: "▶︎  Play".into(),
                kind: ActionKind::Play {
                    uri: uri.clone(),
                    name: item.name.clone(),
                },
            });
            items.push(ActionItem {
                label: "→  Open Album".into(),
                kind: ActionKind::Open {
                    uri: uri.clone(),
                    name: item.name.clone(),
                },
            });
            if let Some(v) = client
                .as_ref()
                .zip(token)
                .and_then(|(c, t)| get_json_cached(c, &format!("{API}/albums/{id}"), t))
            {
                if let (Some(au), Some(an)) = (
                    v["artists"][0]["uri"].as_str(),
                    v["artists"][0]["name"].as_str(),
                ) {
                    items.push(ActionItem {
                        label: format!("→  Go to Artist ({an})"),
                        kind: ActionKind::Open {
                            uri: au.to_string(),
                            name: an.to_string(),
                        },
                    });
                }
            }
            items.push(ActionItem {
                label: "⧉  Copy Link".into(),
                kind: ActionKind::CopyLink { uri },
            });
        }
        "playlist" => {
            items.push(ActionItem {
                label: "＋  Add to Your Library".into(),
                kind: ActionKind::FollowPlaylist { id },
            });
            items.push(ActionItem {
                label: "▶︎  Play".into(),
                kind: ActionKind::Play {
                    uri: uri.clone(),
                    name: item.name.clone(),
                },
            });
            items.push(ActionItem {
                label: "→  Open".into(),
                kind: ActionKind::Open {
                    uri: uri.clone(),
                    name: item.name.clone(),
                },
            });
            items.push(ActionItem {
                label: "⧉  Copy Link".into(),
                kind: ActionKind::CopyLink { uri },
            });
        }
        _ => {}
    }
    ActionMenu {
        title: item.name.clone(),
        items,
        selected: 0,
    }
}

pub(crate) fn spawn_action(
    engine: &Player,
    webapi: Arc<Mutex<WebApi>>,
    kind: ActionKind,
    tx: flume::Sender<String>,
) {
    if let ActionKind::Queue { uri } = kind {
        // Queue intent goes to the daemon's controller, not an account-global
        // player endpoint selected by a client.
        let msg = match engine.add_to_queue(uri) {
            Ok(()) => match engine.request_queue() {
                Ok(()) => "added to queue".to_string(),
                Err(e) => format!("added to queue; refresh failed: {e}"),
            },
            Err(e) => format!("queue failed: {e}"),
        };
        let _ = tx.send(msg);
        return;
    }
    tokio::task::spawn_blocking(move || {
        if !webapi.lock().map(|w| w.is_available()).unwrap_or(false) {
            let _ = tx.send(
                "Spotify Web API unavailable; this library action requires authorization"
                    .to_string(),
            );
            return;
        }
        let msg = match connection_of(&webapi) {
            Some(t) => run_action(&t, kind),
            None => "not authorized".to_string(),
        };
        let _ = tx.send(msg);
    });
}

pub(crate) fn run_action(token: &Connection, kind: ActionKind) -> String {
    let client = http_client();
    match kind {
        ActionKind::ToggleLike { id, saved } => {
            match library_write(&client, token, saved, &format!("spotify:track:{id}")) {
                Ok(()) => {
                    if saved {
                        "removed from Liked".into()
                    } else {
                        "added to Liked \u{2665} (press r to refresh)".into()
                    }
                }
                Err(e) => format!("like failed: {e}"),
            }
        }
        ActionKind::Queue { uri } => {
            match api_modify(
                &client,
                token,
                "POST",
                &format!("{API}/me/player/queue?uri={}", urlencode(&uri)),
            ) {
                Ok(()) => "added to queue".into(),
                Err(e) => format!("queue failed: {e} (start playback first)"),
            }
        }
        ActionKind::AddToPlaylist {
            playlist_id,
            track_uri,
        } => {
            match api_modify(
                &client,
                token,
                "POST",
                &format!(
                    "{API}/playlists/{playlist_id}/items?uris={}",
                    urlencode(&track_uri)
                ),
            ) {
                Ok(()) => "added to playlist".into(),
                Err(e) => format!("add failed: {e}"),
            }
        }
        ActionKind::ToggleFollowArtist { id, following } => {
            match library_write(&client, token, following, &format!("spotify:artist:{id}")) {
                Ok(()) => {
                    if following {
                        "unfollowed".into()
                    } else {
                        "following".into()
                    }
                }
                Err(e) => format!("follow failed: {e}"),
            }
        }
        ActionKind::ToggleSaveAlbum { id, saved } => {
            match library_write(&client, token, saved, &format!("spotify:album:{id}")) {
                Ok(()) => {
                    if saved {
                        "removed album".into()
                    } else {
                        "saved album".into()
                    }
                }
                Err(e) => format!("album action failed: {e}"),
            }
        }
        ActionKind::FollowPlaylist { id } => {
            match library_write(&client, token, false, &format!("spotify:playlist:{id}")) {
                Ok(()) => "added to library".into(),
                Err(e) => format!("add failed: {e}"),
            }
        }
        _ => String::new(),
    }
}

/// Add or remove one library item — track, album, artist or playlist.
///
/// One endpoint for all four since February 2026; the per-type ones it replaced
/// are gone and answer 403. `uris` must go in the query string: a JSON body is
/// rejected as missing it.
pub(crate) fn library_write(
    client: &reqwest::blocking::Client,
    token: &Connection,
    remove: bool,
    uri: &str,
) -> Result<(), String> {
    api_modify(
        client,
        token,
        if remove { "DELETE" } else { "PUT" },
        &format!("{API}/me/library?uris={}", urlencode(uri)),
    )
}

/// The daemon owns authorization, mutation routing and retry policy.
pub(crate) fn api_modify(
    _client: &reqwest::blocking::Client,
    token: &Connection,
    method: &str,
    url: &str,
) -> Result<(), String> {
    let method = match method {
        "PUT" => myx::webapi::HttpMethod::Put,
        "DELETE" => myx::webapi::HttpMethod::Delete,
        "POST" => myx::webapi::HttpMethod::Post,
        _ => return Err("unsupported catalog action".into()),
    };
    token
        .catalog(method, catalog_path(url), None)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Is `uri` in the user's library? Replaced the `/me/*/contains` family in
/// February 2026; those are gone and answer 403.
pub(crate) fn api_contains(token: &Connection, uri: &str) -> bool {
    let client = http_client();
    let url = format!("{API}/me/library/contains?uris={}", urlencode(uri));
    get_json(&client, &url, token)
        .and_then(|v| v.get(0).and_then(|b| b.as_bool()))
        .unwrap_or(false)
}
