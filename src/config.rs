//! Private user configuration. Loading never writes a template or migrates a
//! live file. Errors intentionally omit paths, TOML excerpts and secret values.

use anyhow::{bail, ensure, Result};
use serde::Deserialize;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const INVALID: &str = "myx configuration is invalid; check the configuration file";
const REMOVED: &str = "myx configuration contains removed settings; stage a migration before use";
const UNSAFE: &str = "myx configuration requires a same-user, mode 0600 regular nonsymlink file";
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const OBSOLETE_ROOT: &[&str] = &["backend", "api", "bitrate", "normalize_volume"];
const OBSOLETE_SOLOIST: &[&str] = &["endpoint", "dsp"];

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct SoloistConfig {
    pub directory: Option<PathBuf>,
    pub sdk: Option<PathBuf>,
    pub runner: Option<PathBuf>,
    pub sysroot: Option<PathBuf>,
    pub library_manifest: Option<PathBuf>,
    pub library_path: Option<String>,
    pub audio_tools: Option<PathBuf>,
    pub api_key: Option<String>,
    pub api_key_file: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub expected_sdk_sha256: Option<String>,
    pub device_name: String,
    pub initial_volume: u8,
}

impl Default for SoloistConfig {
    fn default() -> Self {
        Self {
            directory: None,
            sdk: None,
            runner: None,
            sysroot: None,
            library_manifest: None,
            library_path: None,
            audio_tools: None,
            api_key: None,
            api_key_file: None,
            data_dir: None,
            cache_dir: None,
            expected_sdk_sha256: None,
            device_name: "myx".into(),
            initial_volume: 25,
        }
    }
}

impl SoloistConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.api_key.is_none() || self.api_key_file.is_none(),
            "configure only one of soloist.api_key and soloist.api_key_file"
        );
        // DNS-SD's instance label limit is bytes, not Unicode scalar values.
        // Keep the actual friendly name intact, including spaces and Unicode.
        ensure!(
            !self.device_name.is_empty()
                && self.device_name.len() <= 63
                && !self.device_name.chars().any(char::is_control),
            "soloist.device_name must contain 1 to 63 UTF-8 bytes without control characters"
        );
        ensure!(
            self.initial_volume <= 100,
            "soloist.initial_volume must be from 0 to 100"
        );
        for path in [
            &self.directory,
            &self.sdk,
            &self.runner,
            &self.sysroot,
            &self.library_manifest,
            &self.audio_tools,
            &self.api_key_file,
            &self.data_dir,
            &self.cache_dir,
        ]
        .into_iter()
        .flatten()
        {
            absolute_soloist_path(path)?;
        }
        if let Some(paths) = &self.library_path {
            ensure!(
                !paths.is_empty()
                    && paths
                        .split(':')
                        .all(|p| p.starts_with('/') && !p.chars().any(char::is_control)),
                "Soloist library_path must contain absolute nonempty paths"
            );
        }
        if let Some(hash) = &self.expected_sdk_sha256 {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "Soloist SDK checksum must contain 64 hexadecimal digits"
            );
        }
        Ok(())
    }

    /// Resolve paths lexically without reading or creating runtime data.
    pub fn directory(&self) -> Result<PathBuf> {
        let path = match &self.directory {
            Some(path) => path.clone(),
            None => crate::home_dir()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Soloist requires a home directory or an absolute directory setting"
                    )
                })?
                .join(".config/myx/soloist"),
        };
        absolute_soloist_path(&path)
    }

    pub fn resolve(&self, value: Option<&PathBuf>, fallback: &str) -> Result<PathBuf> {
        match value {
            Some(path) => absolute_soloist_path(path),
            None => absolute_soloist_path(&self.directory()?.join(fallback)),
        }
    }
}

fn absolute_soloist_path(path: &Path) -> Result<PathBuf> {
    let expanded = if let Ok(rest) = path.strip_prefix("~") {
        crate::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot expand Soloist path without a home directory"))?
            .join(rest)
    } else {
        path.to_path_buf()
    };
    ensure!(
        expanded.is_absolute(),
        "Soloist paths must be absolute (or start with ~/)"
    );
    Ok(expanded)
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct NowPlayingConfig {
    pub enabled: bool,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(skip)]
    load_error: Option<&'static str>,
    pub soloist: SoloistConfig,
    pub scrolloff: usize,
    pub restore_on_startup: bool,
    pub client_id: Option<String>,
    pub protocol: Option<String>,
    pub transparent: bool,
    pub now_playing: NowPlayingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            load_error: None,
            soloist: SoloistConfig::default(),
            scrolloff: 3,
            restore_on_startup: true,
            client_id: None,
            protocol: None,
            transparent: false,
            now_playing: NowPlayingConfig::default(),
        }
    }
}

// Outer None means unselected; inner None means no default home is available.
static SELECTED_PATH: Mutex<Option<Option<PathBuf>>> = Mutex::new(None);

fn default_path() -> Option<PathBuf> {
    Some(crate::home_dir()?.join(".config/myx/config.toml"))
}

fn select_path(selection: &mut Option<Option<PathBuf>>, path: Option<PathBuf>) -> Result<()> {
    if let Some(selected) = selection {
        ensure!(
            *selected == path,
            "configuration path has already been selected"
        );
    } else {
        *selection = Some(path);
    }
    Ok(())
}

/// Select one configuration before `get()`. Repeating the same selection is
/// harmless; switching profiles after any selection is an error.
pub fn init_path(path: Option<&Path>) -> Result<()> {
    let path = match path {
        Some(path) => Some(if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|_| anyhow::anyhow!("cannot resolve configuration path"))?
                .join(path)
        }),
        None => default_path(),
    };
    let mut selected = SELECTED_PATH
        .lock()
        .map_err(|_| anyhow::anyhow!("configuration path selection is unavailable"))?;
    select_path(&mut selected, path)
}

/// Shared read-once settings. A load error is retained and must be surfaced by
/// startup through `validate()`, rather than silently using playable defaults.
pub fn get() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| match Config::path() {
        Some(path) => Config::load_path(&path).unwrap_or_else(|error| {
            let message = match error.to_string().as_str() {
                REMOVED => REMOVED,
                INVALID => INVALID,
                UNSAFE => UNSAFE,
                _ => "myx configuration could not be loaded; check file safety and settings",
            };
            Config {
                load_error: Some(message),
                ..Config::default()
            }
        }),
        None => Config {
            load_error: Some("myx configuration path is unavailable"),
            ..Config::default()
        },
    })
}

/// Documentation/staged setup only. Loading a missing file does not write it.
pub fn template() -> &'static str {
    TEMPLATE
}

const TEMPLATE: &str = "\
# myx settings. All keys are optional. Keep this file private (mode 0600).
#scrolloff = 3
#restore_on_startup = true
#client_id = \"\"
#protocol = \"kitty\"
# Press T in the TUI to toggle the terminal background.
transparent = true

# Both required services are owned by the daemon. Paths must be absolute or ~/.
#[soloist]
#directory = \"~/.config/myx/soloist\"
#device_name = \"myx\"
#initial_volume = 25
#sdk = \"/absolute/path/to/sdk\"
#runner = \"/absolute/path/to/runner\"
#sysroot = \"/absolute/path/to/sysroot\"
#library_manifest = \"/absolute/path/to/arm-libraries.json\"
#library_path = \"/absolute/path/to/libraries\"
#audio_tools = \"/absolute/path/to/audio-tools\"
# Choose only one credential source. Never share the completed file.
#api_key = \"YOUR_SOLOIST_API_KEY\"
#api_key_file = \"/absolute/path/to/api-key\"
#data_dir = \"~/.config/myx/soloist/data\"
#cache_dir = \"~/.config/myx/soloist/cache\"
#expected_sdk_sha256 = \"\"

#[now_playing]
#enabled = false
#path = \"~/.local/state/myx/now-playing.json\"
";

fn document(text: &str) -> Result<toml::Table> {
    ensure!(
        text.len() as u64 <= MAX_CONFIG_BYTES,
        "myx configuration exceeds the size limit"
    );
    toml::from_str(text).map_err(|_| anyhow::anyhow!(INVALID))
}

fn has_removed(table: &toml::Table) -> bool {
    OBSOLETE_ROOT.iter().any(|key| table.contains_key(*key))
        || table
            .get("soloist")
            .and_then(toml::Value::as_table)
            .is_some_and(|soloist| {
                OBSOLETE_SOLOIST
                    .iter()
                    .any(|key| soloist.contains_key(*key))
            })
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if let Some(message) = self.load_error {
            bail!(message);
        }
        self.soloist.validate()?;
        if self.now_playing.enabled {
            let path = self.now_playing.path.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "enabled now-playing output requires an absolute or home-relative path"
                )
            })?;
            ensure!(
                path.is_absolute()
                    || path
                        .strip_prefix("~")
                        .is_ok_and(|rest| !rest.as_os_str().is_empty()),
                "enabled now-playing output requires an absolute or home-relative path"
            );
        }
        Ok(())
    }

    pub fn path() -> Option<PathBuf> {
        let mut selected = SELECTED_PATH.lock().ok()?;
        selected.get_or_insert_with(default_path).clone()
    }

    pub fn load_path(path: &Path) -> Result<Self> {
        let text = private_file::read(path).map_err(|_| anyhow::anyhow!(UNSAFE))?;
        match text {
            Some(text) => Self::parse(&text),
            None => Ok(Self::default()),
        }
    }

    fn parse(text: &str) -> Result<Self> {
        let table = document(text)?;
        ensure!(!has_removed(&table), REMOVED);
        let config: Self = toml::from_str(text).map_err(|_| anyhow::anyhow!(INVALID))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save_transparent(on: bool) -> io::Result<()> {
        Self::save_key("transparent", if on { "true" } else { "false" })
    }

    pub fn save_client_id(id: &str) -> io::Result<()> {
        // Validate here too: this is a public persistence boundary, not a TOML
        // interpolation API. Spotify app IDs are 32 hexadecimal characters.
        if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid client id",
            ));
        }
        Self::save_key("client_id", &format!("\"{id}\""))
    }

    fn save_key(key: &str, value: &str) -> io::Result<()> {
        let path = Self::path().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "configuration path unavailable")
        })?;
        private_file::update(&path, |text| {
            Self::parse(text).map_err(|_| safe_io())?;
            set_key(text, key, value).map_err(|_| safe_io())
        })
        .map_err(|_| safe_io())
    }
}

fn safe_io() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "configuration could not be safely updated",
    )
}

/// Conservative, line-preserving TOML edits. A line must independently parse as
/// a simple assignment or bare table header. Refuse multiline/array-table and
/// other ambiguous syntax rather than risk deleting adjacent credential bytes.
struct Line<'a> {
    raw: &'a str,
    path: Vec<String>,
    header: bool,
    comment: Option<&'a str>,
}

fn comment_start(line: &str) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && ch == '\\' {
            escaped = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() {
            match ch {
                '\'' | '"' => quote = Some(ch),
                '#' => return Some(index),
                _ => {}
            }
        }
    }
    None
}

fn single_key_path(table: &toml::Table) -> Result<Vec<String>> {
    ensure!(table.len() == 1, "configuration edit is ambiguous");
    let (key, value) = table
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!(INVALID))?;
    let mut path = vec![key.clone()];
    if let Some(nested) = value.as_table() {
        path.extend(single_key_path(nested)?);
    }
    Ok(path)
}

fn lines(text: &str) -> Result<Vec<Line<'_>>> {
    let mut scope = Vec::new();
    text.split_inclusive('\n')
        .map(|raw| {
            let comment = comment_start(raw).map(|at| &raw[at..]);
            let code = raw[..comment_start(raw).unwrap_or(raw.len())].trim();
            let mut path = scope.clone();
            let header = code.starts_with('[');
            if !code.is_empty() {
                ensure!(
                    !code.contains("\"\"\"") && !code.contains("'''"),
                    "configuration edit is ambiguous"
                );
                if header {
                    ensure!(
                        !code.starts_with("[[") && code.ends_with(']'),
                        "configuration edit is ambiguous"
                    );
                    let name = &code[1..code.len() - 1];
                    ensure!(
                        name.split('.').all(|part| !part.trim().is_empty()
                            && part
                                .trim()
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric()
                                    || matches!(byte, b'_' | b'-'))),
                        "configuration edit is ambiguous"
                    );
                    scope = name.split('.').map(|part| part.trim().to_owned()).collect();
                    path = scope.clone();
                } else {
                    // Parse both the real assignment and a dummy value to identify
                    // quoted/dotted keys without inspecting its possibly secret value.
                    let _: toml::Table =
                        toml::from_str(code).map_err(|_| anyhow::anyhow!(INVALID))?;
                    let (key, _) = code
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!(INVALID))?;
                    let key: toml::Table = toml::from_str(&format!("{key}= 0"))
                        .map_err(|_| anyhow::anyhow!(INVALID))?;
                    path.extend(single_key_path(&key)?);
                }
            } else {
                path.clear();
            }
            Ok(Line {
                raw,
                path,
                header,
                comment,
            })
        })
        .collect()
}

fn obsolete_path(path: &[String]) -> bool {
    path.first()
        .is_some_and(|key| OBSOLETE_ROOT.contains(&key.as_str()))
        || (path.first().is_some_and(|key| key == "soloist")
            && path
                .get(1)
                .is_some_and(|key| OBSOLETE_SOLOIST.contains(&key.as_str())))
}

/// Produce a staged candidate only. Preserve every retained line byte-for-byte,
/// including credentials and comments; never write or overwrite a live file.
pub fn migration_candidate(text: &str) -> Result<String> {
    let mut expected = document(text)?;
    if !has_removed(&expected) {
        Config::parse(text)?;
        return Ok(text.to_owned());
    }
    for key in OBSOLETE_ROOT {
        expected.remove(*key);
    }
    if let Some(soloist) = expected
        .get_mut("soloist")
        .and_then(toml::Value::as_table_mut)
    {
        for key in OBSOLETE_SOLOIST {
            soloist.remove(*key);
        }
    }
    let mut candidate = String::new();
    for line in lines(text)? {
        if obsolete_path(&line.path) {
            // Standalone comments and trailing comments survive removed keys.
            if let Some(comment) = line.comment {
                candidate.push_str(comment);
            }
        } else {
            candidate.push_str(line.raw);
        }
    }
    ensure!(
        document(&candidate)? == expected,
        "configuration migration is ambiguous; edit a staged copy manually"
    );
    Config::parse(&candidate)?;
    Ok(candidate)
}

/// Explicit setup only. Never consult the shared read-once configuration, launch
/// services, inspect credentials/SDKs, or include source text in diagnostics.
pub fn setup(
    command: crate::cli::ConfigCommand,
    source: &Path,
    output: Option<&Path>,
) -> Result<()> {
    use crate::cli::ConfigCommand;
    match command {
        ConfigCommand::Check => {
            let text = private_file::read(source)
                .map_err(|_| anyhow::anyhow!(UNSAFE))?
                .ok_or_else(|| anyhow::anyhow!("configuration is missing; use config init"))?;
            Config::parse(&text)?;
        }
        ConfigCommand::Init => {
            private_file::create(output.unwrap_or(source), template()).map_err(|_| {
                anyhow::anyhow!(
                    "cannot create new private configuration; destination must not exist"
                )
            })?;
        }
        ConfigCommand::Migrate => {
            let output = output.ok_or_else(|| anyhow::anyhow!("migration requires --output"))?;
            let text = private_file::read(source)
                .map_err(|_| anyhow::anyhow!(UNSAFE))?
                .ok_or_else(|| anyhow::anyhow!("migration source is missing"))?;
            let candidate = migration_candidate(&text)?;
            private_file::create(output, &candidate).map_err(|_| {
                anyhow::anyhow!(
                    "cannot create private migration candidate; destination must not exist"
                )
            })?;
        }
    }
    Ok(())
}

fn set_key(text: &str, key: &str, value: &str) -> Result<String> {
    let mut expected = document(text)?;
    let replacement: toml::Table =
        toml::from_str(&format!("{key} = {value}")).map_err(|_| anyhow::anyhow!(INVALID))?;
    expected.insert(
        key.into(),
        replacement
            .get(key)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?
            .clone(),
    );
    let parsed = lines(text)?;
    let existing = parsed
        .iter()
        .position(|line| !line.header && line.path == [key]);
    let insertion = existing.unwrap_or_else(|| {
        parsed
            .iter()
            .position(|line| line.header)
            .unwrap_or(parsed.len())
    });
    let mut updated = String::new();
    for (index, line) in parsed.iter().enumerate() {
        if index == insertion {
            updated.push_str(&format!("{key} = {value}"));
            if existing.is_some() {
                if let Some(comment) = line.comment {
                    updated.push(' ');
                    updated.push_str(comment.trim_end_matches(['\r', '\n']));
                }
            }
            updated.push('\n');
        }
        if Some(index) != existing {
            updated.push_str(line.raw);
        }
    }
    if insertion == parsed.len() {
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&format!("{key} = {value}\n"));
    }
    ensure!(
        document(&updated)? == expected,
        "configuration update is ambiguous"
    );
    Ok(updated)
}

/// Descriptor-relative Unix operations prevent parent/final symlink traversal.
/// Private temporary files are committed by same-directory atomic rename.
#[cfg(unix)]
mod private_file {
    use super::*;
    use std::ffi::{CString, OsStr};
    use std::fs::{File, Metadata};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    use std::path::Component;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn cstr(value: &OsStr) -> io::Result<CString> {
        CString::new(value.as_bytes()).map_err(|_| safe_io())
    }

    fn opened(fd: libc::c_int) -> io::Result<File> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: the successful open/openat returned a new owned descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn parent(path: &Path, create: bool) -> io::Result<(File, CString)> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let name = cstr(path.file_name().ok_or_else(safe_io)?)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: constant NUL-terminated path and valid flags.
        let mut dir = opened(unsafe { libc::open(c"/".as_ptr(), flags) })?;
        for component in path.parent().ok_or_else(safe_io)?.components() {
            let part = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(part) => cstr(part)?,
                _ => return Err(safe_io()),
            };
            // SAFETY: live directory descriptor and NUL-terminated component.
            let mut next = unsafe { libc::openat(dir.as_raw_fd(), part.as_ptr(), flags) };
            if next < 0 && create && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
                // SAFETY: same descriptor/component; no existing path is modified.
                if unsafe { libc::mkdirat(dir.as_raw_fd(), part.as_ptr(), 0o700) } < 0
                    && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(safe_io());
                }
                // SAFETY: same descriptor/component; O_NOFOLLOW still applies.
                next = unsafe { libc::openat(dir.as_raw_fd(), part.as_ptr(), flags) };
            }
            dir = opened(next)?;
        }
        let metadata = dir.metadata()?;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(safe_io());
        }
        Ok((dir, name))
    }

    fn check(metadata: &Metadata) -> io::Result<()> {
        // SAFETY: geteuid has no preconditions.
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() > MAX_CONFIG_BYTES
        {
            return Err(safe_io());
        }
        Ok(())
    }

    fn read_at(dir: &File, name: &CString) -> io::Result<Option<(String, Metadata)>> {
        // O_NONBLOCK prevents FIFOs/devices from hanging before fstat rejects them.
        // SAFETY: live directory descriptor and NUL-terminated component.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        let mut file = match opened(fd) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        };
        let metadata = file.metadata()?;
        check(&metadata)?;
        let mut text = String::new();
        (&mut file)
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut text)?;
        if text.len() as u64 > MAX_CONFIG_BYTES {
            return Err(safe_io());
        }
        Ok(Some((text, metadata)))
    }

    pub(super) fn read(path: &Path) -> io::Result<Option<String>> {
        let (dir, name) = match parent(path, false) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        };
        Ok(read_at(&dir, &name)?.map(|(text, _)| text))
    }

    pub(super) fn create(path: &Path, text: &str) -> io::Result<()> {
        if text.len() as u64 > MAX_CONFIG_BYTES {
            return Err(safe_io());
        }
        let (dir, name) = parent(path, true)?;
        // Publish only a fully written candidate. linkat is no-replace, including
        // concurrent creators and symlinks; no rename can overwrite live config.
        static NEXT_CREATE: AtomicU64 = AtomicU64::new(0);
        let temp = CString::new(format!(
            ".myx-candidate-{}-{}",
            std::process::id(),
            NEXT_CREATE.fetch_add(1, Ordering::Relaxed)
        ))
        .map_err(|_| safe_io())?;
        // SAFETY: exclusive relative creation through the verified parent fd.
        let mut file = opened(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                temp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })?;
        let result = (|| {
            // SAFETY: only the newly owned candidate descriptor is modified.
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } < 0 {
                return Err(safe_io());
            }
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            // SAFETY: both names are relative to our live directory descriptor.
            if unsafe {
                libc::linkat(
                    dir.as_raw_fd(),
                    temp.as_ptr(),
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            } < 0
            {
                return Err(safe_io());
            }
            dir.sync_all()
        })();
        // SAFETY: unlink only our exclusive temporary name, never the destination.
        let cleanup = unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) };
        result?;
        if cleanup < 0 {
            return Err(safe_io());
        }
        dir.sync_all()
    }

    pub(super) fn update(
        path: &Path,
        edit: impl FnOnce(&str) -> io::Result<String>,
    ) -> io::Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (dir, name) = parent(path, true)?;
        let original = read_at(&dir, &name)?;
        let updated = edit(original.as_ref().map_or("", |(text, _)| text.as_str()))?;
        if updated.len() as u64 > MAX_CONFIG_BYTES {
            return Err(safe_io());
        }
        let temp = CString::new(format!(
            ".myx-config-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .map_err(|_| safe_io())?;
        // SAFETY: exclusive creation in our verified directory with mode 0600.
        let mut file = opened(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                temp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })?;
        let result = (|| {
            // SAFETY: this is our exclusively created file, never the target.
            // fchmod makes the contract independent of a restrictive umask.
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } < 0 {
                return Err(safe_io());
            }
            file.write_all(updated.as_bytes())?;
            file.sync_all()?;
            let current = read_at(&dir, &name)?;
            match (&original, &current) {
                (None, None) => {}
                (Some((before, old)), Some((after, now)))
                    if before == after && old.dev() == now.dev() && old.ino() == now.ino() => {}
                _ => return Err(safe_io()),
            }
            // SAFETY: both names are relative to the same still-open directory.
            if unsafe {
                libc::renameat(
                    dir.as_raw_fd(),
                    temp.as_ptr(),
                    dir.as_raw_fd(),
                    name.as_ptr(),
                )
            } < 0
            {
                return Err(safe_io());
            }
            dir.sync_all()
        })();
        if result.is_err() {
            // SAFETY: remove only the exclusive temporary name we created.
            unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) };
        }
        result
    }
}

#[cfg(not(unix))]
mod private_file {
    use super::*;
    pub(super) fn read(_: &Path) -> io::Result<Option<String>> {
        Err(safe_io())
    }
    pub(super) fn create(_: &Path, _: &str) -> io::Result<()> {
        Err(safe_io())
    }
    pub(super) fn update(_: &Path, _: impl FnOnce(&str) -> io::Result<String>) -> io::Result<()> {
        Err(safe_io())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(text: &str) -> String {
        match Config::parse(text) {
            Ok(_) => panic!("expected invalid configuration"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn defaults_template_and_path_resolution() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.scrolloff, 3);
        assert!(config.restore_on_startup);
        assert!(!config.transparent && !config.now_playing.enabled);
        assert!(Config::parse(template()).unwrap().transparent);
        let soloist = SoloistConfig {
            directory: Some("/private/myx".into()),
            ..SoloistConfig::default()
        };
        assert_eq!(soloist.directory().unwrap(), PathBuf::from("/private/myx"));
        assert_eq!(
            soloist.resolve(None, "sdk").unwrap(),
            PathBuf::from("/private/myx/sdk")
        );
        assert!(soloist
            .resolve(Some(&PathBuf::from("relative")), "sdk")
            .is_err());
        assert_eq!(
            soloist
                .resolve(Some(&PathBuf::from("/other/sdk")), "sdk")
                .unwrap(),
            PathBuf::from("/other/sdk")
        );
    }

    #[test]
    fn path_selection_is_single_and_does_not_change_profiles() {
        let mut selected = None;
        select_path(&mut selected, Some("/private/a".into())).unwrap();
        select_path(&mut selected, Some("/private/a".into())).unwrap();
        assert!(select_path(&mut selected, Some("/private/b".into())).is_err());
        assert!(select_path(&mut selected, None).is_err());
    }

    #[test]
    fn malformed_removed_and_mutually_exclusive_settings_are_redacted() {
        for text in [
            "client_id = \"PRIVATE-SENTINEL\"\nscrolloff = \"PRIVATE-SENTINEL\"",
            "client_id = \"PRIVATE-SENTINEL",
            "[soloist]\napi_key = \"PRIVATE-SENTINEL\"\napi_key_file = \"/PRIVATE-SENTINEL\"",
            "[soloist]\ninitial_volume = 101",
        ] {
            assert!(!error(text).contains("PRIVATE-SENTINEL"));
        }
        for key in OBSOLETE_ROOT {
            assert!(error(&format!("{key} = \"PRIVATE-SENTINEL\"")).contains("removed"));
            assert!(error(&format!("[{key}]\nvalue = \"PRIVATE-SENTINEL\"")).contains("removed"));
        }
        for key in OBSOLETE_SOLOIST {
            assert!(error(&format!("[soloist]\n{key} = \"PRIVATE-SENTINEL\"")).contains("removed"));
        }
        // A lookalike key in an unrelated table is never a removed selector.
        Config::parse("[future]\nbackend = 'kept'\nendpoint = 'kept'").unwrap();
    }

    #[test]
    fn friendly_name_preserves_spaces_unicode_and_enforces_dns_sd_byte_limit() {
        for name in [
            " Shu's 音楽 ".to_owned(),
            "x".repeat(63),
            "é".repeat(31),
            " ".to_owned(),
        ] {
            let soloist = SoloistConfig {
                device_name: name.clone(),
                ..SoloistConfig::default()
            };
            soloist.validate().unwrap();
            assert_eq!(soloist.device_name, name);
        }
        for name in [
            String::new(),
            "x".repeat(64),
            "é".repeat(32),
            "bad\nname".into(),
            "bad\u{7f}name".into(),
        ] {
            let soloist = SoloistConfig {
                device_name: name,
                ..SoloistConfig::default()
            };
            assert!(soloist.validate().is_err());
        }
        for volume in [0, 100] {
            SoloistConfig {
                initial_volume: volume,
                ..SoloistConfig::default()
            }
            .validate()
            .unwrap();
        }
    }

    #[test]
    fn now_playing_requires_a_path_only_when_enabled() {
        for text in [
            "[now_playing]\nenabled = true",
            "[now_playing]\nenabled = true\npath = 'relative'",
            "[now_playing]\nenabled = true\npath = '~'",
        ] {
            assert!(Config::parse(text).is_err());
        }
        for path in ["/private/output.json", "~/.local/state/myx/output.json"] {
            Config::parse(&format!("[now_playing]\nenabled = true\npath = '{path}'")).unwrap();
        }
        Config::parse("[now_playing]\nenabled = false\npath = 'relative'").unwrap();
    }

    #[test]
    fn staged_migration_preserves_credential_bytes_and_unrelated_nested_keys() {
        let text = "# settings\r\nbackend = 'soloist' # retain comment\r\nbitrate = 320\r\n[soloist]\r\napi_key = 'PRIVATE-SENTINEL#literal' # unchanged\r\nendpoint = 'http://unused'\r\ndsp = false\r\n[future]\r\nbackend = 'untouched'\r\n[future.nested]\r\ndsp = true\r\n";
        let candidate = migration_candidate(text).unwrap();
        assert!(candidate.contains("api_key = 'PRIVATE-SENTINEL#literal' # unchanged\r\n"));
        assert!(candidate.contains("# retain comment\r\n"));
        assert!(candidate
            .contains("[future]\r\nbackend = 'untouched'\r\n[future.nested]\r\ndsp = true\r\n"));
        Config::parse(&candidate).unwrap();
        assert_eq!(migration_candidate(&candidate).unwrap(), candidate);
        assert!(text.contains("endpoint = 'http://unused'"));
        let tables = "[backend]\nname = 'obsolete'\n[soloist]\napi_key = 'PRIVATE-SENTINEL'\n";
        assert_eq!(
            migration_candidate(tables).unwrap(),
            "[soloist]\napi_key = 'PRIVATE-SENTINEL'\n"
        );
        for ambiguous in [
            "soloist = { endpoint = 'removed', api_key = 'PRIVATE-SENTINEL' }",
            "backend = '''multi\nPRIVATE-SENTINEL\n'''",
            "backend = 'soloist'\n[soloist]\napi_key = '''PRIVATE-SENTINEL'''",
        ] {
            let error = migration_candidate(ambiguous).err().unwrap().to_string();
            assert!(!error.contains("PRIVATE-SENTINEL"));
        }
    }

    #[test]
    fn scoped_setting_updates_do_not_edit_nested_keys_or_credentials() {
        let text = "# doc\n[soloist] # table\napi_key = 'PRIVATE-SENTINEL'\ntransparent = false\n";
        let updated = set_key(text, "transparent", "true").unwrap();
        assert!(updated.starts_with("# doc\ntransparent = true\n[soloist] # table\n"));
        assert!(updated.ends_with("api_key = 'PRIVATE-SENTINEL'\ntransparent = false\n"));
        let updated = set_key("transparent = false # keep\n", "transparent", "true").unwrap();
        assert_eq!(updated, "transparent = true # keep\n");
        let updated = set_key("\"transparent\" = false\n", "transparent", "true").unwrap();
        assert!(Config::parse(&updated).unwrap().transparent);
    }

    #[test]
    fn client_id_cannot_inject_toml_or_secret_values_into_errors() {
        // Invalid inputs return before selecting or opening any config path.
        for id in [
            "",
            "PRIVATE-SENTINEL",
            "\"\ntransparent = true",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ] {
            let error = Config::save_client_id(id).unwrap_err().to_string();
            assert!(!error.contains("PRIVATE-SENTINEL"));
        }
        let text = set_key(
            "[soloist]\napi_key = 'PRIVATE-SENTINEL'\n",
            "client_id",
            "'0123456789abcdef0123456789abcdef'",
        )
        .unwrap();
        assert_eq!(
            Config::parse(&text).unwrap().client_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::fs;
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

        fn fixture() -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            // macOS's temporary root may itself be a symlink. Fixtures select
            // the physical path; production deliberately refuses symlink paths.
            let root = dir.path().canonicalize().unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let path = root.join("config.toml");
            (dir, path)
        }

        fn write(path: &Path, text: &str) {
            fs::write(path, text).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        #[test]
        fn missing_load_uses_defaults_without_creating_anything() {
            let (_dir, path) = fixture();
            assert_eq!(Config::load_path(&path).unwrap().scrolloff, 3);
            assert!(!path.exists());
            let path = path.parent().unwrap().join("missing/config.toml");
            Config::load_path(&path).unwrap();
            assert!(!path.parent().unwrap().exists());
        }

        #[test]
        fn private_regular_file_loads_and_permissions_size_and_links_are_rejected() {
            let (_dir, path) = fixture();
            write(&path, "scrolloff = 7");
            assert_eq!(Config::load_path(&path).unwrap().scrolloff, 7);
            for mode in [0o644, 0o640, 0o400, 0o1600] {
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                assert!(Config::load_path(&path).is_err());
            }
            write(&path, &"#".repeat(MAX_CONFIG_BYTES as usize + 1));
            assert!(Config::load_path(&path).is_err());
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            assert!(Config::load_path(&path).is_err());
            fs::remove_dir(&path).unwrap();
            let target = path.with_extension("target");
            write(&target, "client_id = 'PRIVATE-SENTINEL'");
            symlink(&target, &path).unwrap();
            assert!(Config::load_path(&path).is_err());
            fs::remove_file(&path).unwrap();
            fs::hard_link(&target, &path).unwrap();
            assert!(Config::load_path(&path).is_err());
            fs::remove_file(&path).unwrap();
            let link = path.parent().unwrap().join("linked-parent");
            symlink(path.parent().unwrap(), &link).unwrap();
            assert!(Config::load_path(&link.join("missing.toml")).is_err());
        }

        #[test]
        fn atomic_update_preserves_secrets_and_never_truncates_unsafe_files() {
            let (_dir, path) = fixture();
            write(&path, "[soloist]\napi_key = 'PRIVATE-SENTINEL'\n");
            let before = fs::metadata(&path).unwrap().ino();
            private_file::update(&path, |text| {
                set_key(text, "transparent", "true").map_err(|_| safe_io())
            })
            .unwrap();
            let metadata = fs::metadata(&path).unwrap();
            assert_eq!(metadata.mode() & 0o777, 0o600);
            assert_ne!(before, metadata.ino());
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.contains("api_key = 'PRIVATE-SENTINEL'\n"));
            assert!(Config::load_path(&path).unwrap().transparent);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(private_file::update(&path, |_| Ok("".into())).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
            fs::remove_file(&path).unwrap();
            let target = path.with_extension("target");
            write(&target, "PRIVATE-SENTINEL");
            symlink(&target, &path).unwrap();
            assert!(private_file::update(&path, |_| Ok("".into())).is_err());
            assert_eq!(fs::read_to_string(&target).unwrap(), "PRIVATE-SENTINEL");
        }

        #[test]
        fn writable_parent_and_fifo_are_refused_without_blocking() {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;

            let (_dir, path) = fixture();
            let root = path.parent().unwrap();
            write(&path, "scrolloff = 7");
            fs::set_permissions(root, fs::Permissions::from_mode(0o777)).unwrap();
            assert!(Config::load_path(&path).is_err());
            assert!(private_file::update(&path, |_| Ok("".into())).is_err());
            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "scrolloff = 7");
            fs::remove_file(&path).unwrap();
            let name = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: create only a FIFO in the owned disposable fixture.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            assert!(Config::load_path(&path).is_err());
            assert!(private_file::update(&path, |_| Ok("".into())).is_err());
        }

        #[test]
        fn update_detects_a_target_changed_during_edit() {
            let (_dir, path) = fixture();
            write(&path, "scrolloff = 7");
            let result = private_file::update(&path, |_| {
                write(&path, "scrolloff = 9");
                Ok("transparent = true\n".into())
            });
            assert!(result.is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "scrolloff = 9");
            let entries = fs::read_dir(path.parent().unwrap()).unwrap().count();
            assert_eq!(
                entries, 1,
                "failed updates must remove only their temporary file"
            );
        }
    }
}
