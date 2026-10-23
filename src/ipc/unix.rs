use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};

#[derive(Clone)]
pub struct RuntimePaths {
    pub directory: PathBuf,
    pub socket: PathBuf,
    pub startup_lock: PathBuf,
    pub daemon_lock: PathBuf,
}

impl RuntimePaths {
    pub fn for_user() -> Result<Self> {
        let home = crate::home_dir().context("A home directory is required")?;
        Ok(Self::in_directory(home.join(".cache/myx/daemon")))
    }
    pub fn in_directory(directory: PathBuf) -> Self {
        Self {
            socket: directory.join("daemon-v1.sock"),
            startup_lock: directory.join("startup.lock"),
            daemon_lock: directory.join("owner.lock"),
            directory,
        }
    }
    pub fn prepare(&self) -> Result<()> {
        private_directory(&self.directory)
    }
}

pub fn private_directory(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "Private directory must be absolute");
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "Private path has a symlink component"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => anyhow::bail!("Private directory cannot be inspected"),
        }
    }
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
    {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => anyhow::bail!("Private directory cannot be created"),
    }
    let metadata = fs::symlink_metadata(path).context("Inspect private directory")?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Private directory must be owned by this user and mode0700"
    );
    Ok(())
}

pub fn open_private_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .context("Open private daemon lock")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1,
        "Daemon lock must be a private owned regular file"
    );
    Ok(file)
}

pub fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let uid = {
        let mut uid = 0;
        let mut gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        uid
    };
    #[cfg(not(target_os = "macos"))]
    let uid = {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Peer authentication requires the Mac adapter",
        ));
        #[allow(unreachable_code)]
        0
    };
    if uid == unsafe { libc::geteuid() } {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC peer is not this user",
        ))
    }
}
