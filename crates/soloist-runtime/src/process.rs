// Adapted from tools/soloist-elfuse under the project's MIT license.
use anyhow::{bail, ensure, Context, Result};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Only a child spawned here may be signalled. No persisted PID is accepted.
pub struct OwnedChild {
    pub child: Child,
    group: i32,
}

impl OwnedChild {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        command.stdin(Stdio::null());
        // SAFETY: the post-fork closure calls only async-signal-safe syscalls;
        // no allocations, locks, environment inspection or Rust destructors.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::setrlimit(libc::RLIMIT_CORE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Never format Command: SDK argv contains the provisioned key.
        let child = command.spawn().context("cannot spawn owned child")?;
        let group = child.id() as i32;
        Ok(Self { child, group })
    }

    pub fn stop(&mut self) -> Result<()> {
        if self.group == 0 {
            return Ok(());
        }
        // SAFETY: this process group was created by this owner, never attached
        // using a status-file PID. Kill the group even if its leader exited.
        signal(self.group, libc::SIGTERM)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.child.try_wait()?.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        // Descendants can outlive an exited group leader.
        signal(self.group, libc::SIGKILL)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.child.try_wait()?.is_none() {
            ensure!(
                Instant::now() < deadline,
                "owned child did not exit after SIGKILL"
            );
            thread::sleep(Duration::from_millis(20));
        }
        self.group = 0;
        Ok(())
    }
}

fn signal(group: i32, signal: i32) -> Result<()> {
    // SAFETY: only the stored, locally created process group is signalled.
    if unsafe { libc::kill(-group, signal) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        bail!("cannot signal owned process group");
    }
    Ok(())
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub fn remaining(deadline: Instant, cap: Duration) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now()).min(cap);
    ensure!(
        !remaining.is_zero(),
        "runtime readiness exceeded 25 seconds"
    );
    Ok(remaining)
}

/// Bounded native utilities; stdout is consumed in memory and never logged.
pub fn output(command: &mut Command, deadline: Instant, limit: usize) -> Result<Vec<u8>> {
    remaining(deadline, Duration::from_secs(3))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = OwnedChild::spawn(command)?;
    let stdout = child.child.stdout.take().context("child stdout missing")?;
    let reader = thread::Builder::new()
        .name("soloist-utility".into())
        .spawn(move || {
            let mut data = Vec::new();
            stdout
                .take(limit as u64 + 1)
                .read_to_end(&mut data)
                .map(|_| data)
        })
        .context("cannot start native output reader")?;
    let status = loop {
        if let Some(status) = child.child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.stop()?;
            let _ = reader.join();
            bail!("native utility timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    child.stop()?;
    let data = reader
        .join()
        .map_err(|_| anyhow::anyhow!("native output reader failed"))??;
    ensure!(
        status.success() && data.len() <= limit,
        "native utility failed or output exceeded limit"
    );
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utility_output_and_wait_are_bounded() {
        assert_eq!(
            output(
                Command::new("/usr/bin/printf").arg("fixture"),
                Instant::now() + Duration::from_secs(2),
                32
            )
            .unwrap(),
            b"fixture"
        );
        assert!(output(
            Command::new("/usr/bin/printf").arg("oversized"),
            Instant::now() + Duration::from_secs(2),
            2
        )
        .is_err());
        let began = Instant::now();
        assert!(output(
            Command::new("/bin/sleep").arg("30"),
            Instant::now() + Duration::from_millis(50),
            32
        )
        .is_err());
        assert!(began.elapsed() < Duration::from_secs(6));
    }

    #[test]
    fn stopping_child_twice_is_safe() {
        let mut child = OwnedChild::spawn(
            Command::new("/bin/sleep")
                .arg("30")
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .unwrap();
        child.stop().unwrap();
        child.stop().unwrap();
        assert!(child.child.try_wait().unwrap().is_some());
    }
}
