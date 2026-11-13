"""iOS package checks: the bundled 「Oink 复制」 shortcuts and the app extensions' versions."""
import datetime
import importlib.util
import plistlib
import re
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support'))
import ios_package
from ios_package import CheckError

IOS = ROOT / 'apps/mobile/ios'


def load_generator():
    spec = importlib.util.spec_from_file_location('make_shortcut', IOS / 'Shortcuts/make_shortcut.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class BundledShortcut(unittest.TestCase):
    def test_signing_certificate_outlives_the_next_three_months(self):
        # Fails three months before the anyone-signature's certificate ends:
        # re-sign with apps/mobile/ios/Shortcuts/sign-shortcut.sh (macOS, iCloud).
        expires = ios_package.check_shortcut(months=3)
        self.assertGreater(expires, datetime.datetime.now(datetime.timezone.utc))

    def test_an_expiring_certificate_fails_the_check(self):
        expires = ios_package.check_shortcut(months=0)
        with self.assertRaisesRegex(CheckError, 'sign-shortcut.sh'):
            ios_package.check_shortcut(now=expires - datetime.timedelta(days=60), months=3)
        ios_package.check_shortcut(now=expires - datetime.timedelta(days=100), months=3)

    def copy_shortcuts(self, work):
        work = Path(work)
        for path in ios_package.SHORTCUTS.glob('OinkCopy*'):
            (work / path.name).write_bytes(path.read_bytes())
        return work

    def test_signed_file_is_the_one_recorded_for_the_source(self):
        with tempfile.TemporaryDirectory() as work:
            work = self.copy_shortcuts(work)
            changed = work / 'OinkCopy-teal.unsigned.plist'
            changed.write_bytes(changed.read_bytes().replace(b'Oink', b'Oinx', 1))
            with self.assertRaisesRegex(CheckError, 'OinkCopy-teal.unsigned.plist changed since'):
                ios_package.check_shortcut(directory=work)

    def test_every_source_is_signed_and_nothing_else_is_recorded(self):
        with tempfile.TemporaryDirectory() as work:
            work = self.copy_shortcuts(work)
            (work / 'OinkCopy-violet.shortcut').unlink()
            with self.assertRaisesRegex(CheckError, 'OinkCopy-violet.shortcut is not the recorded'):
                ios_package.check_shortcut(directory=work)
        with tempfile.TemporaryDirectory() as work:
            work = self.copy_shortcuts(work)
            for name in ('OinkCopy-violet.unsigned.plist', 'OinkCopy-violet.shortcut'):
                (work / name).unlink()
            with self.assertRaisesRegex(CheckError, 'records OinkCopy-violet.shortcut without a source'):
                ios_package.check_shortcut(directory=work)

    def test_the_sources_are_what_the_generator_writes(self):
        generator = load_generator()
        pairs = ios_package.variants()
        self.assertEqual([source.name for source, _ in pairs],
                         sorted(f'OinkCopy-{color}.unsigned.plist' for color in generator.COLORS))
        for color in generator.COLORS:
            self.assertEqual(generator.source_path(color).read_bytes(), generator.source_bytes(color))

    def test_variants_differ_only_in_the_tile_colour(self):
        generator = load_generator()
        icons = set()
        bodies = set()
        for color in generator.COLORS:
            workflow = plistlib.loads(generator.source_path(color).read_bytes())
            icon = workflow.pop('WFWorkflowIcon')
            self.assertEqual(icon['WFWorkflowIconGlyphNumber'], 61593)
            icons.add(icon['WFWorkflowIconStartColor'])
            bodies.add(plistlib.dumps(workflow))
        self.assertEqual(len(icons), len(generator.COLORS))
        self.assertEqual(len(bodies), 1)

    def test_signature_names_no_apple_id(self):
        # `--mode anyone` only: people-who-know-me embeds AppleIDCertificateChain and
        # AppleIDValidationRecord (account identifiers) and cannot be imported by others.
        for _, signed in ios_package.variants():
            data = signed.read_bytes()
            length = int.from_bytes(data[8:12], 'little')
            context = plistlib.loads(data[12:12 + length])
            self.assertEqual(set(context), {'SigningCertificateChain'}, signed.name)

    def test_action_is_the_app_intent_of_the_app(self):
        # The action identifier is a protocol constant: <bundle id>.<intent type name>.
        # Renaming CopyLatestSharedClipboardIntent or changing the bundle id turns the
        # bundled shortcut (and every shortcut users built) into an unknown action.
        source = ios_package.variants()[0][0]
        workflow = plistlib.loads(source.read_bytes())
        action = workflow['WFWorkflowActions'][0]
        descriptor = action['WFWorkflowActionParameters']['AppIntentDescriptor']
        intent, bundle = descriptor['AppIntentIdentifier'], descriptor['BundleIdentifier']
        project = (IOS / 'project.yml').read_text()
        self.assertEqual(re.search(r'^\s+OINK_BUNDLE_ID: (\S+)$', project, re.M).group(1), bundle)
        swift = (IOS / 'App/ReceiveClipboardIntent.swift').read_text()
        self.assertRegex(swift, rf'\bstruct {re.escape(intent)}: AppIntent\b')
        self.assertEqual(action['WFWorkflowActionIdentifier'], f'{bundle}.{intent}')
        # The second action copies the first one's result.
        copy = workflow['WFWorkflowActions'][1]['WFWorkflowActionParameters']
        self.assertEqual(copy['WFInput']['Value']['OutputUUID'], action['WFWorkflowActionParameters']['UUID'])

    def test_every_accent_imports_a_bundled_variant(self):
        # make_shortcut.ACCENTS, ShortcutFile.colorByAccent and OinkPalette.Accent agree,
        # and the app bundles every variant.
        generator = load_generator()
        theme = (IOS / 'Shared/OinkTheme.swift').read_text()
        cases = re.search(r'enum Accent: String[^{]*\{\s*case ([^\n]+)', theme).group(1)
        accents = {name.strip() for name in cases.split(',')}
        self.assertEqual(set(generator.ACCENTS), accents)
        self.assertEqual(set(generator.ACCENTS.values()), set(generator.COLORS))
        swift = (IOS / 'App/ShortcutFile.swift').read_text()
        table = re.search(r'colorByAccent: \[String: String\] = \[(.*?)\]', swift, re.S).group(1)
        self.assertEqual(dict(re.findall(r'"(\w+)": "(\w+)"', table)), generator.ACCENTS)
        self.assertIn('"OinkCopy-\\($0)"', swift)
        project = (IOS / 'project.yml').read_text()
        for color in generator.COLORS:
            self.assertIn(f'- path: Shortcuts/OinkCopy-{color}.shortcut\n        buildPhase: resources', project)


class ExtensionVersions(unittest.TestCase):
    def make_app(self, root, app=('0.4.0', '400001'), extensions=None):
        app_dir = Path(root) / 'Oink.app'
        (app_dir / 'PlugIns').mkdir(parents=True)
        def info(path, bundle, version):
            path.mkdir(parents=True, exist_ok=True)
            (path / 'Info.plist').write_bytes(plistlib.dumps({
                'CFBundleIdentifier': bundle, 'CFBundleShortVersionString': version[0], 'CFBundleVersion': version[1]}))
        info(app_dir, 'com.hachiwii.oink', app)
        for name, (bundle, version) in (extensions or {}).items():
            extension = app_dir / 'PlugIns' / f'{name}.appex'
            info(extension, bundle, version)
            if bundle.endswith('.broadcast'):
                path = extension / 'Info.plist'
                own = plistlib.loads(path.read_bytes())
                own['NSExtension'] = {'NSExtensionPointIdentifier': 'com.apple.broadcast-services-upload',
                    'NSExtensionAttributes': {'RPBroadcastProcessMode': 'RPBroadcastProcessModeSampleBuffer'}}
                path.write_bytes(plistlib.dumps(own))
        return app_dir

    def test_matching_extensions_pass(self):
        with tempfile.TemporaryDirectory() as work:
            app = self.make_app(work, extensions={
                'OinkShare': ('com.hachiwii.oink.share', ('0.4.0', '400001')),
                'OinkControls': ('com.hachiwii.oink.controls', ('0.4.0', '400001')),
                'OinkBroadcast': ('com.hachiwii.oink.broadcast', ('0.4.0', '400001'))})
            self.assertEqual(ios_package.check_extensions(app), ['OinkBroadcast.appex', 'OinkControls.appex', 'OinkShare.appex'])

    def test_a_different_build_or_bundle_fails(self):
        with tempfile.TemporaryDirectory() as work:
            app = self.make_app(work, extensions={
                'OinkShare': ('com.hachiwii.oink.share', ('0.4.0', '400001')),
                'OinkControls': ('com.hachiwii.oink.controls', ('0.4.0', '1'))})
            with self.assertRaisesRegex(CheckError, r'OinkControls\.appex: 0\.4\.0 \(1\)'):
                ios_package.check_extensions(app)
        with tempfile.TemporaryDirectory() as work:
            app = self.make_app(work, extensions={'OinkControls': ('com.example.controls', ('0.4.0', '400001'))})
            with self.assertRaisesRegex(CheckError, 'not under com.hachiwii.oink'):
                ios_package.check_extensions(app)

    def test_missing_broadcast_fails(self):
        with tempfile.TemporaryDirectory() as work:
            app = self.make_app(work, extensions={
                'OinkShare': ('com.hachiwii.oink.share', ('0.4.0', '400001')),
                'OinkControls': ('com.hachiwii.oink.controls', ('0.4.0', '400001'))})
            with self.assertRaisesRegex(CheckError, 'missing extension .*broadcast'):
                ios_package.check_extensions(app)

    def test_wrong_replaykit_mode_fails(self):
        with tempfile.TemporaryDirectory() as work:
            app = self.make_app(work, extensions={
                'OinkBroadcast': ('com.hachiwii.oink.broadcast', ('0.4.0', '400001'))})
            path = app / 'PlugIns/OinkBroadcast.appex/Info.plist'
            own = plistlib.loads(path.read_bytes())
            own['NSExtension']['NSExtensionAttributes']['RPBroadcastProcessMode'] = 'wrong'
            path.write_bytes(plistlib.dumps(own))
            with self.assertRaisesRegex(CheckError, 'invalid ReplayKit'):
                ios_package.check_extensions(app)

    def test_an_app_without_extensions_fails(self):
        with tempfile.TemporaryDirectory() as work:
            with self.assertRaisesRegex(CheckError, 'no app extensions'):
                ios_package.check_extensions(self.make_app(work))


if __name__ == '__main__':
    unittest.main()
