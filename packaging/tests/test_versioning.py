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
from identity import IDENTITY, artifact_stem, decode_build, encode_build, signpath_configuration, windows_product_version
from provenance import verify, digest
import versioning
from versioning import release_version, local_source_commit, macos_version_fields, package_build
from windows_version import display_version_resource, set_display_version

COMMIT = '0123abcd' * 5
INHOUSE = IDENTITY['inhouse_version']
BUILD = encode_build(INHOUSE, 17)
DISPLAY = f"{INHOUSE}-inhouse.17"


class PhoneVersions(unittest.TestCase):
    """Android/iOS in-house packages take their fields from the PC rule (build-support/mobile_version.py)."""

    def setUp(self):
        spec = importlib.util.spec_from_file_location('mobile_version', ROOT / 'build-support/mobile_version.py')
        self.mobile = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.mobile)

    def test_first_040_phone_build(self):
        with patch.dict(versioning.IDENTITY, inhouse_version='0.4.0'):
            self.assertEqual(self.mobile.mobile_version(self.mobile.parse_build('000400001')),
                             {'build': 400001, 'version_name': '0.4.0-inhouse.1', 'marketing_version': '0.4.0'})
            self.assertEqual(self.mobile.mobile_version(400017)['version_name'], '0.4.0-inhouse.17')

    def test_a_build_of_another_version_is_rejected(self):
        with patch.dict(versioning.IDENTITY, inhouse_version='0.4.0'):
            for build in (306002, 115, 400000, 401001, 2**31):
                with self.subTest(build=build), self.assertRaises(ValueError):
                    self.mobile.mobile_version(build)
        for text in ('', '-400001', '400001.0', ' 400001', '4e5'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                self.mobile.parse_build(text)

    def test_command_line_for_gradle_and_xcodebuild(self):
        build = encode_build(INHOUSE, 3)
        script = str(ROOT / 'build-support/mobile_version.py')
        gradle = subprocess.run([sys.executable, script, '--channel', 'beta', str(build)], capture_output=True, text=True, check=True)
        self.assertEqual(json.loads(gradle.stdout),
                         {'build': build, 'version_name': f'{INHOUSE}-beta.3', 'marketing_version': INHOUSE})
        xcode = subprocess.run([sys.executable, script, '--channel', 'beta', '--xcode', str(build)], capture_output=True, text=True, check=True)
        self.assertEqual(xcode.stdout.split(), [f'MARKETING_VERSION={INHOUSE}', f'CURRENT_PROJECT_VERSION={build}', f'OINK_DISPLAY_VERSION={INHOUSE}-beta.3'])
        refused = subprocess.run([sys.executable, script, '306002' if INHOUSE != '0.3.6' else '400001'],
                                 capture_output=True, text=True)
        self.assertEqual(refused.returncode, 1)
        self.assertIn('encodes', refused.stderr)

    def test_android_build_reads_the_helper(self):
        gradle = (ROOT / 'apps/mobile/android/app/build.gradle.kts').read_text()
        self.assertIn('build-support/mobile_version.py', gradle)


class BuildNumbers(unittest.TestCase):
    def test_build_encodes_version_and_sequence(self):
        self.assertEqual(encode_build('0.3.6', 2), 306002)
        self.assertEqual(decode_build(306002), ('0.3.6', 2))
        self.assertEqual(encode_build('1.0.0', 1), 10_000_001)
        self.assertEqual(encode_build('214.74.83', 647), 2**31 - 1)
        for version, sequence in [('0.0.0', 1), ('0.3.6', 999), ('2.99.99', 500), ('12.0.7', 3)]:
            self.assertEqual(decode_build(encode_build(version, sequence)), (version, sequence))

    def test_out_of_range_versions_sequences_and_builds_are_rejected(self):
        for version in ('0.100.0', '0.3.100', '0.3.6-beta.1', '0.03.6', '0.3', '214.74.84', None):
            with self.subTest(version=version), self.assertRaises(ValueError):
                encode_build(version, 1)
        for sequence in (0, 1000, -1, True, '1', None):
            with self.subTest(sequence=sequence), self.assertRaises(ValueError):
                encode_build('0.3.6', sequence)
        with self.assertRaises(ValueError):
            encode_build('214.74.83', 648)
        for build in (0, 306000, 2**31, -1, True, '306002', None):
            with self.subTest(build=build), self.assertRaises(ValueError):
                decode_build(build)

    def test_new_builds_order_above_the_old_global_counter_and_by_version(self):
        # 113 was the last globally reserved build (public 0.3.5); old clients compare builds only.
        self.assertGreater(encode_build('0.3.6', 1), 113)
        ordered = [('0.3.6', 1), ('0.3.6', 2), ('0.3.6', 999), ('0.3.7', 1), ('0.4.0', 1), ('1.0.0', 1)]
        builds = [encode_build(*item) for item in ordered]
        self.assertEqual(builds, sorted(set(builds)))
        msi = [tuple(map(int, windows_product_version(build).split('.'))) for build in [113, *builds]]
        self.assertEqual(msi, sorted(set(msi)))

    def test_package_build_uses_inhouse_or_manifest_version(self):
        with patch.dict(versioning.IDENTITY, version='0.3.5', inhouse_version='0.3.6'):
            self.assertEqual(package_build(2, True), 306002)
            self.assertEqual(package_build(4, False), 305004)
        with patch.dict(versioning.IDENTITY, version='0.4.0-beta.2'):
            self.assertEqual(package_build(3, False), 400003)


class LocalVersions(unittest.TestCase):
    def test_artifact_names_use_only_display_version_and_match_signpath(self):
        import fnmatch
        import xml.etree.ElementTree as ET
        pattern = ET.fromstring(signpath_configuration()).find('.//{*}msi-file').get('path')
        self.assertEqual(signpath_configuration(), (ROOT / '.signpath/artifact-configurations/windows-msi-v1.xml').read_bytes())
        for version in ('0.3.3', '0.3.3-beta.1', '0.3.3-inhouse.97', '0.3.6-inhouse.2'):
            for platform, arch, extensions in [('windows', 'x64', ['msi']), ('macos', 'arm64', ['dmg', 'zip']), ('macos', 'x64', ['dmg', 'zip'])]:
                for ext in extensions:
                    name = f'{artifact_stem(version, platform, arch)}.{ext}'
                    self.assertEqual(name, f'Oink-{version}-{platform}-{arch}.{ext}')
                    if platform == 'windows':
                        self.assertTrue(fnmatch.fnmatchcase(name, pattern))
        self.assertTrue(fnmatch.fnmatchcase('Oink-0.3.2-93-windows-x64.msi', pattern))
        self.assertNotEqual(artifact_stem(release_version('0.3.3', COMMIT, BUILD), 'windows', 'x64'),
                            artifact_stem(release_version('0.3.3', COMMIT, BUILD + 1), 'windows', 'x64'))

    def test_build_label_is_independent_of_source_commit(self):
        self.assertEqual(release_version('0.1.0-beta.1', COMMIT, BUILD), DISPLAY)
        self.assertEqual(release_version('0.1.0-beta.1', '01234567' * 5, BUILD), DISPLAY)
        self.assertEqual(release_version('0.1.0-beta.2', None, 100_003), '0.1.0-beta.2')
        for commit in ('0123abcd', 'A' * 40, 'z' * 40, 'a' * 39, 'a' * 41, ''):
            with self.assertRaises(ValueError):
                release_version('0.1.0-beta.1', commit, BUILD)
        self.assertEqual(release_version('0.1.0', COMMIT, BUILD), DISPLAY)
        self.assertEqual(release_version('0.1.0', None, 100_001), '0.1.0')

    def test_label_uses_the_sequence_and_never_the_numeric_build(self):
        with patch.dict(versioning.IDENTITY, inhouse_version='0.3.6'):
            self.assertEqual(release_version('0.3.5', COMMIT, 306002), '0.3.6-inhouse.2')
            self.assertEqual(release_version('0.3.5', COMMIT, 306999), '0.3.6-inhouse.999')
        self.assertEqual(release_version('0.3.6', None, 306004), '0.3.6')
        self.assertEqual(release_version('0.3.6-beta.1', None, 306003), '0.3.6-beta.1')

    def test_build_must_encode_the_packaged_version(self):
        with patch.dict(versioning.IDENTITY, inhouse_version='0.3.6'):
            for build in (None, 0, -1, 2**31, True, '306002', 306000, 113, 305001, 307001):
                with self.subTest(build=build), self.assertRaises(ValueError):
                    release_version('0.3.5', COMMIT, build)
        for build in (113, 305004, 307001, 306000):
            with self.subTest(public=build), self.assertRaises(ValueError):
                release_version('0.3.6', None, build)
        with self.assertRaises(ValueError):
            release_version('0.3.6-beta.1', None, 305001)
        for base in ('0.3', '0.3.6-rc.1', '0.3.6-inhouse.1'):
            with self.subTest(base=base), self.assertRaises(ValueError):
                release_version(base, None, 306001)

    def test_inhouse_uses_the_configured_version_and_public_versions_stay_exact(self):
        for public in ('0.1.4', '0.1.5', '1.2.9'):
            self.assertEqual(release_version(public, COMMIT, BUILD), DISPLAY)
        with patch.dict(versioning.IDENTITY, inhouse_version='0.4.0'):
            self.assertEqual(release_version('0.1.5', COMMIT, 400_017), '0.4.0-inhouse.17')
        for public, build in (('0.1.4', 104_001), ('0.1.5', 105_001), ('0.2.0-beta.1', 200_001)):
            self.assertEqual(release_version(public, None, build), public)

    def test_inhouse_configuration_is_numeric_and_does_not_affect_public_builds(self):
        for invalid in (None, '', '0.2', '0.2.0-beta.1', '01.2.0', 7):
            with self.subTest(invalid=invalid), patch.dict(versioning.IDENTITY, inhouse_version=invalid):
                with self.assertRaisesRegex(ValueError, 'inhouse_version'):
                    release_version('0.1.5', COMMIT, BUILD)
                self.assertEqual(release_version('0.1.5', None, 105_001), '0.1.5')

    def test_numeric_bundle_fields_and_msi_order_ignore_the_commit(self):
        self.assertEqual(macos_version_fields(DISPLAY, BUILD), dict(
            CFBundleShortVersionString=INHOUSE, CFBundleVersion=str(BUILD), CFBundleGetInfoString=DISPLAY))
        with self.assertRaisesRegex(ValueError, 'CFBundleVersion'):
            macos_version_fields(DISPLAY, BUILD + 1_000)
        builds = (17, 18, 65535, 65537, 306002, 16777217, 2147483647)
        versions = [tuple(map(int, windows_product_version(build).split('.'))) for build in builds]
        self.assertEqual(versions, sorted(set(versions)))
        self.assertEqual(windows_product_version(17), '0.0.17')
        self.assertEqual(windows_product_version(306002), '0.4.43858')
        with self.assertRaises(ValueError):
            windows_product_version(306000)

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
                          build=BUILD, target='target', identity_sha256=digest(ROOT / 'packaging/identity.json'), files={})
            (root / 'release-build.json').write_text(json.dumps(record))
            verify(root, 'a' * 64, DISPLAY, 'target', BUILD, COMMIT)
            for commit in (None, COMMIT[:8] + 'f' * 32):
                with self.assertRaisesRegex(RuntimeError, 'source commit'):
                    verify(root, 'a' * 64, DISPLAY, 'target', BUILD, commit)

    def test_local_entrypoint_propagates_commit_to_build_and_package_public_flow_opts_out(self):
        spec = importlib.util.spec_from_file_location('local_packager', ROOT / 'build-support/package.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        for public in (False, True):
            with self.subTest(public=public), tempfile.TemporaryDirectory() as temp:
                build = encode_build(IDENTITY['version'] if public else INHOUSE, 17)
                argv = ['package.py', '--platform', 'windows', '--sequence', '17', '--output-root', temp]
                if public:
                    argv += ['--public-version']
                else:
                    receipt = dict(schema=1, channel='inhouse', repository='oinkvm/oink', workflow='ci.yml', ref='refs/heads/0.4', version=INHOUSE, sequence=17, build=build, source_commit=COMMIT, request_id='a'*32, run_id=1)
                    path = Path(temp)/'reservation.json'; path.write_text(json.dumps(receipt))
                    argv = ['package.py', '--platform', 'windows', '--output-root', temp, '--build-reservation', str(path), '--source-commit', COMMIT]
                with patch.object(sys, 'argv', argv), patch.object(module.platform, 'system', return_value='Windows'), \
                        patch.object(module.platform, 'machine', return_value='AMD64'), \
                        patch.object(module, 'local_source_commit', return_value=COMMIT) as resolve, \
                        patch.object(module.package_notes, 'generate', return_value={'schema':1,'kind':'public' if public else 'inhouse','build':build,'source_commit':None if public else COMMIT,'notes':{'en':'Fixture notes'}}), \
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
                    self.assertEqual(command[command.index('--build') + 1], str(build))
                    if public:
                        self.assertNotIn('--source-commit', command)
                    else:
                        self.assertEqual(command[command.index('--source-commit') + 1], COMMIT)
                package_command = run.call_args_list[-1].args[0]
                embedded = Path(package_command[package_command.index('--update-notes-file') + 1])
                self.assertEqual(json.loads(embedded.read_text())['notes'], {'en':'Fixture notes'})
                record = json.loads((Path(temp) / ('a' * 64) / 'windows-x64/build-environment.json').read_text())
                self.assertEqual(record['version'], IDENTITY['version'] if public else DISPLAY)


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
