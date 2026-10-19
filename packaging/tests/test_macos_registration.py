"""Private smoke registration cleanup must never select the installed bundle."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
import macos_registration as M


class RegistrationCleanup(unittest.TestCase):
    def test_only_exact_private_paths_are_unregistered(self):
        with tempfile.TemporaryDirectory(suffix='.noindex') as directory:
            app = Path(directory).resolve() / 'Oink.app'
            extension = app / 'Contents/PlugIns/OinkFileProvider.appex'
            installed = '/Applications/Oink.app/Contents/PlugIns/OinkFileProvider.appex'
            commands = []
            queries = iter([str(extension) + '\n' + installed, installed])
            def run(command, **kwargs):
                commands.append(command)
                output = next(queries) if command[1] == '-mAvvv' else ''
                return subprocess.CompletedProcess(command, 0, output, '')
            with patch.object(M.subprocess, 'run', side_effect=run):
                M.unregister_bundle(app, 'OinkFileProvider.appex', 'test.provider')
            self.assertEqual(commands, [
                ['pluginkit', '-mAvvv', '-i', 'test.provider'],
                ['pluginkit', '-r', str(extension)],
                [M.LSREGISTER, '-u', str(app)],
                ['pluginkit', '-mAvvv', '-i', 'test.provider'],
            ])

    def test_unregistered_extension_after_early_launch_failure_is_allowed(self):
        with tempfile.TemporaryDirectory(suffix='.noindex') as directory, \
                patch.object(M.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, '(no matches)', '')) as run:
            app = Path(directory).resolve() / 'Oink.app'
            M.unregister_bundle(app, 'OinkFileProvider.appex', 'test.provider')
            self.assertEqual(len(run.call_args_list), 3)
            self.assertEqual(run.call_args_list[1].args[0], [M.LSREGISTER, '-u', str(app)])

    def test_remaining_registration_and_tool_errors_fail(self):
        with tempfile.TemporaryDirectory(suffix='.noindex') as directory:
            app = Path(directory).resolve() / 'Oink.app'
            for code, output in [(0, str(app / 'Contents/PlugIns/OinkFileProvider.appex')), (1, '')]:
                with self.subTest(code=code), patch.object(M.subprocess, 'run', return_value=subprocess.CompletedProcess([], code, output, '')):
                    with self.assertRaises(RuntimeError):
                        M.unregister_bundle(app, 'OinkFileProvider.appex', 'test.provider')

    def test_nonprivate_bundle_rejected_before_commands(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(M.subprocess, 'run') as run:
            with self.assertRaises(ValueError):
                M.unregister_bundle(Path(directory) / 'Oink.app', 'OinkFileProvider.appex', 'test.provider')
            run.assert_not_called()

    def test_cleanup_always_runs_after_success_or_smoke_failure(self):
        for failed in [False, True]:
            with self.subTest(failed=failed), patch.object(M, 'unregister_bundle') as cleanup:
                try:
                    with M.temporary_registration('private-app', 'extension', 'identifier'):
                        if failed:
                            raise RuntimeError('smoke failed')
                except RuntimeError:
                    self.assertTrue(failed)
                cleanup.assert_called_once_with('private-app', 'extension', 'identifier')


if __name__ == '__main__':
    unittest.main()
