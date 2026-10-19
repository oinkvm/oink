import importlib.util
from pathlib import Path
import tempfile
import plistlib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('dmg', ROOT / 'packaging/macos/dmg.py')
D = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(D)


class VolumeIcon(unittest.TestCase):
    def test_read_only_icon_audit_detaches_even_on_failure_without_launching(self):
        attach = plistlib.dumps({'system-entities': [{'dev-entry': '/dev/disk-test', 'mount-point': '/mount'}]})
        with patch.object(D.subprocess, 'check_output', return_value=attach), \
                patch.object(D.subprocess, 'run') as run, \
                patch.object(D, 'verify_volume_icon', side_effect=RuntimeError('bad icon')):
            with self.assertRaisesRegex(RuntimeError, 'bad icon'):
                D.verify_image_icon(Path('test.dmg'), Path('icon.icns'))
            self.assertEqual(run.call_count, 1)
            self.assertEqual(run.call_args.args[0], ['hdiutil', 'detach', '/dev/disk-test'])

    def test_exact_app_icon_and_custom_volume_flag_are_both_required(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            icon = root / 'icon.icns'
            icon.write_bytes(b'icns-app-artwork')
            (root / '.VolumeIcon.icns').write_bytes(icon.read_bytes())
            flags = bytearray(32)
            flags[8:10] = (0x0400).to_bytes(2, 'big')
            with patch.object(D.subprocess, 'check_output', return_value=bytes(flags).hex()):
                D.verify_volume_icon(root, icon)
                (root / '.VolumeIcon.icns').write_bytes(b'other-artwork')
                with self.assertRaisesRegex(RuntimeError, 'differs'):
                    D.verify_volume_icon(root, icon)
            (root / '.VolumeIcon.icns').write_bytes(icon.read_bytes())
            for attributes in [bytes(32), bytes(10)]:
                with patch.object(D.subprocess, 'check_output', return_value=attributes.hex()):
                    with self.assertRaisesRegex(RuntimeError, 'Finder flag'):
                        D.verify_volume_icon(root, icon)
