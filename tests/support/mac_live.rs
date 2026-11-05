//! Owner-provisioned, fail-closed MAC6 harness. No service registration, SDK
//! invocation, authorization flow, key/token reads, global HOME change, PTY or
//! hardware-event synthesis. Only the copied release daemon is launched.
//!
//! Environment (all absolute paths, no symlink components):
//! MYX_LIVE_ALLOW=1; MYX_LIVE_HOME=<dedicated mode0700 home, not login HOME>;
//! MYX_TEST_CONFIG=<private regular config inside that home>;
//! MYX_TEST_ARTIFACT=<copied release executable outside checkout and target>;
//! MYX_LIVE_FIXTURES=<private public-JSON manifest inside that home>.
//! Library scenario additionally requires MYX_LIVE_WRITE_ALLOW=1.
//! The owner must provision pairing/credentials and isolate ALL config output,
//! cache, SDK and export paths beforehand. We inspect config metadata only.
//!
//! Manifest schema is exactly `Fixtures` below (deny_unknown_fields). URI cases
//! are actual dedicated account assets, not synthetic strings. Source entries
//! are checkout-relative public Rust/Cargo files with SHA256. Supply the entire
//! release source manifest, not private config/SDK assets. Baseline has measured
//! CPU/RSS and counter budgets; there are intentionally no defaults. Durations are seconds,
//! resource RSS is bytes, CPU is one-core percent. Counter budgets are maxima
//! over each whole >=60-second phase, not rates. The harness never widens them.
//!
//! The harness creates a private `mac6-*` evidence directory in fixture HOME,
//! prints its path, and writes challenge.json BEFORE accepting owner receipts.
//! Owner writes terminal.json, media-pause.json, media-play.json and c5.json in
//! that directory while the harness waits (each phase bounded to 120 seconds).
//! Structures are `TerminalReceipt`, `MediaReceipt`, `SignalReceipt` below.
//! All bind artifact/source/session hashes, daemon PID and challenge timestamp.
//! Terminal receipts contain real attached and reattached TUI PIDs, their exact
//! ps lstart identities and observed exit timestamps. TUI executable paths must
//! match the copied artifact. Hardware receipts are operator attestations backed
//! by live event-counter/state transitions, never an opaque `passed:true`.
//! Resource sequence: >=60s stable real-TUI window, owner close/reopen/close,
//! then >=300s stable zero-TUI playback on the same decoder, followed by >=60s
//! Paused and >=60s Stopped. resource_track must have >=900s duration. Transition
//! CPU is not qualified by ps polling: final CPU of non-child exit is unknown.
//! Any disappearing process inside a measured resource window fails the gate.
//! active-terminal.json uses ActiveTerminal below and is removed only by the
//! owner after the registered real TUI exits. Wait for attached-samples.json
//! before closing the first TUI. Missing/operator-unavailable receipts fail.
//!
//! Limits: these three scenarios do not certify C7 GUI/service registration,
//! auth refresh/expiry, takeover, device hotplug or crash recovery. Those remain
//! owner-required separate gates. No native/terminal/acoustic acceptance follows
//! from compilation or from an ignored test. A C5 signal receipt is required for
//! transport latency; estimated_latency_micros is reported separately and is NOT
//! acoustic proof. Receipt provenance still requires owner review.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use myx::audio::EqualizerSettings;
use myx::domain::{PlaybackStatus, PlayerCommand, RepeatMode, ServiceState, Snapshot};
use myx::ipc::protocol::{self, Operation, Payload, QueuePage, Request, VERSION};
use myx::ipc::unix::verify_peer;
use myx::webapi::HttpMethod;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const RPC: Duration = Duration::from_secs(30);
const OWNER_WAIT: Duration = Duration::from_secs(120);
const MAX_PUBLIC_JSON: u64 = 1024 * 1024;

#[derive(Clone, Copy)]
pub enum Scenario {
    Library,
    Resources,
    Media,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    path: String,
    sha256: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedRow {
    uri: String,
    row: u32,
    track: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LikedCase {
    source_position: u32,
    track: String,
    expected_playable: Vec<String>,
    start_index: u32,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Budgets {
    cpu_one_core_percent: f64,
    rss_bytes: u64,
    processed_frames: u64,
    fft_frames: u64,
    worker_wakes: u64,
    resets: u64,
    dropped_frames: u64,
    recovery_attempts: u64,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    // Public measured-baseline evidence, separately hashed and bound to source.
    evidence: String,
    sha256: String,
    source_sha256: String,
    measured_seconds: u64,
    paused: Budgets,
    idle: Budgets,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixtures {
    schema: u32,
    owned_home: String,
    isolated_config_paths_verified_by_owner: bool,
    media_allowance: bool,
    artifact_sha256: String,
    source_sha256: String,
    release_build_receipt: String,
    release_build_receipt_sha256: String,
    source_files: Vec<SourceFile>,
    track: String,
    episode: String,
    album: SelectedRow,
    playlist: SelectedRow,
    // >=101 tracks, selected occurrence >50, duplicate occurrence with distinct
    // neighbours. The entire original array is sent, without rotation/truncation.
    explicit_tracks: Vec<String>,
    duplicate_index: u32,
    liked: LikedCase,
    // Dedicated, initially unsaved track. No writes without separate allowance.
    write_track: String,
    owned_library_write_fixture: bool,
    // Long enough for owner interaction, >=60s attached and >=300s detached.
    // This harness requires >=900s metadata duration; no looping/reloads.
    resource_track: String,
    baseline: Baseline,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseBuildReceipt {
    artifact_sha256: String,
    source_sha256: String,
    profile: String,
    target: String,
    operator: String,
    built_ms: u64,
}

// Deserialize the raw Snapshot payload separately: missing primary integration
// must fail, not be silently erased by Snapshot's unknown-field handling.
#[derive(Clone, Deserialize, Serialize)]
struct AudioTelemetry {
    processed_frames: u64,
    fft_frames: u64,
    worker_wakes: u64,
    resets: u64,
    dropped_frames: u64,
    estimated_latency_micros: u64,
    latency_valid: bool,
    // Same encoding as soloist_runtime::pcm::AudioStats: 1 = ready.
    health: u8,
    recovery_attempts: u64,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
struct OwnedGroup {
    pid: u32,
    pgid: u32,
}

#[derive(Deserialize)]
struct TelemetryContract {
    audio: Option<AudioTelemetry>,
    owned_process_groups: Vec<OwnedGroup>,
    media_event_count: u64,
    last_media_command: Option<String>,
    spectrum_subscribers: u32,
}

struct Observation {
    snapshot: Snapshot,
    telemetry: TelemetryContract,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    artifact_sha256: String,
    source_sha256: String,
    session_sha256: String,
    daemon_pid: u32,
    challenge_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalReceipt {
    binding: Binding,
    operator: String,
    real_terminal: bool,
    navigation_input_art_clipboard_observed: bool,
    attached_pid: u32,
    attached_start: String,
    detached_ms: u64,
    reattached_pid: u32,
    reattached_start: String,
    reattached_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaReceipt {
    binding: Binding,
    operator: String,
    physical_media_key: bool,
    command: String,
    event_ms: u64,
    before_count: u64,
    after_count: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalReceipt {
    binding: Binding,
    operator: String,
    method: String,
    started_ms: u64,
    ended_ms: u64,
    input_signal_sha256: String,
    output_signal_sha256: String,
    // Actual C5 correlation measurements, not runtime buffer estimates.
    correlated_markers: u64,
    transport_latency_micros: u64,
    measured_processed_start: u64,
    measured_processed_end: u64,
    markers: Vec<SignalMarker>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalMarker {
    input_micros: u64,
    output_micros: u64,
    processed_frame: u64,
}

#[derive(Clone, Serialize)]
struct Process {
    pid: u32,
    ppid: u32,
    pgid: u32,
    uid: u32,
    cpu_percent: f64,
    rss_bytes: u64,
    cpu_seconds: f64,
    start: String,
    ucomm: String,
}

struct Harness {
    home: PathBuf,
    config: PathBuf,
    artifact: PathBuf,
    socket: PathBuf,
    child: Child,
    child_start: String,
    request_id: u64,
    session: Option<String>,
    auth_revision: Option<u64>,
    binding: Option<Binding>,
    fixture: Fixtures,
    evidence: tempfile::TempDir,
    seen: BTreeMap<u32, Process>,
    socket_identity: Option<(u64, u64)>,
    ipc_identities: BTreeMap<PathBuf, (u64, u64)>,
    terminal_seen: BTreeMap<u32, (String, u64)>,
    terminal_absent_ms: Vec<u64>,
    owned_groups: BTreeSet<u32>,
    finished: bool,
}

pub fn run(scenario: Scenario) -> Result<()> {
    ensure!(
        std::env::var("MYX_LIVE_ALLOW").as_deref() == Ok("1"),
        "GateMissing: MYX_LIVE_ALLOW=1"
    );
    if matches!(scenario, Scenario::Library) {
        ensure!(
            std::env::var("MYX_LIVE_WRITE_ALLOW").as_deref() == Ok("1"),
            "GateMissing: library-write acceptance requires MYX_LIVE_WRITE_ALLOW=1"
        );
    }
    let mut h = Harness::start()?;
    // Always attempt exact-child cleanup, including failed assertions/receipts.
    let result = (|| {
        h.ready()?;
        match scenario {
            Scenario::Library => h.library(),
            Scenario::Resources => h.resources(),
            Scenario::Media => h.media(),
        }
    })();
    let cleanup = h.finish();
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) => Err(e),
        (Ok(()), Err(e)) => Err(e),
        (Err(e), Err(cleanup)) => {
            Err(e.context(format!("Additionally owned cleanup failed: {cleanup}")))
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

fn env_path(name: &str) -> Result<PathBuf> {
    let path =
        PathBuf::from(std::env::var_os(name).with_context(|| format!("GateMissing: {name}"))?);
    ensure!(path.is_absolute(), "Fixture paths must be absolute");
    for part in path.ancestors() {
        let m = fs::symlink_metadata(part).context("Fixture path must already exist")?;
        ensure!(!m.file_type().is_symlink(), "Symlink fixture path refused");
    }
    ensure!(
        fs::canonicalize(&path)? == path,
        "Non-canonical fixture path refused"
    );
    Ok(path)
}

fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.nlink() == 1,
        "Expected private same-user regular fixture file"
    );
    Ok(file)
}

fn public_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    private_file(path)?
        .take(MAX_PUBLIC_JSON + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PUBLIC_JSON,
        "Public JSON exceeds bound"
    );
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Invalid public fixture/receipt schema (contents redacted)"))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut input = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn uri(value: &str, kind: &str) -> Result<()> {
    ensure!(
        myx::player::intents::valid_uri(value, &[kind]),
        "Missing/invalid dedicated fixture URI"
    );
    Ok(())
}

impl Fixtures {
    fn validate(&self, home: &Path, artifact: &Path) -> Result<()> {
        ensure!(
            self.schema == 1
                && Path::new(&self.owned_home) == home
                && self.isolated_config_paths_verified_by_owner,
            "GateMissing: owner must provision an isolated dedicated HOME/config"
        );
        ensure!(
            self.media_allowance,
            "GateMissing: explicit media allowance"
        );
        ensure!(
            valid_hash(&self.artifact_sha256) && hash_file(artifact)? == self.artifact_sha256,
            "Copied release artifact hash mismatch"
        );
        let build_path = Path::new(&self.release_build_receipt);
        ensure!(
            build_path.starts_with(home)
                && fs::canonicalize(build_path)? == build_path
                && valid_hash(&self.release_build_receipt_sha256),
            "GateMissing: public release build receipt inside fixture HOME"
        );
        let build: ReleaseBuildReceipt = public_json(build_path)?;
        ensure!(
            hash_file(build_path)? == self.release_build_receipt_sha256
                && build.artifact_sha256 == self.artifact_sha256
                && build.source_sha256 == self.source_sha256
                && build.profile == "release"
                && build.target == "aarch64-apple-darwin"
                && !build.operator.trim().is_empty()
                && build.built_ms > 0
                && build.built_ms <= now_ms(),
            "Release build receipt source/artifact/profile/target mismatch"
        );
        ensure!(
            valid_hash(&self.source_sha256) && !self.source_files.is_empty(),
            "GateMissing: source manifest"
        );
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut manifest = String::new();
        let mut unique = BTreeSet::new();
        for entry in &self.source_files {
            let p = Path::new(&entry.path);
            ensure!(
                !p.is_absolute()
                    && p.components().all(|c| matches!(c, Component::Normal(_)))
                    && (entry.path.ends_with(".rs")
                        || p.file_name().is_some_and(|n| n == "Cargo.toml")
                        || entry.path == "Cargo.lock")
                    && (entry.path.starts_with("src/")
                        || entry.path.starts_with("crates/")
                        || entry.path.starts_with("tests/")
                        || entry.path.starts_with("packaging/")
                        || matches!(
                            entry.path.as_str(),
                            "Cargo.toml" | "Cargo.lock" | "build.rs"
                        ))
                    && unique.insert(entry.path.clone()),
                "Source manifest must contain unique public source paths only"
            );
            let full = root.join(p);
            ensure!(
                fs::canonicalize(&full)? == full,
                "Source manifest symlink refused"
            );
            ensure!(
                valid_hash(&entry.sha256) && hash_file(&full)? == entry.sha256,
                "Release source hash mismatch"
            );
            manifest.push_str(&format!("{}  {}\n", entry.sha256, entry.path));
        }
        ensure!(
            digest(manifest.as_bytes()) == self.source_sha256,
            "Source manifest aggregate hash mismatch (ordered sha256, two spaces, path, newline)"
        );
        for t in [&self.track, &self.write_track, &self.resource_track] {
            uri(t, "track")?;
        }
        uri(&self.episode, "episode")?;
        for (case, kind) in [(&self.album, "album"), (&self.playlist, "playlist")] {
            uri(&case.uri, kind)?;
            uri(&case.track, "track")?;
        }
        ensure!(
            self.playlist.row > 50,
            "GateMissing: playlist selected row beyond first page"
        );
        ensure!(
            self.explicit_tracks.len() > 100 && self.duplicate_index > 50,
            "GateMissing: large complete explicit source / duplicate index"
        );
        for t in &self.explicit_tracks {
            uri(t, "track")?;
        }
        let i = self.duplicate_index as usize;
        ensure!(
            i > 0 && i + 1 < self.explicit_tracks.len(),
            "Duplicate index out of bounds"
        );
        ensure!(
            self.explicit_tracks[..i].contains(&self.explicit_tracks[i])
                && self.explicit_tracks[i - 1] != self.explicit_tracks[i]
                && self.explicit_tracks[i + 1] != self.explicit_tracks[i],
            "GateMissing: disambiguatable duplicate occurrence"
        );
        ensure!(
            self.liked.source_position > 50
                && self.liked.expected_playable.len() > 100
                && self.liked.start_index > 0
                && self.liked.source_position > self.liked.start_index,
            "GateMissing: all-page Liked case with original-row filtering before selection"
        );
        ensure!(
            self.liked
                .expected_playable
                .get(self.liked.start_index as usize)
                == Some(&self.liked.track),
            "Liked occurrence mismatch"
        );
        for t in &self.liked.expected_playable {
            uri(t, "track")?;
        }
        Ok(())
    }
}

impl Harness {
    fn start() -> Result<Self> {
        // No file/config/native work before the explicit allowance above.
        let home = env_path("MYX_LIVE_HOME")?;
        let real_home = std::env::var_os("HOME").context("Login HOME missing")?;
        let real_home = fs::canonicalize(real_home)?;
        ensure!(
            home != real_home && !real_home.starts_with(&home),
            "Real profile HOME refused"
        );
        let m = fs::symlink_metadata(&home)?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "Fixture HOME must be owned mode0700"
        );
        let config = env_path("MYX_TEST_CONFIG")?;
        ensure!(
            config.starts_with(&home),
            "Config must be inside owned HOME"
        );
        drop(private_file(&config)?); // Never read key/token/config contents.
        let artifact = env_path("MYX_TEST_ARTIFACT")?;
        let checkout = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))?;
        ensure!(
            !artifact.starts_with(&checkout)
                && !artifact
                    .components()
                    .any(|c| c.as_os_str() == "target" || c.as_os_str() == "debug"),
            "GateMissing: copied release artifact outside checkout/target/debug"
        );
        let am = fs::metadata(&artifact)?;
        ensure!(
            am.is_file()
                && am.uid() == unsafe { libc::geteuid() }
                && am.mode() & 0o022 == 0
                && am.mode() & 0o111 != 0,
            "Unsafe copied executable metadata"
        );
        let fixtures_path = env_path("MYX_LIVE_FIXTURES")?;
        ensure!(
            fixtures_path.starts_with(&home),
            "Public fixtures must be inside owned HOME"
        );
        let fixture: Fixtures = public_json(&fixtures_path)?;
        fixture.validate(&home, &artifact)?;
        // Do not connect to or remove ANY pre-existing fixture runtime owner.
        let runtime = home.join(".cache/myx/daemon");
        for ancestor in runtime
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(&home))
        {
            match fs::symlink_metadata(ancestor) {
                Ok(m) => ensure!(
                    m.is_dir()
                        && !m.file_type().is_symlink()
                        && m.uid() == unsafe { libc::geteuid() }
                        && m.mode() & 0o077 == 0,
                    "Unsafe fixture cache parent"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        ensure!(
            matches!(fs::symlink_metadata(&runtime), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "Existing fixture runtime refused: owner must reconcile it manually"
        );
        if matches!(std::env::var("MYX_LIVE_WRITE_ALLOW").as_deref(), Ok("1")) {
            ensure!(
                fixture.owned_library_write_fixture,
                "GateMissing: dedicated owned library write fixture"
            );
        }
        let mut evidence = tempfile::Builder::new().prefix("mac6-").tempdir_in(&home)?;
        evidence.disable_cleanup(true);
        fs::set_permissions(evidence.path(), fs::Permissions::from_mode(0o700))?;
        // Null streams intentionally avoid capturing credential-bearing SDK logs.
        // No service adapter, detached launcher, inherited env/config or args scan.
        let child = Command::new(&artifact).args(["--config"]).arg(&config).args(["daemon", "run"])
            .env_clear().env("HOME", &home).env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("TMPDIR", evidence.path()).env("CI", "true")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .process_group(0).spawn().context("Owned foreground daemon spawn failed; owner must check copied release/SDK provisioning")?;
        let mut h = Self {
            home,
            config,
            artifact,
            socket: runtime.join("daemon-v1.sock"),
            child,
            child_start: String::new(),
            request_id: 0,
            session: None,
            auth_revision: None,
            binding: None,
            fixture,
            evidence,
            seen: BTreeMap::new(),
            socket_identity: None,
            ipc_identities: BTreeMap::new(),
            terminal_seen: BTreeMap::new(),
            terminal_absent_ms: Vec::new(),
            owned_groups: BTreeSet::new(),
            finished: false,
        };
        let processes = h.ps()?;
        let p = processes.get(&h.child.id()).context(
            "Owned child exited before identity capture; owner must check SDK/auth/setup",
        )?;
        ensure!(
            p.pgid == p.pid && p.uid == unsafe { libc::geteuid() },
            "Owned daemon process-group mismatch"
        );
        h.child_start = p.start.clone();
        h.owned_groups.insert(p.pgid);
        h.seen.insert(p.pid, p.clone());
        Ok(h)
    }

    fn rpc(&mut self, operation: Operation) -> Result<Value> {
        ensure!(self.child.try_wait()?.is_none(), "Owner startup/runtime exited; check fixture auth/SDK setup (logs deliberately not captured)");
        let m = fs::symlink_metadata(&self.socket)?;
        ensure!(
            m.file_type().is_socket()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0,
            "Unsafe fixture IPC endpoint"
        );
        let identity = (m.dev(), m.ino());
        if let Some(expected) = self.socket_identity {
            ensure!(identity == expected, "Fixture IPC owner replaced");
        }
        self.socket_identity = Some(identity);
        let runtime = self.socket.parent().context("IPC parent")?;
        for path in [
            runtime.to_owned(),
            runtime.join("owner.lock"),
            runtime.join("startup.lock"),
        ] {
            match fs::symlink_metadata(&path) {
                Ok(m) => {
                    ensure!(
                        !m.file_type().is_symlink()
                            && m.uid() == unsafe { libc::geteuid() }
                            && m.mode() & 0o077 == 0,
                        "Unsafe owned IPC directory/lock"
                    );
                    let identity = (m.dev(), m.ino());
                    if let Some(old) = self.ipc_identities.insert(path, identity) {
                        ensure!(old == identity, "IPC directory/lock replaced");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(RPC))?;
        stream.set_write_timeout(Some(RPC))?;
        verify_peer(&stream).context("IPC requires strict same-user peer")?;
        // Darwin LOCAL_PEERPID (sys/un.h), in addition to getpeereid above.
        // Never trust a Snapshot merely because it claims our child's PID.
        use std::os::fd::AsRawFd;
        let mut peer_pid: libc::pid_t = 0;
        let mut length = std::mem::size_of_val(&peer_pid) as libc::socklen_t;
        let peer_result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                0,
                0x002,
                (&mut peer_pid as *mut libc::pid_t).cast(),
                &mut length,
            )
        };
        ensure!(
            peer_result == 0
                && length as usize == std::mem::size_of_val(&peer_pid)
                && peer_pid > 0
                && peer_pid as u32 == self.child.id(),
            "IPC peer is not exact owned foreground child"
        );
        self.request_id = self
            .request_id
            .checked_add(1)
            .context("Request ID exhausted")?;
        protocol::send(
            &mut stream,
            &Request {
                protocol_version: VERSION,
                request_id: self.request_id,
                config_path: Some(self.config.to_str().context("UTF8 config required")?.into()),
                operation,
            },
        )?;
        // Retain raw snapshot extensions without changing the production domain.
        // receive enforces MAX_FRAME before allocating its body.
        let response: Value = protocol::receive(&mut stream)?;
        ensure!(
            response["protocol_version"].as_u64() == Some(u64::from(VERSION))
                && response["request_id"].as_u64() == Some(self.request_id),
            "IPC version/request identity mismatch"
        );
        // Also validate the actual typed Response envelope, not a JSON PASS shim.
        let _: myx::ipc::protocol::Response = serde_json::from_value(response.clone())
            .map_err(|_| anyhow::anyhow!("Invalid typed IPC response"))?;
        if let Some(error) = response["result"].get("Err") {
            let code: protocol::ErrorCode = serde_json::from_value(error["code"].clone())?;
            bail!("MAC6 IPC failed: {code:?}; owner must resolve auth/403/quota/SDK/setup, never skip the gate");
        }
        response["result"]
            .get("Ok")
            .cloned()
            .context("Missing IPC result")
    }

    fn observe(&mut self) -> Result<Observation> {
        let value = self.rpc(Operation::Status)?;
        ensure!(value["type"] == "snapshot", "Expected native snapshot");
        let raw = value["value"].clone();
        for field in [
            "audio",
            "owned_process_groups",
            "media_event_count",
            "last_media_command",
            "spectrum_subscribers",
        ] {
            ensure!(
                raw.get(field).is_some(),
                "GateMissing: primary Snapshot.{field}"
            );
        }
        let snapshot: Snapshot = serde_json::from_value(raw.clone())
            .map_err(|_| anyhow::anyhow!("Invalid Snapshot schema"))?;
        let telemetry: TelemetryContract = serde_json::from_value(raw).map_err(|_| anyhow::anyhow!(
            "GateMissing: primary Snapshot audio/owned_process_groups/media_event_count/last_media_command/spectrum_subscribers contract"))?;
        ensure!(
            snapshot.pid == self.child.id()
                && snapshot.config_path == self.config.to_str().context("UTF8 config")?,
            "Snapshot PID/config mismatch"
        );
        ensure!(!snapshot.session_id.is_empty(), "Missing session identity");
        if let Some(session) = &self.session {
            ensure!(*session == snapshot.session_id, "Daemon session changed");
        }
        self.session = Some(snapshot.session_id.clone());
        if let Some(revision) = self.auth_revision {
            ensure!(
                revision == snapshot.auth_revision,
                "Auth revision changed during scenario"
            );
        }
        ensure!(
            snapshot.control_error.is_none() && snapshot.output_error.is_none(),
            "Observed native control/output error (details redacted)"
        );
        Ok(Observation {
            snapshot,
            telemetry,
        })
    }

    fn ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + OWNER_WAIT;
        while !self.socket.exists() {
            ensure!(
                self.child.try_wait()?.is_none(),
                "Owner startup exited; provision fixture config, SDK and auth before retrying"
            );
            ensure!(
                Instant::now() < deadline,
                "Owner startup timed out; inspect fixture provisioning, not ordinary user daemon"
            );
            thread::sleep(Duration::from_millis(200));
        }
        loop {
            let o = self.observe()?;
            ensure!(!matches!(o.snapshot.readiness, ServiceState::SetupRequired | ServiceState::Error),
                "GateMissing: owner must preauthorize fixture and provision SDK/key; harness will not authorize");
            if o.snapshot.readiness == ServiceState::Ready
                && o.snapshot.web_api_ready
                && o.snapshot.catalog_available
                && o.snapshot.soloist_ready
                && o.snapshot.owned_device_verified
                && o.snapshot.native_media_ready
            {
                self.auth_revision = Some(o.snapshot.auth_revision);
                let binding = Binding {
                    artifact_sha256: self.fixture.artifact_sha256.clone(),
                    source_sha256: self.fixture.source_sha256.clone(),
                    session_sha256: digest(o.snapshot.session_id.as_bytes()),
                    daemon_pid: self.child.id(),
                    challenge_ms: now_ms(),
                };
                self.write_json("challenge.json", &json!({"binding": binding, "fixture_home": self.home,
                    "artifact": self.artifact, "config": self.config, "owner_wait_seconds": OWNER_WAIT.as_secs(),
                    "required_files": ["terminal.json", "media-pause.json", "media-play.json", "c5.json"]}))?;
                self.binding = Some(binding);
                eprintln!(
                    "MAC6 owner challenge/evidence directory: {}",
                    self.evidence.path().display()
                );
                self.sample(&o, None)?;
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "GateMissing: native/auth/owned-device/media readiness timed out"
            );
            thread::sleep(Duration::from_millis(200));
        }
    }

    fn write_json(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.evidence.path().join(name))?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn journal(&self, value: &Value) -> Result<()> {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.evidence.path().join("observations.jsonl"))?;
        let m = file.metadata()?;
        ensure!(
            m.is_file()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0
                && m.nlink() == 1,
            "Unsafe evidence journal"
        );
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn playing(o: &Observation) -> Result<&AudioTelemetry> {
        ensure!(
            o.snapshot.playback == PlaybackStatus::Playing
                && o.snapshot.locally_active
                && o.snapshot.owned_device_verified
                && o.snapshot.web_api_ready
                && o.snapshot.soloist_ready
                && o.snapshot.readiness == ServiceState::Ready
                && !o.snapshot.control_pending,
            "Uninterrupted authenticated locally requested Playing/ownership gate failed"
        );
        let audio = o
            .telemetry
            .audio
            .as_ref()
            .context("GateMissing: actual audio telemetry")?;
        ensure!(
            audio.health == 1,
            "Audio health not ready (AudioStats health=1 required)"
        );
        Ok(audio)
    }

    fn command(
        &mut self,
        command: PlayerCommand,
        predicate: impl Fn(&Snapshot) -> bool,
    ) -> Result<Observation> {
        let before = self.observe()?.snapshot;
        let ack = self.rpc(Operation::Player(command))?;
        ensure!(
            ack["type"] == "accepted",
            "Expected control submission acknowledgement"
        );
        let deadline = Instant::now() + RPC;
        loop {
            let o = self.observe()?;
            if o.snapshot.sequence > before.sequence
                && o.snapshot.control_id > before.control_id
                && !o.snapshot.control_pending
                && predicate(&o.snapshot)
            {
                self.journal(&json!({"binding": self.binding, "timestamp_ms": now_ms(), "kind": "observed_control",
                    "sequence": o.snapshot.sequence, "control_id": o.snapshot.control_id,
                    "track_uri": o.snapshot.track.as_ref().map(|t| &t.uri), "context_uri": o.snapshot.context_uri,
                    "playback": o.snapshot.playback, "observed_position": o.snapshot.observed_position,
                    "shuffle": o.snapshot.shuffle, "repeat": o.snapshot.repeat,
                    "options_observed_revision": o.snapshot.options_observed_revision,
                    "queue_revision": o.snapshot.queue_revision,
                    "load_requires_live_qualification": o.snapshot.load_requires_live_qualification}))?;
                return Ok(o);
            }
            ensure!(
                Instant::now() < deadline,
                "Control ACK never became required observed native state"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn load(
        &mut self,
        command: PlayerCommand,
        expected: &str,
        context: Option<&str>,
    ) -> Result<Observation> {
        let o = self.command(command, |s| {
            s.playback == PlaybackStatus::Playing
                && s.track.as_ref().is_some_and(|t| t.uri == expected)
                && context.is_none_or(|c| s.context_uri.as_deref() == Some(c))
        })?;
        Self::playing(&o)?;
        Ok(o)
    }

    fn catalog(&mut self, method: HttpMethod, path: String, body: Option<Value>) -> Result<Value> {
        let response = self.rpc(Operation::Catalog { method, path, body })?;
        ensure!(response["type"] == "json", "Expected catalog JSON");
        Ok(response["value"].clone())
    }

    fn rows(&mut self, base: &str) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        let mut total = None;
        for _ in 0..10000 {
            let page = self.catalog(
                HttpMethod::Get,
                format!("{base}?limit=50&offset={}", rows.len()),
                None,
            )?;
            ensure!(
                page["offset"].as_u64() == Some(rows.len() as u64),
                "Catalog page offset mismatch"
            );
            let n = page["total"].as_u64().context("Catalog total missing")? as usize;
            if let Some(t) = total {
                ensure!(t == n, "Collection changed across pages");
            }
            total = Some(n);
            let items = page["items"].as_array().context("Catalog items missing")?;
            ensure!(
                items.len() <= 50 && rows.len() + items.len() <= n,
                "Catalog page overflow"
            );
            rows.extend(items.iter().cloned());
            if rows.len() == n {
                return Ok(rows);
            }
            ensure!(
                !items.is_empty() && !page["next"].is_null(),
                "Catalog ended before complete source"
            );
        }
        bail!("Catalog pagination bound exhausted; no truncated acceptance")
    }

    fn queue(&mut self, previous: bool) -> Result<Vec<myx::domain::queue::QueueEntry>> {
        let snapshot = self.observe()?.snapshot;
        let mut entries = Vec::new();
        let mut expected_total = None;
        for _ in 0..10000 {
            let value = self.rpc(Operation::QueuePage {
                revision: Some(snapshot.queue_revision),
                offset: entries.len(),
                limit: 50,
                previous,
                metadata: false,
            })?;
            let payload: Payload = serde_json::from_value(value)?;
            let Payload::QueuePage(QueuePage {
                session_id,
                auth_revision,
                revision,
                offset,
                total,
                previous: p,
                entries: page,
                ..
            }) = payload
            else {
                bail!("Expected native QueuePage");
            };
            ensure!(
                Some(&session_id) == self.session.as_ref()
                    && Some(auth_revision) == self.auth_revision
                    && revision == snapshot.queue_revision
                    && offset == entries.len()
                    && p == previous,
                "Queue session/auth/revision/page mismatch"
            );
            if let Some(t) = expected_total {
                ensure!(t == total, "Native queue total changed");
            }
            expected_total = Some(total);
            ensure!(
                page.len() <= 50 && entries.len() + page.len() <= total,
                "Queue page overflow"
            );
            for entry in &page {
                ensure!(!entry.uid.is_empty(), "Missing native occurrence UID");
            }
            let empty = page.is_empty();
            entries.extend(page);
            if entries.len() == total {
                self.journal(&json!({"binding": self.binding, "timestamp_ms": now_ms(), "kind": "native_queue",
                    "revision": revision, "previous": previous, "total": total, "entries": entries}))?;
                return Ok(entries);
            }
            ensure!(!empty, "Truncated native queue");
        }
        bail!("Queue pagination bound exhausted")
    }

    fn library(&mut self) -> Result<()> {
        ensure!(std::env::var("MYX_LIVE_WRITE_ALLOW").as_deref() == Ok("1") && self.fixture.owned_library_write_fixture,
            "GateMissing: MYX_LIVE_WRITE_ALLOW=1 and dedicated owned library write fixture; read-only is not library-write acceptance");
        let f = self.fixture.clone();
        for (case, kind) in [(&f.album, "albums"), (&f.playlist, "playlists")] {
            let id = case.uri.rsplit(':').next().context("Context ID")?;
            let suffix = if kind == "albums" { "tracks" } else { "items" };
            let playlist_revision = if kind == "playlists" {
                Some(
                    self.catalog(HttpMethod::Get, format!("/v1/playlists/{id}"), None)?
                        ["snapshot_id"]
                        .as_str()
                        .context("GateMissing: playlist snapshot revision")?
                        .to_owned(),
                )
            } else {
                None
            };
            let rows = self.rows(&format!("/v1/{kind}/{id}/{suffix}"))?;
            let row = rows
                .get(case.row as usize)
                .context("GateMissing: selected original row")?;
            let item = row.get("item").or_else(|| row.get("track")).unwrap_or(row);
            ensure!(
                item["uri"].as_str() == Some(&case.track),
                "Original selected row changed"
            );
            if let Some(revision) = playlist_revision {
                let after = self.catalog(HttpMethod::Get, format!("/v1/playlists/{id}"), None)?;
                ensure!(
                    after["snapshot_id"].as_str() == Some(&revision),
                    "Playlist revision changed across all-page read"
                );
                ensure!(
                    rows[..case.row as usize]
                        .iter()
                        .filter_map(context_row_uri)
                        .any(|u| u == case.track),
                    "GateMissing: playlist original-row duplicate occurrence case"
                );
            }
            self.load(
                PlayerCommand::PlayContextAt {
                    uri: case.uri.clone(),
                    track: Some(case.track.clone()),
                    row_position: case.row,
                    position_ms: 0,
                    shuffle: false,
                },
                &case.track,
                Some(&case.uri),
            )?;
            let expected: Vec<_> = rows[case.row as usize + 1..]
                .iter()
                .filter_map(context_row_uri)
                .collect();
            let actual = self.queue(false)?;
            ensure!(actual.iter().map(|e| e.uri.as_str()).eq(expected),
                "Native context source suffix incomplete or selected original occurrence wrong; not catalog-context qualification");
        }
        for playable in [&f.track, &f.episode] {
            self.load(
                PlayerCommand::PlayContext {
                    uri: playable.clone(),
                    track: None,
                    position_ms: 0,
                    shuffle: false,
                },
                playable,
                None,
            )?;
        }
        let rows = self.rows("/v1/me/tracks")?;
        let selected = rows
            .get(f.liked.source_position as usize)
            .context("Liked source row unavailable")?;
        ensure!(
            selected["track"]["uri"].as_str() == Some(&f.liked.track),
            "Liked source position changed"
        );
        let observed: Vec<String> = rows
            .iter()
            .filter_map(myx::player::intents::liked_track)
            .filter_map(|t| t["uri"].as_str().map(str::to_owned))
            .collect();
        ensure!(
            observed == f.liked.expected_playable,
            "All-page Liked source differs from owned case"
        );
        self.load(
            PlayerCommand::PlayLiked {
                start: Some(f.liked.track.clone()),
                source_position: Some(f.liked.source_position),
                position_ms: 0,
                shuffle: false,
            },
            &f.liked.track,
            None,
        )?;
        self.qualify_array(&f.liked.expected_playable, f.liked.start_index as usize)?;
        self.load(
            PlayerCommand::PlayTracksAt {
                uris: f.explicit_tracks.clone(),
                start_index: f.duplicate_index,
                position_ms: 0,
                shuffle: false,
            },
            &f.explicit_tracks[f.duplicate_index as usize],
            None,
        )?;
        self.qualify_array(&f.explicit_tracks, f.duplicate_index as usize)?;
        let before = self.queue(false)?;
        self.command(
            PlayerCommand::Add {
                uri: f.episode.clone(),
            },
            |s| s.queue_observed_revision > 0,
        )?;
        let after = self.queue(false)?;
        ensure!(
            after.iter().filter(|e| e.uri == f.episode).count()
                > before.iter().filter(|e| e.uri == f.episode).count(),
            "Queue add ACK without actual added occurrence"
        );
        let id = f
            .write_track
            .rsplit(':')
            .next()
            .context("Write fixture ID")?;
        let contains_path = format!("/v1/me/tracks/contains?ids={id}");
        ensure!(
            self.catalog(HttpMethod::Get, contains_path.clone(), None)? == json!([false]),
            "Dedicated write fixture must initially be unsaved; refusing existing library mutation"
        );
        self.catalog(
            HttpMethod::Put,
            "/v1/me/tracks".into(),
            Some(json!({"ids": [id]})),
        )?;
        ensure!(
            self.catalog(HttpMethod::Get, contains_path.clone(), None)? == json!([true]),
            "Save not observed; owner must reconcile dedicated write fixture before rerun"
        );
        self.catalog(
            HttpMethod::Delete,
            "/v1/me/tracks".into(),
            Some(json!({"ids": [id]})),
        )?;
        ensure!(
            self.catalog(HttpMethod::Get, contains_path, None)? == json!([false]),
            "Library restore not observed; owner must reconcile dedicated write fixture"
        );
        self.write_json("library-observations.json", &json!({"binding": self.binding,
            "explicit_array_offset": "observed selected occurrence, complete queue, previous, repeat wrap and shuffle",
            "daemon_qualification_flag_is_not_acceptance": true,
            "liked_rows": rows.len(), "explicit_rows": f.explicit_tracks.len(), "write_fixture_restored": true}))
    }

    fn qualify_array(&mut self, source: &[String], index: usize) -> Result<()> {
        // An Accepted/204 or the daemon's qualification flag proves nothing.
        // Observe the complete remaining native source and preserve duplicates.
        let upcoming = self.queue(false)?;
        ensure!(upcoming.iter().map(|e| &e.uri).eq(source[index + 1..].iter()),
            "Native queue does not expose complete original suffix; explicit-array offset UNQUALIFIED");
        // Initial selected row was already observed before these controls. This
        // traversal qualifies retained earlier source entries, never simulates
        // the initial offset by skipping from the head.
        for previous_index in (0..index).rev() {
            self.command(PlayerCommand::Seek { position_ms: 0 }, |s| {
                s.position.position_ms < 1500
            })?;
            self.command(PlayerCommand::Previous, |s| {
                s.track
                    .as_ref()
                    .is_some_and(|t| t.uri == source[previous_index])
            })?;
            let suffix = self.queue(false)?;
            ensure!(suffix.iter().map(|e| &e.uri).eq(source[previous_index + 1..].iter()),
                "Earlier complete-source entries were truncated/rotated or duplicate occurrence changed");
        }
        self.command(PlayerCommand::Next, |s| {
            s.track.as_ref().is_some_and(|t| t.uri == source[1])
        })?;
        let history = self.queue(true)?;
        ensure!(
            history.first().is_some_and(|e| e.uri == source[0]),
            "Native history not corroborated after complete-prefix traversal"
        );
        let last = source.len() - 1;
        self.load(
            PlayerCommand::PlayTracksAt {
                uris: source.to_vec(),
                start_index: last as u32,
                position_ms: 0,
                shuffle: false,
            },
            &source[last],
            None,
        )?;
        let before = self.observe()?.snapshot.options_observed_revision;
        self.command(
            PlayerCommand::Repeat {
                mode: RepeatMode::All,
            },
            |s| s.repeat == RepeatMode::All && s.options_observed_revision > before,
        )?;
        self.command(PlayerCommand::Next, |s| {
            s.track.as_ref().is_some_and(|t| t.uri == source[0])
        })?;
        self.command(
            PlayerCommand::Repeat {
                mode: RepeatMode::One,
            },
            |s| s.repeat == RepeatMode::One,
        )?;
        let current = self.observe()?;
        let duration = current
            .snapshot
            .track
            .as_ref()
            .and_then(|t| t.duration_ms)
            .filter(|d| *d > 5000 && *d <= u64::from(u32::MAX))
            .context("GateMissing: Repeat One fixture duration")?;
        let near_end = duration as u32 - 2000;
        let seek = self.command(
            PlayerCommand::Seek {
                position_ms: near_end,
            },
            |s| {
                s.observed_position
                    .is_some_and(|p| p.position_ms.abs_diff(u64::from(near_end)) < 1000)
            },
        )?;
        let after_seek = seek.snapshot.sequence;
        let deadline = Instant::now() + RPC;
        loop {
            let o = self.observe()?;
            Self::playing(&o)?;
            ensure!(
                o.snapshot
                    .track
                    .as_ref()
                    .is_some_and(|t| t.uri == source[0]),
                "Repeat One advanced to another item"
            );
            if o.snapshot.sequence > after_seek
                && o.snapshot.observed_position.is_some_and(|p| {
                    p.timestamp_ms
                        > seek
                            .snapshot
                            .observed_position
                            .map_or(0, |s| s.timestamp_ms)
                        && p.position_ms < 1500
                })
            {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "Repeat One option ACK had no actual natural end-to-start observation"
            );
            thread::sleep(Duration::from_millis(100));
        }
        self.command(
            PlayerCommand::Repeat {
                mode: RepeatMode::Off,
            },
            |s| s.repeat == RepeatMode::Off,
        )?;
        let before = self.observe()?.snapshot.options_observed_revision;
        self.command(PlayerCommand::Shuffle { enabled: true }, |s| {
            s.shuffle && s.options_observed_revision > before
        })?;
        let shuffled = self.queue(false)?;
        let mut observed: Vec<_> = shuffled.into_iter().map(|e| e.uri).collect();
        let mut expected = source[1..].to_vec();
        observed.sort();
        expected.sort();
        ensure!(
            observed == expected,
            "Shuffle lost/duplicated complete-source occurrences"
        );
        self.command(PlayerCommand::Shuffle { enabled: false }, |s| !s.shuffle)?;
        Ok(())
    }

    fn resources(&mut self) -> Result<()> {
        self.validate_baseline()?;
        let track = self.fixture.resource_track.clone();
        self.load(
            PlayerCommand::PlayContext {
                uri: track.clone(),
                track: None,
                position_ms: 0,
                shuffle: false,
            },
            &track,
            None,
        )?;
        self.command(
            PlayerCommand::Equalizer {
                settings: EqualizerSettings {
                    enabled: true,
                    gains_db: [3, 0, -2, 0, 0, 0, 0, 0, 0, 0],
                },
            },
            |s| s.equalizer.enabled && !s.equalizer.is_flat(),
        )?;
        // Terminal attachment is real and owner-driven. Never emulate it with a
        // Watch/spectrum consumer. Owner performs attach/close/reopen during run.
        let initial = self.observe()?;
        let initial_audio = Self::playing(&initial)?.clone();
        ensure!(initial.snapshot.track.as_ref().is_some_and(|t| t.duration_ms.is_some_and(|d| d >= 900_000)),
            "GateMissing: resource item must last >=900 seconds for actual terminal interaction and stable paired windows");
        self.attached_resources(&track, &initial_audio)?;
        self.terminal_receipt()?;
        // Exiting non-child TUIs cannot provide wait4 final CPU accounting to
        // this process. Do not pretend a disappearing PID consumed zero CPU.
        // Measure paired stable windows on the SAME uninterrupted decoder;
        // terminal transitions have a separate lifecycle receipt, not a resource
        // pass. Any process exit inside a measured window fails cpu_delta.
        let initial = self.observe()?;
        let after_terminal = Self::playing(&initial)?;
        ensure!(
            after_terminal.resets == initial_audio.resets
                && after_terminal.recovery_attempts == initial_audio.recovery_attempts
                && after_terminal.processed_frames > initial_audio.processed_frames,
            "Terminal lifecycle restarted or stalled actual decoder"
        );
        let started = Instant::now();
        let start_ms = now_ms();
        let mut last = after_terminal.clone();
        let mut previous_processes = self.sample(&initial, None)?;
        let mut previous_sample = Instant::now();
        let mut detached_seconds = 0.0;
        let mut max_cpu = 0.0_f64;
        let mut max_rss = 0_u64;
        let mut samples = Vec::new();
        while started.elapsed() < Duration::from_secs(300) {
            thread::sleep(Duration::from_secs(1));
            let o = self.observe()?;
            let a = Self::playing(&o)?;
            ensure!(
                o.snapshot.track.as_ref().is_some_and(|t| t.uri == track)
                    && o.snapshot.equalizer.enabled
                    && !o.snapshot.equalizer.is_flat(),
                "Requested item/nonflat EQ changed during uninterrupted run"
            );
            ensure!(a.processed_frames > last.processed_frames && a.resets == initial_audio.resets
                && a.recovery_attempts == initial_audio.recovery_attempts && a.dropped_frames >= last.dropped_frames,
                "Real PCM stopped, decoder reset, recovery or telemetry counter reset during uninterrupted run");
            let tui = self.active_tui()?;
            ensure!(
                tui.is_none(),
                "Owner must keep actual TUI closed during >=300s detached resource window"
            );
            let processes = self.sample(&o, tui.as_ref())?;
            let elapsed = previous_sample.elapsed().as_secs_f64();
            let cpu = cpu_delta(&previous_processes, &processes, elapsed)?;
            let rss: u64 = processes.values().map(|p| p.rss_bytes).sum();
            self.journal(&json!({"binding": self.binding, "kind": "detached_resource_sample", "timestamp_ms": now_ms(),
                "cpu_one_core_percent": cpu, "rss_bytes": rss, "audio": a, "processes": processes}))?;
            ensure!(
                cpu <= 100.0 && rss <= 512 * 1024 * 1024,
                "Owned aggregate exceeded one CPU core or 512MiB RSS: cpu={cpu}, rss_bytes={rss}"
            );
            max_cpu = max_cpu.max(cpu);
            max_rss = max_rss.max(rss);
            ensure!(o.telemetry.spectrum_subscribers == 0 && a.fft_frames == last.fft_frames,
                "Detached phase has spectrum consumer/FFT work; not zero-TUI demand-driven evidence");
            detached_seconds += elapsed;
            samples.push(json!({"timestamp_ms": now_ms(), "elapsed_seconds": started.elapsed().as_secs_f64(),
                "cpu_one_core_percent": cpu, "rss_bytes": rss, "audio": a,
                "processed_frames_per_second": (a.processed_frames - last.processed_frames) as f64 / elapsed,
                "spectrum_subscribers": o.telemetry.spectrum_subscribers, "processes": processes}));
            previous_processes = processes;
            previous_sample = Instant::now();
            last = a.clone();
        }
        self.write_json("resource-samples.json", &json!({"binding": self.binding, "started_ms": start_ms,
            "ended_ms": now_ms(), "wall_seconds": started.elapsed().as_secs_f64(), "samples": samples,
            "max_cpu_one_core_percent": max_cpu, "max_rss_bytes": max_rss,
            "attached_metrics": "attached-samples.json", "detached_seconds": detached_seconds,
            "transition_resources": "not qualified: non-child exit final CPU unavailable; stable windows only"}))?;
        ensure!(
            started.elapsed() >= Duration::from_secs(300) && detached_seconds > 0.0,
            "GateMissing: >=300s wall-clock detached sampling window"
        );
        self.signal_receipt(
            start_ms,
            initial_audio.processed_frames,
            last.processed_frames,
        )?;
        self.command(PlayerCommand::Pause, |s| {
            s.playback == PlaybackStatus::Paused
        })?;
        self.quiescent(
            "paused",
            self.fixture.baseline.paused.clone(),
            PlaybackStatus::Paused,
        )?;
        self.command(PlayerCommand::Stop, |s| {
            matches!(s.playback, PlaybackStatus::Stopped | PlaybackStatus::Paused)
        })?;
        // A paused Stop is not evidence of idle. Require actual Stopped state.
        self.quiescent(
            "idle",
            self.fixture.baseline.idle.clone(),
            PlaybackStatus::Stopped,
        )
    }

    fn attached_resources(&mut self, track: &str, original: &AudioTelemetry) -> Result<()> {
        let deadline = Instant::now() + OWNER_WAIT;
        let (mut previous, mut last_audio) = loop {
            let o = self.observe()?;
            let a = Self::playing(&o)?;
            ensure!(
                a.resets == original.resets
                    && a.recovery_attempts == original.recovery_attempts
                    && o.snapshot.track.as_ref().is_some_and(|t| t.uri == track),
                "Requested decoder changed before real TUI attachment"
            );
            let tui = self.active_tui()?;
            let p = self.sample(&o, tui.as_ref())?;
            if tui.is_some() {
                break (p, a.clone());
            }
            ensure!(
                Instant::now() < deadline,
                "GateMissing: real owner TUI attachment for measured attached window"
            );
            thread::sleep(Duration::from_millis(200));
        };
        let start = Instant::now();
        let mut sampled = Instant::now();
        let mut samples = Vec::new();
        while start.elapsed() < Duration::from_secs(60) {
            thread::sleep(Duration::from_secs(1));
            let o = self.observe()?;
            let a = Self::playing(&o)?;
            let tui = self
                .active_tui()?
                .context("Real TUI closed inside stable attached resource window")?;
            ensure!(
                a.processed_frames > last_audio.processed_frames
                    && a.resets == original.resets
                    && a.recovery_attempts == original.recovery_attempts
                    && a.dropped_frames >= last_audio.dropped_frames
                    && o.snapshot.track.as_ref().is_some_and(|t| t.uri == track)
                    && o.snapshot.equalizer.enabled
                    && !o.snapshot.equalizer.is_flat(),
                "Attached requested PCM/EQ continuity failed"
            );
            if o.telemetry.spectrum_subscribers > 0 {
                ensure!(
                    a.fft_frames > last_audio.fft_frames,
                    "Actual subscribed TUI FFT did not advance"
                );
            }
            let p = self.sample(&o, Some(&tui))?;
            let elapsed = sampled.elapsed().as_secs_f64();
            let cpu = cpu_delta(&previous, &p, elapsed)?;
            let rss: u64 = p.values().map(|p| p.rss_bytes).sum();
            self.journal(&json!({"binding": self.binding, "kind": "attached_resource_sample", "timestamp_ms": now_ms(),
                "cpu_one_core_percent": cpu, "rss_bytes": rss, "audio": a, "processes": p}))?;
            ensure!(cpu <= 100.0 && rss <= 512 * 1024 * 1024,
                "Attached daemon/SDK/audio/TUI aggregate exceeded one core or 512MiB: cpu={cpu}, rss_bytes={rss}");
            samples.push(json!({"timestamp_ms": now_ms(), "cpu_one_core_percent": cpu, "rss_bytes": rss,
                "processes": p, "audio": a, "spectrum_subscribers": o.telemetry.spectrum_subscribers,
                "processed_frames_per_second": (a.processed_frames - last_audio.processed_frames) as f64 / elapsed}));
            previous = p;
            sampled = Instant::now();
            last_audio = a.clone();
        }
        self.write_json("attached-samples.json", &json!({"binding": self.binding, "seconds": start.elapsed().as_secs_f64(), "samples": samples}))?;
        eprintln!("MAC6 attached stable window captured; owner must now close, reopen and close the real TUI and supply terminal.json");
        Ok(())
    }

    fn validate_baseline(&self) -> Result<()> {
        let b = &self.fixture.baseline;
        let p = Path::new(&b.evidence);
        ensure!(
            p.starts_with(&self.home)
                && fs::canonicalize(p)? == p
                && b.source_sha256 == self.fixture.source_sha256
                && b.measured_seconds >= 60
                && valid_hash(&b.sha256),
            "GateMissing: measured source-bound >=60s baseline"
        );
        // Public evidence only, never credentials. Exact same budgets must be in
        // the measured evidence, not merely an unverified hash attached to JSON.
        let evidence: BaselineEvidence = public_json(p)?;
        ensure!(
            hash_file(p)? == b.sha256
                && evidence.source_sha256 == b.source_sha256
                && evidence.measured_seconds == b.measured_seconds
                && !evidence.operator.trim().is_empty()
                && evidence.measured_ms > 0
                && evidence.measured_ms < now_ms(),
            "Baseline evidence binding mismatch"
        );
        ensure!(
            serde_json::to_value(&evidence.paused)? == serde_json::to_value(&b.paused)?
                && serde_json::to_value(&evidence.idle)? == serde_json::to_value(&b.idle)?,
            "Baseline budgets differ from measured evidence"
        );
        for budget in [&b.paused, &b.idle] {
            ensure!(
                budget.cpu_one_core_percent.is_finite()
                    && (0.0..=100.0).contains(&budget.cpu_one_core_percent)
                    && budget.rss_bytes > 0
                    && budget.rss_bytes <= 512 * 1024 * 1024,
                "Measured baseline resource budgets missing or outside release caps"
            );
        }
        Ok(())
    }

    fn quiescent(&mut self, name: &str, budget: Budgets, state: PlaybackStatus) -> Result<()> {
        let initial = self.observe()?;
        let a = initial
            .telemetry
            .audio
            .as_ref()
            .context("GateMissing: audio counters")?
            .clone();
        let start = Instant::now();
        let mut previous = self.sample(&initial, None)?;
        let mut sampled = Instant::now();
        let mut samples = Vec::new();
        while start.elapsed() < Duration::from_secs(60) {
            thread::sleep(Duration::from_secs(1));
            let o = self.observe()?;
            ensure!(
                o.snapshot.playback == state
                    && o.snapshot.web_api_ready
                    && o.snapshot.owned_device_verified
                    && o.snapshot.readiness == ServiceState::Ready
                    && o.snapshot.soloist_ready
                    && o.telemetry.spectrum_subscribers == 0
                    && self.active_tui()?.is_none(),
                "Paused/idle phase is not stable, authenticated, zero TUI"
            );
            let current = o
                .telemetry
                .audio
                .as_ref()
                .context("GateMissing: audio counters")?;
            counters_within(&a, current, &budget)?;
            let p = self.sample(&o, None)?;
            let cpu = cpu_delta(&previous, &p, sampled.elapsed().as_secs_f64())?;
            let rss: u64 = p.values().map(|p| p.rss_bytes).sum();
            self.journal(
                &json!({"binding": self.binding, "kind": name, "timestamp_ms": now_ms(),
                "cpu_one_core_percent": cpu, "rss_bytes": rss, "audio": current, "processes": p}),
            )?;
            ensure!(
                cpu <= budget.cpu_one_core_percent && rss <= budget.rss_bytes,
                "Paused/idle measured resource baseline exceeded: cpu={cpu}, rss_bytes={rss}"
            );
            samples.push(json!({"timestamp_ms": now_ms(), "audio": current, "cpu_one_core_percent": cpu, "rss_bytes": rss, "processes": p}));
            previous = p;
            sampled = Instant::now();
        }
        self.write_json(&format!("{name}-samples.json"), &json!({"binding": self.binding, "seconds": start.elapsed().as_secs_f64(), "samples": samples}))
    }

    fn media(&mut self) -> Result<()> {
        let track = self.fixture.resource_track.clone();
        self.load(
            PlayerCommand::PlayContext {
                uri: track.clone(),
                track: None,
                position_ms: 0,
                shuffle: false,
            },
            &track,
            None,
        )?;
        // Owner must attach/close/reopen a real terminal, and finally close it.
        self.terminal_receipt()?;
        let before = self.observe()?;
        let frames = Self::playing(&before)?.processed_frames;
        self.await_media("pause", "media-pause.json", PlaybackStatus::Paused)?;
        self.await_media("play", "media-play.json", PlaybackStatus::Playing)?;
        let after = self.observe()?;
        let a = Self::playing(&after)?;
        ensure!(
            a.processed_frames > frames && after.telemetry.spectrum_subscribers == 0,
            "Media commands failed actual processed output with no TUI"
        );
        // JSON here is the real safe core snapshot, not /me/player as proxy.
        self.write_json("media-observations.json", &json!({"binding": self.binding,
            "media_event_count": after.telemetry.media_event_count, "last_media_command": after.telemetry.last_media_command,
            "processed_frames": a.processed_frames, "decoder_resets": a.resets,
            "service_gui_acceptance": "owner-required separate C7 gate"}))
    }

    fn check_binding(&self, b: &Binding) -> Result<()> {
        ensure!(
            Some(b) == self.binding.as_ref(),
            "Owner receipt artifact/source/session/PID/challenge mismatch"
        );
        Ok(())
    }

    fn receipt<T: serde::de::DeserializeOwned>(&mut self, name: &str) -> Result<T> {
        let deadline = Instant::now() + OWNER_WAIT;
        let path = self.evidence.path().join(name);
        while !path.exists() {
            self.observe()?; // Fail on owner/auth/control failure during wait.
            ensure!(
                Instant::now() < deadline,
                "GateMissing: owner receipt {name} unavailable"
            );
            thread::sleep(Duration::from_millis(200));
        }
        public_json(&path)
    }

    fn active_tui(&self) -> Result<Option<Process>> {
        // Live attachment registration is an owner public JSON receipt:
        // {binding, pid, start, attached_ms}. Remove it after real TUI exits.
        let path = self.evidence.path().join("active-terminal.json");
        if !path.exists() {
            return Ok(None);
        }
        let r: ActiveTerminal = public_json(&path)?;
        self.check_binding(&r.binding)?;
        ensure!(
            r.attached_ms >= r.binding.challenge_ms && r.attached_ms <= now_ms(),
            "Terminal timestamp mismatch"
        );
        let all = self.ps()?;
        let p = all
            .get(&r.pid)
            .context("Registered TUI no longer exists; remove active-terminal.json")?;
        ensure!(
            p.start == r.start && p.uid == unsafe { libc::geteuid() } && p.pid != self.child.id(),
            "TUI start/user identity mismatch"
        );
        ensure!(
            executable(p.pid)? == self.artifact,
            "Real TUI must execute exact copied release artifact"
        );
        ensure!(
            hash_file(&self.artifact)? == self.fixture.artifact_sha256,
            "Copied artifact changed before actual TUI observation"
        );
        Ok(Some(p.clone()))
    }

    fn terminal_receipt(&mut self) -> Result<()> {
        // Capture live identities on BOTH sides of a real close/reopen, not just
        // an operator's assertion after the processes have already disappeared.
        let deadline = Instant::now() + OWNER_WAIT;
        let initial = self.observe()?;
        let resets = initial
            .telemetry
            .audio
            .as_ref()
            .context("GateMissing: audio telemetry")?
            .resets;
        let track = initial.snapshot.track.as_ref().map(|t| t.uri.clone());
        let r: TerminalReceipt = loop {
            let o = self.observe()?;
            Self::playing(&o)?;
            ensure!(
                o.telemetry
                    .audio
                    .as_ref()
                    .context("Audio telemetry missing")?
                    .resets
                    == resets
                    && o.snapshot.track.as_ref().map(|t| t.uri.clone()) == track,
                "Terminal lifecycle restarted decoder/changed requested item"
            );
            match self.active_tui()? {
                Some(p) => {
                    self.sample(&o, Some(&p))?;
                }
                None => {
                    self.sample(&o, None)?;
                }
            }
            let path = self.evidence.path().join("terminal.json");
            if path.exists() {
                break public_json(&path)?;
            }
            ensure!(
                Instant::now() < deadline,
                "GateMissing: real terminal attach/detach/reattach owner receipt"
            );
            thread::sleep(Duration::from_millis(200));
        };
        self.check_binding(&r.binding)?;
        ensure!(
            !r.operator.trim().is_empty()
                && r.real_terminal
                && r.navigation_input_art_clipboard_observed,
            "GateMissing: actual terminal owner interaction evidence"
        );
        let first = self
            .terminal_seen
            .get(&r.attached_pid)
            .context("GateMissing: first real TUI not observed live")?;
        let second = self
            .terminal_seen
            .get(&r.reattached_pid)
            .context("GateMissing: reopened real TUI not observed live")?;
        ensure!(
            r.attached_pid != r.reattached_pid
                && first.0 == r.attached_start
                && second.0 == r.reattached_start
                && first.1 <= r.detached_ms
                && r.detached_ms <= r.reattached_ms
                && r.reattached_ms <= second.1
                && r.reattached_ms <= now_ms()
                && self
                    .terminal_absent_ms
                    .iter()
                    .any(|t| *t >= first.1 && *t <= second.1),
            "Terminal PID/start/timestamp transitions not corroborated"
        );
        let all = self.ps()?;
        ensure!(
            !all.contains_key(&r.attached_pid)
                && !all.contains_key(&r.reattached_pid)
                && self.active_tui()?.is_none(),
            "GateMissing: real terminal processes must be closed before no-TUI media/idle checks"
        );
        Ok(())
    }

    fn await_media(
        &mut self,
        command: &str,
        filename: &str,
        expected: PlaybackStatus,
    ) -> Result<()> {
        let before = self.observe()?;
        ensure!(
            self.active_tui()?.is_none() && before.telemetry.spectrum_subscribers == 0,
            "Media gate requires no TUI"
        );
        let began = now_ms();
        let deadline = Instant::now() + OWNER_WAIT;
        let mut observed = None;
        loop {
            let o = self.observe()?;
            ensure!(
                self.active_tui()?.is_none() && o.telemetry.spectrum_subscribers == 0,
                "TUI appeared during hardware media gate"
            );
            self.sample(&o, None)?;
            if o.telemetry.media_event_count > before.telemetry.media_event_count
                && o.telemetry.last_media_command.as_deref() == Some(command)
                && o.snapshot.playback == expected
                && before.snapshot.playback != expected
            {
                observed = Some((o.telemetry.media_event_count, now_ms()));
            }
            if self.evidence.path().join(filename).exists() {
                let receipt: MediaReceipt = public_json(&self.evidence.path().join(filename))?;
                self.check_binding(&receipt.binding)?;
                let (count, at) = observed
                    .context("Hardware receipt has no actual native event/state transition")?;
                ensure!(
                    !receipt.operator.trim().is_empty()
                        && receipt.physical_media_key
                        && receipt.command == command
                        && receipt.before_count == before.telemetry.media_event_count
                        && receipt.after_count == count
                        && receipt.event_ms >= began
                        && receipt.event_ms <= at,
                    "Hardware media receipt counter/state/time mismatch"
                );
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "GateMissing: owner physical media key receipt/state transition"
            );
            thread::sleep(Duration::from_millis(200));
        }
    }

    fn signal_receipt(&mut self, began: u64, first: u64, last: u64) -> Result<()> {
        let r: SignalReceipt = self.receipt("c5.json")?;
        self.check_binding(&r.binding)?;
        ensure!(!r.operator.trim().is_empty() && r.method == "c5_controlled_dsp_signal_correlation"
            && r.started_ms >= began && r.ended_ms > r.started_ms && r.ended_ms <= now_ms()
            && valid_hash(&r.input_signal_sha256) && valid_hash(&r.output_signal_sha256)
            && r.input_signal_sha256 != r.output_signal_sha256 && r.correlated_markers >= 2
            && r.measured_processed_start >= first && r.measured_processed_end > r.measured_processed_start
            && r.measured_processed_end <= last && r.transport_latency_micros <= 50_000,
            "GateMissing: actual source/session-bound C5 <=50ms signal correlation receipt; buffer estimate is not proof");
        ensure!(
            r.markers.len() as u64 == r.correlated_markers
                && r.markers
                    .windows(2)
                    .all(|w| w[1].input_micros > w[0].input_micros
                        && w[1].output_micros > w[0].output_micros
                        && w[1].processed_frame > w[0].processed_frame),
            "C5 actual correlation marker sequence missing"
        );
        let mut max_latency = 0;
        for marker in r.markers {
            ensure!(
                marker.output_micros >= marker.input_micros
                    && marker.processed_frame >= r.measured_processed_start
                    && marker.processed_frame <= r.measured_processed_end,
                "C5 marker frame/time binding mismatch"
            );
            max_latency = max_latency.max(marker.output_micros - marker.input_micros);
        }
        ensure!(
            max_latency == r.transport_latency_micros,
            "C5 receipt latency does not match actual correlation markers"
        );
        Ok(())
    }

    fn ps(&self) -> Result<BTreeMap<u32, Process>> {
        // ONLY safe fields. In particular never command/args: SDK argv may hold
        // an API key. File-backed bounded output avoids stdout pipe deadlocks.
        let mut output = tempfile::tempfile_in(self.evidence.path())?;
        let mut child = Command::new("/bin/ps")
            .args([
                "-axo",
                "pid=,ppid=,pgid=,uid=,%cpu=,rss=,time=,lstart=,ucomm=",
            ])
            .env_clear()
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::from(output.try_clone()?))
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "Safe process sampler failed");
                break;
            }
            if Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                bail!("Safe process sampler timed out");
            }
            thread::sleep(Duration::from_millis(20));
        }
        use std::io::{Seek, SeekFrom};
        output.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        output.take(MAX_PUBLIC_JSON + 1).read_to_string(&mut text)?;
        ensure!(
            text.len() as u64 <= MAX_PUBLIC_JSON,
            "Safe ps output bound exceeded"
        );
        parse_ps(&text)
    }

    fn sample(&mut self, o: &Observation, tui: Option<&Process>) -> Result<BTreeMap<u32, Process>> {
        let all = self.ps()?;
        let daemon = all.get(&self.child.id()).context("Owned daemon missing")?;
        ensure!(
            daemon.start == self.child_start
                && daemon.pgid == daemon.pid
                && daemon.uid == unsafe { libc::geteuid() },
            "Daemon PID reused or process identity changed"
        );
        let mut owned = BTreeSet::from([daemon.pid]);
        loop {
            let n = owned.len();
            for p in all.values() {
                if owned.contains(&p.ppid) {
                    owned.insert(p.pid);
                }
            }
            if owned.len() == n {
                break;
            }
        }
        for p in all.values().filter(|p| p.pgid == daemon.pgid) {
            ensure!(
                p.uid == daemon.uid && owned.contains(&p.pid),
                "Unrelated member in foreground daemon process group"
            );
        }
        let artifact_name = self
            .artifact
            .file_name()
            .and_then(|n| n.to_str())
            .context("UTF8 artifact name required")?;
        for p in all
            .values()
            .filter(|p| p.uid == daemon.uid && !owned.contains(&p.pid))
        {
            // ucomm can be truncated on macOS. proc_pidpath is executable-only,
            // never argv. Reject another exact-artifact client without a live
            // owner registration instead of treating absent JSON as no TUI.
            if (artifact_name.starts_with(&p.ucomm) || p.ucomm == artifact_name)
                && executable(p.pid)? == self.artifact
            {
                ensure!(tui.is_some_and(|t| t.pid == p.pid && t.start == p.start),
                    "Unregistered exact-artifact client exists; cannot claim zero-TUI or omit its resources");
            }
        }
        // Primary reports exact SDK/audio host roots; verify descent and pgid,
        // then include EVERY member, even if later reparented. Never trust a
        // group ID supplied by snapshot without actual process ownership.
        ensure!(
            !o.telemetry.owned_process_groups.is_empty(),
            "GateMissing: owned SDK/audio process groups"
        );
        for group in &o.telemetry.owned_process_groups {
            let p = all.get(&group.pid).context("Owned helper root absent")?;
            let previously = self.seen.get(&p.pid).is_some_and(|s| s.start == p.start);
            ensure!(
                (owned.contains(&p.pid) || previously)
                    && p.uid == daemon.uid
                    && p.pgid == group.pgid
                    && group.pid == group.pgid
                    && group.pid != daemon.pid,
                "Unverified SDK/audio helper process group"
            );
            self.owned_groups.insert(group.pgid);
            for member in all.values().filter(|m| m.pgid == group.pgid) {
                ensure!(
                    member.uid == daemon.uid
                        && (owned.contains(&member.pid)
                            || self
                                .seen
                                .get(&member.pid)
                                .is_some_and(|p| p.start == member.start)),
                    "Unrelated member in SDK/audio process group"
                );
                owned.insert(member.pid);
            }
        }
        // Real TUI is separately launched by the owner; account its tree too.
        if let Some(tui) = tui {
            ensure!(
                all.get(&tui.pid).is_some_and(|p| p.start == tui.start),
                "TUI identity changed between samples"
            );
            owned.insert(tui.pid);
            self.terminal_seen
                .entry(tui.pid)
                .or_insert((tui.start.clone(), now_ms()));
            loop {
                let n = owned.len();
                for p in all.values() {
                    if owned.contains(&p.ppid) {
                        owned.insert(p.pid);
                    }
                }
                if n == owned.len() {
                    break;
                }
            }
        }
        if tui.is_none() && !self.terminal_seen.is_empty() {
            self.terminal_absent_ms.push(now_ms());
        }
        for (pid, old) in &self.seen {
            if let Some(p) = all.get(pid) {
                ensure!(
                    p.start == old.start && p.uid == old.uid,
                    "Owned PID reused; measurement/cleanup refused"
                );
                owned.insert(*pid);
            }
        }
        let mut result = BTreeMap::new();
        for pid in owned {
            let p = all
                .get(&pid)
                .context("Owned process disappeared during sampling")?;
            ensure!(p.uid == daemon.uid, "Owned process changed user");
            self.seen.insert(pid, p.clone());
            result.insert(pid, p.clone());
        }
        Ok(result)
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        // Snapshot verified identity before Shutdown; never send stop to an
        // unknown endpoint. Child::kill fallback targets only our unreaped child.
        let shutdown = if self.session.is_some() && self.child.try_wait()?.is_none() {
            self.observe()
                .and_then(|o| self.sample(&o, None))
                .and_then(|_| self.rpc(Operation::Shutdown))
                .map(|_| ())
        } else {
            Ok(())
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.child.try_wait()?.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        let forced = self.child.try_wait()?.is_none();
        if forced {
            self.child.kill()?;
            self.child.wait()?;
        }
        // Never kill helper/group PIDs from a stale sample. Cleanup failures are
        // failures with owner reconciliation, not broad killpg or service stop.
        let all = self.ps()?;
        let remaining: BTreeSet<_> = all
            .values()
            .filter(|p| {
                self.owned_groups.contains(&p.pgid)
                    || self
                        .seen
                        .get(&p.pid)
                        .is_some_and(|old| old.start == p.start)
            })
            .map(|p| p.pid)
            .collect();
        let socket_absent = matches!(fs::symlink_metadata(&self.socket), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        let artifact_unchanged =
            hash_file(&self.artifact).is_ok_and(|hash| hash == self.fixture.artifact_sha256);
        let ipc_cleanup = if remaining.is_empty() && socket_absent {
            self.clean_private_ipc()
        } else {
            Err(anyhow::anyhow!(
                "Owned process/socket still present; refusing IPC cleanup"
            ))
        };
        self.write_json("cleanup.json", &json!({"binding": self.binding, "forced_exact_child_kill": forced,
            "remaining_owned_pids": remaining, "socket_absent": socket_absent, "shutdown_rpc_ok": shutdown.is_ok(),
            "private_ipc_cleaned": ipc_cleanup.is_ok(), "artifact_unchanged": artifact_unchanged}))?;
        self.finished = true;
        ensure!(
            !forced
                && shutdown.is_ok()
                && remaining.is_empty()
                && socket_absent
                && ipc_cleanup.is_ok()
                && artifact_unchanged,
            "Owned daemon/helper/IPC cleanup failed; owner must reconcile fixture only"
        );
        Ok(())
    }

    fn clean_private_ipc(&self) -> Result<()> {
        use fs2::FileExt;
        let runtime = self.socket.parent().context("IPC directory")?;
        let m = fs::symlink_metadata(runtime)?;
        ensure!(
            self.ipc_identities.get(runtime) == Some(&(m.dev(), m.ino()))
                && m.is_dir()
                && !m.file_type().is_symlink()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0,
            "Owned IPC directory identity changed; cleanup refused"
        );
        for name in ["owner.lock", "startup.lock"] {
            let path = runtime.join(name);
            if let Some(expected) = self.ipc_identities.get(&path) {
                let file = private_file(&path)?;
                let m = file.metadata()?;
                ensure!(
                    *expected == (m.dev(), m.ino()),
                    "Owned IPC lock identity changed"
                );
                file.try_lock_exclusive()
                    .context("Owner lock still held; cleanup refused")?;
                let current = fs::symlink_metadata(&path)?;
                ensure!(
                    (current.dev(), current.ino()) == *expected,
                    "Owned IPC lock replaced before removal"
                );
                fs::remove_file(path)?;
            }
        }
        // remove_dir refuses unrecognized new entries; never recursive deletion.
        fs::remove_dir(runtime)?;
        ensure!(
            matches!(fs::symlink_metadata(runtime), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "Owned private IPC directory cleanup not observed"
        );
        Ok(())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if !self.finished {
            // Panic/error fallback never scans or signals ordinary user daemons.
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        // Evidence retention was configured at construction. Never recursively
        // delete or change owner-provisioned files from this fallback path.
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveTerminal {
    binding: Binding,
    pid: u32,
    start: String,
    attached_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineEvidence {
    source_sha256: String,
    operator: String,
    measured_ms: u64,
    measured_seconds: u64,
    paused: Budgets,
    idle: Budgets,
}

fn counters_within(a: &AudioTelemetry, b: &AudioTelemetry, max: &Budgets) -> Result<()> {
    for (first, last, limit) in [
        (a.processed_frames, b.processed_frames, max.processed_frames),
        (a.fft_frames, b.fft_frames, max.fft_frames),
        (a.worker_wakes, b.worker_wakes, max.worker_wakes),
        (a.resets, b.resets, max.resets),
        (a.dropped_frames, b.dropped_frames, max.dropped_frames),
        (
            a.recovery_attempts,
            b.recovery_attempts,
            max.recovery_attempts,
        ),
    ] {
        ensure!(last >= first && last - first <= limit,
        "Paused/idle counter reset or measured baseline budget exceeded: first={first}, last={last}, allowed_delta={limit}");
    }
    Ok(())
}

fn context_row_uri(row: &Value) -> Option<&str> {
    let item = row.get("item").or_else(|| row.get("track")).unwrap_or(row);
    let uri = item["uri"].as_str()?;
    (row["is_local"].as_bool() != Some(true)
        && item["is_local"].as_bool() != Some(true)
        && row["is_playable"].as_bool() != Some(false)
        && item["is_playable"].as_bool() != Some(false)
        && !row["restrictions"]
            .as_object()
            .is_some_and(|r| !r.is_empty())
        && !item["restrictions"]
            .as_object()
            .is_some_and(|r| !r.is_empty())
        && myx::player::intents::valid_uri(uri, &["track", "episode"]))
    .then_some(uri)
}

fn parse_ps(text: &str) -> Result<BTreeMap<u32, Process>> {
    let mut result = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(fields.len() >= 13, "Malformed safe ps record");
        let p = Process {
            pid: fields[0].parse()?,
            ppid: fields[1].parse()?,
            pgid: fields[2].parse()?,
            uid: fields[3].parse()?,
            cpu_percent: fields[4].parse()?,
            rss_bytes: fields[5]
                .parse::<u64>()?
                .checked_mul(1024)
                .context("RSS overflow")?,
            cpu_seconds: cpu_time(fields[6])?,
            start: fields[7..12].join(" "),
            ucomm: fields[12..].join(" "),
        };
        ensure!(
            p.cpu_percent.is_finite()
                && p.cpu_percent >= 0.0
                && !p.start.is_empty()
                && result.insert(p.pid, p).is_none(),
            "Invalid or duplicate safe ps identity"
        );
    }
    Ok(result)
}

fn cpu_time(text: &str) -> Result<f64> {
    let (days, clock) = match text.split_once('-') {
        Some((d, c)) => (d.parse::<f64>()?, c),
        None => (0.0, text),
    };
    let parts: Vec<f64> = clock
        .split(':')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(
        (2..=3).contains(&parts.len())
            && days.is_finite()
            && days >= 0.0
            && parts.iter().all(|p| p.is_finite() && *p >= 0.0),
        "Invalid CPU cumulative time"
    );
    let mut seconds = days * 86400.0;
    for (i, p) in parts.iter().rev().enumerate() {
        seconds += p * 60_f64.powi(i as i32);
    }
    Ok(seconds)
}

fn cpu_delta(
    before: &BTreeMap<u32, Process>,
    after: &BTreeMap<u32, Process>,
    seconds: f64,
) -> Result<f64> {
    ensure!(
        seconds.is_finite() && seconds > 0.0,
        "Invalid process sampling wall interval"
    );
    let mut used = 0.0;
    for (pid, old) in before {
        // Unknown exited-process CPU must not disappear from the aggregate.
        let new = after.get(pid).context(
            "Process exited between resource samples; final CPU unaccounted, gate must be rerun",
        )?;
        ensure!(
            old.start == new.start && old.uid == new.uid && new.cpu_seconds >= old.cpu_seconds,
            "PID reuse/CPU reset"
        );
        used += new.cpu_seconds - old.cpu_seconds;
    }
    for (pid, new) in after {
        if !before.contains_key(pid) {
            used += new.cpu_seconds;
        }
    }
    Ok(used / seconds * 100.0)
}

fn executable(pid: u32) -> Result<PathBuf> {
    // proc_pidpath returns executable path, never argv or environment.
    unsafe extern "C" {
        fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, size: u32) -> libc::c_int;
    }
    let mut bytes = vec![0_u8; 4096];
    let n = unsafe {
        proc_pidpath(
            pid as libc::c_int,
            bytes.as_mut_ptr().cast(),
            bytes.len() as u32,
        )
    };
    ensure!(n > 0, "Cannot verify real TUI executable identity");
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .context("Unterminated executable path")?;
    use std::os::unix::ffi::OsStrExt;
    Ok(fs::canonicalize(Path::new(std::ffi::OsStr::from_bytes(
        &bytes[..end],
    )))?)
}
