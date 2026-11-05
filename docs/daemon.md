# Daemon and macOS LaunchAgent

One `myx` executable owns the daemon or acts as its client. The daemon owns the
Web API session, Soloist process, mandatory DSP, media integration and JSON
publisher. A TUI closing does not stop playback or release daemon ownership.

```sh
myx daemon run --config /absolute/private/config.toml
myx daemon start --config /absolute/private/config.toml
myx daemon status --config /absolute/private/config.toml --json
myx daemon restart --config /absolute/private/config.toml
myx daemon stop --config /absolute/private/config.toml
```

`run` is the foreground owner entry point. `start` ensures an owner is available;
`restart` explicitly replaces it using the selected profile. `status` does not
start playback or authorize. Stop first removes a recognized supervisor's
activation so it cannot respawn the owner, then requests bounded owner shutdown.
Only the application's own recognized service is managed.

## Registration is explicit

Install the complete copied release in a stable, user-owned directory outside
Cargo checkouts, `target`, caches and Nix store paths. Use its actual `bin/myx`,
not a development wrapper, shell shim or symlink. Prepare a private configuration
before registration. Under owner approval in the intended Mac GUI-user session:

```sh
/absolute/install/myx-macos-aarch64/bin/myx service install --config /absolute/private/config.toml
/absolute/install/myx-macos-aarch64/bin/myx service status --config /absolute/private/config.toml --json
```

Install writes `~/Library/LaunchAgents/io.myx.daemon.plist` and prepares private
logs under `~/Library/Logs/io.myx.daemon`. It is registration-only: it does not
bootstrap the job, stop an existing daemon, migrate configuration or perform a
live cutover. An explicit `daemon start` activates the registered agent. A loaded
agent starts at login and restarts after unsuccessful exit, with a 30-second
throttle. Explicit stop boots it out so a clean stop stays stopped.

`service uninstall` boots out and removes only a recognized managed registration.
It does not remove credentials, user data or caches. A modified/foreign plist or
changed registration path is rejected rather than overwritten. Changing the
registered executable/config paths requires explicit uninstall and reinstall.

The template in `packaging/macos/io.myx.daemon.plist.in` is a definition example.
The Rust service renderer is authoritative; XML-special path characters must be
escaped, not interpolated through a shell. Do not manually install that template
or treat XML parsing as proof of launchd behavior.

## Diagnose setup

Run `config check` first, then inspect `daemon status` for owner/profile/readiness
state. A daemon can be online while SDK or Web API setup is incomplete. Do not
share private files, raw child output or authorization URLs. Background startup
never opens consent UI; use an interactive client when authorization is needed.

Actual registration, login/on-demand activation, media controls without a TUI,
stop-without-respawn and owned cleanup need final GUI-user receipts. Those owner
operations are pending; this document does not record acceptance.
