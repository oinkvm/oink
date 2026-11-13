"""Shared beta/in-house numbering and draft integrity without native builds or network."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT/'build-support'))
import beta_draft as beta
import build_reservation as reservation
import versioning
import mobile_version
from identity import encode_build, windows_product_version
from provenance import digest

COMMIT='a'*40


class SharedSequence(unittest.TestCase):
    def test_ci_and_inhouse_share_one_ordered_nonoverlapping_counter(self):
        with patch.dict(versioning.IDENTITY, inhouse_version='0.4.0', version='0.3.6'):
            local=beta.plan(82,True,'a'*32); public=beta.plan(83); later=beta.plan(84,True,'b'*32)
            self.assertEqual([r['build'] for r in (local,public,later)],[400082,400083,400084])
            self.assertEqual([r['label'] for r in (local,public,later)],['0.4.0-inhouse.82','0.4.0-beta.83','0.4.0-inhouse.84'])
            self.assertEqual(mobile_version.mobile_version(400083,'beta')['version_name'],'0.4.0-beta.83')
            self.assertEqual(versioning.release_version('0.3.6',None,400083,channel='beta'),'0.4.0-beta.83')
            self.assertEqual(versioning.package_build(83,False,channel='beta'),400083)
            self.assertEqual(versioning.package_build(83,False),306083)
            self.assertEqual(versioning.release_version('0.3.6',None,306083),'0.3.6')
            msi=[tuple(map(int,windows_product_version(r['build']).split('.'))) for r in (local,public,later)]
            self.assertEqual(msi,sorted(set(msi)))
            self.assertEqual(versioning.macos_version_fields('0.4.0-beta.83',400083)['CFBundleGetInfoString'],'0.4.0-beta.83')

    def test_counter_exhaustion_never_wraps_and_legacy_builds_stay_readable(self):
        self.assertEqual(beta.plan(999)['build'],400999)
        for n in (0,8,1000,True,-1):
            with self.subTest(n=n),self.assertRaises(ValueError):beta.plan(n)
        self.assertEqual(mobile_version.mobile_version(400008)['version_name'],'0.4.0-inhouse.8')

    def test_local_receipt_is_bound_to_source_channel_counter_and_version(self):
        with patch.dict(os.environ,GITHUB_RUN_ID='42'):
            r=beta.plan(82,True,'a'*32)
        r['source_commit']=COMMIT
        self.assertEqual(reservation.validate_receipt(r,400082,COMMIT),400082)
        for change in [dict(source_commit='b'*40),dict(channel='beta'),dict(build=400083),dict(sequence=1000),dict(repository='other/repo')]:
            with self.subTest(change=change),self.assertRaises(ValueError):reservation.validate_receipt({**r,**change},400082,COMMIT)

    def test_offline_reservation_stops_without_fallback_or_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            output=Path(tmp)/'reservation.json'
            with patch.object(reservation,'gh',side_effect=subprocess.CalledProcessError(1,['gh'])):
                with self.assertRaises(subprocess.CalledProcessError):reservation.reserve(output,COMMIT)
            self.assertFalse(output.exists())

    def test_beta_notes_are_current_and_do_not_reuse_stable_release_copy(self):
        import package_notes
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ,OINK_PACKAGE_CHANNEL='beta'):
            root=Path(tmp);r=package_notes.generate(root,400083,None,root)
            self.assertIn('0.4.0-beta.83',r['notes']['en'])
            p=root/'notes.json';package_notes.write(p,r)
            self.assertEqual(package_notes.read(p,400083,None,root),r)

    def test_cli_mobile_beta_and_ios_display_keep_numeric_platform_fields(self):
        output=subprocess.check_output([sys.executable,str(ROOT/'build-support/mobile_version.py'),'--channel','beta','--xcode','400083'],text=True)
        self.assertEqual(output.split(),['MARKETING_VERSION=0.4.0','CURRENT_PROJECT_VERSION=400083','OINK_DISPLAY_VERSION=0.4.0-beta.83'])
        failed=subprocess.run([sys.executable,str(ROOT/'build-support/mobile_version.py'),'400083'],capture_output=True,text=True)
        self.assertEqual(failed.returncode,1); self.assertIn('reservation',failed.stderr)
        for name in ['App','Share','Controls','Broadcast']:
            self.assertIn('$(OINK_DISPLAY_VERSION)',(ROOT/f'apps/mobile/ios/Config/{name}.plist').read_text())


class DraftIntegrity(unittest.TestCase):
    def make(self,directory,target,sequence=83):
        d=directory/target;d.mkdir();p=d/('Oink-0.4.0-beta.83-'+target+'.msi');p.write_bytes(b'fixture')
        r=dict(target=target,version='0.4.0-beta.83',build=400000+sequence,built_commit=COMMIT,source_tree_sha256='b'*64,signing='fixture',artifacts=[dict(name=p.name,size=p.stat().st_size,sha256=digest(p))])
        (d/f'manifest-{target}.json').write_text(json.dumps(r));return p

    def test_missing_nonblocking_targets_are_explicit_and_no_ios_is_expected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);self.make(root,'macos-arm64');self.make(root,'windows-x64')
            r=beta.assemble(root,root/'out',beta.plan(83),COMMIT,'b'*64)
            self.assertEqual(r['missing'],['android-arm64-v8a','android-x86_64','macos-x64'])
            self.assertNotIn('ios',beta.TARGETS)

    def test_mismatched_sources_or_versions_or_missing_blocking_target_prevent_draft(self):
        for bad in ('missing','tamper','version'):
            with self.subTest(bad=bad),tempfile.TemporaryDirectory() as tmp:
                root=Path(tmp);p=self.make(root,'macos-arm64')
                if bad!='missing':self.make(root,'windows-x64',84 if bad=='version' else 83)
                if bad=='tamper':p.write_bytes(b'changed')
                with self.assertRaises(ValueError):beta.assemble(root,root/'out',beta.plan(83),COMMIT,'b'*64)

    def test_draft_creation_is_explicit_and_never_modifies_a_published_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);self.make(root,'macos-arm64');self.make(root,'windows-x64')
            out=root/'out';record=beta.assemble(root,out,beta.plan(83),COMMIT,'b'*64)
            assets=[{'name':a['name']} for a in record['artifacts']]+[{'name':'release.json'},{'name':'SHA256SUMS'}]
            replies=['[]',json.dumps({'id':42}),json.dumps({'draft':True,'assets':assets,'html_url':'https://example.invalid/draft'})]
            with patch.object(beta.subprocess,'check_output',side_effect=replies), patch.object(beta.subprocess,'run') as upload, patch.dict(os.environ,GITHUB_STEP_SUMMARY=str(root/'summary')):
                beta.publish(out,record)
            request=json.loads((out/'draft-request.json').read_text())
            self.assertTrue(request['draft']);self.assertTrue(request['prerelease'])
            self.assertEqual(request['tag_name'],'ci-0.4.0-beta.83')
            self.assertEqual(request['target_commitish'],COMMIT)
            self.assertEqual(
                request['body'],
                'NOTICE: Early beta — expect bugs. Not compatible with earlier versions; '
                'all devices in a group must upgrade together. Use with caution.\n\n'
                'Supports mobile devices.',
            )
            self.assertEqual(upload.call_count,1)
            with patch.object(beta.subprocess,'check_output',return_value=json.dumps([dict(tag_name='ci-0.4.0-beta.83',draft=False,target_commitish=COMMIT,id=42)])), patch.object(beta.subprocess,'run') as upload:
                with self.assertRaises(ValueError):beta.publish(out,record)
                upload.assert_not_called()

if __name__=='__main__':unittest.main()
