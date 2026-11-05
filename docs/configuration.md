# Configuration and explicit setup

The default path is `~/.config/myx/config.toml`. `--config PATH` selects a profile
before settings are read. Relative CLI paths resolve against the caller's working
directory; runtime paths in TOML must be absolute or start with `~/`.

## Prepare without starting services

```sh
myx config init --output /absolute/private/candidate.toml
myx --config /absolute/private/candidate.toml config check
myx --config /absolute/private/old.toml config migrate --output /absolute/private/migrated.toml
myx --config /absolute/private/migrated.toml config check
```

`config init` writes a commented template to `--output`, or to the selected config
path when output is omitted. It refuses any existing destination. `config check`
requires an existing private file and validates TOML and configuration constraints
only. It does not load an SDK, read a key file, contact Spotify, validate runtime
readiness or print configuration contents. Diagnostics are static and redacted.

`config migrate` requires an explicit new output path. It reads the protected
source and removes obsolete root `backend`, `api`, `bitrate`, `normalize_volume`
and `soloist.endpoint`/`soloist.dsp` settings. Retained lines, key bytes and comments
are preserved; ambiguous multiline/inline-table edits fail rather than rewriting
credentials. The source is never modified. Migration does not select the
candidate, replace a live configuration or restart the daemon.

Files must be same-user regular nonsymlink files with exactly mode 0600 and one
link. Symlink ancestors are rejected. Init/migrate publish a completed private
candidate using a no-replace link operation; a colliding file is never clobbered.
Missing directories are created privately only by these explicit write operations.
Load and check never bootstrap or migrate automatically.

## Settings

```toml
scrolloff = 3
restore_on_startup = true
transparent = true
client_id = "YOUR_SPOTIFY_APPLICATION_ID"
# protocol = "kitty"

[soloist]
directory = "~/.config/myx/soloist"
sdk = "/absolute/private/vendor/soloist"
api_key_file = "/absolute/private/api-key"
data_dir = "~/.config/myx/soloist/data"
cache_dir = "~/.config/myx/soloist/cache"
device_name = "myx"
initial_volume = 25
# expected_sdk_sha256 = "64_HEXADECIMAL_DIGITS"

[now_playing]
enabled = false
# path = "~/.local/state/myx/now-playing.json"
```

Use either `api_key_file` or an inline `api_key`, never both. The external key file
must also satisfy the runtime's private credential-file requirements. Prefer the
file form; do not paste keys into argv, bug reports or shell history. SDK/key/data/
cache paths always remain external to a release. Pairing, OAuth tokens, cookies,
per-launch scratch and other mutable state must not be written into the bundle.

`device_name` is a friendly name of 1 to 63 UTF-8 bytes without control characters.
Spaces and Unicode are retained. `initial_volume` is 0 through 100. Changing the
friendly name requires an explicit daemon restart before a new advertisement is
used. Restoration starts paused, awaiting an explicit playback request.

The installed bundle supplies runner, sysroot, library manifest and audio tools
through the read-only discovery hook described in [release assembly](macos-release.md).
Explicit `runner`, `sysroot`, `library_manifest`, `library_path`, `audio_tools`
paths take precedence. `library_path` is a colon-separated list of absolute
directories. External/development library manifests retain the existing JSON
`arch: "aarch64-linux"`, absolute string `libraryPath` contract. The installed
bundle's relative array manifest must be resolved by `RuntimeBundle`, not that
legacy external-manifest parser. Never resolve the SDK or user state via bundle
fallbacks.

## Applying a candidate

Inspect a candidate privately, then explicitly choose it with `--config` and
restart when authorized. A running daemon retains its selected profile; attaching
with a different profile fails rather than silently switching it. Service
registration and live configuration cutover are separate owner operations.
The template is available from `config init`; no first launch is required.
