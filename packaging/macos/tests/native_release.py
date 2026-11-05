"""Explicit copied-release checks: private null sink, no SDK, user state or hardware."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest


class CopiedRelease(unittest.TestCase):
    def test_pactl_preserves_unicode_json_without_host_nix_store(self):
        self.assertEqual(os.environ.get("MYX_NATIVE_AUDIO_ALLOW"), "1",
                         "explicit native fixture allowance required")
        executable = Path(os.environ["MYX_TEST_COPIED_BUNDLE"])
        self.assertTrue(executable.is_absolute() and executable.is_file())
        tools = executable.parent.parent / "runtime/audio"
        sandbox = ["/usr/bin/sandbox-exec", "-p",
                   '(version 1) (allow default) (deny file-read* (subpath "/nix/store"))']
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary).resolve()
            cookie = run / "cookie"
            cookie.write_bytes(os.urandom(256))
            cookie.chmod(0o600)
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            config = run / "server.pa"
            config.write_text(
                '.fail\nload-module module-null-sink sink_name=myx_fixture '
                'sink_properties="device.description=\'Myx café 日本\'"\n'
                f'load-module module-native-protocol-tcp listen=127.0.0.1 port={port} '
                f'auth-anonymous=no auth-cookie={json.dumps(str(cookie))}\n')
            env = {"PATH": "/usr/bin:/bin", "HOME": str(run), "LANG": "en_US.UTF-8",
                   "PULSE_RUNTIME_PATH": str(run), "PULSE_STATE_PATH": str(run),
                   "PULSE_SERVER": f"tcp:127.0.0.1:{port}", "PULSE_COOKIE": str(cookie)}
            with subprocess.Popen(
                [*sandbox, str(tools / "bin/pulseaudio"), "--dl-search-path", str(tools / "modules"),
                 "--daemonize=no", "--use-pid-file=no", "--system=no", "--exit-idle-time=-1",
                 "--disable-shm=yes", "-n", "-F", str(config)],
                env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            ) as server:
                try:
                    deadline = time.monotonic() + 15
                    while True:
                        self.assertIsNone(server.poll(), "private native server exited")
                        try:
                            with socket.create_connection(("127.0.0.1", port), timeout=.1):
                                break
                        except OSError:
                            self.assertLess(time.monotonic(), deadline, "native server readiness timed out")
                            time.sleep(.02)
                    result = subprocess.run(
                        [*sandbox, str(tools / "bin/pactl"), "--format=json", "list", "sinks"],
                        env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                        encoding="utf-8", timeout=5)
                    self.assertEqual(result.returncode, 0, "copied pactl failed")
                    try:
                        sinks = json.loads(result.stdout)
                    except json.JSONDecodeError:
                        self.fail("copied pactl stdout is not a complete JSON document")
                    self.assertEqual(len(sinks), 1)
                    self.assertEqual(sinks[0]["properties"]["device.description"], "Myx café 日本")
                finally:
                    if server.poll() is None:
                        server.terminate()
                    try:
                        server.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait(timeout=5)
                        self.fail("private native server did not stop")
