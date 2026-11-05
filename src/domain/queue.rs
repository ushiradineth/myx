use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueSource {
    Context,
    Queue,
    Autoplay,
    Requested,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub uid: String,
    pub source: QueueSource,
    pub uri: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshot {
    /// Native SDK order: most recently played first. Duplicate URIs/UIDs remain distinct entries.
    pub previous: Vec<QueueEntry>,
    /// Native SDK order: next to play first, never a logical request/source list.
    pub upcoming: Vec<QueueEntry>,
    /// SDK availability is not evidence that an entire catalog context was returned.
    pub complete_catalog_context: bool,
}
