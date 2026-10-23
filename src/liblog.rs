//! Optional bounded application diagnostics. Provider HTTP/token logs are not forwarded.
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::Mutex;

static LOG_LOCK: Mutex<()> = Mutex::new(());
struct AppLog;
impl log::Log for AppLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target().starts_with("myx")
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            liblog(format!("{}: {}", record.level(), record.args()));
        }
    }
    fn flush(&self) {}
}

pub fn install_logger() {
    if std::env::var_os("MYX_LOG").is_some() && log::set_boxed_logger(Box::new(AppLog)).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

pub fn liblog(message: impl AsRef<str>) {
    if std::env::var_os("MYX_LOG").is_none() {
        return;
    }
    let Ok(_guard) = LOG_LOCK.try_lock() else {
        return;
    };
    let Some(home) = crate::home_dir() else {
        return;
    };
    let directory = home.join(".cache/myx");
    if crate::ipc::unix::private_directory(&directory).is_err() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(directory.join("myx.log"))
    else {
        return;
    };
    let Ok(metadata) = file.metadata() else {
        return;
    };
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
        || metadata.len() >= 2 * 1024 * 1024
    {
        return;
    }
    let mut line = message.as_ref().chars().take(1024).collect::<String>();
    line.retain(|character| !character.is_control() || character == '\t');
    let _ = writeln!(file, "{} {line}", crate::domain::playback::now_ms());
}
