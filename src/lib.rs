//! Daemon-owned player services and an attachable terminal client.

use std::path::PathBuf;

pub mod anim;
pub mod color;
pub mod components;
pub mod cover;
pub mod gradient;
pub mod httpcache;
pub mod liblog;
pub mod lyrics;
pub mod reactive;
pub mod theme;
pub mod util;

pub mod audio;
pub mod cli;
pub mod client;
pub mod config;
pub mod daemon;
pub mod domain;
pub mod ipc;
pub mod media;
pub mod player;
pub mod service;
pub mod soloist;
pub mod term;
pub mod webapi;

/// Cross-platform home directory. Uses `HOME` on Unix, `USERPROFILE` on Windows.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    let var = "HOME";
    #[cfg(windows)]
    let var = "USERPROFILE";
    std::env::var(var).ok().map(PathBuf::from)
}
