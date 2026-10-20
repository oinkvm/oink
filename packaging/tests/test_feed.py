import base64
import copy
import os
import subprocess
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support/update'))
import feed

# RFC 8032 deterministic TEST key; never a production trust root.
SEED = bytes.fromhex('9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60')
PUB = base64.b64encode(bytes.fromhex('d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a')).decode()
PEM = '-----BEGIN PRIVATE KEY-----\n' + base64.b64encode(bytes.fromhex('302e020100300506032b657004220420') + SEED).decode() + '\n-----END PRIVATE KEY-----\n'
EPOCH = {'epoch': 'win-2026-01', 'public_key': PUB, 'channels': ['stable', 'beta']}
NOW = 1800000000


class OpenSSLToolTests(unittest.TestCase):
    def test_missing_tool_fails_before_crypto(self):
        with patch.object(feed.shutil, 'which', return_value=None), patch.object(feed.subprocess, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'openssl_3_required'):
                feed.openssl(['pkeyutl', '-sign'])
            run.assert_not_called()

    def test_incompatible_or_failed_version_probe_never_receives_crypto_arguments(self):
        for status, version in [(0, b'LibreSSL 3.3.6'), (0, b'OpenSSL 1.1.1w'),
                                (0, b'OpenSSL 4.0.0'), (0, b''), (1, b'OpenSSL 3.0.0')]:
            with self.subTest(status=status, version=version), \
                    patch.object(feed.shutil, 'which', return_value='selected-openssl'), \
                    patch.object(feed.subprocess, 'run', return_value=subprocess.CompletedProcess([], status, version)) as run:
                with self.assertRaisesRegex(ValueError, 'openssl_3_required'):
                    feed.openssl(['pkeyutl', '-sign', '-inkey', 'test-key.pem'])
                run.assert_called_once_with(['selected-openssl', 'version'],
                                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)

    def test_version_check_and_crypto_use_the_same_resolved_tool(self):
        with patch.object(feed.shutil, 'which', return_value='selected-openssl'), \
                patch.object(feed.subprocess, 'run', side_effect=[
                    subprocess.CompletedProcess([], 0, b'OpenSSL 3.0.0 7 Sep 2021'),
                    subprocess.CompletedProcess([], 0, b'signature')]) as run:
            self.assertEqual(feed.openssl(['pkeyutl', '-sign', '-rawin']), b'signature')
            self.assertEqual([call.args[0] for call in run.call_args_list],
                             [['selected-openssl', 'version'], ['selected-openssl', 'pkeyutl', '-sign', '-rawin']])

    def test_crypto_failure_is_not_swallowed_or_disclosed(self):
        with patch.object(feed.shutil, 'which', return_value='selected-openssl'), \
                patch.object(feed.subprocess, 'run', side_effect=[
                    subprocess.CompletedProcess([], 0, b'OpenSSL 3.6.0'),
                    subprocess.CompletedProcess([], 1, b'', b'sensitive tool error')]) as run:
            with self.assertRaisesRegex(ValueError, '^crypto$'):
                feed.openssl(['pkeyutl', '-verify', '-rawin'])
            self.assertEqual(run.call_args.kwargs['stderr'], subprocess.DEVNULL)


class FeedTests(unittest.TestCase):
    def setUp(self):
        self.secret = patch.dict(os.environ, {feed.KEY_ENV[EPOCH['epoch']]: PEM})
        self.secret.start()
        self.addCleanup(self.secret.stop)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.state = Path(self.temp.name)

    def payload(self, sequence=1, channel='stable'):
        return {'schema': 1, 'product': 'com.hachiwii.oink', 'keyEpoch': EPOCH['epoch'],
                'channel': channel, 'sequence': sequence, 'generatedAt': feed.iso(NOW),
                'expiresAt': feed.iso(NOW + 30*86400), 'releases': []}

    def envelope(self, obj):
        payload = feed.encode(obj)
        return feed.encode({'envelopeSchema': 1, 'payload': base64.b64encode(payload).decode(),
                            'signatures': [{'keyId': EPOCH['epoch'], 'ed25519': feed.sign(payload, EPOCH['epoch'], PUB)}]})

    def setup_registry(self):
        feeds = {}
        for channel in EPOCH['channels']:
            raw = self.envelope(self.payload(channel=channel))
            feed.write(self.state / 'site' / feed.relative(EPOCH['epoch'], channel), raw)
            feeds[channel] = {'records': [], 'last_allocated': 1, 'manifest_sha256': feed.digest(raw), 'verified_at': feed.iso(NOW)}
        feed.write(self.state / 'registry.json', {'schema': 1, 'feeds': {EPOCH['epoch']: feeds}})
        feed.write(self.state / 'site/mac/appcast.xml', b'OFFLINE SIGNED BYTES\r\n')
        return {'windows': [EPOCH], 'repository': 'oinkvm/oink'}

    def test_exact_signature_expiry_and_identity(self):
        raw = self.envelope(self.payload())
        self.assertEqual(feed.manifest(raw, EPOCH, 'stable', now=NOW)['sequence'], 1)
        for channel, now in [('beta', NOW), ('stable', NOW+30*86400), ('stable', NOW-301)]:
            with self.assertRaises(ValueError): feed.manifest(raw, EPOCH, channel, now=now)
        envelope = feed.loads(raw)
        payload = bytearray(base64.b64decode(envelope['payload']))
        payload[5] ^= 1
        envelope['payload'] = base64.b64encode(payload).decode()
        with self.assertRaises(ValueError): feed.manifest(feed.encode(envelope), EPOCH, 'stable', now=NOW)

    def test_duplicates_and_wrong_public_key_rejected(self):
        with self.assertRaises(ValueError): feed.loads(b'{"x":1,"x":2}')
        with self.assertRaises(ValueError): feed.sign(b'data', EPOCH['epoch'], base64.b64encode(bytes(32)).decode())

    def test_bilingual_notes_signed_and_each_translation_tamper_rejected(self):
        notes = {'zh-Hans': '修复问题。\n安全更新 & <文字>', 'en': 'Fix a problem.\nSecurity update & <text>'}
        obj = self.payload()
        obj['releases'] = [{'notes': notes}]
        raw = self.envelope(obj)
        self.assertEqual(feed.manifest(raw, EPOCH, 'stable', now=NOW)['releases'][0]['notes'], notes)
        for language in notes:
            changed = copy.deepcopy(obj)
            changed['releases'][0]['notes'][language] += 'tampered'
            envelope = feed.loads(raw)
            envelope['payload'] = base64.b64encode(feed.encode(changed)).decode()
            with self.assertRaises(ValueError): feed.manifest(feed.encode(envelope), EPOCH, 'stable', now=NOW)

    def test_signed_notes_reject_missing_languages_controls_and_utf8_overflow(self):
        for notes in ['legacy', {}, {'en': 'English'}, {'zh-Hans': '中文'},
                      {'en': 'English', 'zh-Hans': None}, {'en': '', 'zh-Hans': '中文'},
                      {'en': 'English', 'zh-Hans': '中\u0085文'}, {'en': '\r', 'zh-Hans': '中文'},
                      {'en': 'English', 'zh-Hans': '中' * (feed.MAX_NOTES // 3 + 1)},
                      {'en': 'x' * (feed.MAX_NOTES + 1), 'zh-Hans': '中文'},
                      {'en': 'English', 'zh-Hans': '中文', 'fr': 'extra'}]:
            obj = self.payload()
            obj['releases'] = [{'notes': notes}]
            with self.subTest(notes=type(notes)):
                with self.assertRaises(ValueError): feed.manifest(self.envelope(obj), EPOCH, 'stable', now=NOW)
        feed.validate_notes({'en': 'x' * (feed.MAX_NOTES - 2) + '\n\t', 'zh-Hans': '中' * (feed.MAX_NOTES // 3)})
        duplicate = b'{"zh-Hans":"a","en":"first","en":"second"}'
        with self.assertRaises(ValueError): feed.validate_notes(feed.loads(duplicate))

    def test_renewal_boundary_monotonic_and_mac_preserved(self):
        config = self.setup_registry()
        feed.allocate(self.state, config, NOW+9*86400-1)
        self.assertEqual(feed.read(self.state/'pending-renewal.json')['plans'], [])
        feed.allocate(self.state, config, NOW+9*86400)
        plans = feed.read(self.state/'pending-renewal.json')['plans']
        self.assertEqual(len(plans), 2)
        for plan in plans:
            obj = feed.loads(base64.b64decode(plan['payload']))
            self.assertEqual(obj['sequence'], 2)
            self.assertEqual(feed.timestamp(obj['expiresAt'])-feed.timestamp(obj['generatedAt']), 30*86400)
            self.assertEqual(obj['releases'], [])
        # A failed signer consumes its allocation; retry uses a fresh sequence.
        feed.allocate(self.state, config, NOW+9*86400)
        self.assertEqual(feed.registry(self.state)['feeds'][EPOCH['epoch']]['stable']['last_allocated'], 3)
        self.assertEqual((self.state/'site/mac/appcast.xml').read_bytes(), b'OFFLINE SIGNED BYTES\r\n')

    def test_snapshot_tamper_does_not_renew(self):
        config = self.setup_registry()
        path = self.state/'site'/feed.relative(EPOCH['epoch'], 'stable')
        path.write_bytes(self.envelope(self.payload(2)))
        with self.assertRaises(ValueError): feed.allocate(self.state, config, NOW+10*86400)
        self.assertEqual(feed.registry(self.state)['feeds'][EPOCH['epoch']]['stable']['last_allocated'], 1)

    def test_missing_key_leaves_site_unchanged(self):
        config = self.setup_registry()
        before = (self.state/'site'/feed.relative(EPOCH['epoch'], 'stable')).read_bytes()
        feed.allocate(self.state, config, NOW+10*86400)
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(ValueError): feed.sign_plans(self.state, config)
        self.assertEqual((self.state/'site'/feed.relative(EPOCH['epoch'], 'stable')).read_bytes(), before)


if __name__ == '__main__': unittest.main()

class PublicationTests(FeedTests):
    def test_package_signer_rejects_unapproved_or_changed_artifacts(self):
        import release
        folder = self.state/'input'
        folder.mkdir()
        raw = b'ONLY A TEST MSI PAYLOAD; NOT EXECUTABLE'
        artifact = {'name': 'Oink-test.msi', 'size': len(raw), 'sha256': feed.digest(raw)}
        record = {'schema': 1, 'product': 'Oink', 'source_tree_sha256': 'a'*64, 'architecture': 'x64',
                  'signing': 'signpath-approved-authenticode-verified', 'updater_included': True,
                  'collector_included': True, 'build': 2, 'protocol_baseline': 1, 'artifacts': [artifact],
                  'version': '1.0.0', 'minimumWindowsBuild': 19045}
        feed.write(folder/'release.json', record)
        feed.write(folder/'Oink-test.msi', raw)
        feed.write(folder/'update.json', {'channel': 'stable', 'severity': 'normal', 'notes': {'zh-Hans': '测试更新', 'en': 'Test update'}})
        config = {'windows': [EPOCH], 'repository': 'oinkvm/oink'}
        context = {'GITHUB_ACTIONS': 'true', 'GITHUB_REPOSITORY': 'oinkvm/oink', 'GITHUB_RUN_ID': '12',
                   'OINK_SOURCE_TREE_SHA256': 'a'*64, 'OINK_RELEASE_TAG': 'v1.0.0', 'OINK_SIGNPATH_APPROVAL_ID': 'TEST'}
        with patch.dict(os.environ, context):
            release.sign_package(config, folder, self.state/'output.json')
            output = feed.read(self.state/'output.json')
            self.assertFalse(output['published'])
            self.assertEqual(output['release']['notes'], {'zh-Hans': '测试更新', 'en': 'Test update'})
            feed.verify(raw, output['release']['package']['ed25519'], PUB)
            annotations = feed.read(folder/'update.json')
            feed.write(folder/'update.json', {**annotations, 'notes': {'en': 'missing translation'}})
            with patch.object(feed, 'sign', side_effect=AssertionError('must validate notes before signing')):
                with self.assertRaises(ValueError): release.sign_package(config, folder, self.state/'output.json')
            feed.write(folder/'update.json', annotations)
            (folder/'Oink-test.msi').write_bytes(b'tampered')
            with self.assertRaises(ValueError): release.sign_package(config, folder, self.state/'output.json')
            (folder/'Oink-test.msi').write_bytes(raw)
            record['signing'] = 'unsigned'
            feed.write(folder/'release.json', record)
            with self.assertRaises(ValueError): release.sign_package(config, folder, self.state/'output.json')
