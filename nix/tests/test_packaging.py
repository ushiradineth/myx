"""Credential-free tests for the candidate's actual audit function."""
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).parent
HOOK = Path(os.environ.get('AUDIT_HOOK_SOURCE', ROOT / 'compat/audit-tmpdir.sh'))
BASH = os.environ.get('AUDIT_TEST_BASH', '/nix/store/n51n6xlxqwf2i95k5v6pi4z1862ikfg5-bash-5.3p15/bin/bash')
TEMP = Path(os.environ.get('AUDIT_TEST_TMPDIR', ROOT / '.runtime'))
HELPERS = r'''
# Signature readers follow Nixpkgs setup.sh:511-560 (MIT).
# Copyright (c) 2003-2026 Eelco Dolstra and the Nixpkgs/NixOS contributors.
# See compat/nixpkgs-LICENSE.
set -euo pipefail
ulimit -c 0
source "$AUDIT_HOOK_SOURCE"
isELF() {
    local fn="$1" fd magic
    exec {fd}< "$fn"
    LANG=C read -r -n 4 -u "$fd" magic
    exec {fd}<&-
    [[ "$magic" == $'\177ELF' ]]
}
isScript() {
    local fn="$1" fd magic
    exec {fd}< "$fn"
    LANG=C read -r -n 2 -u "$fd" magic
    exec {fd}<&-
    [[ "$magic" =~ \#! ]]
}
'''


class AuditPackagingTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='audit-test-', dir=TEMP)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.output = self.root / 'output'
        self.output.mkdir(mode=0o700)
        self.scratch = self.root / 'scratch'
        self.scratch.mkdir(mode=0o700)
        (self.output / 'fixture').write_text('fixture\n')

    def run_hook(self, setup='', conditional=False, after='', extra_env=None):
        call = 'if auditTmpdir "$1"; then status=0; else status=$?; fi; exit "$status"' if conditional else 'auditTmpdir "$1"'
        environment = {k: v for k, v in os.environ.items() if not k.startswith('LC_') and k not in ('LANG', 'BASH_ENV', 'ENV')}
        environment.update({'AUDIT_HOOK_SOURCE': str(HOOK), 'TMPDIR': str(self.scratch),
                            'HOME': str(self.root), 'NIX_BUILD_CORES': '2'})
        environment.update(extra_env or {})
        arguments = [BASH, '-c', HELPERS + setup + '\n' + call + '\n' + after, 'audit-fixture', str(self.output)]
        with subprocess.Popen(arguments, env=environment, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True) as process:
            try:
                stdout, stderr = process.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate(timeout=2)
                raise
            return subprocess.CompletedProcess(arguments, process.returncode, stdout, stderr)

    def test_classifier_failure_is_not_success_under_direct_or_conditional_call(self):
        for conditional in (False, True):
            with self.subTest(conditional=conditional):
                result = self.run_hook('isELF() { exit 73; }', conditional=conditional)
                self.assertEqual(result.returncode, 73, result.stderr)
                self.assertEqual(list(self.scratch.iterdir()), [], 'FIFO scratch must be cleaned on failure')

    def test_failed_find_status_is_preserved(self):
        for conditional in (False, True):
            with self.subTest(conditional=conditional):
                result = self.run_hook('find() { return 41; }', conditional=conditional)
                self.assertEqual(result.returncode, 41, result.stderr)
                self.assertEqual(list(self.scratch.iterdir()), [])

    def wrapped_script(self, unsafe=False):
        wrapper = self.output / 'Unicode song λ.sh'
        body = '#!/bin/sh\nprintf safe\n' if not unsafe else '#!/bin/sh\n# ' + str(self.scratch / 'forbidden') + '\n'
        wrapper.write_text(body)
        (self.output / ('.' + wrapper.name + '-wrapped')).write_text('wrapped fixture\n')

    def test_safe_unicode_wrapper_is_scanned_successfully(self):
        self.wrapped_script()
        result = self.run_hook()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(self.scratch.iterdir()), [])

    def test_unsafe_unicode_wrapper_is_rejected(self):
        self.wrapped_script(unsafe=True)
        result = self.run_hook(conditional=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('forbidden reference', result.stdout)
        self.assertEqual(list(self.scratch.iterdir()), [])

    def test_classifier_locale_is_c_but_caller_locale_is_unchanged(self):
        result = self.run_hook('isELF() { [[ ${LC_ALL-} == C ]] || exit 79; return 1; }',
                               after='[[ ${LC_ALL+x} != x ]]')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rpath_consumer_still_checks_forbidden_references(self):
        binaries = self.root / 'bin'
        binaries.mkdir(mode=0o700)
        patchelf = binaries / 'patchelf'
        patchelf.write_text('#!' + BASH + '\nprintf "%s\\n" "$FAKE_RPATH"\n')
        patchelf.chmod(0o700)
        (self.output / 'fixture').write_bytes(b'\x7fELFfixture\n')
        for unsafe in (False, True):
            with self.subTest(unsafe=unsafe):
                value = str(self.scratch / 'forbidden') if unsafe else '/safe/lib'
                result = self.run_hook(conditional=True, extra_env={
                    'PATH': str(binaries) + ':' + os.environ['PATH'], 'FAKE_RPATH': value})
                self.assertEqual(result.returncode == 0, not unsafe, result.stderr)
                self.assertEqual(list(self.scratch.iterdir()), [])

    def test_cleanup_failure_is_not_success(self):
        result = self.run_hook('rm() { command rm "$@"; return 66; }', conditional=True)
        self.assertEqual(result.returncode, 66, result.stderr)
        self.assertEqual(list(self.scratch.iterdir()), [])

    def test_scratch_setup_failures_stop_before_classification(self):
        for setup, status in [('mktemp() { return 28; }', 28), ('mkfifo() { return 29; }', 29)]:
            with self.subTest(status=status):
                result = self.run_hook(setup, conditional=True)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(list(self.scratch.iterdir()), [])

    def test_failure_waits_for_both_handlers_and_cleans_scratch(self):
        setup = r'''
isELF() { echo "$BASHPID" > "$HOME/classifier"; exit 73; }
xargs() { cat >/dev/null; sleep .1; echo "$BASHPID" > "$HOME/handler-$BASHPID"; }
'''
        result = self.run_hook(setup, conditional=True)
        self.assertEqual(result.returncode, 73, result.stderr)
        records = [self.root / 'classifier', *self.root.glob('handler-*')]
        self.assertEqual(len(records), 3)
        for record in records:
            with self.assertRaises(ProcessLookupError):
                os.kill(int(record.read_text().strip()), 0)
        self.assertEqual(list(self.scratch.iterdir()), [])


if __name__ == '__main__':
    unittest.main()
