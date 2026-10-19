#!/usr/bin/env python3
"""Ephemeral signing keychain on GitHub-hosted runners only (never local setup)."""
import base64
import json
import os
from pathlib import Path
import secrets
import shlex
import shutil
import subprocess
import sys


def run(*args, capture=False):
    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode:
        # Arguments and command output may contain credentials. Never print them.
        raise RuntimeError(f'signing setup command failed with code {result.returncode}')
    return result.stdout if capture else None


def main():
    if os.environ.get('GITHUB_ACTIONS') != 'true' or os.environ.get('RUNNER_OS') != 'macOS':
        raise SystemExit('This helper is restricted to macOS GitHub Actions runners.')
    directory = Path(os.environ['RUNNER_TEMP']) / 'release-signing'
    keychain = directory / 'signing.keychain-db'
    if sys.argv[1:] == ['cleanup']:
        if (directory / 'original.json').exists():
            run('security', 'list-keychains', '-d', 'user', '-s', *json.loads((directory / 'original.json').read_text()))
        if keychain.exists():
            run('security', 'delete-keychain', str(keychain))
        if directory.exists():
            shutil.rmtree(directory)
        return
    with open(os.environ['GITHUB_OUTPUT'], 'a') as out:
        required = ['MACOS_CERTIFICATE_P12', 'MACOS_CERTIFICATE_PASSWORD', 'MACOS_SIGNING_FINGERPRINT',
                    'APPLE_NOTARY_KEY_P8', 'APPLE_NOTARY_KEY_ID', 'APPLE_NOTARY_ISSUER']
        if not all(os.environ.get(name) for name in required):
            raise RuntimeError('Developer ID and notarization credentials are required for release packages')
        from signing import select_identity
        team = json.loads((Path(__file__).parents[1] / 'identity.json').read_text())['macos']['team_id']
        directory.mkdir(mode=0o700)
        original = shlex.split(run('security', 'list-keychains', '-d', 'user', capture=True))
        (directory / 'original.json').write_text(json.dumps(original))
        password = secrets.token_urlsafe(32)
        run('security', 'create-keychain', '-p', password, str(keychain))
        run('security', 'set-keychain-settings', '-lut', '21600', str(keychain))
        run('security', 'unlock-keychain', '-p', password, str(keychain))
        certificate = directory / 'certificate.p12'
        certificate.write_bytes(base64.b64decode(os.environ['MACOS_CERTIFICATE_P12'], validate=True))
        certificate.chmod(0o600)
        run('security', 'import', str(certificate), '-k', str(keychain), '-P', os.environ['MACOS_CERTIFICATE_PASSWORD'], '-T', '/usr/bin/codesign')
        certificate.unlink()
        run('security', 'set-key-partition-list', '-S', 'apple-tool:,apple:,codesign:', '-s', '-k', password, str(keychain))
        run('security', 'list-keychains', '-d', 'user', '-s', str(keychain), *original)
        select_identity(team, os.environ['MACOS_SIGNING_FINGERPRINT'])
        out.write('signed=true\n')
        key = directory / 'notary.p8'
        key.write_text(os.environ['APPLE_NOTARY_KEY_P8'])
        key.chmod(0o600)
        run('xcrun', 'notarytool', 'store-credentials', 'release-notary', '--key', str(key),
            '--key-id', os.environ['APPLE_NOTARY_KEY_ID'], '--issuer', os.environ['APPLE_NOTARY_ISSUER'], '--keychain', str(keychain))
        key.unlink()
        out.write('notarize=true\n')
        (directory / 'notary-ready').touch()


if __name__ == '__main__':
    main()
