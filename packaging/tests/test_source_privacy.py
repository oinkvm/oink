import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
import tempfile
import unittest

from source_privacy import scan


class SourcePrivacy(unittest.TestCase):
    def test_identifier_underscores_and_camel_case_do_not_hide_markers(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'fixture.rs').write_text('fn route_to_fixturebox() {}\nfn routeToFixtureBox() {}\n')
            findings = scan(root, ['fixturebox'])
            self.assertEqual([(f['scope'], f['line']) for f in findings], [('content', 1), ('content', 2)])
            self.assertTrue(all('fixturebox' not in str(f) for f in findings))

    def test_file_and_directory_names_are_scanned(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            directory = root / 'tests/current_FixtureBox'
            directory.mkdir(parents=True)
            (directory / 'layout.json').write_text('{}')
            (root / 'current_c_fixturebox.rs').write_text('// neutral contents\n')
            findings = scan(root, ['fixturebox'])
            self.assertEqual(len(findings), 2)
            self.assertTrue(all(f['scope'] == 'path' and f['line'] is None for f in findings))

    def test_markers_are_literals_and_generic_names_pass(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'remote_windows.rs').write_text('fixtureXbox\n')
            self.assertEqual(scan(root, ['fixture.box']), [])
            (root / 'remote_windows.rs').write_text('fixture.box\n')
            self.assertEqual(len(scan(root, ['fixture.box'])), 1)

    def test_private_internal_documents_and_build_outputs_are_not_export_inputs(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for relative in ['docs/internal.md', 'scripts/local.py', 'target/output', '.git']:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('fixturebox')
            self.assertEqual(scan(root, ['fixturebox']), [])

    def test_nested_public_readmes_are_scanned(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root / 'crates/fixture/README.md'
            path.parent.mkdir(parents=True)
            path.write_text('fixturebox')
            self.assertEqual(len(scan(root, ['fixturebox'])), 1)

    def test_invalid_marker_configuration_fails(self):
        for markers in [None, 'fixturebox', [], [''], [123]]:
            with self.assertRaises(ValueError):
                scan(Path('.'), markers)
