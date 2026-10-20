import sys
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / "build-support"), str(ROOT / "build-support/update")]
"""Exercise release orchestration without running npm, Cargo or signing tools."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2] / 'build-support'
with patch.object(sys, 'path', [str(SCRIPTS), *sys.path]):
    SPEC = importlib.util.spec_from_file_location('release_builder', SCRIPTS / 'build.py')
    B = importlib.util.module_from_spec(SPEC)
    SPEC.loader.exec_module(B)


class ReleaseBuild(unittest.TestCase):
    def test_build_sources_and_agreement(self):
        for explicit, environment in [
                ('17', {}), (None, {'OINK_RELEASE_BUILD': '17'}),
                (None, {'RELEASE_BUILD': '17'}),
                ('17', {'OINK_RELEASE_BUILD': '017', 'RELEASE_BUILD': '17'})]:
            with self.subTest(explicit=explicit, environment=environment):
                self.assertEqual(B.release_build(explicit, environment), 17)
        self.assertEqual(B.release_build('2147483647', {}), 2147483647)

    def test_missing_invalid_or_conflicting_build_fails_before_commands(self):
        cases = [(None, {}), ('17', {'RELEASE_BUILD': '16'}),
                 (None, {'OINK_RELEASE_BUILD': '16', 'RELEASE_BUILD': '17'})]
        for value in ['', '0', '-1', '1.5', '2147483648', 'x', ' 17', '1_000']:
            cases.extend([(value, {}), (None, {'RELEASE_BUILD': value}),
                          (None, {'OINK_RELEASE_BUILD': value})])
        for explicit, environment in cases:
            argv = ['build.py', '--target', 'x86_64-pc-windows-msvc',
                    '--source-tree-sha256', 'a' * 64, '--version', B.IDENTITY['version']]
            if explicit is not None:
                argv.extend(['--build', explicit])
            with self.subTest(explicit=explicit, environment=environment), \
                    patch.dict(os.environ, environment, clear=True), \
                    patch.object(sys, 'argv', argv), patch.object(B, 'run') as run, \
                    patch.object(B.subprocess, 'check_output') as read, \
                    contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as result:
                    B.main()
                self.assertEqual(result.exception.code, 2)
                run.assert_not_called()
                read.assert_not_called()

    def test_every_release_command_and_record_use_same_build(self):
        targets = ['x86_64-pc-windows-msvc', 'aarch64-apple-darwin']
        cases = [(target, commit, origin, fail_health) for target in targets for commit in (None, '0123abcd' * 5)
                 for origin in ([None, 'http://127.0.0.1:8765'] if 'windows' in target else [None])
                 for fail_health in ([False, True] if origin else [False])]
        for target, commit, origin, fail_health in cases:
            with self.subTest(target=target, commit=commit), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                output = root / 'target' / target / 'release'
                output.mkdir(parents=True)
                windows = 'windows' in target
                roles = ['ui', 'agent', 'service', 'update_worker', 'crash_collector'] if windows else ['ui', 'agent', 'backend', 'crash_collector']
                for role in roles:
                    (output / (B.IDENTITY['binaries'][role] + ('.exe' if windows else ''))).write_bytes(b'fixture')
                (root / 'packaging').mkdir()
                (root / 'packaging/identity.json').write_text(json.dumps(B.IDENTITY))
                metadata = {'workspace_members': ['agent', 'collector'], 'packages': [
                    {'id': 'agent', 'name': 'oink-agent'},
                    {'id': 'collector', 'name': 'oink-crash'},
                    {'id': 'registry', 'name': 'external-cache-preserved'},
                ]}
                argv = ['build.py', '--target', target, '--source-tree-sha256', 'a' * 64, '--version', B.IDENTITY['version']]
                argv += ['--build', '17']
                if commit:
                    argv += ['--source-commit', commit]
                argv += [] if windows else ['--sparkle-cache', str(root / 'sparkle')]
                if origin:
                    argv += ['--windows-update-test-feed', origin]
                if fail_health:
                    argv += ['--windows-update-test-fail-health']
                with patch.dict(os.environ, {'RELEASE_BUILD': '17', 'CARGO_HOME': str(root / 'cargo-home')}, clear=True), \
                        patch.object(sys, 'argv', argv), patch.object(B, 'ROOT', root), \
                        patch.object(B, 'source_tree_sha256', return_value='a' * 64), patch.object(B.platform, 'system', return_value='Windows' if windows else 'Darwin'), \
                        patch.object(B, 'run') as run, \
                        patch.object(B.subprocess, 'check_output', return_value=json.dumps(metadata)) as read, \
                        patch.object(B, 'prepare_sparkle', return_value=root / 'sparkle'), \
                        patch('macos_imports.verify_imports') as imports, \
                        patch('windows_version.set_display_version', return_value=True) as resources:
                    B.main()
                calls = run.call_args_list
                self.assertEqual(len(calls), 4)
                for call in [*calls, *read.call_args_list]:
                    self.assertEqual(call.kwargs['env']['OINK_RELEASE_BUILD'], '17')
                    self.assertEqual(call.kwargs['env']['RELEASE_BUILD'], '17')
                    self.assertEqual(call.kwargs['env']['OINK_RELEASE_VERSION'], B.release_version(B.IDENTITY['version'], commit))
                    self.assertEqual(call.kwargs['env']['OINK_SOURCE_COMMIT'], commit or '')
                self.assertEqual(calls[2].args[:2], ('cargo', 'clean'))
                self.assertIn('--release', calls[2].args)
                self.assertIn('oink-agent', calls[2].args)
                self.assertIn('oink-crash', calls[2].args)
                self.assertIn('opusic-sys', calls[2].args)
                self.assertNotIn('external-cache-preserved', calls[2].args)
                self.assertIn('tauri', calls[3].args)
                config = json.loads(calls[3].args[calls[3].args.index('--config') + 1])
                self.assertEqual(config['version'], B.IDENTITY['version'])
                forwarded = calls[3].args[calls[3].args.index('--config') + 3:]
                expected_packages = ['--locked', '-p', 'oink-ui', '-p', 'oink-agent', '-p', 'oink-crash']
                if windows:
                    expected_packages += ['-p', 'oink-winsvc']
                self.assertEqual(list(forwarded), expected_packages)
                self.assertFalse(any(call.args[:2] == ('cargo', 'build') for call in calls))
                features = calls[3].args[calls[3].args.index('--features') + 1]
                expected_features = 'custom-protocol' if windows else 'custom-protocol,oink-agent/sparkle'
                self.assertEqual(features, expected_features + (',oink-winsvc/test-feed' if origin else ''))
                flags = calls[3].kwargs['env']['CARGO_ENCODED_RUSTFLAGS']
                self.assertIn('--remap-path-prefix=', flags)
                self.assertEqual(calls[3].kwargs['env']['CARGO_PROFILE_RELEASE_DEBUG'], '1')
                self.assertEqual(calls[3].kwargs['env']['CARGO_PROFILE_RELEASE_STRIP'], 'none')
                if windows:
                    imports.assert_not_called()
                    self.assertEqual(resources.call_count, len(roles))
                    self.assertIn('+crt-static', calls[3].kwargs['env']['CARGO_ENCODED_RUSTFLAGS'])
                    self.assertIn('/PDBALTPATH:%_PDB%', flags)
                    self.assertNotIn(B.IDENTITY['binaries']['backend'] + '.exe', json.loads((output / 'release-build.json').read_text())['files'])
                else:
                    imports.assert_called_once_with(
                        [output / B.IDENTITY['binaries'][role] for role in ['ui', 'agent', 'crash_collector', 'backend']],
                        B.IDENTITY['macos_minimum'])
                    self.assertEqual(calls[3].kwargs['env']['CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO'], 'packed')
                record = json.loads((output / 'release-build.json').read_text())
                self.assertEqual(record['version'], B.release_version(B.IDENTITY['version'], commit))
                self.assertEqual(record['source_commit'], commit)
                self.assertEqual(record['update_test_feed'], origin)
                self.assertEqual(record['update_test_fail_health'], fail_health)
                self.assertEqual(record['build'], 17)
                self.assertIs(type(record['build']), int)

    def test_changed_sources_rejected_after_unified_build(self):
        argv = ['build.py', '--target', 'x86_64-pc-windows-msvc',
                '--source-tree-sha256', 'a' * 64, '--build', '17']
        metadata = {'workspace_members': [], 'packages': []}
        with tempfile.TemporaryDirectory() as temp, \
                patch.dict(os.environ, {'CARGO_TARGET_DIR': temp, 'CARGO_HOME': temp}, clear=True), \
                patch.object(sys, 'argv', argv), \
                patch.object(B, 'source_tree_sha256', side_effect=['a' * 64, 'b' * 64]), \
                patch.object(B.subprocess, 'check_output', return_value=json.dumps(metadata)), \
                patch.object(B, 'run') as run, \
                patch('windows_version.set_display_version', return_value=True):
            with self.assertRaisesRegex(RuntimeError, 'Source tree changed during build'):
                B.main()
            self.assertEqual(len(run.call_args_list), 4)
            self.assertFalse((Path(temp) / 'x86_64-pc-windows-msvc/release/release-build.json').exists())


if __name__ == '__main__':
    unittest.main()
