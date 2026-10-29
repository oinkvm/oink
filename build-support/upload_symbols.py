#!/usr/bin/env python3
"""Upload only matching native debug files, never dumps, source bundles or logs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import urllib.request

VERSION = '2.58.0'
ASSETS = {
    'Darwin': ('sentry-cli-Darwin-universal', 'b23b628f2d000898cbc8b323339218134e0856b0dfa2491c0328c09a27b02e69'),
    'Windows': ('sentry-cli-Windows-x86_64.exe', '3f7e7d8ab53052fb6d41f3428df405b5cc521474d26d4bfd3fb6b05cd2e708c9'),
}


def symbol_files(root):
    root = Path(root)
    manifest = json.loads((root / 'manifest.json').read_text())
    result = []
    for image in manifest['images']:
        relative = image.get('pdb') or image['dwarf_file']
        candidate = Path(relative)
        if image.get('pdb'):
            if candidate.suffix.lower() != '.pdb':
                raise ValueError('Only PDBs may use a PDB manifest entry')
        elif (candidate.parent.parts[-3:] != ('Contents', 'Resources', 'DWARF')
              or candidate.parent.parent.parent.parent.suffix != '.dSYM'):
            raise ValueError('Only dSYM DWARF files may use a native debug entry')
        path = (root / relative).resolve()
        if not path.is_relative_to(root.resolve()) or not path.is_file():
            raise ValueError('Symbol manifest references a missing or external file')
        expected = image.get('pdb_sha256') or image['dwarf_sha256']
        if hashlib.sha256(path.read_bytes()).hexdigest() != expected:
            raise ValueError('Symbol manifest hash mismatch')
        result.append(path)
    if not result:
        raise ValueError('No native symbol files')
    return result


def upload(root, cache, environment=None):
    environment = dict(os.environ if environment is None else environment)
    if not environment.get('SENTRY_AUTH_TOKEN'):
        print('Symbols: SENTRY_AUTH_TOKEN unset; skipping Sentry upload (local archive retained).')
        return
    # Use the very same project as the compiled EU DSN, not an independent slug.
    match = re.fullmatch(r'https://[a-fA-F0-9]{32}@o[0-9]+\.ingest\.de\.sentry\.io/([0-9]+)', environment.get('OINK_REPORT_DSN', ''))
    if not match:
        raise ValueError('Symbol upload requires the existing EU OINK_REPORT_DSN')
    files = symbol_files(root)
    asset, digest = ASSETS[platform.system()]
    tool = Path(cache) / ('sentry-cli-' + VERSION) / asset
    if not tool.exists():
        tool.parent.mkdir(parents=True, exist_ok=True)
        # Standalone pinned build tool in the package cache; no global install.
        with urllib.request.urlopen(f'https://github.com/getsentry/sentry-cli/releases/download/{VERSION}/{asset}', timeout=60) as response:
            data = response.read(64 * 1024 * 1024 + 1)
        if hashlib.sha256(data).hexdigest() != digest:
            raise ValueError('Pinned sentry-cli checksum mismatch')
        tool.write_bytes(data)
        tool.chmod(0o700)
    if hashlib.sha256(tool.read_bytes()).hexdigest() != digest:
        raise ValueError('Cached sentry-cli checksum mismatch')
    # Disable ambient CLI configuration; credentials stay in the environment,
    # never command arguments, generated records or the shipped product.
    environment.update(SENTRY_URL='https://de.sentry.io/', SENTRY_ORG='hachiwii',
                       SENTRY_PROJECT=match[1], SENTRY_DISABLE_UPDATE_CHECK='1',
                       SENTRY_LOAD_DOTENV='0', SENTRY_LOG_LEVEL='warn')
    command = [str(tool), '--url', 'https://de.sentry.io/', 'debug-files', 'upload',
               '--org', 'hachiwii', '--project', match[1], '--no-sources']
    # Explicit manifest-listed PDB/DWARF files only: never recurse a package or
    # symbols tree (which also carries JS maps). No --include-sources.
    subprocess.run([*command, *map(str, files)], check=True, env=environment)
    print(f'Symbols: uploaded {len(files)} native debug files to the configured EU project.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--packages', type=Path, required=True)
    parser.add_argument('--tool-cache', type=Path, required=True)
    args = parser.parse_args()
    roots = sorted(args.packages.rglob('symbols.noindex'))
    if not os.environ.get('SENTRY_AUTH_TOKEN'):
        upload(args.packages, args.tool_cache)
        return
    if not roots:
        raise ValueError('No symbol archives found')
    for root in roots:
        upload(root, args.tool_cache)


if __name__ == '__main__':
    main()
