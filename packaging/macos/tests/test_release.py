"""Owned-fixture/pure packaging scenarios. Do not run before primary's batch."""
import importlib.util
from pathlib import Path
import plistlib
import struct
import tempfile
import unittest
from unittest.mock import patch
from xml.sax.saxutils import escape

SPEC = importlib.util.spec_from_file_location("release", Path(__file__).parents[1] / "release.py")
assert SPEC is not None and SPEC.loader is not None
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class Paths(unittest.TestCase):
    def test_sdk_compiled_ca_bundle_defaults_are_permitted_runtime_assets(self):
        for name in ("etc/ssl/cert.pem", "etc/ssl/certs/ca-certificates.crt",
                     "etc/ssl/ca-bundle.pem", "etc/pki/tls/cacert.pem",
                     "etc/pki/tls/certs/ca-bundle.crt", "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem"):
            with self.subTest(name=name):
                self.assertTrue(release.allowed_member("runtime/sysroot/" + name))

    def test_sdk_ca_aliases_use_identical_public_certificate_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root/"certificates/etc/ssl/certs/ca-bundle.crt"
            source.parent.mkdir(parents=True)
            content = b"-----BEGIN CERTIFICATE-----\nPUBLIC-FIXTURE\n-----END CERTIFICATE-----\n"
            source.write_bytes(content)
            copied = {}
            class Assembly:
                inputs = {"certificates":str(root/"certificates")}
                def copy(self, src, member):
                    copied[member] = src.read_bytes()
            release.stage_ca_bundles(Assembly())
            self.assertEqual(set(copied),set(release.CA_BUNDLE_MEMBERS))
            self.assertTrue(all(value==content for value in copied.values()))
            source.write_bytes(b"-----BEGIN CERTIFICATE-----\nPRIVATE KEY\n")
            with self.assertRaises(ValueError):release.stage_ca_bundles(Assembly())

    def test_traversal_absolute_controls_and_user_assets(self):
        for value in ("", "/tmp/x", "../x", "a/../../x", "a/./b", "a//b", "a:b", "a\\b",
                      "line\nbreak", "runtime/sdk", "runtime/api-key", ".cache/token", "runtime/data/state"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                release.relative(value)
        self.assertEqual(str(release.relative("runtime/sysroot/lib/libc.so.6")),
                         "runtime/sysroot/lib/libc.so.6")
        for member in ("runtime/vendor-sdk", "private.toml", "runtime/sysroot/bin/vendor", "runtime/audio/bin/soloist"):
            self.assertFalse(release.allowed_member(member))
        self.assertTrue(release.allowed_member("sources/" + "a" * 64 + ".tar.gz"))
        self.assertTrue(release.allowed_member("sources/inventory.json"))
        self.assertFalse(release.allowed_member("sources/vendor/sdk"))
        self.assertFalse(release.allowed_member("sources/" + "🦀" * 17 + ".tar.gz"))

    def test_collision_never_clobbers(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary).resolve()
            parent.chmod(0o700)
            destination = parent / "release"
            destination.write_text("keep")
            with self.assertRaises(ValueError):
                release.safe_parent(destination)
            self.assertEqual(destination.read_text(), "keep")

    def test_symlink_parent_is_refused(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary).resolve()
            actual = parent / "actual"
            actual.mkdir(mode=0o700)
            alias = parent / "alias"
            alias.symlink_to(actual, target_is_directory=True)
            with self.assertRaises(ValueError):
                release.safe_parent(alias / "release")

    def test_stage_copy_does_not_replace_an_existing_physical_destination(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            package = root / "public-output"
            package.mkdir()
            source = package / "libfixture.dylib"
            source.write_bytes(b"public fixture")
            staged = root / "staged"
            target = staged / "runtime/audio/lib/libfixture.dylib"
            target.parent.mkdir(parents=True)
            target.write_bytes(b"preserve existing member")
            assembly = release.Stage(staged, {"closure": [str(package)], "releaseTools": str(package)})
            with patch.object(release, "store_root", return_value=package):
                with self.assertRaises(FileExistsError):
                    assembly.copy(source, "runtime/audio/lib/libfixture.dylib")
            self.assertEqual(target.read_bytes(), b"preserve existing member")

    def test_public_elf_closure_and_loader_are_copied_without_binary_rewriting(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            package = base / "public-package"
            (package / "lib").mkdir(parents=True)
            data = bytearray(80)
            data[:6] = b"\x7fELF\x02\x01"
            data[18:20] = (183).to_bytes(2, "little")
            data[32:] = b"original loader self-relocation bytes".ljust(48, b"\0")
            loader = package / "lib/ld-linux-aarch64.so.1"
            loader.write_bytes(data)
            manifest = base / "libraries.json"
            import json
            manifest.write_text(json.dumps({"arch":"aarch64-linux","elfuse":release.ELFUSE,
                "nixpkgs":release.NIXPKGS,"loader":str(loader),"libraryPath":str(package/"lib")}))
            root = base / "staged"
            assembly = release.Stage(root, {"closure":[str(package)],"libraries":str(manifest),"releaseTools":str(base)})
            def invoke(*args):
                self.assertEqual(args[:3], ("nix", "path-info", "--recursive"), "ELF rewriting must not run")
                return str(package) + "\n"
            def member(path):
                return "runtime/sysroot/nix/store/public-package/" + str(Path(path).relative_to(package))
            with patch.object(release,"store_root",return_value=package), patch.object(release,"guest_library_member",side_effect=member), patch.object(release,"run",side_effect=invoke):
                assembly.linux()
            self.assertEqual((root/"runtime/sysroot/lib/ld-linux-aarch64.so.1").read_bytes(), bytes(data))
            self.assertEqual((root/member(loader)).read_bytes(), bytes(data))

    def test_native_iconv_uses_system_converter_instead_of_unrelocatable_modules(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            package = base / "public-package"
            package.mkdir()
            source = package / "libfixture.dylib"
            iconv = package / "libiconv.2.dylib"
            for path in (source, iconv):
                path.write_bytes(struct.pack("<IIII", 0xFEEDFACF, 0x0100000C, 0, 6))
            staged = base / "staged"
            assembly = release.Stage(staged, {"closure": [str(package)], "releaseTools": str(package)})
            edits = []

            def invoke(*args):
                if args[:2] == ("/usr/bin/lipo", "-archs"):
                    return "arm64\n"
                if args[:2] == ("/usr/bin/otool", "-L"):
                    path = Path(args[2])
                    text = f"{path}:\n\t{path} (compatibility version 1.0.0, current version 1.0.0)\n"
                    if path == source:
                        text += "\t@loader_path/libiconv.2.dylib (compatibility version 7.0.0, current version 7.0.0)\n"
                    return text
                if args[:2] == ("/usr/bin/otool", "-l"):
                    return ""
                self.assertEqual(args[0], "/usr/bin/install_name_tool")
                edits.append(args[1:])
                return ""

            with patch.object(release, "store_root", return_value=package), patch.object(release, "run", side_effect=invoke):
                target = assembly.add_mach(source, "runtime/audio/lib/libfixture.dylib")
            self.assertIn(("-change", "@loader_path/libiconv.2.dylib", "/usr/lib/libiconv.2.dylib", target), edits)
            self.assertFalse((staged / "runtime/audio/lib/libiconv.2.dylib").exists())


class Metadata(unittest.TestCase):
    def test_guest_namespace_preserves_same_basename_without_host_store_paths(self):
        first = Path("/nix/store/first-gcc/lib/libgcc_s.so.1")
        second = Path("/nix/store/second-gcc/lib/libgcc_s.so.1")
        self.assertNotEqual(release.guest_library_member(first), release.guest_library_member(second))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            binary = root / release.guest_library_member(first)
            binary.parent.mkdir(parents=True)
            dependency = root / release.guest_library_member(second)
            dependency.parent.mkdir(parents=True)
            dependency.write_bytes(b"fixture")
            target = "$ORIGIN/../../second-gcc/lib/libgcc_s.so.1"
            self.assertEqual(release.origin_target(root, binary, target), dependency)
            with self.assertRaises(ValueError):
                release.origin_target(root, binary, "$ORIGIN/../../../../../../../../outside")
            with self.assertRaises(ValueError):
                release.origin_target(root, binary, "/nix/store/host/lib/libgcc_s.so.1")

    def test_pinned_pulse_module_layout_and_shared_protocol_libraries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            modules = root / "lib/pulseaudio/modules"
            modules.mkdir(parents=True)
            for name in release.PULSE_MODULES:
                (modules / name).write_bytes(b"fixture")
            self.assertEqual(release.pulse_modules(root), modules.resolve())
            self.assertTrue(release.allowed_member("runtime/audio/lib/libprotocol-native.so"))
            (modules / release.PULSE_MODULES[0]).unlink()
            with self.assertRaises(ValueError):
                release.pulse_modules(root)

    def test_macho_dependency_and_rpath_parsing(self):
        text = "binary:\n\t/nix/store/hash-pkg/lib/liba.dylib (compatibility version 1.0.0, current version 1.0.0)\n\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1.0.0)\n"
        self.assertEqual(release.macho_deps(text), ["/nix/store/hash-pkg/lib/liba.dylib", "/usr/lib/libSystem.B.dylib"])
        text = "Load command 7\n          cmd LC_RPATH\n      cmdsize 48\n         path /nix/store/hash/lib (offset 12)\n"
        self.assertEqual(release.macho_rpaths(text), ["/nix/store/hash/lib"])

    def test_elf_interpreter_bounds(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "libc.so.6"
            data = bytearray(160)
            data[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<Q", data, 32, 64)
            struct.pack_into("<HH", data, 54, 56, 1)
            struct.pack_into("<I", data, 64, 3)
            interpreter = b"/lib/ld-linux-aarch64.so.1\0"
            struct.pack_into("<Q", data, 72, 128)
            struct.pack_into("<Q", data, 96, len(interpreter))
            data[128:128 + len(interpreter)] = interpreter
            path.write_bytes(data)
            self.assertEqual(release.elf_interpreter(path), "/lib/ld-linux-aarch64.so.1")
            struct.pack_into("<Q", data, 72, 2**64 - 1)
            path.write_bytes(data)
            with self.assertRaises(ValueError):
                release.elf_interpreter(path)

    def test_launchagent_template_xml_quoting(self):
        template = (Path(__file__).parents[1] / "io.myx.daemon.plist.in").read_text()
        executable = "/Applications/My & <Player>/bin/myx"
        config = "/Users/test/a'\"&.toml"
        for key, value in {"EXECUTABLE": executable, "CONFIG": config,
                           "STDOUT": "/Users/test/log/out", "STDERR": "/Users/test/log/err"}.items():
            template = template.replace(f"@{key}_XML@", escape(value, {'"': "&quot;", "'": "&apos;"}))
        definition = plistlib.loads(template.encode())
        self.assertEqual(definition["ProgramArguments"], [executable, "daemon", "run", "--config", config])
        self.assertEqual(definition["KeepAlive"], {"SuccessfulExit": False})
        self.assertEqual(definition["EnvironmentVariables"], {"MYX_SUPERVISED": "1"})


if __name__ == "__main__":
    unittest.main()
