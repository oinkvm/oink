"""Package identity and installer migration in temporary fixtures; never use HAL."""
import importlib.util
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support'))
sys.path.insert(0, str(ROOT / 'packaging/macos'))
SPEC = importlib.util.spec_from_file_location('mic_content', ROOT / 'packaging/macos/content.py')
CONTENT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONTENT)


class MicrophoneIdentity(unittest.TestCase):
    def test_package_requires_oink_identity_and_provenance(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory) / 'Oink.app'
            payload = app / 'Contents/Resources/AudioPlugin'
            plugin = payload / 'OinkMicrophone.driver'
            (plugin / 'Contents').mkdir(parents=True)
            (payload / 'install.sh').write_text('fixture')
            info = dict(CFBundleIdentifier='com.hachiwii.oink.microphone.driver',
                        CFBundleExecutable='OinkMicrophone', OinkGitSHA='a'*64, OinkPluginVersion='b'*64)
            with patch.object(CONTENT, 'verify_signature') as verify:
                (plugin / 'Contents/Info.plist').write_bytes(plistlib.dumps(info))
                CONTENT.verify_microphone(app, 'a'*64, '893V37X2CW')
                verify.assert_called_once_with(plugin, '893V37X2CW')
                for key in info:
                    changed = dict(info, **{key: 'wrong'})
                    (plugin / 'Contents/Info.plist').write_bytes(plistlib.dumps(changed))
                    with self.subTest(key=key), self.assertRaisesRegex(RuntimeError, 'identity/provenance'):
                        CONTENT.verify_microphone(app, 'a'*64, '893V37X2CW')

    def fixture(self, root, mode, present, fail_verification=False, symlink=None, wrong_identifier=False):
        hal = root / 'HAL'; hal.mkdir()
        backups = root / 'backups'
        source = root / 'source.driver'
        (source / 'Contents').mkdir(parents=True)
        (source / 'Contents/Info.plist').write_bytes(plistlib.dumps(dict(CFBundleIdentifier='com.hachiwii.oink.microphone.driver')))
        (source / 'new-payload').write_text('new')
        current = hal / 'OinkMicrophone.driver'
        legacy = hal / 'AcrossMicrophone.driver'
        for name in present:
            path = hal / name; (path / 'Contents').mkdir(parents=True); (path / 'old-payload').write_text(name)
            identifier = 'com.hachiwii.oink.microphone.driver' if name == 'OinkMicrophone.driver' else 'com.across.microphone.driver'
            (path / 'Contents/Info.plist').write_bytes(plistlib.dumps(dict(CFBundleIdentifier='com.vendor.other' if wrong_identifier else identifier)))
        if symlink:
            (hal / symlink).symlink_to(source, target_is_directory=True)
        commands = root / 'bin'; commands.mkdir()
        # Only the fixture's command shims can perform installer side effects.
        for name, body in {
            'codesign': 'exit 9' if fail_verification else 'echo TeamIdentifier=893V37X2CW >&2',
            'chown': ':',
            'ditto': '/bin/cp -R "$1" "$2"',
            'killall': 'echo "$*" >> "'+str(root / 'restarts')+'"',
        }.items():
            file = commands / name; file.write_text('#!/bin/bash\n'+body+'\n'); file.chmod(0o755)
        text = (ROOT / 'apps/audio-plugin/install.sh').read_text()
        guard = "[[ $EUID == 0 ]] || { echo 'User: run sudo -v, then the documented locked installation command.' >&2; exit 2; }"
        self.assertIn(guard, text)
        text = text.replace(guard, ': # No privilege is used in this fixture.')
        text = text.replace('export PATH=/usr/bin:/bin:/usr/sbin:/sbin', 'export PATH="'+str(commands)+':/usr/bin:/bin"')
        text = text.replace('/Library/Audio/Plug-Ins/HAL', str(hal)).replace('/Library/Application Support/Oink/AudioPluginBackups', str(backups))
        self.assertNotIn('/Library/', text)
        runner = root / 'install-fixture.sh'; runner.write_text(text)
        result = subprocess.run(['/bin/bash', str(runner), mode, str(source)], env={'OINK_MIC_INSTALL_VERIFIED':'1'}, capture_output=True, text=True)
        return result, current, legacy, backups

    @unittest.skipUnless(sys.platform == 'darwin', 'macOS installer fixture uses PlistBuddy')
    def test_install_and_uninstall_clean_both_names_in_one_operation(self):
        for mode in ['install', 'uninstall']:
            for present in [[], ['AcrossMicrophone.driver'], ['OinkMicrophone.driver'], ['AcrossMicrophone.driver','OinkMicrophone.driver']]:
                with self.subTest(mode=mode, present=present), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    result, current, legacy, backups = self.fixture(root, mode, present)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertFalse(legacy.exists())
                    self.assertEqual(current.exists(), mode == 'install')
                    if mode == 'install': self.assertTrue((current / 'new-payload').is_file())
                    self.assertEqual(len(list(backups.glob('*.driver'))), len(present))
                    self.assertEqual((root / 'restarts').read_text().splitlines(), ['coreaudiod'])

    @unittest.skipUnless(sys.platform == 'darwin', 'macOS installer fixture uses PlistBuddy')
    def test_bad_payload_or_either_symlink_cannot_remove_existing_driver(self):
        for bad_signature, symlink in [(True,None),(False,'OinkMicrophone.driver'),(False,'AcrossMicrophone.driver')]:
            with self.subTest(signature=bad_signature,symlink=symlink), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                present = ['OinkMicrophone.driver' if symlink == 'AcrossMicrophone.driver' else 'AcrossMicrophone.driver']
                result, _, _, _ = self.fixture(root,'install',present,bad_signature,symlink)
                self.assertNotEqual(result.returncode,0)
                self.assertTrue((root / 'HAL' / present[0] / 'old-payload').is_file())
                self.assertFalse((root / 'restarts').exists())

    @unittest.skipUnless(sys.platform == 'darwin', 'macOS installer fixture uses PlistBuddy')
    def test_uninstall_refuses_foreign_identity_signature_and_symlink_before_mutation(self):
        for bad_signature, wrong_id, symlink in [(True, False, None), (False, True, None),
                                                 (False, False, 'AcrossMicrophone.driver')]:
            with self.subTest(signature=bad_signature, wrong_id=wrong_id, symlink=symlink), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                result, current, _, backups = self.fixture(root, 'uninstall', ['OinkMicrophone.driver'], bad_signature, symlink, wrong_id)
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue((current / 'old-payload').exists())
                self.assertFalse(backups.exists())
                self.assertFalse((root / 'restarts').exists())

    @unittest.skipUnless(sys.platform == 'darwin', 'macOS installer fixture uses PlistBuddy')
    def test_uninstall_leaves_third_party_bundles_untouched(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result, _, _, backups = self.fixture(root, 'uninstall', ['OtherVendor.driver'])
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((root / 'HAL/OtherVendor.driver/old-payload').exists())
            self.assertFalse(backups.exists())
