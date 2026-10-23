//! All-page collection with injected owner requests; no independent authorization or transport.
use std::collections::HashSet;

use serde_json::Value;

use super::client::validate_request;
use super::{ApiError, HttpMethod};

/// Collect root paging objects, or a named paging container such as `artists` or `tracks`.
///
/// Items are retained verbatim, including null/unavailable entries and duplicates. A later-page
/// error discards the entire result. For album details, request `/albums/{id}/tracks` explicitly:
/// the album detail's nested first page is not a complete collection.
pub fn collect_pages(
    mut fetch: impl FnMut(&str) -> Result<serde_json::Value, ApiError>,
    initial_path: &str,
    container: Option<&str>,
) -> Result<Vec<Value>, ApiError> {
    let mut path = paging_path(initial_path)?;
    let mut visited = HashSet::new();
    let mut items = Vec::new();
    let mut expected = None;
    loop {
        if !visited.insert(path.clone()) {
            return Err(ApiError::CollectionChanged);
        }
        let response = fetch(&path)?;
        let page = match container {
            Some(name) => response.get(name).ok_or(ApiError::InvalidResponse)?,
            None => &response,
        };
        let page_items = page
            .get("items")
            .and_then(Value::as_array)
            .ok_or(ApiError::InvalidResponse)?;
        let total = optional_total(page)?;
        let snapshot = optional_snapshot(page)?;
        let root_snapshot = optional_snapshot(&response)?;
        let state = (total, snapshot, root_snapshot);
        if let Some(expected) = &expected {
            if expected != &state {
                return Err(ApiError::CollectionChanged);
            }
        } else {
            expected = Some(state);
        }
        items.extend(page_items.iter().cloned());
        if total.is_some_and(|total| items.len() as u64 > total) {
            return Err(ApiError::CollectionChanged);
        }
        match page.get("next") {
            Some(Value::Null) => {
                if total.is_some_and(|total| items.len() as u64 != total) {
                    return Err(ApiError::CollectionChanged);
                }
                return Ok(items);
            }
            Some(Value::String(next)) => {
                path = paging_path(next)?;
                if page_items.is_empty() {
                    // A continuation that makes no progress must not be presented as complete.
                    return Err(ApiError::CollectionChanged);
                }
            }
            _ => return Err(ApiError::InvalidResponse),
        }
    }
}

fn optional_total(page: &Value) -> Result<Option<u64>, ApiError> {
    match page.get("total") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(ApiError::InvalidResponse),
    }
}

fn optional_snapshot(page: &Value) -> Result<Option<String>, ApiError> {
    match page.get("snapshot_id") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or(ApiError::InvalidResponse),
    }
}

fn paging_path(link: &str) -> Result<String, ApiError> {
    // Inspect the original spelling before URL parsing can normalize traversal or credentials.
    // Only the exact HTTPS origin (no alternate ports) and /v1/ root-relative links are accepted.
    let path = link.strip_prefix("https://api.spotify.com").unwrap_or(link);
    let url = validate_request(HttpMethod::Get, path, None, None)?;
    let mut canonical = url.path().to_owned();
    if let Some(query) = url.query() {
        canonical.push('?');
        canonical.push_str(query);
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn follows_more_than_five_thousand_items_without_caps() {
        let mut calls = 0;
        let result = collect_pages(|path| {
            let offset = path.rsplit('=').next().unwrap().parse::<usize>().unwrap();
            calls += 1;
            let end = (offset + 100).min(5107);
            Ok(json!({
                "items": (offset..end).map(|id| json!({"id":id})).collect::<Vec<_>>(),
                "total": 5107,
                "next": if end < 5107 { Some(format!("https://api.spotify.com/v1/me/tracks?offset={end}")) } else { None }
            }))
        }, "/v1/me/tracks?offset=0", None).unwrap();
        assert_eq!(result.len(), 5107);
        assert_eq!(result.last(), Some(&json!({"id":5106})));
        assert_eq!(calls, 52);
    }

    #[test]
    fn retains_order_duplicates_null_and_unavailable_entries() {
        let unavailable = json!({"track":null,"is_local":false});
        let mut calls = 0;
        let result = collect_pages(
            |_| {
                calls += 1;
                Ok(if calls == 1 {
                    json!({"items":[{"id":"a"},null],"next":"/v1/me/tracks?offset=2","total":4})
                } else {
                    json!({"items":[{"id":"a"},unavailable],"next":null,"total":4})
                })
            },
            "/v1/me/tracks",
            None,
        )
        .unwrap();
        assert_eq!(
            result,
            vec![
                json!({"id":"a"}),
                Value::Null,
                json!({"id":"a"}),
                unavailable
            ]
        );
    }

    #[test]
    fn root_and_named_paging_support_library_album_followed_and_search_routes() {
        for (path, container) in [
            ("/v1/me/playlists", None),
            ("/v1/me/albums", None),
            ("/v1/albums/id/tracks", None),
            ("/v1/me/following?type=artist", Some("artists")),
            ("/v1/search?q=test&type=track", Some("tracks")),
        ] {
            let mut calls = 0;
            let result = collect_pages(
                |requested| {
                    calls += 1;
                    let next = if calls == 1 {
                        Some(format!(
                            "{path}{}page=2",
                            if path.contains('?') { '&' } else { '?' }
                        ))
                    } else {
                        None
                    };
                    assert!(requested.starts_with("/v1/"));
                    let page = json!({"items":[calls],"next":next});
                    Ok(if let Some(name) = container {
                        json!({(name):page})
                    } else {
                        page
                    })
                },
                path,
                container,
            )
            .unwrap();
            assert_eq!(result, vec![json!(1), json!(2)]);
        }
    }

    #[test]
    fn later_page_errors_propagate_without_partial_success_or_retries() {
        for error in [
            ApiError::Restricted,
            ApiError::Quota,
            ApiError::Unauthorized,
            ApiError::RateLimited {
                retry_after_secs: Some(87),
            },
            ApiError::Offline,
            ApiError::Server,
        ] {
            let mut calls = 0;
            let result = collect_pages(
                |_| {
                    calls += 1;
                    if calls == 1 {
                        Ok(json!({"items":[1],"next":"/v1/me/tracks?offset=1","total":2}))
                    } else {
                        Err(error.clone())
                    }
                },
                "/v1/me/tracks",
                None,
            );
            assert_eq!(result, Err(error));
            assert_eq!(calls, 2);
        }
    }

    #[test]
    fn rejects_malformed_off_origin_credentials_fragments_and_traversal_links() {
        for next in [
            "https://evil.test/v1/me/tracks",
            "http://api.spotify.com/v1/me/tracks",
            "https://api.spotify.com.evil.test/v1/me/tracks",
            "https://user:password@api.spotify.com/v1/me/tracks",
            "https://api.spotify.com:443/v1/me/tracks",
            "//api.spotify.com/v1/me/tracks",
            "/v1/me/../tracks",
            "/v1/me/%2e%2e/tracks",
            "/v1/me/tracks#fragment",
            "/v1/me\\tracks",
            "/v1/me//tracks",
            "/v1/me/tracks\n",
            "?offset=1",
            "",
            "/v1/me/tracks with spaces",
            "/api/token",
        ] {
            let mut calls = 0;
            assert_eq!(
                collect_pages(
                    |_| {
                        calls += 1;
                        Ok(json!({"items":[1],"next":next}))
                    },
                    "/v1/me/tracks",
                    None
                ),
                Err(ApiError::InvalidRequest),
                "{next}"
            );
            assert_eq!(calls, 1);
        }
        let mut calls = 0;
        assert_eq!(
            collect_pages(
                |_| {
                    calls += 1;
                    Ok(Value::Null)
                },
                "https://evil.test/v1/me",
                None
            ),
            Err(ApiError::InvalidRequest)
        );
        assert_eq!(calls, 0);
    }

    #[test]
    fn cycles_are_typed_failure_including_absolute_relative_aliases() {
        let mut calls = 0;
        assert_eq!(
            collect_pages(
                |_| {
                    calls += 1;
                    Ok(json!({"items":[1],"next":"https://api.spotify.com/v1/me/tracks"}))
                },
                "/v1/me/tracks",
                None
            ),
            Err(ApiError::CollectionChanged)
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn changed_or_missing_totals_snapshots_and_incomplete_terminal_fail() {
        for second in [
            json!({"items":[2],"next":null,"total":3,"snapshot_id":"a"}),
            json!({"items":[2],"next":null,"total":2,"snapshot_id":"b"}),
            json!({"items":[2],"next":null,"snapshot_id":"a"}),
            json!({"items":[2],"next":null,"total":2}),
            json!({"items":[],"next":null,"total":2,"snapshot_id":"a"}),
            json!({"items":[2,3],"next":null,"total":2,"snapshot_id":"a"}),
        ] {
            let mut calls = 0;
            assert_eq!(
                collect_pages(
                    |_| {
                        calls += 1;
                        Ok(if calls == 1 {
                            json!({"items":[1],"next":"/v1/playlists/id/tracks?offset=1","total":2,"snapshot_id":"a"})
                        } else {
                            second.clone()
                        })
                    },
                    "/v1/playlists/id/tracks",
                    None
                ),
                Err(ApiError::CollectionChanged)
            );
        }
    }

    #[test]
    fn root_snapshot_is_checked_even_for_named_containers() {
        let mut calls = 0;
        assert_eq!(
            collect_pages(
                |_| {
                    calls += 1;
                    Ok(json!({"snapshot_id": calls.to_string(), "artists":{
                        "items":[calls],"next": if calls == 1 { Some("/v1/me/following?after=a") } else { None }
                    }}))
                },
                "/v1/me/following",
                Some("artists")
            ),
            Err(ApiError::CollectionChanged)
        );
    }

    #[test]
    fn malformed_paging_is_not_an_empty_collection() {
        for page in [
            json!({}),
            Value::Null,
            json!({"items":[],"next":3}),
            json!({"items":null,"next":null}),
            json!({"items":[],"next":null,"total":-1}),
            json!({"items":[],"next":null,"snapshot_id":17}),
        ] {
            assert_eq!(
                collect_pages(|_| Ok(page.clone()), "/v1/me/tracks", None),
                Err(ApiError::InvalidResponse)
            );
        }
        assert_eq!(
            collect_pages(
                |_| Ok(json!({"items":[],"next":null})),
                "/v1/me/following",
                Some("artists")
            ),
            Err(ApiError::InvalidResponse)
        );
        assert_eq!(
            collect_pages(
                |_| Ok(json!({"items":[],"next":"/v1/me/tracks?offset=1"})),
                "/v1/me/tracks",
                None
            ),
            Err(ApiError::CollectionChanged)
        );
        assert_eq!(
            collect_pages(
                |_| Ok(json!({"items":[],"next":null,"total":0})),
                "/v1/me/tracks",
                None
            ),
            Ok(vec![])
        );
    }
}
