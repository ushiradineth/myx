//! Search pages retain provider-bound visibility, not an arbitrary UI result cap.
use super::*;
use crate::*;

// Provider bounds, not application collection caps. Spotify Search reference,
// retrieved 2026-10-23: limit 0..10 and offset 0..1000 inclusive.
// https://developer.spotify.com/documentation/web-api/reference/search
const SEARCH_PAGE_SIZE: u64 = 10;
const MAX_SEARCH_OFFSET: u64 = 1000;

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
        let values = collect_search_pages(&mut fetch, path, query, kind, container);
        let values = match values {
            Ok(pages) => {
                successful_groups += 1;
                if let Some(notice) = pages.notice {
                    access_notices.push(format!("{label}: {notice}"));
                }
                pages.items
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
    let notice = (!access_notices.is_empty())
        .then(|| format!("partial search results: {}", access_notices.join("; ")));
    Ok(SearchResults { rows, notice })
}

struct SearchPages {
    items: Vec<serde_json::Value>,
    notice: Option<&'static str>,
}

// Search documents next=null as EOF and limit as a maximum, but no snapshot
// guarantee. Keep returned rankings; totals are visibility hints, not invariants.
// https://developer.spotify.com/documentation/web-api/reference/search
fn collect_search_pages(
    fetch: &mut impl FnMut(&str) -> FetchResult<serde_json::Value>,
    mut path: String,
    query: &str,
    kind: &str,
    container: &str,
) -> FetchResult<SearchPages> {
    let mut offset = 0;
    let mut items = Vec::new();
    let mut previous_total = None;
    let mut incomplete = false;
    let mut offset_limited = false;
    loop {
        let response = fetch(&path)?;
        let page = response.get(container).ok_or(ApiError::InvalidResponse)?;
        let page_items = page["items"].as_array().ok_or(ApiError::InvalidResponse)?;
        let total = page["total"].as_u64().ok_or(ApiError::InvalidResponse)?;
        if page["offset"].as_u64() != Some(offset) || page_items.len() as u64 > SEARCH_PAGE_SIZE {
            return Err(ApiError::InvalidResponse);
        }
        incomplete |= previous_total.is_some_and(|previous| previous != total);
        previous_total = Some(total);
        items.extend(page_items.iter().cloned());
        match page.get("next") {
            Some(serde_json::Value::Null) => {
                incomplete |= items.len() as u64 != total;
                break;
            }
            Some(serde_json::Value::String(next)) => {
                let (next_path, next_offset) = search_continuation(next, query, kind)?;
                // Strictly increasing offsets reject cycles regardless of URL spelling.
                if next_offset <= offset {
                    return Err(ApiError::InvalidResponse);
                }
                if next_offset > MAX_SEARCH_OFFSET {
                    offset_limited = true;
                    break;
                }
                incomplete |= next_offset != offset + page_items.len() as u64;
                if page_items.is_empty() {
                    incomplete = true;
                    break;
                }
                path = next_path;
                offset = next_offset;
            }
            _ => return Err(ApiError::InvalidResponse),
        }
    }
    Ok(SearchPages {
        items,
        notice: if offset_limited {
            Some("Spotify's maximum search offset reached; narrow the query")
        } else if incomplete {
            Some("Spotify search totals changed or pages were incomplete; narrow the query")
        } else {
            None
        },
    })
}

fn search_continuation(link: &str, query: &str, kind: &str) -> FetchResult<(String, u64)> {
    // Check raw spelling before URL parsing can normalize traversal or credentials.
    let path = link.strip_prefix("https://api.spotify.com").unwrap_or(link);
    if path.len() > 16384
        || !path.starts_with("/v1/search?")
        || path.contains(['#', '\\'])
        || path.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return Err(ApiError::InvalidRequest);
    }
    let url = reqwest::Url::parse(&format!("https://api.spotify.com{path}"))
        .map_err(|_| ApiError::InvalidRequest)?;
    let mut params = std::collections::HashMap::new();
    for (key, value) in url.query_pairs() {
        if params.insert(key, value).is_some() {
            return Err(ApiError::InvalidRequest);
        }
    }
    if params.get("q").map(|value| value.as_ref()) != Some(query)
        || params.get("type").map(|value| value.as_ref()) != Some(kind)
        || params
            .get("limit")
            .and_then(|value| value.parse::<u64>().ok())
            .is_none_or(|limit| limit == 0 || limit > SEARCH_PAGE_SIZE)
    {
        return Err(ApiError::InvalidRequest);
    }
    let offset = params
        .get("offset")
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(ApiError::InvalidRequest)?;
    Ok((path.to_owned(), offset))
}
