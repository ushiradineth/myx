// Adapted from tools/soloist-elfuse under the project's MIT license.
use crate::{
    platform, private,
    process::{self, OwnedChild},
};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn configuration(
    object: u32,
    cookie: &Path,
    port: u16,
    frames: u32,
    processed: bool,
) -> Result<String> {
    ensure!(
        object != 0 && port != 0 && (1..=65536).contains(&frames),
        "invalid output device, port or buffer size"
    );
    let cookie = cookie.to_str().context("cookie path must be UTF-8")?;
    ensure!(!cookie.contains(['\n', '\r', '\0']), "invalid cookie path");
    let virtual_sink = if processed {
        "load-module module-null-sink sink_name=myx_input format=float32le rate=44100 channels=2 channel_map=front-left,front-right norewinds=true\nset-default-sink myx_input\n"
    } else {
        ""
    };
    let native = if processed {
        let socket = Path::new(cookie)
            .parent()
            .context("cookie directory missing")?
            .join("native");
        format!(
            "load-module module-native-protocol-unix socket={} auth-anonymous=no auth-cookie={}\n",
            serde_json::to_string(socket.to_str().context("non-UTF8 PCM socket")?)?,
            serde_json::to_string(cookie)?
        )
    } else {
        String::new()
    };
    Ok(format!(".fail\nload-module module-coreaudio-device object_id={object} record=no playback=yes ioproc_frames={frames}\n{virtual_sink}load-module module-native-protocol-tcp listen=127.0.0.1 port={port} auth-anonymous=no auth-cookie={}\n{native}", serde_json::to_string(cookie)?))
}

pub struct Audio {
    pub child: OwnedChild,
    pub environment: BTreeMap<String, String>,
    bridge: Option<crate::pulse::Bridge>,
    tools: PathBuf,
    output: Option<(u32, u32)>,
    output_module: Option<u64>,
    listener: platform::OutputListener,
    recovery: Recovery,
}

/// Five reloads per observed device change, at 250/500/1000/2000/4000ms.
/// Exhaustion remains visibly unavailable until another device notification.
#[derive(Default)]
struct Recovery {
    attempts: u32,
    next: Option<Instant>,
}
impl Recovery {
    fn reset(&mut self) {
        *self = Self::default();
    }
    fn due(&self, now: Instant) -> bool {
        self.attempts < 5 && self.next.is_none_or(|next| now >= next)
    }
    fn attempted(&mut self, now: Instant) {
        self.next = Some(now + Duration::from_millis(250u64 << self.attempts.min(4)));
        self.attempts += 1;
    }
}

pub(crate) fn validate_modules_path(modules: Option<&Path>) -> Result<()> {
    if let Some(path) = modules {
        ensure!(
            path.is_absolute()
                && path.file_name().is_some()
                && path.components().all(|component| matches!(component, std::path::Component::RootDir | std::path::Component::Normal(_)))
                && path.to_str().is_some_and(|text| !text.contains(':') && !text.chars().any(char::is_control)),
            "audio module directory must be a single absolute UTF-8 path without traversal, search-path separators or control characters"
        );
    }
    Ok(())
}

impl Audio {
    pub fn start(
        run: &Path,
        tools: &Path,
        modules: Option<&Path>,
        deadline: Instant,
        processing: Option<(
            crate::pcm::AudioControl,
            Box<dyn crate::pcm::AudioProcessor>,
        )>,
    ) -> Result<Self> {
        validate_modules_path(modules)?;
        let processed = processing.is_some();
        let listener = platform::OutputListener::new()?;
        let (object, frames) = platform::default_output()?;
        let state = run.join("pulse");
        private::directory(&state)?;
        let cookie = state.join("cookie");
        let mut random = [0; 256];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        private::write_private(&cookie, &random)?;
        let reservation = TcpListener::bind(("127.0.0.1", 0))?;
        let port = reservation.local_addr()?.port();
        let config = state.join("server.pa");
        private::write_private(
            &config,
            configuration(object, &cookie, port, frames, processed)?.as_bytes(),
        )?;
        let client = state.join("client.conf");
        private::write_private(
            &client,
            b"autospawn = no\nenable-shm = no\nenable-memfd = no\n",
        )?;
        let mut environment = BTreeMap::new();
        for (key, value) in [
            ("PATH", format!("{}/bin:/usr/bin:/bin", tools.display())),
            (
                "HOME",
                state.to_str().context("non-UTF8 audio path")?.to_owned(),
            ),
            ("LANG", "C".into()),
            (
                "PULSE_RUNTIME_PATH",
                state.to_str().context("non-UTF8 audio path")?.to_owned(),
            ),
            (
                "PULSE_STATE_PATH",
                state.to_str().context("non-UTF8 audio path")?.to_owned(),
            ),
            (
                "PULSE_CLIENTCONFIG",
                client.to_str().context("non-UTF8 client path")?.to_owned(),
            ),
            (
                "PULSE_COOKIE",
                cookie.to_str().context("non-UTF8 cookie path")?.to_owned(),
            ),
            ("PULSE_SERVER", format!("tcp:127.0.0.1:{port}")),
        ] {
            environment.insert(key.to_owned(), value);
        }
        if processed {
            environment.insert("PULSE_SINK".into(), "myx_input".into());
        }
        let mut command = Command::new(tools.join("bin/pulseaudio"));
        if let Some(modules) = modules {
            command.arg("--dl-search-path").arg(modules);
        }
        command
            .args([
                "--daemonize=no",
                "--use-pid-file=no",
                "--system=no",
                "--exit-idle-time=-1",
                "--disable-shm=yes",
                "--log-target=stderr",
                "--log-level=error",
                "-n",
                "-F",
            ])
            .arg(config)
            .env_clear()
            .envs(&environment)
            .current_dir(&state)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        drop(reservation);
        let mut child = OwnedChild::spawn(&mut command)?;
        loop {
            process::remaining(deadline, Duration::from_secs(2))?;
            ensure!(
                child.child.try_wait()?.is_none(),
                "owned native audio server exited"
            );
            let mut query = Command::new(tools.join("bin/pactl"));
            query
                .args(["--format=json", "list", "sinks"])
                .env_clear()
                .envs(&environment);
            let query_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
            if let Ok(bytes) = process::output(&mut query, query_deadline, 131072) {
                let sinks: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("invalid audio sink response"))?;
                let sinks = sinks.as_array().context("sink response is not an array")?;
                let outputs: Vec<_> = sinks
                    .iter()
                    .filter(|sink| sink["properties"]["device.api"] == "CoreAudio")
                    .collect();
                ensure!(
                    outputs.len() == 1 && sinks.len() == if processed { 2 } else { 1 },
                    "private server has no unique verified CoreAudio sink"
                );
                let bridge = if let Some((control, processor)) = processing {
                    ensure!(
                        sinks.iter().any(|sink| sink["name"] == "myx_input"
                            && sink["sample_specification"] == "float32le 2ch 44100Hz"),
                        "private PCM input sink missing or not float32le stereo 44100Hz"
                    );
                    let sink = outputs[0]["name"]
                        .as_str()
                        .context("CoreAudio output name missing")?;
                    Some(crate::pulse::Bridge::start(
                        crate::pulse::Endpoint {
                            tools,
                            server: &format!("unix:{}", state.join("native").display()),
                            cookie: &cookie,
                            sink,
                            source: "myx_input.monitor",
                        },
                        control,
                        processor,
                        deadline,
                    )?)
                } else {
                    None
                };
                return Ok(Self {
                    child,
                    environment,
                    bridge,
                    tools: tools.to_owned(),
                    output: Some((object, frames)),
                    output_module: Some(
                        outputs[0]["owner_module"]
                            .as_u64()
                            .context("CoreAudio module ID missing")?,
                    ),
                    listener,
                    recovery: Recovery::default(),
                });
            }
            thread::sleep(process::remaining(deadline, Duration::from_millis(50))?);
        }
    }
    pub fn running(&mut self) -> bool {
        let changed = self.listener.take_changed();
        let failed = self.bridge.as_ref().is_some_and(|bridge| !bridge.running());
        if changed {
            self.recovery.reset();
            self.output = None;
            if let Some(bridge) = &self.bridge {
                bridge.retarget(None);
            }
        }
        if self.bridge.is_some()
            && (changed || failed || self.output.is_none())
            && self.recovery.due(Instant::now())
        {
            self.recovery.attempted(Instant::now());
            if let Some(bridge) = &self.bridge {
                bridge.output_recovery();
            }
            // Recovery never restarts the private server/SDK or changes its input.
            // The existing processor stays owned by the same bridge worker.
            if self.reload_output().is_err() {
                self.output = None;
                if let Some(bridge) = &self.bridge {
                    bridge.retarget(None);
                }
            }
        }
        self.bridge.as_ref().is_none_or(|bridge| bridge.alive())
    }
    fn pactl(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut query = Command::new(self.tools.join("bin/pactl"));
        query.args(args).env_clear().envs(&self.environment);
        process::output(
            &mut query,
            Instant::now() + Duration::from_millis(500),
            131072,
        )
    }
    fn reload_output(&mut self) -> Result<()> {
        let (object, frames) = platform::default_output()?;
        if let Some(bridge) = &self.bridge {
            bridge.retarget(None);
        }
        if let Some(module) = self.output_module {
            // Keep the ID until unload succeeds, so retries cannot leak modules.
            let modules: Value =
                serde_json::from_slice(&self.pactl(&["--format=json", "list", "modules"])?)?;
            if modules
                .as_array()
                .context("invalid module list")?
                .iter()
                .any(|entry| entry["index"].as_u64() == Some(module))
            {
                self.pactl(&["unload-module", &module.to_string()])?;
            }
            self.output_module = None;
        }
        ensure!((1..=65536).contains(&frames), "invalid output buffer size");
        let bytes = self.pactl(&[
            "load-module",
            "module-coreaudio-device",
            &format!("object_id={object}"),
            "record=no",
            "playback=yes",
            &format!("ioproc_frames={frames}"),
        ])?;
        self.output_module = Some(std::str::from_utf8(&bytes)?.trim().parse()?);
        let sinks: Value =
            serde_json::from_slice(&self.pactl(&["--format=json", "list", "sinks"])?)?;
        let outputs: Vec<_> = sinks
            .as_array()
            .context("sink response is not an array")?
            .iter()
            .filter(|sink| {
                sink["properties"]["device.api"] == "CoreAudio"
                    && sink["owner_module"].as_u64() == self.output_module
            })
            .collect();
        ensure!(outputs.len() == 1, "reloaded CoreAudio sink is not unique");
        let sink = outputs[0]["name"]
            .as_str()
            .context("reloaded sink has no name")?;
        self.output = Some((object, frames));
        if let Some(bridge) = &self.bridge {
            bridge.retarget(Some(sink.to_owned()));
        }
        self.recovery.next = Some(Instant::now() + Duration::from_secs(1));
        Ok(())
    }
    pub fn stop(&mut self) -> Result<()> {
        let bridge = self.bridge.as_mut().map_or(Ok(()), |bridge| bridge.stop());
        let child = self.child.stop();
        bridge.and(child)
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn output_is_only_current_device_and_cookie_authenticated_loopback() {
        let text = configuration(42, Path::new("/private/cookie"), 12345, 512, false).unwrap();
        assert!(text.contains("object_id=42 record=no playback=yes ioproc_frames=512"));
        assert!(text.contains("listen=127.0.0.1 port=12345 auth-anonymous=no"));
        for frames in [0, 65537] {
            assert!(configuration(42, Path::new("/cookie"), 12345, frames, false).is_err());
        }
        assert!(configuration(42, Path::new("/bad\npath"), 12345, 512, false).is_err());
        assert!(configuration(0, Path::new("/cookie"), 12345, 512, false).is_err());
        let processed = configuration(42, Path::new("/cookie"), 12345, 512, true).unwrap();
        assert!(processed.contains("set-default-sink myx_input"));
        assert!(processed.contains("record=no playback=yes"));
        assert!(configuration(42, Path::new("/cookie"), 0, 512, false).is_err());
    }

    #[test]
    #[ignore = "requires MYX_AUDIO_TOOLS and current CoreAudio output; sends digital silence only"]
    pub(crate) fn managed_pcm_opens_current_output_and_cleans_up_without_an_sdk() {
        struct Silence;
        impl crate::pcm::AudioProcessor for Silence {
            fn process(&mut self, samples: &mut [f32]) {
                samples.fill(0.0);
            }
            fn reset(&mut self) {}
        }
        let tools = crate::native_audio::owner_tools();
        let temp = tempfile::tempdir().unwrap();
        let control = crate::pcm::AudioControl::default();
        let mut audio = Audio::start(
            temp.path(),
            &tools,
            None,
            Instant::now() + Duration::from_secs(15),
            Some((control.clone(), Box::new(Silence))),
        )
        .unwrap();
        assert!(control.is_ready());
        control.set_active(true);
        thread::sleep(Duration::from_millis(150));
        assert!(audio.running(), "native PCM output worker failed");
        assert!(
            control.stats().processed_frames > 0,
            "physical output route never processed PCM"
        );
        println!("CoreAudio private PCM: {:?}", control.stats());
        control.set_active(false);
        audio.stop().unwrap();
        assert!(!control.is_ready());
        assert!(audio.child.child.try_wait().unwrap().is_some());
    }

    #[test]
    pub(crate) fn recovery_is_bounded_and_notification_rearms() {
        let mut recovery = Recovery::default();
        let mut now = Instant::now();
        for milliseconds in [250, 500, 1000, 2000, 4000] {
            assert!(recovery.due(now));
            recovery.attempted(now);
            assert!(!recovery.due(now + Duration::from_millis(milliseconds - 1)));
            now += Duration::from_millis(milliseconds);
        }
        assert!(!recovery.due(now + Duration::from_secs(3600)));
        recovery.reset();
        assert!(recovery.due(now));
    }

    pub(crate) fn owned_output_module_loss_recovers() {
        struct Silence;
        impl crate::pcm::AudioProcessor for Silence {
            fn process(&mut self, samples: &mut [f32]) {
                samples.fill(0.0);
            }
            fn reset(&mut self) {}
        }
        let tools = crate::native_audio::owner_tools();
        let temp = tempfile::tempdir().unwrap();
        let control = crate::pcm::AudioControl::default();
        let mut audio = Audio::start(
            temp.path(),
            &tools,
            None,
            Instant::now() + Duration::from_secs(15),
            Some((control.clone(), Box::new(Silence))),
        )
        .unwrap();
        let pid = audio.child.child.id();
        control.set_active(true);
        let module = audio.output_module.unwrap();
        audio
            .pactl(&["unload-module", &module.to_string()])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut saw_unavailable = false;
        while Instant::now() < deadline {
            saw_unavailable |= !control.is_ready();
            assert!(
                audio.running(),
                "private worker exited during output-only loss"
            );
            assert_eq!(audio.child.child.id(), pid, "private server was replaced");
            assert!(audio.child.child.try_wait().unwrap().is_none());
            if control.stats().recovery_attempts > 0 && control.is_ready() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_unavailable, "output loss was not surfaced");
        assert!(
            control.is_ready(),
            "owned output reload exhausted: {:?}",
            control.stats()
        );
        assert_ne!(audio.output_module, Some(module));
        audio.stop().unwrap();
        assert!(!control.is_ready());
        assert!(audio.child.child.try_wait().unwrap().is_some());
    }
}
