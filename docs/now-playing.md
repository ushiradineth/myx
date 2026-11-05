# Local now-playing JSON

The daemon can publish a private local JSON file independently of the TUI,
artwork fetching and spectrum subscribers. It is disabled by default and does no
path or filesystem work while disabled.

```toml
[now_playing]
enabled = true
path = "~/.local/state/myx/now-playing.json"
```

Choose a user-owned output outside the immutable runtime, SDK, key, configuration
and data/cache locations. Symlink/unsafe destinations are rejected. Publication
uses complete temporary files and atomic replacement, at most once a second
with a heartbeat within five seconds. A consumer should treat an
`updated_at_unix_ms` older than 15 seconds as stale, including after a crash.

Version 1 fields are `schema_version`, `session_id`, `pid`,
`updated_at_unix_ms`, `status`, `position_ms`, `metadata_status` and nullable
`track`. A track contains `kind`, `uri`, nullable `title`, `artists`, `album`,
`duration_ms`, `artwork_url` and `spotify_url`. Missing canonical metadata stays
null; URI text is not a fallback title. Consumers should handle unknown future
versions explicitly and never infer local playback from a Spotify account's
remote-device state.

The writer uses the daemon's owned-device verification and local readiness to
project state. Paused/buffering metadata may remain available; clean shutdown
attempts a final stopped document with null track and zero position. Position
advances from a timestamp/speed anchor only when locally playing, bounded by
known duration. Export failure is reported separately and must not block audio.
Shutdown waits at most two seconds; a stalled filesystem can prevent a final
document, so consumers still need the stale timestamp rule.

This output is not the daemon management `--json` status response and is not a
credential/telemetry export. Its software fixtures and actual no-TUI operation
on final release bytes are separate acceptance checks.
