//! Application-owned state shared by the daemon and its clients.

pub mod metadata;
pub mod playback;
pub mod queue;

pub use metadata::{ItemKind, MetadataStatus, TrackMetadata};
pub use playback::{
    PlaybackStatus, PlayerCommand, PositionAnchor, RepeatMode, ServiceState, Snapshot,
};
pub use queue::{QueueEntry, QueueSnapshot, QueueSource};
