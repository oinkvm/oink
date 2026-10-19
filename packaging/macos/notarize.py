#!/usr/bin/env python3
"""Optionally notarize an existing signed package using an existing profile.

No credentials are created. Updated archive/image hashes replace the detached
release record only after signing and ticket checks finish.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
from dmg import create
from signing import notarize, private_run, select_identity, verify_signature, staple_and_validate
from provenance import ROOT, source_tree_sha256


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--directory', type=Path, required=True)
    p.add_argument('--profile', default=os.environ.get('NOTARY_PROFILE'), required=not bool(os.environ.get('NOTARY_PROFILE')))
    p.add_argument('--keychain', type=Path)
    p.add_argument('--timeout', type=int, default=300)
    p.add_argument('--tool-cache', type=Path, default=Path.home() / '.cache/oink-build')
    args = p.parse_args()
    directory = args.directory.resolve()
    record = json.loads((directory / 'release.json').read_text())
    if source_tree_sha256(ROOT) != record['source_tree_sha256']:
        p.error('run from the source tree recorded in the signed package')
    if record['signing'] != 'developer-id' or record['team_id'] != '893V37X2CW':
        p.error('expected the signed Oink Developer ID package')
    for artifact in record['artifacts']:
        path = directory / artifact['name']
        if path.parent != directory or hashlib.sha256(path.read_bytes()).hexdigest() != artifact['sha256']:
            p.error('package artifact changed')
    identity = select_identity(record['team_id'], record['certificate_sha1'])
    archive = directory / next(a['name'] for a in record['artifacts'] if a['name'].endswith('.zip'))
    image = directory / next(a['name'] for a in record['artifacts'] if a['name'].endswith('.dmg'))
    with tempfile.TemporaryDirectory(prefix='oink-notarize-', suffix='.noindex') as temporary:
        root = Path(temporary)
        subprocess.run(['ditto', '-x', '-k', str(archive), str(root)], check=True)
        app = root / 'Oink.app'
        verify_signature(app, record['team_id'])
        app_result = notarize(archive, args.profile, args.keychain, args.timeout)
        if app_result['status'] == 'Accepted':
            staple_and_validate(app)
            app_result['stapled'] = True
            subprocess.run(['ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', str(app), str(root / archive.name)], check=True)
            create(app, root / image.name, args.tool_cache)
            private_run('codesign', '--force', '--timestamp', '--sign', identity, root / image.name)
            (root / image.name).replace(image)
            (root / archive.name).replace(archive)
        image_result = notarize(image, args.profile, args.keychain, args.timeout)
    record['notarization'] = {'app': app_result, 'dmg': image_result}
    record['notarized'] = app_result.get('stapled', False) and image_result.get('stapled', False)
    for artifact in record['artifacts']:
        path = directory / artifact['name']
        artifact.update(size=path.stat().st_size, sha256=hashlib.sha256(path.read_bytes()).hexdigest())
    (directory / 'release.json').write_text(json.dumps(record, indent=2) + '\n')
    (directory / 'SHA256SUMS').write_text(''.join(f"{a['sha256']}  {a['name']}\n" for a in record['artifacts']))


if __name__ == '__main__':
    main()
