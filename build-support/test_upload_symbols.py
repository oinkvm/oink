import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import upload_symbols


class UploadTests(unittest.TestCase):
    def test_no_token_never_downloads_or_uploads(self):
        with patch('upload_symbols.urllib.request.urlopen') as download, patch('upload_symbols.subprocess.run') as run:
            upload_symbols.upload('/does-not-exist', '/does-not-exist', {})
            download.assert_not_called()
            run.assert_not_called()

    def test_only_verified_manifest_symbols_go_to_dsn_project(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            symbols = root / 'symbols.noindex'
            symbols.mkdir()
            pdb = symbols / 'shipped.pdb'
            pdb.write_bytes(b'debug identity fixture')
            (symbols / 'crash.dmp').write_bytes(b'must never upload')
            (symbols / 'source.zip').write_bytes(b'must never upload')
            (symbols / 'manifest.json').write_text(json.dumps({'images': [{'pdb': 'shipped.pdb', 'pdb_sha256': hashlib.sha256(pdb.read_bytes()).hexdigest()}]}))
            tool = root / ('sentry-cli-' + upload_symbols.VERSION) / 'fixture-cli'
            tool.parent.mkdir()
            tool.write_bytes(b'fixture tool')
            assets = {'Darwin': (tool.name, hashlib.sha256(tool.read_bytes()).hexdigest())}
            env = {'SENTRY_AUTH_TOKEN': 'fixture-token', 'OINK_REPORT_DSN': 'https://' + 'a'*32 + '@o1.ingest.de.sentry.io/42'}
            with patch.object(upload_symbols, 'ASSETS', assets), patch('upload_symbols.platform.system', return_value='Darwin'), patch('upload_symbols.subprocess.run') as run:
                upload_symbols.upload(symbols, root, env)
            args, kwargs = run.call_args
            command = args[0]
            self.assertEqual(command[-1], str(pdb.resolve()))
            self.assertIn('--no-sources', command)
            self.assertIn('https://de.sentry.io/', command)
            self.assertEqual(command[command.index('--project')+1], '42')
            self.assertNotIn('fixture-token', ' '.join(command))
            self.assertEqual(kwargs['env']['SENTRY_ORG'], 'hachiwii')
            pdb.write_bytes(b'wrong binary symbols')
            with self.assertRaises(ValueError):
                upload_symbols.symbol_files(symbols)

    def test_even_a_manifest_entry_cannot_upload_a_dump(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/'crash.dmp').write_bytes(b'raw memory')
            (root/'manifest.json').write_text(json.dumps({'images': [{'pdb': 'crash.dmp','pdb_sha256': hashlib.sha256(b'raw memory').hexdigest()}]}))
            with self.assertRaises(ValueError):
                upload_symbols.symbol_files(root)

    def test_manifest_cannot_upload_outside_symbol_archive(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/'manifest.json').write_text(json.dumps({'images': [{'pdb': '../crash.dmp','pdb_sha256': '0'*64}]}))
            with self.assertRaises(ValueError):
                upload_symbols.symbol_files(root)


if __name__ == '__main__':
    unittest.main()
