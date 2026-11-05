# myx

A terminal Spotify client with a daemon that owns playback, authorization and
media controls. An Apple Silicon macOS preview is available from
[GitHub releases](https://github.com/ushiradineth/myx/releases);
exhaustive Mac acceptance remains incomplete. Windows development has started,
but there is no runnable Windows release yet.

Download and extract the complete `myx-macos-aarch64` layout, then run `bin/myx`.
Keep its sibling `runtime`, source and license materials together. The preview
is ad-hoc signed, not Developer ID notarized. Intermittent/startup underrun
telemetry remains an open issue despite sampled no-audible-glitch feedback;
actual terminal/media/account/login-service qualification is incomplete.

For declarative installation and age-backed credentials, see [Nix setup](docs/nix.md).

## macOS setup

The delivery target is Apple Silicon on macOS 15 or newer. One `bin/myx`
executable serves the TUI, bounded headless client and daemon. The daemon requires
both Spotify Web API authorization and the Soloist runtime, including the
processed PCM/EQ route. There is no backend feature flag or decoder fallback.

Soloist's vendor SDK and API key are user-provisioned. Neither belongs in a public
archive, Nix input, issue report or source checkout. Spotify Premium is required
for playback. You can prepare configuration before obtaining SDK/account assets;
setup does not require Windows fixtures or start playback.

1. Obtain a `myx-macos-aarch64` preview layout or follow the
   [release assembly guide](docs/macos-release.md). Keep the whole layout together
   in a stable user-owned location outside a Cargo checkout or Nix store.
2. Explicitly initialize and edit a private configuration:

   ```sh
   /absolute/install/myx-macos-aarch64/bin/myx config init --output /absolute/private/config.toml
   /absolute/install/myx-macos-aarch64/bin/myx --config /absolute/private/config.toml config check
   ```

   Use an existing owned private parent, or let init create missing directories
   with mode 0700. The file is mode 0600; existing files are never overwritten.
   See [configuration](docs/configuration.md) for SDK/key paths and migration.
3. Set a Spotify application `client_id` in the private file. Register
   `http://127.0.0.1:8989/login` in the
   [Spotify developer dashboard](https://developer.spotify.com/dashboard).
   Start `myx --config /absolute/private/config.toml` interactively when ready
   to authorize. Authorization is explicit client work; a background daemon does
   not open a browser. `--non-interactive` keeps clients from opening one too.

Configuration load never creates or migrates a file. Missing credentials are a
setup state, not evidence that playback is ready. Older upstream installers and
package-manager releases are not installers for this rewrite.

## Use

```sh
myx --config /absolute/private/config.toml
myx daemon start --config /absolute/private/config.toml
myx daemon status --config /absolute/private/config.toml --json
myx --headless --duration 30 --non-interactive --config /absolute/private/config.toml
myx daemon stop --config /absolute/private/config.toml
```

Track, album, playlist, artist and episode Spotify URIs and HTTPS share URLs are
accepted. Headless runs require a duration from 1 to 86400 seconds. Closing the
TUI detaches the client; use `daemon stop` to stop its owner. See
[daemon and LaunchAgent](docs/daemon.md) for explicit service management.

| Key | Action |
| --- | --- |
| Tab / `[` / `]` | Switch section |
| Arrows / `j` / `k` | Navigate |
| Enter | Play or open |
| Shift+Enter / `P` | Play highlighted source |
| `/` / `a` | Search / actions |
| Space / `n` / `b` | Pause or play / next / previous |
| Shift+Left / Shift+Right | Seek |
| `s` / `R` | Shuffle / repeat |
| `+` / `-` / `e` | Volume / equalizer |
| `T` / `z` | Transparent background / sidebar |
| `q` | Detach TUI |

Album art, reactive colors, lyrics and a post-EQ spectrum belong to the client
view. The daemon processes PCM even without a TUI or spectrum subscriber. Terminal
image quality depends on the terminal protocol. System media controls and audio
device recovery require owner acceptance on the final Mac build.

Optional local JSON output is documented in [now playing](docs/now-playing.md).
The [platform scope](docs/platforms.md) distinguishes the local Mac release from
in-progress Windows work and deferred ports.

## Develop

Use the pinned Nix shell on an Apple Silicon Mac:

```sh
nix develop -c cargo fmt --all -- --check
nix develop -c cargo clippy --locked --workspace --all-targets -- -D warnings
nix develop -c cargo test --locked --workspace --all-targets
```

Software gates do not authorize or replace SDK/account, native audio, media-key,
GUI or LaunchAgent acceptance. The Mac assembler never publishes; the legacy
tag-triggered publishing workflow has been removed. There are no `soloist` build
features, `--api`, `--backend`, MXC
CLI mode or compiled librespot playback engine in this application.

## Provenance and licenses

This is a fork of [HaseebKhalid1507/Myx](https://github.com/HaseebKhalid1507/Myx).
The project remains MIT-licensed; upstream credits and historical adaptations
are retained in [NOTICE](NOTICE) and [LICENSE](LICENSE). The release includes
separately licensed redistributable helpers and libraries. See
[licensing](docs/licensing.md) before assembling or distributing them.
