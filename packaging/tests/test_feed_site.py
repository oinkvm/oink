import base64
import copy
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support/update'))
import feed
import feed_site
import deploy
from test_feed import EPOCH, NOW, PEM, PUB

MAC_PATH = 'mac/v1/mac-2026-01/beta/appcast.xml'


class SiteTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.state = self.root / 'state'
        self.secret = patch.dict(os.environ, {feed.KEY_ENV[EPOCH['epoch']]: PEM, 'OINK_UPDATE_QUALIFIED_BUILD': '2'})
        self.secret.start()
        self.addCleanup(self.secret.stop)
        self.config = {'schema': 1, 'repository': 'oinkvm/oink', 'windows': [EPOCH],
                       'mac': {'epoch': 'mac-2026-01', 'enabled': True, 'public_key': PUB}}
        self.trust = patch.object(feed, 'trust', return_value=self.config)
        self.trust.start()
        self.addCleanup(self.trust.stop)
        original = feed.manifest
        self.clock = patch.object(feed, 'manifest', side_effect=lambda raw, epoch, channel, allow_expired=False, now=None:
                                  original(raw, epoch, channel, allow_expired, NOW))
        self.clock.start()
        self.addCleanup(self.clock.stop)
        feed.write(self.state / 'registry.json', {
            'schema': 1, 'revision': 0,
            'platforms': {p: {'enabled': False, 'revision': 0, 'files': {}} for p in feed_site.PLATFORMS},
            'feeds': {EPOCH['epoch']: {c: {'records': [], 'last_allocated': 0, 'manifest_sha256': None}
                                      for c in EPOCH['channels']}}})
        feed.write(self.state / 'site/CNAME', b'updates.oink.hachiwii.com\n')
        (self.state / 'records').mkdir()
        self.packages = {}
        self.download = patch.object(feed, 'download', side_effect=lambda url, *args: self.packages[url])
        self.download.start()
        self.addCleanup(self.download.stop)

    def mac(self, build):
        package = b'PUBLIC TEST ARCHIVE ' + str(build).encode()
        url = f'https://github.com/oinkvm/oink/releases/download/v1.0.{build}/Oink-arm64.zip'
        self.packages[url] = package
        xml = f'''<rss xmlns:sparkle="{feed_site.SPARKLE}" xmlns:oink="{feed_site.OINK}"><channel><item>
<sparkle:version>{build}</sparkle:version><sparkle:shortVersionString>1.0.{build}</sparkle:shortVersionString>
<sparkle:channel>beta</sparkle:channel><enclosure url="{url}" length="{len(package)}" sparkle:edSignature="{feed.sign(package,EPOCH['epoch'],PUB)}"/>
<oink:releaseNotes>{{"en":"Test update","zh-Hans":"测试更新"}}</oink:releaseNotes>
</item></channel></rss>'''.encode()
        raw = xml + f'<!-- sparkle-signatures:\nedSignature: {feed.sign(xml,EPOCH["epoch"],PUB)}\nlength: {len(xml)}\n-->\n'.encode()
        record = {'architecture': 'arm64', 'build': build, 'qualified_build': build, 'developer_id_verified': True,
                  'notarized': True, 'sha256': feed.digest(package), 'url': url, 'length': len(package), 'version': f'1.0.{build}'}
        feed.write(self.state / 'site' / MAC_PATH, raw)
        return record

    def windows(self, sequence, releases=None):
        reg = feed.registry(self.state)
        for channel in EPOCH['channels']:
            payload = feed.encode({'schema': 1, 'product': 'com.hachiwii.oink', 'keyEpoch': EPOCH['epoch'],
                                   'channel': channel, 'sequence': sequence, 'generatedAt': feed.iso(NOW),
                                   'expiresAt': feed.iso(NOW + 30*86400), 'releases': releases or []})
            raw = feed.encode({'envelopeSchema': 1, 'payload': base64.b64encode(payload).decode(),
                               'signatures': [{'keyId': EPOCH['epoch'], 'ed25519': feed.sign(payload,EPOCH['epoch'],PUB)}]})
            feed.write(self.state / 'site' / feed.relative(EPOCH['epoch'], channel), raw)
            reg['feeds'][EPOCH['epoch']][channel].update(last_allocated=sequence, manifest_sha256=feed.digest(raw))
        feed.write(self.state / 'registry.json', reg)

    def install(self, platform, build=1):
        reg = feed.registry(self.state)
        if platform == 'mac':
            catalog = {MAC_PATH: self.mac(build)}
        else:
            self.windows(build)
            reg = feed.registry(self.state)
            catalog = None
        reg['platforms'][platform] = {'enabled': True, 'revision': build,
            'files': {platform + '/' + name: feed.digest(raw) for name, raw in feed_site.files(self.state / 'site' / platform).items()}}
        if catalog:
            reg['platforms'][platform]['catalog'] = catalog
        feed.write(self.state / 'registry.json', reg)

    def make_candidate(self, platform, build=2):
        before = feed_site.files(self.state)
        reg = feed.registry(self.state)
        if platform == 'mac':
            catalog = {MAC_PATH: self.mac(build)}
            feed.write(self.root / 'catalog.json', catalog)
        else:
            self.windows(build)
        output = self.root / f'{platform}-candidate'
        feed_site.candidate(self.state, platform, output, self.root / 'catalog.json')
        # Reconstruct the authoritative snapshot. Windows reserves sequence and
        # catalog before signing but leaves the deployed manifest bytes intact.
        for path in self.state.rglob('*'):
            if path.is_file(): path.unlink()
        for path, raw in before.items(): feed.write(self.state / path, raw)
        if platform == 'windows':
            for slot in reg['feeds'][EPOCH['epoch']].values(): slot['last_allocated'] = build
            feed.write(self.state / 'registry.json', reg)
        return output

    def test_all_four_present_absent_combinations_and_both_publishers(self):
        for mac in [False, True]:
            for windows in [False, True]:
                for platform in feed_site.PLATFORMS:
                    with self.subTest(mac=mac, windows=windows, publish=platform):
                        # Every iteration has an independent protected snapshot.
                        case = SiteTests('runTest')
                        case.setUp()
                        try:
                            if mac: case.install('mac')
                            if windows: case.install('windows')
                            feed_site.verify_all(case.state)
                            other = 'windows' if platform == 'mac' else 'mac'
                            preserved = feed_site.files(case.state / 'site' / other)
                            proposed = case.make_candidate(platform)
                            output = case.root / 'next'
                            feed_site.merge(case.state, proposed, output, 0)
                            self.assertEqual(feed_site.files(output / 'site' / other), preserved)
                            self.assertEqual(feed.registry(output)['revision'], 1)
                            # Readback verifies bytes and absent-platform 404s without a real deployment.
                            public = {feed.BASE + path: raw for path, raw in feed_site.files(output / 'site').items()}
                            import urllib.error
                            def get(url, *args):
                                if url not in public: raise urllib.error.HTTPError(url, 404, 'missing', {}, None)
                                return public[url]
                            with patch.object(feed, 'download', side_effect=get):
                                feed_site.verify_public(output)
                        finally:
                            case.doCleanups()

    def test_both_absent_requires_neither_platform_trust(self):
        with patch.object(feed, 'trust', side_effect=AssertionError('disabled platform must not load trust')):
            feed_site.verify_all(self.state)
        self.assertFalse((self.state / 'site/mac').exists())
        self.assertFalse((self.state / 'site/windows').exists())

    def test_mac_only_needs_no_windows_key_epoch_secret_or_catalog(self):
        self.config['windows'] = []
        proposed = self.make_candidate('mac')
        with patch.dict(os.environ, {feed.KEY_ENV[EPOCH['epoch']]: ''}):
            feed_site.merge(self.state, proposed, self.root / 'next', 0)
        self.assertFalse((self.root / 'next/site/windows').exists())
        feed_site.verify_all(self.root / 'next')

    def test_stale_state_platform_and_build_rejected_without_changing_current_site(self):
        self.install('mac')
        proposed = self.make_candidate('mac')
        before = feed_site.files(self.state)
        with self.assertRaisesRegex(ValueError, 'state_cas'):
            feed_site.merge(self.state, proposed, self.root / 'bad-state', 5)
        proposal = feed.read(proposed / 'candidate.json')
        proposal['base_revision'] = 0
        feed.write(proposed / 'candidate.json', proposal)
        with self.assertRaisesRegex(ValueError, 'platform_cas'):
            feed_site.merge(self.state, proposed, self.root / 'bad-platform', 0)
        proposal['base_revision'] = 1
        proposal['slot']['catalog'][MAC_PATH]['build'] = 1
        feed.write(proposed / 'candidate.json', proposal)
        with self.assertRaisesRegex(ValueError, 'stale_candidate'):
            feed_site.merge(self.state, proposed, self.root / 'bad-build', 0)
        self.assertEqual(feed_site.files(self.state), before)

    def test_signature_package_hash_and_qualification_fail_closed(self):
        proposed = self.make_candidate('mac')
        before = feed_site.files(self.state)
        original = (proposed / 'site' / MAC_PATH).read_bytes()
        for mutate in ['xml', 'package', 'qualification']:
            with self.subTest(mutate=mutate):
                proposal = feed.read(proposed / 'candidate.json')
                raw = original
                if mutate == 'xml': raw = original.replace(b'Test update', b'Bad! update')
                if mutate == 'package': proposal['slot']['catalog'][MAC_PATH]['sha256'] = '0'*64
                if mutate == 'qualification': proposal['slot']['catalog'][MAC_PATH]['notarized'] = False
                proposal['slot']['files'][MAC_PATH] = feed.digest(raw)
                feed.write(proposed / 'candidate.json', proposal)
                feed.write(proposed / 'site' / MAC_PATH, raw)
                with self.assertRaises(ValueError):
                    feed_site.merge(self.state, proposed, self.root / mutate, 0)
                self.assertEqual(feed_site.files(self.state), before)

    def test_candidate_cannot_delete_or_write_other_platform(self):
        self.install('windows')
        proposed = self.make_candidate('mac')
        feed.write(proposed / 'site/windows/injected.json', b'changed')
        with self.assertRaisesRegex(ValueError, 'candidate_prefix'):
            feed_site.merge(self.state, proposed, self.root / 'bad-prefix', 0)
        (proposed / 'site/windows/injected.json').unlink()
        proposal = feed.read(proposed / 'candidate.json')
        proposal['slot']['files'] = {}
        feed.write(proposed / 'candidate.json', proposal)
        with self.assertRaisesRegex(ValueError, 'implicit_delete'):
            feed_site.merge(self.state, proposed, self.root / 'bad-delete', 0)

    def test_windows_stale_allocation_rejected_and_mac_bytes_preserved_on_failed_signer(self):
        self.install('mac')
        self.install('windows')
        proposed = self.make_candidate('windows')
        reg = feed.registry(self.state)
        reg['feeds'][EPOCH['epoch']]['beta']['last_allocated'] += 1
        feed.write(self.state / 'registry.json', reg)
        before = feed_site.files(self.state / 'site')
        with self.assertRaisesRegex(ValueError, 'reservation_changed'):
            feed_site.merge(self.state, proposed, self.root / 'bad-reservation', 0)
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(ValueError, 'secret_unavailable'):
                feed.sign(b'payload', EPOCH['epoch'], PUB)
        self.assertEqual(feed_site.files(self.state / 'site'), before)

    def test_windows_publisher_revalidates_catalog_and_public_package(self):
        self.install('mac')
        raw = b'PUBLIC TEST MSI BYTES'
        url = 'https://github.com/oinkvm/oink/releases/download/v1.0.2/Oink-x64.msi'
        self.packages[url] = raw
        release = {'build': 2, 'version': '1.0.2', 'tag': 'v1.0.2', 'sourceTreeSha256': 'a'*64,
                   'architecture': 'x64', 'minimumWindowsBuild': 19045, 'protocolBaseline': 1,
                   'minimumUpdaterSchema': 1, 'channel': 'stable', 'severity': 'normal',
                   'notes': {'en': 'Test update', 'zh-Hans': '测试更新'},
                   'package': {'url': url, 'length': len(raw), 'sha256': feed.digest(raw),
                               'keyId': EPOCH['epoch'], 'ed25519': feed.sign(raw, EPOCH['epoch'], PUB)}}
        record = {'schema': 1, 'approved': True, 'published': True, 'source_tree_sha256': 'a'*64,
                  'workflow_run_id': 1, 'signing_approval': 'TEST', 'release': release}
        encoded = feed.encode(record)
        record_hash = feed.digest(encoded)
        feed.write(self.state / 'records' / (record_hash + '.json'), encoded)
        feed.write(self.state / 'site/windows/releases/2/release.json', encoded)
        self.windows(2, [release])
        reg = feed.registry(self.state)
        for slot in reg['feeds'][EPOCH['epoch']].values(): slot['records'] = [record_hash]
        feed.write(self.state / 'registry.json', reg)
        candidate = self.root / 'signed-windows'
        feed_site.candidate(self.state, 'windows', candidate)
        # Retain the durable allocation/catalog, but restore the pre-publication absence.
        for path in (self.state / 'site/windows').rglob('*'):
            if path.is_file(): path.unlink()
        for slot in reg['feeds'][EPOCH['epoch']].values(): slot['manifest_sha256'] = None
        feed.write(self.state / 'registry.json', reg)
        feed_site.merge(self.state, candidate, self.root / 'valid', 0)
        self.assertEqual(feed_site.files(self.state / 'site/mac'), feed_site.files(self.root / 'valid/site/mac'))
        self.packages[url] = b'TAMPERED PACKAGE'
        with self.assertRaisesRegex(ValueError, 'asset_digest'):
            feed_site.merge(self.state, candidate, self.root / 'tampered', 0)

    def test_other_platform_revision_can_advance_during_signing(self):
        proposed = self.make_candidate('mac')
        self.install('windows')
        reg = feed.registry(self.state)
        reg['revision'] = 1
        feed.write(self.state / 'registry.json', reg)
        feed_site.merge(self.state, proposed, self.root / 'next', 1)
        self.assertEqual(feed_site.files(self.root / 'next/site/windows'), feed_site.files(self.state / 'site/windows'))


class TrustTests(unittest.TestCase):
    def test_disabled_is_explicit_and_enabled_missing_roots_is_an_error(self):
        config = feed.read(feed.ROOT / 'packaging/update-trust.json')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(feed, 'ROOT', root):
                feed.write(root / 'packaging/update-trust.json', config)
                self.assertEqual(feed.trust('windows')['windows'], [])
                self.assertFalse(feed.trust('mac')['mac']['enabled'])
                config['windows'][0]['enabled'] = True
                feed.write(root / 'packaging/update-trust.json', config)
                with self.assertRaisesRegex(ValueError, 'trust_unconfigured'): feed.trust('windows')
                # Mac trust validation has no dependency on Windows trust.
                config['mac'].update(enabled=True, public_key=PUB)
                feed.write(root / 'packaging/update-trust.json', config)
                self.assertTrue(feed.trust('mac')['mac']['enabled'])
                config['mac']['public_key'] = None
                feed.write(root / 'packaging/update-trust.json', config)
                with self.assertRaisesRegex(ValueError, 'trust_unconfigured'): feed.trust('mac')


class DeploymentTests(unittest.TestCase):
    def test_readback_failure_does_not_write_or_commit_state(self):
        with patch.object(feed, 'read', return_value={'head': 'a'*40, 'revision': 1}), \
             patch.object(feed_site, 'verify_public', side_effect=ValueError('deployment_digest')), \
             patch.object(deploy, 'git') as git, patch.object(deploy.shutil, 'copytree') as copy:
            with self.assertRaisesRegex(ValueError, 'deployment_digest'): deploy.commit()
            git.assert_not_called()
            copy.assert_not_called()

    def test_failed_cas_does_not_accept_candidate(self):
        with patch.object(feed, 'read', return_value={'head': 'a'*40, 'revision': 1}), \
             patch.object(feed_site, 'verify_public'), patch.object(deploy, 'git', return_value='b'*40), \
             patch.object(deploy.shutil, 'copytree') as copy:
            with self.assertRaisesRegex(ValueError, 'state_cas'): deploy.commit()
            copy.assert_not_called()

    def test_remote_cas_failure_and_lost_push_acknowledgement(self):
        for accepted in [False, True]:
            with self.subTest(accepted=accepted), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                state, next_state = root / 'state', root / 'next'
                feed.write(state / 'registry.json', {'schema': 1, 'revision': 1})
                feed.write(next_state / 'registry.json', {'schema': 1, 'revision': 2})
                feed.write(next_state / 'site/feed.xml', b'new signed feed')
                committed = False
                commands = []
                def git(*args):
                    nonlocal committed
                    commands.append(args)
                    if args[0] == 'rev-parse': return ('b' if committed else 'a') * 40
                    if args[0] == 'commit': committed = True
                    if args[0] == 'push': raise subprocess.CalledProcessError(1, 'git push')
                    if args[0] == 'ls-remote': return ('b' if accepted else 'c') * 40 + ' refs/heads/update-feed-state'
                    return ''
                read = feed.read
                def metadata(path):
                    if path == Path('transaction.json'): return {'head': 'a'*40, 'revision': 1}
                    return read(path)
                with patch.object(deploy, 'STATE', state), patch.object(deploy, 'NEXT', next_state), \
                     patch.object(feed, 'read', side_effect=metadata), patch.object(deploy, 'git', side_effect=git), \
                     patch.object(feed_site, 'verify_public'):
                    if accepted:
                        deploy.commit()
                    else:
                        with self.assertRaises(subprocess.CalledProcessError): deploy.commit()
                self.assertIn(('push', 'origin', 'HEAD:update-feed-state'), commands)
                self.assertIn(('ls-remote', 'origin', 'refs/heads/update-feed-state'), commands)

    def test_rollback_reads_frozen_parent_even_after_local_commit(self):
        old = b'EXACT PREVIOUS HOSTED BYTES\r\n'
        with patch.object(feed, 'read', return_value={'head': 'a'*40}), \
             patch.object(deploy, 'git', return_value='site/mac/feed.xml'), \
             patch.object(deploy.subprocess, 'check_output', side_effect=[b'{"platforms":{}}', old]) as show, \
             patch.object(feed_site, 'verify_absent') as absent, \
             patch.object(feed, 'download', return_value=old):
            deploy.verify_rollback()
            self.assertEqual(show.call_args.args[0][-1], 'a'*40 + ':site/mac/feed.xml')
            absent.assert_called_once_with({'platforms': {}})
        with patch.object(feed, 'read', return_value={'head': 'a'*40}), \
             patch.object(deploy, 'git', return_value='site/mac/feed.xml'), \
             patch.object(deploy.subprocess, 'check_output', side_effect=[b'{"platforms":{}}', old]), \
             patch.object(feed, 'download', return_value=b'failed rollback'):
            with self.assertRaisesRegex(ValueError, 'rollback_digest'): deploy.verify_rollback()

    def test_workflows_share_lock_and_rollback_on_all_post_deploy_failures(self):
        root = Path(__file__).resolve().parents[2] / '.github/workflows'
        workflow = (root / 'deploy-feed.yml').read_text()
        self.assertNotIn('WINDOWS_UPDATE_ED25519', workflow)
        self.assertIn('group: oink-update-feed-publish', workflow)
        self.assertIn('artifact_name: previous-feed', workflow)
        self.assertIn("steps.publish.outcome == 'failure'", workflow)
        self.assertIn("steps.commit.outcome != 'success'", workflow)
        for name in ('publish-windows-feed.yml', 'renew-windows-update-manifests.yml'):
            text = (root / name).read_text()
            self.assertIn('group: oink-update-feed-publish', text)
            self.assertIn('uses: ./.github/workflows/deploy-feed.yml', text)
        mac = (root / 'publish-macos-feed.yml').read_text()
        self.assertNotIn('windows', mac.lower())


if __name__ == '__main__': unittest.main()
