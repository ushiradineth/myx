//! Catalog metadata shared by the daemon and its clients, never decoder state.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Track,
    Episode,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataStatus {
    Loading,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackMetadata {
    pub kind: ItemKind,
    pub uri: String,
    pub title: Option<String>,
    pub artists: Option<Vec<String>>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub artwork_url: Option<String>,
    pub spotify_url: Option<String>,
}

impl TrackMetadata {
    /// An observed identifier must not masquerade as resolved catalog metadata.
    pub fn pending(kind: ItemKind, uri: String) -> Self {
        Self {
            kind,
            uri,
            title: None,
            artists: None,
            album: None,
            duration_ms: None,
            artwork_url: None,
            spotify_url: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_identifier_is_not_a_title_or_guessed_metadata() {
        let metadata = TrackMetadata::pending(ItemKind::Episode, "spotify:episode:pending".into());
        assert_eq!(metadata.kind, ItemKind::Episode);
        assert_eq!(metadata.title, None);
        assert_eq!(metadata.artists, None);
        assert_eq!(metadata.duration_ms, None);
        let value = serde_json::to_value(metadata).unwrap();
        assert!(value["title"].is_null());
        assert_eq!(value["kind"], "episode");
    }

    #[test]
    fn metadata_preserves_unicode_quotes_and_nullable_fields() {
        let mut metadata = TrackMetadata::pending(ItemKind::Track, "spotify:track:pending".into());
        metadata.title = Some("夜の\"音楽\"\n".into());
        metadata.artists = Some(vec!["Björk".into()]);
        let encoded = serde_json::to_vec(&metadata).unwrap();
        assert_eq!(
            serde_json::from_slice::<TrackMetadata>(&encoded).unwrap(),
            metadata
        );
    }
}
