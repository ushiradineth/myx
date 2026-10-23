use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use myx::daemon::server::Server;
use myx::domain::Snapshot;
use myx::ipc::protocol::{self, ErrorCode, Operation, Payload, Request, Response, VERSION};
use myx::ipc::unix::{verify_peer, RuntimePaths};

fn request(paths: &RuntimePaths, version: u16, config: &str) -> Response {
    let mut stream = UnixStream::connect(&paths.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    verify_peer(&stream).unwrap();
    protocol::send(
        &mut stream,
        &Request {
            protocol_version: version,
            request_id: 42,
            config_path: Some(config.into()),
            operation: Operation::Status,
        },
    )
    .unwrap();
    protocol::receive(&mut stream).unwrap()
}

#[test]
fn racing_owners_config_versions_and_client_detach_are_real_socket_contracts() {
    let root = tempfile::tempdir().unwrap();
    // macOS exposes TMPDIR via /var, a symlink to /private/var. Production
    // rejects symlink components; make the owned fixture use its real path.
    let paths = RuntimePaths::in_directory(root.path().canonicalize().unwrap().join("owner"));
    let snapshot = Snapshot::starting("/test/config".into(), "Office Speakers".into());
    let handler = Arc::new(move |_| Ok(Payload::Snapshot(Box::new(snapshot.clone()))));
    let mut owner = Server::bind(paths.clone(), "/test/config".into(), handler.clone()).unwrap();
    assert!(Server::bind(paths.clone(), "/test/config".into(), handler).is_err());
    let response = request(&paths, VERSION, "/test/config");
    assert_eq!(response.request_id, 42);
    assert!(matches!(response.result, Ok(Payload::Snapshot(_))));
    // Dropping each client connection is not global shutdown.
    assert!(matches!(
        request(&paths, VERSION, "/test/config").result,
        Ok(Payload::Snapshot(_))
    ));
    assert_eq!(
        request(&paths, VERSION + 1, "/test/config")
            .result
            .err()
            .unwrap()
            .code,
        ErrorCode::VersionMismatch
    );
    assert_eq!(
        request(&paths, VERSION, "/other/config")
            .result
            .err()
            .unwrap()
            .code,
        ErrorCode::ConfigMismatch
    );
    let mut oversized = UnixStream::connect(&paths.socket).unwrap();
    oversized
        .write_all(&((protocol::MAX_FRAME + 1) as u32).to_be_bytes())
        .unwrap();
    drop(oversized);
    assert!(request(&paths, VERSION, "/test/config").result.is_ok());
    owner.stop();
    assert!(!paths.socket.exists());
}

#[test]
fn simultaneous_starts_have_one_locked_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::in_directory(root.path().canonicalize().unwrap().join("owner"));
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let (results, received) = std::sync::mpsc::channel();
    let mut workers = Vec::new();
    for _ in 0..2 {
        let paths = paths.clone();
        let barrier = barrier.clone();
        let results = results.clone();
        workers.push(std::thread::spawn(move || {
            let snapshot = Snapshot::starting("/test/config".into(), "Device".into());
            let handler = Arc::new(move |_| Ok(Payload::Snapshot(Box::new(snapshot.clone()))));
            barrier.wait();
            results
                .send(Server::bind(paths, "/test/config".into(), handler))
                .unwrap();
        }));
    }
    barrier.wait();
    let first = received.recv_timeout(Duration::from_secs(3)).unwrap();
    let second = received.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(request(&paths, VERSION, "/test/config").result.is_ok());
    for worker in workers {
        worker.join().unwrap();
    }
    drop((first, second));
    assert!(!paths.socket.exists());
}
