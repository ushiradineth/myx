use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use serde_json::Value;

use crate::domain::{ServiceState, Snapshot};
use crate::ipc::protocol::{self, ErrorCode, Operation, Payload, Request, Response, VERSION};
use crate::ipc::unix::{open_private_lock, verify_peer, RuntimePaths};
use crate::webapi::HttpMethod;

const START_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const WATCH_TIMEOUT: Duration = Duration::from_secs(2);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(25);

/// An authenticated local endpoint, never an SDK session or HTTP credential.
#[derive(Clone)]
pub struct Connection {
    paths: RuntimePaths,
    config: PathBuf,
}

impl Connection {
    pub(super) fn selected(config: &Path) -> Result<Self> {
        ensure!(
            config.is_absolute(),
            "Select an absolute configuration path before attaching"
        );
        ensure!(
            config.to_str().is_some(),
            "Configuration path must be valid UTF-8"
        );
        Ok(Self {
            paths: RuntimePaths::for_user()?,
            config: config.to_owned(),
        })
    }

    pub fn ensure(config: &Path) -> Result<Self> {
        let connection = Self::selected(config)?;
        connection.paths.prepare()?;
        let deadline = Instant::now() + START_TIMEOUT;
        if connection.probe(deadline)?.is_some() {
            return Ok(connection);
        }
        let lock = open_private_lock(&connection.paths.startup_lock)?;
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    ensure!(
                        Instant::now() < deadline,
                        "Daemon startup is busy; retry daemon status"
                    );
                    thread::sleep(remaining(deadline)?.min(Duration::from_millis(40)));
                }
                Err(_) => bail!("Cannot acquire the private daemon startup lock"),
            }
        }
        // Keep this actual fs2 lock until a compatible owner answers, not merely
        // until spawn returns. The server separately holds its lifetime lock.
        if connection.probe(deadline)?.is_some() {
            return Ok(connection);
        }
        ensure!(Instant::now() < deadline, "Daemon startup timed out");
        // The service adapter has its own bounded subprocesses, but several
        // lookups may add up. Bound this caller's entire startup/lock budget.
        let (service_tx, service_rx) = std::sync::mpsc::sync_channel(1);
        thread::Builder::new()
            .name("myx-service-start".into())
            .spawn(move || {
                let _ = service_tx.send(crate::service::start());
            })
            .map_err(|_| anyhow::anyhow!("Cannot start service registration lookup"))?;
        let registered = service_rx
            .recv_timeout(remaining(deadline)?)
            .map_err(|_| anyhow::anyhow!("Service startup timed out; inspect daemon status"))??;
        let mut child = if registered {
            None
        } else {
            ensure!(Instant::now() < deadline, "Daemon startup timed out");
            Some(DetachedChild::spawn(&connection.config)?)
        };
        loop {
            ensure!(
                Instant::now() < deadline,
                "Daemon startup timed out; inspect daemon status and setup"
            );
            if connection.probe(deadline)?.is_some() {
                return Ok(connection);
            }
            if let Some(child) = child.as_mut() {
                if child
                    .0
                    .as_mut()
                    .context("Missing daemon child")?
                    .try_wait()
                    .map_err(|_| anyhow::anyhow!("Cannot inspect daemon startup"))?
                    .is_some()
                {
                    child.0.take();
                    bail!(
                        "Daemon exited during startup; check client_id, Soloist key and SDK setup"
                    );
                }
            }
            thread::sleep(remaining(deadline)?.min(Duration::from_millis(60)));
        }
    }

    fn probe(&self, deadline: Instant) -> Result<Option<Snapshot>> {
        match self.rpc(
            Operation::Status,
            true,
            deadline.min(Instant::now() + WATCH_TIMEOUT),
        ) {
            Ok(Payload::Snapshot(snapshot)) => Ok(Some(*snapshot)),
            Ok(_) => bail!("Invalid daemon status response"),
            Err(error) if absent(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        snapshot_payload(self.request(Operation::Status, CONTROL_TIMEOUT)?)
    }

    pub(super) fn refresh_subscription(&self) -> Result<Snapshot> {
        snapshot_payload(self.request(Operation::Status, WATCH_TIMEOUT)?)
    }

    /// Management can inspect an owner using another config without attaching to it.
    pub fn status_any(&self) -> Result<Option<Snapshot>> {
        self.status_any_until(Instant::now() + CONTROL_TIMEOUT)
    }

    pub(super) fn status_any_until(&self, deadline: Instant) -> Result<Option<Snapshot>> {
        match self.rpc(
            Operation::Status,
            false,
            deadline.min(Instant::now() + CONTROL_TIMEOUT),
        ) {
            Ok(payload) => snapshot_payload(payload).map(Some),
            Err(error) if absent(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn owner_released(&self) -> Result<bool> {
        match fs::symlink_metadata(&self.paths.daemon_lock) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(_) => bail!("Cannot inspect daemon owner lock"),
            Ok(_) => {}
        }
        let lock = open_private_lock(&self.paths.daemon_lock)?;
        match lock.try_lock_exclusive() {
            Ok(()) => Ok(true), // Closing this probe immediately releases it.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(_) => bail!("Cannot inspect daemon owner lock"),
        }
    }

    pub fn prepare(&self, interactive: bool) -> Result<Snapshot> {
        // Cached refresh, owned runtime readiness and device-list propagation
        // have separate bounded deadlines; they can exceed the IPC start time.
        let deadline = Instant::now() + Duration::from_secs(if interactive { 180 } else { 90 });
        let mut authorized = false;
        loop {
            let snapshot = snapshot_payload(self.rpc(
                Operation::Status,
                true,
                deadline.min(Instant::now() + CONTROL_TIMEOUT),
            )?)?;
            if snapshot.soloist_ready
                && snapshot.owned_device_verified
                && matches!(
                    snapshot.readiness,
                    ServiceState::Ready | ServiceState::Degraded
                )
            {
                return Ok(snapshot);
            }
            ensure!(snapshot.readiness != ServiceState::Error,
                "Daemon setup failed; check client_id, Soloist key and SDK setup, then explicitly restart");
            if !snapshot.web_api_ready
                && !authorized
                && snapshot.readiness == ServiceState::SetupRequired
            {
                ensure!(interactive,
                    "Web API authorization requires setup; run myx interactively before non-interactive use");
                let payload = self.rpc(
                    Operation::Authorize,
                    true,
                    deadline.min(Instant::now() + CONTROL_TIMEOUT),
                )?;
                match payload {
                    Payload::Authorization { url } => {
                        // The owner supplies the flow, but only this explicitly
                        // interactive client may display or open its URL.
                        ensure!(
                            url.starts_with("https://accounts.spotify.com/authorize?")
                                && !url.chars().any(char::is_control),
                            "Invalid authorization URL from daemon"
                        );
                        eprintln!("Authorize Spotify in your browser:\n{url}");
                        // Nonblocking opener avoids inheriting a terminal or
                        // waiting for a browser process to finish.
                        let _ = open::that_detached(&url);
                        authorized = true;
                    }
                    _ => bail!("Invalid daemon authorization response"),
                }
            }
            if snapshot.readiness == ServiceState::SetupRequired
                && snapshot.web_api_ready
                && !snapshot.soloist_ready
            {
                bail!("Soloist setup required; configure the API key and SDK, then explicitly restart the daemon");
            }
            ensure!(Instant::now() < deadline,
                "Daemon readiness timed out; check Web API authorization, client_id, Soloist key and SDK setup");
            thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn catalog(&self, method: HttpMethod, path: &str, body: Option<Value>) -> Result<Value> {
        ensure!(
            path.starts_with("/v1/")
                && !path.contains("://")
                && !path.contains('#')
                && !path.chars().any(char::is_control),
            "Catalog requires a Spotify /v1/ API path"
        );
        match self.request(
            Operation::Catalog {
                method,
                path: path.into(),
                body,
            },
            CATALOG_TIMEOUT,
        )? {
            Payload::Json(value) => Ok(value),
            _ => bail!("Invalid daemon catalog response"),
        }
    }

    pub(super) fn watch(&self, after: u64, client_id: &str, spectrum: bool) -> Result<Snapshot> {
        snapshot_payload(self.request(
            Operation::Watch {
                after,
                client_id: client_id.into(),
                spectrum,
            },
            WATCH_TIMEOUT,
        )?)
    }

    pub(super) fn request(&self, operation: Operation, timeout: Duration) -> Result<Payload> {
        self.rpc(operation, true, Instant::now() + timeout)
    }

    fn rpc(&self, operation: Operation, filter_config: bool, deadline: Instant) -> Result<Payload> {
        let stream = connect(&self.paths, deadline)?;
        verify_peer(&stream).map_err(|_| anyhow::anyhow!("Daemon peer authentication failed"))?;
        let request = Request {
            protocol_version: VERSION,
            request_id: rand::random(),
            config_path: filter_config.then(|| self.config.to_str().unwrap_or_default().to_owned()),
            operation,
        };
        let mut stream = DeadlineStream { stream, deadline };
        protocol::send(&mut stream, &request)
            .map_err(|_| anyhow::anyhow!("Daemon request could not be sent within its deadline"))?;
        let response: Response = protocol::receive(&mut stream).map_err(|_| {
            anyhow::anyhow!("Daemon response is malformed, disconnected or timed out")
        })?;
        validate_response(response, request.request_id, request.config_path.as_deref())
    }
}

fn snapshot_payload(payload: Payload) -> Result<Snapshot> {
    match payload {
        Payload::Snapshot(snapshot) => Ok(*snapshot),
        _ => bail!("Invalid daemon snapshot response"),
    }
}

fn validate_response(response: Response, request_id: u64, config: Option<&str>) -> Result<Payload> {
    ensure!(
        response.protocol_version == VERSION,
        "Incompatible daemon protocol; explicitly stop the existing daemon first"
    );
    ensure!(
        response.request_id == request_id,
        "Daemon response does not match this request"
    );
    // Never echo a peer-provided diagnostic: even a same-user old binary may
    // return configuration contents or provider response bodies.
    let payload = response.result.map_err(|error| {
        let message = match error.code {
            ErrorCode::ConfigMismatch => {
                "Daemon uses a different configuration; explicitly restart with --config PATH"
            }
            ErrorCode::VersionMismatch => {
                "Incompatible daemon protocol; explicitly stop the existing daemon first"
            }
            ErrorCode::SetupRequired => {
                "Setup required; check client_id, Soloist key, SDK and Web API authorization"
            }
            ErrorCode::Unauthorized => {
                "Spotify authorization is required or expired; run myx interactively"
            }
            ErrorCode::Restricted => "Spotify denied this operation",
            ErrorCode::NotFound => "Requested resource was not found",
            ErrorCode::Quota => "Spotify application quota reached",
            ErrorCode::RateLimited => "Spotify rate limit reached; retry later",
            ErrorCode::Busy => "Daemon is busy; retry later",
            ErrorCode::Timeout => "Daemon operation timed out",
            ErrorCode::Unavailable => "Daemon operation is unavailable",
            ErrorCode::InvalidRequest => "Daemon rejected this request",
            ErrorCode::Internal => "Daemon operation failed",
            ErrorCode::WriteUncertain => {
                "Spotify write outcome is uncertain; refresh before retrying"
            }
            ErrorCode::CollectionChanged => "Collection changed while loading; refresh it",
        };
        let mut sanitized = crate::ipc::protocol::IpcError::new(error.code, message);
        if error.code == ErrorCode::RateLimited {
            sanitized.retry_after_secs = error.retry_after_secs.map(|seconds| seconds.min(300));
        }
        anyhow::Error::new(sanitized)
    })?;
    if let (Some(expected), Payload::Snapshot(snapshot)) = (config, &payload) {
        ensure!(
            snapshot.config_path == expected,
            "Daemon uses a different configuration; explicitly restart with --config PATH"
        );
    }
    Ok(payload)
}

#[derive(Debug)]
struct Absent;
impl std::fmt::Display for Absent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Daemon is offline")
    }
}
impl std::error::Error for Absent {}
fn absent(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Absent>().is_some()
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "IPC deadline exceeded"))
}

/// Nonblocking connect bounds even a saturated Unix listen backlog. Socket
/// ownership/type are checked before connecting and peer UID after connecting.
fn connect(paths: &RuntimePaths, deadline: Instant) -> Result<UnixStream> {
    // Status is read-only and must not create the runtime directory. Validate
    // its existing ancestry with the same nonsymlink rule as prepare().
    for ancestor in paths.directory.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "Daemon runtime path has a symlink component"
                );
                if ancestor == paths.directory {
                    ensure!(
                        metadata.is_dir()
                            && metadata.uid() == unsafe { libc::geteuid() }
                            && metadata.mode() & 0o077 == 0,
                        "Daemon runtime directory must be private and owned by this user"
                    );
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(Absent.into()),
            Err(_) => bail!("Cannot inspect daemon runtime directory"),
        }
    }
    match fs::symlink_metadata(&paths.socket) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_socket()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "Daemon socket must be private and owned by this user"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(Absent.into()),
        Err(_) => bail!("Cannot inspect daemon socket permissions"),
    }
    let bytes = paths.socket.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    ensure!(
        bytes.len() < address.sun_path.len() && !bytes.contains(&0),
        "Invalid daemon socket path"
    );
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
    }
    remaining(deadline)?;
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    ensure!(fd >= 0, "Cannot create daemon connection");
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } >= 0,
        "Cannot protect daemon connection"
    );
    stream.set_nonblocking(true)?;
    let result = unsafe {
        libc::connect(
            fd,
            &address as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ) {
            return Err(Absent.into());
        }
        ensure!(
            matches!(
                error.raw_os_error(),
                Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
            ),
            "Cannot connect to daemon; check socket permissions and protocol"
        );
        loop {
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let millis = remaining(deadline)?.as_millis().clamp(1, i32::MAX as u128) as i32;
            let result = unsafe { libc::poll(&mut poll, 1, millis) };
            if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            ensure!(result > 0, "Daemon connection timed out or failed");
            let error = stream
                .take_error()
                .map_err(|_| anyhow::anyhow!("Cannot inspect daemon connection"))?;
            ensure!(error.is_none(), "Daemon connection failed");
            break;
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

struct DeadlineStream {
    stream: UnixStream,
    deadline: Instant,
}
impl Read for DeadlineStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream
            .set_read_timeout(Some(remaining(self.deadline)?))?;
        self.stream.read(bytes)
    }
}
impl Write for DeadlineStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream
            .set_write_timeout(Some(remaining(self.deadline)?))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DetachedChild(Option<Child>);
impl DetachedChild {
    fn spawn(config: &Path) -> Result<Self> {
        let mut command =
            Command::new(std::env::current_exe().context("Cannot locate myx executable")?);
        command
            .arg("--config")
            .arg(config)
            .args(["daemon", "run"])
            .env_remove("MYX_SUPERVISED")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Only async-signal-safe work is allowed between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        Ok(Self(Some(command.spawn().map_err(|_| {
            anyhow::anyhow!("Cannot spawn detached daemon")
        })?)))
    }
}
impl Drop for DetachedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // A live owner must outlast this client. Reap it eventually without
            // making client success, failure or exit wait on its lifetime.
            let _ = thread::Builder::new()
                .name("myx-daemon-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_errors_keep_classification_and_bounded_retry_information() {
        use crate::ipc::protocol::IpcError;
        let mut error = IpcError::new(ErrorCode::RateLimited, "PRIVATE-PEER-DIAGNOSTIC");
        error.retry_after_secs = Some(u64::MAX);
        let response = Response {
            protocol_version: VERSION,
            request_id: 17,
            result: Err(error),
        };
        let failure = validate_response(response, 17, None).err().unwrap();
        let typed = failure.downcast_ref::<IpcError>().unwrap();
        assert_eq!(typed.code, ErrorCode::RateLimited);
        assert_eq!(typed.retry_after_secs, Some(300));
        assert!(!failure.to_string().contains("PRIVATE-PEER"));
        let response = Response {
            protocol_version: VERSION,
            request_id: 18,
            result: Err(IpcError::new(ErrorCode::WriteUncertain, "PRIVATE")),
        };
        let failure = validate_response(response, 18, None).err().unwrap();
        assert_eq!(
            failure.downcast_ref::<IpcError>().unwrap().code,
            ErrorCode::WriteUncertain
        );
    }

    #[test]
    fn response_must_match_version_id_and_selected_config() {
        let response = |version, id, config: &str| Response {
            protocol_version: version,
            request_id: id,
            result: Ok(Payload::Snapshot(Box::new(Snapshot::starting(
                config.into(),
                "myx".into(),
            )))),
        };
        assert!(validate_response(response(VERSION, 7, "/a"), 7, Some("/a")).is_ok());
        assert!(validate_response(response(VERSION + 1, 7, "/a"), 7, Some("/a")).is_err());
        assert!(validate_response(response(VERSION, 8, "/a"), 7, Some("/a")).is_err());
        assert!(validate_response(response(VERSION, 7, "/b"), 7, Some("/a")).is_err());
        assert!(validate_response(response(VERSION, 7, "/b"), 7, None).is_ok());
    }
}
