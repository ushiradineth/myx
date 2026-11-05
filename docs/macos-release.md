# Assemble and qualify a Mac release

The assembler targets one `myx-macos-aarch64` directory with one `bin/myx`
daemon/client executable. It accepts only the pinned redistributable ELFUSE,
private PulseAudio, recursive native Mach-O dependencies, Linux AArch64 loader/
shared-library closure and CA certificates, with corresponding sources and
license texts. SDK/key/config/data inputs do not
exist in the interface. Assembly and static inspection do not qualify native
playback, account behavior or GUI/service acceptance.

## Build, stage, inspect, archive

Run from the checkout on Apple Silicon after the software checks pass.
Use a physical absolute user-owned mode-0700/0755 output parent outside the
checkout and Nix store. Every output path must be new; neither build receipts,
staging directories nor archives are overwritten. A failed stage remains an
owned partial directory for inspection, never an implicitly cleaned destination.

```sh
nix develop --no-update-lock-file -c python3 packaging/macos/release.py build \
  --output /absolute/owned-release/build-inputs.json

nix develop --no-update-lock-file -c python3 packaging/macos/release.py stage \
  --inputs /absolute/owned-release/build-inputs.json \
  --output /absolute/owned-release/myx-macos-aarch64

nix develop --no-update-lock-file -c python3 packaging/macos/release.py verify \
  --bundle /absolute/copied-location/myx-macos-aarch64 \
  --patchelf /absolute/path/to/pinned-releaseTools/bin/patchelf

nix develop --no-update-lock-file -c python3 packaging/macos/release.py archive \
  --bundle /absolute/copied-location/myx-macos-aarch64 \
  --patchelf /absolute/path/to/pinned-releaseTools/bin/patchelf \
  --output /absolute/owned-release/myx-macos-aarch64.tar.gz
```

`build` uses pinned Nix first, without global installs or output GC-root symlinks.
It builds the application's release package and existing `nix/soloist.nix`
attributes, then records exact store outputs and their recursive closure in a
private build receipt. `releaseTools` supplies Python/patchelf; obtain its exact
path from that receipt. Darwin's `otool`, `lipo`, `install_name_tool` and
`codesign` inspect/edit/sign copied files; they do not run the application.
Keep the build receipt outside the archive. Its store paths are build inputs,
not runtime requirements.

Stage recursively closes native imports, rejects unresolved/undeclared imports
and basename collisions, rewrites imports/IDs to `@loader_path`, and removes
compiled rpaths. Required dynamically loaded Pulse modules are explicit roots.
Native libiconv imports bind to macOS's `/usr/lib/libiconv.2.dylib`, not a copied
Nix libiconv whose dynamically loaded Citrus modules and conversion data retain
compiled store paths. Copied-runtime checks must exercise non-ASCII conversion
and complete `pactl` JSON responses with host `/nix/store` reads denied; inspecting
Mach-O import paths alone does not prove dynamic dependencies are self-contained.
The pinned PulseAudio JSON encoder also has a focused UTF-8 fix in `nix/compat`:
non-ASCII strings must not become `"(null)"` in otherwise valid JSON. Native
dependency install checks exercise UTF-8 and existing JSON escape sequences.
Linux shared objects and SONAME aliases are regular files under the private
`runtime/sysroot/nix/store` guest namespace. Keeping package directories separate
preserves distinct libraries with the same basename and contains absolute guest
`dlopen` paths without depending on the host's Nix store. Public guest ELF files,
including glibc's self-relocating loader, are copied byte-for-byte. Original
guest-absolute interpreter/dependency/search paths must resolve to the copied
private namespace; they are not rewritten with patchelf. The Linux loader and
certificates are also copied inside the sysroot. The vendor SDK is never patched. Native Mach-O imports do
not depend on developer Nix paths, and the library manifest contains only
bundle-relative directories.

The same pinned public CA bundle is installed at all six compiled SDK fallback
paths under `/etc/ssl` and `/etc/pki` inside the guest sysroot, matching the
validated development layout. `SSL_CERT_FILE` alone does not cover a TLS client
that explicitly selects a compiled-in CA filename. The aliases contain identical
certificate bytes; no extra trust roots, private keys or insecure TLS flags are added.

Final native bytes are ad-hoc signed after loader edits. ELFUSE receives
`com.apple.security.hypervisor = true`; signing verification checks it remains
present. The assembler does not supply Developer ID/notarization, disable system
security checks, remove quarantine or enable any OS feature.

Stage includes exact source inputs, patches, build recipes and full license
texts for every copied output, including transitive dependencies. Rust vendor
sources and their notices are included too. Missing source or license material
stops assembly; there is no generated written source offer. See
[licensing](licensing.md) for inventory details and distribution review.

## Installed runtime contract

`runtime/manifest.json` schema 1 inventories SHA-256 of every final signed/
relocated file except itself. It pins ELFUSE and Nixpkgs revisions. The schema is
`packaging/macos/manifest.schema.json`. `runtime/arm-libraries.json` has the
separate bundle-relative array schema in `libraries.schema.json`. Neither
manifest contains SDK/key/user-data locations. Checksums detect corruption;
they are not a signature or publisher-authenticity mechanism.

The daemon and SDK runtime configuration discover `bin/myx` with a sibling
`../runtime/manifest.json`. Missing metadata uses external development defaults;
present invalid metadata is an error, never a fallback to developer Nix paths.
Discovery verifies pins, inventory, digests, file/link/path safety and loader
metadata without launching helpers.

Explicit runtime paths take precedence over bundle defaults. A user
`library_manifest` retains its external absolute-string format; otherwise the
bundle's relative array resolves to host absolute library directories. Pulse
receives the relocated module directory through `--dl-search-path`, and its
libloading entry point is `runtime/audio/lib/libpulse.0.dylib`.

SDK/key/config/data/cache and now-playing output remain external. Startup rejects
paths into the bundle, including symlink aliases and not-yet-created paths,
before starting the publisher or native runtime. The sysroot remains immutable;
`Managed::start` passes it directly to ELFUSE. Actual sysroot mapping, dynamic
module loading and absence of runtime writes still require copied-layout native
qualification. Never rewrite installed manifests or credentials in place.

`config init|check|migrate` runs before shared configuration loading or daemon
startup. Migration requires a new `--output` candidate and preserves the source.
The former publishing workflow and obsolete Debian/librespot package metadata
have been removed; Mac software CI neither publishes nor registers a service.

`verify` checks architecture, exact inventory/hashes, native imports/rpaths,
ELF interpreter/DT_NEEDED/RPATH closure, signatures and ELFUSE entitlement. It
also verifies the source inventory and its material/license checksums. It
does not prove Pulse dynamic module loading, guest sysroot mapping, immutable
runtime startup, DSP or audio playback. Archive ordering, IDs and timestamps
are normalized; deterministic archives still require reproducible signed input
bytes. Rebuilding twice and comparing hashes remains a release check.

## Written software and owner scenarios

The pure/owned-fixture packaging tests in `packaging/macos/tests` cover path
traversal and SDK/user-state exclusion, no-clobber destinations, symlink parents,
Mach dependency/rpath parsing, ELF interpreter bounds and LaunchAgent XML
quoting. `setup.rs` contains Rust fixture scenarios for the config CLI, staged
migration, copied bundle discovery, pins/checksums, traversal and symlinks.
It is included in the library's ordinary software test target.

Source fixtures exercise archive preservation, safe inert symlinks, vendor
license material, fixed-output source fetching, output/recipe identity and
checksums. Runtime preflight checks loader closure before collecting source
materials; it does not replace the final manifest/source/signature verification.

For a newly assembled, owned layout, run the production Rust reader without
starting a daemon or reading a user profile:

```sh
MYX_TEST_COPIED_BUNDLE=/absolute/copied-location/myx-macos-aarch64/bin/myx \
  nix develop -c cargo test --locked --lib \
  macos_setup_tests::actual_copied_release_passes_the_production_bundle_reader \
  -- --ignored --exact
```

Software checks cover moved fixtures, pin/hash mismatches, missing/extra files,
unsafe paths and links, malformed loader metadata and developer imports.
Configuration fixtures check source/comment/key-byte preservation, collisions,
redacted errors and absence of implicit writes. Run these against owned fixtures,
without reading actual user profiles.

Exercise the copied native helpers with host-store access denied and a private
Unicode-valued null sink (no SDK, microphone, hardware output or user state):

```sh
MYX_NATIVE_AUDIO_ALLOW=1 \
MYX_TEST_COPIED_BUNDLE=/absolute/copied-location/myx-macos-aarch64/bin/myx \
  nix develop -c python3 -m unittest packaging.macos.tests.native_release -v
```

Copy the final layout outside checkout and dev GC roots, qualify it without
access to host `/nix/store`, and record hashes plus exact launch/DSP/loader/
resource results. Then obtain owner receipts for real account setup, sustained
processed playback, terminal/GUI behavior, media keys without a TUI, LaunchAgent
registration/login/stop/no-respawn/cleanup and explicit config cutover. Those
checks remain pending. Manifest inspection or successful ad-hoc signing alone
cannot qualify a copied release.
