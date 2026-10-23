use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use fs2::FileExt;

use crate::ipc::protocol::{
    self, ErrorCode, IpcError, Operation, Payload, Request, Response, VERSION,
};
use crate::ipc::unix::{open_private_lock, verify_peer, RuntimePaths};

pub type Handler = dyn Fn(Operation) -> Result<Payload, IpcError> + Send + Sync + 'static;
struct RequestCount(Arc<AtomicUsize>);
impl Drop for RequestCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A real locked/authenticated endpoint. The handler seam permits deterministic
/// contract fixtures without introducing a production service-disable mode.
pub struct Server {
    paths: RuntimePaths,
    inode: u64,
    _lock: File,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub fn bind(paths: RuntimePaths, config: String, handler: Arc<Handler>) -> Result<Self> {
        paths.prepare()?;
        let lock = open_private_lock(&paths.daemon_lock)?;
        lock.try_lock_exclusive()
            .context("Another daemon owns this user session")?;
        match fs::symlink_metadata(&paths.socket) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket()
                        && metadata.uid() == unsafe { libc::geteuid() }
                        && metadata.mode() & 0o077 == 0,
                    "Refusing an unrelated or unsafe IPC destination"
                );
                match UnixStream::connect(&paths.socket) {
                    Ok(_) => anyhow::bail!("A daemon is already listening"),
                    Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                        fs::remove_file(&paths.socket)?
                    }
                    Err(_) => {
                        anyhow::bail!("Cannot establish ownership of the existing IPC endpoint")
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => anyhow::bail!("Cannot inspect IPC endpoint"),
        }
        let listener = UnixListener::bind(&paths.socket).context("Bind private daemon endpoint")?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
        let inode = fs::symlink_metadata(&paths.socket)?.ino();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let worker = thread::Builder::new()
            .name("myx-ipc".into())
            .spawn(move || {
                for incoming in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = incoming else {
                        break;
                    };
                    if active.fetch_add(1, Ordering::AcqRel) >= 16 {
                        active.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    }
                    let count = RequestCount(active.clone());
                    let handler = handler.clone();
                    let config = config.clone();
                    let _ =
                        thread::Builder::new()
                            .name("myx-ipc-request".into())
                            .spawn(move || {
                                let _count = count;
                                serve(stream, &config, &handler);
                            });
                }
            })?;
        Ok(Self {
            paths,
            inode,
            _lock: lock,
            stopping,
            worker: Some(worker),
        })
    }

    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.paths.socket);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if fs::symlink_metadata(&self.paths.socket)
            .is_ok_and(|metadata| metadata.ino() == self.inode && metadata.file_type().is_socket())
        {
            let _ = fs::remove_file(&self.paths.socket);
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve(mut stream: UnixStream, config: &str, handler: &Arc<Handler>) {
    if verify_peer(&stream).is_err() {
        return;
    }
    if stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .is_err()
        || stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        return;
    }
    let Ok(request) = protocol::receive::<Request>(&mut stream) else {
        return;
    };
    let result = if request.protocol_version != VERSION {
        Err(IpcError::new(
            ErrorCode::VersionMismatch,
            "Daemon/client version mismatch; upgrade and restart daemon",
        ))
    } else if request
        .config_path
        .as_deref()
        .is_some_and(|path| path != config)
        || (request.config_path.is_none() && !matches!(request.operation, Operation::Status))
    {
        Err(IpcError::new(
            ErrorCode::ConfigMismatch,
            "Daemon uses another config; inspect status and explicitly restart it",
        ))
    } else {
        handler(request.operation)
    };
    let response = Response {
        protocol_version: VERSION,
        request_id: request.request_id,
        result,
    };
    if protocol::send(&mut stream, &response)
        .is_err_and(|error| error.kind() == io::ErrorKind::InvalidData)
    {
        // Framing validates size before any write, so only that error permits a
        // small typed reply. A partial I/O failure must never be replayed.
        let _ = protocol::send(
            &mut stream,
            &Response {
                protocol_version: VERSION,
                request_id: response.request_id,
                result: Err(IpcError::new(
                    ErrorCode::InvalidRequest,
                    "Reply exceeds IPC limit; request a smaller collection page",
                )),
            },
        );
    }
}
