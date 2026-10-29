"""Every emitted MSI File row must remain acceptable to shipped 0.2.2 readers."""
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location('bridge_022', Path(__file__).resolve().parents[1] / 'windows/bridge_022.py')
B = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(B)


def tables(extra=''):
    result = {
        'Directory': [['INSTALLFOLDER', 'ProgramFiles64Folder', 'Oink'],
                      ['Audio', 'INSTALLFOLDER', 'AUDIO~1|AudioPlugin'],
                      ['Licenses', 'INSTALLFOLDER', 'licenses'],
                      ['Text', 'Licenses', 'texts']],
        'Component': [['Root', '', 'INSTALLFOLDER'], ['Audio', '', 'Audio'], ['Text', '', 'Text']],
        'File': [[name, 'Root', name] for name in sorted(B.FILES) if '/' not in name]
                + [['Vendor', 'Audio', 'CABLE~1.ZIP|VBCABLE_Driver_Pack45.zip'],
                   ['License', 'Text', 'fixture.txt']],
    }
    if extra:
        result['File'].append(['Extra', 'Audio', extra])
    return result


class Bridge022(unittest.TestCase):
    def test_all_actual_file_rows_are_checked_against_the_frozen_allowlist(self):
        # A future reader fix cannot repair already-shipped 0.2.2 updaters; a
        # single extra File row breaks upgrade before any custom action runs.
        self.assertEqual(len(B.verify_file_table(tables())), len(B.FILES) + 1)
        for extra in ('Microphone.ps1', 'future-helper.dll', 'other.zip'):
            with self.subTest(extra=extra), self.assertRaisesRegex(RuntimeError, 'shipped 0.2.2'):
                B.verify_file_table(tables(extra))

    def test_component_directory_ancestry_and_resource_names_are_validated(self):
        for kind in ('outside', 'cycle', 'missing-component', 'reserved', 'escape', 'source-directory'):
            fixture = tables()
            if kind == 'outside': fixture['Directory'][1][1] = 'ProgramFiles64Folder'
            if kind == 'cycle': fixture['Directory'][1][1] = 'Audio'
            if kind == 'missing-component': fixture['File'][0][1] = 'Missing'
            if kind == 'reserved': fixture['File'][-1][2] = 'NUL.txt'
            if kind == 'escape': fixture['File'][-1][2] = '../escape.txt'
            if kind == 'source-directory': fixture['Directory'][1][2] = 'AudioPlugin:Source'
            with self.subTest(kind=kind), self.assertRaises(RuntimeError):
                B.verify_file_table(fixture)
