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
        asset = dict(name='Oink-0.1.1-30-macos-arm64.zip', size=20, sha256='c'*64)
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
        for change in [dict(channel='beta'), dict(notes={'en':'missing Chinese'})]:
            with self.assertRaises(ValueError): M.validate_release(record, {**notes,**change}, 'v0.1.1', COMMIT, TREE)

    def test_release_event_must_still_be_published_when_queried(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'event.json'
            release = dict(id=10, tag_name='v0.1.1',draft=False,prerelease=False,published_at='2026-10-20T20:04:19Z')
            event = dict(action='published', release=release)
            path.write_text(json.dumps(event))
            env = dict(GITHUB_ACTIONS='true',GITHUB_EVENT_PATH=str(path),GITHUB_EVENT_NAME='release',GITHUB_REPOSITORY='oinkvm/oink')
            with patch.dict(os.environ,env), patch.object(M,'command',return_value=json.dumps(release).encode()):
                M.published('v0.1.1')
                for draft, prerelease in [(True,False),(False,True)]:
                    path.write_text(json.dumps({**event,'release':{**release,'draft':draft,'prerelease':prerelease}}))
                    with self.assertRaisesRegex(ValueError,'published_release_required'): M.published('v0.1.1')
            path.write_text(json.dumps(event))
            with patch.dict(os.environ,env), patch.object(M,'command',return_value=json.dumps({**release,'draft':True}).encode()):
                with self.assertRaisesRegex(ValueError,'release_not_public'): M.published('v0.1.1')

    def test_new_item_preserves_older_notes_archive_and_has_no_beta_channel(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'appcast.xml'
            metadata = dict(version='0.1.1',build=30,length=20,sha256='c'*64,url='https://github.com/oinkvm/oink/releases/download/v0.1.1/Oink.zip')
            M.write_appcast(path,None,metadata,'public-signature',NOTES)
            older = ET.fromstring(path.read_bytes()).find('./channel/item')
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
                verify.assert_called_once_with(b'archive',signature,PUBLIC)

    @unittest.skipUnless(os.environ.get('SPARKLE_TEST_SIGN_UPDATE'),'pinned official native signer optional')
    def test_official_archive_and_feed_signatures_with_public_test_seed(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ,{M.SECRET:SEED}):
            directory = Path(tmp); archive=directory/'Oink.zip';archive.write_bytes(b'Public TEST archive')
            tool = Path(os.environ['SPARKLE_TEST_SIGN_UPDATE'])
            signature = M.sign(tool,archive,PUBLIC)
            metadata = dict(version='0.1.1',build=30,length=archive.stat().st_size,sha256=M.feed.digest(archive.read_bytes()),
                            url='https://github.com/oinkvm/oink/releases/download/v0.1.1/Oink.zip')
            path=directory/'appcast.xml'
            M.write_appcast(path,None,metadata,signature,NOTES)
            M.sign(tool,path,PUBLIC,xml=True)
            config=dict(mac=dict(enabled=True,epoch='mac-2026-01',public_key=PUBLIC),repository='oinkvm/oink')
            with patch.object(M.feed,'download',return_value=archive.read_bytes()):
                self.assertEqual(M.feed_site.appcast(path.read_bytes(),config,'stable')[0]['length'],archive.stat().st_size)
                path.write_bytes(path.read_bytes().replace(b'Test update',b'Tampered update'))
                with self.assertRaises(ValueError):M.feed_site.appcast(path.read_bytes(),config,'stable',True)


if __name__ == '__main__':unittest.main()
