"""Windows update publication and test-build boundaries; no installs or real keys."""
import base64
import contextlib
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / 'build-support'), str(ROOT / 'build-support/update')]
import feed
from test_feed import EPOCH, PUB
import update_qualification as qualification


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'build-support/update' / filename)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


setup = module('windows_key_setup', 'setup-windows-key.py')
published = module('windows_published', 'windows-published.py')


class KeySetup(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.config = Path(self.temporary.name) / 'trust.json'
        self.original = feed.read(ROOT / 'packaging/update-trust.json')
        self.original['windows'][0].update(public_key=None, enabled=False)
        feed.write(self.config, self.original)
        for mock in [patch.object(setup.sys, 'platform', 'darwin'), patch.dict(os.environ, {}, clear=True),
                     patch.object(setup.shutil, 'which', side_effect=lambda tool: tool)]:
            mock.start(); self.addCleanup(mock.stop)
        keychain_patch = patch.object(setup, 'LoginKeychain')
        self.keychain_class = keychain_patch.start()
        self.addCleanup(keychain_patch.stop)
        self.keychain = self.keychain_class.return_value.__enter__.return_value
        self.keychain.exists.return_value = False
        self.private = b'PUBLIC TEST FIXTURE, NOT A GENERATED KEY'
        self.public = bytes.fromhex('302a300506032b6570032100') + base64.b64decode(PUB)

    def test_private_bytes_only_reach_keychain_crypto_and_gh_stdin_public_only_is_written(self):
        order = []
        def backup(value):
            self.assertEqual(value, self.private)
            order.append('backup')
        self.keychain.store.side_effect = backup
        def command(argv, input=None, **kwargs):
            self.assertNotIn(self.private.decode(), repr(argv))
            self.assertEqual(kwargs['stderr'], subprocess.DEVNULL)
            self.assertEqual(kwargs['stdout'], subprocess.PIPE)
            if argv[1:] == ['version']: output = b'OpenSSL 3.5.0'
            elif argv[1:3] == ['secret', 'list']: output = b'[]'
            elif argv[1:] == ['genpkey', '-algorithm', 'ED25519']: output = self.private
            elif argv[1:] == ['pkey', '-pubout', '-outform', 'DER']:
                self.assertEqual(input, self.private); output = self.public
            elif argv[1:3] == ['secret', 'set']:
                self.assertEqual(input, self.private)
                self.assertEqual(argv[3:], [setup.SECRET, '--repo', self.original['repository'], '--env', 'release'])
                order.append('upload'); output = b''
            else: self.fail(str(argv))
            return subprocess.CompletedProcess(argv, 0, output)
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch.object(setup.subprocess, 'run', side_effect=command), \
                contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            setup.setup(self.config)
        expected = copy.deepcopy(self.original)
        expected['windows'][0]['public_key'] = PUB
        self.assertEqual(feed.read(self.config), expected)
        self.assertEqual(order, ['backup', 'upload'])
        self.assertNotIn(self.private, self.config.read_bytes())
        self.assertNotIn(self.private.decode(), stdout.getvalue() + stderr.getvalue())
        self.assertEqual(sorted(p.name for p in Path(self.temporary.name).iterdir()), ['trust.json'])

    def test_existing_epoch_or_secret_never_generates_a_key(self):
        configured = copy.deepcopy(self.original)
        configured['windows'][0]['public_key'] = PUB
        feed.write(self.config, configured)
        with patch.object(setup, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'already_configured'): setup.setup(self.config)
            run.assert_not_called()
        feed.write(self.config, self.original)
        with patch.object(setup, 'run', side_effect=[b'OpenSSL 3.5.0', json.dumps([{'name': setup.SECRET}]).encode()]) as run:
            with self.assertRaisesRegex(ValueError, 'secret_already_exists'): setup.setup(self.config)
            self.assertEqual(run.call_count, 2)
        self.keychain_class.assert_not_called()
        self.assertEqual(feed.read(self.config), self.original)

    def test_existing_keychain_item_stops_before_key_generation(self):
        self.keychain.exists.return_value = True
        with patch.object(setup, 'run', side_effect=[b'OpenSSL 3.5.0', b'[]']) as run:
            with self.assertRaisesRegex(ValueError, 'keychain_item_already_exists'): setup.setup(self.config)
            self.assertEqual(run.call_count, 2)
        self.keychain.store.assert_not_called()
        self.assertEqual(feed.read(self.config), self.original)

    def test_backup_failure_blocks_upload_and_public_pin(self):
        self.keychain.store.side_effect = ValueError('keychain_backup_failed')
        with patch.object(setup, 'run', side_effect=[b'OpenSSL 3.5.0', b'[]', self.private, self.public]) as run:
            with self.assertRaisesRegex(ValueError, 'keychain_backup_failed'): setup.setup(self.config)
            self.assertEqual(run.call_count, 4)
        self.assertEqual(feed.read(self.config), self.original)

    def test_upload_failure_keeps_backup_and_does_not_write_public_pin(self):
        with patch.object(setup, 'run', side_effect=[b'OpenSSL 3.5.0', b'[]', self.private, self.public,
                                                   ValueError('key_setup_command_failed')]):
            with self.assertRaisesRegex(ValueError, 'command_failed'): setup.setup(self.config)
        self.keychain.store.assert_called_once_with(self.private)
        self.assertEqual(feed.read(self.config), self.original)

    def test_other_platform_setup_is_preserved_and_changed_windows_slot_refused(self):
        for platform in ['mac', 'windows']:
            feed.write(self.config, self.original)
            changed = copy.deepcopy(self.original)
            slot = changed['mac'] if platform == 'mac' else changed['windows'][0]
            slot['public_key'] = PUB
            self.keychain.store.side_effect = lambda _: feed.write(self.config, changed)
            with patch.object(setup, 'run', side_effect=[b'OpenSSL 3.5.0', b'[]', self.private, self.public, b'']), \
                    contextlib.redirect_stdout(io.StringIO()):
                if platform == 'windows':
                    with self.assertRaisesRegex(ValueError, 'trust_changed'): setup.setup(self.config)
                    self.assertEqual(feed.read(self.config), changed)
                else:
                    setup.setup(self.config)
                    changed['windows'][0]['public_key'] = PUB
                    self.assertEqual(feed.read(self.config), changed)

    def test_non_mac_or_ci_stops_before_subprocess_or_keychain(self):
        for platform, environment in [('win32', {}), ('darwin', {'CI': 'true'}),
                                      ('darwin', {'GITHUB_ACTIONS': 'true'})]:
            with patch.object(setup.sys, 'platform', platform), patch.dict(os.environ, environment, clear=True), \
                    patch.object(setup, 'run') as run:
                with self.assertRaisesRegex(ValueError, 'outside_ci'): setup.setup(self.config)
                run.assert_not_called()
        self.keychain_class.assert_not_called()


class KeychainBridge(unittest.TestCase):
    def test_login_keychain_lookup_add_and_duplicate_use_only_framework_memory(self):
        security, core = MagicMock(), MagicMock()
        security.SecKeychainOpen.return_value = 0
        security.SecKeychainFindGenericPassword.return_value = -25300
        security.SecKeychainAddGenericPassword.return_value = 0
        fixture = b'nonsecret test bytes'
        def add(*args):
            self.assertEqual(args[1:5], (len(setup.KEYCHAIN_SERVICE), setup.KEYCHAIN_SERVICE.encode(),
                                        len(setup.KEYCHAIN_ACCOUNT), setup.KEYCHAIN_ACCOUNT.encode()))
            self.assertEqual(setup.ctypes.string_at(args[6], args[5]), fixture)
            self.assertIsNone(args[7])
            return 0
        security.SecKeychainAddGenericPassword.side_effect = add
        with patch.object(setup.ctypes, 'CDLL', side_effect=[security, core]), \
                patch.object(setup, 'run', return_value=b'  "/mock/login.keychain-db"\n') as run:
            with setup.LoginKeychain() as keychain:
                run.assert_called_once_with(['/usr/bin/security', 'login-keychain'])
                self.assertFalse(keychain.exists())
                self.assertEqual(security.SecKeychainFindGenericPassword.call_args.args[-3:], (None, None, None))
                security.SecKeychainFindGenericPassword.return_value = 0
                self.assertTrue(keychain.exists())
                keychain.store(fixture)
                password = security.SecKeychainAddGenericPassword.call_args.args[6]
                self.assertEqual(setup.ctypes.string_at(password, len(fixture)), b'\0' * len(fixture))
                security.SecKeychainAddGenericPassword.side_effect = None
                security.SecKeychainAddGenericPassword.return_value = -25299
                with self.assertRaisesRegex(ValueError, 'already_exists'): keychain.store(fixture)
                security.SecKeychainAddGenericPassword.return_value = -1
                with self.assertRaisesRegex(ValueError, 'backup_failed'): keychain.store(fixture)
                security.SecKeychainFindGenericPassword.return_value = -1
                with self.assertRaisesRegex(ValueError, 'access_failed'): keychain.exists()
            core.CFRelease.assert_called_once()
        security.SecKeychainOpen.assert_called_once()
        self.assertEqual(security.SecKeychainOpen.call_args.args[0], b'/mock/login.keychain-db')
        security.SecKeychainDelete.assert_not_called()
        security.SecKeychainItemModifyAttributesAndData.assert_not_called()


class ProductionBoundary(unittest.TestCase):
    def test_loopback_is_explicit_bounded_and_never_allowed_in_public_binary(self):
        self.assertEqual(qualification.origin('http://127.0.0.1:8765'), 'http://127.0.0.1:8765')
        for value in ['http://localhost:8765', 'http://127.0.0.2:8765', 'https://127.0.0.1:8765',
                      'http://127.0.0.1:0', 'http://127.0.0.1:65536', 'http://127.0.0.1:8765/x',
                      'http://127.0.0.1:8765?x', 'http://127.0.0.1:8765@evil']:
            with self.assertRaises(ValueError): qualification.origin(value)
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / 'image.exe'
            binary.write_bytes(b'production bytes')
            qualification.verify_binaries([binary])
            binary.write_bytes(b'http://127.0.0.1:8765/' + qualification.MARKER)
            with self.assertRaisesRegex(ValueError, 'TEST update endpoint'): qualification.verify_binaries([binary])
            qualification.verify_binaries([binary], 'http://127.0.0.1:8765')

    def test_empty_authenticode_pins_allowed_but_malformed_pins_rejected(self):
        config = feed.read(ROOT / 'packaging/update-trust.json')
        config['windows'][0].update(enabled=True, public_key=PUB, authenticode_spki_sha256=[])
        with patch.object(feed, 'read', return_value=config):
            self.assertEqual(feed.trust()['windows'][0]['authenticode_spki_sha256'], [])
        for pins in [['a' * 63], ['A' * 64], ['garbage'], 'not-an-array']:
            config['windows'][0]['authenticode_spki_sha256'] = pins
            with patch.object(feed, 'read', return_value=config):
                with self.assertRaisesRegex(ValueError, 'trust_unconfigured'): feed.trust()


class PublishedRelease(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.event_path = self.root / 'event.json'
        self.output_path = self.root / 'output'
        self.env = {'GITHUB_ACTIONS': 'true', 'GITHUB_EVENT_NAME': 'workflow_dispatch', 'GITHUB_REPOSITORY': 'oinkvm/oink',
                    'GITHUB_REF': 'refs/heads/main',
                    'OINK_RELEASE_TAG': 'v1.0.0', 'GITHUB_EVENT_PATH': str(self.event_path),
                    'GITHUB_OUTPUT': str(self.root / 'github-output')}
        environment = patch.dict(os.environ, self.env)
        environment.start(); self.addCleanup(environment.stop)
        self.live = dict(id=2, draft=False, published_at='2026-10-20T00:00:00Z', tag_name='v1.0.0', prerelease=False, assets=[])
        self.event = {'repository': {'default_branch': 'main'}, 'inputs': {'release_tag': 'v1.0.0'}}
        self.msi = b'isolated non-executable TEST MSI bytes'
        def artifact(name, raw): return {'name': name, 'size': len(raw), 'sha256': feed.digest(raw)}
        self.msi_artifact = artifact('Oink-test.msi', self.msi)
        self.record = {'schema': 1, 'product': 'Oink', 'build': 3, 'version': '1.0.0', 'built_commit': 'b'*40,
                       'source_tree_sha256': 'a'*64, 'artifacts': [self.msi_artifact],
                       'platforms': {'windows': {'build': 3, 'version': '1.0.0', 'built_commit': 'b'*40,
                                                'source_tree_sha256': 'a'*64, 'artifacts': [self.msi_artifact]}}}
        self.raw_assets = {'release.json': feed.encode(self.record), 'Oink-test.msi': self.msi}
        self.config = {'repository': 'oinkvm/oink', 'windows': [EPOCH]}

    def prepare(self, commit='b'*40, source='a'*64):
        feed.write(self.event_path, self.event)
        self.live['assets'] = [dict(name=name, size=len(raw), browser_download_url=f'https://github.com/oinkvm/oink/releases/download/v1.0.0/{name}') for name, raw in self.raw_assets.items()]
        with patch.object(published, 'gh_json', return_value=self.live), \
                patch.object(published.feed, 'download', side_effect=lambda url, *args: self.raw_assets[url.rsplit('/', 1)[1]]), \
                patch.object(published.subprocess, 'run') as run, \
                patch.object(published.subprocess, 'check_output', return_value=commit + '\n'), \
                patch.object(published, 'source_tree_sha256', return_value=source):
            published.prepare(self.config, self.output_path)
            self.assertEqual(run.call_args_list[0].args[0][:3], ['git', 'fetch', '--no-tags'])

    def test_published_exact_tag_source_and_msi_are_required_without_notes_asset(self):
        self.prepare()
        self.assertEqual((self.output_path / 'Oink-test.msi').read_bytes(), self.msi)
        self.assertFalse((self.output_path / 'update.json').exists())
        self.assertIn('source_tree_sha256=' + 'a'*64, (self.root / 'github-output').read_text())
        for commit, source in [('c'*40, 'a'*64), ('b'*40, 'c'*64)]:
            with self.assertRaises(ValueError): self.prepare(commit, source)
        self.raw_assets['Oink-test.msi'] = b'replaced asset'
        with self.assertRaisesRegex(ValueError, 'asset_digest'): self.prepare()

    def test_live_draft_prerelease_or_release_event_is_rejected_before_signing(self):
        for field in ['draft', 'prerelease']:
            self.live[field] = True
            with self.assertRaisesRegex(ValueError, 'release_not_public'): self.prepare()
            self.live[field] = False
        self.event.update(action='published', release=copy.deepcopy(self.live))
        with patch.dict(os.environ, GITHUB_EVENT_NAME='release'):
            with self.assertRaisesRegex(ValueError, 'published_release_required'): self.prepare()

    def test_old_release_with_bound_notes_remains_usable_without_reading_its_notes(self):
        self.record['artifacts'].append(dict(name='update.json', size=1, sha256='f'*64))
        self.raw_assets['release.json'] = feed.encode(self.record)
        self.raw_assets['update.json'] = b'old notes are ignored'
        self.prepare()
        self.assertFalse((self.output_path / 'update.json').exists())
