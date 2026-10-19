"""Checks that the packaging launch gate rejects early exit and hung startup."""
import contextlib
import io
from pathlib import Path
import sys
import tempfile
import unittest

from macos_launch_smoke import launch_smoke


class LaunchSmokeTests(unittest.TestCase):
    def run_stub(self, body):
        with tempfile.TemporaryDirectory() as directory:
            backend = Path(directory) / 'backend'
            backend.write_text(f'#!{sys.executable}\n' + body)
            backend.chmod(0o700)
            with contextlib.redirect_stdout(io.StringIO()):
                launch_smoke(backend, seconds=1.5, environment={})

    def test_clean_early_exit_still_fails(self):
        with self.assertRaisesRegex(RuntimeError, 'exit 0'):
            self.run_stub('pass\n')

    def test_abort_fails(self):
        with self.assertRaisesRegex(RuntimeError, 'exited during'):
            self.run_stub('import os, signal\nos.kill(os.getpid(), signal.SIGKILL)\n')

    def test_alive_without_ready_host_fails(self):
        with self.assertRaisesRegex(RuntimeError, 'did not complete'):
            self.run_stub('import time\ntime.sleep(60)\n')

    def test_ready_host_requires_clean_shutdown(self):
        body = '''import os, signal, time
from pathlib import Path
home = Path(os.environ['OINK_HOME'])
assert list(home.iterdir()) == []
assert 'OINK_IPC_PORT' not in os.environ
signal.signal(signal.SIGTERM, lambda *_: exit(0))
(home / 'ipc.json').write_text('{}')
(home / 'launch-smoke-ready').write_text('ready')
time.sleep(60)
'''
        self.run_stub(body)
        with self.assertRaisesRegex(RuntimeError, 'shutdown failed'):
            self.run_stub(body.replace('exit(0)', 'exit(1)'))


if __name__ == '__main__':
    unittest.main()
