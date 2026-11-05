//! Read-only installed release discovery. This is not a backend selector and
//! never resolves SDKs, keys, user data, cookies, scratch or caches into a bundle.
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub const ELFUSE_REV: &str = "ba455f45592cd12488519e207c44c32fa8e167df";
pub const NIXPKGS_REV: &str = "0954f7ee2f6bb3dc7d4e3d0d8bcb8fd4bde4cfc5";
const CA_BUNDLE_MEMBERS: &[&str] = &[
    "runtime/sysroot/etc/ssl/cert.pem",
    "runtime/sysroot/etc/ssl/certs/ca-certificates.crt",
    "runtime/sysroot/etc/ssl/ca-bundle.pem",
    "runtime/sysroot/etc/pki/tls/cacert.pem",
    "runtime/sysroot/etc/pki/tls/certs/ca-bundle.crt",
    "runtime/sysroot/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
];

#[derive(Debug)]
pub struct RuntimeBundle {
    pub root: PathBuf,
    pub runner: PathBuf,
    pub sysroot: PathBuf,
    pub library_manifest: PathBuf,
    /// Resolved host absolute paths, suitable for runtime Config.library_path.
    pub library_path: String,
    pub audio_tools: PathBuf,
    /// Pass to pulseaudio --dl-search-path, never rely on its compiled Nix path.
    pub audio_modules: PathBuf,
}

impl RuntimeBundle {
    /// SDKs, credentials and mutable state must never become bundle members,
    /// including through a symlink or a not-yet-created directory.
    pub fn ensure_external_user_path(&self, path: &Path) -> Result<()> {
        ensure!(path.is_absolute(), "user asset path must be absolute");
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::RootDir | Component::Normal(_) => normalized.push(component),
                Component::CurDir => {}
                Component::ParentDir => {
                    normalized.pop();
                }
                _ => anyhow::bail!("invalid user asset path"),
            }
        }
        ensure!(
            !normalized.starts_with(&self.root),
            "user assets and output must be outside the runtime bundle"
        );
        // Resolve the original spelling too: symlink/.. traversal can differ
        // from lexical normalization.
        let mut ancestor = path;
        loop {
            match ancestor.canonicalize() {
                Ok(resolved) => {
                    ensure!(
                        !resolved.starts_with(&self.root),
                        "user assets and output must be outside the runtime bundle"
                    );
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ancestor = ancestor
                        .parent()
                        .context("cannot inspect user asset path")?;
                }
                Err(_) => anyhow::bail!("cannot inspect user asset path"),
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    target: String,
    elfuse: String,
    nixpkgs: String,
    runner: String,
    sysroot: String,
    library_manifest: String,
    audio_tools: String,
    audio_modules: String,
    files: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Libraries {
    schema: u32,
    arch: String,
    loader: String,
    /// Bundle-root relative directories. Never pass these directly as LD paths.
    #[serde(rename = "libraryPath")]
    library_path: Vec<String>,
}

/// Only bin/myx with ../runtime/manifest.json is recognized. A missing manifest
/// returns None for a source/development install; a present invalid one is fatal.
pub fn discover() -> Result<Option<RuntimeBundle>> {
    discover_from_exe(&std::env::current_exe().context("locate installed executable")?)
}

pub fn discover_from_exe(exe: &Path) -> Result<Option<RuntimeBundle>> {
    ensure!(exe.is_absolute(), "bundle executable must be absolute");
    let bin = exe.parent().context("bundle executable has no parent")?;
    if bin.file_name().is_none_or(|name| name != "bin")
        || exe.file_name().is_none_or(|name| name != "myx")
    {
        return Ok(None);
    }
    let root = bin.parent().context("bundle has no root")?;
    let manifest = root.join("runtime/manifest.json");
    match fs::symlink_metadata(&manifest) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        result => {
            result.context("inspect runtime bundle manifest")?;
        }
    }
    safe_absolute(root)?;
    checked(root, "bin/myx", false)?;
    checked(root, "runtime/manifest.json", false)?;
    let manifest: Manifest = serde_json::from_slice(&bounded(&manifest, 4 * 1024 * 1024)?)
        .context("invalid runtime bundle manifest")?;
    ensure!(
        manifest.schema == 1
            && manifest.target == "aarch64-apple-darwin"
            && manifest.elfuse == ELFUSE_REV
            && manifest.nixpkgs == NIXPKGS_REV,
        "runtime bundle schema, target or pin mismatch"
    );
    ensure!(
        !manifest.files.is_empty() && manifest.files.len() <= 20000,
        "invalid bundle inventory"
    );
    ensure!(
        manifest.runner == "runtime/bin/elfuse"
            && manifest.sysroot == "runtime/sysroot"
            && manifest.library_manifest == "runtime/arm-libraries.json"
            && manifest.audio_tools == "runtime/audio"
            && manifest.audio_modules == "runtime/audio/modules",
        "invalid runtime bundle roles"
    );
    for (relative, digest) in &manifest.files {
        ensure!(allowed_member(relative), "excluded bundle asset");
        let path = checked(root, relative, false)?;
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid bundle checksum"
        );
        let mut file = fs::File::open(path).context("read bundle member")?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let n = file.read(&mut buffer).context("hash bundle member")?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        ensure!(
            format!("{:x}", hash.finalize()) == *digest,
            "runtime bundle checksum mismatch"
        );
        validate_dependencies(root, &root.join(relative))?;
    }
    // No undeclared files, including credential/cache additions, are accepted.
    let mut actual = BTreeSet::new();
    inventory(root, root, &mut actual)?;
    actual.remove("runtime/manifest.json");
    ensure!(
        actual == manifest.files.keys().cloned().collect(),
        "runtime bundle inventory mismatch"
    );
    let runner = checked(root, &manifest.runner, false)?;
    let sysroot = checked(root, &manifest.sysroot, true)?;
    let library_manifest = checked(root, &manifest.library_manifest, false)?;
    let audio_tools = checked(root, &manifest.audio_tools, true)?;
    let audio_modules = checked(root, &manifest.audio_modules, true)?;
    for required in [
        "runtime/audio/bin/pulseaudio",
        "runtime/audio/bin/pactl",
        "runtime/audio/lib/libpulse.0.dylib",
        "runtime/audio/modules/module-coreaudio-device.so",
        "runtime/audio/modules/module-null-sink.so",
        "runtime/audio/modules/module-native-protocol-tcp.so",
        "runtime/audio/modules/module-native-protocol-unix.so",
    ] {
        checked(root, required, false)?;
    }
    let libraries: Libraries = serde_json::from_slice(&bounded(&library_manifest, 65536)?)
        .context("invalid bundle library manifest")?;
    ensure!(
        libraries.schema == 1
            && libraries.arch == "aarch64-linux"
            && libraries.loader == "runtime/sysroot/lib/ld-linux-aarch64.so.1"
            && !libraries.library_path.is_empty()
            && libraries.library_path.len() <= 128
            && libraries.library_path.iter().collect::<BTreeSet<_>>().len()
                == libraries.library_path.len()
            && libraries
                .library_path
                .iter()
                .all(|path| path == "runtime/sysroot/lib"
                    || path.starts_with("runtime/sysroot/nix/store/")),
        "invalid bundle library target"
    );
    checked(root, &libraries.loader, false)?;
    let library_path = libraries
        .library_path
        .iter()
        .map(|p| {
            checked(root, p, true)?
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("bundle path must be UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?
        .join(":");
    Ok(Some(RuntimeBundle {
        root: root.to_owned(),
        runner,
        sysroot,
        library_manifest,
        library_path,
        audio_tools,
        audio_modules,
    }))
}

fn bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "bundle metadata exceeds size limit"
    );
    Ok(bytes)
}

fn safe_absolute(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "bundle root must be absolute");
    let text = path.to_str().context("bundle path must be UTF-8")?;
    ensure!(!text.contains(':') && !text.chars().any(char::is_control)
        && !text.starts_with("/nix/store/")
        && !path.components().any(|c| matches!(c, Component::Normal(n) if n == ".git" || n == "target" || n == ".direnv")), "unsafe or development bundle location");
    let mut current = PathBuf::new();
    for part in path.components() {
        ensure!(
            matches!(part, Component::RootDir | Component::Normal(_)),
            "unsafe bundle ancestor"
        );
        current.push(part);
        let metadata = fs::symlink_metadata(&current)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "symlink or invalid bundle ancestor"
        );
        safe_metadata(&metadata, false)?;
    }
    Ok(())
}

fn checked(root: &Path, relative: &str, directory: bool) -> Result<PathBuf> {
    let path = Path::new(relative);
    ensure!(
        !relative.is_empty()
            && !relative.contains([':', '\\'])
            && !relative.chars().any(char::is_control)
            && path.components().all(|c| matches!(c, Component::Normal(_)))
            && relative
                .split('/')
                .all(|s| !s.is_empty() && s != "." && s != ".."),
        "unsafe bundle member path"
    );
    let mut resolved = root.to_owned();
    let parts: Vec<_> = path.components().collect();
    for (index, part) in parts.iter().enumerate() {
        resolved.push(part);
        let metadata = fs::symlink_metadata(&resolved).context("missing bundle member")?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "bundle symlink rejected"
        );
        safe_metadata(&metadata, metadata.is_file())?;
        ensure!(
            if index + 1 == parts.len() && !directory {
                metadata.is_file()
            } else {
                metadata.is_dir()
            },
            "invalid bundle member type"
        );
    }
    Ok(resolved)
}

fn allowed_member(relative: &str) -> bool {
    matches!(
        relative,
        "bin/myx"
            | "LICENSE"
            | "NOTICE"
            | "runtime/bin/elfuse"
            | "runtime/arm-libraries.json"
            | "runtime/audio/bin/pulseaudio"
            | "runtime/audio/bin/pactl"
            | "runtime/sysroot/etc/ssl/cert.pem"
    ) || CA_BUNDLE_MEMBERS.contains(&relative)
        || relative.starts_with("licenses/")
        || relative.strip_prefix("sources/").is_some_and(|name| {
            let bytes = name.as_bytes();
            name == "inventory.json"
                || (bytes.len() > 65
                    && bytes[..64].iter().all(u8::is_ascii_hexdigit)
                    && matches!(bytes[64], b'.' | b'-')
                    && bytes[65..].iter().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                    }))
        })
        || (relative.starts_with("runtime/audio/lib/")
            && (relative.ends_with(".dylib") || relative.ends_with(".so")))
        || (relative.starts_with("runtime/audio/modules/") && relative.ends_with(".so"))
        || ((relative.starts_with("runtime/sysroot/lib/")
            || relative.starts_with("runtime/sysroot/nix/store/"))
            && relative.contains(".so"))
}

#[cfg(unix)]
fn safe_metadata(metadata: &fs::Metadata, file: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // System ancestors are allowed, never another user's writable runtime.
    let sticky_root = !file && metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
    ensure!(
        (metadata.mode() & 0o022 == 0 || sticky_root)
            && (metadata.uid() == 0 || metadata.uid() == unsafe { libc::geteuid() })
            && (!file || metadata.nlink() == 1)
            && metadata.mode() & 0o6000 == 0,
        "unsafe runtime bundle permissions or links"
    );
    Ok(())
}

#[cfg(not(unix))]
fn safe_metadata(_: &fs::Metadata, _: bool) -> Result<()> {
    anyhow::bail!("runtime bundle requires macOS")
}

/// Inspect Mach-O loader commands without running native tools. ELF closure
/// verification is also performed by the assembler; no vendor ELF is accepted.
fn validate_dependencies(root: &Path, path: &Path) -> Result<()> {
    let mut file = fs::File::open(path)?;
    let mut header = [0_u8; 32];
    let read = file.read(&mut header)?;
    let member = path.strip_prefix(root).context("invalid bundle member")?;
    let expected_elf = member.starts_with("runtime/sysroot/lib")
        || member.starts_with("runtime/sysroot/nix/store");
    let expected_mach = member == Path::new("bin/myx")
        || member == Path::new("runtime/bin/elfuse")
        || member.starts_with("runtime/audio/bin")
        || member.starts_with("runtime/audio/lib")
        || member.starts_with("runtime/audio/modules");
    ensure!(
        !expected_elf || (read == 32 && header[..4] == *b"\x7fELF"),
        "invalid bundle ELF member"
    );
    ensure!(
        !expected_mach || (read == 32 && header[..4] == [0xcf, 0xfa, 0xed, 0xfe]),
        "invalid bundle Mach-O member"
    );
    if read < 32 {
        return Ok(());
    }
    if header[..4] == *b"\x7fELF" {
        return validate_elf(root, path);
    }
    if header[..4] != [0xcf, 0xfa, 0xed, 0xfe] {
        return Ok(());
    }
    ensure!(
        u32::from_le_bytes(header[4..8].try_into()?) == 0x0100000c,
        "bundle Mach-O must be ARM64"
    );
    let count = u32::from_le_bytes(header[16..20].try_into()?);
    let size = u32::from_le_bytes(header[20..24].try_into()?) as usize;
    ensure!(
        count <= 4096 && size <= 4 * 1024 * 1024,
        "invalid Mach-O load commands"
    );
    let mut bytes = vec![0; size];
    file.read_exact(&mut bytes)?;
    let mut offset = 0;
    for _ in 0..count {
        ensure!(offset + 8 <= bytes.len(), "invalid Mach-O load command");
        let command = u32::from_le_bytes(bytes[offset..offset + 4].try_into()?);
        let length = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
        ensure!(
            length >= 8 && offset + length <= bytes.len(),
            "invalid Mach-O command size"
        );
        // LC_RPATH must be absent; imports and IDs must be loader-relative or OS.
        ensure!(command != 0x8000001c, "unexpected runtime Mach-O rpath");
        if matches!(
            command,
            0xc | 0xd | 0x80000018 | 0x8000001f | 0x20 | 0x80000023
        ) {
            ensure!(length >= 24, "invalid Mach-O dylib command");
            let start = u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into()?) as usize;
            ensure!(start >= 24 && start < length, "invalid Mach-O dylib name");
            let raw = &bytes[offset + start..offset + length];
            let end = raw
                .iter()
                .position(|b| *b == 0)
                .context("unterminated Mach-O name")?;
            let name = std::str::from_utf8(&raw[..end])?;
            ensure!(
                name.starts_with("@loader_path/")
                    || name.starts_with("/usr/lib/")
                    || name.starts_with("/System/Library/"),
                "unsafe or development Mach-O dependency"
            );
            if let Some(relative) = name.strip_prefix("@loader_path/") {
                let dependency = path
                    .parent()
                    .context("missing loader directory")?
                    .join(relative)
                    .canonicalize()
                    .context("unresolved runtime Mach-O dependency")?;
                ensure!(
                    dependency.starts_with(root) && dependency.is_file(),
                    "Mach-O dependency escapes bundle"
                );
            } else {
                ensure!(
                    Path::new(name)
                        .components()
                        .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
                    "unsafe OS dependency path"
                );
            }
        }
        offset += length;
    }
    ensure!(offset == size, "invalid Mach-O command inventory");
    Ok(())
}

fn origin_target(root: &Path, binary: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        name == "$ORIGIN" || name.starts_with("$ORIGIN/"),
        "unsafe or development ELF library path"
    );
    ensure!(
        !name.contains(['\\', ':']) && !name.chars().any(char::is_control),
        "unsafe ELF origin path"
    );
    let target = binary
        .parent()
        .context("ELF has no directory")?
        .join(name.trim_start_matches("$ORIGIN").trim_start_matches('/'))
        .canonicalize()
        .context("unclosed ELF origin path")?;
    ensure!(
        target.starts_with(root.join("runtime/sysroot")),
        "ELF origin path escapes private sysroot"
    );
    Ok(target)
}

fn guest_elf_target(root: &Path, binary: &Path, name: &str) -> Result<PathBuf> {
    if name.starts_with("/nix/store/") || name == "/lib/ld-linux-aarch64.so.1" {
        ensure!(
            !name.contains(['\\', ':'])
                && !name.chars().any(char::is_control)
                && Path::new(name)
                    .components()
                    .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
            "unsafe guest ELF path"
        );
        return Ok(root
            .join("runtime/sysroot")
            .join(name.trim_start_matches('/')));
    }
    origin_target(root, binary, name)
}

fn validate_elf(root: &Path, path: &Path) -> Result<()> {
    // Public redistributable shared objects only, ELF64 little-endian AArch64.
    // Bound all offsets before slicing; no external tool or guest execution.
    let bytes = bounded(path, 128 * 1024 * 1024)?;
    ensure!(
        bytes.len() >= 64 && bytes[4..6] == [2, 1] && bytes[18..20] == [183, 0],
        "invalid bundle ELF architecture"
    );
    let u64_at = |at: usize| -> Result<u64> {
        let end = at.checked_add(8).context("ELF offset overflow")?;
        Ok(u64::from_le_bytes(
            bytes
                .get(at..end)
                .context("invalid ELF offset")?
                .try_into()?,
        ))
    };
    let phoff = usize::try_from(u64_at(32)?)?;
    let count = u16::from_le_bytes(bytes[56..58].try_into()?) as usize;
    ensure!(
        count <= 1024 && u16::from_le_bytes(bytes[54..56].try_into()?) == 56,
        "invalid ELF program headers"
    );
    ensure!(
        phoff
            .checked_add(count * 56)
            .is_some_and(|end| end <= bytes.len()),
        "ELF program headers exceed file"
    );
    let mut loads = Vec::new();
    let mut dynamic = None;
    for index in 0..count {
        let at = phoff
            .checked_add(index * 56)
            .context("ELF offset overflow")?;
        let header = bytes
            .get(at..at + 56)
            .context("invalid ELF program header")?;
        let kind = u32::from_le_bytes(header[..4].try_into()?);
        let offset = u64_at(at + 8)?;
        let address = u64_at(at + 16)?;
        let size = u64_at(at + 32)?;
        if kind == 3 {
            let end = offset
                .checked_add(size)
                .context("ELF interpreter overflow")?;
            let raw = bytes
                .get(usize::try_from(offset)?..usize::try_from(end)?)
                .context("invalid ELF interpreter range")?;
            ensure!(
                !raw.is_empty() && raw.len() <= 4096 && raw.last() == Some(&0),
                "invalid ELF interpreter"
            );
            let name = std::str::from_utf8(&raw[..raw.len() - 1])?;
            ensure!(
                guest_elf_target(root, path, name)?.is_file(),
                "unclosed guest ELF interpreter"
            );
        }
        if kind == 1 {
            loads.push((address, offset, size));
        }
        if kind == 2 {
            dynamic = Some((offset, size));
        }
    }
    let (offset, size) = dynamic.context("runtime ELF has no dynamic table")?;
    ensure!(
        size <= 1024 * 1024 && size % 16 == 0,
        "invalid ELF dynamic table"
    );
    let mut strings = None;
    let mut strings_size = None;
    let mut names = Vec::new();
    let start = usize::try_from(offset)?;
    ensure!(
        start
            .checked_add(usize::try_from(size)?)
            .is_some_and(|end| end <= bytes.len()),
        "ELF dynamic table exceeds file"
    );
    for index in 0..usize::try_from(size / 16)? {
        let at = start
            .checked_add(index * 16)
            .context("ELF offset overflow")?;
        let tag = u64_at(at)?;
        let value = u64_at(at + 8)?;
        match tag {
            0 => break,
            5 => strings = Some(value),
            10 => strings_size = Some(value),
            1 | 15 | 29 => names.push((tag, value)),
            _ => {}
        }
    }
    let address = strings.context("ELF string table missing")?;
    let size = strings_size.context("ELF string size missing")?;
    let (base, offset, span) = loads
        .iter()
        .find(|(base, _, span)| address >= *base && address - base < *span)
        .context("ELF string table outside load segments")?;
    ensure!(
        size <= span - (address - base),
        "ELF string table exceeds segment"
    );
    let start = usize::try_from(
        offset
            .checked_add(address - base)
            .context("ELF offset overflow")?,
    )?;
    let end = start
        .checked_add(usize::try_from(size)?)
        .context("ELF offset overflow")?;
    let strings = bytes.get(start..end).context("invalid ELF string range")?;
    let libraries: Libraries =
        serde_json::from_slice(&bounded(&root.join("runtime/arm-libraries.json"), 65536)?)?;
    ensure!(
        !libraries.library_path.is_empty() && libraries.library_path.len() <= 128,
        "invalid ELF library search inventory"
    );
    let mut search = libraries
        .library_path
        .iter()
        .map(|value| checked(root, value, true))
        .collect::<Result<Vec<_>>>()?;
    let mut dependencies = Vec::new();
    for (tag, offset) in names {
        let raw = strings
            .get(usize::try_from(offset)?..)
            .context("invalid ELF name offset")?;
        let end = raw
            .iter()
            .position(|b| *b == 0)
            .context("unterminated ELF dependency")?;
        let name = std::str::from_utf8(&raw[..end])?;
        if tag == 1 {
            if name.starts_with("$ORIGIN/") || name.starts_with('/') {
                ensure!(
                    guest_elf_target(root, path, name)?.is_file(),
                    "unclosed runtime ELF dependency"
                );
                continue;
            }
            ensure!(
                !name.is_empty()
                    && !name.contains(['/', '\\', ':'])
                    && !name.chars().any(char::is_control)
                    && name != "."
                    && name != "..",
                "unsafe or development ELF dependency"
            );
            dependencies.push(name.to_owned());
        } else {
            for entry in name.split(':').filter(|entry| !entry.is_empty()) {
                let directory = guest_elf_target(root, path, entry)?;
                ensure!(
                    !directory.exists() || directory.is_dir(),
                    "invalid ELF search directory"
                );
                search.push(directory);
            }
        }
    }
    for name in dependencies {
        ensure!(
            search
                .iter()
                .any(|directory| directory.join(&name).is_file()),
            "unclosed runtime ELF dependency"
        );
    }
    Ok(())
}

fn inventory(root: &Path, directory: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    ensure!(files.len() <= 20000, "bundle inventory exceeds limit");
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .context("bundle traversal")?
            .to_str()
            .context("bundle member must be UTF-8")?
            .to_owned();
        let kind = entry.file_type()?;
        let path = checked(root, &relative, kind.is_dir())?;
        if kind.is_dir() {
            ensure!(
                matches!(
                    relative.as_str(),
                    "bin"
                        | "runtime"
                        | "runtime/bin"
                        | "runtime/audio"
                        | "runtime/audio/bin"
                        | "runtime/audio/lib"
                        | "runtime/audio/modules"
                        | "runtime/sysroot"
                        | "runtime/sysroot/lib"
                        | "runtime/sysroot/nix"
                        | "runtime/sysroot/nix/store"
                        | "runtime/sysroot/etc"
                        | "runtime/sysroot/etc/ssl"
                        | "runtime/sysroot/etc/ssl/certs"
                        | "runtime/sysroot/etc/pki"
                        | "runtime/sysroot/etc/pki/tls"
                        | "runtime/sysroot/etc/pki/tls/certs"
                        | "runtime/sysroot/etc/pki/ca-trust"
                        | "runtime/sysroot/etc/pki/ca-trust/extracted"
                        | "runtime/sysroot/etc/pki/ca-trust/extracted/pem"
                        | "licenses"
                        | "sources"
                ) || relative.starts_with("licenses/")
                    || relative.starts_with("runtime/sysroot/nix/store/"),
                "excluded runtime bundle directory"
            );
            ensure!(
                path.components().count() <= root.components().count() + 24,
                "bundle nesting exceeds limit"
            );
            inventory(root, &path, files)?;
        } else {
            ensure!(kind.is_file(), "nonregular bundle member");
            files.insert(relative);
        }
    }
    Ok(())
}

#[cfg(test)]
mod source_member_tests {
    use super::{allowed_member, guest_elf_target};

    #[test]
    fn original_guest_store_paths_are_contained_without_allowing_host_paths() {
        let root = std::path::Path::new("/owned/copied-player");
        let binary = root.join("runtime/sysroot/lib/ld-linux-aarch64.so.1");
        assert_eq!(
            guest_elf_target(root, &binary, "/nix/store/public-glibc/lib/libc.so.6").unwrap(),
            root.join("runtime/sysroot/nix/store/public-glibc/lib/libc.so.6")
        );
        for name in [
            "/etc/passwd",
            "/usr/lib/libc.so.6",
            "/nix/store/package/../../outside",
            "/nix/store/package/lib\\escape",
        ] {
            assert!(guest_elf_target(root, &binary, name).is_err());
        }
    }

    #[test]
    fn source_archives_are_flat_hash_named_members_without_utf8_slicing_panics() {
        assert!(allowed_member("sources/inventory.json"));
        assert!(allowed_member(&format!(
            "sources/{}.tar.gz",
            "a".repeat(64)
        )));
        for name in [
            "sources/vendor/sdk",
            "sources/sdk",
            "sources/../config",
            "sources/🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀.tar.gz",
        ] {
            assert!(!allowed_member(name));
        }
    }
}
