// Adapted from tools/soloist-elfuse under the project's MIT license.
use crate::{executable, platform, private, process};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

pub fn validate_sdk(path: &Path, expected: Option<&str>) -> Result<()> {
    let bytes = private::read_regular(path, None, 64 * 1024 * 1024)?;
    executable::validate(&bytes, executable::Target::LinuxAarch64)?;
    if let Some(hash) = expected {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "expected SDK SHA256 must contain 64 hexadecimal digits"
        );
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == hash.to_ascii_lowercase(),
            "SDK SHA256 does not match provisioned digest"
        );
    }
    Ok(())
}

pub fn listener_owned(pid: u32, port: u16, loopback: bool, deadline: Instant) -> bool {
    let result = process::output(
        Command::new("/usr/sbin/lsof")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args([
                "-nP",
                "-a",
                "-p",
                &pid.to_string(),
                &format!("-iTCP:{port}"),
                "-sTCP:LISTEN",
                "-Fpn",
            ]),
        deadline.min(Instant::now() + Duration::from_secs(2)),
        16384,
    );
    result.is_ok_and(|bytes| listener_records(&bytes, pid, port, loopback))
}

fn listener_records(bytes: &[u8], pid: u32, port: u16, loopback: bool) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let expected_pid = pid.to_string();
    let suffix = format!(":{port}");
    let expected_name = format!("127.0.0.1:{port}");
    let mut owner = false;
    let mut found = false;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix('p') {
            if value != expected_pid {
                return false;
            }
            owner = true;
        } else if let Some(value) = line.strip_prefix('n') {
            if !owner || !value.ends_with(&suffix) || (loopback && value != expected_name) {
                return false;
            }
            found = true;
        }
    }
    found
}

pub fn fresh_port(path: &Path, pid: u32, since: SystemTime, deadline: Instant) -> Option<u16> {
    let file = private::open_regular(path, None).ok()?;
    let meta = file.metadata().ok()?;
    if meta.modified().ok()? < since || meta.len() > 32 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(33).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 32 {
        return None;
    }
    let port = std::str::from_utf8(&bytes)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()?;
    (port != 0 && listener_owned(pid, port, true, deadline)).then_some(port)
}

/// Drains raw output without retaining it. Only a public Zeroconf port escapes.
/// Nonblocking reads make Drop independent of descendants holding pipe writers.
pub struct Drain {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub(crate) struct Diagnostics {
    get_info: AtomicU64,
    add_user: AtomicU64,
    flags: AtomicU32,
}
impl Diagnostics {
    pub fn snapshot(&self) -> crate::SdkDiagnostics {
        crate::SdkDiagnostics {
            get_info_requests: self.get_info.load(Ordering::Relaxed),
            add_user_requests: self.add_user.load(Ordering::Relaxed),
            failure_flags: self.flags.load(Ordering::Relaxed),
        }
    }
    fn note(&self, line: &[u8]) {
        // Match fixed program phrases without allocating/copying a lowercased
        // raw line. Never retain a payload, username, credential or broker route.
        let has = |text: &[u8]| {
            line.windows(text.len())
                .any(|part| part.eq_ignore_ascii_case(text))
        };
        let increment = |counter: &AtomicU64| {
            let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
        };
        if has(b"zeroconf: received getInfo") {
            increment(&self.get_info);
        }
        if has(b"zeroconf: received addUser") {
            increment(&self.add_user);
        }
        let mut flags = 0;
        if has(b"login failed") || has(b"zeroconf_login_failed") {
            flags |= 1;
        }
        if has(b"could not resolve")
            || has(b"connection failed")
            || has(b"connection timed out")
            || has(b"network is unreachable")
        {
            flags |= 2;
        }
        if has(b"certificate verify failed")
            || has(b"tls handshake failed")
            || has(b"ssl certificate problem")
        {
            flags |= 4;
        }
        if has(b"make sure --api-key is valid")
            || has(b"invalid api key")
            || has(b"api key rejected")
        {
            flags |= 8;
        }
        if has(b"client expired, please update to a newer version") {
            flags |= 16;
        }
        if has(b"zeroconf_bad_request")
            || has(b"zeroconf_spotify_error")
            || has(b"zeroconf_other_spotify_error")
            || has(b"hmac verification failed")
        {
            flags |= 32;
        }
        if flags != 0 {
            self.flags.fetch_or(flags, Ordering::Relaxed);
        }
    }
}

impl Drain {
    pub fn start(
        mut pipe: impl Read + AsRawFd + Send + 'static,
        port: Arc<AtomicU16>,
        diagnostics: Arc<Diagnostics>,
    ) -> Result<Self> {
        let fd = pipe.as_raw_fd();
        // SAFETY: the owned pipe's descriptor remains valid for both calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
            "cannot make SDK output drain nonblocking"
        );
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let handle = thread::Builder::new()
            .name("soloist-output".into())
            .spawn(move || {
                // Keep only bounded public discovery facts, never raw SDK lines,
                // credentials, account IDs, broker routes or exception payloads.
                let mut buffer = [0; 4096];
                let mut line = Vec::with_capacity(8192);
                let mut overflow = false;
                while !stopping.load(Ordering::Relaxed) {
                    match pipe.read(&mut buffer) {
                        Ok(0) => return,
                        Ok(count) => {
                            for &byte in &buffer[..count] {
                                if byte == b'\n' {
                                    if !overflow {
                                        diagnostics.note(&line);
                                        if let Some(value) = pairing_port(&line) {
                                            port.store(value, Ordering::Relaxed);
                                        }
                                    }
                                    line.fill(0);
                                    line.clear();
                                    overflow = false;
                                } else if line.len() < 8192 && !overflow {
                                    line.push(byte);
                                } else {
                                    overflow = true;
                                }
                            }
                            buffer[..count].fill(0);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => return,
                    }
                }
            })
            .context("cannot start SDK output drain")?;
        Ok(Self {
            stop,
            thread: Some(handle),
        })
    }

    pub fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("SDK output drain failed"))?;
        }
        Ok(())
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn pairing_port(line: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(line).ok()?;
    let (_, after) = text.split_once("Started ZeroConf service on port ")?;
    let (number, _) = after.split_once(" path /zc")?;
    if !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse::<u16>().ok().filter(|&value| value != 0)
}

/// Return only the public deviceID from getInfo after verifying the listener
/// belongs to the owned SDK. The response and pairing key remain private.
pub fn pairing_identity(
    pid: u32,
    port: u16,
    control: u16,
    name: &str,
    deadline: Instant,
) -> Result<String> {
    ensure!(
        port != 0 && port != control && listener_owned(pid, port, false, deadline),
        "pairing endpoint is not owned by SDK"
    );
    let bytes = process::output(
        Command::new("/usr/bin/curl")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args([
                "--disable",
                "--fail",
                "--silent",
                "--noproxy",
                "*",
                "--max-time",
                "2",
                "--max-filesize",
                "65536",
                "--proto",
                "=http",
                &format!("http://127.0.0.1:{port}/zc?action=getInfo"),
            ]),
        deadline.min(Instant::now() + Duration::from_secs(2)),
        65536,
    )?;
    identity_from_info(&bytes, name)
}

fn identity_from_info(bytes: &[u8], name: &str) -> Result<String> {
    let info: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("invalid pairing endpoint JSON"))?;
    ensure!(
        info["status"] == 101
            && info["remoteName"] == name
            && info["publicKey"].as_str().is_some_and(|s| !s.is_empty()),
        "pairing endpoint is not the configured device"
    );
    let identity = info["deviceID"]
        .as_str()
        .context("missing pairing device identity")?;
    platform::validate_registration(1, identity)?;
    Ok(identity.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    #[test]
    fn sdk_checks_architecture_bounds_digest_and_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let bytes = executable::tests::elf();
        std::fs::write(&sdk, &bytes).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        assert!(validate_sdk(&sdk, Some(&hash)).is_ok());
        assert!(validate_sdk(&sdk, Some(&"0".repeat(64))).is_err());
        assert!(validate_sdk(&sdk, Some("not a digest")).is_err());
        for offset in [0, 4, 5, 6, 16, 18, 20, 32, 52, 54, 56] {
            let mut wrong = bytes.clone();
            wrong[offset] = 255;
            assert!(
                executable::validate(&wrong, executable::Target::LinuxAarch64).is_err(),
                "offset {offset}"
            );
        }
        assert!(executable::validate(&bytes[..119], executable::Target::LinuxAarch64).is_err());
        assert!(executable::validate(&bytes[..6], executable::Target::LinuxAarch64).is_err());
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&sdk, &alias).unwrap();
        assert!(validate_sdk(&alias, None).is_err());
    }

    #[test]
    fn listener_records_require_exact_pid_and_all_control_binds_loopback() {
        assert!(listener_records(b"p42\nn127.0.0.1:1234\n", 42, 1234, true));
        for text in [
            "p41\nn127.0.0.1:1234\n",
            "p42\nn*:1234\n",
            "p42\nn127.0.0.1:1234\nn*:1234\n",
            "n127.0.0.1:1234\n",
            "p42\nn127.0.0.1:12345\n",
        ] {
            assert!(!listener_records(text.as_bytes(), 42, 1234, true));
        }
        assert!(listener_records(b"p42\nn*:1234\n", 42, 1234, false));
    }

    #[test]
    fn port_file_rejects_stale_symlink_oversized_and_invalid_values() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ws.port");
        let deadline = Instant::now() + Duration::from_secs(2);
        let future = SystemTime::now() + Duration::from_secs(60);
        private::write_private(&path, b"12345").unwrap();
        assert_eq!(
            fresh_port(&path, std::process::id(), future, deadline),
            None
        );
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert_eq!(
            fresh_port(&alias, std::process::id(), SystemTime::UNIX_EPOCH, deadline),
            None
        );
        for value in [b"0".as_slice(), b"65536", b"not a port", &[b'1'; 33]] {
            std::fs::write(&path, value).unwrap();
            assert_eq!(
                fresh_port(&path, std::process::id(), SystemTime::UNIX_EPOCH, deadline),
                None
            );
        }
    }

    #[test]
    fn pairing_validates_public_identity_and_does_not_return_account_fields() {
        let mut info = json!({"status":101,"remoteName":"Myx","publicKey":"public",
            "deviceID":"abc-123","account":"PRIVATE-SENTINEL"});
        assert_eq!(
            identity_from_info(&serde_json::to_vec(&info).unwrap(), "Myx").unwrap(),
            "abc-123"
        );
        for (field, wrong) in [
            ("status", json!(102)),
            ("remoteName", json!("other")),
            ("publicKey", json!("")),
            ("deviceID", json!("bad.name")),
            ("deviceID", json!("设备")),
            ("deviceID", json!("")),
            ("deviceID", Value::Null),
        ] {
            let old = info[field].clone();
            info[field] = wrong;
            let error = identity_from_info(&serde_json::to_vec(&info).unwrap(), "Myx").unwrap_err();
            assert!(!format!("{error:#}").contains("PRIVATE-SENTINEL"));
            info[field] = old;
        }
    }

    #[test]
    fn drain_is_bounded_and_can_stop_while_writer_remains_open() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let port = Arc::new(AtomicU16::new(0));
        let diagnostics = Arc::new(Diagnostics::default());
        let mut drain = Drain::start(reader, port.clone(), diagnostics.clone()).unwrap();
        writer.write_all(b"private account payload\n").unwrap();
        writer.write_all(&vec![b'x'; 9000]).unwrap();
        writer
            .write_all(b"Started ZeroConf service on port 9999 path /zc\n")
            .unwrap();
        writer
            .write_all(b"Started ZeroConf service on port 12345 path /zc secret\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while port.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(port.load(Ordering::Relaxed), 12345);
        drain.stop().unwrap();
        drain.stop().unwrap();
    }

    #[test]
    fn auth_diagnostics_keep_only_fixed_categories_and_request_counts() {
        let diagnostics = Diagnostics::default();
        diagnostics.note(b"zeroconf: received getInfo PRIVATE-USERNAME");
        diagnostics.note(b"zeroconf: received addUser PRIVATE-LOGIN-BLOB");
        diagnostics.note(b"login failed: make sure --api-key is valid PRIVATE-KEY");
        diagnostics.note(b"certificate verify failed PRIVATE-TOKEN");
        let state = diagnostics.snapshot();
        assert_eq!(state.get_info_requests, 1);
        assert_eq!(state.add_user_requests, 1);
        assert_eq!(state.failure_flags, 1 | 4 | 8);
        let text = state.safe_message();
        assert!(text.contains("api_key_validity_hint") && text.contains("tls_failure"));
        assert!(!text.contains("PRIVATE"));
        let clean = Diagnostics::default();
        clean.note(b"username=PRIVATE credential=PRIVATE arbitrary error");
        assert_eq!(clean.snapshot(), crate::SdkDiagnostics::default());
    }
}
