#!/usr/bin/env python3
"""Restore only the existing CI keystore, verify its public certificate, then remove it."""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

CERTIFICATE_SHA256 = '2c57e1b11da7080c5632f5a3da515ab5fac4e331d96be53d17e203040ffe9fd8'


def signing_directory():
    root = Path(os.environ['RUNNER_TEMP'])
    if not root.is_absolute():
        raise ValueError('runner temporary directory must be absolute')
    return root / 'android-signing'


def prepare():
    if os.environ.get('GITHUB_REPOSITORY') != 'oinkvm/oink' or os.environ.get('GITHUB_REF') != 'refs/heads/0.4':
        raise ValueError('CI keystore setup is restricted to the public 0.4 branch')
    names=['OINK_ANDROID_KEYSTORE_BASE64','OINK_ANDROID_KEYSTORE_PASSWORD','OINK_ANDROID_KEY_ALIAS','OINK_ANDROID_KEY_PASSWORD']
    if not all(os.environ.get(n) for n in names):
        raise ValueError('all four existing Android signing credentials are required; no signing fallback')
    directory=signing_directory();directory.mkdir(mode=0o700)
    path=directory/'signing.keystore'
    path.write_bytes(base64.b64decode(os.environ['OINK_ANDROID_KEYSTORE_BASE64'],validate=True));path.chmod(0o600)
    with open(os.environ['GITHUB_ENV'],'a') as f:f.write(f'OINK_ANDROID_KEYSTORE_PATH={path}\n')


def verify(apk, apksigner, output):
    result=subprocess.run([str(apksigner),'verify','--print-certs',str(apk)],capture_output=True,text=True)
    if result.returncode:
        raise ValueError('APK signature verification failed')
    # SDK 37 reports per-scheme `V2 Signer:`, older releases use `Signer #1`.
    # Multiple schemes may repeat one leaf certificate; any different signer fails.
    found=re.findall(r'^(?:Signer #\d+ certificate|V\d+(?:\.\d+)? Signer: certificate) SHA-256 digest:\s*([0-9a-fA-F]{64})',result.stdout,re.MULTILINE)
    if not found or {v.lower() for v in found}!={CERTIFICATE_SHA256}:
        raise ValueError('APK signer differs from the existing in-house certificate')
    output.write_text(json.dumps(dict(verified=True,certificate_sha256=CERTIFICATE_SHA256),indent=2)+'\n')
    print('APK signature verified against the existing in-house certificate')


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('operation',choices=['prepare','verify','cleanup'])
    p.add_argument('--apk',type=Path);p.add_argument('--apksigner',type=Path);p.add_argument('--output',type=Path)
    a=p.parse_args()
    if a.operation=='prepare':prepare()
    elif a.operation=='cleanup':shutil.rmtree(signing_directory(),ignore_errors=True)
    else:verify(a.apk,a.apksigner,a.output)


if __name__=='__main__':main()
