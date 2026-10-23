//! A single authorization owner's disposable response cache. No disk or global state.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::Value;

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 1024 * 1024;
// Reserve the maximum queue metadata up front in the budget, even after entries are evicted.
const PAYLOAD_BUDGET: usize = MAX_BYTES - MAX_ENTRIES * std::mem::size_of::<Entry>();
const LIBRARY_TTL: Duration = Duration::from_secs(15);
const CATALOG_TTL: Duration = Duration::from_secs(300);

struct Entry {
    path: Box<str>,
    // Store serialized bytes so the retained payload budget is exact, rather than estimating
    // the much larger allocation graph of arbitrary JSON objects.
    bytes: Box<[u8]>,
    expires: Instant,
}

impl Entry {
    fn size(&self) -> usize {
        self.path.len() + self.bytes.len()
    }
}

#[derive(Default)]
pub(super) struct ResponseCache {
    entries: VecDeque<Entry>,
    bytes: usize,
}

impl ResponseCache {
    pub(super) fn contains_fresh(&mut self, path: &str, now: Instant) -> bool {
        self.expire(now);
        self.entries.iter().any(|entry| entry.path.as_ref() == path)
    }

    pub(super) fn get(&mut self, path: &str, now: Instant) -> Option<Value> {
        self.expire(now);
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.path.as_ref() == path)?;
        serde_json::from_slice(&entry.bytes).ok()
    }

    pub(super) fn insert(&mut self, path: &str, value: &Value, now: Instant) {
        self.expire(now);
        let Some(ttl) = ttl(path) else { return };
        if value.is_null() {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(value) else {
            return;
        };
        let size = path.len() + bytes.len();
        if size > MAX_ENTRY_BYTES {
            return;
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.path.as_ref() == path)
        {
            self.remove(index);
        }
        while self.entries.len() >= MAX_ENTRIES || self.bytes + size > PAYLOAD_BUDGET {
            self.remove(0);
        }
        self.bytes += size;
        self.entries.push_back(Entry {
            path: path.into(),
            bytes: bytes.into_boxed_slice(),
            expires: now + ttl,
        });
    }

    /// Any mutation attempt may affect library membership, playlist details and search results.
    /// Conservatively clear the small owner cache even if the attempt fails or is uncertain.
    pub(super) fn invalidate(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn expire(&mut self, now: Instant) {
        for index in (0..self.entries.len()).rev() {
            if self.entries[index].expires <= now {
                self.remove(index);
            }
        }
    }

    fn remove(&mut self, index: usize) {
        if let Some(entry) = self.entries.remove(index) {
            self.bytes -= entry.size();
        }
    }
}

fn ttl(path: &str) -> Option<Duration> {
    let route = path.split('?').next()?;
    // Opt in only known catalog/library families. Player, profile and authorization data
    // are deliberately absent, including recently-played and all player subroutes.
    if matches!(
        route,
        "/v1/me/albums"
            | "/v1/me/tracks"
            | "/v1/me/episodes"
            | "/v1/me/shows"
            | "/v1/me/audiobooks"
            | "/v1/me/playlists"
            | "/v1/me/following"
    ) || route.starts_with("/v1/me/top/")
        || route.starts_with("/v1/me/albums/")
        || route.starts_with("/v1/me/tracks/")
        || route.starts_with("/v1/me/episodes/")
        || route.starts_with("/v1/me/shows/")
        || route.starts_with("/v1/me/following/")
    {
        return Some(LIBRARY_TTL);
    }
    let family = route.strip_prefix("/v1/")?.split('/').next()?;
    match family {
        // Revision-before/after and content pages must observe the provider,
        // never a mixture of cached playlist revisions/pages.
        "playlists" => None,
        "users" => Some(LIBRARY_TTL),
        "albums" | "artists" | "audiobooks" | "browse" | "chapters" | "episodes" | "markets"
        | "recommendations" | "search" | "shows" | "tracks" | "audio-features"
        | "audio-analysis" => Some(CATALOG_TTL),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn count_byte_and_entry_budgets_evict_without_unbounded_retention() {
        let now = Instant::now();
        let mut cache = ResponseCache::default();
        for i in 0..200 {
            cache.insert(&format!("/v1/tracks/{i}"), &json!({"id":i}), now);
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(cache.get("/v1/tracks/0", now).is_none());
        let large = json!("x".repeat(800 * 1024));
        for i in 0..40 {
            cache.insert(&format!("/v1/albums/{i}"), &large, now);
            assert!(cache.bytes <= MAX_BYTES);
            assert!(cache.entries.len() <= MAX_ENTRIES);
        }
        assert!(cache.get("/v1/albums/0", now).is_none());
        assert_eq!(cache.get("/v1/albums/39", now), Some(large));
        cache.insert(
            "/v1/tracks/oversized",
            &json!("x".repeat(MAX_ENTRY_BYTES)),
            now,
        );
        assert!(cache.get("/v1/tracks/oversized", now).is_none());
        assert_eq!(
            cache.bytes,
            cache.entries.iter().map(Entry::size).sum::<usize>()
        );
    }

    #[test]
    fn library_expires_sooner_and_no_stale_result_survives_expiry_or_invalidation() {
        let now = Instant::now();
        let mut cache = ResponseCache::default();
        cache.insert("/v1/me/tracks", &json!({"items":[]}), now);
        cache.insert("/v1/albums/a", &json!({"id":"a"}), now);
        assert!(cache.get("/v1/me/tracks", now + LIBRARY_TTL).is_none());
        assert!(cache.get("/v1/albums/a", now + LIBRARY_TTL).is_some());
        assert!(cache.get("/v1/albums/a", now + CATALOG_TTL).is_none());
        assert_eq!(cache.bytes, 0);
        cache.insert("/v1/me/tracks", &json!({"items":[]}), now);
        cache.insert("/v1/albums/a", &json!({"id":"a"}), now);
        cache.invalidate();
        assert!(cache.get("/v1/me/tracks", now).is_none());
        assert!(cache.get("/v1/albums/a", now).is_none());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn player_auth_null_and_unknown_routes_are_never_cached() {
        let now = Instant::now();
        let mut cache = ResponseCache::default();
        for path in [
            "/v1/me",
            "/v1/me/player",
            "/v1/me/player/devices",
            "/v1/me/player/recently-played",
            "/v1/playlists/p?fields=snapshot_id",
            "/v1/playlists/p/items?offset=0",
            "/api/token",
            "/v1/unknown",
        ] {
            cache.insert(path, &json!({"secret":"not retained"}), now);
            assert!(cache.get(path, now).is_none());
        }
        cache.insert("/v1/albums/a", &Value::Null, now);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn replacement_accounts_for_bytes_and_query_is_part_of_key() {
        let now = Instant::now();
        let mut cache = ResponseCache::default();
        cache.insert("/v1/me/tracks?offset=0", &json!([1, 2]), now);
        cache.insert("/v1/me/tracks?offset=0", &json!([3]), now);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, cache.entries[0].size());
        assert_eq!(cache.get("/v1/me/tracks?offset=0", now), Some(json!([3])));
        assert!(cache.get("/v1/me/tracks?offset=1", now).is_none());
    }

    #[test]
    fn probing_before_token_maintenance_does_not_extend_freshness() {
        let now = Instant::now();
        let mut cache = ResponseCache::default();
        cache.insert("/v1/me/tracks", &json!({"items":[]}), now);
        assert!(cache.contains_fresh("/v1/me/tracks", now + LIBRARY_TTL - Duration::from_secs(1)));
        // Token maintenance can take time; the actual read must recheck expiry afterward.
        assert!(cache.get("/v1/me/tracks", now + LIBRARY_TTL).is_none());
        assert!(!cache.contains_fresh("/v1/me/tracks", now + LIBRARY_TTL));
    }
}
