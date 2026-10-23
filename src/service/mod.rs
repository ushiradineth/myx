//! Explicit, per-GUI-user service registration and control.

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::{install, start, status, stop, uninstall};

/// Registration and activation are independent: installation never starts a daemon.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ServiceStatus {
    pub installed: bool,
    pub loaded: bool,
}
