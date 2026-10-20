import importlib.util
import json
import plistlib
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / 'packaging/macos'), str(ROOT / 'build-support')]
SPEC = importlib.util.spec_from_file_location('macos_content', ROOT / 'packaging/macos/content.py')
C = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(C)
from localizations import DECLARATIONS, STRINGS, write_localizations


class MacProductNames(unittest.TestCase):
    def test_host_and_helper_must_match_product_name_and_app_basename(self):
        info = {'CFBundleName': 'Oink', 'CFBundleDisplayName': 'Oink'}
        for bundle in [Path('Oink.app'), Path('Oink.app/Contents/Helpers/Oink.app')]:
            C.verify_product_name(bundle, info, 'Oink')
            for key in info:
                for wrong in ['Oink UI', None]:
                    with self.subTest(bundle=bundle, key=key, wrong=wrong):
                        with self.assertRaisesRegex(RuntimeError, 'user-visible bundle names'):
                            C.verify_product_name(bundle, {**info, key: wrong}, 'Oink')
        with self.assertRaisesRegex(RuntimeError, 'basename'):
            C.verify_product_name(Path('Oink.app/Contents/Helpers/Oink UI.app'), info, 'Oink')

    def test_tauri_helper_source_names_and_identity(self):
        info = plistlib.loads((ROOT / 'apps/oink/src-tauri/Info.plist').read_bytes())
        C.verify_product_name(Path('Oink.app'), info, 'Oink')
        config = json.loads((ROOT / 'apps/oink/src-tauri/tauri.conf.json').read_text())
        self.assertEqual(config['productName'], 'Oink')
        self.assertEqual(config['identifier'], 'com.hachiwii.oink.ui')

    def test_both_localized_names_cannot_override_product(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundle = Path(tmp)
            info = dict(DECLARATIONS)
            for language in STRINGS:
                for key in ['CFBundleName', 'CFBundleDisplayName']:
                    write_localizations(bundle)
                    resource = bundle / f'Contents/Resources/{language}.lproj/InfoPlist.strings'
                    localized = plistlib.loads(resource.read_bytes())
                    self.assertEqual(localized[key], 'Oink')
                    for wrong in ['Oink UI', None]:
                        modified = {**localized, key: wrong} if wrong else {k: v for k, v in localized.items() if k != key}
                        resource.write_bytes(plistlib.dumps(modified))
                        with self.subTest(language=language, key=key, wrong=wrong):
                            with self.assertRaisesRegex(RuntimeError, 'missing or incorrect'):
                                C.verify_localizations(bundle, info)


class MacLocalizations(unittest.TestCase):
    def test_host_helper_and_extension_resources(self):
        with tempfile.TemporaryDirectory() as tmp:
            for name, permissions in [('host', True), ('helper', False), ('extension', False)]:
                bundle = Path(tmp) / name
                write_localizations(bundle, permissions)
                info = {**DECLARATIONS, **(STRINGS['en'] if permissions else {})}
                C.verify_localizations(bundle, info, permissions)
                with self.assertRaisesRegex(RuntimeError, 'must declare'):
                    C.verify_localizations(bundle, {**info, 'CFBundleLocalizations': ['en']}, permissions)
                for language in ['zh-Hans', 'ja', 'ko']:
                    resource = bundle / f'Contents/Resources/{language}.lproj/InfoPlist.strings'
                    original = resource.read_bytes()
                    resource.unlink()
                    with self.subTest(bundle=name, language=language), self.assertRaisesRegex(RuntimeError, 'missing or incorrect'):
                        C.verify_localizations(bundle, info, permissions)
                    resource.write_bytes(original)

    def test_dev_helper_advertises_same_languages(self):
        info = plistlib.loads((ROOT / 'apps/oink/src-tauri/Info.plist').read_bytes())
        for key, expected in DECLARATIONS.items():
            self.assertEqual(info[key], expected)

    def test_permission_translations_and_fallbacks(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundle = Path(tmp)
            write_localizations(bundle, permissions=True)
            info = {**DECLARATIONS, **STRINGS['en']}
            self.assertEqual(list(STRINGS), DECLARATIONS['CFBundleLocalizations'])
            for language in ['zh-Hans', 'ja', 'ko']:
                for key in ['NSAudioCaptureUsageDescription', 'NSLocalNetworkUsageDescription']:
                    self.assertNotEqual(STRINGS['en'][key], STRINGS[language][key])
            with self.assertRaisesRegex(RuntimeError, 'fallback'):
                C.verify_localizations(bundle, {**info, 'NSLocalNetworkUsageDescription': 'wrong'}, True)
            resource = bundle / 'Contents/Resources/zh-Hans.lproj/InfoPlist.strings'
            resource.write_bytes(plistlib.dumps(STRINGS['en']))
            with self.assertRaisesRegex(RuntimeError, 'missing or incorrect'):
                C.verify_localizations(bundle, info, True)


class MacTccPolicy(unittest.TestCase):
    def test_production_bundles_forbid_local_network_and_other_ats_exceptions(self):
        C.verify_update_transport({})
        C.verify_update_transport({'SUFeedURL':'https://updates.oink.hachiwii.com/mac/v1/mac-2026-01/stable/appcast.xml'})
        for info in [{'NSAppTransportSecurity':{}}, {'NSAppTransportSecurity':{'NSAllowsLocalNetworking':True}},
                     {'SUFeedURL':'http://127.0.0.1:9000/appcast.xml'}, {'SUFeedURL':'https://example.test/'}]:
            with self.subTest(info=info), self.assertRaises(RuntimeError):C.verify_update_transport(info)

    def test_audio_capture_requirements_belong_to_backend(self):
        info = {'CFBundleExecutable': 'backend', 'NSAudioCaptureUsageDescription': 'Share system audio.'}
        entitlements = {'com.apple.security.device.audio-input': True}
        C.verify_audio_capture_host(info, entitlements, 'backend')
        for key, value in [('CFBundleExecutable', 'ui'), ('NSAudioCaptureUsageDescription', ''),
                           ('NSAudioCaptureUsageDescription', None)]:
            with self.subTest(key=key, value=value), self.assertRaises(RuntimeError):
                C.verify_audio_capture_host({**info, key: value}, entitlements, 'backend')
        for value in [None, False]:
            with self.subTest(entitlement=value), self.assertRaises(RuntimeError):
                C.verify_audio_capture_host(info, {'com.apple.security.device.audio-input': value}, 'backend')

    def test_product_permissions_do_not_include_camera_or_automation(self):
        C.verify_tcc_policy({
            'NSAudioCaptureUsageDescription': 'Share system audio.',
            'NSLocalNetworkUsageDescription': 'Connect devices.',
        }, {'com.apple.security.application-groups': ['TEAM.example.group']})
        C.verify_tcc_policy({}, {})  # The UI has no entitlements.

    def test_unused_entitlement_or_usage_description_fails_content_gate(self):
        for entitlement, description in (
                ('com.apple.security.automation.apple-events', 'NSAppleEventsUsageDescription'),
                ('com.apple.security.device.camera', 'NSCameraUsageDescription')):
            with self.subTest(entitlement=entitlement):
                with self.assertRaisesRegex(RuntimeError, 'Unused TCC permission'):
                    C.verify_tcc_policy({}, {entitlement: True})
                with self.assertRaisesRegex(RuntimeError, 'Unused TCC permission'):
                    C.verify_tcc_policy({description: 'Unused permission.'}, {})


if __name__ == '__main__':
    unittest.main()
