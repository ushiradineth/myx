"""Collect inert corresponding-source materials from the exact public Nix build.

This inventory is provenance, not a legal review or a written source offer. No
source code is executed. Missing inputs and missing full license texts fail shut.
"""
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import shutil
import stat
import subprocess
import tarfile
import tomllib

STORE = Path("/nix/store")
REPO = Path(__file__).resolve().parents[2]
MAX_FILE = 2 * 1024**3
MAX_TOTAL = 8 * 1024**3
MAX_TEXT = 8 * 1024**2
MAX_ENTRIES = 1_000_000
LICENSE_REFERENCE = "v3.27.0"
LICENSE_NAME = re.compile(r"^(?:licen[cs]e|copying|copyright|notice|patents)(?:[._-].*|\d*)$", re.I)


def sha256(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class Budget:
    def __init__(self, require):
        self.require, self.bytes, self.entries = require, 0, 0

    def add(self, size):
        self.require(0 <= size <= MAX_FILE, "source file size limit exceeded")
        self.bytes += size
        self.entries += 1
        self.require(self.bytes <= MAX_TOTAL and self.entries <= MAX_ENTRIES,
                     "source material budget limit exceeded")


def write_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("x", encoding="utf-8") as stream:
        json.dump(data, stream, ensure_ascii=False, sort_keys=True, indent=2)
        stream.write("\n")


def exclusive_copy(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    with source.open("rb") as src, destination.open("xb") as dst:
        shutil.copyfileobj(src, dst, 65536)


def store_root(path, require):
    path = Path(path)
    require(path.is_absolute() and path.is_relative_to(STORE)
            and len(path.relative_to(STORE).parts) > 0, "source must be a declared store input")
    return STORE / path.relative_to(STORE).parts[0]


def license_record(root, material, member, stream, size, require):
    require(0 < size <= MAX_TEXT, "license text size limit exceeded")
    data = stream.read(MAX_TEXT + 1)
    require(len(data) == size and b"\0" not in data, "invalid full license text")
    # Preserve the original bytes, including non-UTF8 historical notices.
    name = hashlib.sha256((material + "\0" + member).encode()).hexdigest() + ".txt"
    destination = root / "licenses" / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open("xb") as output:
        output.write(data)
    return {"path": str(destination.relative_to(root)), "member": member,
            "sha256": hashlib.sha256(data).hexdigest()}


def package_material(path, root, require, budget, *, need_license=False,
                     vendor=False, approve_link=None, supplement_license=None,
                     preserve_links=False):
    path, root = Path(path), Path(root)
    store_root(path, require)
    require(path.exists(), "unavailable source material: " + path.name)
    require(path.resolve().is_relative_to(STORE), "source symlink escape")
    key = hashlib.sha256(str(path).encode()).hexdigest()
    directory = path.is_dir()
    name = key + (".tar.gz" if directory else "-" + re.sub(r"[^A-Za-z0-9._-]", "_", path.name))
    destination = root / "sources" / name
    require(not os.path.lexists(destination), "source destination already exists")
    destination.parent.mkdir(parents=True, exist_ok=True)
    licenses, vendor_licenses = [], {}

    def note_license(member, stream, size):
        budget.add(size)
        record = license_record(root, key, member, stream, size, require)
        licenses.append(record)
        return record

    if directory:
        base = path.resolve()
        files, file_info, directories, links = {}, {}, set(), {}

        def walk(folder, relative, ancestors, approved, canonical=None):
            budget.add(0)
            canonical = folder.resolve(strict=True) if canonical is None else canonical
            require(canonical not in ancestors, "source symlink cycle")
            allowed = any(canonical.is_relative_to(parent) for parent in approved)
            require(allowed or (approve_link is not None and approve_link(canonical)),
                    "unapproved source symlink escape")
            directories.add(relative)
            approved = approved | {canonical}
            for child in sorted(folder.iterdir(), key=lambda p: p.name):
                member = relative / child.name
                metadata = child.lstat()
                symbolic = stat.S_ISLNK(metadata.st_mode)
                if preserve_links and symbolic:
                    # Preserve upstream rootfs/test-fixture links as inert tar
                    # entries. Never dereference their targets on the host.
                    budget.add(0)
                    links[member] = os.readlink(child)
                    continue
                # Ordinary entries are already under a checked immutable store
                # parent. Resolve only links, not every ancestor of every file.
                resolved = child.resolve(strict=True) if symbolic else canonical / child.name
                if symbolic:
                    require(any(resolved.is_relative_to(parent) for parent in approved)
                            or (approve_link is not None and approve_link(resolved)),
                            "unapproved source symlink escape")
                    metadata = child.stat()
                if stat.S_ISDIR(metadata.st_mode):
                    walk(child, member, ancestors | {canonical}, approved, resolved)
                else:
                    require(stat.S_ISREG(metadata.st_mode), "nonregular source material")
                    budget.add(metadata.st_size)
                    files[member] = child
                    file_info[member] = metadata

        walk(path, PurePosixPath(), set(), {base})
        if vendor:
            crates = sorted({member.parts[0] for member in files if len(member.parts) == 2
                             and member.name == "Cargo.toml"})
            require(crates, "missing vendored Cargo sources")
            for crate in crates:
                vendor_licenses[crate] = []
        with destination.open("xb") as raw:
            with gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as gz:
                with tarfile.open(fileobj=gz, mode="w|") as archive:
                    for member in sorted(set(files) | set(links) | (directories - {PurePosixPath()}), key=str):
                        info = tarfile.TarInfo(str(member))
                        info.uid = info.gid = info.mtime = 0
                        if member in files:
                            source = files[member]
                            info.mode = 0o755 if file_info[member].st_mode & 0o111 else 0o644
                            info.size = file_info[member].st_size
                            with source.open("rb") as stream:
                                archive.addfile(info, stream)
                            if info.size > 0 and LICENSE_NAME.fullmatch(member.name):
                                with source.open("rb") as stream:
                                    record = note_license(str(member), stream, info.size)
                                if member.parts[0] in vendor_licenses:
                                    vendor_licenses[member.parts[0]].append(record)
                        elif member in links:
                            info.type, info.mode, info.linkname = tarfile.SYMTYPE, 0o777, links[member]
                            archive.addfile(info)
                        else:
                            info.type, info.mode = tarfile.DIRTYPE, 0o755
                            archive.addfile(info)
        for crate, texts in vendor_licenses.items():
            if not texts and supplement_license is not None:
                manifest = tomllib.loads(files[PurePosixPath(crate) / "Cargo.toml"].read_text())
                texts.extend(supplement_license(manifest.get("package", {}).get("license", "")))
            require(texts, "missing full license for vendored crate " + crate)
        if not licenses and not vendor:
            # Some upstream trees (notably Apple's Citrus libiconv) carry full
            # BSD notices in source headers rather than a standalone LICENSE.
            for member, source in files.items():
                if member.suffix.lower() not in (".c", ".h", ".cc", ".cpp", ".rs", ".s"):
                    continue
                with source.open("rb") as stream:
                    header = stream.read(65536)
                for index, match in enumerate(re.finditer(rb"/\*.*?\*/", header, re.S)):
                    block = match.group()
                    normalized = re.sub(rb"\s*\*\s?", b" ", block).lower()
                    permission = b"redistribution and use in source and binary forms" in normalized or b"permission is hereby granted" in normalized
                    if b"copyright" in normalized and b"as is" in normalized and permission:
                        note_license(str(member) + "#header-" + str(index), io.BytesIO(block), len(block))
        material_format = "tar.gz"
    else:
        budget.add(path.stat().st_size)
        exclusive_copy(path, destination)
        material_format = "raw"
        if tarfile.is_tarfile(path):
            material_format = "upstream-tar"
            with tarfile.open(path, "r:*") as archive:
                for index, member in enumerate(archive):
                    require(index < MAX_ENTRIES, "source archive member limit exceeded")
                    # Inspect bytes only, never extract paths or follow links.
                    if member.isfile() and member.size > 0 and LICENSE_NAME.fullmatch(PurePosixPath(member.name).name):
                        stream = archive.extractfile(member)
                        if stream is None:
                            raise ValueError("unavailable license archive member")
                        with stream:
                            note_license(member.name, stream, member.size)
        elif path.stat().st_size > 0 and LICENSE_NAME.fullmatch(path.name):
            with path.open("rb") as stream:
                note_license(path.name, stream, path.stat().st_size)
    require(not need_license or licenses, "missing full license text for " + path.name)
    return {"archive": str(destination.relative_to(root)), "sha256": sha256(destination),
            "format": material_format, "licenses": licenses,
            "vendor_licenses": vendor_licenses}


class Metadata:
    def __init__(self, run, require):
        self.run, self.require, self.cache = run, require, {}

    def derivation(self, drv):
        drv = str(drv if Path(drv).is_absolute() else STORE / drv)
        if drv not in self.cache:
            value = json.loads(self.run("nix", "derivation", "show", drv))
            values = value.get("derivations", value)
            key = drv if drv in values else Path(drv).name
            self.require(key in values, "missing source derivation metadata")
            self.cache[drv] = values[key]
        body = self.cache[drv]
        if any(not value.get("path") for value in body["outputs"].values()):
            # Modern fixed-output metadata can omit the computable path.
            # Query declared outputs; this does not realize the derivation.
            declared = self.run("nix-store", "--query", "--outputs", drv).strip().splitlines()
            for name, value in body["outputs"].items():
                env_path = body.get("env", {}).get(name)
                if not value.get("path") and env_path in declared:
                    value["path"] = env_path
            if len(body["outputs"]) == 1 and len(declared) == 1:
                next(iter(body["outputs"].values())).setdefault("path", declared[0])
        return body

    def attrs(self, body):
        env = body.get("env", {})
        structured = body.get("structuredAttrs")
        if structured is None and env.get("__json"):
            structured = json.loads(env["__json"])
        return {**env, **(structured or {})}

    def output_path(self, value):
        if not value.get("path"):
            return None
        path = Path(value["path"])
        return path if path.is_absolute() else STORE / path

    def owner(self, path):
        root = store_root(path, self.require)
        # A substituted output may record an unavailable older deriver. Prefer
        # an exact output identity in the freshly evaluated pinned build graph.
        for drv, body in self.cache.items():
            if root in [self.output_path(value) for value in body.get("outputs", {}).values()]:
                return drv, body
        drv = self.run("nix-store", "--query", "--deriver", root).strip()
        self.require(drv.endswith(".drv") and drv != "unknown-deriver", "unavailable source derivation")
        body = self.derivation(drv)
        self.require(root in [self.output_path(v) for v in body["outputs"].values()],
                     "source derivation output mismatch")
        return drv, body

    def ensure(self, path, parent_body=None):
        path = Path(path)
        store_root(path, self.require)
        if path.exists():
            return path
        try:
            drv, body = self.owner(path)
        except (ValueError, OSError, subprocess.CalledProcessError):
            # Substituted binaries need not retain valid local source outputs.
            # Their immediate input derivations still bind the exact fetcher.
            wanted = store_root(path, self.require)
            match = None
            for parent in ([parent_body] if parent_body is not None else list(self.cache.values())):
                for candidate in parent.get("inputDrvs", parent.get("inputs", {}).get("drvs", {})):
                    child = self.derivation(candidate)
                    if wanted in [self.output_path(value) for value in child["outputs"].values()]:
                        match = (str(candidate if Path(candidate).is_absolute() else STORE / candidate), child)
                        break
                if match is not None:
                    break
            if match is None:
                raise ValueError("unavailable source material: " + path.name) from None
            drv, body = match
        attrs = self.attrs(body)
        fixed = bool(attrs.get("outputHash")) or any(v.get("hash") for v in body["outputs"].values())
        fetcher = bool(attrs.get("url") or attrs.get("urls") or attrs.get("repo"))
        self.require(fixed and fetcher, "source realization requires a fixed-output fetcher")
        self.run("nix-store", "--realise", drv)
        self.require(path.exists(), "unavailable source after fixed-output fetch")
        return path

    def approved_vendor_link(self, vendor_body, target):
        root = store_root(target, self.require)
        inputs = vendor_body.get("inputs", {})
        if any(str(root) == str(path if Path(path).is_absolute() else STORE / path)
               for path in vendor_body.get("inputSrcs", inputs.get("srcs", []))):
            return True
        for drv in vendor_body.get("inputDrvs", inputs.get("drvs", {})):
            if root in [self.output_path(v) for v in self.derivation(drv)["outputs"].values()]:
                return True
        return False


def nix_string(value):
    return json.dumps(str(value)).replace("${", "\\${")


def include_sources(root, outputs, inputs, run, require):
    root = Path(root)
    require(not os.path.lexists(root / "sources"), "source inventory already exists")
    metadata, budget = Metadata(run, require), Budget(require)
    for role in ("app", "elfuse", "audio", "libraries", "certificates"):
        if role not in inputs:
            continue
        drv, _ = metadata.owner(Path(inputs[role]))
        graph = json.loads(run("nix", "derivation", "show", "--recursive", drv))
        for key, body in graph.get("derivations", graph).items():
            key = str(key if Path(key).is_absolute() else STORE / key)
            metadata.cache.setdefault(key, body)
    inventory = {"schema": 1, "notice": "Source/license provenance, not a legal review or written source offer",
                 "outputs": {}, "materials": {}}
    cached = {}
    standard_texts = {}

    def add(path, role, parent=None, **kwargs):
        path = metadata.ensure(Path(path), parent)
        cache_key = (str(path), role)
        if cache_key not in cached:
            record = package_material(path, root, require, budget,
                                      preserve_links=role in ("source", "nixpkgs"), **kwargs)
            record["role"] = role
            key = hashlib.sha256((str(path) + "\0" + role).encode()).hexdigest()
            inventory["materials"][key] = record
            cached[cache_key] = key
        return cached[cache_key]

    def full_license(expression):
        # Older Cargo releases used slash-separated dual licenses for OR.
        if re.fullmatch(r"[A-Za-z0-9.+-]+(?:/[A-Za-z0-9.+-]+)+", expression):
            expression = expression.replace("/", " OR ")
        require(expression and not re.search(r"[^A-Za-z0-9.()+ -]", expression),
                "missing or unsupported declared source license")
        tokens = re.findall(r"[A-Za-z0-9.+-]+", expression)
        records, exception = [], False
        for identifier in tokens:
            if identifier in ("AND", "OR"):
                continue
            if identifier == "WITH":
                exception = True
                continue
            require(not identifier.startswith(("LicenseRef-", "DocumentRef-")),
                    "custom license requires its upstream full text")
            kind = "exceptions" if exception else "text"
            exception = False
            cache_key = (kind, identifier)
            if cache_key not in standard_texts:
                url = f"https://raw.githubusercontent.com/spdx/license-list-data/{LICENSE_REFERENCE}/{kind}/{identifier}.txt"
                lines = run("nix-prefetch-url", "--type", "sha256", "--print-path", url).strip().splitlines()
                require(len(lines) == 2, "license text fetch did not provide a hashed store input")
                path = Path(lines[1])
                store_root(path, require)
                key = add(path, "license")
                record = inventory["materials"][key]
                record["url"] = url
                record["reference"] = LICENSE_REFERENCE
                with path.open("rb") as stream:
                    text = license_record(root, key, identifier, stream, path.stat().st_size, require)
                record["licenses"] = [text]
                standard_texts[cache_key] = text
            records.append(standard_texts[cache_key])
        require(records, "missing full declared license text")
        return records

    for output in sorted(outputs):
        drv, body = metadata.owner(Path(output))
        attrs = metadata.attrs(body)
        require(attrs.get("src"), "missing exact source input for " + Path(output).name)
        materials = [add(attrs["src"], "source", parent=body)]
        source_record = inventory["materials"][materials[0]]
        patches = attrs.get("patches", [])
        if isinstance(patches, str):
            patches = shlex.split(patches)
        materials += [add(path, "patch", parent=body) for path in patches]
        vendor = attrs.get("cargoDeps") or attrs.get("cargoVendorDir")
        if vendor:
            _, vendor_body = metadata.owner(Path(vendor)) if not Path(vendor).is_dir() else (None, None)
            # Plain fixture/source directories need no link approval. Actual Nix
            # vendor links are admitted only from that vendor derivation's inputs.
            if vendor_body is None:
                try:
                    _, vendor_body = metadata.owner(Path(vendor))
                except ValueError:
                    vendor_body = {}
            materials.append(add(vendor, "vendor", vendor=True,
                                 approve_link=lambda target: metadata.approved_vendor_link(vendor_body, target),
                                 supplement_license=full_license))
        # Preserve build phases/patches and the exact dependency graph. No process
        # argv, credentials or host environment are queried by this collector.
        recipe_keys = {"pname", "version", "system", "src", "patches", "configureFlags", "cmakeFlags",
                       "mesonFlags", "makeFlags", "prePatch", "postPatch", "preConfigure", "postConfigure",
                       "configurePhase", "buildPhase", "installPhase", "preBuild", "postBuild", "postFixup",
                       "cargoDeps", "cargoVendorDir", "cargoBuildFlags", "cargoTestFlags"}
        recipe = {"derivation": Path(drv).name, "platform": body.get("system"),
                  "attributes": {k: v for k, v in attrs.items() if k in recipe_keys},
                  "inputs": body.get("inputDrvs", body.get("inputs", {}).get("drvs", {})),
                  "source_inputs": body.get("inputSrcs", body.get("inputs", {}).get("srcs", []))}
        key = hashlib.sha256((output + "\0recipe").encode()).hexdigest()
        target = root / "sources" / (key + ".json")
        write_json(target, recipe)
        inventory["materials"][key] = {"role": "recipe", "archive": str(target.relative_to(root)),
                                        "sha256": sha256(target), "licenses": []}
        materials.append(key)
        if not source_record["licenses"] and Path(attrs["src"]).is_file():
            with Path(attrs["src"]).open("rb") as stream:
                header = stream.read(65536)
            if b"mozilla.org/MPL/2.0" in header:
                source_record["licenses"] = full_license("MPL-2.0")
        require(source_record["licenses"], "missing full license text for " + Path(output).name)
        inventory["outputs"][output] = {"materials": materials}

    # Include the exact pinned recipes rather than promising a future source offer.
    locked = json.loads((REPO / "flake.lock").read_text())["nodes"]["nixpkgs"]["locked"]
    require(inputs.get("nixpkgs_pin", locked["rev"]) == locked["rev"], "source recipe pin mismatch")
    expression = "let tree = builtins.fetchTree " + "{ " + " ".join(
        key + " = " + nix_string(locked[key]) + ";" for key in ("type", "owner", "repo", "rev", "narHash")) + " }; in { sourcePath = builtins.toString tree.outPath; narHash = tree.narHash; }"
    tree = json.loads(run("nix", "eval", "--json", "--expr", expression))
    require(isinstance(tree, dict) and tree.get("narHash") == locked["narHash"], "source recipe hash mismatch")
    recipe_key = add(tree["sourcePath"], "nixpkgs", need_license=True)
    for entry in inventory["outputs"].values():
        entry["materials"].append(recipe_key)
    require(set(inventory["outputs"]) == set(outputs), "incomplete source output coverage")
    write_json(root / "sources/inventory.json", inventory)


def verify_sources(root, require):
    root = Path(root)
    path = root / "sources/inventory.json"
    require(path.is_file() and not path.is_symlink() and path.stat().st_size <= 16 * 1024**2,
            "missing or oversized corresponding-source inventory")
    inventory = json.loads(path.read_text())
    require(inventory.get("schema") == 1 and inventory.get("outputs")
            and inventory.get("materials"), "invalid corresponding-source inventory")

    def checked(member):
        value = PurePosixPath(member)
        require(not value.is_absolute() and len(value.parts) == 2
                and value.parts[0] in ("sources", "licenses")
                and all(p not in ("", ".", "..") for p in member.split("/")),
                "unsafe source inventory path")
        target = root / member
        require(target.is_file() and not target.is_symlink(), "missing source inventory material")
        return target

    for material in inventory["materials"].values():
        require(sha256(checked(material["archive"])) == material["sha256"], "source material checksum mismatch")
        for license_text in material.get("licenses", []):
            require(sha256(checked(license_text["path"])) == license_text["sha256"],
                    "license material checksum mismatch")
        for crate, texts in material.get("vendor_licenses", {}).items():
            require(texts, "missing full vendored license material: " + crate)
            for text in texts:
                require(sha256(checked(text["path"])) == text["sha256"], "vendor license checksum mismatch")
    for output in inventory["outputs"].values():
        require(all(key in inventory["materials"] for key in output["materials"]), "missing source material reference")
        records = [inventory["materials"][key] for key in output["materials"]]
        require({"source", "recipe", "nixpkgs"} <= {record["role"] for record in records},
                "incomplete corresponding-source roles")
        require(any(record.get("licenses") for record in records if record["role"] == "source"),
                "missing full source license material")
