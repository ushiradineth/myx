//! Managed process, audio and Bonjour lifetime; Myx owns WebSocket transport.
//!
//! `start` is blocking (readiness is bounded to 25 seconds). Call it off the UI
//! thread. Readiness does not imply authentication. Poll `is_running` to observe
//! child exits and native Bonjour errors; call `shutdown` to receive cleanup errors.
//! Drop performs the same cleanup on a best-effort basis. Failed startup performs
//! cleanup before returning; group termination can take up to five seconds per
//! child in addition to the readiness deadline.

use anyhow::Result;
use std::path::PathBuf;

pub mod pcm;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod pulse;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod audio;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod managed;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod platform;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod private;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod process;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod sdk;

/// Provisioned paths are supplied by Myx, never discovered in development trees.
/// Data/cache directories must be owned mode0700 directories (or absent with an
/// existing parent). File credentials must be owned mode0600 regular non-symlinks.
/// The caller must protect any config containing an inline credential.
pub struct Config {
    pub sdk: PathBuf,
    pub runner: PathBuf,
    pub sysroot: PathBuf,
    pub library_path: String,
    pub audio_tools: PathBuf,
    pub api_key: ApiKey,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub device_name: String,
    pub initial_volume: u8,
    pub expected_sdk_sha256: Option<String>,
}

/// Deliberately not Debug: credential values must never enter diagnostics.
pub enum ApiKey {
    Inline(String),
    File(PathBuf),
}

pub struct Runtime {
    endpoint: String,
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    managed: managed::Managed,
}

impl Runtime {
    pub fn start(config: Config) -> Result<Self> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            let (managed, endpoint) = managed::Managed::start(config, None)?;
            Ok(Self { endpoint, managed })
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            let _ = config;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "managed Soloist runtime requires macOS ARM64; use an external WebSocket backend on this platform",
            ).into())
        }
    }

    /// Own a bounded native PCM bridge as part of the same private runtime.
    pub fn start_processed(
        config: Config,
        control: pcm::AudioControl,
        processor: Box<dyn pcm::AudioProcessor>,
    ) -> Result<Self> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            let (managed, endpoint) = managed::Managed::start(config, Some((control, processor)))?;
            Ok(Self { endpoint, managed })
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            let _ = (config, control, processor);
            anyhow::bail!("managed PCM processing requires macOS ARM64");
        }
    }

    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub fn is_running(&mut self) -> bool {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            self.managed.is_running()
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            false
        }
    }

    pub fn shutdown(&mut self) -> Result<()> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            self.managed.shutdown()
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            Ok(())
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(all(test, not(all(target_os = "macos", target_arch = "aarch64"))))]
mod tests {
    use super::*;

    #[test]
    fn unsupported_start_does_not_access_provisioned_paths() {
        let error = Runtime::start(Config {
            sdk: "absent".into(),
            runner: "absent".into(),
            sysroot: "absent".into(),
            library_path: String::new(),
            audio_tools: "absent".into(),
            api_key: ApiKey::File("absent".into()),
            data_dir: "absent".into(),
            cache_dir: "absent".into(),
            device_name: String::new(),
            initial_volume: 0,
            expected_sdk_sha256: None,
        })
        .err()
        .unwrap();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::Unsupported
        );
    }
}
