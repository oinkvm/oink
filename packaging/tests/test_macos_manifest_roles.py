"""Exercise the production content gate with eight distinct linked identities."""
import importlib.util
import json
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import uuid

ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / 'packaging/macos'), str(ROOT / 'build-support')]
SPEC = importlib.util.spec_from_file_location('macos_manifest_content', ROOT / 'packaging/macos/content.py')
C = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(C)
from identity import IDENTITY
from native_image_fixtures import GUID, macho_image
from report_manifest import image_identity, write_manifest
from versioning import macos_version_fields, release_version


class MacManifestRoles(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.app = Path(self.tmp.name) / 'Oink.app'
        self.b = IDENTITY['binaries']; self.mac = IDENTITY['macos']
        self.group = self.mac['team_id'] + '.' + self.mac['app_group_suffix']
        self.helper = self.app / 'Contents/Helpers/Oink.app'
        self.extension = self.app / 'Contents/PlugIns' / self.mac['file_provider_bundle']
        self.finder = self.app / 'Contents/PlugIns/OinkFinderSync.appex'
        self.paste = self.app / 'Contents/Helpers/OinkPaste.app'
        self.images = {k: self.app / 'Contents/MacOS' / self.b[k]
                       for k in ('backend', 'agent', 'crash_collector', 'file_provider_manager')}
        self.images.update(ui=self.helper / 'Contents/MacOS' / self.b['ui'],
                           file_provider_extension=self.extension / 'Contents/MacOS' / self.b['file_provider_extension'],
                           finder_sync=self.finder / 'Contents/MacOS/OinkFinderSync',
                           paste_helper=self.paste / 'Contents/MacOS/OinkPaste')
        for role, path in self.images.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            data = macho_image()
            self.assertEqual(data.count(GUID.bytes), 1)
            path.write_bytes(data.replace(GUID.bytes, uuid.uuid5(uuid.NAMESPACE_DNS, role).bytes))
        version = release_version(IDENTITY['version'], 'b' * 40, 137)
        self.metadata = {'version': version, 'build': 137, 'source_commit': 'b' * 40,
                         'source_tree_sha256': 'a' * 64, 'update_policy': 'sparkle'}
        trust = json.loads((ROOT / 'packaging/update-trust.json').read_text())['mac']
        self.metadata['update_policy'] = 'sparkle' if trust['enabled'] else 'manual-only'
        for bundle, ident, executable in [
                (self.app, IDENTITY['bundle_id'], self.b['backend']),
                (self.helper, self.mac['ui_id'], self.b['ui']),
                (self.extension, self.mac['file_provider_id'], self.b['file_provider_extension']),
                (self.finder, IDENTITY['bundle_id'] + '.findersync', 'OinkFinderSync'),
                (self.paste, IDENTITY['bundle_id'] + '.paste', 'OinkPaste')]:
            info = dict(CFBundleIdentifier=ident, CFBundleExecutable=executable,
                        CFBundleName='Oink', CFBundleDisplayName='Oink',
                        LSMinimumSystemVersion='14.2', NSAudioCaptureUsageDescription='Share audio.',
                        SUPublicEDKey=trust['public_key'],
                        NSExtension={'NSExtensionPointIdentifier': 'com.apple.FinderSync'},
                        **macos_version_fields(version, 137))
            info[self.mac['app_group_key']] = self.group
            (bundle / 'Contents').mkdir(parents=True, exist_ok=True)
            (bundle / 'Contents/Info.plist').write_bytes(plistlib.dumps(info))
        resource = self.app / 'Contents/Resources'; resource.mkdir()
        (resource / IDENTITY['metadata_file']).write_text(json.dumps(self.metadata))
        (self.helper / 'Contents/Resources').mkdir()
        (self.app / 'Contents/Frameworks/Sparkle.framework').mkdir(parents=True)
        self.set_report(['backend', 'agent', 'ui', 'file_provider_extension'])

    def set_report(self, roles):
        manifest = self.app / 'Contents/Resources/report-manifest.json'
        write_manifest(manifest, 'a' * 64, self.metadata['version'], 137,
                       list(zip(['agent', 'agent', 'ui_rust', 'file_provider'],
                                [self.images[k] for k in roles])))
        (self.helper / 'Contents/Resources/report-manifest.json').write_bytes(manifest.read_bytes())

    def check(self):
        group_paths = {self.app, self.images['backend'], self.images['file_provider_manager'],
                       self.extension, self.images['file_provider_extension'], self.finder,
                       self.images['finder_sync'], self.paste, self.images['paste_helper']}
        def signed(*args):
            rights = {'com.apple.security.application-groups': [self.group]} if args[-1] in group_paths else {}
            if args[-1] == self.app:
                rights['com.apple.security.device.audio-input'] = True
            return subprocess.CompletedProcess(args, 0, plistlib.dumps(rights).decode(),
                                               'Identifier=' + IDENTITY['bundle_id'] + '.file-provider-manager\n')
        def native(args, **kwargs):
            if args[0] == 'lipo': return 'arm64'
            if args[0] == 'otool': return '    minos 14.2\n'
            raise AssertionError(args)
        with patch.object(C, 'private_run', side_effect=signed), \
                patch.object(C.subprocess, 'check_output', side_effect=native), \
                patch.object(C, 'verify_signature'), patch.object(C, 'verify_imports'), \
                patch.object(C, 'verify_localizations'), patch.object(C, 'verify_microphone'):
            return C.verify_app(self.app, IDENTITY)

    def test_extra_finder_and_paste_components_do_not_replace_report_roles(self):
        components = self.check()
        self.assertEqual(len(components), 8)
        by_role = {r['role']: r for r in components}
        self.assertEqual(by_role.keys(), self.images.keys())
        for role, image in self.images.items():
            with self.subTest(role=role):
                self.assertEqual(by_role[role]['path'], image.relative_to(self.app).as_posix())
                self.assertEqual(by_role[role]['image_identity'], image_identity(image))

    def test_finder_and_paste_cannot_impersonate_ui_and_file_provider(self):
        self.set_report(['backend', 'agent', 'finder_sync', 'paste_helper'])
        with self.assertRaisesRegex(RuntimeError, 'crash report manifest'):
            self.check()

    def test_additional_components_need_valid_linked_identity(self):
        for role in ('finder_sync', 'paste_helper'):
            image = self.images[role]
            original = image.read_bytes()
            with self.subTest(role=role):
                image.write_bytes(b'not a linked image')
                with self.assertRaises(ValueError):
                    self.check()
            image.write_bytes(original)
