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
    /// Exact original collection row, before unavailable rows or UI headers are filtered.
    PlayContextAt {
        uri: String,
        track: Option<String>,
        row_position: u32,
        position_ms: u32,
        shuffle: bool,
    },
    /// Complete explicit source, with an occurrence index rather than a URI lookup.
    PlayTracksAt {
        uris: Vec<String>,
        start_index: u32,
        position_ms: u32,
        shuffle: bool,
    },
    /// Owner resolves all /me/tracks pages. Never sends the library through IPC.
    PlayLiked {
        start: Option<String>,
        source_position: Option<u32>,
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
pub struct OwnedProcessGroup {
    pub pid: u32,
    pub pgid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTimingTelemetry {
    pub max_capture_gap_micros: u64,
    pub max_write_gap_micros: u64,
    pub max_dispatch_micros: u64,
    pub max_transfer_micros: u64,
    #[serde(default)]
    pub max_wait_micros: Option<u64>,
    #[serde(default)]
    pub max_iteration_gap_micros: Option<u64>,
    pub last_underflow_capture_gap_micros: u64,
    pub last_underflow_pending_frames: u64,
    /// Requested write credit, not server-side audio occupancy.
    pub last_underflow_write_credit_frames: u64,
    pub observed_underflows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTelemetry {
    #[serde(default)]
    pub timing: Option<AudioTimingTelemetry>,
    pub processed_frames: u64,
    pub peak_pending_frames: u64,
    pub estimated_latency_micros: u64,
    pub latency_valid: bool,
    pub dropped_frames: u64,
    /// None when an older daemon does not provide these diagnostics.
    #[serde(default)]
    pub capture_hole_frames: Option<u64>,
    #[serde(default)]
    pub playback_underflows: Option<u64>,
    pub worker_wakes: u64,
    pub resets: u64,
    pub fft_frames: u64,
    pub fft_compute_micros: u64,
    pub peak_fft_compute_micros: u64,
    pub recovery_attempts: u64,
    pub health: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub audio: Option<AudioTelemetry>,
    pub owned_process_groups: Vec<OwnedProcessGroup>,
    pub media_event_count: u64,
    pub last_media_command: Option<String>,
    pub spectrum_subscribers: u32,
    pub sequence: u64,
    pub session_id: String,
    pub auth_revision: u64,
    pub pid: u32,
    pub config_path: String,
    pub readiness: ServiceState,
    pub message: Option<String>,
    pub output_error: Option<String>,
    #[serde(default)]
    pub control_id: u64,
    #[serde(default)]
    pub control_pending: bool,
    #[serde(default)]
    pub control_outcome: Option<String>,
    #[serde(default)]
    pub control_error: Option<crate::ipc::protocol::ErrorCode>,
    /// Software payload coverage does not qualify native explicit-array offsets.
    #[serde(default)]
    pub load_requires_live_qualification: bool,
    #[serde(default)]
    pub queue_revision: u64,
    #[serde(default)]
    pub queue_previous_total: usize,
    #[serde(default)]
    pub queue_upcoming_total: usize,
    #[serde(default)]
    pub options_observed_revision: u64,
    #[serde(default)]
    pub volume_observed_revision: u64,
    #[serde(default)]
    pub queue_observed_revision: u64,
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
    /// Unmodified SDK anchor for the last local item, even while PCM is gated.
    #[serde(default)]
    pub observed_position: Option<PositionAnchor>,
    #[serde(default)]
    pub context_uri: Option<String>,
    /// Transient SDK actions; None means availability has not been observed.
    #[serde(default)]
    pub available_actions: Option<Vec<String>>,
    #[serde(default)]
    pub seek_forward_step_ms: Option<u64>,
    #[serde(default)]
    pub seek_backward_step_ms: Option<u64>,
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
            audio: None,
            owned_process_groups: Vec::new(),
            media_event_count: 0,
            last_media_command: None,
            spectrum_subscribers: 0,
            sequence: 0,
            session_id: format!("{:016x}", rand::random::<u64>()),
            auth_revision: 0,
            pid: std::process::id(),
            config_path,
            readiness: ServiceState::Starting,
            message: None,
            output_error: None,
            control_id: 0,
            control_pending: false,
            control_outcome: None,
            control_error: None,
            load_requires_live_qualification: false,
            queue_revision: 0,
            queue_previous_total: 0,
            queue_upcoming_total: 0,
            options_observed_revision: 0,
            volume_observed_revision: 0,
            queue_observed_revision: 0,
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
            observed_position: None,
            context_uri: None,
            available_actions: None,
            seek_forward_step_ms: None,
            seek_backward_step_ms: None,
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
    fn gap_telemetry_distinguishes_unsupported_from_zero_and_serializes_numbers() {
        let mut wire = serde_json::json!({
            "processed_frames": 100, "peak_pending_frames": 128,
            "estimated_latency_micros": 0, "latency_valid": false,
            "dropped_frames": 0, "worker_wakes": 2, "resets": 0,
            "fft_frames": 0, "fft_compute_micros": 0, "peak_fft_compute_micros": 0,
            "recovery_attempts": 0, "health": 1
        });
        let legacy: AudioTelemetry = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(legacy.capture_hole_frames, None);
        assert_eq!(legacy.playback_underflows, None);
        assert!(legacy.timing.is_none());
        wire["capture_hole_frames"] = 0.into();
        wire["playback_underflows"] = 7.into();
        wire["timing"] = serde_json::json!({
            "max_capture_gap_micros": 5000, "max_write_gap_micros": 3000,
            "max_dispatch_micros": 120, "max_transfer_micros": 80,
            "last_underflow_capture_gap_micros": 3000,
            "last_underflow_pending_frames": 64,
            "last_underflow_write_credit_frames": 128, "observed_underflows": 7
        });
        let current: AudioTelemetry = serde_json::from_value(wire).unwrap();
        assert_eq!(current.capture_hole_frames, Some(0));
        assert_eq!(current.playback_underflows, Some(7));
        let encoded = serde_json::to_value(current).unwrap();
        assert_eq!(encoded["capture_hole_frames"].as_u64(), Some(0));
        assert_eq!(encoded["playback_underflows"].as_u64(), Some(7));
        assert_eq!(
            encoded["timing"]["max_capture_gap_micros"].as_u64(),
            Some(5000)
        );
        assert_eq!(
            encoded["timing"]["last_underflow_write_credit_frames"].as_u64(),
            Some(128)
        );
    }
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
