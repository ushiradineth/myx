#!/usr/bin/env bash
set -euo pipefail

# Only this daemon and its private socket are used; no hardware modules load.
audio_bin=${1:?usage: pulse-coreaudio-init.sh /path/to/pulseaudio/bin}
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/pulse-coreaudio-init.XXXXXX")
daemon_pid=
cleanup() {
    status=$?
    trap - EXIT
    if [ -n "$daemon_pid" ]; then
        kill "$daemon_pid" 2>/dev/null || :
        wait "$daemon_pid" 2>/dev/null || :
    fi
    if [ "$status" -ne 0 ]; then
        cat "$test_dir/daemon.log" >&2
    fi
    rm -rf "$test_dir"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
ulimit -c 0

export PULSE_SERVER="unix:$test_dir/native"
export PULSE_CLIENTCONFIG=/dev/null
export PULSE_RUNTIME_PATH="$test_dir/runtime"
export HOME="$test_dir/home"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
mkdir -p "$PULSE_RUNTIME_PATH" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"
"$audio_bin/pulseaudio" --daemonize=no --use-pid-file=no --exit-idle-time=-1 \
    --disable-shm=yes --disallow-exit --log-target=stderr --log-level=debug \
    -n --load="module-native-protocol-unix socket=$test_dir/native auth-anonymous=1" \
    --load="module-null-sink sink_name=cleanup_test" >"$test_dir/daemon.log" 2>&1 &
daemon_pid=$!

ready=false
for ((attempt = 0; attempt < 100; attempt++)); do
    kill -0 "$daemon_pid"
    if "$audio_bin/pactl" info >/dev/null 2>&1; then
        ready=true
        break
    fi
    sleep 0.05
done
"$ready"

for ((attempt = 0; attempt < 3; attempt++)); do
    for object_id in 0 invalid; do
        if "$audio_bin/pactl" load-module module-coreaudio-device \
            "object_id=$object_id" record=no playback=yes >"$test_dir/load.log" 2>&1; then
            printf 'Unexpected success for object_id=%s\n' "$object_id" >&2
            exit 1
        fi
        # A client failure alone also happens on SIGABRT: prove server survival.
        kill -0 "$daemon_pid"
        "$audio_bin/pactl" info >/dev/null
    done
done
grep -q 'Failed to get kAudioDevicePropertyDeviceName' "$test_dir/daemon.log"
grep -q 'Failed to parse object_id argument' "$test_dir/daemon.log"
printf 'CoreAudio failed-init cleanup: six rejected loads; private daemon survived\n'
