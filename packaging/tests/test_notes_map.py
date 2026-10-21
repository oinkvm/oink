"""A new language needs data only; 0.1.3 itself remains readable by old clients."""
import json
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT/'build-support/update'))
import feed

class NotesMap(unittest.TestCase):
    def test_future_languages_are_data_but_ambiguous_or_invalid_tags_are_rejected(self):
        notes={'en':'First\nSecond','zh-Hans':'第一项\n第二项','ja':'テスト','ko':'테스트','es-MX':'Prueba'}
        self.assertEqual(feed.validate_notes(notes),notes)
        for tag in ['x','en_US','en-a','../ja','EN']:
            with self.subTest(tag=tag),self.assertRaises(ValueError): feed.validate_notes({**notes,tag:'test'})
        with self.assertRaises(ValueError): feed.validate_notes({**notes,**{f'zz-{i:02d}':'test' for i in range(33)}})
        for invalid in ['', '\r', '\u0085', 'x'*(feed.MAX_NOTES+1)]:
            with self.assertRaises(ValueError): feed.validate_notes({**notes,'ja':invalid})

    def test_013_release_bytes_stay_bilingual_for_shipped_strict_clients(self):
        identity=json.loads((ROOT/'packaging/identity.json').read_text())
        if identity['version']=='0.1.3':
            notes=feed.read(ROOT/'.github/update-notes.json')['notes']
            self.assertEqual(set(notes),{'en','zh-Hans'})
            self.assertEqual([len(text.split('\n')) for text in notes.values()],[9,9])
            feed.validate_notes(notes)
            self.assertEqual(notes,feed.read(ROOT/'crates/oink-update/tests/fixtures/release-notes-0.1.3.json'))
