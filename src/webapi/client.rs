use std::fmt;
use std::io::Read;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cache::ResponseCache;
use super::Authorization;

const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RETRY_AFTER: u64 = 300;
// Missing/invalid numeric headers must not turn repeated callers into a busy retry loop.
const RATE_LIMIT_BACKOFF: u64 = 5;
const QUOTA_BACKOFF: u64 = 60;

#[derive(Default)]
struct Cooldown {
    active: Option<CooldownEntry>,
}

struct CooldownEntry {
    started: Instant,
    delay: Duration,
    quota: bool,
}

impl Cooldown {
    fn record(&mut self, error: &ApiError, retry_after: Option<u64>, now: Instant) {
        let (seconds, quota) = match error {
            ApiError::RateLimited { .. } => (retry_after.unwrap_or(RATE_LIMIT_BACKOFF), false),
            // A quota response with no usable positive delay gets a conservative 60s backoff.
            ApiError::Quota => (
                retry_after.filter(|n| *n > 0).unwrap_or(QUOTA_BACKOFF),
                true,
            ),
            _ => return,
        };
        self.active = Some(CooldownEntry {
            started: now,
            delay: Duration::from_secs(seconds),
            quota,
        });
    }

    fn check(&mut self, now: Instant) -> Result<(), ApiError> {
        let Some(entry) = &self.active else {
            return Ok(());
        };
        // Never add an untrusted duration to Instant: even u64::MAX seconds is safe here.
        let remaining = entry
            .delay
            .saturating_sub(now.saturating_duration_since(entry.started));
        if remaining.is_zero() {
            self.active = None;
            return Ok(());
        }
        if entry.quota {
            return Err(ApiError::Quota);
        }
        // Round up partial seconds, then cap only the redacted public representation.
        let seconds = remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() > 0));
        Err(ApiError::RateLimited {
            retry_after_secs: Some(seconds.min(MAX_RETRY_AFTER)),
        })
    }
}

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
    Busy,
    Unauthorized,
    Restricted,
    NotFound,
    RateLimited { retry_after_secs: Option<u64> },
    Quota,
    Offline,
    Server,
    InvalidRequest,
    InvalidResponse,
    AmbiguousWrite,
    CollectionChanged,
    DeviceNotOwned,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "Daemon API worker is busy; retry after the pending operation",
            Self::Unauthorized => "Spotify authorization requires setup or renewal",
            Self::Restricted => "Spotify denied this operation",
            Self::NotFound => "Spotify resource was not found",
            Self::RateLimited { .. } => "Spotify rate limit reached",
            Self::Quota => "Spotify application quota reached",
            Self::Offline => "Spotify is unavailable or the request timed out",
            Self::Server => "Spotify service failed",
            Self::InvalidRequest => "Invalid Spotify API request",
            Self::InvalidResponse => "Invalid Spotify API response",
            Self::AmbiguousWrite => {
                "Spotify write outcome is uncertain; refresh before trying again"
            }
            Self::CollectionChanged => "Spotify collection changed or is incomplete; reload it",
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
    cache: ResponseCache,
    cooldown: Cooldown,
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
            cache: ResponseCache::default(),
            cooldown: Cooldown::default(),
        }
    }

    /// The controller supplies an independently verified stable device ID, never a friendly name.
    /// Clearing ownership immediately revokes permission for player writes.
    pub fn set_owned_device(&mut self, device: Option<String>) {
        self.owned_device = device.filter(|id| valid_device_id(id));
    }

    /// Called by the owner worker; a usable, nonexpiring token does no network work.
    pub fn maintain(&mut self) -> Result<(), ApiError> {
        let result = self.auth.token().map(|_| ()).map_err(|error| {
            error
                .downcast_ref::<ApiError>()
                .cloned()
                .unwrap_or(ApiError::Unauthorized)
        });
        if result.is_err() {
            self.cache.invalidate();
        }
        result
    }

    pub fn request(
        &mut self,
        method: HttpMethod,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ApiError> {
        if method != HttpMethod::Get {
            self.cache.invalidate();
        }
        let url = validate_request(method, path, body.as_ref(), self.owned_device.as_deref())?;
        if method == HttpMethod::Get && self.cache.contains_fresh(path, Instant::now()) {
            // Cached data may bypass catalog cooldown, never token expiry/refresh failure.
            self.maintain()?;
            // A refresh can outlast the cache TTL; never return the pre-refresh value stale.
            if let Some(value) = self.cache.get(path, Instant::now()) {
                return Ok(value);
            }
        }
        self.cooldown.check(Instant::now())?;
        // Proactive maintain() remains independent of catalog cooldown. Replacing the
        // authorization through a new ApiClient clears both owner-local cache and cooldown.
        self.maintain()?;
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
            self.cooldown.check(Instant::now())?;
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
            let response = request
                .send()
                .map_err(|_| outcome_error(method, ApiError::Offline))?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
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
                if error == ApiError::Unauthorized {
                    self.cache.invalidate();
                    self.auth.invalidate_access();
                }
                self.cooldown.record(&error, retry_after, Instant::now());
                if let Some(delay) = short_retry_delay(method, attempt, &error) {
                    std::thread::sleep(delay);
                    continue;
                }
                return Err(outcome_error(method, error));
            }
            if status == 204 {
                return Ok(Value::Null);
            }
            if response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES)
            {
                return Err(outcome_error(method, ApiError::InvalidResponse));
            }
            let mut bytes = Vec::new();
            response
                .take(MAX_RESPONSE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| outcome_error(method, ApiError::Offline))?;
            if bytes.len() as u64 > MAX_RESPONSE_BYTES {
                return Err(outcome_error(method, ApiError::InvalidResponse));
            }
            if bytes.is_empty() {
                return Ok(Value::Null);
            }
            let value = serde_json::from_slice(&bytes)
                .map_err(|_| outcome_error(method, ApiError::InvalidResponse))?;
            if method == HttpMethod::Get {
                self.cache.insert(path, &value, Instant::now());
            }
            return Ok(value);
        }
        Err(ApiError::Server)
    }
}

fn outcome_error(method: HttpMethod, error: ApiError) -> ApiError {
    if method != HttpMethod::Get
        && matches!(
            error,
            ApiError::Offline | ApiError::Server | ApiError::InvalidResponse
        )
    {
        ApiError::AmbiguousWrite
    } else {
        error
    }
}

fn short_retry_delay(method: HttpMethod, attempt: usize, error: &ApiError) -> Option<Duration> {
    if method == HttpMethod::Get && attempt == 0 {
        if let ApiError::RateLimited {
            retry_after_secs: Some(seconds @ 0..=2),
        } = error
        {
            return Some(Duration::from_secs(*seconds));
        }
    }
    None
}

fn classify_status(status: u16, retry_after_secs: Option<u64>) -> ApiError {
    match status {
        401 => ApiError::Unauthorized,
        402 => ApiError::Quota,
        403 => ApiError::Restricted,
        404 => ApiError::NotFound,
        429 => ApiError::RateLimited {
            retry_after_secs: retry_after_secs.map(|n| n.min(MAX_RETRY_AFTER)),
        },
        500..=599 => ApiError::Server,
        _ => ApiError::InvalidRequest,
    }
}

fn quota_message(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|value| {
            let error = value.get("error")?;
            if error.get("reason").and_then(Value::as_str) == Some("QUOTA_EXCEEDED") {
                return Some(true);
            }
            error
                .get("message")?
                .as_str()
                .map(|message| message.to_ascii_lowercase().contains("quota"))
        })
        .unwrap_or(false)
}

fn parse_retry_after(value: &str) -> Option<u64> {
    // Preserve the full numeric delay for cooldown; only explicit 0..=2 may become a sleep.
    // HTTP-date and integers outside u64 use the bounded fallback instead.
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok()
}

fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

pub(super) fn validate_request(
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
        assert!(quota_message(
            br#"{"error":{"reason":"QUOTA_EXCEEDED","message":"Denied"}}"#
        ));
        assert!(!quota_message(
            br#"{"error":{"message":"Insufficient scope"}}"#
        ));
        assert_eq!(parse_retry_after("99999"), Some(99999));
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

    #[test]
    fn uncertain_write_errors_are_redacted_and_known_api_failures_stay_typed() {
        for method in [HttpMethod::Post, HttpMethod::Put, HttpMethod::Delete] {
            for error in [
                ApiError::Offline,
                ApiError::Server,
                ApiError::InvalidResponse,
            ] {
                assert_eq!(
                    outcome_error(method, error.clone()),
                    ApiError::AmbiguousWrite
                );
                assert_eq!(outcome_error(HttpMethod::Get, error.clone()), error);
            }
            for status in [400, 401, 402, 403, 404, 409, 429] {
                let error = classify_status(status, Some(47));
                assert_eq!(outcome_error(method, error.clone()), error);
            }
        }
        assert_eq!(
            serde_json::to_string(&ApiError::AmbiguousWrite).unwrap(),
            "\"ambiguous_write\""
        );
        assert_eq!(
            serde_json::to_string(&ApiError::CollectionChanged).unwrap(),
            "\"collection_changed\""
        );
        assert_eq!(
            serde_json::from_str::<ApiError>("\"ambiguous_write\"").unwrap(),
            ApiError::AmbiguousWrite
        );
        assert_eq!(
            serde_json::from_str::<ApiError>("\"collection_changed\"").unwrap(),
            ApiError::CollectionChanged
        );
    }

    #[test]
    fn only_first_get_with_explicit_short_rate_delay_can_retry() {
        for seconds in [0, 1, 2] {
            let error = ApiError::RateLimited {
                retry_after_secs: Some(seconds),
            };
            assert_eq!(
                short_retry_delay(HttpMethod::Get, 0, &error),
                Some(Duration::from_secs(seconds))
            );
            assert_eq!(short_retry_delay(HttpMethod::Get, 1, &error), None);
            for method in [HttpMethod::Post, HttpMethod::Put, HttpMethod::Delete] {
                assert_eq!(short_retry_delay(method, 0, &error), None);
            }
        }
        for error in [
            ApiError::Quota,
            ApiError::Offline,
            ApiError::Server,
            ApiError::RateLimited {
                retry_after_secs: None,
            },
            ApiError::RateLimited {
                retry_after_secs: Some(3),
            },
            ApiError::RateLimited {
                retry_after_secs: Some(300),
            },
        ] {
            assert_eq!(short_retry_delay(HttpMethod::Get, 0, &error), None);
        }
    }

    #[test]
    fn cooldown_keeps_full_hour_while_reporting_at_most_three_hundred_seconds() {
        let start = Instant::now();
        let mut cooldown = Cooldown::default();
        let delay = parse_retry_after("3600");
        let error = classify_status(429, delay);
        assert_eq!(
            error,
            ApiError::RateLimited {
                retry_after_secs: Some(300)
            }
        );
        assert_eq!(short_retry_delay(HttpMethod::Get, 0, &error), None);
        cooldown.record(&error, delay, start);
        for elapsed in [0, 300, 301, 1800, 3300] {
            assert_eq!(
                cooldown.check(start + Duration::from_secs(elapsed)),
                Err(ApiError::RateLimited {
                    retry_after_secs: Some(300)
                })
            );
        }
        assert_eq!(
            cooldown.check(start + Duration::from_secs(3599) + Duration::from_nanos(1)),
            Err(ApiError::RateLimited {
                retry_after_secs: Some(1)
            })
        );
        assert_eq!(cooldown.check(start + Duration::from_secs(3600)), Ok(()));
        assert!(cooldown.active.is_none());
        assert_eq!(cooldown.check(start + Duration::from_secs(3601)), Ok(()));
    }

    #[test]
    fn missing_invalid_and_huge_rate_delays_use_safe_constant_space_state() {
        let start = Instant::now();
        for raw in [
            None,
            Some(""),
            Some("-1"),
            Some("1.5"),
            Some("Wed, 21 Oct 2026 07:28:00 GMT"),
            Some("18446744073709551616"),
        ] {
            let delay = raw.and_then(parse_retry_after);
            assert_eq!(delay, None);
            let mut cooldown = Cooldown::default();
            cooldown.record(&classify_status(429, delay), delay, start);
            assert_eq!(
                cooldown.check(start),
                Err(ApiError::RateLimited {
                    retry_after_secs: Some(RATE_LIMIT_BACKOFF)
                })
            );
            assert_eq!(
                cooldown.check(start + Duration::from_secs(RATE_LIMIT_BACKOFF)),
                Ok(())
            );
        }
        let delay = parse_retry_after("18446744073709551615");
        assert_eq!(delay, Some(u64::MAX));
        let mut cooldown = Cooldown::default();
        cooldown.record(&classify_status(429, delay), delay, start);
        assert_eq!(
            cooldown.check(start + Duration::from_secs(3600)),
            Err(ApiError::RateLimited {
                retry_after_secs: Some(300)
            })
        );
        assert_eq!(
            cooldown.active.as_ref().unwrap().delay,
            Duration::from_secs(u64::MAX)
        );
    }

    #[test]
    fn quota_preserves_type_uses_full_delay_or_fallback_and_never_replays_mutations() {
        let start = Instant::now();
        for delay in [None, Some(0), Some(3600)] {
            let seconds = delay.filter(|n| *n > 0).unwrap_or(QUOTA_BACKOFF);
            let mut cooldown = Cooldown::default();
            cooldown.record(&ApiError::Quota, delay, start);
            for method in [
                HttpMethod::Get,
                HttpMethod::Post,
                HttpMethod::Put,
                HttpMethod::Delete,
            ] {
                assert_eq!(short_retry_delay(method, 0, &ApiError::Quota), None);
                assert_eq!(
                    cooldown.check(start + Duration::from_secs(seconds - 1)),
                    Err(ApiError::Quota)
                );
                // Blocked calls only check the existing deadline; they never extend it or
                // register pending work to replay when the deadline elapses.
                assert_eq!(cooldown.active.as_ref().unwrap().started, start);
            }
            assert_eq!(cooldown.check(start + Duration::from_secs(seconds)), Ok(()));
            assert!(cooldown.active.is_none());
        }
    }

    #[test]
    fn short_get_retry_consumes_only_its_explicit_delay_and_followup_is_still_bounded() {
        let start = Instant::now();
        let mut cooldown = Cooldown::default();
        let first = classify_status(429, Some(2));
        cooldown.record(&first, Some(2), start);
        assert_eq!(
            short_retry_delay(HttpMethod::Get, 0, &first),
            Some(Duration::from_secs(2))
        );
        assert!(cooldown.check(start + Duration::from_secs(1)).is_err());
        let second_attempt = start + Duration::from_secs(2);
        assert_eq!(cooldown.check(second_attempt), Ok(()));
        let second = classify_status(429, Some(3600));
        cooldown.record(&second, Some(3600), second_attempt);
        assert_eq!(short_retry_delay(HttpMethod::Get, 1, &second), None);
        for method in [HttpMethod::Post, HttpMethod::Put, HttpMethod::Delete] {
            assert_eq!(short_retry_delay(method, 0, &first), None);
            assert_eq!(short_retry_delay(method, 0, &second), None);
        }
        assert!(cooldown
            .check(second_attempt + Duration::from_secs(300))
            .is_err());
        assert_eq!(
            cooldown.check(second_attempt + Duration::from_secs(3600)),
            Ok(())
        );
    }
}
