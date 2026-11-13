"""CI signing uses the existing key and fails closed, without real credentials."""
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[2]/'build-support'))
import android_signing as signing

class AndroidSigning(unittest.TestCase):
    def test_restore_is_private_and_does_not_persist_passwords(self):
        with tempfile.TemporaryDirectory() as tmp:
            env=dict(GITHUB_REPOSITORY='oinkvm/oink',GITHUB_REF='refs/heads/0.4',RUNNER_TEMP=tmp,GITHUB_ENV=str(Path(tmp)/'env'),OINK_ANDROID_KEYSTORE_BASE64=base64.b64encode(b'fixture').decode(),OINK_ANDROID_KEYSTORE_PASSWORD='test-only-store-password',OINK_ANDROID_KEY_ALIAS='fixture',OINK_ANDROID_KEY_PASSWORD='test-only-key-password')
            with patch.dict(os.environ,env):
                signing.prepare();path=Path(tmp)/'android-signing/signing.keystore'
                self.assertEqual(path.read_bytes(),b'fixture');self.assertEqual(path.stat().st_mode&0o777,0o600)
                exported=(Path(tmp)/'env').read_text();self.assertNotIn('password',exported)
                self.assertIn(str(path),exported)
                signing.shutil.rmtree(signing.signing_directory())
                self.assertFalse(path.exists())

    def test_missing_credentials_never_fall_back(self):
        with patch.dict(os.environ,dict(GITHUB_REPOSITORY='oinkvm/oink',GITHUB_REF='refs/heads/0.4'),clear=True):
            with self.assertRaisesRegex(ValueError,'no signing fallback'):signing.prepare()

    def test_verification_rejects_another_certificate_even_if_signature_is_valid(self):
        with tempfile.TemporaryDirectory() as tmp:
            out=Path(tmp)/'proof.json'
            for certificate,code in [(signing.CERTIFICATE_SHA256,0),('a'*64,0),(signing.CERTIFICATE_SHA256,1)]:
                result=subprocess.CompletedProcess([],code,f'Signer #1 certificate SHA-256 digest: {certificate}\n','')
                with patch.object(signing.subprocess,'run',return_value=result):
                    if certificate==signing.CERTIFICATE_SHA256 and code==0:
                        signing.verify(Path('fixture.apk'),Path('apksigner'),out)
                        self.assertEqual(json.loads(out.read_text())['certificate_sha256'],certificate)
                    else:
                        with self.assertRaises(ValueError):signing.verify(Path('fixture.apk'),Path('apksigner'),out)

if __name__=='__main__':unittest.main()
