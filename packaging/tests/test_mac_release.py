import base64
import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support/update'))
import mac_release as M

PUBLIC = base64.b64encode(bytes.fromhex('d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a')).decode()
SEED = base64.b64encode(bytes.fromhex('9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60')).decode()
COMMIT = 'a' * 40
TREE = 'b' * 64
NOTES = {'en':'Test update', 'zh-Hans':'测试更新', 'ja':'テスト', 'ko':'테스트', 'es-MX':'Prueba'}


class MacReleaseTests(unittest.TestCase):
    def record(self):
        asset = dict(name='Oink-0.1.1-macos-arm64.zip', size=20, sha256='c'*64)
        common = dict(version='0.1.1', build=30, built_commit=COMMIT, source_tree_sha256=TREE)
        mac = dict(**common, source_commit=None, team_id='893V37X2CW', notarized=True,
                   notarization={k:dict(status='Accepted',stapled=True) for k in ['app','dmg']},
                   launch_smoke=dict(status='passed',signed=True,seconds=10),
                   sparkle=dict(public_key_configured=True), artifacts=[asset])
        return dict(schema=1, product='Oink', **common, platforms=dict(macos=mac), artifacts=[asset])

    def test_only_bound_notarized_signed_smoked_stable_releases_can_be_signed(self):
        record = self.record()
        notes = dict(channel='stable', severity='normal', notes=NOTES)
        self.assertEqual(M.validate_release(record, notes, 'v0.1.1', COMMIT, TREE)['name'], record['artifacts'][0]['name'])
        for path, value in [(('built_commit',),'d'*40), (('source_tree_sha256',),'d'*64),
                            (('platforms','macos','notarized'),False),
                            (('platforms','macos','source_commit'),COMMIT),
                            (('platforms','macos','launch_smoke','signed'),False),
                            (('platforms','macos','notarization','app','stapled'),False),
                            (('platforms','macos','sparkle','public_key_configured'),False)]:
            bad = copy.deepcopy(record); slot = bad
            for k in path[:-1]: slot = slot[k]
            slot[path[-1]] = value
            with self.subTest(path=path), self.assertRaises(ValueError): M.validate_release(bad, notes, 'v0.1.1', COMMIT, TREE)
        for change in [dict(channel='alpha'), dict(notes={'en':'missing Chinese'})]:
            with self.assertRaises(ValueError): M.validate_release(record, {**notes,**change}, 'v0.1.1', COMMIT, TREE)

    def test_v2_record_binds_the_signed_notarized_dmg(self):
        record = self.record()
        dmg = dict(name='Oink-0.1.1-macos-arm64.dmg', size=40, sha256='e'*64)
        record['artifacts'].append(dmg)
        record['platforms']['macos'].update(artifacts=[*record['platforms']['macos']['artifacts'], dmg],
                                            minimum_os='14.2', protocol_baseline=1)
        self.assertEqual(M.artifact(record, 'macos', 'dmg'), dmg)
        url = 'https://github.com/oinkvm/oink/releases/download/v0.1.1/' + dmg['name']
        signature = base64.b64encode(b's'*64).decode()
        v2 = M.v2_record(record, 'macos', 'arm64', 'v0.1.1', NOTES, 'stable', dmg, url, signature, 'mac-2026-01')
        path = 'mac/releases/30/arm64.json'
        self.assertEqual(M.feed_v2.mac_record(M.feed.encode(v2), path, 'oinkvm/oink'), v2)
        self.assertEqual(v2['release']['package'], dict(url=url, length=40, sha256='e'*64, keyId='mac-2026-01', ed25519=signature))
        self.assertEqual(v2['release']['minimumUpdaterSchema'], 3)
        with self.assertRaisesRegex(ValueError, 'mac_qualification'):
            M.feed_v2.mac_record(M.feed.encode(v2), 'mac/releases/30/x64.json', 'oinkvm/oink')
        record['platforms']['macos']['artifacts'].append(dict(dmg, name='Second.dmg'))
        with self.assertRaisesRegex(ValueError, 'dmg_required'): M.artifact(record, 'macos', 'dmg')

    def test_legacy_asset_names_remain_valid(self):
        record = self.record()
        record['artifacts'][0]['name'] = 'Oink-0.1.1-30-macos-arm64.zip'
        self.assertEqual(M.validate_release(record, dict(channel='stable', severity='normal', notes=NOTES),
                                           'v0.1.1', COMMIT, TREE)['name'],
                         'Oink-0.1.1-30-macos-arm64.zip')

    def test_manual_dispatch_must_still_be_published_when_queried(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'event.json'
            release = dict(id=10, tag_name='v0.1.1',draft=False,prerelease=False,published_at='2026-10-20T20:04:19Z')
            event = dict(repository=dict(default_branch='main'), inputs=dict(release_tag='v0.1.1'))
            path.write_text(json.dumps(event))
            env = dict(GITHUB_ACTIONS='true',GITHUB_EVENT_PATH=str(path),GITHUB_EVENT_NAME='workflow_dispatch',
                       GITHUB_REF='refs/heads/main',GITHUB_REPOSITORY='oinkvm/oink')
            with patch.dict(os.environ,env), patch.object(M,'command',return_value=json.dumps(release).encode()):
                M.published('v0.1.1')
            for change in [dict(draft=True), dict(prerelease=None), dict(published_at=None)]:
                with patch.dict(os.environ,env), patch.object(M,'command',return_value=json.dumps({**release,**change}).encode()):
                    with self.assertRaisesRegex(ValueError,'release_not_public'): M.published('v0.1.1')
            # A vX.Y.Z prerelease is published to beta (promoted later).
            with patch.dict(os.environ,env), patch.object(M,'command',return_value=json.dumps({**release,'prerelease':True}).encode()):
                self.assertEqual(M.published_release.channel(M.published('v0.1.1')), 'beta')

    def test_candidate_reads_default_branch_notes_and_ignores_legacy_bound_asset(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            annotations = dict(channel='stable', severity='critical', notes=NOTES)
            M.feed.write(root / '.github/update-notes.json', dict(severity='critical', notes=NOTES))
            record = self.record()
            # Legacy releases may list a hash-bound notes asset. Never request it.
            record['artifacts'].append(dict(name='update.json', size=99, sha256='d'*64))
            config = dict(repository='oinkvm/oink', mac=dict(enabled=True, public_key=PUBLIC, epoch='mac-2026-01'))
            with patch.object(M, 'published', return_value=dict(prerelease=False)), patch.object(M.feed, 'ROOT', root), \
                 patch.object(M.feed, 'trust', return_value=config), \
                 patch.object(M, 'tag_source', return_value=(COMMIT, TREE, config['mac'])), \
                 patch.object(M.feed, 'download', return_value=M.feed.encode(record)) as download, \
                 patch.object(M, 'validate_release', wraps=M.validate_release) as validate, \
                 patch.object(M.feed_site, 'state_info', side_effect=RuntimeError('stop before native signing')):
                with self.assertRaisesRegex(RuntimeError, 'stop before native signing'):
                    M.candidate(root/'state', root/'output', root/'cache', 'v0.1.1')
                self.assertEqual(validate.call_args.args[1], annotations)
                download.assert_called_once_with(
                    'https://github.com/oinkvm/oink/releases/download/v0.1.1/release.json', M.feed.MAX_FEED, redirects=True)

                # The channel is the live flag; a committed channel is rejected.
                M.feed.write(root / '.github/update-notes.json', annotations)
                with self.assertRaisesRegex(ValueError, 'release_annotations_required'):
                    M.candidate(root/'state', root/'output', root/'cache', 'v0.1.1')

    def test_sparkle_free_builds_skip_only_the_sparkle_checks_and_beta_needs_no_zip(self):
        notes = dict(channel='stable', severity='normal', notes=NOTES)
        record = self.record()
        record['build'] = record['platforms']['macos']['build'] = M.SPARKLE_FREE_BUILD + 1
        del record['platforms']['macos']['sparkle']
        self.assertEqual(M.validate_release(record, notes, 'v0.1.1', COMMIT, TREE)['name'], 'Oink-0.1.1-macos-arm64.zip')
        record['build'] = record['platforms']['macos']['build'] = M.SPARKLE_FREE_BUILD - 1
        with self.assertRaises(KeyError): M.validate_release(record, notes, 'v0.1.1', COMMIT, TREE)
        record['platforms']['macos']['sparkle'] = dict(public_key_configured=False)
        with self.assertRaisesRegex(ValueError, 'notarized_signed_package_required'):
            M.validate_release(record, notes, 'v0.1.1', COMMIT, TREE)
        record = self.record()
        record['version'] = record['platforms']['macos']['version'] = '0.1.1-beta.1'
        record['platforms']['macos']['artifacts'] = []
        self.assertIsNone(M.validate_release(record, {**notes, 'channel': 'beta'}, 'v0.1.1-beta.1', COMMIT, TREE))

    def test_new_item_preserves_older_notes_archive_and_has_no_beta_channel(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'appcast.xml'
            metadata = dict(version='0.1.1',build=30,length=20,sha256='c'*64,url='https://github.com/oinkvm/oink/releases/download/v0.1.1/Oink-0.1.1-macos-arm64.zip')
            M.write_appcast(path,None,metadata,'public-signature',NOTES)
            older = ET.fromstring(path.read_bytes()).find('./channel/item')
            self.assertEqual(older.find('enclosure').get('url'), metadata['url'])
            self.assertEqual(older.findtext(f'{{{M.appcast.SPARKLE}}}version'), '30')
            previous = path.read_bytes()
            M.write_appcast(path,previous,{**metadata,'build':31,'version':'0.1.2'},'new-signature',NOTES)
            items = ET.fromstring(path.read_bytes()).findall('./channel/item')
            self.assertEqual(ET.tostring(items[1]), ET.tostring(older))
            self.assertEqual(len(items),2)
            for item in items:
                self.assertIsNone(item.find(f'{{{M.appcast.SPARKLE}}}channel'))
                self.assertEqual(json.loads(item.findtext(f'{{{M.appcast.OINK}}}releaseNotes')),NOTES)

    def test_signer_receives_key_on_stdin_and_no_output_is_forwarded(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'Oink.zip'; path.write_bytes(b'archive')
            signature = base64.b64encode(b's'*64).decode()
            with patch.dict(os.environ,{M.SECRET:SEED}), patch.object(M,'command',return_value=signature.encode()) as run, \
                 patch.object(M.feed,'verify') as verify:
                self.assertEqual(M.sign(Path('sign_update'),path,PUBLIC),signature)
                self.assertEqual(run.call_args.kwargs,dict(input=SEED.encode()))
                self.assertNotIn(SEED, str(run.call_args.args))
                self.assertEqual(verify.call_count, 1)
                self.assertEqual(verify.call_args.args, (b'archive', signature, PUBLIC))

    @unittest.skipUnless(os.environ.get('SPARKLE_TEST_SIGN_UPDATE'),'pinned official native signer optional')
    def test_official_archive_and_feed_signatures_with_public_test_seed(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ,{M.SECRET:SEED}):
            directory = Path(tmp); archive=directory/'Oink.zip';archive.write_bytes(b'Public TEST archive')
            tool = Path(os.environ['SPARKLE_TEST_SIGN_UPDATE'])
            signature = M.sign(tool,archive,PUBLIC)
            metadata = dict(version='0.1.1',build=30,length=archive.stat().st_size,sha256=M.feed.digest(archive.read_bytes()),
                            url='https://github.com/oinkvm/oink/releases/download/v0.1.1/Oink-0.1.1-macos-arm64.zip')
            path=directory/'appcast.xml'
            M.write_appcast(path,None,metadata,signature,NOTES)
            M.sign(tool,path,PUBLIC,xml=True)
            config=dict(mac=dict(enabled=True,epoch='mac-2026-01',public_key=PUBLIC),repository='oinkvm/oink')
            with patch.object(M.feed,'download',return_value=archive.read_bytes()):
                self.assertEqual(M.feed_site.appcast(path.read_bytes(),config,'stable')[0]['length'],archive.stat().st_size)
                path.write_bytes(path.read_bytes().replace(b'Test update',b'Tampered update'))
                with self.assertRaises(ValueError):M.feed_site.appcast(path.read_bytes(),config,'stable',True)


class MacPromotionTests(unittest.TestCase):
    """candidate() of one vX.Y.Z build: prerelease (beta) first, then promoted to stable."""
    DMG = b'notarized TEST dmg bytes'
    SIGNATURE = base64.b64encode(b's' * 64).decode()

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.runs = 0

    def record(self):
        zip_asset = dict(name='Oink-0.3.6-macos-arm64.zip', size=20, sha256='c' * 64)
        dmg = dict(name='Oink-0.3.6-macos-arm64.dmg', size=len(self.DMG), sha256=M.feed.digest(self.DMG))
        common = dict(version='0.3.6', build=306001, built_commit=COMMIT, source_tree_sha256=TREE)
        mac = dict(**common, source_commit=None, team_id='893V37X2CW', notarized=True, architecture='arm64',
                   notarization={k: dict(status='Accepted', stapled=True) for k in ['app', 'dmg']},
                   launch_smoke=dict(status='passed', signed=True, seconds=10), minimum_os='14.2',
                   protocol_baseline=1, artifacts=[zip_asset, dmg])
        return dict(schema=1, product='Oink', **common, platforms=dict(macos=mac), artifacts=[zip_asset, dmg])

    def candidate(self, state, prerelease, notes=NOTES):
        """One publisher run against `state` (the deployed site); returns (output, v1 signer mock)."""
        self.runs += 1
        M.feed.write(self.root / '.github/update-notes.json', dict(severity='normal', notes=notes))
        (state / 'site/mac').mkdir(parents=True, exist_ok=True)
        config = dict(repository='oinkvm/oink', mac=dict(enabled=True, public_key=PUBLIC, epoch='mac-2026-01'))
        record = self.record()
        download = lambda url, limit, redirects=False: M.feed.encode(record) if url.endswith('/release.json') else self.DMG
        output = self.root / f'output-{self.runs}'
        with patch.object(M, 'published', return_value=dict(prerelease=prerelease)), \
                patch.object(M.feed, 'ROOT', self.root), patch.object(M.feed, 'trust', return_value=config), \
                patch.object(M, 'tag_source', return_value=(COMMIT, TREE, config['mac'])), \
                patch.object(M.feed, 'download', side_effect=download), \
                patch.object(M.feed_site, 'state_info', return_value={'platforms': {'mac': {'catalog': {}}}}), \
                patch.object(M.feed_site, 'verify_platform'), patch.object(M, 'prepare', return_value=self.root), \
                patch.object(M, 'verify_dmg'), patch.object(M, 'sign', return_value=self.SIGNATURE), \
                patch.object(M, 'sign_v1') as v1:
            self.assertEqual(M.candidate(state, output, self.root / 'cache', 'v0.3.6'), 306001)
        return output, v1

    def published_record(self, output):
        return M.feed.read(output / 'site/mac/releases/306001/arm64.json')

    def test_prerelease_is_beta_only_and_promotion_rewrites_the_same_record_to_stable(self):
        beta, v1 = self.candidate(self.root / 'empty', prerelease=True)
        v1.assert_not_called()
        record = self.published_record(beta)
        self.assertEqual(record['release']['channel'], 'beta')
        self.assertEqual(record['release']['tag'], 'v0.3.6')

        stable, v1 = self.candidate(beta, prerelease=False)
        v1.assert_called_once()
        promoted = self.published_record(stable)
        self.assertEqual(promoted, {**record, 'release': {**record['release'], 'channel': 'stable'}})
        self.assertEqual(sorted(p.relative_to(stable).as_posix() for p in (stable / 'site/mac/releases').rglob('*.json')),
                         ['site/mac/releases/306001/arm64.json'])

    def test_promotion_mismatches_and_republication_are_rejected(self):
        beta, _ = self.candidate(self.root / 'empty', prerelease=True)
        with self.assertRaisesRegex(ValueError, 'release_build_changed'):
            self.candidate(beta, prerelease=False, notes={**NOTES, 'en': 'Edited after the prerelease'})
        with self.assertRaisesRegex(ValueError, 'release_not_monotonic'):
            self.candidate(beta, prerelease=True)
        stable, _ = self.candidate(beta, prerelease=False)
        with self.assertRaisesRegex(ValueError, 'release_not_monotonic'):
            self.candidate(stable, prerelease=False)
        # Never stable -> beta.
        with self.assertRaisesRegex(ValueError, 'release_build_changed'):
            self.candidate(stable, prerelease=True)


if __name__ == '__main__':unittest.main()
