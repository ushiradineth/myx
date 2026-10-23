use super::ServiceStatus;
use anyhow::{bail, ensure, Context, Result};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const LABEL: &str = "io.myx.daemon";
const PLIST: &str = "io.myx.daemon.plist";
const MAX_PLIST: u64 = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
// launchctl's service-lookup error is distinct from the POSIX bootout ESRCH.
const SERVICE_NOT_FOUND: i32 = 113;

struct Layout {
    home: PathBuf,
    uid: u32,
}

impl Layout {
    fn current() -> Result<Self> {
        // A GUI LaunchAgent must never be registered on behalf of root or another UID.
        let uid = unsafe { libc::getuid() };
        ensure!(
            uid != 0 && uid == unsafe { libc::geteuid() },
            "service requires a non-root GUI user"
        );
        let home = std::env::var_os("HOME").context("HOME is required for the user service")?;
        let home = PathBuf::from(home);
        validate_absolute(&home)?;
        Ok(Self { home, uid })
    }

    fn logs(&self) -> PathBuf {
        self.home.join("Library/Logs/io.myx.daemon")
    }

    fn registration_path(&self) -> PathBuf {
        self.home.join("Library/LaunchAgents").join(PLIST)
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self) -> String {
        format!("{}/{LABEL}", self.domain())
    }

    fn agents(&self, create: bool) -> Result<Option<Directory>> {
        let home = Directory::absolute(&self.home, self.uid)?;
        let Some(library) = home.child("Library", create, false)? else {
            return Ok(None);
        };
        library.child("LaunchAgents", create, false)
    }
}

/// Directory-relative operations keep validation and use on the same directory inode.
struct Directory {
    file: File,
    uid: u32,
}

impl Directory {
    fn absolute(path: &Path, uid: u32) -> Result<Self> {
        validate_absolute(path)?;
        let mut file = File::open("/")?;
        for component in path.components() {
            if let Component::Normal(name) = component {
                file = open_at(
                    &file,
                    name.as_bytes(),
                    libc::O_RDONLY | libc::O_DIRECTORY,
                    0,
                )?;
                let metadata = file.metadata()?;
                ensure!(metadata.is_dir(), "service path is not a directory");
                // Ancestors may be system-owned, but may not be writable by other users.
                let root_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
                ensure!(
                    metadata.mode() & 0o022 == 0 || root_sticky,
                    "service path has a writable ancestor"
                );
                ensure!(
                    metadata.uid() == 0 || metadata.uid() == uid,
                    "service path has a foreign ancestor"
                );
            }
        }
        ensure!(
            file.metadata()?.uid() == uid,
            "service directory is not owned by this user"
        );
        Ok(Self { file, uid })
    }

    fn child(&self, name: &str, create: bool, private: bool) -> Result<Option<Self>> {
        let mut opened = open_at(
            &self.file,
            name.as_bytes(),
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        );
        if is_missing(&opened) && create {
            let name_c = CString::new(name)?;
            let result = unsafe { libc::mkdirat(self.file.as_raw_fd(), name_c.as_ptr(), 0o700) };
            if result != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(std::io::Error::last_os_error().into());
            }
            opened = open_at(
                &self.file,
                name.as_bytes(),
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            );
        }
        if is_missing(&opened) {
            return Ok(None);
        }
        let file = opened.context("cannot open service directory safely")?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == self.uid && metadata.mode() & 0o022 == 0,
            "unsafe service directory ownership or permissions"
        );
        if private {
            ensure!(
                metadata.mode() & 0o777 == 0o700,
                "service log directory must be private (0700)"
            );
        }
        Ok(Some(Self {
            file,
            uid: self.uid,
        }))
    }

    fn read_registration(&self, layout: &Layout) -> Result<Option<(File, String)>> {
        let opened = open_at(
            &self.file,
            PLIST.as_bytes(),
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        );
        if is_missing(&opened) {
            return Ok(None);
        }
        let mut file = opened.context("cannot open service registration safely")?;
        validate_owned_file(&file, self.uid)?;
        ensure!(
            file.metadata()?.len() <= MAX_PLIST,
            "service registration is too large"
        );
        let mut text = String::new();
        Read::by_ref(&mut file)
            .take(MAX_PLIST + 1)
            .read_to_string(&mut text)?;
        ensure!(
            text.len() as u64 <= MAX_PLIST,
            "service registration is too large"
        );
        ensure!(
            recognized(&text, layout),
            "refusing foreign or modified service registration"
        );
        Ok(Some((file, text)))
    }

    fn same_registration(&self, expected: &File) -> Result<()> {
        let current = open_at(
            &self.file,
            PLIST.as_bytes(),
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        )?;
        validate_owned_file(&current, self.uid)?;
        let a = expected.metadata()?;
        let b = current.metadata()?;
        ensure!(
            a.dev() == b.dev() && a.ino() == b.ino(),
            "service registration changed during operation"
        );
        Ok(())
    }
}

fn is_missing<T>(result: &std::io::Result<T>) -> bool {
    matches!(result, Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn open_at(parent: &File, name: &[u8], flags: i32, mode: libc::mode_t) -> std::io::Result<File> {
    let name = CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn validate_owned_file(file: &File, uid: u32) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == uid,
        "service file must be an owned regular file without hardlinks"
    );
    ensure!(
        metadata.mode() & 0o777 == 0o600,
        "service file must be private (0600)"
    );
    Ok(())
}

fn validate_absolute(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "service paths must be absolute");
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "service paths must not contain parent traversal"
    );
    let value = path.to_str().context("service paths must be valid UTF-8")?;
    ensure!(
        !value.chars().any(|c| c.is_control()),
        "service paths must not contain control characters"
    );
    Ok(())
}

fn validate_input(path: &Path, executable: bool, uid: u32) -> Result<()> {
    validate_absolute(path)?;
    if executable {
        ensure!(!path.components().any(|c| matches!(c, Component::Normal(name) if name == ".git" || name == "target" || name == ".cache")), "install a stable executable outside developer build/cache directories first");
        for parent in path.ancestors().skip(1) {
            match fs::symlink_metadata(parent.join("Cargo.toml")) {
                Ok(_) => bail!("install a stable executable outside the Cargo checkout first"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => {
                    return Err(anyhow::Error::new(error)
                        .context("cannot inspect executable checkout ancestry"))
                }
            }
        }
    }
    let parent = path
        .parent()
        .context("service input needs a parent directory")?;
    // Config files may live in normal private directories; neither input is read here.
    let directory = Directory::absolute(parent, uid)?;
    let file = open_at(
        &directory.file,
        path.file_name()
            .context("service input needs a filename")?
            .as_bytes(),
        libc::O_RDONLY | libc::O_NONBLOCK,
        0,
    )?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == uid,
        "service input must be an owned regular file without symlinks or hardlinks"
    );
    ensure!(
        metadata.mode() & 0o022 == 0,
        "service input is writable by other users"
    );
    if executable {
        ensure!(
            metadata.mode() & 0o100 != 0,
            "service executable is not executable by its owner"
        );
    }
    Ok(())
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn render(executable: &Path, config: &Path, logs: &Path) -> Result<String> {
    for path in [executable, config, logs] {
        validate_absolute(path)?;
    }
    let executable = xml(executable.to_str().context("invalid executable path")?);
    let config = xml(config.to_str().context("invalid config path")?);
    let stdout = xml(logs
        .join("stdout.log")
        .to_str()
        .context("invalid log path")?);
    let stderr = xml(logs
        .join("stderr.log")
        .to_str()
        .context("invalid log path")?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- myx managed LaunchAgent v1 -->
<plist version="1.0">
<dict>
<key>Label</key>
<string>{LABEL}</string>
<key>ProgramArguments</key>
<array>
<string>{executable}</string>
<string>daemon</string>
<string>run</string>
<string>--config</string>
<string>{config}</string>
</array>
<key>EnvironmentVariables</key>
<dict>
<key>MYX_SUPERVISED</key>
<string>1</string>
</dict>
<key>RunAtLoad</key>
<true/>
<key>KeepAlive</key>
<dict>
<key>SuccessfulExit</key>
<false/>
</dict>
<key>ThrottleInterval</key>
<integer>30</integer>
<key>StandardOutPath</key>
<string>{stdout}</string>
<key>StandardErrorPath</key>
<string>{stderr}</string>
<key>Umask</key>
<integer>63</integer>
</dict>
</plist>
"#
    ))
}

fn unxml(value: &str) -> String {
    value
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

fn recognized(text: &str, layout: &Layout) -> bool {
    let strings: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("<string>")?.strip_suffix("</string>"))
        .collect();
    if strings.len() != 9 {
        return false;
    }
    // Exact canonical reconstruction rejects foreign keys, arguments and modified semantics.
    let executable = PathBuf::from(unxml(strings[1]));
    let config = PathBuf::from(unxml(strings[5]));
    render(&executable, &config, &layout.logs()).is_ok_and(|expected| expected == text)
}

fn prepare_logs(layout: &Layout) -> Result<()> {
    let home = Directory::absolute(&layout.home, layout.uid)?;
    let library = home
        .child("Library", true, false)?
        .context("missing Library")?;
    let logs = library
        .child("Logs", true, false)?
        .context("missing Logs")?;
    let private = logs
        .child(LABEL, true, true)?
        .context("missing private log directory")?;
    for name in ["stdout.log", "stderr.log"] {
        let file = match open_at(
            &private.file,
            name.as_bytes(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NONBLOCK,
            0o600,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => open_at(
                &private.file,
                name.as_bytes(),
                libc::O_WRONLY | libc::O_NONBLOCK,
                0,
            )?,
            Err(error) => return Err(error.into()),
        };
        validate_owned_file(&file, layout.uid)?;
    }
    Ok(())
}

/// Never capture launchctl's output: print diagnostics can include environment secrets.
fn launchctl(args: &[&str]) -> Result<ExitStatus> {
    let mut child = Command::new("/bin/launchctl")
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot spawn launchctl")?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("launchctl timed out after five seconds");
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::Error::new(error).context("cannot wait for launchctl"));
            }
        }
    }
}

fn loaded(layout: &Layout) -> Result<bool> {
    let result = launchctl(&["print", &layout.target()])?;
    if result.success() {
        return Ok(true);
    }
    ensure!(
        matches!(result.code(), Some(libc::ESRCH) | Some(SERVICE_NOT_FOUND)),
        "cannot query user service (launchctl exit {:?})",
        result.code()
    );
    Ok(false)
}

pub fn status() -> Result<ServiceStatus> {
    let layout = Layout::current()?;
    let installed = match layout.agents(false)? {
        Some(agents) => agents.read_registration(&layout)?.is_some(),
        None => false,
    };
    Ok(ServiceStatus {
        installed,
        loaded: loaded(&layout)?,
    })
}

pub fn install(executable: &Path, config: &Path) -> Result<ServiceStatus> {
    let layout = Layout::current()?;
    validate_input(executable, true, layout.uid)?;
    validate_input(config, false, layout.uid)?;
    let agents = layout.agents(true)?.context("missing LaunchAgents")?;
    let text = render(executable, config, &layout.logs())?;
    if let Some((_, existing)) = agents.read_registration(&layout)? {
        ensure!(
            existing == text,
            "service is already registered with different paths; explicitly uninstall it first"
        );
        prepare_logs(&layout)?;
        return Ok(ServiceStatus {
            installed: true,
            loaded: loaded(&layout)?,
        });
    }
    prepare_logs(&layout)?;
    // O_EXCL refuses even a concurrent/symlink/foreign destination. Publish by hard link
    // only after the complete private file has been written and synced.
    let temporary = format!(".{PLIST}.{}.tmp", std::process::id());
    let mut file = open_at(
        &agents.file,
        temporary.as_bytes(),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    let result = (|| -> Result<()> {
        validate_owned_file(&file, layout.uid)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        let source = CString::new(temporary.as_str())?;
        let destination = CString::new(PLIST)?;
        let result = unsafe {
            libc::linkat(
                agents.file.as_raw_fd(),
                source.as_ptr(),
                agents.file.as_raw_fd(),
                destination.as_ptr(),
                0,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    })();
    unlink(&agents, &temporary)?;
    result?;
    agents.file.sync_all()?;
    Ok(ServiceStatus {
        installed: true,
        loaded: loaded(&layout)?,
    })
}

fn unlink(directory: &Directory, name: &str) -> Result<()> {
    let name = CString::new(name)?;
    let result = unsafe { libc::unlinkat(directory.file.as_raw_fd(), name.as_ptr(), 0) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn bootout(layout: &Layout) -> Result<()> {
    if !loaded(layout)? {
        return Ok(());
    }
    let result = launchctl(&["bootout", &layout.target()])?;
    ensure!(
        result.success() || !loaded(layout)?,
        "cannot stop user service (launchctl exit {:?})",
        result.code()
    );
    Ok(())
}

pub fn start() -> Result<bool> {
    let layout = Layout::current()?;
    let Some(agents) = layout.agents(false)? else {
        return Ok(false);
    };
    let Some((file, text)) = agents.read_registration(&layout)? else {
        return Ok(false);
    };
    if loaded(&layout)? {
        return Ok(true);
    }
    let strings: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("<string>")?.strip_suffix("</string>"))
        .collect();
    validate_input(Path::new(&unxml(strings[1])), true, layout.uid)?;
    validate_input(Path::new(&unxml(strings[5])), false, layout.uid)?;
    prepare_logs(&layout)?;
    agents.same_registration(&file)?;
    let path = layout.registration_path();
    let result = launchctl(&[
        "bootstrap",
        &layout.domain(),
        path.to_str().context("invalid registration path")?,
    ])?;
    // Concurrent callers can bootstrap the same label; confirm it actually became loaded.
    ensure!(
        loaded(&layout)?,
        "cannot start user service (launchctl exit {:?})",
        result.code()
    );
    Ok(true)
}

pub fn stop() -> Result<bool> {
    let layout = Layout::current()?;
    let Some(agents) = layout.agents(false)? else {
        return Ok(false);
    };
    let Some((file, _)) = agents.read_registration(&layout)? else {
        return Ok(false);
    };
    agents.same_registration(&file)?;
    // Bootout removes activation, not the registration. A clean stop stays stopped.
    bootout(&layout)?;
    Ok(true)
}

pub fn uninstall() -> Result<()> {
    let layout = Layout::current()?;
    let Some(agents) = layout.agents(false)? else {
        return Ok(());
    };
    let Some((file, _)) = agents.read_registration(&layout)? else {
        return Ok(());
    };
    agents.same_registration(&file)?;
    bootout(&layout)?;
    agents.same_registration(&file)?;
    unlink(&agents, PLIST)?;
    agents.file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture() -> (tempfile::TempDir, Layout) {
        let root = tempfile::tempdir().unwrap();
        // macOS's /var alias is a symlink; use the physical fixture path.
        let home = root.path().canonicalize().unwrap();
        let layout = Layout {
            home,
            uid: unsafe { libc::getuid() },
        };
        (root, layout)
    }

    #[test]
    fn xml_escaping_and_exact_recognition() {
        let (_root, layout) = fixture();
        let text = render(
            Path::new("/Applications/My & <Player>/myx"),
            Path::new("/Users/test/a'\"&.toml"),
            &layout.logs(),
        )
        .unwrap();
        assert!(text.contains("My &amp; &lt;Player&gt;"));
        assert!(text.contains("a&apos;&quot;&amp;.toml"));
        assert!(recognized(&text, &layout));
        assert!(!recognized(
            &text.replace("<integer>30</integer>", "<integer>0</integer>"),
            &layout
        ));
        assert!(!recognized(
            &text.replace("<string>run</string>", "<string>stop</string>"),
            &layout
        ));
    }

    #[test]
    fn absolute_path_rules() {
        for path in ["relative", "/tmp/../myx", "/tmp/line\nbreak", "/tmp/\0nul"] {
            assert!(validate_absolute(Path::new(path)).is_err());
        }
        assert!(validate_absolute(Path::new("/Applications/Myx Player/myx")).is_ok());
    }

    #[test]
    fn stable_input_rejects_checkout_cache_symlink_and_hardlink() {
        let (_root, layout) = fixture();
        let binary = layout.home.join("myx");
        fs::write(&binary, b"fixture").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(validate_input(&binary, true, layout.uid).is_ok());
        let link = layout.home.join("alias");
        symlink(&binary, &link).unwrap();
        assert!(validate_input(&link, true, layout.uid).is_err());
        fs::remove_file(&link).unwrap();
        fs::hard_link(&binary, &link).unwrap();
        assert!(validate_input(&binary, true, layout.uid).is_err());
        fs::remove_file(&link).unwrap();
        fs::write(layout.home.join("Cargo.toml"), b"fixture").unwrap();
        assert!(validate_input(&binary, true, layout.uid).is_err());
        for component in [".git", "target", ".cache"] {
            assert!(
                validate_input(&layout.home.join(component).join("myx"), true, layout.uid).is_err()
            );
        }
    }

    #[test]
    fn registration_fixtures_reject_foreign_symlink_and_hardlink() {
        let (_root, layout) = fixture();
        let agents = layout.agents(true).unwrap().unwrap();
        assert!(agents.read_registration(&layout).unwrap().is_none());
        let path = layout.registration_path();
        fs::write(&path, b"unrelated").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(agents.read_registration(&layout).is_err());
        let text = render(
            Path::new("/Applications/myx"),
            Path::new("/Users/test/config.toml"),
            &layout.logs(),
        )
        .unwrap();
        fs::write(&path, text).unwrap();
        let (file, _) = agents.read_registration(&layout).unwrap().unwrap();
        agents.same_registration(&file).unwrap();
        let alias = layout.home.join("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(agents.read_registration(&layout).is_err());
        fs::remove_file(&alias).unwrap();
        fs::rename(&path, &alias).unwrap();
        symlink(&alias, &path).unwrap();
        assert!(agents.read_registration(&layout).is_err());
    }

    #[test]
    fn log_setup_is_idempotent_and_preserves_shared_directory_modes() {
        let (_root, layout) = fixture();
        fs::create_dir(layout.home.join("Library")).unwrap();
        fs::set_permissions(
            layout.home.join("Library"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        prepare_logs(&layout).unwrap();
        prepare_logs(&layout).unwrap();
        assert_eq!(
            fs::metadata(layout.home.join("Library")).unwrap().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::metadata(layout.logs()).unwrap().mode() & 0o777, 0o700);
        for name in ["stdout.log", "stderr.log"] {
            assert_eq!(
                fs::metadata(layout.logs().join(name)).unwrap().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn unsafe_directory_and_file_permissions_are_rejected_without_repair() {
        let (_root, layout) = fixture();
        let agents = layout.agents(true).unwrap().unwrap();
        let path = layout.registration_path();
        let text = render(
            Path::new("/Applications/myx"),
            Path::new("/Users/test/config.toml"),
            &layout.logs(),
        )
        .unwrap();
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(agents.read_registration(&layout).is_err());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        prepare_logs(&layout).unwrap();
        fs::set_permissions(layout.logs(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_logs(&layout).is_err());
        assert_eq!(fs::metadata(layout.logs()).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn foreign_owner_and_symlinked_home_are_rejected() {
        let (_root, layout) = fixture();
        prepare_logs(&layout).unwrap();
        let file = File::open(layout.logs().join("stdout.log")).unwrap();
        assert!(validate_owned_file(&file, layout.uid.wrapping_add(1)).is_err());
        let alias = layout.home.join("home-alias");
        symlink(&layout.home, &alias).unwrap();
        assert!(Directory::absolute(&alias, layout.uid).is_err());
    }
}
