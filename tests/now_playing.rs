//! Credential-free scenarios. Written ahead of the primary's MAC4 validation batch.
//! Requires the primary to expose `myx::outputs` and wire Publisher after owner-lock acquisition.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use myx::config::NowPlayingConfig;
use myx::domain::{ItemKind, MetadataStatus, PlaybackStatus, Snapshot, TrackMetadata};
use myx::outputs::now_playing::Publisher;

fn snapshot() -> Snapshot {
    let mut snapshot =
        Snapshot::starting("/fixture/not-a-real-config".into(), "Owned fixture".into());
    snapshot.playback = PlaybackStatus::Playing;
    snapshot.owned_device_verified = true;
    snapshot.soloist_ready = true;
    snapshot.locally_active = true;
    snapshot.track = Some(TrackMetadata::pending(
        ItemKind::Episode,
        "spotify:episode:fixture".into(),
    ));
    snapshot.metadata_status = MetadataStatus::Loading;
    snapshot
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "publisher did not reach expected state"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn public_worker_publishes_latest_metadata_and_clean_stop_with_stable_owner_identity() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap().join("output.json");
    let initial = snapshot();
    let owner = initial.session_id.clone();
    let pid = initial.pid;
    let mut publisher = Publisher::start(
        &NowPlayingConfig {
            enabled: true,
            path: Some(path.clone()),
        },
        &[],
        initial.clone(),
    );
    wait_for(|| path.exists());
    let pending: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(pending["metadata_status"], "loading");
    assert!(pending["track"]["title"].is_null());
    for sequence in 1..100 {
        let mut next = initial.clone();
        next.sequence = sequence;
        next.track.as_mut().unwrap().title = Some(format!("夜の音楽\n{sequence}"));
        next.metadata_status = MetadataStatus::Ready;
        publisher.submit(next);
    }
    wait_for(|| {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["track"]["title"] == "夜の音楽\n99"
    });
    assert!(publisher.error().is_none());
    let start = Instant::now();
    publisher.finish().unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.last(), Some(&b'\n'));
    let stopped: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(stopped["session_id"], owner);
    assert_eq!(stopped["pid"], pid);
    assert_eq!(stopped["status"], "stopped");
    assert!(stopped["track"].is_null());
    assert_eq!(stopped["position_ms"], 0);
}

#[test]
fn disabled_invalid_output_never_creates_files() {
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::start(
        &NowPlayingConfig {
            enabled: false,
            path: Some(PathBuf::from("~/\0invalid")),
        },
        &[root.path().to_path_buf()],
        snapshot(),
    );
    publisher.submit(snapshot());
    publisher.finish().unwrap();
    assert!(publisher.error().is_none());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn missing_parent_reports_only_safe_error_then_retries_without_stopping_the_producer() {
    let root = tempfile::tempdir().unwrap();
    let parent = root
        .path()
        .canonicalize()
        .unwrap()
        .join("sensitive-fixture-name");
    let path = parent.join("output.json");
    let mut publisher = Publisher::start(
        &NowPlayingConfig {
            enabled: true,
            path: Some(path.clone()),
        },
        &[],
        snapshot(),
    );
    wait_for(|| publisher.error().is_some());
    let error = publisher.error().unwrap();
    assert!(!error.contains("sensitive-fixture-name"));
    assert!(!parent.exists());
    std::fs::create_dir(&parent).unwrap();
    publisher.submit(snapshot());
    wait_for(|| path.exists() && publisher.error().is_none());
    publisher.finish().unwrap();
}

#[test]
fn protected_destination_is_preserved_and_final_failure_is_visible() {
    let root = tempfile::tempdir().unwrap();
    let path = root
        .path()
        .canonicalize()
        .unwrap()
        .join("credential-fixture");
    std::fs::write(&path, b"do-not-replace-fixture").unwrap();
    let mut publisher = Publisher::start(
        &NowPlayingConfig {
            enabled: true,
            path: Some(path.clone()),
        },
        std::slice::from_ref(&path),
        snapshot(),
    );
    wait_for(|| publisher.error().is_some());
    let start = Instant::now();
    assert!(publisher.finish().is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(std::fs::read(&path).unwrap(), b"do-not-replace-fixture");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
