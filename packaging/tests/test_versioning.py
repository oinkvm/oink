"""Display labels must not participate in numeric package/update ordering."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support'))
from identity import IDENTITY, windows_product_version
from provenance import verify, digest
import versioning
from versioning import release_version, local_source_commit, macos_version_fields
from windows_version import display_version_resource, set_display_version

COMMIT = '0123abcd' * 5
DISPLAY = f"{IDENTITY['inhouse_version']}-inhouse.17"


class LocalVersions(unittest.TestCase):
    def test_build_label_is_independent_of_source_commit(self):
        self.assertEqual(release_version('0.1.0-beta.1', COMMIT, 17), DISPLAY)
        self.assertEqual(release_version('0.1.0-beta.1', '01234567' * 5, 17),
                         f"{IDENTITY['inhouse_version']}-inhouse.17")
        self.assertEqual(release_version('0.1.0-beta.2'), '0.1.0-beta.2')
        for commit in ('0123abcd', 'A' * 40, 'z' * 40, 'a' * 39, 'a' * 41, ''):
            with self.assertRaises(ValueError):
                release_version('0.1.0-beta.1', commit, 17)
        self.assertEqual(release_version('0.1.0', COMMIT, 17), DISPLAY)
        self.assertEqual(release_version('0.1.0'), '0.1.0')

    def test_inhouse_requires_a_valid_build(self):
        for build in (None, 0, -1, 2**31, True, '44'):
            with self.subTest(build=build), self.assertRaises(ValueError):
                release_version('0.1.0', COMMIT, build)
        self.assertEqual(release_version('0.1.0', COMMIT, 44),
                         f"{IDENTITY['inhouse_version']}-inhouse.44")

    def test_inhouse_uses_the_configured_version_and_public_versions_stay_exact(self):
        for public in ('0.1.4', '0.1.5', '1.2.9'):
            self.assertEqual(release_version(public, COMMIT, 17), DISPLAY)
        with patch.dict(versioning.IDENTITY, inhouse_version='0.4.0'):
            self.assertEqual(release_version('0.1.5', COMMIT, 17), '0.4.0-inhouse.17')
        for public in ('0.1.4', '0.1.5', '0.2.0-beta.1'):
            self.assertEqual(release_version(public), public)

    def test_inhouse_configuration_is_numeric_and_does_not_affect_public_builds(self):
        for invalid in (None, '', '0.2', '0.2.0-beta.1', '01.2.0', 7):
            with self.subTest(invalid=invalid), patch.dict(versioning.IDENTITY, inhouse_version=invalid):
                with self.assertRaisesRegex(ValueError, 'inhouse_version'):
                    release_version('0.1.5', COMMIT, 17)
                self.assertEqual(release_version('0.1.5'), '0.1.5')

    def test_numeric_bundle_fields_and_msi_order_ignore_the_commit(self):
        self.assertEqual(macos_version_fields(DISPLAY, 17), dict(
            CFBundleShortVersionString=IDENTITY['inhouse_version'], CFBundleVersion='17', CFBundleGetInfoString=DISPLAY))
        builds = (17, 18, 65535, 65536, 16777216, 2147483647)
        versions = [tuple(map(int, windows_product_version(build).split('.'))) for build in builds]
        self.assertEqual(versions, sorted(set(versions)))
        self.assertEqual(windows_product_version(17), '0.0.17')

    def test_checkout_commit_must_match_and_product_sources_must_be_clean(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            def git(*args):
                return subprocess.check_output(['git', *args], cwd=root, stderr=subprocess.DEVNULL).decode().strip()
            git('init', '-q')
            (root / 'source.txt').write_text('source')
            git('add', 'source.txt')
            git('-c', 'user.name=version-test', '-c', 'user.email=version-test@localhost',
                '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture')
            head = git('rev-parse', 'HEAD')
            self.assertEqual(local_source_commit(root=root), head)
            self.assertEqual(local_source_commit(head, root), head)
            with self.assertRaisesRegex(ValueError, 'differs'):
                local_source_commit('0' * 40, root)
            (root / 'evidence').mkdir()
            (root / 'evidence/note').write_text('excluded')
            self.assertEqual(local_source_commit(root=root), head)
            for staged in (False, True):
                (root / 'source.txt').write_text('changed')
                if staged:
                    git('add', 'source.txt')
                with self.assertRaisesRegex(ValueError, 'committed'):
                    local_source_commit(root=root)
            git('reset', '--hard', '-q', 'HEAD')
            (root / 'new.rs').write_text('untracked product source')
            with self.assertRaisesRegex(ValueError, 'committed'):
                local_source_commit(root=root)

    def test_archives_require_an_explicit_full_commit(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            with self.assertRaisesRegex(ValueError, 'source archives require'):
                local_source_commit(root=root)
            self.assertEqual(local_source_commit(COMMIT, root), COMMIT)

    def test_packaging_rejects_a_different_commit_even_with_matching_tree_and_label(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            record = dict(source_tree_sha256='a' * 64, version=DISPLAY, source_commit=COMMIT,
                          build=17, target='target', identity_sha256=digest(ROOT / 'packaging/identity.json'), files={})
            (root / 'release-build.json').write_text(json.dumps(record))
            verify(root, 'a' * 64, DISPLAY, 'target', 17, COMMIT)
            for commit in (None, COMMIT[:8] + 'f' * 32):
                with self.assertRaisesRegex(RuntimeError, 'source commit'):
                    verify(root, 'a' * 64, DISPLAY, 'target', 17, commit)

    def test_local_entrypoint_propagates_commit_to_build_and_package_public_flow_opts_out(self):
        spec = importlib.util.spec_from_file_location('local_packager', ROOT / 'build-support/package.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        for public in (False, True):
            with self.subTest(public=public), tempfile.TemporaryDirectory() as temp:
                argv = ['package.py', '--platform', 'windows', '--build', '17', '--output-root', temp]
                argv += ['--public-version'] if public else ['--source-commit', COMMIT]
                with patch.object(sys, 'argv', argv), patch.object(module.platform, 'system', return_value='Windows'), \
                        patch.object(module.platform, 'machine', return_value='AMD64'), \
                        patch.object(module, 'local_source_commit', return_value=COMMIT) as resolve, \
                        patch.object(module.package_notes, 'generate', return_value={'schema':1,'kind':'public' if public else 'inhouse','build':17,'source_commit':None if public else COMMIT,'notes':{'en':'Fixture notes'}}), \
                        patch.object(module, 'source_inventory', return_value=[]), \
                        patch.object(module, 'source_tree_sha256', return_value='a' * 64), \
                        patch.object(module.subprocess, 'check_output', side_effect=['rustc 1.99.0 fixture', 'v24.14.1', '11.11.0']), \
                        patch.object(module.subprocess, 'run') as run, contextlib.redirect_stdout(io.StringIO()):
                    module.main()
                self.assertEqual(run.call_count, 2)
                if public:
                    resolve.assert_not_called()
                else:
                    resolve.assert_called_once_with(COMMIT)
                for call in run.call_args_list:
                    command = call.args[0]
                    if public:
                        self.assertNotIn('--source-commit', command)
                    else:
                        self.assertEqual(command[command.index('--source-commit') + 1], COMMIT)
                package_command = run.call_args_list[-1].args[0]
                embedded = Path(package_command[package_command.index('--update-notes-file') + 1])
                self.assertEqual(json.loads(embedded.read_text())['notes'], {'en':'Fixture notes'})
                record = json.loads((Path(temp) / ('a' * 64) / 'windows-x64/build-environment.json').read_text())
                self.assertEqual(record['version'], IDENTITY['version'] if public else release_version(IDENTITY['version'], COMMIT, 17))


class WindowsResources(unittest.TestCase):
    def test_display_strings_change_without_modifying_numeric_fields_or_other_strings(self):
        def block(key, value=b'', kind=1, children=()):
            data = bytearray(struct.pack('<HHH', 0, len(value) // (2 if kind else 1), kind) + (key + '\0').encode('utf-16le'))
            data.extend(bytes(-len(data) % 4))
            data.extend(value)
            for child in children:
                data.extend(bytes(-len(data) % 4))
                data.extend(child)
            struct.pack_into('<H', data, 0, len(data))
            return bytes(data)
        def text(key, value):
            return block(key, (value + '\0').encode('utf-16le'))
        fixed = struct.pack('<13I', 0xFEEF04BD, *range(12))
        source = block('VS_VERSION_INFO', fixed, 0, [block('StringFileInfo', children=[
            block('040904b0', children=[text('FileVersion', '0.1.0-beta.1'),
                text('ProductVersion', '0.1.0-beta.1'), text('ProductName', 'Oink')])])])
        updated = display_version_resource(source, DISPLAY)
        self.assertEqual(updated.count((DISPLAY + '\0').encode('utf-16le')), 2)
        self.assertNotIn('beta.1'.encode('utf-16le'), updated)
        self.assertIn(fixed, updated)
        self.assertIn(('Oink\0').encode('utf-16le'), updated)
        self.assertEqual(display_version_resource(updated, DISPLAY), updated)

    @unittest.skipUnless(os.name == 'nt', 'Windows PE resource APIs')
    def test_explorer_reads_updated_version_from_a_private_executable_copy(self):
        with tempfile.TemporaryDirectory() as temp:
            copy = Path(temp) / 'version-test.exe'
            shutil.copy2(sys.executable, copy)
            self.assertTrue(set_display_version(copy, DISPLAY))
            code = "$v=[Diagnostics.FileVersionInfo]::GetVersionInfo('" + str(copy).replace("'", "''") + "'); @($v.FileVersion,$v.ProductVersion) | ConvertTo-Json -Compress"
            values = json.loads(subprocess.check_output(['powershell', '-NoProfile', '-Command', code], text=True))
            self.assertEqual(values, [DISPLAY, DISPLAY])
