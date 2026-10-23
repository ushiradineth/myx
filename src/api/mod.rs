//! UI catalog adapters. Authentication, Spotify HTTP and catalog caching belong
//! to the daemon; public artwork and lyrics remain viewer-side work.

mod actions;
mod detail;
mod library;
mod lyrics;
mod search;
mod track;

pub(crate) use actions::*;
pub(crate) use detail::*;
pub(crate) use library::*;
pub(crate) use lyrics::*;
pub(crate) use search::*;
pub(crate) use track::*;

use crate::*;

pub(crate) fn connection_of(webapi: &Arc<Mutex<WebApi>>) -> Option<Connection> {
    Some(webapi.lock().ok()?.connection())
}

pub(crate) const API: &str = "/v1";

fn catalog_path(url: &str) -> &str {
    url.strip_prefix("https://api.spotify.com").unwrap_or(url)
}

pub(crate) fn get_json(
    _client: &reqwest::blocking::Client,
    url: &str,
    connection: &Connection,
) -> Option<serde_json::Value> {
    match connection.catalog(myx::webapi::HttpMethod::Get, catalog_path(url), None) {
        Ok(value) => Some(value),
        Err(error) => {
            liblog(format!("daemon catalog request failed: {error}"));
            None
        }
    }
}

/// Compatibility adapter; catalog caching is exclusively daemon-owned.
pub(crate) fn get_json_cached(
    client: &reqwest::blocking::Client,
    url: &str,
    connection: &Connection,
) -> Option<serde_json::Value> {
    get_json(client, url, connection)
}

pub(crate) fn fetch_cover(client: &reqwest::blocking::Client, url: &str) -> Option<Vec<u8>> {
    if let Some(bytes) = myx::httpcache::get_bytes(url) {
        return Some(bytes);
    }
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        liblog(format!("cover: HTTP {}", response.status().as_u16()));
        return None;
    }
    let bytes = response.bytes().ok()?.to_vec();
    myx::httpcache::put_bytes(url, &bytes);
    Some(bytes)
}

pub(crate) fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}
