use std::fmt;
use std::io::Read;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Authorization;

const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RETRY_AFTER: u64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}

/// Only bounded, non-sensitive classifications cross the daemon boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiError {
    Unauthorized,
    Restricted,
    NotFound,
    RateLimited { retry_after_secs: Option<u64> },
    Quota,
    Offline,
    Server,
    InvalidRequest,
    InvalidResponse,
    DeviceNotOwned,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unauthorized => "Spotify authorization requires setup or renewal",
            Self::Restricted => "Spotify denied this operation",
            Self::NotFound => "Spotify resource was not found",
            Self::RateLimited { .. } => "Spotify rate limit reached",
            Self::Quota => "Spotify application quota reached",
            Self::Offline => "Spotify is unavailable or the request timed out",
            Self::Server => "Spotify service failed",
            Self::InvalidRequest => "Invalid Spotify API request",
            Self::InvalidResponse => "Invalid Spotify API response",
            Self::DeviceNotOwned => "Playback operation requires the verified owned device",
        })
    }
}

impl std::error::Error for ApiError {}

pub struct ApiClient {
    auth: Authorization,
    // Builder failure is retained as a typed error rather than a panic or an unbounded fallback client.
    http: Result<reqwest::blocking::Client, ApiError>,
    owned_device: Option<String>,
}

impl ApiClient {
    pub fn new(auth: Authorization) -> Self {
        let http = reqwest::blocking::Client::builder()
            .tls_backend_native()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| ApiError::Offline);
        Self {
            auth,
            http,
            owned_device: None,
        }
    }

    /// The controller supplies an independently verified stable device ID, never a friendly name.
    /// Clearing ownership immediately revokes permission for player writes.
    pub fn set_owned_device(&mut self, device: Option<String>) {
        self.owned_device = device.filter(|id| valid_device_id(id));
    }

    pub fn request(
        &mut self,
        method: HttpMethod,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ApiError> {
        let url = validate_request(method, path, body.as_ref(), self.owned_device.as_deref())?;
        let client = self.http.as_ref().map_err(Clone::clone)?;
        let method_http = match method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Delete => reqwest::Method::DELETE,
        };
        // Never replay mutations: a timeout or server failure can follow an accepted write.
        // GET may retry once on a short explicit rate-limit delay. No unbounded Retry-After sleep.
        for attempt in 0..2 {
            let token = self.auth.token().map_err(|error| {
                error
                    .downcast_ref::<ApiError>()
                    .cloned()
                    .unwrap_or(ApiError::Unauthorized)
            })?;
            let mut request = client
                .request(method_http.clone(), url.clone())
                .bearer_auth(token);
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request.send().map_err(|_| ApiError::Offline)?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            if status == 429 && method == HttpMethod::Get && attempt == 0 {
                if let Some(seconds @ 0..=2) = retry_after {
                    drop(response);
                    std::thread::sleep(Duration::from_secs(seconds));
                    continue;
                }
            }
            if !(200..300).contains(&status) {
                let mut error = classify_status(status, retry_after);
                if matches!(status, 403 | 429) {
                    // Inspect only a bounded known error field, never retain or expose the raw body.
                    let mut bytes = Vec::new();
                    if response.take(16385).read_to_end(&mut bytes).is_ok()
                        && bytes.len() <= 16384
                        && quota_message(&bytes)
                    {
                        error = ApiError::Quota;
                    }
                }
                return Err(error);
            }
            if status == 204 {
                return Ok(Value::Null);
            }
            if response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES)
            {
                return Err(ApiError::InvalidResponse);
            }
            let mut bytes = Vec::new();
            response
                .take(MAX_RESPONSE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| ApiError::Offline)?;
            if bytes.len() as u64 > MAX_RESPONSE_BYTES {
                return Err(ApiError::InvalidResponse);
            }
            if bytes.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_slice(&bytes).map_err(|_| ApiError::InvalidResponse);
        }
        Err(ApiError::Server)
    }
}

fn classify_status(status: u16, retry_after_secs: Option<u64>) -> ApiError {
    match status {
        401 => ApiError::Unauthorized,
        402 => ApiError::Quota,
        403 => ApiError::Restricted,
        404 => ApiError::NotFound,
        429 => ApiError::RateLimited { retry_after_secs },
        500..=599 => ApiError::Server,
        _ => ApiError::InvalidRequest,
    }
}

fn quota_message(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|value| {
            value
                .get("error")?
                .get("message")?
                .as_str()
                .map(str::to_owned)
        })
        .is_some_and(|message| message.to_ascii_lowercase().contains("quota"))
}

fn parse_retry_after(value: &str) -> Option<u64> {
    // HTTP-date and oversized/untrusted integers are not used as sleep instructions.
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().map(|n| n.min(MAX_RETRY_AFTER))
}

fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn validate_request(
    method: HttpMethod,
    path: &str,
    body: Option<&Value>,
    owned_device: Option<&str>,
) -> Result<reqwest::Url, ApiError> {
    if path.len() > 16384
        || !path.starts_with("/v1/")
        || path.contains(['#', '\\'])
        || path.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return Err(ApiError::InvalidRequest);
    }
    let route = path.split('?').next().ok_or(ApiError::InvalidRequest)?;
    // Encoded route separators/dot segments must not bypass the player-write boundary.
    if route.contains('%')
        || route
            .split('/')
            .skip(2)
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(ApiError::InvalidRequest);
    }
    let first = route.split('/').nth(2).ok_or(ApiError::InvalidRequest)?;
    if !matches!(
        first,
        "albums"
            | "artists"
            | "audiobooks"
            | "browse"
            | "chapters"
            | "episodes"
            | "markets"
            | "me"
            | "playlists"
            | "recommendations"
            | "search"
            | "shows"
            | "tracks"
            | "users"
            | "audio-features"
            | "audio-analysis"
    ) {
        return Err(ApiError::InvalidRequest);
    }
    let url = reqwest::Url::parse(&format!("https://api.spotify.com{path}"))
        .map_err(|_| ApiError::InvalidRequest)?;
    if url.host_str() != Some("api.spotify.com") || url.path() != route {
        return Err(ApiError::InvalidRequest);
    }
    if method == HttpMethod::Get && body.is_some() {
        return Err(ApiError::InvalidRequest);
    }
    if method != HttpMethod::Get
        && (route == "/v1/me/player" || route.starts_with("/v1/me/player/"))
    {
        let device = owned_device
            .filter(|id| valid_device_id(id))
            .ok_or(ApiError::DeviceNotOwned)?;
        let targets: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key == "device_id")
            .collect();
        if route == "/v1/me/player" {
            if method != HttpMethod::Put {
                return Err(ApiError::InvalidRequest);
            }
            let devices = body
                .and_then(|b| b.get("device_ids"))
                .and_then(Value::as_array)
                .ok_or(ApiError::DeviceNotOwned)?;
            if devices.len() != 1
                || devices[0].as_str() != Some(device)
                || (!targets.is_empty() && (targets.len() != 1 || targets[0].1 != device))
            {
                return Err(ApiError::DeviceNotOwned);
            }
        } else if targets.len() != 1 || targets[0].1 != device {
            return Err(ApiError::DeviceNotOwned);
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_host_traversal_fragment_and_encoded_player_bypasses() {
        for path in [
            "https://evil.test/v1/me",
            "//evil.test/v1/me",
            "/v1/me#fragment",
            "/v1/me/../tracks",
            "/v1/me/%70layer/play",
            "/v1/me/player%2fplay",
            "/v1/me\\player/play",
            "/v1/me//player/play",
            "/v1/me/player/play\n",
        ] {
            assert_eq!(
                validate_request(HttpMethod::Put, path, None, Some("owned")),
                Err(ApiError::InvalidRequest)
            );
        }
    }

    #[test]
    fn playback_writes_require_exact_verified_target_including_transfer() {
        for (target, owner) in [
            ("/v1/me/player/play", Some("owned")),
            ("/v1/me/player/play?device_id=other", Some("owned")),
            (
                "/v1/me/player/play?device_id=owned&device_id=other",
                Some("owned"),
            ),
            ("/v1/me/player/play?device_id=owned", None),
        ] {
            assert_eq!(
                validate_request(HttpMethod::Put, target, None, owner),
                Err(ApiError::DeviceNotOwned)
            );
        }
        assert!(validate_request(
            HttpMethod::Put,
            "/v1/me/player/play?device_id=owned",
            None,
            Some("owned")
        )
        .is_ok());
        assert!(validate_request(
            HttpMethod::Put,
            "/v1/me/player",
            Some(&json!({"device_ids":["owned"]})),
            Some("owned")
        )
        .is_ok());
        for body in [
            json!({}),
            json!({"device_ids":["other"]}),
            json!({"device_ids":["owned","other"]}),
        ] {
            assert_eq!(
                validate_request(HttpMethod::Put, "/v1/me/player", Some(&body), Some("owned")),
                Err(ApiError::DeviceNotOwned)
            );
        }
        assert!(validate_request(HttpMethod::Put, "/v1/me/tracks?ids=track", None, None).is_ok());
        assert!(validate_request(HttpMethod::Get, "/v1/me/player", None, None).is_ok());
    }

    #[test]
    fn errors_are_typed_redacted_and_retry_after_is_bounded() {
        assert_eq!(classify_status(401, None), ApiError::Unauthorized);
        assert_eq!(classify_status(403, None), ApiError::Restricted);
        assert_eq!(classify_status(404, None), ApiError::NotFound);
        assert_eq!(classify_status(402, None), ApiError::Quota);
        assert_eq!(classify_status(503, None), ApiError::Server);
        assert!(quota_message(
            br#"{"error":{"message":"Application quota exceeded"}}"#
        ));
        assert!(!quota_message(
            br#"{"error":{"message":"Insufficient scope"}}"#
        ));
        assert_eq!(parse_retry_after("99999"), Some(300));
        assert_eq!(parse_retry_after("-1"), None);
        assert_eq!(parse_retry_after("99999999999999999999999"), None);
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        let error = classify_status(429, Some(123));
        assert_eq!(error.to_string(), "Spotify rate limit reached");
        assert!(!error.to_string().contains("123"));
        assert_eq!(
            serde_json::to_string(&HttpMethod::Delete).unwrap(),
            "\"delete\""
        );
    }
}
