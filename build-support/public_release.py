#!/usr/bin/env python3
"""Stage verified installers, Sparkle archive and public release metadata."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
from identity import IDENTITY
from provenance import digest, source_tree_sha256
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent / 'update'))
import feed


def inputs(build, environment):
    if not re.fullmatch(r'[0-9]+', build) or not 28 <= int(build) < 2**31:
        raise ValueError('reserve a monotonically increasing build in 28..2147483647')
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', IDENTITY['version']):
        raise ValueError('public release requires a stable product version')
    # Mirrors Endpoint::parse; reject an inert release without exposing its DSN.
    if not re.fullmatch(r'https://[a-fA-F0-9]{32}@o[0-9]+\.ingest\.de\.sentry\.io/[0-9]+', environment.get('OINK_REPORT_DSN', '')):
        raise ValueError('OINK_REPORT_DSN must be the EU reporting DSN')
    if environment.get('OINK_REPORT_REGION') != 'eu':
        raise ValueError('OINK_REPORT_REGION must be eu')
    names = ['SIGNPATH_API_TOKEN', 'SIGNPATH_ORGANIZATION_ID', 'SIGNPATH_PROJECT_SLUG']
    supplied = [bool(environment.get(n)) for n in names]
    if any(supplied) and not all(supplied):
        raise ValueError('configure all SIGNPATH_API_TOKEN, SIGNPATH_ORGANIZATION_ID and SIGNPATH_PROJECT_SLUG, or none')
    return all(supplied)


def locate(packages, platform):
    directory = 'macos-arm64' if platform == 'macos' else 'windows-x64'
    records = list(Path(packages).glob('*/' + directory + '/release.json'))
    if len(records) != 1:
        raise ValueError('expected one platform package record')
    return records[0]


def stage(packages, output, platform, build, commit):
    path = locate(packages, platform)
    record = json.loads(path.read_text())
    if record.get('update_test_feed') or record.get('mac_update_test_feed'):
        raise ValueError('qualification test builds cannot become public release assets')
    if (record['version'], record['build'], record['source_tree_sha256']) != (IDENTITY['version'], build, source_tree_sha256()):
        raise ValueError('package version/build/source differs from checkout')
    if record.get('source_commit') is not None:
        raise ValueError('public packages must use --public-version')
    if not re.fullmatch('[0-9a-f]{40}', commit):
        raise ValueError('full built commit required')
    proof = json.loads((path.parent / 'report-configuration-check.json').read_text())
    if proof != {'configured': True, 'report_sent': False, 'version': record['version'], 'build': build}:
        raise ValueError('production reporting/version verification missing')
    if platform == 'macos':
        if not record.get('notarized') or not all(record['notarization'][k].get('stapled') and record['notarization'][k]['status'] == 'Accepted' for k in ['app', 'dmg']):
            raise ValueError('app and DMG must be Accepted and stapled')
        if record.get('launch_smoke', {}).get('status') != 'passed' or not record['launch_smoke'].get('signed') or record['launch_smoke'].get('seconds') != 10:
            raise ValueError('signed launch smoke required')
    output.mkdir(parents=True, exist_ok=True)
    for artifact in record['artifacts']:
        source = path.parent / artifact['name']
        if Path(artifact['name']).name != artifact['name'] or digest(source) != artifact['sha256'] or source.stat().st_size != artifact['size']:
            raise ValueError('package artifact hash/size mismatch')
        if source.suffix in ('.dmg', '.msi', '.zip'):
            shutil.copy2(source, output / source.name)
    record['artifacts'] = [a for a in record['artifacts'] if Path(a['name']).suffix in ('.dmg', '.msi', '.zip')]
    if sorted(Path(a['name']).suffix for a in record['artifacts']) != (['.dmg', '.zip'] if platform == 'macos' else ['.msi']):
        raise ValueError('expected Mac DMG + Sparkle ZIP, or Windows MSI')
    record.update(built_commit=commit, reporting=proof)
    (output / f'release-{platform}.json').write_text(json.dumps(record, indent=2) + '\n')


def assemble(downloads, output, build, commit):
    records = {}
    artifacts = []
    output.mkdir(parents=True, exist_ok=True)
    for platform, suffix in [('macos', '.dmg'), ('windows', '.msi')]:
        paths = list(downloads.rglob(f'release-{platform}.json'))
        if len(paths) != 1:
            raise ValueError('both successful platform records required')
        path = paths[0]; record = json.loads(path.read_text(encoding='utf-8-sig'))
        if (record['version'], record['build'], record['built_commit'], record['source_tree_sha256']) != (IDENTITY['version'], build, commit, source_tree_sha256()):
            raise ValueError('platform packages do not share the built source/version/build')
        if platform == 'macos' and not record.get('notarized'):
            raise ValueError('Mac notarization missing')
        if record['reporting'] != {'configured': True, 'report_sent': False, 'version': record['version'], 'build': build}:
            raise ValueError('reporting check missing')
        expected = ['.dmg', '.zip'] if platform == 'macos' else [suffix]
        if sorted(Path(a['name']).suffix for a in record['artifacts']) != expected:
            raise ValueError('Mac DMG + ZIP and Windows MSI required')
        for a in record['artifacts']:
            source = path.parent / a['name']
            if Path(a['name']).name != a['name'] or digest(source) != a['sha256'] or source.stat().st_size != a['size']:
                raise ValueError('final package hash/size mismatch')
            shutil.copy2(source, output / source.name)
            artifacts.append(a)
        records[platform] = record
    annotations = feed.read(Path('.github/update-notes.json'))
    if set(annotations) != {'channel', 'severity', 'notes'} or annotations['channel'] not in ('stable', 'beta') or annotations['severity'] not in ('normal', 'critical'):
        raise ValueError('invalid update annotations')
    feed.validate_notes(annotations['notes'])
    update = output / 'update.json'
    update.write_text(json.dumps(annotations, ensure_ascii=False, indent=2) + '\n')
    artifacts.append(dict(name=update.name, size=update.stat().st_size, sha256=digest(update)))
    release = dict(schema=1, product=IDENTITY['name'], version=IDENTITY['version'], build=build,
                   built_commit=commit, source_tree_sha256=source_tree_sha256(), platforms=records, artifacts=artifacts)
    record_path = output / 'release.json'
    record_path.write_text(json.dumps(release, indent=2) + '\n')
    (output / 'SHA256SUMS').write_text(''.join(f'{digest(p)}  {p.name}\n' for p in [*(output / a['name'] for a in artifacts), record_path]))
    return [output / a['name'] for a in artifacts] + [record_path, output / 'SHA256SUMS']


def publish(directory, commit, notes):
    tag = 'v' + IDENTITY['version']
    # --target only controls creation of a missing tag. Never attach these
    # installers to an already-existing tag that points at different sources.
    rows = subprocess.check_output(['git', 'ls-remote', '--tags', 'origin', f'refs/tags/{tag}', f'refs/tags/{tag}^{{}}'], text=True).splitlines()
    targets = dict(row.split() for row in rows)
    if rows:
        target = next((sha for sha, ref in targets.items() if ref.endswith('^{}')), rows[0].split()[0])
        if target != commit:
            raise ValueError('release tag already targets a different commit')
    record = json.loads((directory / 'release.json').read_text())
    if record['built_commit'] != commit:
        raise ValueError('release commit differs from built commit')
    files = [directory / a['name'] for a in record['artifacts']] + [directory / 'release.json', directory / 'SHA256SUMS']
    subprocess.run(['gh', 'release', 'create', tag, '--draft', '--target', commit, '--title', 'Oink ' + IDENTITY['version'], '--notes-file', str(notes), *map(str, files)], check=True)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('operation', choices=['inputs', 'locate', 'stage', 'assemble', 'publish'])
    p.add_argument('--build', default=os.environ.get('BUILD', '31'))
    p.add_argument('--commit', default=os.environ.get('GITHUB_SHA'))
    p.add_argument('--packages', type=Path)
    p.add_argument('--platform', choices=['macos', 'windows'])
    p.add_argument('--output', type=Path)
    p.add_argument('--downloads', type=Path)
    p.add_argument('--notes', type=Path, default=Path('.github/release-notes.txt'))
    a = p.parse_args()
    if a.operation == 'inputs':
        enabled = inputs(a.build, os.environ)
        with open(os.environ['GITHUB_OUTPUT'], 'a') as f:
            f.write('sign_windows=' + str(enabled).lower() + '\n')
    elif a.operation == 'locate':
        print(locate(a.packages, a.platform).parent)
    elif a.operation == 'stage':
        stage(a.packages, a.output, a.platform, int(a.build), a.commit)
    elif a.operation == 'assemble':
        assemble(a.downloads, a.output, int(a.build), a.commit)
    else:
        publish(a.output, a.commit, a.notes)


if __name__ == '__main__':
    main()
