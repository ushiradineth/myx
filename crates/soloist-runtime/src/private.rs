// Adapted from tools/soloist-elfuse under the project's MIT license.
use anyhow::{bail, ensure, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

pub fn directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e).context("cannot create private directory"),
    }
    let meta = fs::symlink_metadata(path).context("private directory is missing")?;
    // SAFETY: geteuid has no preconditions or borrowed pointers.
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o7777 == 0o700,
        "expected owned mode0700 non-symlink directory"
    );
    Ok(())
}

pub fn open_regular(path: &Path, mode: Option<u32>) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("cannot open owned regular file without following symlinks")?;
    let meta = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    ensure!(
        meta.is_file() && meta.uid() == unsafe { libc::geteuid() },
        "expected owned regular file"
    );
    if let Some(mode) = mode {
        ensure!(
            meta.mode() & 0o7777 == mode,
            "unexpected private file permissions"
        );
    }
    Ok(file)
}

pub fn read_regular(path: &Path, mode: Option<u32>, limit: u64) -> Result<Vec<u8>> {
    let file = open_regular(path, mode)?;
    ensure!(
        file.metadata()?.len() <= limit,
        "private file exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "private file exceeds size limit"
    );
    Ok(bytes)
}

pub fn read_key(path: &Path) -> Result<String> {
    let bytes = read_regular(path, Some(0o600), 4096)?;
    let key = std::str::from_utf8(&bytes).map_err(|_| anyhow::anyhow!("API key must be ASCII"))?;
    validate_key(key)
}

fn validate_key(key: &str) -> Result<String> {
    ensure!(
        key.len() <= 4096 && key.is_ascii(),
        "API key must be bounded ASCII"
    );
    let key = key.trim_end_matches(['\r', '\n']);
    ensure!(
        !key.is_empty() && key.bytes().all(|b| b > 32 && b < 127),
        "API key must be one nonempty line without spaces"
    );
    Ok(key.to_owned())
}

pub fn credential(source: &crate::ApiKey) -> Result<String> {
    match source {
        crate::ApiKey::Inline(key) => validate_key(key),
        crate::ApiKey::File(path) => read_key(path),
    }
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

pub struct SessionLock(File);
impl SessionLock {
    pub fn acquire(directory: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join("soloist-runtime.lock"))?;
        let meta = file.metadata()?;
        // SAFETY: geteuid has no preconditions; flock receives a live owned FD.
        ensure!(
            meta.is_file()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o7777 == 0o600,
            "invalid runtime lock"
        );
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("another runtime owns this data or cache directory");
        }
        Ok(Self(file))
    }
}
impl Drop for SessionLock {
    fn drop(&mut self) {
        // SAFETY: the lock's FD stays valid until this owner is dropped.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn key_rejects_symlinks_permissions_and_invalid_contents() {
        let temp = tempfile::tempdir().unwrap();
        let key = temp.path().join("key");
        write_private(&key, b"test-key\n").unwrap();
        assert_eq!(read_key(&key).unwrap(), "test-key");
        let alias = temp.path().join("alias");
        symlink(&key, &alias).unwrap();
        assert!(read_key(&alias).is_err());
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_key(&key).is_err());
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        for value in [
            b"two lines\nkey".as_slice(),
            b"",
            b"space key",
            b"\xff",
            &[b'x'; 4097],
        ] {
            fs::write(&key, value).unwrap();
            assert!(read_key(&key).is_err());
        }
    }

    #[test]
    fn inline_and_file_credentials_share_validation_without_leaking_values() {
        assert_eq!(
            credential(&crate::ApiKey::Inline("test-key".into())).unwrap(),
            "test-key"
        );
        assert!(credential(&crate::ApiKey::File("/missing/key".into())).is_err());
        for value in [
            "",
            "PRIVATE-SENTINEL two",
            "PRIVATE-SENTINEL\nsecond",
            "非ASCII",
        ] {
            let error = credential(&crate::ApiKey::Inline(value.into()))
                .unwrap_err()
                .to_string();
            assert!(!error.contains(value) || value.is_empty());
            assert!(!error.contains("PRIVATE-SENTINEL"));
        }
        assert!(credential(&crate::ApiKey::Inline("x".repeat(4097))).is_err());
    }

    #[test]
    fn lock_excludes_until_owner_drops_and_preserves_data() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        directory(&data).unwrap();
        fs::write(data.join("persistent"), b"keep").unwrap();
        let lock = SessionLock::acquire(&data).unwrap();
        assert!(SessionLock::acquire(&data).is_err());
        drop(lock);
        assert!(SessionLock::acquire(&data).is_ok());
        assert_eq!(fs::read(data.join("persistent")).unwrap(), b"keep");
    }

    #[test]
    fn private_directory_rejects_symlinks_and_public_mode() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        directory(&data).unwrap();
        let alias = temp.path().join("alias");
        symlink(&data, &alias).unwrap();
        assert!(directory(&alias).is_err());
        fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(directory(&data).is_err());
    }
}
