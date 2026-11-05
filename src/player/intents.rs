//! Pure load planning. Plans describe writes, never observations or SDK queue emulation.
use serde_json::{json, Value};

use crate::domain::PlayerCommand;
use crate::webapi::ApiError;

/// This is a wire integer bound, not a source-list/window limit.
pub const MAX_ROW_POSITION: u32 = i32::MAX as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qualification {
    DocumentedContext,
    /// Complete URI array starting at its head, without an undocumented offset.
    DocumentedExplicitArray,
    /// PUT play documents track URI arrays, but documents offsets only for contexts.
    /// Full-array + offset requires live qualification, including previous/shuffle/repeat-all.
    ExplicitArrayOffsetRequiresLive,
}

#[derive(Debug, Clone)]
pub enum LoadPlan {
    /// Use the supported SDK playable-URI/position path (tracks AND episodes).
    SdkPlayable {
        uri: String,
        position_ms: u32,
        shuffle: bool,
    },
    /// Owner must verify its device and PUT this body to /v1/me/player/play.
    /// Apply native shuffle/repeat options to the complete supplied source. Acceptance
    /// of this request is not evidence of observed playback or native qualification.
    WebApi {
        body: Value,
        /// Authoritative selected row, kept outside the provider request body.
        expected_uri: Option<String>,
        shuffle: bool,
        qualification: Qualification,
    },
    /// Fetch all original collection rows, then call resolve_context. Exact rows
    /// must still match the selection and be playable; URI-only selections must
    /// be unique. Never silently choose another row or a first duplicate.
    ResolveContext { command: PlayerCommand },
    /// Fetch ALL /me/tracks pages with catalog::collect_pages, then resolve_liked.
    ResolveLiked { command: PlayerCommand },
}

/// Only Spotify catalog identifiers are admitted, never local/action/header URIs.
pub fn valid_uri(uri: &str, kinds: &[&str]) -> bool {
    let Some(rest) = uri.strip_prefix("spotify:") else {
        return false;
    };
    let Some((kind, id)) = rest.split_once(':') else {
        return false;
    };
    kinds.contains(&kind) && id.len() == 22 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn row(position: u32) -> Result<(), ApiError> {
    if position <= MAX_ROW_POSITION {
        Ok(())
    } else {
        Err(ApiError::InvalidRequest)
    }
}

fn context_body(
    uri: &str,
    offset: Option<Value>,
    position_ms: u32,
    shuffle: bool,
    expected_uri: Option<String>,
) -> LoadPlan {
    let mut body = json!({"context_uri": uri, "position_ms": position_ms});
    if let Some(offset) = offset {
        body["offset"] = offset;
    }
    LoadPlan::WebApi {
        body,
        expected_uri,
        shuffle,
        qualification: Qualification::DocumentedContext,
    }
}

fn explicit(
    uris: &[String],
    index: u32,
    position_ms: u32,
    shuffle: bool,
) -> Result<LoadPlan, ApiError> {
    row(index)?;
    if uris.is_empty()
        || index as usize >= uris.len()
        || uris.iter().any(|uri| !valid_uri(uri, &["track"]))
    {
        return Err(ApiError::InvalidRequest);
    }
    // No rotation, truncation, deduplication, synthetic queue, or client shuffle.
    let mut body = json!({"uris": uris, "position_ms": position_ms});
    let qualification = if index == 0 {
        Qualification::DocumentedExplicitArray
    } else {
        body["offset"] = json!({"position": index});
        Qualification::ExplicitArrayOffsetRequiresLive
    };
    Ok(LoadPlan::WebApi {
        body,
        expected_uri: Some(uris[index as usize].clone()),
        shuffle,
        qualification,
    })
}

fn unique_index(uris: &[String], selected: &str) -> Result<u32, ApiError> {
    let mut matches = uris
        .iter()
        .enumerate()
        .filter(|(_, uri)| uri.as_str() == selected);
    let (index, _) = matches.next().ok_or(ApiError::CollectionChanged)?;
    if matches.next().is_some() {
        return Err(ApiError::InvalidRequest);
    }
    u32::try_from(index).map_err(|_| ApiError::InvalidRequest)
}

pub fn plan(command: &PlayerCommand) -> Result<Option<LoadPlan>, ApiError> {
    let load = match command {
        PlayerCommand::PlayContext {
            uri,
            track,
            position_ms,
            shuffle,
        } => {
            if valid_uri(uri, &["track", "episode"]) {
                if track.as_ref().is_some_and(|track| track != uri) {
                    return Err(ApiError::InvalidRequest);
                }
                LoadPlan::SdkPlayable {
                    uri: uri.clone(),
                    position_ms: *position_ms,
                    shuffle: *shuffle,
                }
            } else {
                if !valid_uri(uri, &["album", "playlist", "artist"]) {
                    return Err(ApiError::InvalidRequest);
                }
                if let Some(track) = track {
                    if !valid_uri(uri, &["album", "playlist"])
                        || !valid_uri(track, &["track", "episode"])
                    {
                        return Err(ApiError::InvalidRequest);
                    }
                    LoadPlan::ResolveContext {
                        command: command.clone(),
                    }
                } else {
                    context_body(uri, None, *position_ms, *shuffle, None)
                }
            }
        }
        PlayerCommand::PlayContextAt {
            uri,
            track,
            row_position,
            ..
        } => {
            if !valid_uri(uri, &["album", "playlist"])
                || track
                    .as_ref()
                    .is_some_and(|uri| !valid_uri(uri, &["track", "episode"]))
            {
                return Err(ApiError::InvalidRequest);
            }
            row(*row_position)?;
            LoadPlan::ResolveContext {
                command: command.clone(),
            }
        }
        PlayerCommand::PlayTracks {
            uris,
            start,
            position_ms,
            shuffle,
        } => {
            let index = match start {
                Some(start) => unique_index(uris, start)?,
                None => 0,
            };
            explicit(uris, index, *position_ms, *shuffle)?
        }
        PlayerCommand::PlayTracksAt {
            uris,
            start_index,
            position_ms,
            shuffle,
        } => explicit(uris, *start_index, *position_ms, *shuffle)?,
        PlayerCommand::PlayLiked {
            start,
            source_position,
            ..
        } => {
            if start
                .as_ref()
                .is_some_and(|uri| !valid_uri(uri, &["track"]))
                || (source_position.is_some() && start.is_none())
            {
                return Err(ApiError::InvalidRequest);
            }
            if let Some(position) = source_position {
                row(*position)?;
            }
            LoadPlan::ResolveLiked {
                command: command.clone(),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(load))
}

/// Shared availability policy for saved-track UI rows and authoritative owner resolution.
pub fn liked_track(entry: &Value) -> Option<&Value> {
    let track = entry.get("track")?;
    playable(entry, track, &["track"]).then_some(track)
}

fn playable(entry: &Value, item: &Value, kinds: &[&str]) -> bool {
    entry["is_local"].as_bool() != Some(true)
        && item["is_local"].as_bool() != Some(true)
        && entry["is_playable"].as_bool() != Some(false)
        && item["is_playable"].as_bool() != Some(false)
        && !entry["restrictions"]
            .as_object()
            .is_some_and(|value| !value.is_empty())
        && !item["restrictions"]
            .as_object()
            .is_some_and(|value| !value.is_empty())
        && item["uri"]
            .as_str()
            .is_some_and(|uri| valid_uri(uri, kinds))
}

fn context_item(entry: &Value) -> &Value {
    // A present null item consumes its original position; never substitute a
    // legacy wrapper's track when the authoritative item field is null.
    entry
        .get("item")
        .or_else(|| entry.get("track"))
        .unwrap_or(entry)
}

/// Resolve original /me/tracks row coordinates to the complete playable source.
/// Null/unavailable rows consume source positions but never become fake playable URIs.
pub fn resolve_liked(command: &PlayerCommand, rows: &[Value]) -> Result<LoadPlan, ApiError> {
    let PlayerCommand::PlayLiked {
        start,
        source_position,
        position_ms,
        shuffle,
    } = command
    else {
        return Err(ApiError::InvalidRequest);
    };
    plan(command)?;
    let mut uris = Vec::new();
    let mut selected = None;
    for (source, entry) in rows.iter().enumerate() {
        if let Some(track) = liked_track(entry) {
            let uri = track["uri"].as_str().ok_or(ApiError::InvalidResponse)?;
            if source_position.map(|p| p as usize) == Some(source) {
                if start.as_deref() != Some(uri) {
                    return Err(ApiError::CollectionChanged);
                }
                selected = Some(u32::try_from(uris.len()).map_err(|_| ApiError::InvalidRequest)?);
            }
            uris.push(uri.to_owned());
        }
    }
    let index = match (source_position, start) {
        (Some(_), _) => selected.ok_or(ApiError::CollectionChanged)?,
        (None, Some(uri)) => unique_index(&uris, uri)?,
        (None, None) => 0,
    };
    explicit(&uris, index, *position_ms, *shuffle)
}

/// `rows` must be a fresh, full, unfiltered album/playlist source. Original null,
/// local and unavailable rows must retain their coordinates. Playlist revision
/// checks belong to the owner fetching these rows, outside this pure helper.
pub fn resolve_context(command: &PlayerCommand, rows: &[Value]) -> Result<LoadPlan, ApiError> {
    plan(command)?;
    match command {
        PlayerCommand::PlayContextAt {
            uri,
            track,
            row_position,
            position_ms,
            shuffle,
        } => {
            let entry = rows
                .get(*row_position as usize)
                .ok_or(ApiError::CollectionChanged)?;
            let item = context_item(entry);
            if !playable(entry, item, &["track", "episode"])
                || track
                    .as_deref()
                    .is_some_and(|expected| item["uri"].as_str() != Some(expected))
            {
                return Err(ApiError::CollectionChanged);
            }
            Ok(context_body(
                uri,
                Some(json!({"position": row_position})),
                *position_ms,
                *shuffle,
                item["uri"].as_str().map(str::to_owned),
            ))
        }
        PlayerCommand::PlayContext {
            uri,
            track: Some(track),
            position_ms,
            shuffle,
        } if valid_uri(uri, &["album", "playlist"]) => {
            let mut matches = rows
                .iter()
                .filter(|entry| context_item(entry)["uri"].as_str() == Some(track.as_str()));
            let entry = matches.next().ok_or(ApiError::CollectionChanged)?;
            if matches.next().is_some() {
                return Err(ApiError::InvalidRequest);
            }
            if !playable(entry, context_item(entry), &["track", "episode"]) {
                return Err(ApiError::CollectionChanged);
            }
            Ok(context_body(
                uri,
                Some(json!({"uri": track})),
                *position_ms,
                *shuffle,
                Some(track.clone()),
            ))
        }
        _ => Err(ApiError::InvalidRequest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "spotify:track:0123456789abcdefghijkl";
    const B: &str = "spotify:track:1123456789abcdefghijkl";
    const Z: &str = "spotify:track:2123456789abcdefghijkl";

    fn body(load: LoadPlan) -> (Value, bool, Qualification) {
        let LoadPlan::WebApi {
            body,
            shuffle,
            qualification,
            ..
        } = load
        else {
            panic!("expected Web API plan");
        };
        (body, shuffle, qualification)
    }

    #[test]
    fn full_5107_source_keeps_prefix_duplicates_and_exact_middle_start() {
        let mut uris: Vec<_> = (0..5107)
            .map(|i| format!("spotify:track:{i:022}"))
            .collect();
        uris[2553] = uris[17].clone();
        let command = PlayerCommand::PlayTracksAt {
            uris: uris.clone(),
            start_index: 2553,
            position_ms: 42_000,
            shuffle: true,
        };
        let (body, shuffle, qualification) = body(plan(&command).unwrap().unwrap());
        assert_eq!(body["uris"], json!(uris));
        assert_eq!(body["uris"][0], format!("spotify:track:{:022}", 0));
        assert_eq!(body["uris"][17], body["uris"][2553]);
        assert_eq!(body["uris"][5106], format!("spotify:track:{:022}", 5106));
        assert_eq!(body["offset"], json!({"position":2553}));
        assert_eq!(body["position_ms"], 42_000);
        assert!(shuffle);
        // A software payload assertion cannot qualify Spotify's native behavior.
        assert_eq!(
            qualification,
            Qualification::ExplicitArrayOffsetRequiresLive
        );
    }

    #[test]
    fn context_exact_original_row_and_unique_uri_fallback() {
        for kind in ["album", "playlist"] {
            let uri = format!("spotify:{kind}:0123456789abcdefghijkl");
            let exact = PlayerCommand::PlayContextAt {
                uri: uri.clone(),
                track: Some(A.into()),
                row_position: 73,
                position_ms: 456,
                shuffle: false,
            };
            assert!(matches!(
                plan(&exact).unwrap(),
                Some(LoadPlan::ResolveContext { .. })
            ));
            let mut source = vec![Value::Null; 74];
            source[0] = json!({"track":{"uri":A}});
            source[73] = json!({"item":{"uri":A}});
            let (value, _, qualification) = body(resolve_context(&exact, &source).unwrap());
            assert_eq!(
                value,
                json!({"context_uri":uri,"offset":{"position":73},"position_ms":456})
            );
            assert_eq!(qualification, Qualification::DocumentedContext);
            let legacy = PlayerCommand::PlayContext {
                uri: uri.clone(),
                track: Some(A.into()),
                position_ms: 9,
                shuffle: true,
            };
            assert!(matches!(
                plan(&legacy).unwrap(),
                Some(LoadPlan::ResolveContext { .. })
            ));
            let rows = [Value::Null, json!({"uri":A})];
            let (value, _, _) = body(resolve_context(&legacy, &rows).unwrap());
            assert_eq!(
                value,
                json!({"context_uri":uri,"offset":{"uri":A},"position_ms":9})
            );
            assert!(matches!(
                resolve_context(&legacy, &[rows[1].clone(), rows[1].clone()]),
                Err(ApiError::InvalidRequest)
            ));
        }
    }

    #[test]
    fn exact_context_resolution_checks_raw_row_without_falling_back_to_a_duplicate() {
        for kind in ["album", "playlist"] {
            let uri = format!("spotify:{kind}:0123456789abcdefghijkl");
            for track in [Some(A.to_owned()), None] {
                let command = PlayerCommand::PlayContextAt {
                    uri: uri.clone(),
                    track: track.clone(),
                    row_position: 2,
                    position_ms: 123,
                    shuffle: true,
                };
                assert!(matches!(
                    plan(&command).unwrap(),
                    Some(LoadPlan::ResolveContext { .. })
                ));
                for invalid in [
                    Value::Null,
                    json!({"track":null}),
                    json!({"item":null,"track":{"uri":A}}),
                    json!({"item":{"uri":A,"is_playable":false}}),
                    json!({"track":{"uri":A,"is_local":true}}),
                    json!({"item":{"uri":A,"restrictions":{"reason":"market"}}}),
                    json!({"is_playable":false,"item":{"uri":A}}),
                    json!({"uri":"spotify:track:short"}),
                ] {
                    // Another occurrence at row 0 cannot rescue an invalid row 2.
                    let rows = [json!({"uri":A}), Value::Null, invalid];
                    assert!(matches!(
                        resolve_context(&command, &rows),
                        Err(ApiError::CollectionChanged)
                    ));
                }
                let short = [json!({"uri":A})];
                assert!(matches!(
                    resolve_context(&command, &short),
                    Err(ApiError::CollectionChanged)
                ));
                let rows = [
                    json!({"track":{"uri":A}}),
                    Value::Null,
                    json!({"item":{"uri":A}}),
                ];
                let (value, shuffle, qualification) =
                    body(resolve_context(&command, &rows).unwrap());
                assert_eq!(
                    value,
                    json!({"context_uri":uri,"offset":{"position":2},"position_ms":123})
                );
                assert!(shuffle);
                assert_eq!(qualification, Qualification::DocumentedContext);
                if track.is_some() {
                    let moved = [json!({"uri":A}), Value::Null, json!({"uri":B})];
                    assert!(matches!(
                        resolve_context(&command, &moved),
                        Err(ApiError::CollectionChanged)
                    ));
                }
            }
        }
    }

    #[test]
    fn unique_context_uri_must_also_still_be_playable() {
        let command = PlayerCommand::PlayContext {
            uri: "spotify:playlist:0123456789abcdefghijkl".into(),
            track: Some(A.into()),
            position_ms: 0,
            shuffle: false,
        };
        assert!(matches!(
            resolve_context(&command, &[json!({"track":{"uri":A,"is_playable":false}})]),
            Err(ApiError::CollectionChanged)
        ));
    }

    #[test]
    fn catalog_ids_require_exactly_22_ascii_alphanumeric_characters() {
        for kind in ["track", "episode", "album", "playlist", "artist"] {
            let uri = format!("spotify:{kind}:0123456789abcdefghijkl");
            assert!(valid_uri(&uri, &[kind]));
            for id in [
                "",
                "a",
                "0123456789abcdefghijk",
                "0123456789abcdefghijklm",
                "0123456789abcdefghijk_",
                "0123456789abcdefghijké",
            ] {
                assert!(!valid_uri(&format!("spotify:{kind}:{id}"), &[kind]));
            }
        }
        assert!(!valid_uri(A, &["episode"]));
        assert!(!valid_uri(
            "spotify:local:0123456789abcdefghijkl",
            &["track"]
        ));
    }

    #[test]
    fn direct_episodes_use_sdk_position_without_track_only_validation() {
        let command = PlayerCommand::PlayContext {
            uri: "spotify:episode:0123456789abcdefghijkl".into(),
            track: None,
            position_ms: 1234,
            shuffle: false,
        };
        assert!(
            matches!(plan(&command).unwrap(), Some(LoadPlan::SdkPlayable {
            uri, position_ms: 1234, ..
        }) if uri == "spotify:episode:0123456789abcdefghijkl")
        );
        assert!(plan(&PlayerCommand::Pause).unwrap().is_none());
    }

    #[test]
    fn head_of_complete_array_uses_documented_payload_without_offset() {
        let uris = vec![A.into(), A.into(), Z.into()];
        for command in [
            PlayerCommand::PlayTracks {
                uris: uris.clone(),
                start: None,
                position_ms: 31,
                shuffle: true,
            },
            PlayerCommand::PlayTracksAt {
                uris: uris.clone(),
                start_index: 0,
                position_ms: 31,
                shuffle: true,
            },
        ] {
            let (value, shuffle, qualification) = body(plan(&command).unwrap().unwrap());
            assert_eq!(value, json!({"uris":uris,"position_ms":31}));
            assert!(shuffle);
            assert_eq!(qualification, Qualification::DocumentedExplicitArray);
        }
    }

    #[test]
    fn liked_resolves_original_position_not_filtered_or_duplicate_uri_position() {
        let rows = vec![
            json!({"track":{"uri":A}}),
            Value::Null,
            json!({"track":null}),
            json!({"track":{"uri":B,"is_playable":false}}),
            json!({"track":{"uri":"spotify:local:demo"}}),
            json!({"track":{"uri":A}}),
            json!({"track":{"uri":Z}}),
        ];
        let command = PlayerCommand::PlayLiked {
            start: Some(A.into()),
            source_position: Some(5),
            position_ms: 33,
            shuffle: true,
        };
        let (value, shuffle, _) = body(resolve_liked(&command, &rows).unwrap());
        assert_eq!(value["uris"], json!([A, A, Z]));
        assert_eq!(value["offset"], json!({"position":1}));
        assert_eq!(value["position_ms"], 33);
        assert!(shuffle);
        let moved = PlayerCommand::PlayLiked {
            start: Some(A.into()),
            source_position: Some(3),
            position_ms: 33,
            shuffle: true,
        };
        assert!(matches!(
            resolve_liked(&moved, &rows),
            Err(ApiError::CollectionChanged)
        ));
        let ambiguous = PlayerCommand::PlayLiked {
            start: Some(A.into()),
            source_position: None,
            position_ms: 33,
            shuffle: true,
        };
        assert!(matches!(
            resolve_liked(&ambiguous, &rows),
            Err(ApiError::InvalidRequest)
        ));
    }

    #[test]
    fn liked_action_is_a_small_descriptor_and_resolves_all_rows() {
        let command = PlayerCommand::PlayLiked {
            start: None,
            source_position: None,
            position_ms: 0,
            shuffle: false,
        };
        assert!(serde_json::to_vec(&command).unwrap().len() < 200);
        assert!(matches!(
            plan(&command).unwrap(),
            Some(LoadPlan::ResolveLiked { .. })
        ));
        let rows: Vec<_> = (0..5107)
            .map(|i| json!({"track":{"uri":format!("spotify:track:{i:022}")}}))
            .collect();
        let (value, _, _) = body(resolve_liked(&command, &rows).unwrap());
        assert_eq!(value["uris"].as_array().unwrap().len(), 5107);
        assert_eq!(value["uris"][5106], format!("spotify:track:{:022}", 5106));
    }

    #[test]
    fn rejects_invalid_uris_missing_ambiguous_and_out_of_range_selections() {
        for uri in [
            "file:///tmp/a",
            "myx:action:liked-play",
            "",
            "spotify:track:a?b",
            "spotify:track:a:b",
        ] {
            let command = PlayerCommand::PlayContext {
                uri: uri.into(),
                track: None,
                position_ms: 0,
                shuffle: false,
            };
            assert!(plan(&command).is_err());
        }
        let uris = vec![A.into(), A.into()];
        let ambiguous = PlayerCommand::PlayTracks {
            uris: uris.clone(),
            start: Some(uris[0].clone()),
            position_ms: 0,
            shuffle: false,
        };
        assert!(plan(&ambiguous).is_err());
        for index in [2, u32::MAX] {
            let command = PlayerCommand::PlayTracksAt {
                uris: uris.clone(),
                start_index: index,
                position_ms: 0,
                shuffle: false,
            };
            assert!(plan(&command).is_err());
        }
        let empty = PlayerCommand::PlayTracksAt {
            uris: vec![],
            start_index: 0,
            position_ms: 0,
            shuffle: false,
        };
        assert!(plan(&empty).is_err());
        let context = PlayerCommand::PlayContextAt {
            uri: "spotify:playlist:0123456789abcdefghijkl".into(),
            track: None,
            row_position: u32::MAX,
            position_ms: 0,
            shuffle: false,
        };
        assert!(plan(&context).is_err());
    }
}
