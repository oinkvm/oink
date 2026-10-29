import json
import hashlib
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
import package_notes as notes

class PackageNotesTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.root = Path(self.tmp.name)
        subprocess.run(['git','init','-q',str(self.root)],check=True)
        subprocess.run(['git','-C',str(self.root),'config','user.name','Notes test'],check=True)
        subprocess.run(['git','-C',str(self.root),'config','user.email','notes@example.invalid'],check=True)
    def tearDown(self): self.tmp.cleanup()
    def commit(self, subject):
        p=self.root/'file';p.write_text(subject)
        subprocess.run(['git','-C',str(self.root),'add','.'],check=True)
        subprocess.run(['git','-C',str(self.root),'commit','-qm',subject],check=True)
        return subprocess.check_output(['git','-C',str(self.root),'rev-parse','HEAD'],text=True).strip()
    def test_inhouse_tracks_previous_completed_lower_build_and_prioritizes_user_changes(self):
        old=self.commit('feat: original');self.commit('chore: dependency refresh')
        self.commit('fix(input): prevent stuck keys');new=self.commit('feat(update): accept dropped packages')
        out=self.root/'packages';p=out/'old'/'release.json';p.parent.mkdir(parents=True)
        previous={'build':39,'version':'0.3.0-inhouse.39','source_commit':old}
        p.write_text(json.dumps(previous))
        result=notes.generate(self.root,40,new,out)
        self.assertEqual(result['previous'],previous)
        self.assertEqual(result['source_commit'],new)
        text=result['notes']['en'];self.assertLess(text.index('prevent stuck keys'),text.index('dependency refresh'))
        self.assertNotIn('original',text)
    def test_previous_accepts_old_commit_labels_and_matching_build_labels(self):
        commit = '0123abcd' * 5
        path = self.root / 'previous.json'
        for label in ('0.3.0-inhouse.42', '0.3.0-inhouse.0123abcd'):
            record = {'build':42, 'version':label, 'source_commit':commit}
            path.write_text(json.dumps(record))
            self.assertEqual(notes.previous_record(self.root, 44, path), record)
        for label in ('0.3.0-inhouse.41', '0.3.0-inhouse.042', '0.3.0-inhouse.deadbeef'):
            path.write_text(json.dumps({**record, 'version':label}))
            with self.assertRaises(ValueError):
                notes.previous_record(self.root, 44, path)

    def test_public_uses_all_committed_approved_languages(self):
        self.commit('feat: initial')
        p=self.root/'.github/update-notes.json';p.parent.mkdir()
        approved={'channel':'stable','severity':'normal','notes':{'en':'Approved','zh-Hans':'已批准','ja':'承認済み','ko':'승인됨'}}
        p.write_text(json.dumps(approved));self.commit('docs: approve notes')
        result=notes.generate(self.root,40,None,self.root/'packages')
        self.assertEqual(result['notes'],approved['notes'])
        p.write_text(json.dumps({**approved,'notes':{**approved['notes'],'en':'uncommitted'}}))
        with self.assertRaisesRegex(ValueError,'committed'):notes.generate(self.root,40,None,self.root/'packages')
    def test_archive_uses_generated_sidecar_and_missing_notes_are_explicit(self):
        old=self.commit('feat: old');new=self.commit('fix: new')
        p=self.root/'previous.json';p.write_text(json.dumps({'build':2,'version':'0.3.0-inhouse.'+old[:8],'source_commit':old}))
        result=notes.generate(self.root,3,new,self.root/'packages',p)
        target=self.root/'notes.json';notes.write(target,result)
        self.assertEqual(notes.read(target,3,new),result)
        with self.assertRaises(ValueError):notes.read(target,4,new)
        resource=self.root/'bundle/Contents/Resources';notes.embed_macos(target,resource)
        self.assertEqual(json.loads((resource/'oink-update-notes.json').read_text()),result)
    def test_platform_public_sidecar_must_match_committed_approved_notes(self):
        p=self.root/'.github/update-notes.json';p.parent.mkdir()
        p.write_text(json.dumps({'notes':{'en':'Approved','zh-Hans':'已批准','ja':'承認済み','ko':'승인됨'}}))
        self.commit('docs: approve notes')
        approved=notes.generate(self.root,40,None,self.root/'packages')
        target=self.root/'embedded.json';notes.write(target,approved)
        self.assertEqual(notes.read(target,40,None,self.root),approved)
        notes.write(target,{**approved,'notes':{**approved['notes'],'en':'unapproved'}})
        with self.assertRaisesRegex(ValueError,'approved'):notes.read(target,40,None,self.root)
    def test_windows_package_binds_notes_to_the_real_binary_stream_hash(self):
        target=self.root/'embedded-notes.json';target.write_text('{"schema":1,"notes":{"en":"A fix"}}')
        row=['OinkLocalUpdateNotes',hashlib.sha256(target.read_bytes()).hexdigest()]
        notes.verify_windows_embedding({'Binary':[['InstallerCA','0'*64],row]},target)
        for rows in [[],[['OinkLocalUpdateNotes','0'*64]],[row,row]]:
            with self.assertRaisesRegex(RuntimeError,'embedded notes'):
                notes.verify_windows_embedding({'Binary':rows},target)
    def test_same_commit_is_clear_and_wrong_history_is_refused(self):
        old=self.commit('feat: old');p=self.root/'previous.json'
        p.write_text(json.dumps({'build':2,'version':'0.3.0-inhouse.'+old[:8],'source_commit':old}))
        self.assertIn('No source changes',notes.generate(self.root,3,old,self.root/'packages',p)['notes']['en'])
        bad='a'*40;p.write_text(json.dumps({'build':2,'version':'0.3.0-inhouse.aaaaaaaa','source_commit':bad}))
        with self.assertRaisesRegex(ValueError,'history'):notes.generate(self.root,3,old,self.root/'packages',p)

if __name__=='__main__': unittest.main()
