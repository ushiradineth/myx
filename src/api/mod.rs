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
pub(crate) use myx::webapi::{ApiError, HttpMethod};
pub(crate) type FetchResult<T> = std::result::Result<T, ApiError>;

pub(crate) struct Reply<T> {
    pub(crate) id: u64,
    pub(crate) result: FetchResult<T>,
}

/// Never display an anyhow chain or server body. Only classifications cross to UI.
pub(crate) fn fetch_error(error: anyhow::Error) -> ApiError {
    use myx::ipc::protocol::{ErrorCode, IpcError};
    if let Some(error) = error.downcast_ref::<ApiError>() {
        return error.clone();
    }
    match error.downcast_ref::<IpcError>() {
        Some(error) => match error.code {
            ErrorCode::Unauthorized | ErrorCode::SetupRequired => ApiError::Unauthorized,
            ErrorCode::Restricted => ApiError::Restricted,
            ErrorCode::NotFound => ApiError::NotFound,
            ErrorCode::Quota => ApiError::Quota,
            ErrorCode::RateLimited => ApiError::RateLimited {
                retry_after_secs: error.retry_after_secs,
            },
            ErrorCode::WriteUncertain => ApiError::AmbiguousWrite,
            ErrorCode::CollectionChanged => ApiError::CollectionChanged,
            ErrorCode::InvalidRequest => ApiError::InvalidRequest,
            ErrorCode::Internal => ApiError::Server,
            ErrorCode::Busy => ApiError::Busy,
            ErrorCode::Unavailable | ErrorCode::Timeout => ApiError::Offline,
            ErrorCode::VersionMismatch | ErrorCode::ConfigMismatch => ApiError::InvalidRequest,
        },
        None => ApiError::Offline,
    }
}

/// A lost client/daemon response can follow an applied mutation. Without an
/// explicit typed rejection there is no evidence that retrying would be safe.
pub(crate) fn write_error(error: anyhow::Error) -> ApiError {
    if error.downcast_ref::<ApiError>().is_some()
        || error
            .downcast_ref::<myx::ipc::protocol::IpcError>()
            .is_some()
    {
        fetch_error(error)
    } else {
        ApiError::AmbiguousWrite
    }
}

pub(crate) fn connection_of(webapi: &Arc<Mutex<WebApi>>) -> FetchResult<Connection> {
    webapi
        .lock()
        .map(|api| api.connection())
        .map_err(|_| ApiError::Offline)
}

pub(crate) const API: &str = "/v1";

fn catalog_path(url: &str) -> &str {
    url.strip_prefix("https://api.spotify.com").unwrap_or(url)
}

pub(crate) fn get_json(connection: &Connection, url: &str) -> FetchResult<serde_json::Value> {
    connection
        .catalog(HttpMethod::Get, catalog_path(url), None)
        .map_err(fetch_error)
}

const MAX_IMAGE_BYTES: u64 = 12 * 1024 * 1024;

pub(crate) fn fetch_cover(client: &reqwest::blocking::Client, url: &str) -> Option<Vec<u8>> {
    if let Some(bytes) = myx::httpcache::get_bytes(url) {
        if bytes.len() as u64 <= MAX_IMAGE_BYTES {
            return Some(bytes);
        }
        return None;
    }
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        liblog(format!("cover: HTTP {}", response.status().as_u16()));
        return None;
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_IMAGE_BYTES)
    {
        return None;
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    response
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    myx::httpcache::put_bytes(url, &bytes);
    Some(bytes)
}

pub(crate) fn http_client() -> FetchResult<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .tls_backend_native()
        .https_only(true)
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::limited(3))
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| ApiError::Offline)
}
