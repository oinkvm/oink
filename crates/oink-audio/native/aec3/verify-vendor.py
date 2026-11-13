#!/usr/bin/env python3
"""Verify the exact checked-in AEC closure. No downloads, tools or builds.

Optional --webrtc /pinned/checkout --abseil /20240722.0 compares each file to
its source, permitting only the committed realtime-storage.patch differences.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parent
manifest = json.loads((root / 'vendor-manifest.json').read_text())
parser = argparse.ArgumentParser(__doc__)
parser.add_argument('--webrtc', type=Path)
parser.add_argument('--abseil', type=Path)
args = parser.parse_args()
actual = {str(p.relative_to(root)) for p in (root / 'vendor').rglob('*') if p.is_file()}
assert actual == set(manifest['files']), 'vendored closure contains extra/missing files'
for path, digest in manifest['files'].items():
    assert hashlib.sha256((root / path).read_bytes()).hexdigest() == digest, path
if args.webrtc or args.abseil:
    assert args.webrtc and args.abseil, 'both source directories required'
    commit = subprocess.check_output(['git', '-C', str(args.webrtc), 'rev-parse', 'HEAD'], text=True).strip()
    assert commit == manifest['webrtc']['commit'], commit
    changes = {'modules/audio_processing/aec3/comfort_noise_generator.cc',
               'modules/audio_processing/aec3/comfort_noise_generator.h'}
    with tempfile.TemporaryDirectory() as directory:
        patched = Path(directory)
        for rel in changes:
            target = patched / 'webrtc' / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((args.webrtc / 'webrtc' / rel).read_bytes())
        subprocess.run(['patch', '-p1', '-i', str(root / 'realtime-storage.patch')], cwd=patched, check=True)
        for path in manifest['files']:
            relative = Path(path).relative_to('vendor')
            group, *parts = relative.parts
            rel = Path(*parts)
            if group == 'abseil':
                source = args.abseil / rel
            elif str(rel) in changes:
                source = patched / 'webrtc' / rel
            elif str(rel) in {'COPYING', 'AUTHORS'}:
                source = args.webrtc / rel
            else:
                source = args.webrtc / 'webrtc' / rel
            assert source.read_bytes() == (root / path).read_bytes(), path
print(f"Verified {len(actual)} pinned source/header/notice files; no untracked closure entries")
