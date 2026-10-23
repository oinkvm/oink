"""The bridge embeds approved copy without adding a runtime installer feature."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support'))


class EmbeddedReleaseNotes(unittest.TestCase):
    def test_approved_notes_are_written_in_the_existing_schema(self):
        import embedded_release_notes as notes
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.github').mkdir()
            approved = {'en': 'Prepare Windows updates.', 'zh-Hans': '准备 Windows 更新。',
                        'ja': 'Windows の更新を準備します。', 'ko': 'Windows 업데이트를 준비합니다.'}
            (root / '.github/update-notes.json').write_text(json.dumps(
                {'channel': 'stable', 'severity': 'normal', 'notes': approved}))
            path = notes.write(root, root / 'Resources/oink-update-notes.json', 43, None)
            value = json.loads(path.read_bytes())
            self.assertEqual(value['schema'], 1)
            self.assertEqual(value['build'], 43)
            self.assertEqual(value['notes'], approved)
            self.assertEqual(value['kind'], 'public')
            import hashlib
            notes.verify_windows({'Binary': [['OinkLocalUpdateNotes', hashlib.sha256(path.read_bytes()).hexdigest()]]}, path)
            for binary in ([], [['OinkLocalUpdateNotes', '0' * 64]]):
                with self.assertRaises(RuntimeError):
                    notes.verify_windows({'Binary': binary}, path)

    def test_public_notes_must_match_the_committed_approval(self):
        import embedded_release_notes as notes
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.github').mkdir()
            path = root / '.github/update-notes.json'
            path.write_bytes((ROOT / '.github/update-notes.json').read_bytes())
            for args in [('init', '-q'), ('add', '.'), ('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'Approved')]:
                subprocess.run(['git', *args], cwd=root, check=True, capture_output=True)
            notes.write(root, root / 'notes.json', 43, None)
            changed = json.loads(path.read_bytes());changed['notes']['en'] = 'Unapproved edit.'
            path.write_text(json.dumps(changed))
            with self.assertRaises(ValueError):
                notes.write(root, root / 'changed.json', 43, None)
