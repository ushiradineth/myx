#!/usr/bin/env python3
"""Explicit Mac release assembly. Never execute a staged helper or application.

Run via `nix develop -c python3 packaging/macos/release.py ...`.
build compiles public dependencies; stage also fetches fixed-output sources.
No SDK/key/config/data input exists in this interface.
"""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import plistlib
import re
import shutil
import struct
import subprocess
import tarfile
import importlib.util

_SOURCE_SPEC = importlib.util.spec_from_file_location("myx_release_sources", Path(__file__).with_name("sources.py"))
assert _SOURCE_SPEC is not None and _SOURCE_SPEC.loader is not None
source_materials = importlib.util.module_from_spec(_SOURCE_SPEC)
_SOURCE_SPEC.loader.exec_module(source_materials)

ROOT = Path(__file__).resolve().parents[2]
ELFUSE = "ba455f45592cd12488519e207c44c32fa8e167df"
NIXPKGS = "0954f7ee2f6bb3dc7d4e3d0d8bcb8fd4bde4cfc5"
SYSTEM = ("/usr/lib/", "/System/Library/")
MACH = (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf")
PULSE_MODULES = ("module-coreaudio-device.so", "module-null-sink.so",
                 "module-native-protocol-tcp.so", "module-native-protocol-unix.so")
CA_BUNDLE_MEMBERS = tuple("runtime/sysroot/" + name for name in (
    "etc/ssl/cert.pem", "etc/ssl/certs/ca-certificates.crt", "etc/ssl/ca-bundle.pem",
    "etc/pki/tls/cacert.pem", "etc/pki/tls/certs/ca-bundle.crt",
    "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem"))
FORBIDDEN = {".git", ".agents", ".runtime", ".cache", "cache", "data", "sdk",
             "soloist", "api-key", "credentials", "credentials.json", "vm", "dump"}


def require(value, message):
    if not value:
        raise ValueError(message)


def run(*args):
    return subprocess.check_output([str(a) for a in args], text=True,
                                   stdin=subprocess.DEVNULL, timeout=900,
                                   env={**os.environ, "CI": "true", "GIT_TERMINAL_PROMPT": "0"})


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def relative(value):
    require(isinstance(value, str) and value and "\\" not in value and ":" not in value
            and not any(ord(c) < 32 or ord(c) == 127 for c in value), "unsafe relative path")
    path = PurePosixPath(value)
    require(not path.is_absolute() and all(p not in ("", ".", "..") for p in value.split("/")),
            "unsafe relative path")
    require(not any(p.lower() in FORBIDDEN for p in path.parts), "excluded runtime/user asset")
    return path


def allowed_member(member):
    return (member in {"bin/myx", "LICENSE", "NOTICE", "runtime/bin/elfuse", "runtime/arm-libraries.json",
                       "runtime/audio/bin/pulseaudio", "runtime/audio/bin/pactl", "runtime/sysroot/etc/ssl/cert.pem"}
            or member.startswith("licenses/")
            or member in CA_BUNDLE_MEMBERS
            or (member.startswith("sources/") and len(PurePosixPath(member).parts) == 2
                and (member == "sources/inventory.json"
                     or re.fullmatch(r"sources/[0-9a-f]{64}(?:[.-][A-Za-z0-9._-]+)", member)))
            or (member.startswith("runtime/audio/lib/") and member.endswith((".dylib", ".so")))
            or (member.startswith("runtime/audio/modules/") and member.endswith(".so"))
            or (member.startswith(("runtime/sysroot/lib/", "runtime/sysroot/nix/store/"))
                and ".so" in Path(member).name))


def safe_parent(destination):
    require(destination.is_absolute(), "destination must be absolute")
    require(destination.name not in ("", ".", ".."), "invalid destination")
    parent = destination.parent
    require(parent.is_dir() and parent.resolve() == parent, "parent must exist without symlinks")
    info = parent.stat()
    require(info.st_uid == os.geteuid() and not info.st_mode & 0o022,
            "destination parent must be owned and not group/world writable")
    require(not os.path.lexists(destination), "destination already exists; never clobber")
    require(not str(destination).startswith("/nix/") and destination != ROOT
            and ROOT not in destination.parents, "destination must be outside checkout/store")


def store_root(path):
    path = Path(path)
    require(path.is_absolute() and path.parts[:3] == ("/", "nix", "store")
            and len(path.parts) >= 4, "dependency is not a declared Nix output")
    return Path(*path.parts[:4])


def guest_library_member(path):
    store_root(path)
    return "runtime/sysroot" + str(path)


def origin_target(root, binary, name):
    require(name == "$ORIGIN" or name.startswith("$ORIGIN/"), "nonrelocatable ELF library path")
    require(not any(c in name for c in ("\\", ":", "\0", "\n", "\r")), "unsafe ELF library path")
    target = (binary.parent / name.removeprefix("$ORIGIN").lstrip("/")).resolve()
    require(target.is_relative_to(root / "runtime/sysroot"), "ELF library path escapes private sysroot")
    return target


def guest_elf_target(root, binary, name):
    if name.startswith("/nix/store/") or name == "/lib/ld-linux-aarch64.so.1":
        # Guest-absolute names resolve inside this copied sysroot, never against
        # the host Nix store. Preserve the validated loader's original bytes.
        relative(name.lstrip("/"))
        return root / "runtime/sysroot" / name.lstrip("/")
    return origin_target(root, binary, name)


def macho_deps(text):
    # otool -L output, including a dylib's LC_ID (handled by caller).
    return [line.strip().split(" (compatibility version", 1)[0]
            for line in text.splitlines()[1:] if line.startswith("\t")]


def pulse_modules(audio):
    candidates = [audio / "lib/pulseaudio/modules", *sorted((audio / "lib").glob("pulse-*/modules"))]
    candidates = {path.resolve() for path in candidates
                  if all((path / name).is_file() for name in PULSE_MODULES)}
    if not candidates and all((audio / "lib/pulseaudio" / name).is_file() for name in PULSE_MODULES):
        candidates.add((audio / "lib/pulseaudio").resolve())
    require(len(candidates) == 1, "expected one complete private Pulse module directory")
    return candidates.pop()


def macho_rpaths(text):
    return re.findall(r"cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset \d+\)", text)


def kind(path):
    with path.open("rb") as stream:
        magic = stream.read(4)
    return "mach" if magic in MACH else "elf" if magic == b"\x7fELF" else "other"


def elf_interpreter(path):
    data = path.read_bytes()
    require(len(data) >= 64 and data[:6] == b"\x7fELF\x02\x01", "invalid ELF header")
    offset = struct.unpack_from("<Q", data, 32)[0]
    stride, count = struct.unpack_from("<HH", data, 54)
    require(stride == 56 and count <= 1024 and offset + count * stride <= len(data), "invalid ELF program headers")
    for index in range(count):
        at = offset + index * stride
        if struct.unpack_from("<I", data, at)[0] == 3:
            start = struct.unpack_from("<Q", data, at + 8)[0]
            size = struct.unpack_from("<Q", data, at + 32)[0]
            require(start + size <= len(data) and 1 <= size <= 4096, "invalid ELF interpreter")
            raw = data[start:start + size]
            require(raw.endswith(b"\0"), "unterminated ELF interpreter")
            return raw[:-1].decode("utf-8")
    return None


def json_write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def stage_ca_bundles(assembly):
    source = Path(assembly.inputs["certificates"]) / "etc/ssl/certs/ca-bundle.crt"
    require(source.stat().st_size <= 8 * 1024 * 1024, "CA bundle exceeds bound")
    data = source.read_bytes()
    require(b"-----BEGIN CERTIFICATE-----" in data and b"PRIVATE KEY" not in data,
            "expected a public certificate bundle, never private key material")
    for member in CA_BUNDLE_MEMBERS:
        assembly.copy(source, member)


def build(args):
    require(platform.system() == "Darwin" and platform.machine() == "arm64", "requires ARM64 Mac")
    safe_parent(args.output)
    require(json.loads((ROOT / "flake.lock").read_text())["nodes"]["nixpkgs"]["locked"]["rev"] == NIXPKGS,
            "update reviewed runtime pins before building")
    # Evaluate the actual source filter, including new public files, without
    # copying ignored SDK/user assets into a path-flake store snapshot.
    expression = (f"let root = builtins.toPath {source_materials.nix_string(ROOT)}; "
                  "locked = (builtins.fromJSON (builtins.readFile (root + \"/flake.lock\"))).nodes.nixpkgs.locked; "
                  "tree = builtins.fetchTree { inherit (locked) type owner repo rev narHash; }; "
                  "pkgs = import tree.outPath { system = \"aarch64-darwin\"; }; "
                  "outputs = (import (root + \"/flake.nix\")).outputs { self = {}; "
                  "nixpkgs = { legacyPackages.aarch64-darwin = pkgs; lib = pkgs.lib; }; }; "
                  "in outputs.packages.aarch64-darwin.myx")
    app = run("nix-build", "--no-out-link", "--expr", expression).strip()
    outputs = {}
    for attribute in ("elfuse", "audio", "libraries", "certificates", "releaseTools"):
        outputs[attribute] = run("nix-build", "--no-out-link", ROOT / "nix/soloist.nix", "-A", attribute).strip()
    libraries = json.loads(Path(outputs["libraries"]).read_text())
    roots = [app, outputs["elfuse"], outputs["audio"], outputs["certificates"], str(store_root(libraries["loader"]))]
    roots += [str(store_root(p)) for p in libraries["libraryPath"].split(":")]
    closure = run("nix", "path-info", "--recursive", *roots).splitlines()
    inputs = {"schema": 1, "app": app, **outputs, "closure": sorted(set(closure)),
              "elfuse_pin": ELFUSE, "nixpkgs_pin": NIXPKGS}
    # Exclusive create, never replace an earlier build receipt.
    with os.fdopen(os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
        json.dump(inputs, stream, indent=2, sort_keys=True)
        stream.write("\n")


class Stage:
    def __init__(self, root, inputs):
        self.root = root
        self.inputs = inputs
        self.closure = {Path(p) for p in inputs["closure"]}
        self.used = set()
        self.mach = {}  # canonical source -> destination
        self.destinations = {}  # destination -> canonical source, reject collisions
        self.patchelf = Path(inputs["releaseTools"]) / "bin/patchelf"

    def copy(self, source, destination):
        source = source.resolve(strict=True)  # Nix-provided SONAME links are flattened.
        package = store_root(source)
        require(package in self.closure, "undeclared dependency output")
        require(source.is_file(), "only regular dependency files can be staged")
        self.used.add(str(package))
        destination = self.root / relative(destination)
        previous = self.destinations.get(destination)
        require(previous is None or previous == source, "dependency filename collision")
        if previous is None:
            destination.parent.mkdir(parents=True, exist_ok=True)
            # Also reject collisions that the host filesystem treats as equal
            # (for example case-folded names), never silently replace a member.
            with source.open("rb") as src, destination.open("xb") as dst:
                shutil.copyfileobj(src, dst, 65536)
            destination.chmod(0o755 if source.stat().st_mode & 0o111 else 0o644)
            self.destinations[destination] = source
        return destination

    def add_mach(self, source, destination):
        source = source.resolve(strict=True)
        if source in self.mach:
            return self.mach[source]
        target = self.copy(source, destination)
        require(kind(target) == "mach", "expected thin ARM64 Mach-O")
        require(run("/usr/bin/lipo", "-archs", target).strip() == "arm64", "non ARM64 Mach-O")
        self.mach[source] = target
        deps = macho_deps(run("/usr/bin/otool", "-L", source))
        dylib = int.from_bytes(target.read_bytes()[12:16], "little") == 6
        if dylib:
            # otool -L puts LC_ID_DYLIB first. It is an identity, not an import;
            # @rpath IDs need not be resolvable through this file's own LC_RPATH.
            deps = deps[1:]
        rpaths = macho_rpaths(run("/usr/bin/otool", "-l", source))
        for dep in deps:
            if dep.startswith(SYSTEM):
                continue
            dependency = self.resolve_mach(source, dep, rpaths)
            if dependency == source:
                continue
            # Apple's Nix libiconv dlopens Citrus modules and data at compiled
            # store paths. macOS supplies the same iconv ABI without that closure.
            if dependency.name == "libiconv.2.dylib":
                run("/usr/bin/install_name_tool", "-change", dep, "/usr/lib/libiconv.2.dylib", target)
                continue
            relocated = self.add_mach(dependency, "runtime/audio/lib/" + dependency.name)
            install = "@loader_path/" + os.path.relpath(relocated, target.parent)
            run("/usr/bin/install_name_tool", "-change", dep, install, target)
        # All non-system imports now loader-relative. Remove compiled host paths.
        for rpath in rpaths:
            run("/usr/bin/install_name_tool", "-delete_rpath", rpath, target)
        if dylib:  # MH_DYLIB, not MH_BUNDLE
            run("/usr/bin/install_name_tool", "-id", "@loader_path/" + target.name, target)
        return target

    def resolve_mach(self, source, value, rpaths):
        if value.startswith("/nix/store/"):
            resolved = Path(value)
        elif value.startswith("@loader_path/"):
            resolved = source.parent / value.removeprefix("@loader_path/")
        elif value.startswith("@rpath/"):
            candidates = [Path(p.replace("@loader_path", str(source.parent))) / value.removeprefix("@rpath/")
                          for p in rpaths if not p.startswith("@executable_path")]
            found = {p.resolve() for p in candidates if p.is_file()}
            require(len(found) == 1, "unresolved/ambiguous Mach-O @rpath import")
            resolved = found.pop()
        else:
            raise ValueError("unresolved Mach-O dependency: " + value)
        resolved = resolved.resolve(strict=True)
        require(store_root(resolved) in self.closure, "Mach-O dependency outside declared closure")
        return resolved

    def linux(self):
        libraries = json.loads(Path(self.inputs["libraries"]).read_text())
        require(libraries["arch"] == "aarch64-linux" and libraries["elfuse"] == ELFUSE
                and libraries["nixpkgs"] == NIXPKGS, "Linux ABI pin mismatch")
        # Only the dependency closure of these public library roots is considered.
        roots = {store_root(libraries["loader"])}
        roots.update(store_root(p) for p in libraries["libraryPath"].split(":"))
        linux_closure = {Path(p) for p in run("nix", "path-info", "--recursive", *sorted(roots)).splitlines()}
        require(linux_closure <= self.closure, "Linux closure changed since build receipt")
        staged = []
        for package in sorted(linux_closure):
            lib = package / "lib"
            if not lib.is_dir():
                continue
            # Public shared objects only; no binaries, VM images, configs or SDK.
            for source in sorted(lib.rglob("*")):
                if source.is_file() and (".so" in source.name) and kind(source) == "elf":
                    target = self.copy(source, guest_library_member(source))
                    staged.append((source, target))
        require(staged, "empty Linux ABI closure")
        loader = self.copy(Path(libraries["loader"]), "runtime/sysroot/lib/ld-linux-aarch64.so.1")
        staged.append((Path(libraries["loader"]), loader))
        for source, path in staged:
            with source.open("rb") as stream:
                header = stream.read(20)
            require(header[4:6] == b"\x02\x01" and int.from_bytes(header[18:20], "little") == 183,
                    "Linux closure must be little-endian ELF64 AArch64")
            # Rewriting glibc's dynamic loader can break its early self relocation
            # under ELFUSE before the SDK starts. The private guest namespace
            # contains original absolute references, so no ELF rewriting is needed.
            require(digest(source) == digest(path), "public ELF runtime bytes changed during copy")
        json_write(self.root / "runtime/arm-libraries.json", {
            "schema": 1, "arch": "aarch64-linux", "loader": "runtime/sysroot/lib/ld-linux-aarch64.so.1",
            "libraryPath": [guest_library_member(Path(value)) for value in libraries["libraryPath"].split(":")]})

    def license_files(self):
        source_materials.include_sources(self.root, self.used, self.inputs, run, require)


def stage(args):
    require(platform.system() == "Darwin" and platform.machine() == "arm64", "requires ARM64 Mac")
    safe_parent(args.output)
    inputs = json.loads(args.inputs.read_text())
    require(inputs["schema"] == 1 and inputs["elfuse_pin"] == ELFUSE and inputs["nixpkgs_pin"] == NIXPKGS,
            "build input pin mismatch")
    # Atomically claim a new directory; failure leaves this owned partial staging
    # directory for inspection. Never delete/replace a caller's directory.
    args.output.mkdir(mode=0o700)
    assembly = Stage(args.output, inputs)
    audio = Path(inputs["audio"])
    # Reserve the public libloading entry point before transitive imports pick a
    # versioned filename. All aliases in this layout are physical regular files.
    assembly.add_mach(audio / "lib/libpulse.0.dylib", "runtime/audio/lib/libpulse.0.dylib")
    assembly.add_mach(Path(inputs["app"]) / "bin/myx", "bin/myx")
    runner = assembly.add_mach(Path(inputs["elfuse"]) / "bin/elfuse", "runtime/bin/elfuse")
    for name in ("pulseaudio", "pactl"):
        assembly.add_mach(audio / "bin" / name, "runtime/audio/bin/" + name)
    modules = pulse_modules(audio)
    for name in PULSE_MODULES:
        assembly.add_mach(modules / name, "runtime/audio/modules/" + name)
    assembly.linux()
    stage_ca_bundles(assembly)
    (args.output / "licenses").mkdir()
    for source, name in [
        (Path(inputs["elfuse"]) / "share/licenses/elfuse/LICENSE", "ELFUSE-APACHE-2.0"),
        (ROOT / "nix/compat/nixpkgs-LICENSE", "NIXPKGS-MIT"),
    ]:
        shutil.copyfile(source, args.output / "licenses" / name)
    notice = Path(inputs["elfuse"]) / "share/licenses/elfuse/NOTICE"
    if notice.is_file():
        shutil.copyfile(notice, args.output / "licenses/ELFUSE-NOTICE")
    for name in ("LICENSE", "NOTICE"):
        shutil.copyfile(ROOT / name, args.output / name)
    # Codesign after every loader edit. No security policy changes, Developer ID,
    # or OS feature enabling. ELFUSE must retain the Hypervisor entitlement.
    for target in sorted(set(assembly.mach.values())):
        flags = ["--entitlements", ROOT / "packaging/macos/elfuse-entitlements.plist"] if target == runner else []
        run("/usr/bin/codesign", "--force", "--sign", "-", "--timestamp=none", *flags, target)
    inspect_runtime(args.output, assembly.patchelf)
    assembly.license_files()
    files = {}
    for path in sorted(args.output.rglob("*")):
        if path.is_file():
            path.chmod(0o555 if path.stat().st_mode & 0o111 else 0o444)
            files[str(path.relative_to(args.output))] = digest(path)
    json_write(args.output / "runtime/manifest.json", {
        "schema": 1, "target": "aarch64-apple-darwin", "elfuse": ELFUSE, "nixpkgs": NIXPKGS,
        "runner": "runtime/bin/elfuse", "sysroot": "runtime/sysroot",
        "library_manifest": "runtime/arm-libraries.json", "audio_tools": "runtime/audio",
        "audio_modules": "runtime/audio/modules", "files": files})
    (args.output / "runtime/manifest.json").chmod(0o444)
    verify_bundle(args.output, assembly.patchelf)
    for path in sorted(args.output.rglob("*"), reverse=True):
        if path.is_dir():
            path.chmod(0o555)
    args.output.chmod(0o555)


def verify_native_member(root, path, patchelf, libraries, signatures=True):
    if kind(path) == "mach":
        require(run("/usr/bin/lipo", "-archs", path).strip() == "arm64", "Mach architecture")
        for dep in macho_deps(run("/usr/bin/otool", "-L", path)):
            if dep.startswith(SYSTEM):
                continue
            require(dep.startswith("@loader_path/"), "nonrelocatable Mach-O dependency: " + dep)
            resolved = (path.parent / dep.removeprefix("@loader_path/")).resolve()
            require(root in resolved.parents and resolved.is_file(), "unclosed Mach-O dependency")
        require(not macho_rpaths(run("/usr/bin/otool", "-l", path)), "unexpected Mach-O rpath")
        if signatures:
            run("/usr/bin/codesign", "--verify", "--strict", path)
    elif kind(path) == "elf":
        with path.open("rb") as stream:
            header = stream.read(20)
        require(header[4:6] == b"\x02\x01" and int.from_bytes(header[18:20], "little") == 183, "ELF architecture")
        search = [guest_elf_target(root, path, entry) for entry in run(patchelf, "--print-rpath", path).strip().split(":") if entry]
        search += [root / relative(entry) for entry in libraries["libraryPath"]]
        interpreter = elf_interpreter(path)
        require(interpreter is None or guest_elf_target(root, path, interpreter).is_file(), "ELF interpreter")
        for dep in run(patchelf, "--print-needed", path).splitlines():
            if dep.startswith(("$ORIGIN/", "/")):
                require(guest_elf_target(root, path, dep).is_file(), "unclosed ELF dependency")
            else:
                require("/" not in dep and any((directory / dep).is_file() for directory in search), "unclosed ELF dependency: " + dep)


def inspect_runtime(root, patchelf, signatures=True):
    """Static runtime preflight, not source/license or native playback acceptance."""
    libraries = json.loads((root / "runtime/arm-libraries.json").read_text())
    count = 0
    for path in root.rglob("*"):
        member = str(path.relative_to(root))
        if path.is_file() and (member == "bin/myx" or member.startswith(("runtime/bin/", "runtime/audio/", "runtime/sysroot/lib/", "runtime/sysroot/nix/store/"))):
            require(not path.is_symlink(), "runtime symlink")
            require(allowed_member(member), "unexpected runtime member")
            expected = "elf" if member.startswith("runtime/sysroot/") else "mach"
            require(kind(path) == expected, "invalid runtime binary format")
            verify_native_member(root, path, patchelf, libraries, signatures)
            count += 1
    require(count > 0, "empty runtime preflight")
    return count


def verify_bundle(root, patchelf):
    require(root.is_absolute() and root.resolve() == root and not str(root).startswith("/nix/store/")
            and root != ROOT and ROOT not in root.parents,
            "verify a physical copied layout outside Nix")
    manifest = json.loads((root / "runtime/manifest.json").read_text())
    require(manifest["schema"] == 1 and manifest["target"] == "aarch64-apple-darwin"
            and manifest["elfuse"] == ELFUSE and manifest["nixpkgs"] == NIXPKGS, "manifest pin mismatch")
    roles = {"runner": "runtime/bin/elfuse", "sysroot": "runtime/sysroot",
             "library_manifest": "runtime/arm-libraries.json", "audio_tools": "runtime/audio",
             "audio_modules": "runtime/audio/modules"}
    require(set(manifest) == {"schema", "target", "elfuse", "nixpkgs", "files", *roles}, "unexpected manifest field")
    require(all(manifest[key] == value for key, value in roles.items()), "invalid manifest roles")
    require(0 < len(manifest["files"]) <= 20000, "invalid manifest inventory")
    libraries = json.loads((root / manifest["library_manifest"]).read_text())
    actual = set()
    for path in root.rglob("*"):
        require(not path.is_symlink(), "bundle symlink")
        relative(str(path.relative_to(root)))
        require(path.is_dir() or path.is_file(), "nonregular bundle entry")
        if not path.is_file() or path == root / "runtime/manifest.json":
            continue
        member = str(path.relative_to(root))
        require(allowed_member(member), "undeclared asset class in public archive")
        require(path.stat().st_nlink == 1 and not path.stat().st_mode & 0o6022, "unsafe bundle permissions/links")
        actual.add(member)
        require(manifest["files"].get(member) == digest(path), "member checksum mismatch")
        if member.startswith(("runtime/sysroot/lib/", "runtime/sysroot/nix/store/")):
            require(kind(path) == "elf", "invalid ELF runtime asset")
        elif member.startswith(("runtime/audio/", "runtime/bin/")) or member == "bin/myx":
            require(kind(path) == "mach", "invalid Mach-O runtime asset")
        elif member.startswith("licenses/"):
            require(path.stat().st_size <= 8 * 1024 * 1024, "license text exceeds limit")
            require(b"\0" not in path.read_bytes(), "invalid full license text")
        verify_native_member(root, path, patchelf, libraries)
    require(actual == set(manifest["files"]), "undeclared or missing bundle entries")
    for member in CA_BUNDLE_MEMBERS:
        require(member in manifest["files"], "missing SDK CA-bundle default path")
        require(manifest["files"][member] == manifest["files"][CA_BUNDLE_MEMBERS[0]],
                "SDK CA aliases must contain the same pinned public trust roots")
    source_materials.verify_sources(root, require)
    for key in ("runner", "sysroot", "library_manifest", "audio_tools", "audio_modules"):
        require((root / relative(manifest[key])).exists(), "missing manifest role")
    for member in ("bin/myx", "LICENSE", "NOTICE", "runtime/audio/bin/pulseaudio", "runtime/audio/bin/pactl",
                   "runtime/audio/lib/libpulse.0.dylib", "runtime/sysroot/etc/ssl/cert.pem",
                   "licenses/ELFUSE-APACHE-2.0", "licenses/NIXPKGS-MIT"):
        require(member in manifest["files"], "missing required public runtime asset: " + member)
    for module in ("module-coreaudio-device.so", "module-null-sink.so", "module-native-protocol-tcp.so", "module-native-protocol-unix.so"):
        require("runtime/audio/modules/" + module in manifest["files"], "missing required private Pulse module")
    entitlements = run("/usr/bin/codesign", "--display", "--entitlements", ":-", root / manifest["runner"])
    require(plistlib.loads(entitlements.encode()).get("com.apple.security.hypervisor") is True,
            "ELFUSE Hypervisor entitlement missing")
    libraries = json.loads((root / manifest["library_manifest"]).read_text())
    require(set(libraries) == {"schema", "arch", "loader", "libraryPath"}
            and libraries["schema"] == 1 and libraries["arch"] == "aarch64-linux"
            and libraries["loader"] == "runtime/sysroot/lib/ld-linux-aarch64.so.1"
            and 0 < len(libraries["libraryPath"]) <= 128
            and len(set(libraries["libraryPath"])) == len(libraries["libraryPath"])
            and all(value == "runtime/sysroot/lib" or value.startswith("runtime/sysroot/nix/store/")
                    for value in libraries["libraryPath"]), "library schema")
    require((root / relative(libraries["loader"])).is_file(), "loader missing")
    for item in libraries["libraryPath"]:
        require((root / relative(item)).is_dir(), "library directory missing")


def archive(args):
    safe_parent(args.output)
    verify_bundle(args.bundle, args.patchelf)
    # Stable ordering, timestamps and metadata; bytes depend on the signed build
    # inputs. The manifest hashes final signed/relocated bytes, not Nix originals.
    with args.output.open("xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive_file:
                for path in [args.bundle, *sorted(args.bundle.rglob("*"))]:
                    info = archive_file.gettarinfo(str(path), str(Path("myx-macos-aarch64") / path.relative_to(args.bundle)))
                    info.uid = info.gid = info.mtime = 0
                    info.uname = info.gname = ""
                    if path.is_file():
                        with path.open("rb") as stream:
                            archive_file.addfile(info, stream)
                    else:
                        archive_file.addfile(info)
    print(digest(args.output) + "  " + args.output.name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build_parser = commands.add_parser("build")
    build_parser.add_argument("--output", type=Path, required=True)
    stage_parser = commands.add_parser("stage")
    stage_parser.add_argument("--inputs", type=Path, required=True)
    stage_parser.add_argument("--output", type=Path, required=True)
    verify_parser = commands.add_parser("verify")
    archive_parser = commands.add_parser("archive")
    for sub in (verify_parser, archive_parser):
        sub.add_argument("--bundle", type=Path, required=True)
        sub.add_argument("--patchelf", type=Path, required=True)
    archive_parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "build":
        build(args)
    elif args.command == "stage":
        stage(args)
    elif args.command == "verify":
        verify_bundle(args.bundle, args.patchelf)
    else:
        archive(args)


if __name__ == "__main__":
    main()
