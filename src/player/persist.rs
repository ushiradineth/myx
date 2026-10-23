//! Canonical, bounded player preferences. Never stores service or decoder state.

use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::audio::EqualizerSettings;
use crate::domain::{
    PlaybackStatus, PlayerCommand, PositionAnchor, RepeatMode, Snapshot, TrackMetadata,
};

const SCHEMA: &str = "myx.player";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_POSITION: u64 = u32::MAX as u64;
const MAX_TEXT: usize = 4096;
const MAX_ITEMS: usize = 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedPlayer {
    pub track: Option<TrackMetadata>,
    pub position_ms: u64,
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub equalizer: EqualizerSettings,
    pub last_intent: Option<PlayerCommand>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema: String,
    schema_version: u32,
    player: SavedPlayer,
}

impl SavedPlayer {
    pub fn from_snapshot(snapshot: &Snapshot, last_intent: Option<PlayerCommand>) -> Self {
        let duration = snapshot.track.as_ref().and_then(|track| track.duration_ms);
        Self {
            track: snapshot.track.clone(),
            // Copy the observed anchor, never advance a persisted clock.
            position_ms: snapshot
                .position
                .position_ms
                .min(duration.unwrap_or(MAX_POSITION))
                .min(MAX_POSITION),
            volume: snapshot.volume.min(100),
            shuffle: snapshot.shuffle,
            repeat: snapshot.repeat,
            equalizer: snapshot.equalizer.normalized(),
            last_intent: last_intent.filter(|intent| {
                matches!(
                    intent,
                    PlayerCommand::PlayContext { .. } | PlayerCommand::PlayTracks { .. }
                )
            }),
        }
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        let (parent, name) = match open_parent(path) {
            Ok(value) => value,
            Err(error) if is_missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(read_destination(&parent, &name)?.map(|(_, state)| state))
    }

    pub fn apply(&self, snapshot: &mut Snapshot) {
        snapshot.track = self.track.clone();
        snapshot.position = PositionAnchor {
            position_ms: self
                .position_ms
                .min(
                    self.track
                        .as_ref()
                        .and_then(|t| t.duration_ms)
                        .unwrap_or(MAX_POSITION),
                )
                .min(MAX_POSITION),
            speed: 0.0,
            ..PositionAnchor::default()
        };
        snapshot.playback = PlaybackStatus::Paused;
        snapshot.volume = self.volume.min(100);
        snapshot.shuffle = self.shuffle;
        snapshot.repeat = self.repeat;
        snapshot.equalizer = self.equalizer.normalized();
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        validate_path(path)?;
        let bytes = serde_json::to_vec(&Envelope {
            schema: SCHEMA.into(),
            schema_version: VERSION,
            player: self.clone(),
        })?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "Player state exceeds size bound"
        );
        crate::ipc::unix::private_directory(path.parent().context("Missing player parent")?)?;
        let (parent, name) = open_parent(path)?;
        // Serialize cooperating writers on the anchored directory, not a separate
        // lock pathname that could be replaced. Caller threads never enter here.
        ensure!(
            unsafe { libc::flock(parent.as_raw_fd(), libc::LOCK_EX) } == 0,
            "Cannot lock player directory"
        );
        let before = read_destination(&parent, &name)?.map(|(metadata, _)| metadata);
        let (mut file, temporary) = create_temporary(&parent)?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes).context("Write player state")?;
            file.sync_all().context("Sync player state")?;
            // Reopen through the same dirfd and check both identity and marker.
            // A private parent excludes other users; this also detects replacement
            // by another same-user process before publication.
            let current = read_destination(&parent, &name)?.map(|(metadata, _)| metadata);
            ensure!(
                same_destination(before.as_ref(), current.as_ref()),
                "Player destination changed during save"
            );
            let (named_parent, _) = open_parent(path)?;
            let anchored = parent.metadata()?;
            let named = named_parent.metadata()?;
            ensure!(
                anchored.dev() == named.dev() && anchored.ino() == named.ino(),
                "Player parent changed during save"
            );
            publish(&parent, &temporary, &name, before.is_none())?;
            parent.sync_all().context("Sync player directory")?;
            Ok(())
        })();
        // The name is random and belongs exclusively to this invocation.
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0);
        }
        result
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.volume <= 100, "Invalid persisted volume");
        ensure!(
            self.position_ms <= MAX_POSITION,
            "Invalid persisted position"
        );
        ensure!(
            self.equalizer == self.equalizer.normalized(),
            "Invalid persisted equalizer"
        );
        if let Some(track) = &self.track {
            validate_uri(&track.uri)?;
            for text in [&track.title, &track.album].into_iter().flatten() {
                validate_text(text)?;
            }
            if let Some(artists) = &track.artists {
                ensure!(artists.len() <= 128, "Too many persisted artists");
                for artist in artists {
                    validate_text(artist)?;
                }
            }
            if let Some(duration) = track.duration_ms {
                ensure!(
                    duration <= MAX_POSITION && self.position_ms <= duration,
                    "Invalid persisted duration or position"
                );
            }
            for url in [&track.artwork_url, &track.spotify_url]
                .into_iter()
                .flatten()
            {
                validate_url(url)?;
            }
        }
        match &self.last_intent {
            None => {}
            Some(PlayerCommand::PlayContext { uri, track, .. }) => {
                validate_uri(uri)?;
                if let Some(track) = track {
                    validate_uri(track)?;
                }
            }
            Some(PlayerCommand::PlayTracks { uris, start, .. }) => {
                ensure!(
                    !uris.is_empty() && uris.len() <= MAX_ITEMS,
                    "Invalid persisted track list"
                );
                for uri in uris {
                    validate_uri(uri)?;
                }
                if let Some(start) = start {
                    validate_uri(start)?;
                    ensure!(
                        uris.contains(start),
                        "Persisted start is outside track list"
                    );
                }
            }
            _ => anyhow::bail!("Invalid persisted player intent"),
        }
        Ok(())
    }
}

fn validate_text(value: &str) -> Result<()> {
    ensure!(
        value.len() <= MAX_TEXT && !value.contains('\0'),
        "Invalid persisted text"
    );
    Ok(())
}

fn validate_uri(value: &str) -> Result<()> {
    ensure!(value.len() <= MAX_TEXT, "Persisted URI exceeds bound");
    let rest = value
        .strip_prefix("spotify:")
        .context("Persisted URI must be a catalog identifier")?;
    let (kind, id) = rest
        .split_once(':')
        .context("Invalid persisted catalog identifier")?;
    ensure!(
        matches!(
            kind,
            "track" | "episode" | "album" | "artist" | "playlist" | "collection" | "user"
        ) && !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b":_.-".contains(&b)),
        "Invalid persisted catalog identifier"
    );
    Ok(())
}

fn validate_url(value: &str) -> Result<()> {
    ensure!(
        value.len() <= MAX_TEXT
            && !value.chars().any(|c| c.is_control() || c.is_whitespace())
            && !value.contains(['\\', '@', '?', '#']),
        "Invalid persisted metadata URL"
    );
    let rest = value
        .strip_prefix("https://")
        .context("Persisted metadata URL must be HTTPS")?;
    let host = rest.split('/').next().unwrap_or_default();
    ensure!(
        matches!(
            host,
            "open.spotify.com" | "i.scdn.co" | "mosaic.scdn.co" | "image-cdn-ak.spotifycdn.com"
        ) || host.ends_with(".scdn.co")
            || host.ends_with(".spotifycdn.com"),
        "Invalid persisted metadata URL host"
    );
    Ok(())
}

fn validate_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && path.as_os_str().as_bytes().len() <= MAX_TEXT,
        "Player path must be bounded and absolute"
    );
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
        "Player path must not contain traversal components"
    );
    let name = path.file_name().context("Missing player filename")?;
    ensure!(
        name != OsStr::new("state.json") && name.as_bytes().len() <= 255,
        "Refusing legacy or invalid player filename"
    );
    Ok(())
}

fn cstring(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).context("Invalid player path component")
}

fn open_parent(path: &Path) -> Result<(File, CString)> {
    validate_path(path)?;
    let mut directory = File::open("/").context("Open filesystem root")?;
    let parent = path.parent().context("Missing player parent")?;
    for component in parent.components() {
        if let Component::Normal(part) = component {
            let part = cstring(part)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    part.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error()).context("Open player parent");
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
    }
    let metadata = directory.metadata()?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == 0o700,
        "Player parent must be owned and mode0700"
    );
    Ok((
        directory,
        cstring(path.file_name().context("Missing player filename")?)?,
    ))
}

fn is_missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
}

fn read_destination(parent: &File, name: &CString) -> Result<Option<(Metadata, SavedPlayer)>> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("Open player state");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == 0o600
            && metadata.nlink() == 1
            && metadata.len() <= MAX_BYTES,
        "Player state must be a bounded owned mode0600 regular file"
    );
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("Read player state")?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Player state exceeds size bound"
    );
    ensure!(
        same_destination(Some(&metadata), Some(&file.metadata()?)),
        "Player state changed during read"
    );
    let envelope: Envelope =
        serde_json::from_slice(&bytes).context("Invalid player state schema")?;
    ensure!(
        envelope.schema == SCHEMA && envelope.schema_version == VERSION,
        "Unknown player state schema"
    );
    envelope.player.validate()?;
    Ok(Some((metadata, envelope.player)))
}

fn same_destination(left: Option<&Metadata>, right: Option<&Metadata>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.dev() == b.dev()
                && a.ino() == b.ino()
                && a.len() == b.len()
                && a.mtime() == b.mtime()
                && a.mtime_nsec() == b.mtime_nsec()
                && a.ctime() == b.ctime()
                && a.ctime_nsec() == b.ctime_nsec()
        }
        _ => false,
    }
}

fn create_temporary(parent: &File) -> Result<(File, CString)> {
    for _ in 0..16 {
        let name = CString::new(format!(".myx-player-{:032x}.tmp", rand::random::<u128>()))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd >= 0 {
            let file = unsafe { File::from_raw_fd(fd) };
            // fchmod is independent of the process umask.
            if unsafe { libc::fchmod(fd, 0o600) } != 0 {
                let error = io::Error::last_os_error();
                unsafe {
                    libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0);
                }
                return Err(error).context("Protect player temporary file");
            }
            return Ok((file, name));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error).context("Create player temporary file");
        }
    }
    anyhow::bail!("Cannot allocate player temporary file")
}

fn publish(parent: &File, temporary: &CString, destination: &CString, absent: bool) -> Result<()> {
    let fd = parent.as_raw_fd();
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            fd,
            temporary.as_ptr(),
            fd,
            destination.as_ptr(),
            if absent { libc::RENAME_EXCL } else { 0 },
        )
    };
    #[cfg(not(target_os = "macos"))]
    let result = if absent {
        // linkat supplies portable no-replace publication when no destination was
        // observed. The temporary link is removed immediately, before returning.
        unsafe { libc::linkat(fd, temporary.as_ptr(), fd, destination.as_ptr(), 0) }
    } else {
        unsafe { libc::renameat(fd, temporary.as_ptr(), fd, destination.as_ptr()) }
    };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("Publish player state");
    }
    #[cfg(not(target_os = "macos"))]
    if absent && unsafe { libc::unlinkat(fd, temporary.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error()).context("Unlink player temporary file");
    }
    Ok(())
}

struct Slot {
    latest: Option<SavedPlayer>,
    stopping: bool,
    error: Option<String>,
}

struct Shared {
    slot: Mutex<Slot>,
    changed: Condvar,
}

/// A single owned worker with one replaceable pending snapshot, never a queue.
pub struct Writer {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Writer {
    pub fn start(path: PathBuf) -> Result<Self> {
        validate_path(&path)?;
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot {
                latest: None,
                stopping: false,
                error: None,
            }),
            changed: Condvar::new(),
        });
        let owned = shared.clone();
        let worker = thread::Builder::new()
            .name("myx-player-persist".into())
            .spawn(move || loop {
                let state = {
                    let mut slot = owned.slot.lock().unwrap_or_else(|e| e.into_inner());
                    while slot.latest.is_none() && !slot.stopping {
                        slot = owned.changed.wait(slot).unwrap_or_else(|e| e.into_inner());
                    }
                    match slot.latest.take() {
                        Some(state) => state,
                        None => break,
                    }
                };
                if let Err(error) = state.save(&path) {
                    owned.slot.lock().unwrap_or_else(|e| e.into_inner()).error =
                        Some(format!("{error:#}"));
                }
            })
            .context("Start player persistence worker")?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    pub fn submit(&self, state: SavedPlayer) {
        let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.stopping {
            slot.error
                .get_or_insert_with(|| "Player persistence writer has stopped".into());
            return;
        }
        slot.latest = Some(state);
        self.shared.changed.notify_one();
    }

    pub fn error(&self) -> Option<&'static str> {
        self.shared
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .as_ref()
            .map(|_| "Player persistence failed")
    }

    pub fn finish(&mut self) -> Result<()> {
        {
            let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
            slot.stopping = true;
            self.shared.changed.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.shared
                    .slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .error = Some("Player persistence worker panicked".into());
            }
        }
        if let Some(error) = &self
            .shared
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
        {
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ItemKind, ServiceState};
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture() -> SavedPlayer {
        let mut snapshot = Snapshot::starting("test-config".into(), "test-device".into());
        let mut track = TrackMetadata::pending(ItemKind::Track, "spotify:track:test".into());
        track.title = Some("夜の\"音楽\"\n".into());
        track.artists = Some(vec!["Björk".into()]);
        track.duration_ms = Some(100_000);
        snapshot.track = Some(track);
        snapshot.position.position_ms = 4_200;
        snapshot.position.speed = 1.0;
        snapshot.volume = 79;
        snapshot.shuffle = true;
        snapshot.repeat = RepeatMode::All;
        snapshot.equalizer.gains_db[0] = 4;
        SavedPlayer::from_snapshot(
            &snapshot,
            Some(PlayerCommand::PlayContext {
                uri: "spotify:album:test".into(),
                track: None,
                position_ms: 4200,
                shuffle: true,
            }),
        )
    }

    fn private_temp() -> tempfile::TempDir {
        // macOS /var is a symlink; canonicalize only the test-owned fixture root.
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    fn unicode_modes_and_position_restore_paused_without_service_identity() {
        let directory = private_temp();
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("player-v1.json");
        let state = fixture();
        state.save(&path).unwrap();
        let loaded = SavedPlayer::load(&path).unwrap().unwrap();
        let mut snapshot = Snapshot::starting("other-config".into(), "other-device".into());
        let identity = snapshot.session_id.clone();
        loaded.apply(&mut snapshot);
        assert_eq!(snapshot.track, state.track);
        assert_eq!(snapshot.position.position_ms, 4200);
        assert_eq!(snapshot.position.speed, 0.0);
        assert_eq!(snapshot.position.at(u64::MAX, None), 4200);
        assert_eq!(snapshot.playback, PlaybackStatus::Paused);
        assert_eq!(snapshot.volume, 79);
        assert!(snapshot.shuffle);
        assert_eq!(snapshot.repeat, RepeatMode::All);
        assert_eq!(snapshot.equalizer, state.equalizer);
        assert_eq!(snapshot.session_id, identity);
        assert_eq!(snapshot.config_path, "other-config");
        assert_eq!(snapshot.readiness, ServiceState::Starting);
        assert!(!snapshot.web_api_ready && !snapshot.soloist_ready && !snapshot.locally_active);
        assert!(!snapshot.owned_device_verified);
    }

    #[test]
    fn missing_load_does_not_create_directories_or_files() {
        let directory = private_temp();
        let root = directory.path().canonicalize().unwrap();
        assert!(SavedPlayer::load(&root.join("absent/player-v1.json"))
            .unwrap()
            .is_none());
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn refuses_foreign_schema_legacy_paths_links_and_wrong_permissions() {
        let directory = private_temp();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("player-v1.json");
        let state = fixture();
        fs::write(
            &path,
            b"{\"schema_version\":1,\"credentials\":\"unrelated\"}",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let original = fs::read(&path).unwrap();
        assert!(SavedPlayer::load(&path).is_err());
        assert!(state.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(state.save(&root.join("state.json")).is_err());
        assert!(state.save(&root.join("../escape.json")).is_err());
        fs::remove_file(&path).unwrap();
        state.save(&path).unwrap();
        let alias = root.join("alias.json");
        symlink(&path, &alias).unwrap();
        assert!(SavedPlayer::load(&alias).is_err());
        assert!(state.save(&alias).is_err());
        fs::remove_file(&alias).unwrap();
        fs::hard_link(&path, &alias).unwrap();
        assert!(SavedPlayer::load(&path).is_err());
        assert!(state.save(&path).is_err());
        fs::remove_file(&alias).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(SavedPlayer::load(&path).is_err());
        let parent_alias = root.join("parent-link");
        symlink(&root, &parent_alias).unwrap();
        assert!(state.save(&parent_alias.join("nested.json")).is_err());
        assert!(SavedPlayer::load(&parent_alias.join("nested.json")).is_err());
    }

    #[test]
    fn validates_bounds_and_intents_before_creating_parent() {
        let directory = private_temp();
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("absent/player.json");
        let mut state = fixture();
        state.volume = 101;
        assert!(state.save(&path).is_err());
        state.volume = 50;
        state.position_ms = u64::MAX;
        assert!(state.save(&path).is_err());
        state.position_ms = 0;
        state.last_intent = Some(PlayerCommand::Play);
        assert!(state.save(&path).is_err());
        state.last_intent = None;
        state.track.as_mut().unwrap().uri = "file:///private/example".into();
        assert!(state.save(&path).is_err());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn atomic_updates_leave_old_open_inode_complete_and_no_temporary_names() {
        let directory = private_temp();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("player.json");
        let mut state = fixture();
        state.save(&path).unwrap();
        let mut old = File::open(&path).unwrap();
        state.volume = 12;
        state.save(&path).unwrap();
        let mut bytes = Vec::new();
        old.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            serde_json::from_slice::<Envelope>(&bytes)
                .unwrap()
                .player
                .volume,
            79
        );
        assert_eq!(SavedPlayer::load(&path).unwrap().unwrap().volume, 12);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(root).unwrap().count(), 1);
    }

    #[test]
    fn no_replace_publication_refuses_a_racing_unrelated_destination() {
        let directory = private_temp();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("player.json");
        let (parent, name) = open_parent(&path).unwrap();
        let (mut temporary, temporary_name) = create_temporary(&parent).unwrap();
        temporary.write_all(b"ours").unwrap();
        fs::write(&path, b"unrelated").unwrap();
        assert!(publish(&parent, &temporary_name, &name, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"unrelated");
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), temporary_name.as_ptr(), 0);
        }
    }

    #[test]
    fn rejects_unknown_version_oversize_and_nonprivate_parent() {
        let directory = private_temp();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("player.json");
        fixture().save(&path).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["schema_version"] = 2.into();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(SavedPlayer::load(&path).is_err());
        assert!(fixture().save(&path).is_err());
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(SavedPlayer::load(&path).is_err());
        fs::remove_file(&path).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(SavedPlayer::load(&path).is_err());
        assert!(fixture().save(&path).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn writer_coalesces_one_slot_flushes_latest_and_reports_failure() {
        let directory = private_temp();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("player.json");
        let mut writer = Writer::start(path.clone()).unwrap();
        for volume in 0..=100 {
            let mut state = fixture();
            state.volume = volume;
            writer.submit(state);
        }
        writer.finish().unwrap();
        assert_eq!(SavedPlayer::load(&path).unwrap().unwrap().volume, 100);
        assert!(writer.error().is_none());
        writer.finish().unwrap();
        let foreign = root.join("foreign.json");
        fs::write(&foreign, b"{}").unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
        let mut failed = Writer::start(foreign.clone()).unwrap();
        failed.submit(fixture());
        assert!(failed.finish().is_err());
        assert!(failed.error().is_some());
        assert_eq!(fs::read(foreign).unwrap(), b"{}");
    }
}
