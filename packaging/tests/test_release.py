"""Public assembly must bind both platforms and never attach symbols."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support'))
import public_release as R
from provenance import digest

COMMIT = 'a' * 40
TREE = 'b' * 64
DSN = 'https://' + '1' * 32 + '@o1.ingest.de.sentry.io/2'


class PublicRelease(unittest.TestCase):
    def setUp(self):
        identity = patch.object(R, 'IDENTITY', {**R.IDENTITY, 'version': '0.1.0'})
        identity.start()
        self.addCleanup(identity.stop)

    def test_configuration_is_required_but_signpath_is_optional(self):
        env = dict(OINK_REPORT_DSN=DSN, OINK_REPORT_REGION='eu')
        self.assertFalse(R.inputs('28', env))
        for build in ['0', '27', 'x', '2147483648']:
            with self.assertRaises(ValueError): R.inputs(build, env)
        for change in [dict(OINK_REPORT_DSN=''), dict(OINK_REPORT_REGION='us'), dict(SIGNPATH_PROJECT_SLUG='only-one')]:
            with self.assertRaises(ValueError): R.inputs('28', {**env, **change})
        self.assertTrue(R.inputs('28', {**env, 'SIGNPATH_PROJECT_SLUG':'x', 'SIGNPATH_ORGANIZATION_ID':'x', 'SIGNPATH_API_TOKEN':'fixture'}))

    def package(self, root, platform):
        path = root / TREE / ('macos-arm64' if platform == 'macos' else 'windows-x64')
        path.mkdir(parents=True)
        name = 'Oink-0.1.0-28-' + ('macos-arm64.dmg' if platform == 'macos' else 'windows-x64.msi')
        f = path / name; f.write_bytes(platform.encode())
        record = dict(version='0.1.0', build=28, source_commit=None, source_tree_sha256=TREE,
            notarized=True, notarization={k:dict(status='Accepted',stapled=True) for k in ['app','dmg']},
            launch_smoke=dict(status='passed',signed=True,seconds=10),
            artifacts=[dict(name=name,size=f.stat().st_size,sha256=digest(f))])
        if platform == 'macos':
            archive = path / name.replace('.dmg', '.zip')
            archive.write_bytes(b'Stapled Sparkle archive')
            record['artifacts'].append(dict(name=archive.name, size=archive.stat().st_size, sha256=digest(archive)))
        (path / 'release.json').write_text(json.dumps(record))
        proof = dict(configured=True,report_sent=False,version='0.1.0',build=28)
        (path / 'report-configuration-check.json').write_text(json.dumps(proof))
        (path / 'symbols.noindex').mkdir(); (path / 'symbols.noindex/private.pdb').write_bytes(b'private')
        return path,record

    @patch.object(R, 'source_tree_sha256', return_value=TREE)
    def test_both_platforms_staged_and_assembled_with_hashes(self, _hash):
        with tempfile.TemporaryDirectory() as tmp:
            p=Path(tmp)
            for platform in ['macos','windows']:
                self.package(p/'packages',platform)
                R.stage(p/'packages',p/'download'/platform,platform,28,COMMIT)
            assets=R.assemble(p/'download',p/'final',28,COMMIT)
            self.assertEqual({x.suffix for x in assets},{'.dmg','.zip','.msi','.json',''})
            self.assertEqual(len(list((p/'final').iterdir())),6)
            self.assertEqual(json.loads((p/'final/update.json').read_text())['channel'], 'stable')
            for line in (p/'final/SHA256SUMS').read_text().splitlines():
                sha,name=line.split('  ');self.assertEqual(sha,digest(p/'final'/name))
            record=json.loads((p/'download/windows/release-windows.json').read_text());record['built_commit']='c'*40
            (p/'download/windows/release-windows.json').write_text(json.dumps(record))
            with self.assertRaisesRegex(ValueError,'share'):R.assemble(p/'download',p/'bad',28,COMMIT)

    @patch.object(R, 'source_tree_sha256', return_value=TREE)
    def test_tampered_package_and_unnotarized_mac_rejected(self,_hash):
        with tempfile.TemporaryDirectory() as tmp:
            p=Path(tmp);directory,record=self.package(p/'packages','macos')
            record['notarized']=False;(directory/'release.json').write_text(json.dumps(record))
            with self.assertRaisesRegex(ValueError,'Accepted'):R.stage(p/'packages',p/'out','macos',28,COMMIT)
            record['notarized']=True;(directory/'release.json').write_text(json.dumps(record));(directory/record['artifacts'][0]['name']).write_bytes(b'bad')
            with self.assertRaisesRegex(ValueError,'hash'):R.stage(p/'packages',p/'out','macos',28,COMMIT)

    def test_publish_is_draft_targeted_and_refuses_different_existing_tag(self):
        with tempfile.TemporaryDirectory() as tmp:
            p=Path(tmp);(p/'release.json').write_text(json.dumps(dict(built_commit=COMMIT,artifacts=[dict(name='x.dmg'),dict(name='x.msi')])))
            with patch.object(R.subprocess,'check_output',return_value=''),patch.object(R.subprocess,'run') as run:
                R.publish(p,COMMIT,Path('notes.txt'));cmd=run.call_args.args[0]
                self.assertIn('--draft',cmd);self.assertEqual(cmd[cmd.index('--target')+1],COMMIT)
                self.assertEqual(len(cmd[-4:]),4)
            with patch.object(R.subprocess,'check_output',return_value='c'*40+'\trefs/tags/v0.1.0\n'),patch.object(R.subprocess,'run') as run:
                with self.assertRaisesRegex(ValueError,'different'):R.publish(p,COMMIT,Path('notes.txt'))
                run.assert_not_called()

    def test_notarization_secrets_missing_fail_before_keychain_commands(self):
        spec=importlib.util.spec_from_file_location('ci_keychain',ROOT/'packaging/macos/ci-keychain.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as tmp,patch.dict(os.environ,dict(GITHUB_ACTIONS='true',RUNNER_OS='macOS',RUNNER_TEMP=tmp,GITHUB_OUTPUT=str(Path(tmp)/'out')),clear=True),patch.object(sys,'argv',['ci-keychain.py']),patch.object(module,'run') as run:
            with self.assertRaisesRegex(RuntimeError,'notarization'):module.main()
            run.assert_not_called()

    def test_imported_identity_is_searchable_before_selection_and_notary_uses_keychain(self):
        spec=importlib.util.spec_from_file_location('ci_keychain',ROOT/'packaging/macos/ci-keychain.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
        sys.path.insert(0,str(ROOT/'packaging/macos'))
        import signing
        with tempfile.TemporaryDirectory() as tmp:
            env=dict(GITHUB_ACTIONS='true',RUNNER_OS='macOS',RUNNER_TEMP=tmp,GITHUB_OUTPUT=str(Path(tmp)/'out'),
                MACOS_CERTIFICATE_P12='Zml4dHVyZQ==',MACOS_CERTIFICATE_PASSWORD='fixture',MACOS_SIGNING_FINGERPRINT='A'*40,
                APPLE_NOTARY_KEY_P8='fixture',APPLE_NOTARY_KEY_ID='fixture',APPLE_NOTARY_ISSUER='fixture')
            calls=[]
            def run(*args,**kwargs):
                calls.append(args)
                return '"original.keychain-db"' if kwargs.get('capture') else None
            def select(team,fingerprint):
                self.assertEqual(team,'893V37X2CW');self.assertEqual(fingerprint,'A'*40)
                self.assertEqual(calls[-1][:5],('security','list-keychains','-d','user','-s'))
                self.assertEqual(calls[-1][5],str(Path(tmp)/'release-signing/signing.keychain-db'))
            with patch.dict(os.environ,env,clear=True),patch.object(sys,'argv',['ci-keychain.py']),patch.object(module,'run',side_effect=run),patch.object(signing,'select_identity',side_effect=select):
                module.main()
            notary=next(c for c in calls if c[:3]==('xcrun','notarytool','store-credentials'))
            self.assertEqual(notary[-2:],('--keychain',str(Path(tmp)/'release-signing/signing.keychain-db')))
            self.assertFalse((Path(tmp)/'release-signing/notary.p8').exists())
            self.assertFalse((Path(tmp)/'release-signing/certificate.p12').exists())

class WorkflowContract(unittest.TestCase):
    def test_product_versions_are_aligned_for_public_and_local_labels(self):
        version = R.IDENTITY['version']
        cargo = tomllib.loads((ROOT / 'Cargo.toml').read_text(encoding='utf-8'))
        self.assertEqual(cargo['workspace']['package']['version'], version)
        for member in cargo['workspace']['members']:
            package = tomllib.loads((ROOT / member / 'Cargo.toml').read_text(encoding='utf-8'))['package']
            self.assertEqual(package['version'], {'workspace': True})
        for path in ('apps/oink/package.json', 'apps/oink/src-tauri/tauri.conf.json'):
            self.assertEqual(json.loads((ROOT / path).read_text(encoding='utf-8'))['version'], version)
        lock = json.loads((ROOT / 'package-lock.json').read_text(encoding='utf-8'))
        self.assertEqual(lock['packages']['apps/oink']['version'], version)
        rust_lock = tomllib.loads((ROOT / 'Cargo.lock').read_text(encoding='utf-8'))
        workspace = [p for p in rust_lock['package'] if p['name'].startswith('oink-')]
        self.assertEqual(len(workspace), len(cargo['workspace']['members']))
        self.assertTrue(all(p['version'] == version for p in workspace))

    def test_workflow_contract_and_only_draft_can_write_contents(self):
        workflow=(ROOT/'.github/workflows/release.yml').read_text()
        self.assertEqual(workflow.count('contents: write'),1)
        self.assertIn('  draft:\n    needs: package',workflow)
        self.assertIn('--public-version',workflow)
        self.assertIn('OINK_REPORT_REGION: eu',workflow)
        self.assertNotIn('OINK_REPORT_BUILD_MANIFEST',workflow)
        self.assertNotIn('MACOS_SIGNING_IDENTITY',workflow)
        self.assertNotIn('APPLE_TEAM_ID:',workflow)
        self.assertNotIn('SPARKLE_PUBLIC_ED_KEY',workflow)
        self.assertIn('github.event.repository.default_branch',workflow)
        self.assertIn('steps.inputs.outputs.sign_windows',workflow)
        self.assertIn("runner.os == 'macOS' && runner.environment == 'github-hosted'", workflow)
        self.assertIn('proto udp from any to any port 443', workflow)
        for filename in ['ci.yml', 'release.yml']:
            self.assertIn('cargo install cargo-about --version 0.9.2 --locked --features cli',
                          (ROOT / '.github/workflows' / filename).read_text())
