use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{MetadataStatus, QueueSnapshot, TrackMetadata};
use crate::audio::EqualizerSettings;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    #[default]
    Off,
    One,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    SetupRequired,
    Starting,
    Ready,
    Degraded,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    Starting,
    Playing,
    Paused,
    Buffering,
    Stopped,
    Disconnected,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PositionAnchor {
    pub position_ms: u64,
    pub timestamp_ms: u64,
    pub speed: f64,
}

impl Default for PositionAnchor {
    fn default() -> Self {
        Self {
            position_ms: 0,
            timestamp_ms: now_ms(),
            speed: 0.0,
        }
    }
}

impl PositionAnchor {
    pub fn at(&self, timestamp_ms: u64, duration_ms: Option<u64>) -> u64 {
        let elapsed = timestamp_ms.saturating_sub(self.timestamp_ms);
        let advance = if self.speed.is_finite() && self.speed > 0.0 {
            (elapsed as f64 * self.speed).min(u64::MAX as f64) as u64
        } else {
            0
        };
        let position = self.position_ms.saturating_add(advance);
        duration_ms.map_or(position, |duration| position.min(duration))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum PlayerCommand {
    PlayContext {
        uri: String,
        track: Option<String>,
        position_ms: u32,
        shuffle: bool,
    },
    PlayTracks {
        uris: Vec<String>,
        start: Option<String>,
        position_ms: u32,
        shuffle: bool,
    },
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek {
        position_ms: u32,
    },
    Volume {
        percent: u8,
    },
    Shuffle {
        enabled: bool,
    },
    Repeat {
        mode: RepeatMode,
    },
    Equalizer {
        settings: EqualizerSettings,
    },
    Queue,
    Add {
        uri: String,
    },
}

/// This is a public read model, never a config, OAuth cache or SDK protocol body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub sequence: u64,
    pub session_id: String,
    pub auth_revision: u64,
    pub pid: u32,
    pub config_path: String,
    pub readiness: ServiceState,
    pub message: Option<String>,
    pub web_api_ready: bool,
    pub catalog_available: bool,
    pub soloist_ready: bool,
    pub native_media_ready: bool,
    pub owned_device_verified: bool,
    pub locally_active: bool,
    pub configured_device_name: String,
    pub effective_device_name: String,
    pub rename_requires_restart: bool,
    pub playback: PlaybackStatus,
    pub position: PositionAnchor,
    pub track: Option<TrackMetadata>,
    pub metadata_status: MetadataStatus,
    pub queue: QueueSnapshot,
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub equalizer: EqualizerSettings,
    pub spectrum: Option<Vec<f32>>,
}

impl Snapshot {
    pub fn starting(config_path: String, device_name: String) -> Self {
        Self {
            sequence: 0,
            session_id: format!("{:016x}", rand::random::<u64>()),
            auth_revision: 0,
            pid: std::process::id(),
            config_path,
            readiness: ServiceState::Starting,
            message: None,
            web_api_ready: false,
            catalog_available: false,
            soloist_ready: false,
            native_media_ready: false,
            owned_device_verified: false,
            locally_active: false,
            configured_device_name: device_name.clone(),
            effective_device_name: device_name,
            rename_requires_restart: false,
            playback: PlaybackStatus::Starting,
            position: PositionAnchor::default(),
            track: None,
            metadata_status: MetadataStatus::Unavailable,
            queue: QueueSnapshot::default(),
            volume: 25,
            shuffle: false,
            repeat: RepeatMode::Off,
            equalizer: EqualizerSettings::default(),
            spectrum: None,
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn anchors_freeze_clamp_and_tolerate_clock_changes() {
        let mut anchor = PositionAnchor {
            position_ms: 500,
            timestamp_ms: 100,
            speed: 1.0,
        };
        assert_eq!(anchor.at(200, None), 600);
        assert_eq!(anchor.at(0, None), 500);
        assert_eq!(anchor.at(200, Some(550)), 550);
        anchor.speed = 0.0;
        assert_eq!(anchor.at(u64::MAX, None), 500);
    }
}
