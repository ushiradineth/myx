#![cfg(feature = "soloist")]

//! Exercise the production Myx binary and its provider boundary, without Spotify
//! credentials, native audio, a terminal, or test hooks in the application.

use std::io::ErrorKind;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use serde_json::{json, Value};
use tungstenite::Message;

const REQUESTED: &str = "spotify:track:0000000000000000000001";
const OTHER: &str = "spotify:track:0000000000000000000002";

#[derive(Clone, Copy)]
enum Response {
    Playback,
    AcknowledgementOnly,
    WrongTrack,
}

fn run(response: Response) -> std::process::Output {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/myx");
    std::fs::create_dir_all(&config).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    std::fs::write(
        config.join("config.toml"),
        format!("[soloist]\nendpoint = {endpoint:?}\n"),
    )
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server = std::thread::spawn(move || {
        let mut peer = loop {
            if server_stop.load(Ordering::Acquire) {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    // Darwin inherits the listener's nonblocking flag on accept.
                    // The fixture handshake is deliberately synchronous.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut peer = tungstenite::accept(stream).unwrap();
                    peer.get_ref()
                        .set_read_timeout(Some(Duration::from_millis(50)))
                        .unwrap();
                    peer.send(Message::Text(
                        json!({"type":"auth_state","logged_in":true,"is_active":false})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
                    break peer;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("fixture listener failed: {error}"),
            }
        };
        let mut active = false;
        let mut playing = false;
        let mut uri = REQUESTED;
        while !server_stop.load(Ordering::Acquire) {
            let message = match peer.read() {
                Ok(Message::Text(text)) => serde_json::from_str::<Value>(&text).unwrap(),
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    continue
                }
                Err(_) => break,
            };
            let name = message["command"].as_str().unwrap_or("");
            let item = |uri| json!({"uri":uri,"decorations":{"identity":{"name":"Fixture song"},"playback":{"duration_ms":210000}}});
            let event = match name {
                "get_auth_state" => {
                    json!({"type":"auth_state","logged_in":true,"is_active":active})
                }
                "get_queue" => json!({"type":"queue_changed","upcoming":[]}),
                "get_state" => {
                    json!({"type":"playback_state","is_active":active,"status":if playing {"playing"} else {"idle"},"item":if playing {item(uri)} else {json!({})},"volume":25,"position":{"position_ms":0}})
                }
                "activate" => {
                    active = true;
                    json!({"type":"device_changed","is_active":true})
                }
                "play" if !matches!(response, Response::AcknowledgementOnly) => {
                    playing = true;
                    if matches!(response, Response::WrongTrack) {
                        uri = OTHER;
                    }
                    peer.send(Message::Text(
                        json!({"type":"track_changed","item":item(uri)})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
                    json!({"type":"playback_changed","status":"playing"})
                }
                _ => json!({"type":"command_result","command":name}),
            };
            if peer.send(Message::Text(event.to_string().into())).is_err() {
                break;
            }
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_myx"))
        .args([
            "--api",
            "soloist",
            "--no-web-api",
            "--non-interactive",
            "--headless",
            "--duration",
            "1",
            REQUESTED,
        ])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        // CLI must override an explicit different environment backend.
        .env("MYX_BACKEND", "normal")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    output
}

#[test]
fn production_binary_selects_soloist_without_normal_oauth_or_a_terminal() {
    let output = run(Response::Playback);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("soloist backend started"));
}

#[test]
fn command_acceptance_is_not_playback_success() {
    assert!(!run(Response::AcknowledgementOnly).status.success());
}

#[test]
fn an_unrelated_cached_track_is_not_requested_playback_success() {
    assert!(!run(Response::WrongTrack).status.success());
}

#[test]
fn invalid_inline_key_configuration_never_prints_the_credential() {
    for text in [
        "[soloist]\napi_key = \"PRIVATE-SENTINEL\"\napi_key_file = \"/private/key\"\n",
        "[soloist]\napi_key = \"PRIVATE-SENTINEL\n",
    ] {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/myx");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("config.toml"), text).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_myx"))
            .args([
                "--api",
                "soloist",
                "--no-web-api",
                "--non-interactive",
                "--headless",
                "--duration",
                "1",
                REQUESTED,
            ])
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE-SENTINEL"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE-SENTINEL"));
    }
}
