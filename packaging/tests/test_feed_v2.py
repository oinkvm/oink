import base64
import copy
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support/update'))
import feed
import feed_site
import feed_v2
import rules
from test_feed import EPOCH, NOW, PEM, PUB
import test_feed_site

NOTES = {'en': 'Test update', 'zh-Hans': '测试更新'}
NOTICE = {'en': 'Update now.', 'zh-Hans': '请尽快更新。'}
MAX = 2**31 - 1


class RuleTests(unittest.TestCase):
    def test_version_ranges_compile_with_build_encoding(self):
        source = [{'installed': '< 0.3.6', 'kind': 'manual'},
                  {'installed': '0.3.6 - 0.3.6', 'kind': 'critical', 'notice': NOTICE},
                  {'installed': '>= 0.3.7', 'kind': 'none'},
                  {'installed': '*', 'kind': 'normal', 'target': 306002}]
        self.assertEqual(rules.compile_rules(source), [
            {'installed': [0, 305999], 'kind': 'manual', 'target': 'latest'},
            {'installed': [306000, 306999], 'kind': 'critical', 'target': 'latest', 'notice': NOTICE},
            {'installed': [307000, MAX], 'kind': 'none'},
            {'installed': [0, MAX], 'kind': 'normal', 'target': 306002}])
        self.assertEqual(rules.compile_range('<= 0.3.5'), [0, 305999])
        self.assertEqual(rules.compile_range('> 0.3.5'), [306000, MAX])
        self.assertEqual(rules.compile_range('1.0.0 - 1.2.3'), [10000000, 10203999])

    def test_committed_rules_are_a_single_catch_all_normal_rule(self):
        self.assertEqual(rules.load(), [{'installed': [0, MAX], 'kind': 'normal', 'target': 'latest'}])

    def test_invalid_rule_files_are_rejected(self):
        catch_all = {'installed': '*', 'kind': 'normal'}
        for source, code in [
                ([], 'rule_count'), ([catch_all] * 65, 'rule_count'),
                ([{'installed': '< 0.3.6', 'kind': 'normal'}], 'rule_catch_all'),
                ([{'installed': '*', 'kind': 'normal', 'notice': NOTICE}], 'rule_notice'),
                ([{'installed': '*', 'kind': 'none', 'target': 'latest'}], 'rule_target'),
                ([{'installed': '*', 'kind': 'none', 'notice': NOTICE}], 'rule_notice'),
                ([{'installed': '*', 'kind': 'urgent'}], 'rule_shape'),
                ([{'installed': '*', 'kind': 'normal', 'extra': 1}], 'rule_shape'),
                ([{'installed': '*', 'kind': 'normal', 'target': True}], 'rule_target'),
                ([{'installed': '*', 'kind': 'normal', 'target': '306002'}], 'rule_target'),
                ([{'installed': '0.3.7 - 0.3.6', 'kind': 'manual'}, catch_all], 'rule_range'),
                ([{'installed': '< 0.100.0', 'kind': 'manual'}, catch_all], 'rule_version'),
                ([{'installed': '0.3', 'kind': 'manual'}, catch_all], 'rule_range'),
                ([{'installed': '*', 'kind': 'critical', 'notice': {'en': 'English only'}}], 'notes_languages')]:
            with self.subTest(source=source[:2]), self.assertRaisesRegex(ValueError, code):
                rules.compile_rules(source)
        with self.assertRaisesRegex(ValueError, 'duplicate_key'):
            feed.loads(b'[{"installed":"*","kind":"normal","kind":"none"}]')
        compiled = rules.compile_rules([{'installed': '*', 'kind': 'normal', 'target': 306002}])
        rules.validate(compiled, {306002})
        with self.assertRaisesRegex(ValueError, 'rule_target'):
            rules.validate(compiled, {306001})


def release(build, channel='stable', platform='windows', epoch=EPOCH['epoch']):
    raw = b'PUBLIC TEST PACKAGE ' + str(build).encode()
    suffix = 'windows-x64.msi' if platform == 'windows' else 'macos-arm64.dmg'
    return raw, {'build': build, 'version': f'0.3.{build % 1000}', 'tag': f'v0.3.{build % 1000}', 'channel': channel,
                 'sourceTreeSha256': 'a' * 64, 'minimumOs': '10.0.17763' if platform == 'windows' else '14.2',
                 'protocolBaseline': 1, 'minimumUpdaterSchema': 2 if platform == 'windows' else 3, 'notes': NOTES,
                 'package': {'url': f'https://github.com/oinkvm/oink/releases/download/v0.3.{build % 1000}/Oink-{suffix}',
                             'length': len(raw), 'sha256': feed.digest(raw), 'keyId': epoch,
                             'ed25519': feed.sign(raw, EPOCH['epoch'], PUB)}}


def envelope(obj, epoch=EPOCH['epoch']):
    raw = feed.encode(obj)
    return feed.encode({'envelopeSchema': 1, 'payload': base64.b64encode(raw).decode(),
                        'signatures': [{'keyId': epoch, 'ed25519': feed.sign(raw, EPOCH['epoch'], PUB)}]})


class ManifestTests(unittest.TestCase):
    def setUp(self):
        secret = patch.dict(os.environ, {feed.KEY_ENV[EPOCH['epoch']]: PEM})
        secret.start()
        self.addCleanup(secret.stop)
        self.rules = rules.compile_rules([{'installed': '< 0.3.6', 'kind': 'manual'},
                                          {'installed': '*', 'kind': 'normal'}])

    def payload(self, channel='beta', releases=None):
        releases = [release(306001)[1], release(306002, 'beta')[1]] if releases is None else releases
        return feed_v2.payload('windows', EPOCH['epoch'], 'x64', channel, 7, NOW, releases, self.rules)

    def check(self, raw, channel='beta', **kwargs):
        return feed_v2.verify_manifest(raw, 'windows', EPOCH['epoch'], PUB, 'x64', channel, 'oinkvm/oink', at=NOW, **kwargs)

    def test_rfc8032_round_trip_with_exact_payload_shape(self):
        # The Rust test-feed feature trusts this same public RFC 8032 key.
        self.assertEqual(PUB, '11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=')
        obj = self.check(envelope(self.payload()))
        self.assertEqual(list(obj), feed_v2.PAYLOAD_KEYS)
        self.assertEqual([list(r) for r in obj['releases']], [feed_v2.RELEASE_KEYS] * 2)
        self.assertEqual((obj['schema'], obj['platform'], obj['architecture'], obj['expiresAt']),
                         (2, 'windows', 'x64', feed.iso(NOW + 30 * 86400)))

    def test_tampered_unknown_or_inconsistent_manifests_are_rejected(self):
        raw = envelope(self.payload())
        body = feed.loads(raw)
        payload = bytearray(base64.b64decode(body['payload']))
        payload[10] ^= 1
        body['payload'] = base64.b64encode(payload).decode()
        with self.assertRaisesRegex(ValueError, 'crypto'): self.check(feed.encode(body))
        cases = [('channel', 'stable', 'release_channel'), ('extra', 1, 'manifest_identity'),
                 ('platform', 'mac', 'manifest_identity'), ('architecture', 'arm64', 'manifest_identity'),
                 ('releases', [release(306001)[1]] * 2, 'catalog'), ('releases', [release(306001)[1]] * 33, 'release_count'),
                 ('rules', [{'installed': [0, MAX], 'kind': 'normal', 'target': 1}], 'rule_target'),
                 ('rules', [{'installed': [0, 5], 'kind': 'normal', 'target': 'latest'}], 'rule_catch_all')]
        for key, value, code in cases:
            obj = self.payload()
            obj[key] = value
            check_channel = 'stable' if key == 'channel' else 'beta'
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, code):
                self.check(envelope(obj), check_channel)
        extra = self.payload()
        extra['releases'][0]['severity'] = 'normal'
        with self.assertRaisesRegex(ValueError, 'release_shape'): self.check(envelope(extra))
        self.check(envelope(self.payload()), allow_expired=True)
        with self.assertRaisesRegex(ValueError, 'manifest_expired'):
            feed_v2.verify_manifest(envelope(self.payload()), 'windows', EPOCH['epoch'], PUB, 'x64', 'beta',
                                    'oinkvm/oink', at=NOW + 30 * 86400)
        with self.assertRaisesRegex(ValueError, 'signatures'):
            feed_v2.verify_manifest(envelope(self.payload(), 'win-2027-01'), 'windows', EPOCH['epoch'], PUB, 'x64',
                                    'beta', 'oinkvm/oink', at=NOW)


class StateTests(unittest.TestCase):
    setUp = test_feed_site.SiteTests.setUp
    mac = test_feed_site.SiteTests.mac
    install = test_feed_site.SiteTests.install
    windows = test_feed_site.SiteTests.windows

    def set_now(self, at):
        clock = patch.object(feed_v2, 'now', return_value=at)
        clock.start()
        self.addCleanup(clock.stop)

    def record(self, build, channel):
        raw, rel = release(build, channel)
        self.packages[rel['package']['url']] = raw
        v1 = {'build': build, 'version': rel['version'], 'tag': rel['tag'], 'sourceTreeSha256': 'a' * 64,
              'architecture': 'x64', 'minimumWindowsBuild': 17763, 'protocolBaseline': 1, 'minimumUpdaterSchema': 2,
              'channel': channel, 'severity': 'normal', 'notes': NOTES, 'package': rel['package']}
        encoded = feed.encode({'schema': 1, 'approved': True, 'published': True, 'source_tree_sha256': 'a' * 64,
                               'workflow_run_id': 1, 'signing_policy': 'manifest-ed25519', 'release': v1})
        feed.write(self.state / 'records' / (feed.digest(encoded) + '.json'), encoded)
        feed.write(self.state / f'site/windows/releases/{build}/release.json', encoded)
        return feed.digest(encoded), v1, rel

    def windows_state(self):
        """Deployed v1 Windows feed: stable 306001, beta 306001 + 306002, no v2 yet."""
        self.set_now(NOW)
        stable, beta = self.record(306001, 'stable'), self.record(306002, 'beta')
        reg = feed.registry(self.state)
        for channel, records in (('stable', [stable]), ('beta', [stable, beta])):
            payload = feed.encode({'schema': 1, 'product': 'com.hachiwii.oink', 'keyEpoch': EPOCH['epoch'],
                                   'channel': channel, 'sequence': 1, 'generatedAt': feed.iso(NOW),
                                   'expiresAt': feed.iso(NOW + 30 * 86400), 'releases': [r[1] for r in records]})
            raw = feed.encode({'envelopeSchema': 1, 'payload': base64.b64encode(payload).decode(),
                               'signatures': [{'keyId': EPOCH['epoch'], 'ed25519': feed.sign(payload, EPOCH['epoch'], PUB)}]})
            feed.write(self.state / 'site' / feed.relative(EPOCH['epoch'], channel), raw)
            reg['feeds'][EPOCH['epoch']][channel] = {'records': [r[0] for r in records], 'last_allocated': 1,
                                                     'manifest_sha256': feed.digest(raw)}
        reg['platforms']['windows'] = {'enabled': True, 'revision': 1, 'files': {
            'windows/' + n: feed.digest(r) for n, r in feed_site.files(self.state / 'site/windows').items()}}
        feed.write(self.state / 'registry.json', reg)
        return stable[2], beta[2]

    def deploy(self, platform, state=None, revision=0, deployed=None):
        """Merge a candidate signed in `state` into `deployed` (default: the same state)."""
        state = state or self.state
        candidate = self.root / f'{platform}-candidate-{revision}'
        feed_site.candidate(state, platform, candidate)
        output = self.root / f'{platform}-next-{revision}'
        feed_site.merge(deployed or state, candidate, output, revision)
        return output

    def signing_checkout(self, deployed, name):
        """The publisher's own checkout of the deployed state branch."""
        signing = self.root / name
        shutil.copytree(deployed, signing)
        return signing

    def reserve(self, signing, deployed):
        """reserve.py: only the reservation (registry, records) reaches the branch before the merge."""
        shutil.copy(signing / 'registry.json', deployed / 'registry.json')
        if (signing / 'records').exists():
            shutil.copytree(signing / 'records', deployed / 'records', dirs_exist_ok=True)

    def manifests(self, state, platform='windows', arch='x64'):
        result = {}
        for channel in ('stable', 'beta'):
            raw = (state / 'site' / feed_v2.relative(platform, EPOCH['epoch'] if platform == 'windows' else 'mac-2026-01',
                                                      arch, channel)).read_bytes()
            result[channel] = feed.loads(base64.b64decode(feed.loads(raw)['payload']))
        return result

    def test_windows_v2_publication_renewal_and_merge(self):
        stable, beta = self.windows_state()
        compiled = rules.load()
        feed_v2.allocate(self.state, 'windows', self.config, compiled, NOW)
        feed_v2.sign_plans(self.state, 'windows', self.config)
        deployed = self.deploy('windows')
        signed = self.manifests(deployed)
        self.assertEqual(signed['stable']['releases'], [stable])
        self.assertEqual(signed['beta']['releases'], [stable, beta])
        self.assertEqual(signed['beta']['rules'], compiled)
        self.assertEqual(signed['stable']['sequence'], 1)
        # v1 bytes are untouched by the v2 writer.
        for channel in ('stable', 'beta'):
            path = feed.relative(EPOCH['epoch'], channel)
            self.assertEqual((deployed / 'site' / path).read_bytes(), (self.state / 'site' / path).read_bytes())

        # Renewal: nothing before 21 days remain, both after; rules alone re-sign.
        self.assertEqual(feed_v2.allocate(deployed, 'windows', self.config, compiled, NOW + 9 * 86400 - 1), [])
        self.assertEqual(len(feed_v2.allocate(deployed, 'windows', self.config, compiled, NOW + 9 * 86400)), 2)
        critical = rules.compile_rules([{'installed': '< 0.3.2', 'kind': 'critical', 'notice': NOTICE},
                                        {'installed': '*', 'kind': 'normal'}])
        # The signer works on its own checkout; only the reservation reaches
        # the branch before the merge, like reserve.py's push.
        signing = self.root / 'signing'
        shutil.copytree(deployed, signing)
        plans = feed_v2.allocate(signing, 'windows', self.config, critical, NOW + 60)
        self.assertEqual([feed.loads(base64.b64decode(p['payload']))['sequence'] for p in plans], [3, 3])
        shutil.copy(signing / 'registry.json', deployed / 'registry.json')
        self.set_now(NOW + 60)
        feed_v2.sign_plans(signing, 'windows', self.config)
        candidate = self.root / 'renewal-candidate'
        feed_site.candidate(signing, 'windows', candidate)
        renewed = self.root / 'renewed'
        feed_site.merge(deployed, candidate, renewed, 1)
        self.assertEqual(self.manifests(renewed)['beta']['rules'], critical)
        # Re-deploying an older signed candidate never moves a sequence back.
        stale = self.root / 'stale-candidate'
        shutil.copytree(candidate, stale)
        for path in (stale / 'site/windows/v2').rglob('manifest.json'):
            path.write_bytes((deployed / 'site' / path.relative_to(stale / 'site')).read_bytes())
        proposal = feed.read(stale / 'candidate.json')
        proposal['base_revision'] = proposal['slot']['revision'] = feed.registry(renewed)['platforms']['windows']['revision']
        proposal['slot']['files'] = {'windows/' + n: feed.digest(r) for n, r in feed_site.files(stale / 'site/windows').items()}
        feed.write(stale / 'candidate.json', proposal)
        with self.assertRaisesRegex(ValueError, 'stale_candidate'):
            feed_site.merge(renewed, stale, self.root / 'stale-next', 2)

        # A reservation made after the candidate supersedes it.
        candidate = self.root / 'superseded'
        feed_site.candidate(renewed, 'windows', candidate)
        reg = feed.registry(renewed)
        reg['v2'][feed_v2.relative('windows', EPOCH['epoch'], 'x64', 'beta')]['last_allocated'] += 1
        feed.write(renewed / 'registry.json', reg)
        with self.assertRaisesRegex(ValueError, 'reservation_changed'):
            feed_site.merge(renewed, candidate, self.root / 'superseded-next', 2)

    def test_windows_renewal_never_changes_catalog_and_rejects_tampered_snapshot(self):
        self.windows_state()
        compiled = rules.load()
        feed_v2.allocate(self.state, 'windows', self.config, compiled, NOW)
        feed_v2.sign_plans(self.state, 'windows', self.config)
        deployed = self.deploy('windows')
        h, _, _ = self.record(306003, 'beta')
        shutil.copy(self.state / 'records' / f'{h}.json', deployed / 'records' / f'{h}.json')
        reg = feed.registry(deployed)
        reg['feeds'][EPOCH['epoch']]['beta']['records'].append(h)
        feed.write(deployed / 'registry.json', reg)
        with self.assertRaisesRegex(ValueError, 'renewal_catalog_changed'):
            feed_v2.allocate(deployed, 'windows', self.config, compiled, NOW + 60)
        self.assertEqual(len(feed_v2.allocate(deployed, 'windows', self.config, compiled, NOW + 60, release_build=306003)), 1)
        path = deployed / 'site' / feed_v2.relative('windows', EPOCH['epoch'], 'x64', 'stable')
        path.write_bytes(path.read_bytes() + b' ')
        with self.assertRaisesRegex(ValueError, 'snapshot_digest'):
            feed_v2.allocate(deployed, 'windows', self.config, compiled, NOW + 60)

    def test_signed_catalog_must_match_registered_records(self):
        self.windows_state()
        compiled = rules.load()
        reg = feed.registry(self.state)
        feed_v2.allocate(self.state, 'windows', self.config, compiled, NOW)
        # Swap in a payload whose catalog differs from the records, signed at the same sequence.
        pending = feed.read(self.state / 'pending-v2-windows.json')
        obj = feed.loads(base64.b64decode(pending['plans'][0]['payload']))
        obj['releases'] = []
        raw = feed.encode(obj)
        pending['plans'][0].update(payload=base64.b64encode(raw).decode(), payload_sha256=feed.digest(raw))
        feed.write(self.state / 'pending-v2-windows.json', pending)
        feed_v2.sign_plans(self.state, 'windows', self.config)
        with self.assertRaisesRegex(ValueError, 'signed_catalog_changed'):
            feed_site.candidate(self.state, 'windows', self.root / 'bad')
        self.assertEqual(reg['revision'], feed.registry(self.state)['revision'])

    def test_windows_beta_registration_reaches_only_the_v2_beta_manifest(self):
        import release as hooks
        stable, _ = self.windows_state()
        raw, rel = release(306003, 'beta')
        v1 = {'build': 306003, 'version': '0.3.6-beta.1', 'tag': 'v0.3.6-beta.1', 'sourceTreeSha256': 'a' * 64,
              'architecture': 'x64', 'minimumWindowsBuild': 17763, 'protocolBaseline': 1, 'minimumUpdaterSchema': 2,
              'channel': 'beta', 'severity': 'normal', 'notes': NOTES,
              'package': {**rel['package'], 'url': 'https://github.com/oinkvm/oink/releases/download/v0.3.6-beta.1/Oink-0.3.6-beta.1-windows-x64.msi'}}
        self.packages[v1['package']['url']] = raw
        path = self.root / 'update-record.json'
        feed.write(path, {'schema': 1, 'approved': True, 'published': False, 'source_tree_sha256': 'a' * 64,
                          'workflow_run_id': 3, 'signing_policy': 'manifest-ed25519', 'release': v1})
        before = copy.deepcopy(feed.registry(self.state)['feeds'])
        with patch.dict(os.environ, {'OINK_PUBLISHED_BUILD': '306003'}):
            hooks.register(self.config, self.state, path, NOW)
        feeds = feed.registry(self.state)['feeds'][EPOCH['epoch']]
        self.assertEqual(feeds['stable'], before[EPOCH['epoch']]['stable'])
        self.assertEqual(feeds['beta']['records'], before[EPOCH['epoch']]['beta']['records'])
        self.assertEqual(len(feeds['beta']['v2_records']), 1)
        # No v1 manifest changes: only the two (new) v2 manifests are planned.
        self.assertEqual(feed.read(self.state / 'pending-renewal.json')['plans'], [])
        plans = {p['path']: feed.loads(base64.b64decode(p['payload'])) for p in
                 feed.read(self.state / 'pending-v2-windows.json')['plans']}
        beta_path = feed_v2.relative('windows', EPOCH['epoch'], 'x64', 'beta')
        stable_path = feed_v2.relative('windows', EPOCH['epoch'], 'x64', 'stable')
        self.assertEqual([r['build'] for r in plans[beta_path]['releases']], [306001, 306002, 306003])
        self.assertEqual(plans[stable_path]['releases'], [stable])
        feed_v2.sign_plans(self.state, 'windows', self.config)
        deployed = self.deploy('windows')
        self.assertEqual([r['build'] for r in self.manifests(deployed)['beta']['releases']], [306001, 306002, 306003])
        # A later reservation of the beta-only list supersedes this candidate.
        candidate = self.root / 'beta-superseded'
        feed_site.candidate(deployed, 'windows', candidate)
        reg = feed.registry(deployed)
        reg['feeds'][EPOCH['epoch']]['beta']['v2_records'].append('0' * 64)
        feed.write(deployed / 'registry.json', reg)
        with self.assertRaisesRegex(ValueError, 'reservation_changed'):
            feed_site.merge(deployed, candidate, self.root / 'beta-superseded-next', 1)

    def v036(self, channel, run, notes=NOTES):
        """sign_package output for v0.3.6 build 306003: from the prerelease (beta) or after promotion (stable)."""
        raw, rel = release(306003, channel)
        url = 'https://github.com/oinkvm/oink/releases/download/v0.3.6/Oink-0.3.6-windows-x64.msi'
        self.packages[url] = raw
        v1 = {'build': 306003, 'version': '0.3.6', 'tag': 'v0.3.6', 'sourceTreeSha256': 'a' * 64,
              'architecture': 'x64', 'minimumWindowsBuild': 17763, 'protocolBaseline': 1, 'minimumUpdaterSchema': 2,
              'channel': channel, 'severity': 'normal', 'notes': notes, 'package': {**rel['package'], 'url': url}}
        path = self.root / f'update-record-{channel}-{run}.json'
        feed.write(path, {'schema': 1, 'approved': True, 'published': False, 'source_tree_sha256': 'a' * 64,
                          'workflow_run_id': run, 'signing_policy': 'manifest-ed25519', 'release': v1})
        return path

    def test_windows_prerelease_promotion_moves_the_same_build_to_stable_once(self):
        import release as hooks
        stable, legacy_beta = self.windows_state()
        epoch = EPOCH['epoch']
        register = lambda state, path: hooks.register(self.config, state, path, NOW)
        with patch.dict(os.environ, {'OINK_PUBLISHED_BUILD': '306003'}):
            register(self.state, self.v036('beta', 3))
            feed.sign_plans(self.state, self.config)
            feed_v2.sign_plans(self.state, 'windows', self.config)
            first = self.deploy('windows')
            beta_hash = feed.registry(first)['feeds'][epoch]['beta']['v2_records'][0]
            self.assertEqual([(r['build'], r['channel']) for r in self.manifests(first)['beta']['releases']],
                             [(306001, 'stable'), (306002, 'beta'), (306003, 'beta')])
            self.assertEqual(self.manifests(first)['stable']['releases'], [stable])
            v1_before = {c: (first / 'site' / feed.relative(epoch, c)).read_bytes() for c in ('stable', 'beta')}

            # Promotion may change nothing but the channel.
            changed = self.root / 'changed-notes'
            shutil.copytree(first, changed)
            with self.assertRaisesRegex(ValueError, 'release_build_changed'):
                register(changed, self.v036('stable', 4, {**NOTES, 'en': 'Edited after the prerelease'}))

            signing = self.signing_checkout(first, 'promotion-signing')
            register(signing, self.v036('stable', 4))
            feeds = feed.registry(signing)['feeds'][epoch]
            promoted_hash = feeds['stable']['records'][-1]
            self.assertNotEqual(promoted_hash, beta_hash)
            self.assertEqual(feeds['beta']['records'][-1], promoted_hash)
            self.assertEqual(feeds['beta']['v2_records'], [])
            # Both v1 manifests now carry the build for <= 0.3.5 clients.
            self.assertEqual(len(feed.read(signing / 'pending-renewal.json')['plans']), 2)
            self.reserve(signing, first)
            feed.sign_plans(signing, self.config)
            feed_v2.sign_plans(signing, 'windows', self.config)
            promoted = self.deploy('windows', signing, 1, first)
        signed = self.manifests(promoted)
        self.assertEqual([(r['build'], r['channel']) for r in signed['stable']['releases']],
                         [(306001, 'stable'), (306003, 'stable')])
        self.assertEqual([(r['build'], r['channel']) for r in signed['beta']['releases']],
                         [(306001, 'stable'), (306002, 'beta'), (306003, 'stable')])
        beta_release = next(r for r in self.manifests(first)['beta']['releases'] if r['build'] == 306003)
        self.assertEqual(signed['beta']['releases'][-1], {**beta_release, 'channel': 'stable'})
        self.assertEqual(signed['stable']['releases'][-1], signed['beta']['releases'][-1])
        for channel in ('stable', 'beta'):
            v1 = feed.manifest((promoted / 'site' / feed.relative(epoch, channel)).read_bytes(), EPOCH, channel)
            self.assertEqual([r['build'] for r in v1['releases']][-1], 306003)
            self.assertNotEqual(v1_before[channel], (promoted / 'site' / feed.relative(epoch, channel)).read_bytes())
        self.assertEqual(feed.read(promoted / 'site/windows/releases/306003/release.json')['release']['channel'], 'stable')

        # Retrying the promotion reuses the stable record; stable -> beta is rejected.
        before = copy.deepcopy(feed.registry(promoted)['feeds'])
        with patch.dict(os.environ, {'OINK_PUBLISHED_BUILD': '306003'}):
            register(promoted, self.v036('stable', 5))
            self.assertEqual({c: {k: s[k] for k in ('records', 'v2_records') if k in s} for c, s in feed.registry(promoted)['feeds'][epoch].items()},
                             {c: {k: s[k] for k in ('records', 'v2_records') if k in s} for c, s in before[epoch].items()})
            with self.assertRaisesRegex(ValueError, 'release_build_changed'):
                register(promoted, self.v036('beta', 6))

    def test_v2_promotion_accepts_only_the_channel_flip_of_the_published_build(self):
        _, beta = release(306002, 'beta')
        stable = {**beta, 'channel': 'stable'}
        old = [release(306001)[1], beta]
        self.assertTrue(feed_v2.promotion(old, [old[0], stable], 306002))
        for new, build in [([old[0], stable], 306001), ([old[0], {**stable, 'notes': NOTICE}], 306002),
                           ([stable], 306002), ([old[0], beta], 306002)]:
            with self.subTest(new=new, build=build):
                self.assertFalse(feed_v2.promotion(old, new, build))
        self.assertFalse(feed_v2.promotion([old[0], stable], [old[0], beta], 306002))

    def test_mac_prerelease_promotion_rewrites_its_record_and_reaches_stable_once(self):
        self.set_now(NOW)
        self.install('mac', 2)
        rel = self.mac_record(306001)
        relative = 'site/mac/releases/306001/arm64.json'
        record = feed.read(self.state / relative)
        feed.write(self.state / relative, {**record, 'release': {**rel, 'channel': 'beta'}})
        compiled = rules.load()
        signer = lambda: patch.object(feed_v2, 'sign_mac', side_effect=lambda raw, key, cache: feed.sign(raw, EPOCH['epoch'], key))
        qualified = lambda: patch.dict(os.environ, {'OINK_UPDATE_QUALIFIED_BUILD': '306001'})
        with patch.dict(os.environ, {feed_v2.MAC_SECRET: 'test'}), signer():
            feed_v2.allocate(self.state, 'mac', self.config, compiled, NOW, release_build=306001)
            feed_v2.sign_plans(self.state, 'mac', self.config, Path('unused-cache'))
            with qualified():
                first = self.deploy('mac')
            arm = self.manifests(first, 'mac', 'arm64')
            self.assertEqual(arm['stable']['releases'], [])
            self.assertEqual(arm['beta']['releases'], [{**rel, 'channel': 'beta'}])

            # Only the published build's record may change, and only its channel.
            edited = self.root / 'edited'
            shutil.copytree(first, edited)
            feed.write(edited / relative, {**record, 'release': {**rel, 'notes': NOTICE}})
            with self.assertRaisesRegex(ValueError, 'renewal_catalog_changed'):
                feed_v2.allocate(edited, 'mac', self.config, compiled, NOW + 60, release_build=306001)
            signing = self.signing_checkout(first, 'mac-promotion-signing')
            feed.write(signing / relative, record)
            with self.assertRaisesRegex(ValueError, 'record_digest'):
                feed_v2.allocate(signing, 'mac', self.config, compiled, NOW + 60)
            plans = feed_v2.allocate(signing, 'mac', self.config, compiled, NOW + 60, release_build=306001)
            self.assertEqual(sorted(p['path'] for p in plans), ['mac/v2/mac-2026-01/arm64/beta/manifest.json',
                                                               'mac/v2/mac-2026-01/arm64/stable/manifest.json'])
            self.reserve(signing, first)
            self.set_now(NOW + 60)
            feed_v2.sign_plans(signing, 'mac', self.config, Path('unused-cache'))
            with qualified():
                promoted = self.deploy('mac', signing, 1, first)
        arm = self.manifests(promoted, 'mac', 'arm64')
        self.assertEqual(arm['stable']['releases'], [rel])
        self.assertEqual(arm['beta']['releases'], [rel])

    def test_monitor_checks_v1_and_every_v2_manifest_on_every_source(self):
        self.windows_state()
        feed_v2.allocate(self.state, 'windows', self.config, rules.load(), NOW)
        feed_v2.sign_plans(self.state, 'windows', self.config)
        deployed = self.deploy('windows')
        site = feed_site.files(deployed / 'site')
        site['heartbeat.json'] = feed.encode({'checked_at': feed.iso(NOW)})
        config = {**self.config, 'sources': ['https://updates.oink.hachiwii.com', 'https://mirror.example']}
        mac = {**config, 'mac': {**config['mac'], 'enabled': False}}
        urls = []
        def download(url, *args):
            urls.append(url)
            return site[re.sub(r'^https://[^/]+/', '', url)]
        seen = self.root / 'seen.json'
        with patch.object(feed, 'download', side_effect=download):
            feed.monitor(config, seen, mac, NOW)
            self.assertIn('https://mirror.example/windows/v2/win-2026-01/x64/beta/manifest.json', urls)
            self.assertIn(feed.BASE + 'windows/v1/win-2026-01/stable/manifest.json', urls)
            self.assertEqual(len(feed.read(seen)), 2 + 2 * 2)
            site['heartbeat.json'] = feed.encode({'checked_at': feed.iso(NOW + 16 * 86400)})
            with self.assertRaisesRegex(ValueError, 'expiry_alert_1'):
                feed.monitor(config, seen, mac, NOW + 16 * 86400 + 1)
            site['heartbeat.json'] = feed.encode({'checked_at': feed.iso(NOW)})
            # A replayed lower sequence at any source is an alert.
            state = feed.read(seen)
            state['https://mirror.example/windows/v2/win-2026-01/x64/beta/manifest.json']['sequence'] = 9
            feed.write(seen, state)
            with self.assertRaisesRegex(ValueError, 'monitor_replay'):
                feed.monitor(config, seen, mac, NOW)

    def mac_record(self, build, arch='arm64'):
        raw, rel = release(build, platform='mac', epoch='mac-2026-01')
        record = {'schema': 1, 'architecture': arch, 'developer_id_verified': True, 'notarized': True,
                  'qualified_build': build, 'release': rel}
        feed.write(self.state / f'site/mac/releases/{build}/{arch}.json', record)
        return rel

    def test_mac_v2_publication_signs_all_architectures_and_requires_qualification(self):
        self.set_now(NOW)
        self.install('mac', 2)
        rel = self.mac_record(306001)
        compiled = rules.load()
        mac_sign = patch.object(feed_v2, 'sign_mac', side_effect=lambda raw, key, cache: feed.sign(raw, EPOCH['epoch'], key))
        with patch.dict(os.environ, {feed_v2.MAC_SECRET: 'test'}), mac_sign:
            with self.assertRaisesRegex(ValueError, 'record_digest'):
                feed_v2.allocate(self.state, 'mac', self.config, compiled, NOW)
            self.assertEqual(len(feed_v2.allocate(self.state, 'mac', self.config, compiled, NOW, release_build=306001)), 4)
            feed_v2.sign_plans(self.state, 'mac', self.config, Path('unused-cache'))
        with patch.dict(os.environ, {'OINK_UPDATE_QUALIFIED_BUILD': '2'}):
            with self.assertRaisesRegex(ValueError, 'update_not_qualified'):
                feed_site.candidate(self.state, 'mac', self.root / 'unqualified')
        with patch.dict(os.environ, {'OINK_UPDATE_QUALIFIED_BUILD': '306001'}):
            deployed = self.deploy('mac')
        arm, intel = self.manifests(deployed, 'mac', 'arm64'), self.manifests(deployed, 'mac', 'x64')
        self.assertEqual(arm['stable']['releases'], [rel])
        self.assertEqual(arm['beta']['releases'], [rel])
        self.assertEqual(intel['stable']['releases'], [])
        self.assertEqual(arm['stable']['keyEpoch'], 'mac-2026-01')
        # Renewal of the deployed snapshot needs no catalog or qualification.
        with patch.dict(os.environ, {feed_v2.MAC_SECRET: 'test'}), patch.object(
                feed_v2, 'sign_mac', side_effect=lambda raw, key, cache: feed.sign(raw, EPOCH['epoch'], key)):
            self.assertEqual(len(feed_v2.allocate(deployed, 'mac', self.config, compiled, NOW + 9 * 86400)), 4)
        with patch.dict(os.environ, {}, clear=True), self.assertRaisesRegex(ValueError, 'sparkle_secret_unavailable'):
            feed_v2.sign_plans(deployed, 'mac', self.config, Path('unused-cache'))

    def test_mac_beta_record_enters_only_beta_manifests(self):
        self.set_now(NOW)
        self.install('mac', 2)
        stable = self.mac_record(306001)
        beta = {**self.mac_record(306002), 'channel': 'beta'}
        feed.write(self.state / 'site/mac/releases/306002/arm64.json',
                   {**feed.read(self.state / 'site/mac/releases/306002/arm64.json'), 'release': beta})
        reg = feed.registry(self.state)
        reg['platforms']['mac']['files']['mac/releases/306001/arm64.json'] = feed.digest(
            (self.state / 'site/mac/releases/306001/arm64.json').read_bytes())
        feed.write(self.state / 'registry.json', reg)
        plans = {p['path']: feed.loads(base64.b64decode(p['payload'])) for p in
                 feed_v2.allocate(self.state, 'mac', self.config, rules.load(), NOW, release_build=306002)}
        self.assertEqual(plans['mac/v2/mac-2026-01/arm64/stable/manifest.json']['releases'], [stable])
        self.assertEqual(plans['mac/v2/mac-2026-01/arm64/beta/manifest.json']['releases'], [stable, beta])

    @unittest.skipUnless(os.environ.get('SPARKLE_TEST_SIGN_UPDATE'), 'pinned official native signer optional')
    def test_official_sparkle_signer_produces_the_pure_ed25519_manifest_signature(self):
        import mac_release
        seed = base64.b64encode(bytes.fromhex('9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60')).decode()
        tool = Path(os.environ['SPARKLE_TEST_SIGN_UPDATE'])
        with patch.dict(os.environ, {feed_v2.MAC_SECRET: seed}), \
                patch.object(mac_release, 'prepare', return_value=tool.parent.parent):
            raw = b'{"schema":2}'
            self.assertEqual(feed_v2.sign_mac(raw, PUB, Path('unused')), feed.sign(raw, EPOCH['epoch'], PUB))


class SourceTests(unittest.TestCase):
    def test_sources_are_ordered_https_origins_with_deploy_targets(self):
        import deploy
        config = feed.read(ROOT / 'packaging/update-trust.json')
        self.assertEqual(feed.sources(config), ['https://updates.oink.hachiwii.com'])
        self.assertTrue(all(origin in deploy.DEPLOY_TARGETS for origin in config['sources']))
        for sources in [[], ['http://updates.oink.hachiwii.com'], ['https://updates.oink.hachiwii.com/'],
                        ['https://a.example', 'https://a.example'], ['https://user@a.example'], 'https://a.example']:
            with self.subTest(sources=sources), self.assertRaisesRegex(ValueError, 'trust_sources'):
                feed.sources({'sources': sources})

    def test_every_source_receives_the_same_v2_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            feed.write(state / 'registry.json', {'schema': 1, 'revision': 0, 'platforms': {
                p: {'enabled': p == 'windows', 'revision': 0, 'files': {}} for p in feed_site.PLATFORMS}})
            v2 = 'windows/v2/win-2026-01/x64/stable/manifest.json'
            feed.write(state / 'site' / v2, b'SIGNED V2')
            feed.write(state / 'site/heartbeat.json', b'{}')
            config = {'sources': ['https://updates.oink.hachiwii.com', 'https://mirror.example'], 'windows': []}
            seen = []
            def download(url, *args):
                seen.append(url)
                return feed_site.files(state / 'site')[re.sub(r'^https://[^/]+/', '', url)]
            import urllib.error
            def absent(url, *args):
                if '/v1/' in url: raise urllib.error.HTTPError(url, 404, 'Not Found', {}, None)
                return download(url)
            # Only the per-source readback is under test here.
            with patch.object(feed, 'trust', return_value=config), patch.object(feed, 'download', side_effect=absent), \
                    patch.object(feed_site, 'verify_all'), patch.object(feed_site, 'verify_absent'):
                feed_site.verify_public(state)
            self.assertIn('https://mirror.example/' + v2, seen)
            self.assertIn(feed.BASE + v2, seen)
            self.assertNotIn('https://mirror.example/heartbeat.json', seen)


class WorkflowTests(unittest.TestCase):
    def test_renewal_covers_mac_v2_serially_with_only_the_mac_key_on_macos(self):
        text = (ROOT / '.github/workflows/renew-windows-update-manifests.yml').read_text()
        jobs = dict(re.findall(r'^  ([a-z-]+):\n((?:    .*\n|\n)+)', text, re.M))
        self.assertIn('runs-on: macos-15', jobs['sign-mac'])
        self.assertIn('needs: [gate, deploy]', jobs['sign-mac'])
        self.assertIn('group: oink-update-feed-publish', jobs['sign-mac'])
        self.assertIn('feed_v2.py sign --platform mac', jobs['sign-mac'])
        self.assertNotIn('WINDOWS_UPDATE', jobs['sign-mac'])
        self.assertNotIn('SPARKLE', jobs['sign'])
        self.assertIn('feed_v2.py sign --platform windows', jobs['sign'])
        self.assertIn('platform: mac', jobs['deploy-mac'])

    def test_publishers_reserve_and_sign_v2(self):
        windows = (ROOT / '.github/workflows/publish-windows-feed.yml').read_text()
        self.assertIn('feed_v2.py sign --platform windows', windows)
        mac = (ROOT / '.github/workflows/publish-macos-feed.yml').read_text()
        sign = re.search(r'^  sign:\n((?:    .*\n)+)', mac, re.M)[1]
        self.assertIn('group: oink-update-feed-publish', sign)
        self.assertIn('feed_v2.py allocate --platform mac --release-build', sign)
        self.assertIn('reserve.py', sign)


if __name__ == '__main__': unittest.main()
