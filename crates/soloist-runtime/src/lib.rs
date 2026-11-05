//! Managed process, audio and Bonjour lifetime; Myx owns WebSocket transport.
//!
//! `start` is blocking (readiness is bounded to 25 seconds). Call it off the UI
//! thread. Readiness does not imply authentication. Poll `is_running` to observe
//! child exits and native Bonjour errors; call `shutdown` to receive cleanup errors.
//! Drop performs the same cleanup on a best-effort basis. Failed startup performs
//! cleanup before returning; group termination can take up to five seconds per
//! child in addition to the readiness deadline.
//! Recoverable processed-output loss keeps the private server, SDK and discovery
//! alive. Polling `is_running` services bounded output recovery; inspect the
//! supplied `pcm::AudioControl` separately for output readiness and health.

use anyhow::Result;
use std::path::PathBuf;

/// Non-retaining SDK log summary. Only counts and fixed error categories escape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SdkDiagnostics {
    pub get_info_requests: u64,
    pub add_user_requests: u64,
    pub failure_flags: u32,
}
impl SdkDiagnostics {
    pub fn safe_message(self) -> String {
        let categories = [
            (1, "authentication_failed"),
            (2, "network_failure"),
            (4, "tls_failure"),
            (8, "api_key_validity_hint"),
            (16, "sdk_expired"),
            (32, "pairing_rejected"),
        ]
        .into_iter()
        .filter_map(|(bit, name)| (self.failure_flags & bit != 0).then_some(name))
        .collect::<Vec<_>>();
        format!(
            "SDK pairing diagnostics: getInfo={}, addUser={}, categories={}",
            self.get_info_requests,
            self.add_user_requests,
            if categories.is_empty() {
                "none".into()
            } else {
                categories.join(",")
            }
        )
    }
}

/// Safe startup context: never contains argv, credentials or vendor responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupStage {
    Configuration,
    SdkValidation,
    Credential,
    Storage,
    NativeAudio,
    SdkProcess,
    SdkReadiness,
    Transport,
}

impl std::fmt::Display for StartupStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Configuration => "Soloist runtime configuration is invalid or incomplete",
            Self::SdkValidation => "Soloist SDK file validation failed; check the provisioned SDK",
            Self::Credential => {
                "Soloist API-key validation failed; check the configured credential source"
            }
            Self::Storage => "Soloist private data/cache setup or ownership lock failed",
            Self::NativeAudio => "Soloist native PulseAudio/CoreAudio startup failed",
            Self::SdkProcess => "Soloist SDK runner could not be started",
            Self::SdkReadiness => "Soloist SDK did not reach owned endpoint/discovery readiness",
            Self::Transport => "Soloist owned WebSocket transport failed",
        })
    }
}
impl std::error::Error for StartupStage {}

/// Only known local messages/context are exposed, never an arbitrary error chain.
pub fn startup_diagnostic(error: &anyhow::Error) -> &'static str {
    for cause in error.chain() {
        match cause.to_string().as_str() {
            "owned native audio server exited" => return "Private PulseAudio exited during startup; check the relocated audio modules and libraries",
            "private server has no unique verified CoreAudio sink" => return "Private audio startup found no unique verified CoreAudio output",
            "native libpulse unavailable" => return "Private PCM bridge could not load the native libpulse library",
            "native libpulse API unavailable" => return "Private PCM bridge could not load the required native libpulse symbols",
            "owned runtime exited before readiness" => return "Soloist SDK or its owned audio process exited before endpoint readiness",
            _ => {}
        }
    }
    match error.downcast_ref::<StartupStage>() {
        Some(StartupStage::Configuration) => {
            "Soloist runtime configuration is invalid or incomplete"
        }
        Some(StartupStage::SdkValidation) => {
            "Soloist SDK file validation failed; check the provisioned SDK"
        }
        Some(StartupStage::Credential) => {
            "Soloist API-key validation failed; check the configured credential source"
        }
        Some(StartupStage::Storage) => "Soloist private data/cache setup or ownership lock failed",
        Some(StartupStage::NativeAudio) => "Soloist native PulseAudio/CoreAudio startup failed",
        Some(StartupStage::SdkProcess) => "Soloist SDK runner could not be started",
        Some(StartupStage::SdkReadiness) => {
            "Soloist SDK did not reach owned endpoint/discovery readiness"
        }
        Some(StartupStage::Transport) => "Soloist owned WebSocket transport failed",
        None => "Soloist worker stopped after a runtime or transport failure",
    }
}

#[cfg(test)]
mod startup_diagnostic_tests {
    use super::*;
    #[test]
    fn nested_startup_context_is_actionable_without_forwarding_secret_details() {
        let error = anyhow::anyhow!("PRIVATE-CREDENTIAL-AND-PROVIDER-DIAGNOSTIC")
            .context(StartupStage::NativeAudio)
            .context("owned startup failed");
        assert_eq!(
            startup_diagnostic(&error),
            "Soloist native PulseAudio/CoreAudio startup failed"
        );
        assert_eq!(
            startup_diagnostic(&anyhow::anyhow!("PRIVATE-CREDENTIAL")),
            "Soloist worker stopped after a runtime or transport failure"
        );
    }
}

#[cfg(test)]
#[path = "../tests/native_audio.rs"]
mod native_audio;

pub mod executable;
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
    /// Optional owned bundle module directory; None keeps the tools' compiled
    /// module search path. This is not a backend or DSP selector.
    pub audio_modules: Option<PathBuf>,
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
    pub fn sdk_diagnostics(&self) -> SdkDiagnostics {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            self.managed.sdk_diagnostics()
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            SdkDiagnostics::default()
        }
    }
    pub fn process_groups(&self) -> Vec<(u32, u32)> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            self.managed.process_groups()
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            Vec::new()
        }
    }
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
                "managed Soloist runtime requires macOS ARM64",
            )
            .into())
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

    /// Public deviceID validated at the owned SDK's pairing endpoint and used
    /// for discovery registration. No pairing/key/account payload is exposed.
    pub fn device_id(&self) -> Result<String> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            self.managed.device_id().map(str::to_owned)
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            anyhow::bail!("owned SDK device identity is unavailable on this platform")
        }
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
            audio_modules: None,
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
