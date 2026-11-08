"""Credential DLL is a signed File-table payload, never a sixth receipt role."""
import sys
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support'))
from identity import signpath_configuration

class CredentialProviderTests(unittest.TestCase):
    def test_signing_includes_provider(self):
        root = ET.fromstring(signpath_configuration())
        images = {e.get('path') for e in root.iter() if e.tag.endswith('}pe-file')}
        self.assertIn('OinkCredentialProvider.dll', images)
        self.assertIn('OinkPasteMenu.dll', images)
        self.assertEqual(sum(name.endswith('.exe') for name in images), 5)

    def test_file_table_and_service_unregister(self):
        source = (ROOT / 'packaging/windows/Package.wxs').read_text()
        self.assertIn('Source="$(var.Payload)/OinkCredentialProvider.dll"', source)
        self.assertIn('Source="$(var.Payload)/OinkPasteMenu.dll"', source)
        self.assertIn('--unregister-credential-provider', source)

    def test_complete_inventory_and_dll_signature(self):
        source = (ROOT / 'crates/oink-winsvc/src/platform/update/msi.rs').read_text()
        self.assertIn('"OinkCredentialProvider.dll"', source)
        self.assertIn('ends_with(".dll")', source)
