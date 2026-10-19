import base64
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import xml.etree.ElementTree as ET

import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support/update'))
spec = importlib.util.spec_from_file_location('mac_appcast', Path(__file__).resolve().parents[2] / 'build-support/update/mac-appcast.py')
mac = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mac)

NOTES = {'zh-Hans': '修复问题。\n保留 & <纯文本>。', 'en': 'Fix a problem.\nKeep & <plain text>.'}
XML = f'''<rss version="2.0" xmlns:sparkle="{mac.SPARKLE}" xmlns:oink="{mac.OINK}" xmlns:other="https://example.test/schema">
<channel><item><sparkle:version>1</sparkle:version><other:keep>unchanged</other:keep>
<enclosure url="https://example.test/old.dmg" sparkle:edSignature="OLD" length="1"/>
<description>old notes</description><oink:releaseNotes>{{"zh-Hans":"旧版说明","en":"Old notes"}}</oink:releaseNotes></item>
<item><sparkle:version>2</sparkle:version><sparkle:shortVersionString>1.0.1</sparkle:shortVersionString>
<enclosure url="https://example.test/new.dmg" sparkle:edSignature="NEW" length="2"/>
<description>replace this</description><sparkle:releaseNotesLink>https://example.test/old.html</sparkle:releaseNotesLink>
</item></channel></rss>'''.encode()


class MacAppcastTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'appcast.xml'
        self.path.write_bytes(XML)

    def test_both_plain_text_descriptions_and_custom_dictionary_preserve_other_items(self):
        before = ET.fromstring(XML).findall('./channel/item')
        mac.embed_notes(self.path, 2, NOTES)
        items = ET.fromstring(self.path.read_bytes()).findall('./channel/item')
        self.assertEqual(ET.tostring(items[0]), ET.tostring(before[0]))
        self.assertEqual(items[1].find('enclosure').attrib, before[1].find('enclosure').attrib)
        self.assertIsNone(items[1].find(f'{{{mac.SPARKLE}}}releaseNotesLink'))
        self.assertEqual(json.loads(items[1].findtext(f'{{{mac.OINK}}}releaseNotes')), NOTES)
        descriptions = {node.get(f'{{{mac.XML}}}lang'): node.text for node in items[1].findall('description')}
        self.assertEqual(descriptions, NOTES)
        self.assertTrue(all(node.get(f'{{{mac.SPARKLE}}}format') == 'plain-text' for node in items[1].findall('description')))
        self.assertIn(b'<oink:releaseNotes>', self.path.read_bytes())
        # Re-running the candidate edit does not duplicate translation elements.
        once = self.path.read_bytes()
        mac.embed_notes(self.path, 2, NOTES)
        self.assertEqual(self.path.read_bytes(), once)

    def test_missing_translation_unknown_build_and_ambiguous_build_rejected_before_write(self):
        for build, notes in [(2, {'en': 'missing Chinese'}), (3, NOTES)]:
            with self.assertRaises(ValueError): mac.embed_notes(self.path, build, notes)
            self.assertEqual(self.path.read_bytes(), XML)
        duplicate = XML.replace(b'<sparkle:version>1', b'<sparkle:version>2')
        self.path.write_bytes(duplicate)
        with self.assertRaises(ValueError): mac.embed_notes(self.path, 2, NOTES)
        self.assertEqual(self.path.read_bytes(), duplicate)
        legacy = XML.replace('<oink:releaseNotes>{"zh-Hans":"旧版说明","en":"Old notes"}</oink:releaseNotes>'.encode(), b'')
        self.path.write_bytes(legacy)
        with self.assertRaises(ValueError): mac.embed_notes(self.path, 2, NOTES)
        self.assertEqual(self.path.read_bytes(), legacy)

    @unittest.skipUnless(os.environ.get('SPARKLE_TEST_SIGN_UPDATE'), 'pinned native Sparkle signer is optional on this platform')
    def test_official_signature_rejects_tampering_either_translation(self):
        signer = os.environ['SPARKLE_TEST_SIGN_UPDATE']
        # Public RFC 8032 TEST seed only. Never read a Keychain or generate keys.
        seed = bytes.fromhex('9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60')
        key = self.path.with_suffix('.test-key')
        with key.open('x') as stream:
            os.chmod(key, 0o600)
            stream.write(base64.b64encode(seed).decode())
        mac.embed_notes(self.path, 2, NOTES)
        command = [signer, '--ed-key-file', str(key)]
        subprocess.run([*command, str(self.path)], check=True, capture_output=True)
        signed = self.path.read_bytes()
        subprocess.run([*command, '--verify', str(self.path)], check=True, capture_output=True)
        for original, changed in [('修复问题', '篡改说明'), ('Fix a problem', 'Bad a problem')]:
            self.path.write_bytes(signed.replace(original.encode(), changed.encode()))
            result = subprocess.run([*command, '--verify', str(self.path)], capture_output=True)
            self.assertNotEqual(result.returncode, 0)


if __name__ == '__main__': unittest.main()
