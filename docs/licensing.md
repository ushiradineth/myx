# Release licensing

The application is MIT-licensed. Preserve root `LICENSE`, `NOTICE`, upstream
Myx authorship, spotify-player adaptations and noodle design attribution.
Historical librespot attribution remains provenance; it is not a current engine
or dependency claim.

The Mac release includes separately licensed redistributables. ELFUSE is pinned
at `ba455f45592cd12488519e207c44c32fa8e167df` (Apache-2.0). Its full license is
copied from the helper derivation. Local compatibility/packaging patches live in
`nix/compat`; the Nixpkgs-derived audit adjustment carries its MIT license at
`nix/compat/nixpkgs-LICENSE`, copied as `licenses/NIXPKGS-MIT`.

PulseAudio, glibc, compiler runtime, certificate data and every recursively linked
dependency require their own full licenses and notices. LGPL libraries require
the applicable license/source/relinking provisions; compiler runtime exceptions
and bundled source notices must be preserved. A list of SPDX identifiers alone
is insufficient. The release includes corresponding sources and exact patches/
build recipes for the distributed versions. The pinned Nixpkgs revision and package name alone do
not replace those obligations.

## Included source materials

The release includes sources rather than making a written promise to supply them
later. `stage` derives the source inventory from the exact Nix outputs whose
files enter the bundle. It preserves upstream source archives, directory source
snapshots, patches and build-phase recipes, plus the pinned Nixpkgs recipes.
The application's filtered source and vendored Cargo dependencies are included;
the application's MIT license is not applied to all those dependencies.

`sources/inventory.json` binds each copied output to its material and license
checksums. Source archives and recipes live under `sources/`; full license and
notice texts live under `licenses/`. Original notices remain inside the source
archives as well. Directory snapshots normalize archive ordering, ownership and
timestamps. The collector inspects archive members without extracting or
executing them. It fetches missing source inputs only through fixed-output Nix
fetchers, never by executing the SDK or another runtime program.

When a vendored Cargo package declares a standard SPDX license but omits its
full text, the collector includes the matching text from the pinned
`spdx/license-list-data` `v3.27.0` reference. Cargo attribution and original source
headers remain in the source archive. Custom licenses without their full text
stop assembly. Mozilla certificate data retains its source and full MPL-2.0 text.
Complete BSD notices embedded in upstream C source headers are retained as
license material when there is no standalone license file. Directory source
snapshots preserve upstream fixture symlinks as inert archive entries; vendor
aliases are flattened only from declared public source inputs.
These are mechanical provenance checks, not a legal-compliance opinion.

Review the exact inventory, notices, source/build completeness and applicable
LGPL relinking requirements before distributing a release. Source archives do
not grant rights to unrelated proprietary assets, and checksum verification is
not proof that every distribution obligation has been met.

No public artifact may contain the Soloist vendor SDK, API keys, OAuth/pairing
state, caches, credentials, dumps, VM images, private configuration or user data.
Do not apply the application MIT license to the SDK. Vendor provisioning and
usage rights remain external to this project.
