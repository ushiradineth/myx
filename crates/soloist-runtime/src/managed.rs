use crate::{
    audio::Audio,
    platform::Bonjour,
    private::{self, SessionLock},
    process::{self, OwnedChild},
    sdk::{self, Drain},
    Config,
};
use anyhow::{ensure, Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;

pub struct Managed {
    device_id: Option<String>,
    bonjour: Option<Bonjour>,
    sdk: Option<OwnedChild>,
    drains: Vec<Drain>,
    audio: Option<Audio>,
    scratch: Option<TempDir>,
    locks: Vec<SessionLock>,
}

impl Managed {
    pub fn start(
        mut config: Config,
        processing: Option<(
            crate::pcm::AudioControl,
            Box<dyn crate::pcm::AudioProcessor>,
        )>,
    ) -> Result<(Self, String)> {
        let deadline = Instant::now() + Duration::from_secs(25);
        validate_config(&config)?;
        sdk::validate_sdk(&config.sdk, config.expected_sdk_sha256.as_deref())?;
        let key = private::credential(&config.api_key)?;
        private::directory(&config.data_dir)?;
        private::directory(&config.cache_dir)?;
        config.data_dir = config.data_dir.canonicalize()?;
        config.cache_dir = config.cache_dir.canonicalize()?;
        let mut managed = Self {
            device_id: None,
            bonjour: None,
            sdk: None,
            drains: Vec::new(),
            audio: None,
            scratch: None,
            locks: Vec::new(),
        };
        // Lock both persistent stores. Lock files are never unlinked: doing so
        // would let another runtime lock a different inode while this one runs.
        managed.locks.push(SessionLock::acquire(&config.data_dir)?);
        if config.cache_dir != config.data_dir {
            managed.locks.push(SessionLock::acquire(&config.cache_dir)?);
        }
        let scratch = tempfile::Builder::new()
            .prefix(".soloist-run-")
            .tempdir_in(&config.data_dir)?;
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700))?;
        private::directory(scratch.path())?;
        managed.scratch = Some(scratch);
        managed.audio = Some(Audio::start(
            managed.scratch.as_ref().unwrap().path(),
            &config.audio_tools,
            deadline,
            processing,
        )?);
        let audio = managed.audio.as_ref().unwrap();
        let mut command = Command::new(&config.runner);
        command
            .args(["--no-rosetta", "--clear-env", "--timeout", "5", "--sysroot"])
            .arg(&config.sysroot);
        for (name, value) in audio
            .environment
            .iter()
            .filter(|(name, _)| name.starts_with("PULSE_"))
        {
            command.args(["--env", &format!("{name}={value}")]);
        }
        for (name, value) in [
            ("LD_LIBRARY_PATH", config.library_path.as_str()),
            (
                "HOME",
                config.data_dir.to_str().context("non-UTF8 data path")?,
            ),
            ("LANG", "C"),
            ("SSL_CERT_FILE", "/etc/ssl/cert.pem"),
        ] {
            command.args(["--env", &format!("{name}={value}")]);
        }
        // The SDK requires its API key as a CLI argument. Never debug-format the
        // command, argv, environment, raw output or a failing response body.
        command
            .arg(&config.sdk)
            .arg("--device-name")
            .arg(&config.device_name)
            .arg("--api-key")
            .arg(&key)
            .arg("--data-dir")
            .arg(&config.data_dir)
            .arg("--cache-dir")
            .arg(&config.cache_dir)
            .args(["--cache-size", "100", "--initial-volume"])
            .arg(config.initial_volume.to_string())
            .args(["--ws", "127.0.0.1:0", "--verbose"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &config.data_dir)
            .env("CI", "true")
            .current_dir(managed.scratch.as_ref().unwrap().path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process::remaining(deadline, Duration::from_secs(1))?;
        let started = SystemTime::now();
        managed.sdk = Some(OwnedChild::spawn(&mut command)?);
        drop(command);
        drop(key);
        let child = &mut managed.sdk.as_mut().unwrap().child;
        let pid = child.id();
        let port = Arc::new(AtomicU16::new(0));
        managed.drains.push(Drain::start(
            child.stdout.take().context("SDK stdout missing")?,
            port.clone(),
        )?);
        managed.drains.push(Drain::start(
            child.stderr.take().context("SDK stderr missing")?,
            port.clone(),
        )?);
        let mut control = None;
        let mut identity = None;
        loop {
            process::remaining(deadline, Duration::from_millis(50))?;
            ensure!(
                managed.children_running(),
                "owned runtime exited before readiness"
            );
            if control.is_none() {
                control = sdk::fresh_port(&config.data_dir.join("ws.port"), pid, started, deadline);
            }
            let pairing = port.load(Ordering::Relaxed);
            if let Some(control) = control {
                if pairing != 0 && identity.is_none() {
                    // Retry transient HTTP/listener startup races until the same
                    // global deadline; never retain response or error payloads.
                    if let Ok(value) =
                        sdk::pairing_identity(pid, pairing, control, &config.device_name, deadline)
                    {
                        identity = Some((pairing, value));
                    }
                }
                if let Some((pairing, ref identity)) = identity {
                    managed.bonjour = Some(Bonjour::register(pairing, identity, deadline)?);
                    managed.device_id = Some(identity.clone());
                    process::remaining(deadline, Duration::from_secs(1))?;
                    ensure!(
                        managed.children_running(),
                        "owned runtime exited during registration"
                    );
                    return Ok((managed, format!("ws://127.0.0.1:{control}")));
                }
            }
            thread::sleep(process::remaining(deadline, Duration::from_millis(50))?);
        }
    }

    fn children_running(&mut self) -> bool {
        self.sdk
            .as_mut()
            .is_some_and(|sdk| matches!(sdk.child.try_wait(), Ok(None)))
            && self.audio.as_ref().is_some_and(|audio| audio.running())
            && self
                .audio
                .as_mut()
                .is_some_and(|audio| matches!(audio.child.child.try_wait(), Ok(None)))
    }

    pub fn is_running(&mut self) -> bool {
        let running = self.children_running()
            && self
                .bonjour
                .as_mut()
                .is_some_and(|bonjour| bonjour.poll().is_ok());
        if !running {
            let _ = self.shutdown();
        }
        running
    }

    pub fn device_id(&self) -> Result<&str> {
        self.device_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .context("owned SDK device identity is unavailable")
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.device_id = None;
        self.bonjour.take();
        // Attempt both groups even if one fails. Keep the lock and scratch
        // until both stop, so explicit shutdown can be retried after errors.
        let mut failure = None;
        if let Some(sdk) = &mut self.sdk {
            match sdk.stop() {
                Ok(()) => {
                    self.sdk.take();
                }
                Err(error) => {
                    failure = Some(error);
                }
            }
        }
        for drain in &mut self.drains {
            if let Err(error) = drain.stop() {
                failure.get_or_insert(error);
            }
        }
        self.drains.clear();
        if let Some(audio) = &mut self.audio {
            match audio.stop() {
                Ok(()) => {
                    self.audio.take();
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if self.sdk.is_none() && self.audio.is_none() {
            if let Some(scratch) = self.scratch.take() {
                if let Err(error) = scratch.close() {
                    failure.get_or_insert(error.into());
                }
            }
            self.locks.clear();
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn validate_config(config: &Config) -> Result<()> {
    ensure!(
        (1..=128).contains(&config.device_name.len())
            && !config.device_name.chars().any(char::is_control),
        "invalid device name"
    );
    ensure!(
        config.initial_volume <= 100,
        "initial volume must be within 0..100"
    );
    let key_file = match &config.api_key {
        crate::ApiKey::File(path) => Some(path),
        crate::ApiKey::Inline(_) => None,
    };
    for path in [
        &config.sdk,
        &config.runner,
        &config.sysroot,
        &config.audio_tools,
        &config.data_dir,
        &config.cache_dir,
    ]
    .into_iter()
    .chain(key_file)
    {
        ensure!(
            path.is_absolute()
                && path
                    .to_str()
                    .is_some_and(|s| !s.contains(['\0', '\n', '\r'])),
            "runtime paths must be absolute UTF-8 without control separators"
        );
    }
    ensure!(
        !config.library_path.is_empty()
            && config
                .library_path
                .split(':')
                .all(|entry| entry.starts_with('/') && !entry.contains(['\0', '\n', '\r'])),
        "library path must contain only nonempty absolute entries"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            sdk: "/unprovisioned/sdk".into(),
            runner: "/unprovisioned/runner".into(),
            sysroot: "/unprovisioned/root".into(),
            library_path: "/unprovisioned/lib".into(),
            audio_tools: "/unprovisioned/audio".into(),
            api_key: crate::ApiKey::File("/unprovisioned/key".into()),
            data_dir: "/unprovisioned/data".into(),
            cache_dir: "/unprovisioned/cache".into(),
            device_name: "Myx".into(),
            initial_volume: 25,
            expected_sdk_sha256: None,
        }
    }

    #[test]
    fn invalid_configuration_fails_before_files_or_services_are_accessed() {
        assert!(validate_config(&config()).is_ok());
        for name in ["", "line\nfeed", "nul\0byte", &"x".repeat(129)] {
            let mut config = config();
            config.device_name = name.into();
            assert!(validate_config(&config).is_err());
        }
        for path in ["", "/lib:", "/lib:relative", "/lib\nother"] {
            let mut config = config();
            config.library_path = path.into();
            assert!(validate_config(&config).is_err());
        }
        let mut invalid = config();
        invalid.initial_volume = 101;
        assert!(crate::Runtime::start(invalid)
            .err()
            .unwrap()
            .to_string()
            .contains("volume"));
        let mut invalid = config();
        invalid.sdk = "relative".into();
        assert!(validate_config(&invalid).is_err());
        let mut inline = config();
        inline.api_key = crate::ApiKey::Inline("test-key".into());
        assert!(validate_config(&inline).is_ok());
        inline.api_key = crate::ApiKey::File("relative".into());
        assert!(validate_config(&inline).is_err());
    }

    #[test]
    fn runtime_is_send_for_parent_actor_ownership() {
        fn assert_send<T: Send>() {}
        assert_send::<crate::Runtime>();
    }

    #[test]
    fn scratch_cleanup_keeps_persistent_data_and_releases_lock() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        private::directory(&data).unwrap();
        std::fs::write(data.join("persistent"), b"keep").unwrap();
        let scratch = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(&data)
            .unwrap();
        let path = scratch.path().to_owned();
        let lock = SessionLock::acquire(&data).unwrap();
        let mut managed = Managed {
            device_id: Some("public-sdk-id".into()),
            bonjour: None,
            sdk: None,
            drains: Vec::new(),
            audio: None,
            scratch: Some(scratch),
            locks: vec![lock],
        };
        managed.shutdown().unwrap();
        assert!(managed.device_id().is_err());
        managed.shutdown().unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read(data.join("persistent")).unwrap(), b"keep");
        assert!(SessionLock::acquire(&data).is_ok());
    }

    #[test]
    fn dropping_runtime_removes_only_scratch_and_releases_lock() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        private::directory(&data).unwrap();
        std::fs::write(data.join("persistent"), b"keep").unwrap();
        let scratch = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(&data)
            .unwrap();
        let path = scratch.path().to_owned();
        let runtime = crate::Runtime {
            endpoint: "ws://127.0.0.1:1".into(),
            managed: Managed {
                device_id: None,
                bonjour: None,
                sdk: None,
                drains: Vec::new(),
                audio: None,
                scratch: Some(scratch),
                locks: vec![SessionLock::acquire(&data).unwrap()],
            },
        };
        assert!(SessionLock::acquire(&data).is_err());
        drop(runtime);
        assert!(!path.exists());
        assert_eq!(std::fs::read(data.join("persistent")).unwrap(), b"keep");
        assert!(SessionLock::acquire(&data).is_ok());
    }
}
