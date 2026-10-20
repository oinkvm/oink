"""Provisioning tests use only public RFC8032 fixtures and mocked Keychain/GitHub."""
import base64
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support/update'))
import feed_site
spec = importlib.util.spec_from_file_location('setup_macos_key', ROOT / 'build-support/update/setup-macos-key.py')
key = importlib.util.module_from_spec(spec)
spec.loader.exec_module(key)
PUBLIC = base64.b64encode(bytes.fromhex('d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a')).decode()
SEED = base64.b64encode(bytes.fromhex('9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60'))


class SetupTests(unittest.TestCase):
    def test_local_data_initialization_has_no_enabled_feeds_and_never_overwrites(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'state'
            feed_site.initialize(output)
            self.assertEqual(feed_site.state_info(output)['revision'], 0)
            self.assertEqual(set(feed_site.files(output / 'site')), {'CNAME', 'index.html'})
            feed_site.verify_all(output)
            with self.assertRaises(FileExistsError): feed_site.initialize(output)

    def test_secret_only_uses_stdin_and_public_write_follows_success(self):
        for succeeds in (False, True):
            with self.subTest(succeeds=succeeds), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp); (root / 'packaging').mkdir()
                config = root / 'packaging/update-trust.json'
                fixture = json.loads((ROOT / 'packaging/update-trust.json').read_bytes())
                fixture['mac'].update(public_key=None, enabled=False)
                original = json.dumps(fixture).encode()
                config.write_bytes(original)
                calls = []
                def run(command, **kwargs):
                    calls.append((command, kwargs))
                    self.assertNotIn(SEED.decode(), str(command))
                    self.assertTrue(kwargs['capture_output'])
                    if command[0] == 'security': return subprocess.CompletedProcess(command, 0, SEED, b'')
                    if command[-1] == '-p': return subprocess.CompletedProcess(command, 0, PUBLIC.encode(), b'')
                    if command[:3] == ['gh', 'secret', 'set']:
                        self.assertEqual(kwargs['input'], SEED)
                        self.assertEqual(config.read_bytes(), original)
                        return subprocess.CompletedProcess(command, 0 if succeeds else 1, b'', SEED)
                    return subprocess.CompletedProcess(command, 0, b'', b'')
                with patch.object(key, 'ROOT', root), patch.object(key.sys, 'platform', 'darwin'), \
                     patch.dict(key.os.environ, {}, clear=True), patch.object(key, 'prepare', return_value=root), \
                     patch.object(key.subprocess, 'run', side_effect=run):
                    if succeeds: key.setup(root / 'tools')
                    else:
                        with self.assertRaisesRegex(RuntimeError, '^gh failed;'): key.setup(root / 'tools')
                result = json.loads(config.read_bytes())
                self.assertEqual(result['mac']['public_key'], PUBLIC if succeeds else None)
                self.assertFalse(result['mac']['enabled'])
                self.assertNotIn(SEED.decode(), config.read_text())
                self.assertFalse(any(' -x ' in ' '.join(c) for c, _ in calls))

    def test_ci_cannot_provision_a_production_key(self):
        with patch.dict(key.os.environ, {'CI': 'true'}), patch.object(key.subprocess, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'outside CI'): key.setup(Path('unused'))
            run.assert_not_called()


if __name__ == '__main__': unittest.main()
