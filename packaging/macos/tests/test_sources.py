"""Owned fixtures only: no Nix commands, source execution, or live assets."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from typing import Literal, cast
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("macos_sources", Path(__file__).parents[1] / "sources.py")
assert SPEC is not None and SPEC.loader is not None
sources = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sources)


def require(value, message):
    if not value:
        raise ValueError(message)


class SourcesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.store = self.base / "store"
        self.store.mkdir()
        self.repo = self.base / "repo"
        self.repo.mkdir()
        locked = {"type": "github", "owner": "NixOS", "repo": "nixpkgs",
                  "rev": "a" * 40, "narHash": "sha256-fixture"}
        (self.repo / "flake.lock").write_text(json.dumps({"nodes": {"nixpkgs": {"locked": locked}}}))
        (self.repo / "flake.nix").write_text("# fixture recipe\n")
        (self.repo / "nix").mkdir()
        (self.repo / "nix/soloist.nix").write_text("# fixture runtime recipe\n")
        self.tree = self.store / "pin-source"
        self.tree.mkdir()
        (self.tree / "COPYING").write_text("Full fixture license text\n")
        self.addCleanup(patch.stopall)
        patch.object(sources, "STORE", self.store).start()
        patch.object(sources, "REPO", self.repo).start()

    def folder(self, name):
        path = self.store / name
        path.mkdir()
        (path / "LICENSE").write_text("Full fixture license text\n")
        (path / "code.c").write_text("/* public fixture source */\n")
        return path

    def bundle(self, name="bundle"):
        root = self.base / name
        root.mkdir()
        return root

    def tar(self, compression="gz", unsafe=False):
        path = self.store / ("upstream.tar." + compression)
        mode = cast(Literal["w:gz", "w:xz", "w:bz2"], "w:" + compression)
        with tarfile.open(path, mode) as archive:
            for name, data in [("upstream/COPYING", b"Full upstream license\n"),
                               ("upstream/NOTICE", b"Upstream notice\n")]:
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
            link = tarfile.TarInfo("upstream/link")
            link.type = tarfile.SYMTYPE
            link.linkname = "/private/never-read" if unsafe else "COPYING"
            archive.addfile(link)
        return path

    def package(self, path, root=None, **kw):
        root = root or self.bundle()
        return sources.package_material(path, root, require, sources.Budget(require), **kw)

    def test_tar_formats_are_inert_and_license_members_are_discovered(self):
        for compression in ("gz", "xz", "bz2"):
            with self.subTest(compression=compression):
                source = self.tar(compression, unsafe=True)
                root = self.bundle(compression)
                entry = self.package(source, root)
                self.assertEqual(entry["sha256"], sources.sha256(source))
                self.assertEqual(len(entry["licenses"]), 2)
                self.assertEqual((root / entry["archive"]).read_bytes(), source.read_bytes())
                self.assertFalse((root / "private").exists())

    def test_directory_is_deterministic_and_records_checksums(self):
        src = self.folder("source")
        (src / "relative").symlink_to("code.c")
        first, second = self.bundle("first"), self.bundle("second")
        a, b = self.package(src, first), self.package(src, second)
        self.assertEqual(a, b)
        self.assertEqual((first / a["archive"]).read_bytes(), (second / b["archive"]).read_bytes())
        with tarfile.open(first / a["archive"]) as archive:
            members = archive.getmembers()
            self.assertEqual([m.name for m in members], sorted(m.name for m in members))
            self.assertTrue(all(m.uid == m.gid == m.mtime == 0 for m in members))
            self.assertTrue(all(not m.issym() for m in members))
        for item in a["licenses"]:
            self.assertEqual(item["sha256"], sources.sha256(first / item["path"]))

    def test_host_symlink_escape_is_rejected(self):
        src = self.folder("escape")
        (src / "leak").symlink_to(self.repo / "flake.lock")
        with self.assertRaisesRegex(ValueError, "symlink.*escape|unapproved.*symlink"):
            self.package(src)

    def test_upstream_fixture_links_can_be_archived_without_dereferencing(self):
        src = self.folder("inert-links")
        (src / "missing").symlink_to("/private/never-read")
        root = self.bundle()
        entry = self.package(src, root, preserve_links=True)
        with tarfile.open(root / entry["archive"]) as archive:
            link = archive.getmember("missing")
            self.assertTrue(link.issym())
            self.assertEqual(link.linkname, "/private/never-read")

    def test_embedded_full_bsd_header_is_preserved_without_a_license_filename(self):
        src = self.store / "header-source"
        src.mkdir()
        header = b"/* Copyright (c) Fixture authors.\n * Redistribution and use in source and binary forms, with or without\n * modification, are permitted provided that this notice is retained.\n * THIS SOFTWARE IS PROVIDED AS IS, WITHOUT WARRANTY.\n */"
        (src / "code.c").write_bytes(header + b"\nint fixture;\n")
        root = self.bundle()
        material = self.package(src, root, need_license=True)
        self.assertEqual(len(material["licenses"]), 1)
        self.assertEqual((root / material["licenses"][0]["path"]).read_bytes(), header)

    def test_vendor_aliases_require_approval_and_each_crate_license(self):
        vendor = self.store / "vendor"
        vendor.mkdir()
        crate = self.folder("crate")
        (crate / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="1.0.0"\n')
        (vendor / "fixture-1.0.0").symlink_to(crate)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.package(vendor, self.bundle("denied"), vendor=True)
        result = self.package(vendor, self.bundle("allowed"), vendor=True,
                              approve_link=lambda path: path == crate)
        self.assertIn("fixture-1.0.0", result["vendor_licenses"])
        (crate / "LICENSE").unlink()
        with self.assertRaisesRegex(ValueError, "missing full license.*fixture-1.0.0"):
            self.package(vendor, self.bundle("missing-license"), vendor=True,
                         approve_link=lambda path: path == crate)

    def test_raw_source_requires_explicit_full_license_material(self):
        raw = self.store / "certdata.txt"
        raw.write_text("# Mozilla Public License URL only\nCERTIFICATE DATA\n")
        with self.assertRaisesRegex(ValueError, "missing full license"):
            self.package(raw, self.bundle("raw-missing"), need_license=True)
        record = self.package(raw, self.bundle("raw-ok"))
        self.assertEqual(record["format"], "raw")
        self.assertEqual(record["licenses"], [])

    def test_missing_source_and_no_clobber(self):
        with self.assertRaisesRegex(ValueError, "unavailable source"):
            self.package(self.store / "absent")
        src, root = self.folder("source"), self.bundle("owned")
        self.package(src, root)
        before = {p: p.read_bytes() for p in root.rglob("*") if p.is_file()}
        with self.assertRaisesRegex((ValueError, FileExistsError), "exist"):
            self.package(src, root)
        self.assertEqual(before, {p: p.read_bytes() for p in root.rglob("*") if p.is_file()})

    def test_symlink_cycle_and_bounded_files(self):
        src = self.folder("cycle")
        (src / "again").symlink_to(".")
        with self.assertRaisesRegex(ValueError, "cycle"):
            self.package(src)
        with patch.object(sources, "MAX_FILE", 4):
            with self.assertRaisesRegex(ValueError, "limit"):
                self.package(self.folder("large"), self.bundle("large-bundle"))

    def metadata_fixture(self, modern=False, source_exists=True):
        output = str(self.store / "output")
        drv = str(self.store / "output.drv")
        src = self.folder("app-source") if source_exists else self.store / "missing-source"
        vendor = self.store / "cargo-vendor-dir"
        vendor.mkdir()
        crate = vendor / "fixture-1.0"
        crate.mkdir()
        (crate / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="1.0"\n')
        (crate / "LICENSE-MIT").write_text("Full fixture MIT text\n")
        attrs = {"src": str(src), "patches": "", "cargoDeps": str(vendor)}
        body = {"env": attrs, "outputs": {"out": {"path": output}},
                "inputSrcs": [], "inputDrvs": {}}
        if modern:
            body = {"env": {}, "structuredAttrs": attrs,
                    "outputs": {"out": {"path": Path(output).name}},
                    "inputs": {"srcs": [], "drvs": {}}}
        self.calls = []

        def run(*args):
            args = tuple(map(str, args))
            self.calls.append(args)
            if args[:3] == ("nix-store", "--query", "--deriver"):
                if args[3] == output:
                    return drv
                return "unknown-deriver"
            if args[:3] == ("nix", "derivation", "show"):
                return json.dumps({"derivations": {Path(drv).name: body}, "version": 4} if modern else {drv: body})
            if args[:3] == ("nix", "eval", "--json"):
                return json.dumps({"sourcePath": str(self.tree), "narHash": "sha256-fixture"})
            self.fail("unexpected command: " + repr(args))
        return output, run

    def test_api_exact_output_coverage_and_material_checksums_old_and_new_metadata(self):
        for modern in (False, True):
            with self.subTest(modern=modern):
                # Each fixture uses a separate owned store.
                if modern:
                    self.tearDown()
                    self.setUp()
                output, run = self.metadata_fixture(modern)
                root = self.bundle()
                sources.include_sources(root, {output}, {"app": output, "nixpkgs_pin": "a" * 40}, run, require)
                inventory = json.loads((root / "sources/inventory.json").read_text())
                self.assertEqual(set(inventory["outputs"]), {output})
                roles = {inventory["materials"][key]["role"] for key in inventory["outputs"][output]["materials"]}
                self.assertTrue({"source", "vendor", "recipe", "nixpkgs"} <= roles)
                for entry in inventory["materials"].values():
                    self.assertEqual(sources.sha256(root / entry["archive"]), entry["sha256"])
                sources.verify_sources(root, require)
                self.assertFalse(any("--realise" in c for c in self.calls))
                with self.assertRaisesRegex(ValueError, "exist"):
                    sources.include_sources(root, {output}, {"app": output}, run, require)

    def test_api_missing_source_never_builds_runtime(self):
        output, run = self.metadata_fixture(source_exists=False)
        with self.assertRaisesRegex(ValueError, "unavailable source"):
            sources.include_sources(self.bundle(), {output}, {"app": output}, run, require)
        self.assertFalse(any("--realise" in c for c in self.calls))

    def test_legacy_cargo_dual_license_includes_both_standard_texts(self):
        output, original_run = self.metadata_fixture()
        crate = self.store / "cargo-vendor-dir/fixture-1.0"
        (crate / "LICENSE-MIT").unlink()
        (crate / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="1.0"\nlicense="MIT/Apache-2.0"\n')
        fetched = []

        def run(*args):
            if args[0] == "nix-prefetch-url":
                identifier = args[-1].rsplit('/', 1)[1]
                fetched.append(identifier)
                text = self.store / identifier
                text.write_text("Full standard license fixture: " + identifier)
                return "fixture-hash\n" + str(text) + "\n"
            return original_run(*args)

        root = self.bundle()
        sources.include_sources(root, {output}, {"app": output}, run, require)
        sources.verify_sources(root, require)
        self.assertEqual(fetched, ["MIT.txt", "Apache-2.0.txt"])

    def test_source_realization_requires_fixed_output_fetcher(self):
        path = self.store / "absent"
        drv = str(self.store / "fetch.drv")
        calls = []

        def run(*args):
            calls.append(args)
            if args[0] == "nix-store" and "--deriver" in args:
                return drv
            if args[:3] == ("nix", "derivation", "show"):
                return json.dumps({drv: {"env": {"url": "https://public.invalid/source"},
                                         "outputs": {"out": {"path": str(path)}}}})
            self.fail("arbitrary target realization: " + repr(args))
        metadata = sources.Metadata(run, require)
        with self.assertRaisesRegex(ValueError, "fixed-output fetcher"):
            metadata.ensure(path)
        self.assertEqual(len(calls), 2)

    def test_missing_cached_source_uses_only_its_parents_fixed_fetcher(self):
        source = self.store / "missing-source"
        drv = str(self.store / "fetch.drv")
        calls = []

        def run(*args):
            calls.append(args)
            if args[:3] == ("nix-store", "--query", "--deriver"):
                raise ValueError("unavailable cached output")
            if args[:3] == ("nix", "derivation", "show"):
                return json.dumps({"derivations": {Path(drv).name: {
                    "env": {"outputHash": "fixture-hash", "url": "https://public.invalid/source"},
                    "outputs": {"out": {"path": source.name}}
                }}, "version": 4})
            if args[:2] == ("nix-store", "--realise"):
                self.assertEqual(args[2], drv)
                source.write_text("public source fixture")
                return str(source)
            self.fail(repr(args))

        metadata = sources.Metadata(run, require)
        metadata.cache["parent"] = {"inputs": {"drvs": {Path(drv).name: ["out"]}}}
        self.assertEqual(metadata.ensure(source), source)
        self.assertEqual(len([call for call in calls if "--realise" in call]), 1)


if __name__ == "__main__":
    unittest.main()
