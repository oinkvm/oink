from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]


class Entrypoints(unittest.TestCase):
    def test_commands_start_without_inheriting_the_test_import_path(self):
        for path in ('build-support/package.py', 'build-support/build.py',
                     'packaging/windows/build.py', 'packaging/macos/build.py',
                     'packaging/macos/check.py', 'packaging/macos/notarize.py',
                     'build-support/public_release.py', 'build-support/check-report-configuration.py'):
            with self.subTest(path=path):
                result = subprocess.run([sys.executable, str(ROOT / path), '--help'],
                                        cwd=ROOT, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
