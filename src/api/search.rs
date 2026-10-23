//! Search pages retain provider-bound visibility, not an arbitrary UI result cap.
use super::*;
use crate::*;

// Provider bounds, not application collection caps. Spotify Search reference,
// retrieved 2026-10-23: limit 0..10 and offset 0..1000 inclusive.
// https://developer.spotify.com/documentation/web-api/reference/search
const SEARCH_PAGE_SIZE: u64 = 10;
const MAX_SEARCH_OFFSET: u64 = 1000;
const REACHABLE_SEARCH_ITEMS: u64 = MAX_SEARCH_OFFSET + SEARCH_PAGE_SIZE;

pub(crate) struct SearchResults {
    pub(crate) rows: Vec<LibItem>,
    pub(crate) notice: Option<String>,
}

pub(crate) fn spawn_search(
    webapi: Arc<Mutex<WebApi>>,
    query: String,
    id: u64,
    tx: flume::Sender<Reply<SearchResults>>,
) {
    tokio::task::spawn_blocking(move || {
        let result =
            connection_of(&webapi).and_then(|connection| search_blocking(&connection, &query));
        let _ = tx.send(Reply { id, result });
    });
}

pub(crate) fn search_blocking(connection: &Connection, query: &str) -> FetchResult<SearchResults> {
    search_with(|path| get_json(connection, path), query)
}

pub(crate) fn search_with(
    mut fetch: impl FnMut(&str) -> FetchResult<serde_json::Value>,
    query: &str,
) -> FetchResult<SearchResults> {
    let mut rows = Vec::new();
    let mut partial = false;
    let mut access_notices = Vec::new();
    let mut successful_groups = 0;
    let mut first_access_error = None;
    for (kind, container, label) in [
        ("track", "tracks", "Songs"),
        ("artist", "artists", "Artists"),
        ("album", "albums", "Albums"),
        ("playlist", "playlists", "Playlists"),
    ] {
        let path = format!(
            "{API}/search?q={}&type={kind}&limit={SEARCH_PAGE_SIZE}",
            urlencode(query)
        );
        let mut original_total = None;
        let values = myx::webapi::catalog::collect_pages(
            |path| {
                let mut value = fetch(path)?;
                let page = value.get_mut(container).ok_or(ApiError::InvalidResponse)?;
                let total = page["total"].as_u64().ok_or(ApiError::InvalidResponse)?;
                if original_total.is_some_and(|expected| expected != total) {
                    return Err(ApiError::CollectionChanged);
                }
                original_total = Some(total);
                // Spotify documents a maximum search offset of 1000. With its
                // ten-result page limit, the final reachable page ends at 1010.
                // Report that actual provider restriction rather than claiming all.
                if total > REACHABLE_SEARCH_ITEMS {
                    partial = true;
                    page["total"] = serde_json::json!(REACHABLE_SEARCH_ITEMS);
                    let offset = page["offset"].as_u64().ok_or(ApiError::InvalidResponse)?;
                    if offset >= MAX_SEARCH_OFFSET {
                        page["next"] = serde_json::Value::Null;
                    }
                }
                Ok(value)
            },
            &path,
            Some(container),
        );
        let values = match values {
            Ok(values) => {
                successful_groups += 1;
                values
            }
            Err(error @ (ApiError::Restricted | ApiError::NotFound)) => {
                access_notices.push(format!("{label}: {error}"));
                first_access_error.get_or_insert(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let group: Vec<_> = values
            .iter()
            .filter_map(if kind == "track" {
                track_row
            } else {
                context_row
            })
            .collect();
        if !group.is_empty() {
            rows.push(LibItem::header(label));
            rows.extend(group);
        }
    }
    if successful_groups == 0 {
        return Err(first_access_error.unwrap_or(ApiError::InvalidResponse));
    }
    if partial {
        access_notices.push("Spotify's maximum search offset reached; narrow the query".into());
    }
    let notice = (!access_notices.is_empty())
        .then(|| format!("partial search results: {}", access_notices.join("; ")));
    Ok(SearchResults { rows, notice })
}
