//! Demand-driven LRCLIB viewer work, with track identity and bounded transport.
use super::*;
use crate::*;

pub(crate) struct LyricsResult {
    pub(crate) uri: String,
    pub(crate) lines: Vec<(u32, String)>,
    pub(crate) synced: bool,
}

pub(crate) fn request_lyrics(app: &mut App, tx: &flume::Sender<Reply<LyricsResult>>) {
    let Some(now) = app
        .playback
        .now
        .as_ref()
        .filter(|now| !now.title.is_empty())
    else {
        return;
    };
    let key = (
        now.uri.clone(),
        now.title.clone(),
        now.artist.clone(),
        now.album.clone(),
        now.duration_ms,
    );
    if app.session.lyrics_key.as_ref() == Some(&key) {
        return;
    }
    app.session.lyrics_key = Some(key.clone());
    app.session.lyrics_error = None;
    let id = app.session.lyrics_request.start();
    let tx = tx.clone();
    tokio::task::spawn_blocking(move || {
        let (uri, title, artist, album, duration) = key;
        let result = fetch_lyrics_blocking(&artist, &title, &album, duration)
            .map(|(lines, synced)| LyricsResult { uri, lines, synced });
        let _ = tx.send(Reply { id, result });
    });
}

pub(crate) fn fetch_lyrics_blocking(
    artist: &str,
    title: &str,
    album: &str,
    duration_ms: u32,
) -> FetchResult<(Vec<(u32, String)>, bool)> {
    let client = http_client()?;
    let url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}&album_name={}&duration={}",
        urlencode(artist),
        urlencode(title),
        urlencode(album),
        duration_ms / 1000
    );
    let response = client
        .get(&url)
        .header("User-Agent", "myx (terminal spotify player)")
        .send()
        .map_err(|_| ApiError::Offline)?;
    if response.status().as_u16() == 404 {
        return Ok((Vec::new(), false));
    }
    if !response.status().is_success() {
        return Err(ApiError::Server);
    }
    use std::io::Read;
    const MAX_LYRICS_BYTES: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    response
        .take(MAX_LYRICS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ApiError::Offline)?;
    if bytes.len() as u64 > MAX_LYRICS_BYTES {
        return Err(ApiError::InvalidResponse);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| ApiError::InvalidResponse)?;
    if let Some(synced) = value["syncedLyrics"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        return Ok((parse_lrc(synced), true));
    }
    if let Some(plain) = value["plainLyrics"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        return Ok((
            plain.lines().map(|line| (0, line.to_owned())).collect(),
            false,
        ));
    }
    Ok((Vec::new(), false))
}
