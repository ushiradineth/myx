//! One track's metadata, cover, and derived theme.

use super::*;
use crate::*;

pub(crate) fn fetch_track_meta(webapi: &Arc<Mutex<WebApi>>, track_id: &str) -> TrackMeta {
    let uri = format!("spotify:track:{track_id}");
    let empty = || TrackMeta {
        uri: uri.clone(),
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        duration_ms: 0,
        image: TrackImage {
            image: None,
            url: None,
        },
        theme: None,
    };
    let Some(token) = token_of(webapi) else {
        return empty();
    };
    let client = http_client();
    let Some(v) = get_json_cached(&client, &format!("{API}/tracks/{track_id}"), &token) else {
        return empty();
    };

    let title = v["name"].as_str().unwrap_or("").to_string();
    let artist = v["artists"][0]["name"].as_str().unwrap_or("").to_string();
    let album = v["album"]["name"].as_str().unwrap_or("").to_string();
    let duration_ms = v["duration_ms"].as_u64().unwrap_or(0) as u32;
    let cover_url = largest_cover_url(&v["album"]["images"]).map(String::from);

    let image = cover_url
        .clone()
        .and_then(|u| fetch_cover(&client, &u))
        .and_then(|bytes| image::load_from_memory(&bytes).ok());
    let theme = image.as_ref().map(|img| derive_theme(img, "album ✦"));

    TrackMeta {
        uri,
        title,
        artist,
        album,
        duration_ms,
        image: TrackImage {
            image,
            url: cover_url,
        },
        theme,
    }
}

/// Prefer known pixel area over unknown dimensions, keeping API order on ties.
/// A valid URL with missing/bad dimensions is still useful as a last resort.
fn largest_cover_url(images: &serde_json::Value) -> Option<&str> {
    let mut best = None;
    for image in images.as_array()? {
        let Some(url) = image["url"].as_str() else {
            continue;
        };
        let Ok(parsed) = reqwest::Url::parse(url) else {
            continue;
        };
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            continue;
        }
        let area = match (image["width"].as_u64(), image["height"].as_u64()) {
            (Some(w), Some(h)) if w > 0 && h > 0 => u128::from(w) * u128::from(h),
            _ => 0,
        };
        if best.is_none_or(|(_, best_area)| area > best_area) {
            best = Some((url, area));
        }
    }
    best.map(|(url, _)| url)
}

/// Optional artwork enrichment only: SDK text remains authoritative, and this
/// path never initializes Web API authorization. Run on the blocking worker.
pub(crate) fn fetch_sdk_artwork(
    webapi: &Arc<Mutex<WebApi>>,
    meta: myx::engine::TrackMetadata,
) -> TrackMeta {
    let client = http_client();
    // There is no episode metadata fetch in the existing API layer. Episodes
    // (and other SDK URI kinds) keep their SDK cover instead of guessing an API.
    let preferred = track_id_from_uri(&meta.uri)
        .and_then(|id| {
            let token = token_of(webapi)?;
            get_json_cached(&client, &format!("{API}/tracks/{id}"), &token)
        })
        .and_then(|v| largest_cover_url(&v["album"]["images"]).map(String::from));
    let fallback = TrackMeta {
        uri: meta.uri.clone(),
        title: meta.title.clone(),
        artist: meta.artist.clone(),
        album: meta.album.clone(),
        duration_ms: meta.duration_ms,
        image: TrackImage {
            image: None,
            url: meta.cover_url.clone(),
        },
        theme: None,
    };
    // Text/lyrics/native metadata must still be delivered when all image HTTP
    // fails. The receiver preserves a same-track cover on an image-less update.
    sdk_artwork_meta(meta, preferred.as_deref(), |url| fetch_cover(&client, url))
        .unwrap_or(fallback)
}

fn sdk_artwork_meta(
    meta: myx::engine::TrackMetadata,
    preferred: Option<&str>,
    mut fetch: impl FnMut(&str) -> Option<Vec<u8>>,
) -> Option<TrackMeta> {
    // Only the selected Web API image and SDK fallback are tried. In particular,
    // don't request the same URL twice when both sources name the same image.
    let fallback = meta
        .cover_url
        .as_deref()
        .filter(|url| Some(*url) != preferred);
    let (url, image) = preferred.into_iter().chain(fallback).find_map(|url| {
        let bytes = fetch(url)?;
        let image = image::load_from_memory(&bytes).ok()?;
        Some((url.to_owned(), image))
    })?;
    let theme = Some(derive_theme(&image, "album ✦"));
    Some(TrackMeta {
        uri: meta.uri,
        title: meta.title,
        artist: meta.artist,
        album: meta.album,
        duration_ms: meta.duration_ms,
        image: TrackImage {
            image: Some(image),
            url: Some(url),
        },
        theme,
    })
}

#[cfg(test)]
mod artwork_tests {
    use super::*;
    use serde_json::json;

    fn sdk_meta() -> myx::engine::TrackMetadata {
        myx::engine::TrackMetadata {
            uri: "spotify:track:test".into(),
            title: "SDK title".into(),
            artist: "SDK artist".into(),
            album: "SDK album".into(),
            duration_ms: 123_456,
            cover_url: Some("https://sdk.example/cover".into()),
        }
    }

    fn png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn largest_image_uses_area_not_array_order_or_width_alone() {
        let images = json!([
            {"url": "https://art.example/small", "width": 64, "height": 64},
            {"url": "https://art.example/wide", "width": 2000, "height": 10},
            {"url": "https://art.example/large", "width": 640, "height": 640},
            {"url": "https://art.example/medium", "width": 300, "height": 300}
        ]);
        assert_eq!(
            largest_cover_url(&images),
            Some("https://art.example/large")
        );
    }

    #[test]
    fn image_selection_rejects_invalid_urls_and_tolerates_bad_dimensions() {
        let images = json!([
            {"url": "http://art.example/insecure", "width": 9000, "height": 9000},
            {"url": "not a URL", "width": 9000, "height": 9000},
            {"url": "https://user:password@art.example/private"},
            {"url": null},
            {"url": "https://art.example/unknown", "width": null, "height": null},
            {"url": "https://art.example/negative", "width": -1, "height": 1000},
            {"url": "https://art.example/string", "width": "1000", "height": 1000},
            {"url": "https://art.example/zero", "width": 0, "height": 1000},
            {"url": "https://art.example/known", "width": 64, "height": 64}
        ]);
        assert_eq!(
            largest_cover_url(&images),
            Some("https://art.example/known")
        );
        assert_eq!(
            largest_cover_url(&json!([
                {"url": "https://art.example/unknown"},
                {"url": "https://art.example/also-unknown", "width": 0, "height": 0}
            ])),
            Some("https://art.example/unknown")
        );
        for absent in [json!(null), json!({}), json!([]), json!([{"url": ""}])] {
            assert_eq!(largest_cover_url(&absent), None);
        }
    }

    #[test]
    fn image_selection_handles_large_dimensions_and_preserves_tie_order() {
        let images = json!([
            {"url": "https://art.example/first", "width": u64::MAX, "height": u64::MAX},
            {"url": "https://art.example/tie", "width": u64::MAX, "height": u64::MAX}
        ]);
        assert_eq!(
            largest_cover_url(&images),
            Some("https://art.example/first")
        );
    }

    #[test]
    fn preferred_artwork_preserves_sdk_text_and_skips_fallback_fetch() {
        let mut requested = Vec::new();
        let result = sdk_artwork_meta(sdk_meta(), Some("https://api.example/large"), |url| {
            requested.push(url.to_owned());
            Some(png())
        })
        .unwrap();
        assert_eq!(requested, ["https://api.example/large"]);
        assert_eq!(result.uri, "spotify:track:test");
        assert_eq!(result.title, "SDK title");
        assert_eq!(result.artist, "SDK artist");
        assert_eq!(result.album, "SDK album");
        assert_eq!(result.duration_ms, 123_456);
        assert_eq!(
            result.image.url.as_deref(),
            Some("https://api.example/large")
        );
        assert!(result.image.image.is_some());
        assert!(result.theme.is_some());
    }

    #[test]
    fn unavailable_metadata_or_authorization_uses_sdk_cover_directly() {
        // No preferred URL also represents missing/invalid album images. There
        // is no HTTP or credential access at this deterministic resolver seam.
        let mut requested = Vec::new();
        let result = sdk_artwork_meta(sdk_meta(), None, |url| {
            requested.push(url.to_owned());
            Some(png())
        })
        .unwrap();
        assert_eq!(requested, ["https://sdk.example/cover"]);
        assert_eq!(
            result.image.url.as_deref(),
            Some("https://sdk.example/cover")
        );
        assert_eq!(result.title, "SDK title");
    }

    #[test]
    fn failed_fetch_or_decode_falls_back_to_sdk_cover() {
        for failed_bytes in [None, Some(b"not an image".to_vec())] {
            let mut requested = Vec::new();
            let result = sdk_artwork_meta(sdk_meta(), Some("https://api.example/large"), |url| {
                requested.push(url.to_owned());
                if url == "https://api.example/large" {
                    failed_bytes.clone()
                } else {
                    Some(png())
                }
            })
            .unwrap();
            assert_eq!(
                requested,
                ["https://api.example/large", "https://sdk.example/cover"]
            );
            assert_eq!(
                result.image.url.as_deref(),
                Some("https://sdk.example/cover")
            );
        }
    }

    #[test]
    fn episodes_keep_their_sdk_uri_and_artwork() {
        let mut meta = sdk_meta();
        meta.uri = "spotify:episode:test".into();
        let result = sdk_artwork_meta(meta, None, |_| Some(png())).unwrap();
        assert_eq!(result.uri, "spotify:episode:test");
        assert_eq!(
            result.image.url.as_deref(),
            Some("https://sdk.example/cover")
        );
    }

    #[test]
    fn both_sources_failing_produce_no_replacement() {
        for failed_bytes in [None, Some(b"not an image".to_vec())] {
            let mut requested = Vec::new();
            let result = sdk_artwork_meta(sdk_meta(), Some("https://api.example/large"), |url| {
                requested.push(url.to_owned());
                failed_bytes.clone()
            });
            assert!(result.is_none());
            assert_eq!(
                requested,
                ["https://api.example/large", "https://sdk.example/cover"]
            );
        }
    }

    #[test]
    fn complete_failure_sends_no_cover_replacement_and_duplicate_urls_are_not_retried() {
        let mut requests = 0;
        let result = sdk_artwork_meta(sdk_meta(), Some("https://sdk.example/cover"), |_| {
            requests += 1;
            Some(b"broken cached image".to_vec())
        });
        assert!(result.is_none());
        assert_eq!(requests, 1);
        let mut meta = sdk_meta();
        meta.cover_url = None;
        assert!(sdk_artwork_meta(meta, None, |_| panic!("no URL to fetch")).is_none());
    }
}
