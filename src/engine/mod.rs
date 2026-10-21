//! Backend-neutral playback facade. Keep provider wiring in its own module so
//! upstream librespot changes remain isolated from the optional Soloist adapter.

pub mod auth;
mod normal;
#[cfg(feature = "soloist")]
mod soloist;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use librespot_core::authentication::Credentials;

use crate::audio::{EqualizerSettings, VisBands};
use crate::config::BackendKind;

pub use normal::{credentials, needs_authorization, radio_tracks};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMetadata {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u32,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    TrackChanged { uri: String },
    Playing { uri: String, position_ms: u32 },
    Paused { uri: String, position_ms: u32 },
    Stopped,
    Reconnecting,
    Reconnected,
    EndOfTrack { uri: String },
    PositionCorrection { uri: String, position_ms: u32 },
    Metadata(TrackMetadata),
    QueueChanged { entries: Vec<(String, String)> },
    BackendStatus { message: String },
}

#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub local_dsp: bool,
    pub radio: bool,
}

pub enum Prepared {
    Normal(Credentials),
    #[cfg(feature = "soloist")]
    Soloist,
}

pub fn prepare(backend: BackendKind, non_interactive: bool) -> Result<Prepared> {
    match backend {
        BackendKind::Normal => {
            if non_interactive && needs_authorization() {
                bail!("normal backend requires Spotify authorization; run myx interactively first");
            }
            Ok(Prepared::Normal(credentials()?))
        }
        BackendKind::Soloist => {
            #[cfg(feature = "soloist")]
            {
                Ok(Prepared::Soloist)
            }
            #[cfg(not(feature = "soloist"))]
            {
                bail!("Soloist backend was not compiled; rebuild myx with --features soloist")
            }
        }
    }
}

enum Backend {
    Normal(normal::Engine),
    #[cfg(feature = "soloist")]
    Soloist(soloist::Engine),
}

pub struct Engine {
    pub bands: Arc<Mutex<VisBands>>,
    backend: Backend,
}

macro_rules! delegate {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match &$self.backend {
            Backend::Normal(engine) => engine.$method($($arg),*),
            #[cfg(feature = "soloist")]
            Backend::Soloist(engine) => engine.$method($($arg),*),
        }
    };
}

impl Engine {
    pub fn name(&self) -> &'static str {
        match &self.backend {
            Backend::Normal(_) => "normal",
            #[cfg(feature = "soloist")]
            Backend::Soloist(_) => "soloist",
        }
    }

    pub fn is_soloist(&self) -> bool {
        self.name() == "soloist"
    }

    pub fn capabilities(&self) -> Capabilities {
        let normal = !self.is_soloist();
        let local_dsp = match &self.backend {
            Backend::Normal(_) => true,
            #[cfg(feature = "soloist")]
            Backend::Soloist(engine) => engine.has_local_dsp(),
        };
        Capabilities {
            local_dsp,
            radio: normal,
        }
    }

    pub fn play_context(&self, uri: impl Into<String>, shuffle: bool) -> Result<()> {
        delegate!(self, play_context, uri.into(), shuffle)
    }
    pub fn play_context_at(
        &self,
        context: String,
        track: Option<String>,
        position_ms: u32,
        shuffle: bool,
    ) -> Result<()> {
        delegate!(self, play_context_at, context, track, position_ms, shuffle)
    }
    pub fn play_tracks(
        &self,
        tracks: Vec<String>,
        start: Option<String>,
        position_ms: u32,
        shuffle: bool,
    ) -> Result<()> {
        delegate!(self, play_tracks, tracks, start, position_ms, shuffle)
    }
    pub fn play_track_at(&self, uri: String, position_ms: u32) -> Result<()> {
        delegate!(self, play_track_at, uri, position_ms)
    }
    pub fn play(&self) -> Result<()> {
        delegate!(self, play)
    }
    pub fn pause(&self) -> Result<()> {
        delegate!(self, pause)
    }
    pub fn stop(&self) {
        delegate!(self, stop)
    }
    pub fn shutdown(&self) -> Result<()> {
        match &self.backend {
            Backend::Normal(engine) => {
                engine.stop();
                Ok(())
            }
            #[cfg(feature = "soloist")]
            Backend::Soloist(engine) => engine.shutdown(),
        }
    }
    pub fn toggle(&self) -> Result<()> {
        delegate!(self, toggle)
    }
    pub fn next(&self) -> Result<()> {
        delegate!(self, next)
    }
    pub fn prev(&self) -> Result<()> {
        delegate!(self, prev)
    }
    pub fn shuffle(&self, on: bool) -> Result<()> {
        delegate!(self, shuffle, on)
    }
    pub fn repeat(&self, on: bool) -> Result<()> {
        delegate!(self, repeat, on)
    }
    pub fn set_volume(&self, volume: u16) -> Result<()> {
        delegate!(self, set_volume, volume)
    }
    pub fn set_equalizer(&self, settings: EqualizerSettings) {
        match &self.backend {
            Backend::Normal(engine) => engine.set_equalizer(settings),
            #[cfg(feature = "soloist")]
            Backend::Soloist(engine) => engine.set_equalizer(settings),
        }
    }
    #[cfg(feature = "soloist")]
    pub fn audio_stats(&self) -> Option<soloist_runtime::pcm::AudioStats> {
        match &self.backend {
            Backend::Normal(_) => None,
            Backend::Soloist(engine) => engine.audio_stats(),
        }
    }
    pub fn seek(&self, position_ms: u32) -> Result<()> {
        delegate!(self, seek, position_ms)
    }
    pub fn device_id(&self) -> Result<String> {
        match &self.backend {
            Backend::Normal(engine) => Ok(engine.device_id()),
            #[cfg(feature = "soloist")]
            Backend::Soloist(engine) => engine.device_id(),
        }
    }
    pub async fn web_token(&self) -> Result<String> {
        match &self.backend {
            Backend::Normal(engine) => engine.web_token().await,
            #[cfg(feature = "soloist")]
            Backend::Soloist(_) => bail!("Soloist playback does not provide Web API tokens"),
        }
    }
    /// Compatibility for callers of the normal backend's Mercury integration.
    /// New app code uses radio_tracks(), which owns its session before spawning.
    pub fn session(&self) -> Option<librespot_core::Session> {
        match &self.backend {
            Backend::Normal(engine) => Some(engine.session()),
            #[cfg(feature = "soloist")]
            Backend::Soloist(_) => None,
        }
    }
    pub fn radio_tracks(
        &self,
        seed: String,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + 'static>> {
        match self.session() {
            Some(session) => Box::pin(async move { normal::radio_tracks(&session, &seed).await }),
            None => Box::pin(async { bail!("radio is unavailable on the Soloist backend") }),
        }
    }
    pub fn add_to_queue(&self, uri: String) -> Result<()> {
        match &self.backend {
            #[cfg(feature = "soloist")]
            Backend::Soloist(engine) => engine.add_to_queue(uri),
            Backend::Normal(_) => {
                let _ = uri;
                bail!("normal backend queue additions use the Web API")
            }
        }
    }
    pub fn request_queue(&self) -> Result<()> {
        match &self.backend {
            #[cfg(feature = "soloist")]
            Backend::Soloist(engine) => engine.request_queue(),
            Backend::Normal(_) => Ok(()),
        }
    }
}

pub async fn start(
    prepared: Prepared,
    tx: flume::Sender<EngineEvent>,
    volume: u8,
) -> Result<Engine> {
    match prepared {
        Prepared::Normal(creds) => run(creds, tx, volume).await,
        #[cfg(feature = "soloist")]
        Prepared::Soloist => {
            let engine = soloist::Engine::start(tx, volume).await?;
            Ok(Engine {
                bands: Arc::clone(&engine.bands),
                backend: Backend::Soloist(engine),
            })
        }
    }
}

/// Preserve the existing normal-backend API for upstream examples and users.
pub async fn run(creds: Credentials, tx: flume::Sender<EngineEvent>, volume: u8) -> Result<Engine> {
    let engine = normal::run(creds, tx, volume).await?;
    Ok(Engine {
        bands: Arc::clone(&engine.bands),
        backend: Backend::Normal(engine),
    })
}
