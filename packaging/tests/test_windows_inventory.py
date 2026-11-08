"""Product-root inventories permit new resources without a filename allow-list."""
import importlib.util
from pathlib import Path
import unittest
SPEC = importlib.util.spec_from_file_location('windows_inventory', Path(__file__).resolve().parents[1] / 'windows/inventory.py')
B = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(B)

def tables():
    return {'Directory':[['INSTALLFOLDER','ProgramFiles64Folder','Oink'],['Resources','INSTALLFOLDER','resources']],
            'Component':[['Root','','INSTALLFOLDER'],['Resource','','Resources']],
            'File':[['Core','Root','oink.exe'],['Paste','Root','OinkPasteMenu.dll'],['Provider','Root','OinkCredentialProvider.dll'],['Other','Resource','future.dat']]}

class Inventory(unittest.TestCase):
    def test_future_files_are_accepted_without_per_filename_exceptions(self):
        self.assertEqual(B.verify_file_table(tables()),['oink.exe','OinkPasteMenu.dll','OinkCredentialProvider.dll','resources/future.dat'])
    def test_aliases_escapes_cycles_and_reserved_state_stay_rejected(self):
        for name in ['NUL.txt','../escape','a:stream','maintenance','Files.json','Installed.json']:
            value=tables();value['File'][-1]=['Bad','Root',name]
            with self.subTest(name=name),self.assertRaises(RuntimeError):B.verify_file_table(value)
        for parent in ['ProgramFiles64Folder','Resources','Missing']:
            value=tables();value['Directory'][1][1]=parent
            with self.subTest(parent=parent),self.assertRaises(RuntimeError):B.verify_file_table(value)
