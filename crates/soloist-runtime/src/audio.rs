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
use std::path::Path;
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
}

impl Audio {
    pub fn start(
        run: &Path,
        tools: &Path,
        deadline: Instant,
        processing: Option<(
            crate::pcm::AudioControl,
            Box<dyn crate::pcm::AudioProcessor>,
        )>,
    ) -> Result<Self> {
        let processed = processing.is_some();
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
                        sinks.iter().any(|sink| sink["name"] == "myx_input"),
                        "private PCM input sink missing"
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
                });
            }
            thread::sleep(process::remaining(deadline, Duration::from_millis(50))?);
        }
    }
    pub fn running(&self) -> bool {
        self.bridge.as_ref().is_none_or(|bridge| bridge.running())
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
mod tests {
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
    fn managed_pcm_opens_current_output_and_cleans_up_without_an_sdk() {
        struct Silence;
        impl crate::pcm::AudioProcessor for Silence {
            fn process(&mut self, samples: &mut [f32]) {
                samples.fill(0.0);
            }
            fn reset(&mut self) {}
        }
        let tools = std::path::PathBuf::from(
            std::env::var_os("MYX_AUDIO_TOOLS").expect("MYX_AUDIO_TOOLS required"),
        );
        let temp = tempfile::tempdir().unwrap();
        let control = crate::pcm::AudioControl::default();
        let mut audio = Audio::start(
            temp.path(),
            &tools,
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
}
