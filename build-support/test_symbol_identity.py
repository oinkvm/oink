import os
import struct
import tempfile
from pathlib import Path
import unittest
import uuid
from report_manifest import pdb_identity, image_identity


class IdentityTests(unittest.TestCase):
    def test_pdb_info_stream_retains_guid_endianness_and_age(self):
        data = bytearray(4096 * 5)
        data[:32] = b'Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0'
        struct.pack_into('<IIIIII', data, 32, 4096, 1, 5, 16, 0, 2)
        struct.pack_into('<I', data, 4096*2, 3)
        struct.pack_into('<IIII', data, 4096*3, 2, 0, 28, 4)
        identifier = uuid.UUID('01234567-89ab-cdef-0123-456789abcdef')
        struct.pack_into('<III', data, 4096*4, 20000404, 0, 7)
        data[4096*4+12:4096*4+28] = identifier.bytes_le
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'fixture.pdb'
            path.write_bytes(data)
            self.assertEqual(pdb_identity(path), {'debug_id':list(identifier.bytes), 'debug_age':7})
            path.write_bytes(data[:100])
            with self.assertRaises(ValueError):
                pdb_identity(path)

    @unittest.skipUnless(os.name == 'nt' and os.environ.get('CARGO_TARGET_DIR'), 'Windows native build artifact')
    def test_built_collector_pdb_matches_pe_guid_and_age(self):
        root = Path(os.environ['CARGO_TARGET_DIR']) / 'debug'
        pe = image_identity(root / 'oink-crash-collector.exe')
        pdb = pdb_identity(root / 'oink_crash_collector.pdb')
        self.assertEqual(pdb, {key: pe[key] for key in ('debug_id', 'debug_age')})

    def test_macho_age_is_zero_and_code_id_is_uuid_derived(self):
        data = bytearray(128)
        data[:4] = b'\xcf\xfa\xed\xfe'
        struct.pack_into('<iiIIIII', data, 4, 0x100000c, 0, 2, 2, 96, 0, 0)
        struct.pack_into('<II', data, 32, 0x1b, 24)
        data[40:56] = bytes(range(16))
        struct.pack_into('<II16sQQQ', data, 56, 0x19, 72, b'__TEXT', 0x100000000, 4096, 0)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'fixture'
            path.write_bytes(data)
            self.assertEqual(image_identity(path), {'debug_id':list(range(16)), 'debug_age':0, 'code_id':None, 'text_size':4096})


if __name__ == '__main__':
    unittest.main()
