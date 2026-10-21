// Adapted from tools/soloist-elfuse under the project's MIT license.
use crate::{platform, private, process};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicU16, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

pub fn validate_sdk(path: &Path, expected: Option<&str>) -> Result<()> {
    let bytes = private::read_regular(path, None, 64 * 1024 * 1024)?;
    validate_elf(&bytes)?;
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

fn validate_elf(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() >= 64 && &bytes[..7] == b"\x7fELF\x02\x01\x01",
        "SDK must be an ELF64 little-endian executable"
    );
    let kind = u16::from_le_bytes(bytes[16..18].try_into()?);
    let machine = u16::from_le_bytes(bytes[18..20].try_into()?);
    let version = u32::from_le_bytes(bytes[20..24].try_into()?);
    let offset = u64::from_le_bytes(bytes[32..40].try_into()?);
    let header_size = u16::from_le_bytes(bytes[52..54].try_into()?);
    let entry_size = u16::from_le_bytes(bytes[54..56].try_into()?);
    let count = u16::from_le_bytes(bytes[56..58].try_into()?);
    ensure!(
        matches!(kind, 2 | 3)
            && machine == 183
            && version == 1
            && header_size == 64
            && entry_size == 56
            && count != 0
            && offset >= 64
            && offset
                .checked_add(u64::from(count) * 56)
                .is_some_and(|end| end <= bytes.len() as u64),
        "SDK must be a bounded AArch64 executable with valid ELF program headers"
    );
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

impl Drain {
    pub fn start(
        mut pipe: impl Read + AsRawFd + Send + 'static,
        port: Arc<AtomicU16>,
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
                                        if let Some(value) = pairing_port(&line) {
                                            port.store(value, Ordering::Relaxed);
                                        }
                                    }
                                    line.clear();
                                    overflow = false;
                                } else if line.len() < 8192 && !overflow {
                                    line.push(byte);
                                } else {
                                    overflow = true;
                                }
                            }
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

    fn elf() -> Vec<u8> {
        let mut bytes = vec![0; 120];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&183u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        bytes
    }

    #[test]
    fn sdk_checks_architecture_bounds_digest_and_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let bytes = elf();
        std::fs::write(&sdk, &bytes).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        assert!(validate_sdk(&sdk, Some(&hash)).is_ok());
        assert!(validate_sdk(&sdk, Some(&"0".repeat(64))).is_err());
        assert!(validate_sdk(&sdk, Some("not a digest")).is_err());
        for offset in [0, 4, 5, 6, 16, 18, 20, 32, 52, 54, 56] {
            let mut wrong = bytes.clone();
            wrong[offset] = 255;
            assert!(validate_elf(&wrong).is_err(), "offset {offset}");
        }
        assert!(validate_elf(&bytes[..119]).is_err());
        assert!(validate_elf(&bytes[..6]).is_err());
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
        let mut drain = Drain::start(reader, port.clone()).unwrap();
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
}
