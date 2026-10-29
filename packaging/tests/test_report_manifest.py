import json
from pathlib import Path
import sys
import tempfile
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
from report_manifest import image_identity, write_manifest
from native_image_fixtures import GUID, pe_image, macho_image


class LinkedIdentity(unittest.TestCase):
    def test_native_guid_normalization_and_image_relative_bounds(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'image'
            # The PE fixture has CodeView age 1 and a zero timestamp/image size;
            # Mach-O images carry neither an age nor a PE code ID.
            for data, age, code_id, bound in ((pe_image(), 1, 0, 0x1500), (macho_image(), 0, None, 0x8000)):
                path.write_bytes(data)
                self.assertEqual(image_identity(path), {'debug_id': list(GUID.bytes), 'debug_age': age,
                                                        'code_id': code_id, 'text_size': bound})
            path.write_bytes(pe_image())
            manifest = Path(directory) / 'report-manifest.json'
            value = write_manifest(manifest, 'a' * 64, '0.1.0-beta.0123abcd', 7, [('agent', path)])
            self.assertEqual(value['version'], {'major': 0, 'minor': 1, 'patch': 0, 'build': 7})
            self.assertEqual(json.loads(manifest.read_text()), value)
            self.assertNotIn(directory, manifest.read_text())

    def test_missing_truncated_or_zero_identity_cannot_be_shipped(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'image'
            invalid = [b'', pe_image()[:0x300], macho_image()[:-1],
                       macho_image()[:-16] + bytes(16)]
            for data in invalid:
                path.write_bytes(data)
                with self.assertRaises(ValueError):
                    image_identity(path)
