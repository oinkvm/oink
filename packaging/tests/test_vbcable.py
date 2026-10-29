"""Offline build acquisition checks; never run a vendor installer."""
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('vbcable', Path(__file__).resolve().parents[1] / 'windows/vbcable.py')
V = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(V)


class VBCableBundle(unittest.TestCase):
    def test_verified_cache_is_copied_unchanged_without_network(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            cache = root / 'cache'; cache.mkdir()
            payload = root / 'payload'; payload.mkdir()
            data = b'vendor archive fixture'
            (cache / 'VBCABLE_Driver_Pack45.zip').write_bytes(data)
            with patch.object(V, 'SHA256', hashlib.sha256(data).hexdigest()), patch.object(V.urllib.request, 'urlretrieve') as fetch:
                files = V.bundle(cache, payload)
            fetch.assert_not_called()
            self.assertEqual((payload / files[0]).read_bytes(), data)
            self.assertEqual(files, ['AudioPlugin/VBCABLE_Driver_Pack45.zip'])
            self.assertFalse((payload / 'AudioPlugin/Microphone.ps1').exists())

    def test_custom_action_embeds_the_exact_utf8_script_without_command_line_encoding(self):
        import re
        script = "# fixture café\n" + 'x' * 40000
        header = V.cleanup_header(script)
        raw = bytes(int(value, 16) for value in re.findall(r'\\x([0-9a-f]{2})', header))
        self.assertEqual(raw, ("$Action='uninstall';$Cleanup=$true;\n" + script).encode('utf-8'))

    def test_bad_download_is_not_cached_or_packaged(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            def download(url, path):
                self.assertEqual(url, V.URL)
                path.write_bytes(b'tampered')
            with patch.object(V.urllib.request, 'urlretrieve', side_effect=download):
                with self.assertRaisesRegex(RuntimeError, 'hash mismatch'):
                    V.bundle(root / 'cache', root / 'payload')
            self.assertEqual(list((root / 'cache').iterdir()), [])
            self.assertFalse((root / 'payload').exists())

    def test_bad_cache_is_not_silently_downloaded_again(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / 'VBCABLE_Driver_Pack45.zip').write_bytes(b'tampered')
            with patch.object(V.urllib.request, 'urlretrieve') as fetch:
                with self.assertRaisesRegex(RuntimeError, 'hash mismatch'):
                    V.bundle(root, root / 'payload')
                fetch.assert_not_called()

    def test_cleanup_cannot_run_during_install_upgrade_rollback_or_ui(self):
        import copy
        tables = {'CustomAction': [['CleanupMicrophone', '3649', 'InstallerCA', 'CleanupMicrophone']],
                  'InstallExecuteSequence': [['CheckQuiescence', '', '2000'],
                                             ['CleanupMicrophone', 'REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE', '6500'],
                                             ['CleanupUserData', '', '6501'], ['InstallFinalize', '', '6600']]}
        V.verify_cleanup(tables)
        for kind in ('deferred', 'install', 'upgrade', 'early', 'ui', 'missing'):
            changed = copy.deepcopy(tables)
            if kind == 'deferred': changed['CustomAction'][0][1] = '3137'
            if kind == 'install': changed['InstallExecuteSequence'][1][1] = ''
            if kind == 'upgrade': changed['InstallExecuteSequence'][1][1] = 'REMOVE~="ALL"'
            if kind == 'early': changed['InstallExecuteSequence'][1][2] = '1000'
            if kind == 'ui': changed['InstallUISequence'] = [['CleanupMicrophone', '', '6500']]
            if kind == 'missing': changed['CustomAction'] = []
            with self.subTest(kind=kind), self.assertRaisesRegex(RuntimeError, 'commit-only'):
                V.verify_cleanup(changed)

    def test_extracted_msi_rejects_a_standalone_script_and_changed_vendor_archive(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / 'File').mkdir()
            manifest = root / 'manifest.xml'
            manifest.write_text('<Wix><File Id="archive" Name="VBCABLE_Driver_Pack45.zip"/></Wix>')
            data = b'archive'
            (root / 'File/archive').write_bytes(data)
            with patch.object(V, 'SHA256', hashlib.sha256(data).hexdigest()):
                V.verify_extracted(manifest, root)
                manifest.write_text('<Wix><File Id="archive" Name="VBCABLE_Driver_Pack45.zip"/><File Id="script" Name="Microphone.ps1"/></Wix>')
                with self.assertRaisesRegex(RuntimeError, 'must not install'):
                    V.verify_extracted(manifest, root)
                (root / 'File/archive').write_bytes(b'modified')
                with self.assertRaisesRegex(RuntimeError, 'archive hash mismatch'):
                    V.verify_extracted(manifest, root)
