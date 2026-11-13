#!/usr/bin/env python3
"""0.4 CI: allocate labels, verify installer artifacts, and create only a draft."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import zipfile
from identity import encode_build
from provenance import digest, source_tree_sha256
from versioning import beta_label, inhouse_version

TARGETS = ('macos-arm64', 'macos-x64', 'windows-x64', 'android-arm64-v8a', 'android-x86_64')


def plan(sequence, reserve=False, request_id=''):
    build = encode_build(inhouse_version(), sequence)
    if sequence <= 8:
        raise ValueError('shared CI sequence must exceed the frozen in-house builds')
    if reserve and not re.fullmatch('[0-9a-f]{32}', request_id):
        raise ValueError('reserve-only requests require a random 32-character request id')
    return dict(schema=1, repository='oinkvm/oink', workflow='ci.yml', ref='refs/heads/0.4',
                channel='inhouse' if reserve else 'beta', version=inhouse_version(), sequence=sequence,
                build=build, label=f'{inhouse_version()}-inhouse.{sequence}' if reserve else beta_label(build),
                request_id=request_id, run_id=int(os.environ.get('GITHUB_RUN_ID', '0')))


def context():
    if os.environ.get('GITHUB_REPOSITORY') != 'oinkvm/oink' or os.environ.get('GITHUB_REF') != 'refs/heads/0.4':
        raise ValueError('beta artifact operations are restricted to oinkvm/oink refs/heads/0.4')
    sequence = int(os.environ['GITHUB_RUN_NUMBER'])
    return plan(sequence)


def copy_artifact(source, output, name=None):
    name = name or source.name
    if Path(name).name != name:
        raise ValueError('unsafe artifact basename')
    target = output / name
    shutil.copy2(source, target)
    return dict(name=name, sha256=digest(target), size=target.stat().st_size)


def stage(output, target, packages=None, apk=None, aapt2=None):
    p = context(); output.mkdir(parents=True, exist_ok=True)
    record = dict(schema=1, target=target, version=p['label'], build=p['build'],
                  built_commit=os.environ['GITHUB_SHA'], source_tree_sha256=source_tree_sha256(), artifacts=[])
    if target.startswith('android-'):
        abi = target.removeprefix('android-')
        badging = subprocess.check_output([str(aapt2), 'dump', 'badging', str(apk)], text=True)
        if not re.search(r"versionCode='%d'.*versionName='%s'" % (p['build'], re.escape(p['label'])), badging):
            raise ValueError('APK version/code differs from this CI run')
        with zipfile.ZipFile(apk) as z:
            abis = {n.split('/')[1] for n in z.namelist() if n.startswith('lib/') and n.endswith('.so')}
            if abis != {abi} or f'lib/{abi}/liboink_mobile_ffi.so' not in z.namelist():
                raise ValueError('APK does not contain exactly its requested Rust ABI')
        from android_signing import CERTIFICATE_SHA256
        record['signing'] = 'existing-inhouse-certificate'
        record['certificate_sha256'] = CERTIFICATE_SHA256
        record['artifacts'] = [copy_artifact(apk, output, f"Oink-{p['label']}-android-{abi}.apk")]
    else:
        paths = list(packages.glob('*/' + target + '/release.json'))
        if len(paths) != 1:
            raise ValueError('expected exactly one native package record')
        path = paths[0]; native = json.loads(path.read_text(encoding='utf-8-sig'))
        if (native['version'], native['build'], native['source_tree_sha256']) != (p['label'], p['build'], record['source_tree_sha256']):
            raise ValueError('native package version/build/source differs from CI')
        if target.startswith('macos-'):
            if native['signing'] != 'developer-id' or not native.get('notarized'):
                raise ValueError('Mac packages require Developer ID signing and notarization')
            record['signing'] = 'developer-id-notarized'
        else:
            record['signing'] = 'unsigned'
        for a in native['artifacts']:
            if Path(a['name']).suffix not in ('.dmg', '.zip', '.msi'):
                continue
            source = path.parent / a['name']
            if Path(a['name']).name != a['name'] or digest(source) != a['sha256'] or source.stat().st_size != a['size']:
                raise ValueError('native installer integrity mismatch')
            record['artifacts'].append(copy_artifact(source, output))
        expected = ['.dmg', '.zip'] if target.startswith('macos-') else ['.msi']
        if sorted(Path(a['name']).suffix for a in record['artifacts']) != expected:
            raise ValueError('native installer set incomplete')
    (output / f'manifest-{target}.json').write_text(json.dumps(record, indent=2) + '\n')


def assemble(downloads, output, expected, commit, tree):
    output.mkdir(parents=True, exist_ok=True); records={}; artifacts=[]
    for path in downloads.rglob('manifest-*.json'):
        r = json.loads(path.read_text())
        if r['target'] not in TARGETS or r['target'] in records:
            raise ValueError('unknown or duplicate artifact target')
        if (r['version'], r['build'], r['built_commit'], r['source_tree_sha256']) != (expected['label'], expected['build'], commit, tree):
            raise ValueError('artifacts do not share one source/version/build')
        if r['target'].startswith('android-'):
            from android_signing import CERTIFICATE_SHA256
            proof = json.loads((path.parent/'android-signature.json').read_text())
            if proof != dict(verified=True, certificate_sha256=CERTIFICATE_SHA256) or r.get('certificate_sha256') != CERTIFICATE_SHA256:
                raise ValueError('Android in-house signing certificate proof is missing')
        for a in r['artifacts']:
            source = path.parent / a['name']
            if Path(a['name']).name != a['name'] or digest(source) != a['sha256'] or source.stat().st_size != a['size']:
                raise ValueError('downloaded artifact integrity mismatch')
            artifacts.append(copy_artifact(source, output))
        records[r['target']] = r
    missing = sorted(set(TARGETS) - records.keys())
    if set(('macos-arm64','windows-x64')) - records.keys():
        raise ValueError('blocking Mac arm64 / Windows x64 installers are required for a draft')
    release = dict(schema=1, version=expected['label'], build=expected['build'], built_commit=commit,
                   source_tree_sha256=tree, platforms=records, missing=missing, artifacts=artifacts)
    (output / 'release.json').write_text(json.dumps(release, indent=2) + '\n')
    (output / 'SHA256SUMS').write_text(''.join(f"{digest(p)}  {p.name}\n" for p in [*(output/a['name'] for a in artifacts), output/'release.json']))
    body = [f"CI beta `{expected['label']}` (build {expected['build']}).", f'Source: `{commit}`.', '', 'Installers:']
    body += [f"- {target}: {records[target]['signing']}" for target in sorted(records)]
    if missing: body += ['', 'Missing non-blocking targets: ' + ', '.join(missing) + '.']
    body += ['', 'iOS is intentionally excluded. This draft does not publish update feeds.']
    (output / 'draft-notes.md').write_text('\n'.join(body)+'\n')
    return release


def publish(output, record):
    tag = 'ci-' + record['version']
    # The draft has its own non-release tag name. Never create a git ref or
    # invoke release.yml; publishing this draft remains a separate user action.
    rows = json.loads(subprocess.check_output(['gh','api','repos/oinkvm/oink/releases?per_page=100'],text=True))
    matches = [r for r in rows if r['tag_name'] == tag]
    if matches:
        old = matches[0]
        if not old['draft'] or old['target_commitish'] != record['built_commit']:
            raise ValueError('refuse to modify a published release or a different source')
        release_id = old['id']
    else:
        body = dict(tag_name=tag, target_commitish=record['built_commit'], name='Oink '+record['version'],
                    draft=True, prerelease=True, make_latest='false', body=(output/'draft-notes.md').read_text())
        request = output / 'draft-request.json';request.write_text(json.dumps(body))
        made = json.loads(subprocess.check_output(['gh','api','--method','POST','repos/oinkvm/oink/releases','--input',str(request)],text=True))
        release_id = made['id']
    files = [output/a['name'] for a in record['artifacts']] + [output/'release.json',output/'SHA256SUMS']
    subprocess.run(['gh','release','upload',tag,'--repo','oinkvm/oink','--clobber',*map(str,files)],check=True)
    final = json.loads(subprocess.check_output(['gh','api',f'repos/oinkvm/oink/releases/{release_id}'],text=True))
    if not final['draft'] or set(a['name'] for a in final['assets']) != {p.name for p in files}:
        raise ValueError('draft state/assets verification failed')
    print(final['html_url'])
    with open(os.environ['GITHUB_STEP_SUMMARY'],'a') as summary:
        summary.write(f"Draft: {final['html_url']}\n\n"+(output/'draft-notes.md').read_text())


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('operation',choices=['plan','stage','publish'])
    p.add_argument('--target',choices=TARGETS);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--packages',type=Path);p.add_argument('--apk',type=Path);p.add_argument('--aapt2',type=Path);p.add_argument('--downloads',type=Path)
    a=p.parse_args(); info=context()
    if a.operation=='plan':
        reserve=os.environ.get('RESERVE_ONLY')=='true';info=plan(int(os.environ['GITHUB_RUN_NUMBER']),reserve,os.environ.get('REQUEST_ID',''))
        a.output.mkdir(parents=True,exist_ok=True)
        (a.output/'build-reservation.json').write_text(json.dumps(info,indent=2)+'\n')
        with open(os.environ['GITHUB_OUTPUT'],'a') as out:
            out.write(f"sequence={info['sequence']}\nbuild={info['build']}\nversion={info['label']}\n")
    elif a.operation=='stage':stage(a.output,a.target,a.packages,a.apk,a.aapt2)
    else:publish(a.output,assemble(a.downloads,a.output,info,os.environ['GITHUB_SHA'],source_tree_sha256()))


if __name__=='__main__':main()
