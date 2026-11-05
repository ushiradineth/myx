//! Primary integration: include as a cfg(test) module from src/lib.rs after
//! exporting runtime_bundle. Pure/owned-fixture only; never launches helpers.
use crate::cli::{Cli, ConfigCommand, Mode};
use crate::config;
use crate::runtime_bundle::{discover_from_exe, ELFUSE_REV, NIXPKGS_REV};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

fn private_root() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    (temp, root)
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn explicit_setup_cli_and_redacted_rejections() {
    let cli = Cli::parse_from([
        "myx",
        "config",
        "migrate",
        "--config",
        "/private/old",
        "--output",
        "/private/new",
    ])
    .unwrap();
    assert_eq!(cli.mode, Mode::Config(ConfigCommand::Migrate));
    assert_eq!(cli.output, Some(PathBuf::from("/private/new")));
    for args in [
        vec!["config", "migrate"],
        vec!["config", "check", "--output", "PRIVATE-SENTINEL"],
        vec!["config", "init", "--volume", "50"],
        vec!["config", "init", "--output", "a", "--output", "b"],
        vec!["daemon", "start", "--output", "PRIVATE-SENTINEL"],
    ] {
        let error = Cli::parse_from(std::iter::once("myx").chain(args)).unwrap_err();
        assert!(!error.to_string().contains("PRIVATE-SENTINEL"));
    }
}

#[test]
fn setup_preserves_source_bytes_never_clobbers_and_checks_static_only() {
    let (_temp, root) = private_root();
    let source = root.join("old.toml");
    let candidate = root.join("candidate.toml");
    let original = "# keep\r\nbackend = 'soloist' # keep too\r\n[soloist]\r\napi_key = 'PRIVATE-SENTINEL#key' # exact bytes\r\ndsp = false\r\n";
    write(&source, original);
    config::setup(ConfigCommand::Migrate, &source, Some(&candidate)).unwrap();
    assert_eq!(fs::read_to_string(&source).unwrap(), original);
    let migrated = fs::read_to_string(&candidate).unwrap();
    assert!(migrated.contains("api_key = 'PRIVATE-SENTINEL#key' # exact bytes\r\n"));
    assert!(migrated.contains("# keep too\r\n"));
    config::setup(ConfigCommand::Check, &candidate, None).unwrap();
    assert!(config::setup(ConfigCommand::Migrate, &source, Some(&candidate)).is_err());
    assert!(config::setup(ConfigCommand::Init, &candidate, None).is_err());
    assert_eq!(fs::read_to_string(&candidate).unwrap(), migrated);
    let missing = root.join("uncreated.toml");
    assert!(config::setup(ConfigCommand::Check, &missing, None).is_err());
    assert!(!missing.exists());
    let link = root.join("link.toml");
    symlink(&source, &link).unwrap();
    assert!(config::setup(ConfigCommand::Migrate, &link, Some(&missing)).is_err());
    assert!(!missing.exists());
}

fn bundle(root: &Path) -> PathBuf {
    let mut files = BTreeMap::new();
    // Inert metadata fixtures, not runnable helper substitutes.
    for member in [
        "bin/myx",
        "runtime/bin/elfuse",
        "runtime/audio/bin/pulseaudio",
        "runtime/audio/bin/pactl",
        "runtime/audio/lib/libpulse.0.dylib",
        "runtime/audio/modules/module-coreaudio-device.so",
        "runtime/audio/modules/module-null-sink.so",
        "runtime/audio/modules/module-native-protocol-tcp.so",
        "runtime/audio/modules/module-native-protocol-unix.so",
        "runtime/sysroot/lib/ld-linux-aarch64.so.1",
    ] {
        let bytes = if member.contains("sysroot") {
            elf_metadata()
        } else {
            mach_metadata()
        };
        write(&root.join(member), "");
        fs::write(root.join(member), &bytes).unwrap();
        files.insert(member.to_owned(), format!("{:x}", Sha256::digest(&bytes)));
    }
    let libraries = r#"{"schema":1,"arch":"aarch64-linux","loader":"runtime/sysroot/lib/ld-linux-aarch64.so.1","libraryPath":["runtime/sysroot/lib"]}"#;
    write(&root.join("runtime/arm-libraries.json"), libraries);
    files.insert(
        "runtime/arm-libraries.json".into(),
        format!("{:x}", Sha256::digest(libraries.as_bytes())),
    );
    let manifest = serde_json::json!({
        "schema": 1, "target": "aarch64-apple-darwin", "elfuse": ELFUSE_REV, "nixpkgs": NIXPKGS_REV,
        "runner": "runtime/bin/elfuse", "sysroot": "runtime/sysroot", "library_manifest": "runtime/arm-libraries.json",
        "audio_tools": "runtime/audio", "audio_modules": "runtime/audio/modules", "files": files
    });
    write(
        &root.join("runtime/manifest.json"),
        &serde_json::to_string(&manifest).unwrap(),
    );
    root.join("bin/myx")
}

// Format metadata only: no executable code, guest program or native helper.
fn mach_metadata() -> Vec<u8> {
    let mut bytes = vec![0; 32];
    bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    bytes[4..8].copy_from_slice(&0x0100000c_u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&2_u32.to_le_bytes());
    bytes
}

fn elf_metadata() -> Vec<u8> {
    let mut bytes = vec![0; 320];
    bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
    bytes[18..20].copy_from_slice(&183_u16.to_le_bytes());
    bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
    bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&2_u16.to_le_bytes());
    bytes[64..68].copy_from_slice(&1_u32.to_le_bytes());
    bytes[96..104].copy_from_slice(&320_u64.to_le_bytes());
    bytes[120..124].copy_from_slice(&2_u32.to_le_bytes());
    bytes[128..136].copy_from_slice(&176_u64.to_le_bytes());
    bytes[152..160].copy_from_slice(&64_u64.to_le_bytes());
    for (index, (tag, value)) in [(5_u64, 256_u64), (10, 9), (29, 1), (0, 0)]
        .into_iter()
        .enumerate()
    {
        let at = 176 + index * 16;
        bytes[at..at + 8].copy_from_slice(&tag.to_le_bytes());
        bytes[at + 8..at + 16].copy_from_slice(&value.to_le_bytes());
    }
    bytes[256..265].copy_from_slice(b"\0$ORIGIN\0");
    bytes
}

#[test]
fn user_assets_and_output_cannot_write_into_the_installed_bundle() {
    let (_temp, parent) = private_root();
    let root = parent.join("installed");
    let executable = bundle(&root);
    let paths = discover_from_exe(&executable).unwrap().unwrap();
    let alias = parent.join("bundle-alias");
    symlink(&root, &alias).unwrap();
    let nested_alias = parent.join("runtime-alias");
    symlink(root.join("runtime"), &nested_alias).unwrap();
    for path in [
        root.clone(),
        root.join("runtime/new-state"),
        alias.join("new-state/cache"),
        parent.join("missing/../installed/new-state"),
        nested_alias.join("../new-state/cache"),
    ] {
        assert!(paths.ensure_external_user_path(&path).is_err());
    }
    let external = parent.join("user-state/cache");
    paths.ensure_external_user_path(&external).unwrap();
    assert!(!external.exists(), "validation must not create state");
    assert!(!root.join("new-state").exists());
}

#[test]
#[ignore = "requires an explicit owned copied release path; static read-only inspection, not native playback"]
fn actual_copied_release_passes_the_production_bundle_reader() {
    let executable = PathBuf::from(
        std::env::var_os("MYX_TEST_COPIED_BUNDLE")
            .expect("MYX_TEST_COPIED_BUNDLE must point to the owned copied bin/myx"),
    );
    assert!(executable.is_absolute());
    let discovered = discover_from_exe(&executable)
        .expect("copied bundle validation failed")
        .expect("copied bundle manifest required");
    assert_eq!(
        discovered.root,
        executable.parent().unwrap().parent().unwrap()
    );
    assert!(!discovered
        .library_path
        .split(':')
        .any(|path| path.starts_with("/nix/store/")));
    assert!(discovered.root.join("sources/inventory.json").is_file());
}

#[test]
fn bundle_move_digest_pin_symlink_and_traversal_guards() {
    let (_temp, parent) = private_root();
    let root = parent.join("installed");
    let executable = bundle(&root);
    let paths = discover_from_exe(&executable).unwrap().unwrap();
    assert_eq!(paths.runner, root.join("runtime/bin/elfuse"));
    assert_eq!(
        paths.library_path,
        root.join("runtime/sysroot/lib").to_str().unwrap()
    );
    let moved = parent.join("copied");
    fs::rename(&root, &moved).unwrap();
    assert_eq!(
        discover_from_exe(&moved.join("bin/myx"))
            .unwrap()
            .unwrap()
            .root,
        moved
    );
    let manifest_path = moved.join("runtime/manifest.json");
    let original = fs::read_to_string(&manifest_path).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_str(&original).unwrap();
    manifest["elfuse"] = "wrong".into();
    write(&manifest_path, &manifest.to_string());
    assert!(discover_from_exe(&moved.join("bin/myx")).is_err());
    manifest["elfuse"] = ELFUSE_REV.into();
    manifest["files"]["../outside"] = "0".repeat(64).into();
    write(&manifest_path, &manifest.to_string());
    assert!(discover_from_exe(&moved.join("bin/myx")).is_err());
    write(&manifest_path, &original);
    let runner = moved.join("runtime/bin/elfuse");
    write(&runner, "modified");
    assert!(discover_from_exe(&moved.join("bin/myx")).is_err());
    fs::write(&runner, mach_metadata()).unwrap();
    fs::remove_file(&runner).unwrap();
    symlink(moved.join("bin/myx"), &runner).unwrap();
    assert!(discover_from_exe(&moved.join("bin/myx")).is_err());
}
