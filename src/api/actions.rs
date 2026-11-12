//! Explicit library intent with observable reconciliation, never a blind mutation retry.
use super::*;
use crate::*;

pub(crate) fn spawn_action_menu(
    webapi: Arc<Mutex<WebApi>>,
    item: LibItem,
    id: u64,
    tx: flume::Sender<Reply<ActionMenu>>,
) {
    tokio::task::spawn_blocking(move || {
        let result = connection_of(&webapi)
            .and_then(|connection| build_action_menu(Some(&connection), &item));
        let _ = tx.send(Reply { id, result });
    });
}

pub(crate) fn spawn_playlist_menu(
    webapi: Arc<Mutex<WebApi>>,
    track_uri: String,
    id: u64,
    tx: flume::Sender<Reply<ActionMenu>>,
) {
    tokio::task::spawn_blocking(move || {
        let result = connection_of(&webapi).and_then(|connection| {
            let playlists = fetch_section(&connection, Section::Playlists)?;
            Ok(ActionMenu {
                title: "Add to playlist".into(),
                selected: 0,
                items: playlists
                    .into_iter()
                    .filter_map(|playlist| {
                        let playlist_id = playlist.uri.rsplit(':').next()?.to_string();
                        Some(ActionItem {
                            label: playlist.name,
                            kind: ActionKind::AddToPlaylist {
                                playlist_id,
                                track_uri: track_uri.clone(),
                            },
                        })
                    })
                    .collect(),
            })
        });
        let _ = tx.send(Reply { id, result });
    });
}

/// The immediate menu contains only state-independent actions. A failed contains
/// read never turns unknown membership into an invitation to save/remove.
pub(crate) fn build_action_menu(
    connection: Option<&Connection>,
    item: &LibItem,
) -> FetchResult<ActionMenu> {
    let kind = item.uri.split(':').nth(1).ok_or(ApiError::InvalidRequest)?;
    let id = item
        .uri
        .rsplit(':')
        .next()
        .ok_or(ApiError::InvalidRequest)?
        .to_owned();
    let mut items = Vec::new();
    if let Some(connection) = connection {
        let saved = api_contains(connection, &item.uri)?;
        let (label, action) = match kind {
            "track" => (
                if saved {
                    "Remove from Liked"
                } else {
                    "♥ Add to Liked"
                },
                ActionKind::ToggleLike {
                    id: id.clone(),
                    saved,
                },
            ),
            "album" => (
                if saved {
                    "Remove from Library"
                } else {
                    "Save Album"
                },
                ActionKind::ToggleSaveAlbum {
                    id: id.clone(),
                    saved,
                },
            ),
            "artist" => (
                if saved { "Unfollow" } else { "Follow" },
                ActionKind::ToggleFollowArtist {
                    id: id.clone(),
                    following: saved,
                },
            ),
            "playlist" => (
                if saved {
                    "Remove from Library"
                } else {
                    "Add to Your Library"
                },
                ActionKind::FollowPlaylist {
                    id: id.clone(),
                    following: saved,
                },
            ),
            _ => return Err(ApiError::InvalidRequest),
        };
        items.push(ActionItem {
            label: label.into(),
            kind: action,
        });
    }
    if kind == "track" {
        items.push(ActionItem {
            label: "＋  Add to Queue".into(),
            kind: ActionKind::Queue {
                uri: item.uri.clone(),
            },
        });
        items.push(ActionItem {
            label: "≡  Add to Playlist…".into(),
            kind: ActionKind::AddToPlaylistMenu {
                track_uri: item.uri.clone(),
            },
        });
    } else {
        items.push(ActionItem {
            label: "▶︎  Play".into(),
            kind: ActionKind::Play {
                uri: item.uri.clone(),
                name: item.name.clone(),
            },
        });
        items.push(ActionItem {
            label: "→  Open".into(),
            kind: ActionKind::Open {
                uri: item.uri.clone(),
                name: item.name.clone(),
            },
        });
    }
    if let Some(connection) = connection.filter(|_| matches!(kind, "track" | "album")) {
        let value = get_json(connection, &format!("{API}/{kind}s/{id}"))?;
        for (label, target) in [
            ("Go to Artist", &value["artists"][0]),
            ("Go to Album", &value["album"]),
        ] {
            if let (Some(uri), Some(name)) = (target["uri"].as_str(), target["name"].as_str()) {
                items.push(ActionItem {
                    label: format!("→  {label} ({name})"),
                    kind: ActionKind::Open {
                        uri: uri.into(),
                        name: name.into(),
                    },
                });
            }
        }
    }
    items.push(ActionItem {
        label: "⧉  Copy Link".into(),
        kind: ActionKind::CopyLink {
            uri: item.uri.clone(),
        },
    });
    Ok(ActionMenu {
        title: item.name.clone(),
        items,
        selected: 0,
    })
}

pub(crate) struct ActionOutcome {
    pub(crate) message: &'static str,
    pub(crate) refresh_library: bool,
}

pub(crate) fn spawn_action(
    engine: &Player,
    webapi: Arc<Mutex<WebApi>>,
    kind: ActionKind,
    id: u64,
    tx: flume::Sender<Reply<ActionOutcome>>,
) {
    if let ActionKind::Queue { uri } = kind {
        let result = engine.add_to_queue(uri).map_err(write_error).map(|()| {
            let _ = engine.request_queue();
            ActionOutcome {
                message: "queue request accepted",
                refresh_library: false,
            }
        });
        let _ = tx.send(Reply { id, result });
        return;
    }
    tokio::task::spawn_blocking(move || {
        let result = connection_of(&webapi).and_then(|connection| run_action(&connection, kind));
        let _ = tx.send(Reply { id, result });
    });
}

/// Read after either success or a transport-ambiguous write. Exactly one write
/// attempt; a matching desired state is evidence, a timeout is never evidence of absence.
pub(crate) fn reconcile_membership(
    write: impl FnOnce() -> FetchResult<serde_json::Value>,
    read: impl FnOnce() -> FetchResult<bool>,
    desired: bool,
) -> FetchResult<()> {
    match write() {
        Ok(_) | Err(ApiError::AmbiguousWrite) => match read() {
            Ok(observed) if observed == desired => Ok(()),
            _ => Err(ApiError::AmbiguousWrite),
        },
        Err(error) => Err(error),
    }
}

pub(crate) fn run_action(connection: &Connection, kind: ActionKind) -> FetchResult<ActionOutcome> {
    let (uri, remove, message) = match kind {
        ActionKind::ToggleLike { id, saved } => (
            format!("spotify:track:{id}"),
            saved,
            if saved {
                "removed from Liked, confirmed"
            } else {
                "added to Liked, confirmed"
            },
        ),
        ActionKind::ToggleSaveAlbum { id, saved } => (
            format!("spotify:album:{id}"),
            saved,
            if saved {
                "removed album, confirmed"
            } else {
                "saved album, confirmed"
            },
        ),
        ActionKind::ToggleFollowArtist { id, following } => (
            format!("spotify:artist:{id}"),
            following,
            if following {
                "unfollowed artist, confirmed"
            } else {
                "followed artist, confirmed"
            },
        ),
        ActionKind::FollowPlaylist { id, following } => (
            format!("spotify:playlist:{id}"),
            following,
            if following {
                "removed playlist, confirmed"
            } else {
                "saved playlist, confirmed"
            },
        ),
        ActionKind::AddToPlaylist {
            playlist_id,
            track_uri,
        } => {
            let result = connection
                .catalog(
                    HttpMethod::Post,
                    &format!(
                        "{API}/playlists/{playlist_id}/items?uris={}",
                        urlencode(&track_uri)
                    ),
                    None,
                )
                .map_err(write_error);
            // Spotify's returned revision acknowledges this server mutation; do
            // not infer success from a pre-existing duplicate track in a playlist.
            return confirm_playlist_write(result);
        }
        _ => return Err(ApiError::InvalidRequest),
    };
    reconcile_membership(
        || {
            connection
                .catalog(
                    if remove {
                        HttpMethod::Delete
                    } else {
                        HttpMethod::Put
                    },
                    &format!("{API}/me/library?uris={}", urlencode(&uri)),
                    None,
                )
                .map_err(write_error)
        },
        || api_contains(connection, &uri),
        !remove,
    )?;
    Ok(ActionOutcome {
        message,
        refresh_library: true,
    })
}

pub(crate) fn confirm_playlist_write(
    result: FetchResult<serde_json::Value>,
) -> FetchResult<ActionOutcome> {
    let value = result?;
    if value["snapshot_id"].as_str().is_none_or(str::is_empty) {
        return Err(ApiError::AmbiguousWrite);
    }
    Ok(ActionOutcome {
        message: "added to playlist, server revision confirmed",
        refresh_library: true,
    })
}

pub(crate) fn api_contains(connection: &Connection, uri: &str) -> FetchResult<bool> {
    parse_contains(&get_json(
        connection,
        &format!("{API}/me/library/contains?uris={}", urlencode(uri)),
    )?)
}

pub(crate) fn parse_contains(value: &serde_json::Value) -> FetchResult<bool> {
    value
        .get(0)
        .and_then(serde_json::Value::as_bool)
        .ok_or(ApiError::InvalidResponse)
}
