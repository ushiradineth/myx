//! Version-1 local now-playing output, independent of artwork downloads and clients.
//!
//! One latest-value slot, <=1 write/second, <=5-second heartbeat. Consumers should
//! treat a timestamp older than 15 seconds as stale. `finish` waits at most two
//! seconds; POSIX filesystem calls cannot be forcibly cancelled. A timed-out
//! worker is detached and asked to abandon publication before rename. Neither
//! this check nor rename promises a hostile-same-UID compare-and-swap guarantee.
//! The daemon's single-producer lock must outlive this publisher.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::NowPlayingConfig;
use crate::domain::playback::now_ms;
use crate::domain::{ItemKind, MetadataStatus, PlaybackStatus, Snapshot};

const INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT: Duration = Duration::from_secs(5);
const FINAL_WAIT: Duration = Duration::from_secs(2);
const MAX_DOCUMENT: u64 = 1024 * 1024;
type SafeResult<T> = Result<T, &'static str>;

struct Pending {
    latest: Option<Snapshot>,
    finish: bool,
}

struct Shared {
    pending: Mutex<Pending>,
    wake: Condvar,
    error: Mutex<Option<&'static str>>,
    cancelled: AtomicBool,
}

/// Disabled publishers allocate no worker and perform no path or filesystem work.
pub struct Publisher {
    shared: Option<Arc<Shared>>,
    worker: Option<JoinHandle<()>>,
    done: Option<mpsc::Receiver<SafeResult<()>>>,
    finish_error: Option<&'static str>,
}

impl Publisher {
    pub fn start(
        settings: &NowPlayingConfig,
        protected_paths: &[PathBuf],
        initial: Snapshot,
    ) -> Self {
        Self::start_inner(settings, protected_paths, initial, None)
    }

    pub fn start_owned(
        settings: &NowPlayingConfig,
        protected_paths: &[PathBuf],
        initial: Snapshot,
        owner_lock: File,
    ) -> Self {
        Self::start_inner(settings, protected_paths, initial, Some(owner_lock))
    }

    fn start_inner(
        settings: &NowPlayingConfig,
        protected_paths: &[PathBuf],
        initial: Snapshot,
        owner_lock: Option<File>,
    ) -> Self {
        let mut publisher = Self {
            shared: None,
            worker: None,
            done: None,
            finish_error: None,
        };
        if !settings.enabled {
            return publisher;
        }
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending {
                latest: None,
                finish: false,
            }),
            wake: Condvar::new(),
            error: Mutex::new(None),
            cancelled: AtomicBool::new(false),
        });
        let path = settings.path.clone();
        let mut protected = protected_paths.to_vec();
        protected.push(PathBuf::from(&initial.config_path));
        let state = Arc::clone(&shared);
        let (tx, rx) = mpsc::sync_channel(1);
        publisher.shared = Some(shared);
        match thread::Builder::new()
            .name("now-playing".into())
            .spawn(move || {
                // A timed-out filesystem operation must retain the instance
                // lock until it actually finishes or the process exits.
                let _owner_lock = owner_lock;
                let result = run(state.clone(), path, protected, initial);
                if let Err(message) = result {
                    *state.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
                }
                let _ = tx.send(result);
            }) {
            Ok(worker) => {
                publisher.worker = Some(worker);
                publisher.done = Some(rx);
            }
            Err(_) => {
                publisher.finish_error = Some("now-playing worker could not start");
                *publisher
                    .shared
                    .as_ref()
                    .unwrap()
                    .error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = publisher.finish_error;
            }
        }
        publisher
    }

    /// Replaces the single pending value. No filesystem work runs on this caller.
    pub fn submit(&self, snapshot: Snapshot) {
        if let Some(shared) = &self.shared {
            let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            if !pending.finish {
                pending.latest = Some(snapshot);
                shared.wake.notify_one();
            }
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.as_ref().and_then(|shared| {
            shared
                .error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .map(str::to_owned)
        })
    }

    pub fn finish(&mut self) -> anyhow::Result<()> {
        self.finish_with_timeout(FINAL_WAIT)
    }

    fn finish_with_timeout(&mut self, wait: Duration) -> anyhow::Result<()> {
        if let Some(shared) = &self.shared {
            {
                let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.finish = true;
                shared.wake.notify_one();
            }
            if let Some(done) = self.done.take() {
                match done.recv_timeout(wait) {
                    Ok(result) => self.finish_error = result.err(),
                    Err(_) => {
                        shared.cancelled.store(true, Ordering::Release);
                        self.finish_error =
                            Some("now-playing final update did not complete within two seconds");
                        *shared.error.lock().unwrap_or_else(|e| e.into_inner()) = self.finish_error;
                    }
                }
                // Do not join: completion notification is sufficient, and timeout must stay bounded.
                self.worker.take();
            }
        }
        self.finish_error
            .map_or(Ok(()), |message| Err(anyhow::anyhow!(message)))
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        // Explicit finish is the only operation that waits for the stopped document.
        if let Some(shared) = &self.shared {
            shared.cancelled.store(true, Ordering::Release);
            shared.wake.notify_one();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Track {
    kind: ItemKind,
    uri: String,
    title: Option<String>,
    artists: Option<Vec<String>>,
    album: Option<String>,
    duration_ms: Option<u64>,
    artwork_url: Option<String>,
    spotify_url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema_version: u32,
    session_id: String,
    pid: u32,
    updated_at_unix_ms: u64,
    status: PlaybackStatus,
    position_ms: u64,
    metadata_status: MetadataStatus,
    track: Option<Track>,
}

struct Projection {
    document: Document,
    playing_anchor: Option<crate::domain::PositionAnchor>,
}

impl Projection {
    fn new(snapshot: &Snapshot, now: u64) -> Self {
        let mut projection = Self {
            document: Document {
                schema_version: 1,
                session_id: snapshot.session_id.clone(),
                pid: snapshot.pid,
                updated_at_unix_ms: now,
                status: PlaybackStatus::Starting,
                position_ms: 0,
                metadata_status: MetadataStatus::Unavailable,
                track: None,
            },
            playing_anchor: None,
        };
        projection.apply(snapshot, now);
        projection
    }

    fn apply(&mut self, snapshot: &Snapshot, now: u64) {
        self.tick(now);
        let local =
            snapshot.owned_device_verified && snapshot.locally_active && snapshot.soloist_ready;
        self.document.status = if snapshot.playback == PlaybackStatus::Playing && !local {
            PlaybackStatus::Disconnected
        } else {
            snapshot.playback
        };
        self.playing_anchor = None;
        if self.document.status == PlaybackStatus::Stopped {
            self.stop(now);
            return;
        }
        // Canonical paused/buffering metadata is local even after local audio becomes inactive.
        if local || (snapshot.owned_device_verified && snapshot.playback != PlaybackStatus::Playing)
        {
            self.document.metadata_status = snapshot.metadata_status;
            if let Some(track) = &snapshot.track {
                self.document.track = Some(Track {
                    kind: track.kind,
                    uri: track.uri.clone(),
                    title: track.title.clone(),
                    artists: track.artists.clone(),
                    album: track.album.clone(),
                    duration_ms: track.duration_ms,
                    artwork_url: track.artwork_url.clone(),
                    spotify_url: track.spotify_url.clone(),
                });
            }
            if local && snapshot.playback == PlaybackStatus::Playing {
                self.playing_anchor = Some(snapshot.position);
            } else if snapshot.track.is_some() {
                self.document.position_ms = snapshot.position.position_ms;
            }
        }
        self.tick(now);
    }

    fn tick(&mut self, now: u64) {
        self.document.updated_at_unix_ms = now;
        if let Some(anchor) = self.playing_anchor {
            self.document.position_ms = anchor.at(
                now,
                self.document.track.as_ref().and_then(|t| t.duration_ms),
            );
        }
    }

    fn stop(&mut self, now: u64) {
        self.playing_anchor = None;
        self.document.status = PlaybackStatus::Stopped;
        self.document.position_ms = 0;
        self.document.track = None;
        self.document.metadata_status = MetadataStatus::Unavailable;
        self.document.updated_at_unix_ms = now;
    }
}

/// Pure monotonic scheduling seam: failures retry at most once per second.
#[derive(Default)]
struct Schedule {
    attempted: Option<Duration>,
    dirty: bool,
}

impl Schedule {
    fn delay(&self, now: Duration) -> Duration {
        let period = if self.dirty { INTERVAL } else { HEARTBEAT };
        self.attempted
            .map_or(Duration::ZERO, |last| (last + period).saturating_sub(now))
    }
}

fn run(
    shared: Arc<Shared>,
    path: Option<PathBuf>,
    protected: Vec<PathBuf>,
    initial: Snapshot,
) -> SafeResult<()> {
    let start = Instant::now();
    let mut projection = Projection::new(&initial, now_ms());
    let mut schedule = Schedule {
        dirty: true,
        ..Schedule::default()
    };
    let mut destination = None;
    loop {
        let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
        if shared.cancelled.load(Ordering::Acquire) {
            return Err("now-playing publication cancelled");
        }
        if let Some(snapshot) = pending.latest.take() {
            projection.apply(&snapshot, now_ms());
            schedule.dirty = true;
        }
        let final_update = pending.finish;
        let delay = schedule.delay(start.elapsed());
        if !final_update && !delay.is_zero() {
            drop(
                shared
                    .wake
                    .wait_timeout(pending, delay)
                    .unwrap_or_else(|e| e.into_inner()),
            );
            continue;
        }
        drop(pending);
        if final_update {
            projection.stop(now_ms());
        } else {
            projection.tick(now_ms());
        }
        let result = (|| {
            if destination.is_none() {
                let requested = path.as_ref().ok_or("now-playing output path is required")?;
                destination = Some(Destination::open(requested, &protected)?);
            }
            destination
                .as_ref()
                .unwrap()
                .write(&projection.document, &shared.cancelled)
        })();
        schedule.attempted = Some(start.elapsed());
        schedule.dirty = result.is_err();
        *shared.error.lock().unwrap_or_else(|e| e.into_inner()) = result.as_ref().err().copied();
        if final_update {
            return result;
        }
    }
}

/// Explicit user output path. No canonicalize: symlink components are refused.
struct SourceUserRequestedPath(PathBuf);

impl SourceUserRequestedPath {
    fn resolve(path: &Path) -> SafeResult<Self> {
        let expanded = if let Ok(suffix) = path.strip_prefix("~/") {
            PathBuf::from(
                std::env::var_os("HOME").ok_or("now-playing home directory is unavailable")?,
            )
            .join(suffix)
        } else {
            path.to_path_buf()
        };
        if !expanded.is_absolute()
            || expanded
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err("now-playing output requires an absolute path without dot components");
        }
        Ok(Self(expanded))
    }
}

fn cstring(value: &OsStr) -> SafeResult<CString> {
    CString::new(value.as_bytes()).map_err(|_| "now-playing output path is invalid")
}

fn owned_fd(fd: libc::c_int) -> SafeResult<File> {
    if fd < 0 {
        Err("now-playing output filesystem operation failed")
    } else {
        // SAFETY: successful open/openat returns a new uniquely owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_parent(path: &Path) -> SafeResult<File> {
    // Every component is opened relative to the preceding pinned descriptor.
    let root = cstring(OsStr::new("/"))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut directory = owned_fd(unsafe { libc::open(root.as_ptr(), flags) })?;
    for component in path.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                let name = cstring(name)?;
                directory =
                    owned_fd(unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) })?;
            }
            _ => return Err("now-playing output parent path is invalid"),
        }
    }
    let metadata = directory
        .metadata()
        .map_err(|_| "now-playing output parent cannot be inspected")?;
    if !safe_parent(metadata.uid(), metadata.mode(), unsafe { libc::geteuid() }) {
        return Err("now-playing output parent must be user-owned and not group/world writable");
    }
    Ok(directory)
}

fn safe_parent(owner: u32, mode: u32, uid: u32) -> bool {
    owner == uid && mode & 0o022 == 0
}

fn safe_destination(metadata: &std::fs::Metadata, uid: u32) -> bool {
    metadata.is_file()
        && metadata.nlink() == 1
        && metadata.uid() == uid
        && metadata.mode() & 0o077 == 0
        && metadata.len() <= MAX_DOCUMENT
}

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    ctime: i64,
    ctime_ns: i64,
}

fn identity(metadata: &std::fs::Metadata) -> Identity {
    Identity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        size: metadata.len(),
        ctime: metadata.ctime(),
        ctime_ns: metadata.ctime_nsec(),
    }
}

struct Destination {
    path: PathBuf,
    name: CString,
    parent: File,
    protected: Vec<PathBuf>,
}

impl Destination {
    fn open(path: &Path, protected: &[PathBuf]) -> SafeResult<Self> {
        let path = SourceUserRequestedPath::resolve(path)?.0;
        for protected in protected {
            if let Ok(protected) = SourceUserRequestedPath::resolve(protected) {
                if path.starts_with(protected.0) {
                    return Err("now-playing output conflicts with a protected path");
                }
            }
        }
        let parent = open_parent(
            path.parent()
                .ok_or("now-playing output parent is required")?,
        )?;
        let name = cstring(
            path.file_name()
                .ok_or("now-playing output filename is required")?,
        )?;
        let destination = Self {
            path,
            name,
            parent,
            protected: protected.to_vec(),
        };
        destination.inspect()?;
        Ok(destination)
    }

    fn check_parent(&self) -> SafeResult<()> {
        let current = open_parent(
            self.path
                .parent()
                .ok_or("now-playing output parent is required")?,
        )?;
        let pinned = self
            .parent
            .metadata()
            .map_err(|_| "now-playing output parent cannot be inspected")?;
        let current = current
            .metadata()
            .map_err(|_| "now-playing output parent cannot be inspected")?;
        if pinned.dev() != current.dev() || pinned.ino() != current.ino() {
            return Err("now-playing output parent changed");
        }
        Ok(())
    }

    fn inspect(&self) -> SafeResult<Option<Identity>> {
        self.check_parent()?;
        // Protected references may themselves use a system alias such as /var.
        // Resolve only those references for comparison, never the output path.
        // No protected file contents are opened or read.
        for protected in &self.protected {
            if let Ok(protected) = SourceUserRequestedPath::resolve(protected) {
                if self.path.starts_with(&protected.0)
                    || std::fs::canonicalize(&protected.0)
                        .is_ok_and(|real| self.path.starts_with(real))
                {
                    return Err("now-playing output conflicts with a protected path");
                }
            }
        }
        // O_NONBLOCK prevents a substituted FIFO from blocking before fstat rejects it.
        let fd = unsafe {
            libc::openat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                Ok(None)
            } else {
                Err("now-playing output destination cannot be opened safely")
            };
        }
        let mut file = owned_fd(fd)?;
        let before = file
            .metadata()
            .map_err(|_| "now-playing output destination cannot be inspected")?;
        if !safe_destination(&before, unsafe { libc::geteuid() }) {
            return Err("now-playing output refuses unsafe or non-private destination");
        }
        for protected in &self.protected {
            if let Ok(metadata) = std::fs::metadata(protected) {
                if metadata.dev() == before.dev() && metadata.ino() == before.ino() {
                    return Err("now-playing output conflicts with a protected file");
                }
            }
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_DOCUMENT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "now-playing output destination cannot be read")?;
        recognized(&bytes)?;
        let after = file
            .metadata()
            .map_err(|_| "now-playing output destination cannot be inspected")?;
        if identity(&before) != identity(&after) {
            return Err("now-playing output destination changed while inspected");
        }
        // Reopen at rename time, then compare with the identity observed here.
        Ok(Some(identity(&before)))
    }

    fn write(&self, document: &Document, cancelled: &AtomicBool) -> SafeResult<()> {
        self.write_with(document, cancelled, |file, bytes| file.write_all(bytes))
    }

    fn write_with(
        &self,
        document: &Document,
        cancelled: &AtomicBool,
        fill: impl FnOnce(&mut File, &[u8]) -> std::io::Result<()>,
    ) -> SafeResult<()> {
        if cancelled.load(Ordering::Acquire) {
            return Err("now-playing publication cancelled");
        }
        let previous = self.inspect()?;
        let mut bytes = serde_json::to_vec(document)
            .map_err(|_| "now-playing output cannot encode metadata")?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_DOCUMENT {
            return Err("now-playing output metadata exceeds size limit");
        }
        if cancelled.load(Ordering::Acquire) {
            return Err("now-playing publication cancelled");
        }
        let mut temporary = Temporary::create(&self.parent)?;
        fill(&mut temporary.file, &bytes).map_err(|_| "now-playing output write failed")?;
        temporary
            .file
            .sync_all()
            .map_err(|_| "now-playing output sync failed")?;
        if cancelled.load(Ordering::Acquire) {
            return Err("now-playing publication cancelled");
        }
        if self.inspect()? != previous {
            return Err("now-playing output destination changed during publication");
        }
        if cancelled.load(Ordering::Acquire) {
            return Err("now-playing publication cancelled");
        }
        // Both names are relative to the pinned parent; validation above is repeated
        // on every attempt. POSIX offers no CAS against a hostile same-UID writer.
        if unsafe {
            libc::renameat(
                self.parent.as_raw_fd(),
                temporary.name.as_ptr(),
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
            )
        } != 0
        {
            return Err("now-playing output atomic replacement failed");
        }
        temporary.renamed = true;
        Ok(())
    }
}

fn recognized(bytes: &[u8]) -> SafeResult<()> {
    // Deserialize alone permits absent Option fields. Exact key sets are required
    // before replacing a preexisting document, including every nullable field.
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| "now-playing output refuses unrelated existing data")?;
    fn keys(value: &serde_json::Value, expected: &[&str]) -> bool {
        value.as_object().is_some_and(|object| {
            object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
        })
    }
    if !keys(
        &value,
        &[
            "schema_version",
            "session_id",
            "pid",
            "updated_at_unix_ms",
            "status",
            "position_ms",
            "metadata_status",
            "track",
        ],
    ) || (!value["track"].is_null()
        && !keys(
            &value["track"],
            &[
                "kind",
                "uri",
                "title",
                "artists",
                "album",
                "duration_ms",
                "artwork_url",
                "spotify_url",
            ],
        ))
    {
        return Err("now-playing output refuses unrelated existing data");
    }
    let document: Document = serde_json::from_value(value)
        .map_err(|_| "now-playing output refuses unrelated existing data")?;
    if document.schema_version != 1 || document.session_id.is_empty() || document.pid == 0 {
        return Err("now-playing output refuses unrelated existing data");
    }
    Ok(())
}

struct Temporary<'a> {
    parent: &'a File,
    name: CString,
    file: File,
    renamed: bool,
}

impl<'a> Temporary<'a> {
    fn create(parent: &'a File) -> SafeResult<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..8 {
            let name = CString::new(format!(
                ".myx-now-playing-{}-{:016x}-{}.tmp",
                std::process::id(),
                rand::random::<u64>(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
            .unwrap();
            // Darwin mode_t is u16; C variadic arguments require unsigned-int promotion.
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            if fd >= 0 {
                let temporary = Self {
                    parent,
                    name,
                    file: owned_fd(fd)?,
                    renamed: false,
                };
                // Only this freshly O_EXCL-created private inode is adjusted, never a parent.
                if unsafe { libc::fchmod(temporary.file.as_raw_fd(), 0o600 as libc::mode_t) } != 0 {
                    return Err("now-playing output private temporary permissions failed");
                }
                return Ok(temporary);
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                break;
            }
        }
        Err("now-playing output private temporary file could not be created")
    }
}

impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if !self.renamed {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PositionAnchor, TrackMetadata};
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("now.json");
        (directory, path)
    }

    fn snapshot() -> Snapshot {
        let mut snapshot = Snapshot::starting("/unused/config".into(), "fixture".into());
        snapshot.soloist_ready = true;
        snapshot.owned_device_verified = true;
        snapshot.locally_active = true;
        snapshot.playback = PlaybackStatus::Playing;
        snapshot.position = PositionAnchor {
            position_ms: 500,
            timestamp_ms: 100,
            speed: 1.0,
        };
        snapshot.track = Some(TrackMetadata::pending(
            ItemKind::Track,
            "spotify:track:fixture".into(),
        ));
        snapshot.metadata_status = MetadataStatus::Loading;
        snapshot
    }

    fn write(destination: &Destination, projection: &Projection) -> SafeResult<()> {
        destination.write(&projection.document, &AtomicBool::new(false))
    }

    #[test]
    fn disabled_has_no_worker_or_path_resolution_even_for_nul_path() {
        let settings = NowPlayingConfig {
            enabled: false,
            path: Some(PathBuf::from("~/\0invalid")),
        };
        let mut publisher = Publisher::start(&settings, &[], snapshot());
        assert!(publisher.shared.is_none());
        assert!(publisher.worker.is_none());
        publisher.submit(snapshot());
        publisher.finish().unwrap();
        assert!(publisher.error().is_none());
    }

    #[test]
    fn whitelist_null_metadata_unicode_and_every_state() {
        let mut snapshot = snapshot();
        let mut projection = Projection::new(&snapshot, 100);
        let value = serde_json::to_value(&projection.document).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 8);
        assert_eq!(value["track"].as_object().unwrap().len(), 8);
        assert!(value["track"]["title"].is_null());
        assert_eq!(value["metadata_status"], "loading");
        snapshot.track.as_mut().unwrap().title = Some("夜の\"音楽\"\nBjörk".into());
        snapshot.metadata_status = MetadataStatus::Ready;
        snapshot.message = Some("must-not-export".into());
        snapshot.spectrum = Some(vec![123.0]);
        projection.apply(&snapshot, 200);
        let bytes = serde_json::to_vec(&projection.document).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("must-not-export"));
        assert!(!bytes.contains(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<Document>(&bytes)
                .unwrap()
                .track
                .unwrap()
                .title,
            snapshot.track.as_ref().unwrap().title
        );
        for (state, name) in [
            (PlaybackStatus::Starting, "starting"),
            (PlaybackStatus::Playing, "playing"),
            (PlaybackStatus::Paused, "paused"),
            (PlaybackStatus::Buffering, "buffering"),
            (PlaybackStatus::Disconnected, "disconnected"),
            (PlaybackStatus::Error, "error"),
            (PlaybackStatus::Stopped, "stopped"),
        ] {
            snapshot.playback = state;
            projection.apply(&snapshot, 300);
            assert_eq!(
                serde_json::to_value(&projection.document).unwrap()["status"],
                name
            );
        }
        assert!(projection.document.track.is_none());
        assert_eq!(projection.document.position_ms, 0);
    }

    #[test]
    fn local_gate_retention_freeze_and_producer_identity_survive_client_submissions() {
        let mut snapshot = snapshot();
        let mut projection = Projection::new(&snapshot, 200);
        assert_eq!(projection.document.position_ms, 600);
        let session = projection.document.session_id.clone();
        let pid = projection.document.pid;
        for state in [
            PlaybackStatus::Paused,
            PlaybackStatus::Buffering,
            PlaybackStatus::Disconnected,
        ] {
            snapshot.playback = state;
            snapshot.locally_active = false;
            snapshot.position.position_ms = 777;
            // Even an accidentally live speed must not advance a paused export.
            projection.apply(&snapshot, 300);
            projection.tick(100_000);
            assert_eq!(projection.document.position_ms, 777);
            assert!(projection.document.track.is_some());
        }
        snapshot.session_id = "client-detached".into();
        snapshot.pid = 42;
        snapshot.playback = PlaybackStatus::Playing;
        snapshot.track.as_mut().unwrap().uri = "spotify:track:remote".into();
        for flag in 0..3 {
            snapshot.owned_device_verified = flag != 0;
            snapshot.locally_active = flag != 1;
            snapshot.soloist_ready = flag != 2;
            projection.apply(&snapshot, 101_000);
            assert_eq!(projection.document.status, PlaybackStatus::Disconnected);
            assert_eq!(
                projection.document.track.as_ref().unwrap().uri,
                "spotify:track:fixture"
            );
            assert_eq!(projection.document.position_ms, 777);
        }
        assert_eq!(projection.document.session_id, session);
        assert_eq!(projection.document.pid, pid);
        projection.stop(102_000);
        assert!(projection.document.track.is_none());
        assert_eq!(projection.document.position_ms, 0);
    }

    #[test]
    fn scheduler_coalesces_at_one_second_and_heartbeats_at_five() {
        let mut schedule = Schedule::default();
        assert_eq!(schedule.delay(Duration::ZERO), Duration::ZERO);
        schedule.attempted = Some(Duration::ZERO);
        assert_eq!(schedule.delay(Duration::ZERO), HEARTBEAT);
        schedule.dirty = true;
        assert_eq!(
            schedule.delay(Duration::from_millis(999)),
            Duration::from_millis(1)
        );
        assert_eq!(schedule.delay(INTERVAL), Duration::ZERO);
        schedule.attempted = Some(INTERVAL);
        schedule.dirty = false;
        assert_eq!(schedule.delay(INTERVAL), HEARTBEAT);
        assert_eq!(schedule.delay(Duration::from_secs(6)), Duration::ZERO);
        schedule.dirty = true; // Failed attempt retries, not a busy loop.
        assert_eq!(schedule.delay(INTERVAL), INTERVAL);
    }

    #[test]
    fn latest_slot_is_bounded_and_finish_rejects_new_submissions() {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending {
                latest: None,
                finish: false,
            }),
            wake: Condvar::new(),
            error: Mutex::new(None),
            cancelled: AtomicBool::new(false),
        });
        let mut publisher = Publisher {
            shared: Some(shared.clone()),
            worker: None,
            done: None,
            finish_error: None,
        };
        for sequence in 0..10_000 {
            let mut value = snapshot();
            value.sequence = sequence;
            publisher.submit(value);
        }
        assert_eq!(
            shared
                .pending
                .lock()
                .unwrap()
                .latest
                .as_ref()
                .unwrap()
                .sequence,
            9999
        );
        publisher.finish().unwrap();
        publisher.submit(snapshot());
        assert_eq!(
            shared
                .pending
                .lock()
                .unwrap()
                .latest
                .as_ref()
                .unwrap()
                .sequence,
            9999
        );
    }

    #[test]
    fn destination_refuses_links_directories_protected_and_unrelated_data() {
        let (root, path) = fixture();
        let target = root.path().canonicalize().unwrap().join("credentials");
        let recognized_target = Projection::new(&snapshot(), 100).document;
        let original = serde_json::to_vec(&recognized_target).unwrap();
        std::fs::write(&target, &original).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &path).unwrap();
        assert!(Destination::open(&path, &[]).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&target, &path).unwrap();
        assert!(Destination::open(&path, &[]).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(Destination::open(&path, &[]).is_err());
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, b"unrelated fixture").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Destination::open(&path, &[]).is_err());
        assert!(Destination::open(&path, std::slice::from_ref(&path)).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), original);
        assert_eq!(std::fs::read(&path).unwrap(), b"unrelated fixture");
    }

    #[test]
    fn parent_symlink_writable_parent_missing_parent_and_foreign_ownership_policy() {
        let (root, path) = fixture();
        let canonical = root.path().canonicalize().unwrap();
        let alias = canonical.join("alias");
        symlink(&canonical, &alias).unwrap();
        assert!(Destination::open(&alias.join("output"), &[]).is_err());
        assert!(Destination::open(&canonical.join("missing/output"), &[]).is_err());
        assert!(!canonical.join("missing").exists());
        assert!(!safe_parent(42, 0o700, 43));
        assert!(!safe_parent(42, 0o720, 42));
        assert!(!safe_parent(42, 0o702, 42));
        std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(Destination::open(&path, &[]).is_err());
        assert_eq!(std::fs::metadata(&canonical).unwrap().mode() & 0o777, 0o770);
        std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o700)).unwrap();
        let destination = Destination::open(&path, &[]).unwrap();
        write(&destination, &Projection::new(&snapshot(), 100)).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(!safe_destination(&metadata, metadata.uid().wrapping_add(1)));
    }

    #[test]
    fn write_time_revalidation_retains_changed_data_and_pinned_parent() {
        let (root, path) = fixture();
        let destination = Destination::open(&path, &[]).unwrap();
        let projection = Projection::new(&snapshot(), 100);
        write(&destination, &projection).unwrap();
        std::fs::write(&path, b"new unrelated data").unwrap();
        assert!(write(&destination, &projection).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"new unrelated data");
        std::fs::remove_file(&path).unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let moved = canonical.with_extension("moved-now-playing-test");
        std::fs::rename(&canonical, &moved).unwrap();
        std::fs::create_dir(&canonical).unwrap();
        assert!(write(&destination, &projection).is_err());
        std::fs::remove_dir(&canonical).unwrap();
        std::fs::rename(&moved, &canonical).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn write_time_symlink_hardlink_and_permission_changes_are_refused() {
        let (root, path) = fixture();
        let canonical = root.path().canonicalize().unwrap();
        let target = canonical.join("protected-fixture");
        std::fs::write(&target, b"preserve").unwrap();
        let destination = Destination::open(&path, &[]).unwrap();
        let projection = Projection::new(&snapshot(), 100);
        write(&destination, &projection).unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        assert!(write(&destination, &projection).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&target, &path).unwrap();
        assert!(write(&destination, &projection).is_err());
        std::fs::remove_file(&path).unwrap();
        write(&destination, &projection).unwrap();
        let complete = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(write(&destination, &projection).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), complete);
        std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(write(&destination, &projection).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
    }

    #[test]
    fn destination_changed_between_validation_and_rename_is_not_overwritten() {
        let (_root, path) = fixture();
        let destination = Destination::open(&path, &[]).unwrap();
        let projection = Projection::new(&snapshot(), 100);
        write(&destination, &projection).unwrap();
        let mut changed = projection.document.clone();
        changed.session_id.push_str("-different-producer-fixture");
        let changed = serde_json::to_vec(&changed).unwrap();
        let result = destination.write_with(
            &projection.document,
            &AtomicBool::new(false),
            |file, bytes| {
                file.write_all(bytes)?;
                std::fs::write(&path, &changed)
            },
        );
        assert_eq!(
            result,
            Err("now-playing output destination changed during publication")
        );
        assert_eq!(std::fs::read(&path).unwrap(), changed);
        let result = destination.write_with(
            &projection.document,
            &AtomicBool::new(false),
            |file, bytes| {
                file.write_all(bytes)?;
                std::fs::write(&path, b"unrelated replacement during write")
            },
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"unrelated replacement during write"
        );
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn strict_path_and_loading_without_track_do_not_invent_metadata() {
        for path in ["relative/output", "~other/output", "/tmp/../output", "/"] {
            assert!(Destination::open(Path::new(path), &[]).is_err());
        }
        let mut snapshot = snapshot();
        snapshot.track = None;
        let projection = Projection::new(&snapshot, 100);
        assert!(projection.document.track.is_none());
        assert_eq!(projection.document.metadata_status, MetadataStatus::Loading);
        let (_root, path) = fixture();
        let protected_parent = path.parent().unwrap().to_path_buf();
        assert!(Destination::open(&path, std::slice::from_ref(&protected_parent)).is_err());
        let alias = protected_parent.join("protected-parent-alias");
        symlink(&protected_parent, &alias).unwrap();
        assert!(Destination::open(&path, &[alias]).is_err());
    }

    #[test]
    fn failed_attempt_retains_last_complete_and_retry_succeeds_without_temporary_leaks() {
        let (root, path) = fixture();
        let destination = Destination::open(&path, &[]).unwrap();
        let mut projection = Projection::new(&snapshot(), 100);
        write(&destination, &projection).unwrap();
        let before = std::fs::read(&path).unwrap();
        projection.stop(200);
        let error = destination.write_with(
            &projection.document,
            &AtomicBool::new(false),
            |file, bytes| {
                file.write_all(&bytes[..bytes.len() / 2])?;
                Err(std::io::Error::other("injected partial write failure"))
            },
        );
        assert_eq!(error, Err("now-playing output write failed"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        assert!(destination
            .write(&projection.document, &AtomicBool::new(true))
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        write(&destination, &projection).unwrap();
        let document: Document = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(document.status, PlaybackStatus::Stopped);
        assert!(document.track.is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn strict_recognition_refuses_partial_extra_fields_and_wrong_version() {
        let document = Projection::new(&snapshot(), 100).document;
        let value = serde_json::to_value(document).unwrap();
        recognized(&serde_json::to_vec(&value).unwrap()).unwrap();
        for field in ["track", "metadata_status", "session_id"] {
            let mut partial = value.clone();
            partial.as_object_mut().unwrap().remove(field);
            assert!(recognized(&serde_json::to_vec(&partial).unwrap()).is_err());
        }
        let mut extra = value.clone();
        extra["auth"] = "not ours".into();
        assert!(recognized(&serde_json::to_vec(&extra).unwrap()).is_err());
        let mut wrong = value;
        wrong["schema_version"] = 2.into();
        assert!(recognized(&serde_json::to_vec(&wrong).unwrap()).is_err());
    }

    #[test]
    fn concurrent_readers_only_observe_complete_newline_terminated_json() {
        let (_root, path) = fixture();
        let destination = Destination::open(&path, &[]).unwrap();
        let mut projection = Projection::new(&snapshot(), 100);
        write(&destination, &projection).unwrap();
        let reader_path = path.clone();
        let reader = thread::spawn(move || {
            for _ in 0..250 {
                let bytes = std::fs::read(&reader_path).unwrap();
                assert_eq!(bytes.last(), Some(&b'\n'));
                recognized(&bytes).unwrap();
            }
        });
        for now in 101..151 {
            projection.tick(now);
            write(&destination, &projection).unwrap();
        }
        reader.join().unwrap();
    }

    #[test]
    fn finish_timeout_is_bounded_without_joining_a_stuck_worker() {
        // A held completion sender models a worker stuck inside an OS syscall.
        let (tx, rx) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending {
                latest: None,
                finish: false,
            }),
            wake: Condvar::new(),
            error: Mutex::new(None),
            cancelled: AtomicBool::new(false),
        });
        let mut publisher = Publisher {
            shared: Some(shared.clone()),
            worker: None,
            done: Some(rx),
            finish_error: None,
        };
        let start = Instant::now();
        assert_eq!(FINAL_WAIT, Duration::from_secs(2));
        assert!(publisher
            .finish_with_timeout(Duration::from_millis(10))
            .is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(shared.cancelled.load(Ordering::Acquire));
        assert!(publisher.error().unwrap().contains("two seconds"));
        drop(tx);
    }
}
