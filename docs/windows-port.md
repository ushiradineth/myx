# Windows port status

Windows work now follows the delivered local Mac release. The installed Mac
bundle and its source-inclusive archive remain frozen; this is not a declaration
that every Mac qualification gate passed. Known Mac audio telemetry and remaining
manual checks are documented with the local release.

## What has started

`soloist-runtime::executable` is a platform-neutral, credential-free format
preflight. It validates Linux AArch64 ELF headers and Windows x86_64 PE32+
executable headers, section tables and raw-data bounds. Synthetic Rust fixtures
cover truncation, architecture mismatch, DLL rejection and wrong-target
admission. The Mac caller still requires Linux AArch64, and its protected file
read, digest and symlink policy are unchanged.

Format validation is not vendor identity, Windows execution or playback support.
The real runtime remains Mac-only. No Windows player, service or audio backend
has been enabled by this extraction.

## First runtime decision: private processed audio

Prior investigation recorded an official native Windows x86_64 Soloist SDK and
successful help/version execution. Its current availability and version must be
rechecked on the Windows host; a historical receipt is not fresh native proof.
No documented PCM-export callback, file or pipe was established.

The next candidate to prove is documented Windows per-process loopback capture:

- Microsoft's [Application Loopback sample](https://github.com/microsoft/Windows-classic-samples/tree/main/Samples/ApplicationLoopback)
  captures one process tree independently of physical output endpoints, without
  capturing other applications. It requires Windows build 20348 or newer.
- A credential-free probe should render a synthetic signal in its own process,
  capture only that process, and establish whether capture remains usable while
  that process's original output is muted. The mute must be confirmed before
  starting render. It must not change endpoint/global volume or other sessions.
- Keep samples in memory and report numeric signal/gain/stereo results; do not
  use the example's WAV-file output or capture desktop/microphone audio.
- Then prove the same private route with the unmodified SDK, including startup,
  volume/control transitions, device changes and owned cleanup. Merely capturing
  audio while also hearing the unprocessed SDK stream is a failure.

This is an investigation candidate, not an adopted backend. If session muting
silences the captured signal too, or an exclusive/protected SDK render path is
not capturable, the candidate does not meet the contract. Do not bypass EQ,
install a virtual driver, enable WSL/hypervisor features or patch the vendor SDK
to call the port complete.

Official SDK provisioning reference:
<https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates>.

## Other concrete port boundaries

The domain/reducer, catalog, SDK control protocol, TUI and DSP code are reusable,
but the application does not currently compile for Windows. Remaining owners are:

- Named-pipe IPC with bounded framing, same-user/SID access and peer identity,
  replacing Unix sockets/FDs without weakening private access.
- Windows Job Object/process ownership and cleanup, replacing `setsid`/signals.
- Protected file/ACL and atomic-persistence/publication operations.
- Native SDK launch/validation and PCM/output/device-recovery implementation;
  Mac ELFUSE, CoreAudio, DNS-SD and Linux-library paths are not Windows adapters.
- Windows media controls, clipboard and optional explicit service integration.
- A real Windows build/package and native execution checks. Mac unit tests and
  PE inspection cannot prove these.

## Current external prerequisite

Historical Windows 11 x64/SDK receipts are preserved, not treated as current host
or credential verification. The next native probe needs a reachable authorized
Windows desktop session; SSH/session-zero execution alone does not prove
interactive audio behavior. No native Windows player has been qualified yet.

The synthetic audio probe does not require a Spotify key. Authenticated SDK work
later requires protected Windows-specific key/authorization/pairing references.
Do not copy Mac keys or OAuth/pairing stores automatically, and do not request new
keys merely because an unrelated startup hint mentions them.
