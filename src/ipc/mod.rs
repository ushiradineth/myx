//! Private, bounded same-user daemon IPC. Not a network RPC surface.
pub mod protocol;
#[cfg(unix)]
pub mod unix;
