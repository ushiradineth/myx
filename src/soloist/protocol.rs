//! Safe SDK observations, not authoritative catalog metadata or raw SDK payloads.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedMetadata {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u32,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PlayerEvent {
    ProcessGroups(Vec<(u32, u32)>),
    /// Client-side latest-value read model; SDK transports never synthesize this.
    ReadModel(Box<crate::domain::Snapshot>),
    TrackChanged {
        uri: String,
    },
    Playing {
        uri: String,
        position_ms: u32,
    },
    Paused {
        uri: String,
        position_ms: u32,
    },
    Buffering {
        uri: String,
        position_ms: u32,
    },
    LocallyInactive,
    /// Native availability refreshed with get_queue(limit: 0), never catalog completeness.
    NativeQueue(crate::domain::QueueSnapshot),
    Options {
        shuffle: bool,
        repeat: crate::domain::RepeatMode,
    },
    Volume {
        percent: u8,
    },
    Context {
        uri: Option<String>,
    },
    PositionAnchor {
        uri: String,
        anchor: crate::domain::PositionAnchor,
    },
    AvailableActions {
        actions: Vec<String>,
        seek_forward_step_ms: Option<u64>,
        seek_backward_step_ms: Option<u64>,
    },
    Stopped,
    Reconnecting,
    Reconnected,
    EndOfTrack {
        uri: String,
    },
    PositionCorrection {
        uri: String,
        position_ms: u32,
    },
    Metadata(ObservedMetadata),
    /// Legacy logical request overlay, not a native queue/UID observation.
    QueueChanged {
        entries: Vec<(String, String)>,
    },
    BackendStatus {
        message: String,
    },
    Authentication {
        logged_in: bool,
        is_active: bool,
    },
}
