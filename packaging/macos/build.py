#!/usr/bin/env python3
"""Assemble, sign and optionally notarize a release without installing it."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import sys
import plistlib
import re
import shutil
import subprocess
import tempfile
import uuid
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
from provenance import verify, digest
from payload import copy_notices
from report_manifest import write_manifest, image_identity
from signing import private_run, select_identity, verify_signature, notarize, staple_and_validate
from sparkle import prepare as prepare_sparkle, VERSION as SPARKLE_VERSION, SHA256 as SPARKLE_SHA256
from localizations import DECLARATIONS, STRINGS, write_localizations
from macos_imports import verify_imports
from macos_launch_smoke import launch_smoke
from macos_registration import temporary_registration

from versioning import release_version, macos_version_fields

ROOT = Path(__file__).resolve().parents[2]
I = json.loads((ROOT / 'packaging/identity.json').read_text())


def run(*args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def plist(path, value):
    path.write_bytes(plistlib.dumps(value))


def write_swift_identity(path):
    path.write_text('enum ReleaseIdentity {\n' + '\n'.join(
        f'    static let {key} = {json.dumps(value)}' for key, value in {
            'appGroupKey': I['macos']['app_group_key'], 'domain': I['macos']['file_provider_domain'],
            'displayName': I['name'], 'subsystem': I['macos']['file_provider_id']}.items()) + '\n}\n')


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binaries', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--arch', choices=['arm64'], default='arm64')
    p.add_argument('--version', default=I['version'])
    p.add_argument('--source-commit', help='Full source commit for a local commit-labelled version')
    p.add_argument('--build', type=int, required=True)
    p.add_argument('--source-tree-sha256', required=True)
    p.add_argument('--signing-fingerprint', default=os.environ.get('MACOS_SIGNING_FINGERPRINT'))
    p.add_argument('--team', choices=[I['macos']['team_id']], default=I['macos']['team_id'])
    p.add_argument('--notary-profile', default=os.environ.get('NOTARY_PROFILE'))
    p.add_argument('--notary-timeout', type=int, default=300)
    p.add_argument('--notary-keychain', type=Path, help='Optional non-default keychain containing the notary profile')
    p.add_argument('--sparkle-cache', type=Path)
    a = p.parse_args()
    if not re.fullmatch(r'[0-9a-f]{64}', a.source_tree_sha256) or not 1 <= a.build < 2**31:
        p.error('source tree SHA256 and positive build required')
    if a.version != I['version']:
        p.error('version must match packaging/identity.json')
    try:
        a.version = release_version(a.version, a.source_commit)
    except ValueError as error:
        p.error(str(error))
    trust = json.loads((ROOT / 'packaging/update-trust.json').read_text())
    a.sparkle_public_key = trust['mac']['public_key']
    a.identity = select_identity(a.team, a.signing_fingerprint)
    provenance = verify(a.binaries, a.source_tree_sha256, a.version, 'aarch64-apple-darwin', a.build, a.source_commit)
    if not a.sparkle_cache:
        p.error('the Mac package requires Sparkle.framework')
    if bool(a.sparkle_cache) != provenance.get('sparkle', False):
        p.error('Sparkle feature and embedded framework must match the binary build')
    if a.sparkle_public_key:
        try:
            if len(base64.b64decode(a.sparkle_public_key, validate=True)) != 32:
                raise ValueError('wrong length')
        except ValueError:
            p.error('Sparkle Ed25519 public key must be base64 for exactly 32 bytes')
    a.output = a.output.resolve()
    a.output.mkdir(parents=True, exist_ok=True)
    app = a.output / 'staging.noindex' / I['macos']['app']
    if app.exists():
        p.error('output app already exists; choose a fresh output directory')
    mac = app / 'Contents/MacOS'
    resources = app / 'Contents/Resources'
    mac.mkdir(parents=True)
    resources.mkdir()
    b = I['binaries']
    for role in ['backend', 'agent', 'crash_collector']:
        shutil.copy2(a.binaries / b[role], mac / b[role])
    shutil.copy2(ROOT / 'apps/oink/src-tauri/icons/icon.icns', resources / 'icon.icns')
    # macOS treats a basename/display-name mismatch as a user rename and
    # shows the basename in Dock/Finder (verified on macOS 26). Keep both Oink;
    # the helper's bundle identifier remains its distinct LaunchServices identity.
    helper = app / 'Contents/Helpers/Oink.app'
    (helper / 'Contents/MacOS').mkdir(parents=True)
    shutil.copy2(a.binaries / b['ui'], helper / 'Contents/MacOS' / b['ui'])
    (helper / 'Contents/Resources').mkdir()
    shutil.copy2(resources / 'icon.icns', helper / 'Contents/Resources/icon.icns')
    plist(helper / 'Contents/Info.plist', dict(
        CFBundleExecutable=b['ui'], CFBundleIdentifier=I['macos']['ui_id'],
        CFBundleName=I['name'], CFBundleDisplayName=I['name'], CFBundlePackageType='APPL',
        **macos_version_fields(a.version, a.build),
        CFBundleIconFile='icon.icns', LSMinimumSystemVersion=I['macos_minimum'], LSUIElement=True,
        NSHighResolutionCapable=True, **DECLARATIONS))
    write_localizations(helper)
    group = a.team + '.' + I['macos']['app_group_suffix']
    info = dict(CFBundleExecutable=b['backend'], CFBundleIdentifier=I['bundle_id'],
                CFBundleName=I['name'], CFBundleDisplayName=I['name'], CFBundlePackageType='APPL',
                **macos_version_fields(a.version, a.build), CFBundleIconFile='icon.icns',
                LSMinimumSystemVersion=I['macos_minimum'], LSUIElement=True, NSHighResolutionCapable=True,
                NSBonjourServices=[I['bonjour_service']], **DECLARATIONS)
    info.update(STRINGS['en'])
    write_localizations(app, permissions=True)
    info[I['macos']['app_group_key']] = group
    if a.sparkle_cache:
        info.update(SUVerifyUpdateBeforeExtraction=True, SURequireSignedFeed=True, SUSignedFeedFailureExpirationInterval=0,
                    SUEnableJavaScript=False, SUEnableSystemProfiling=False, SUEnableAutomaticChecks=False,
                    SUAutomaticallyUpdate=False)
        if a.sparkle_public_key:
            info['SUPublicEDKey'] = a.sparkle_public_key
        else:
            print('Automatic updates are disabled for beta.1; no Sparkle public key is configured.')
        framework = app / 'Contents/Frameworks/Sparkle.framework'
        framework.parent.mkdir()
        shutil.copytree(prepare_sparkle(a.sparkle_cache) / 'Sparkle.framework', framework, symlinks=True)
        load_commands = run('otool', '-l', mac / b['backend'], capture_output=True, text=True).stdout
        if '@executable_path/../Frameworks' not in load_commands:
            raise RuntimeError('Sparkle backend is missing the required Frameworks rpath')
    plist(app / 'Contents/Info.plist', info)
    ext = app / 'Contents/PlugIns' / I['macos']['file_provider_bundle']
    (ext / 'Contents/MacOS').mkdir(parents=True)
    ext_info = dict(CFBundleIdentifier=I['macos']['file_provider_id'], CFBundleName=I['macos']['file_provider_name'],
                    CFBundleExecutable=b['file_provider_extension'], CFBundlePackageType='XPC!',
                    **macos_version_fields(a.version, a.build), LSMinimumSystemVersion=I['macos_minimum'],
                    **DECLARATIONS)
    write_localizations(ext)
    ext_info[I['macos']['app_group_key']] = group
    ext_info['NSExtension'] = dict(NSExtensionPointIdentifier='com.apple.fileprovider-nonui',
        NSExtensionPrincipalClass=b['file_provider_class'], NSExtensionFileProviderDocumentGroup=group,
        NSExtensionFileProviderSupportsEnumeration=True)
    plist(ext / 'Contents/Info.plist', ext_info)
    copy_notices(resources)
    with tempfile.TemporaryDirectory(prefix='release-package-') as tmp:
        tmp = Path(tmp)
        generated = tmp / 'ReleaseIdentity.swift'
        write_swift_identity(generated)
        host = {'com.apple.security.application-groups': [group], 'com.apple.security.device.audio-input': True}
        plist(tmp / 'host.plist', host)
        plist(tmp / 'extension.plist', {'com.apple.security.application-groups': [group],
              'com.apple.security.app-sandbox': True, 'com.apple.security.network.client': True})
        manager = dict(CFBundleIdentifier=I['bundle_id'], CFBundleName=I['name'], **macos_version_fields(a.version, a.build))
        manager[I['macos']['app_group_key']] = group
        plist(tmp / 'manager.plist', manager)
        common = ['xcrun', 'swiftc', '-swift-version', '5', '-O', '-g', '-debug-prefix-map', f'{ROOT}=oink',
                  '-debug-prefix-map', f'{tmp}=oink/generated',
                  '-target', f'{a.arch}-apple-macos{I["macos_minimum"]}', '-parse-as-library', '-D', 'OINK_RELEASE']
        run('xcrun', 'clang', '-O2', '-g', f'-ffile-prefix-map={ROOT}=oink', '-std=c11', '-target', f'arm64-apple-macos{I["macos_minimum"]}',
            '-c', ROOT / 'apps/file-provider/CrashCapture.c', '-o', tmp / 'crash-capture.o')
        run(*common, '-module-name', b['file_provider_extension'], '-application-extension',
            '-Xlinker', '-e', '-Xlinker', '_NSExtensionMain', '-l', 'extension',
            generated, ROOT / 'apps/file-provider/Shared.swift', ROOT / 'apps/file-provider/ReportCapture.swift',
            ROOT / 'apps/file-provider/Extension.swift', tmp / 'crash-capture.o',
            '-o', ext / 'Contents/MacOS' / b['file_provider_extension'])
        run(*common, '-Xlinker', '-sectcreate', '-Xlinker', '__TEXT', '-Xlinker', '__info_plist',
            '-Xlinker', tmp / 'manager.plist', generated, ROOT / 'apps/file-provider/Shared.swift',
            ROOT / 'apps/file-provider/Uninstall.swift', ROOT / 'apps/file-provider/Manager.swift', '-o', mac / b['file_provider_manager'])
        report = write_manifest(resources / 'report-manifest.json', a.source_tree_sha256, a.version, a.build,
                                [('agent', mac / b['backend']), ('agent', mac / b['agent']),
                                 ('ui_rust', helper / 'Contents/MacOS' / b['ui']),
                                 ('file_provider', ext / 'Contents/MacOS' / b['file_provider_extension'])])
        shutil.copy2(resources / 'report-manifest.json', helper / 'Contents/Resources/report-manifest.json')
        symbols = a.output / 'symbols.noindex'
        symbols.mkdir(exist_ok=True)
        symbol_images = [mac / b[role] for role in ('backend', 'agent', 'crash_collector', 'file_provider_manager')]
        symbol_images += [helper / 'Contents/MacOS' / b['ui'], ext / 'Contents/MacOS' / b['file_provider_extension']]
        symbol_records = []
        for image in symbol_images:
            dsym = symbols / (image.name + '.dSYM')
            generated = (image.parent if image.name in (b['file_provider_manager'], b['file_provider_extension'])
                         else a.binaries) / (image.name + '.dSYM')
            if not generated.is_dir():
                raise RuntimeError('Link-time dSYM missing for ' + image.name)
            if dsym.exists():
                shutil.rmtree(dsym)
            if generated.is_relative_to(app):
                shutil.move(generated, dsym)
            else:
                shutil.copytree(generated, dsym)
            # Cargo's link-time dSYM keeps the hashed Rust artifact basename;
            # Swift uses the installed executable name. Bind by UUID below.
            dwarfs = list((dsym / 'Contents/Resources/DWARF').iterdir())
            if len(dwarfs) != 1 or not dwarfs[0].is_file():
                raise RuntimeError('Expected one arm64 DWARF image in the dSYM')
            dwarf = dwarfs[0]
            output = subprocess.check_output(['xcrun', 'dwarfdump', '--uuid', str(dwarf)], text=True)
            identifier = re.search(r'UUID: ([0-9A-Fa-f-]{36})', output)
            if not identifier or list(uuid.UUID(identifier[1]).bytes) != image_identity(image)['debug_id']:
                raise RuntimeError('dSYM does not match the linked image')
            statistics = json.loads(subprocess.check_output(['xcrun', 'dwarfdump', '--statistics', str(dwarf)], text=True))
            line_bytes = statistics.get('#bytes in __debug_line', 0)
            if not line_bytes:
                raise RuntimeError('Private dSYM contains no source line information')
            symbol_records.append({'image': image.name, **image_identity(image),
                                   'dwarf_file': dwarf.relative_to(symbols).as_posix(),
                                   'dwarf_sha256': digest(dwarf), 'line_table_bytes': line_bytes})
            # Keep compiler object paths/debug records in the private dSYM,
            # rather than the signed public executable.
            linked_identity = image_identity(image)
            run('xcrun', 'strip', '-S', image)
            if image_identity(image) != linked_identity:
                raise RuntimeError('Removing debug records changed the linked image identity')
        (symbols / 'manifest.json').write_text(json.dumps({'source_tree_sha256': a.source_tree_sha256,
                                                         'images': symbol_records}, indent=2) + '\n')
        record = dict(schema=1, product=I['name'], version=a.version, source_commit=a.source_commit, build=a.build, provenance='source-tree', source_tree_sha256=a.source_tree_sha256,
                      architecture=a.arch, minimum_os=I['macos_minimum'], protocol_baseline=1,
                      signing='developer-id', notarization='see detached release record', update_policy='manual-only')
        record['sparkle'] = dict(included=bool(a.sparkle_cache), version=SPARKLE_VERSION if a.sparkle_cache else None,
                                archive_sha256=SPARKLE_SHA256 if a.sparkle_cache else None,
                                public_key_configured=bool(a.sparkle_public_key))
        (resources / I['metadata_file']).write_text(json.dumps(record, indent=2) + '\n')
        record['report_manifest'] = report
        # Include native Swift code and all embedded framework images. Fail
        # before signing/notarization even when the SDK's availability is wrong.
        verify_imports([app], I['macos_minimum'])

        def sign(path, identifier, entitlements=None):
            args = ['codesign', '--force', '--options', 'runtime', '--timestamp', '--sign', a.identity, '--identifier', identifier]
            if entitlements:
                args += ['--entitlements', str(entitlements)]
            private_run(*args, path)
        if a.sparkle_cache:
            # Sparkle's XPC services retain their upstream sandbox entitlements
            # and identifiers while all code is re-signed by the app's team.
            for path in sorted(framework.rglob('*'), key=lambda p: len(p.parts), reverse=True):
                if path.is_symlink():
                    continue
                executable = path.is_file() and 'Mach-O' in run('file', '-b', path, capture_output=True, text=True).stdout
                bundle = path.is_dir() and path.suffix in ['.app', '.xpc', '.framework']
                if executable:
                    arches = run('lipo', '-archs', path, capture_output=True, text=True).stdout.split()
                    if arches != ['arm64']:
                        thinned = tmp / 'arm64-image'
                        run('lipo', path, '-thin', 'arm64', '-output', thinned)
                        shutil.copymode(path, thinned)
                        shutil.move(thinned, path)
                if executable or bundle:
                    private_run('codesign', '--force', '--options', 'runtime', '--timestamp', '--sign', a.identity,
                        '--preserve-metadata=identifier,entitlements', path)
            private_run('codesign', '--force', '--options', 'runtime', '--timestamp', '--sign', a.identity,
                '--preserve-metadata=identifier,entitlements', framework)
        sign(ext, I['macos']['file_provider_id'], tmp / 'extension.plist')
        sign(mac / b['file_provider_manager'], I['bundle_id'], tmp / 'host.plist')
        sign(helper, I['macos']['ui_id'])
        sign(mac / b['agent'], I['macos']['agent_id'])
        sign(mac / b['crash_collector'], I['bundle_id'] + '.crash-collector')
        sign(app, I['bundle_id'], tmp / 'host.plist')
        verify_signature(app, a.team)
        from content import verify_app
        record['components'] = verify_app(app, I)
        run('node', ROOT / 'build-support/check-package-licenses.mjs', app, 'macos-arm64', cwd=ROOT)
        # Exercise the shipping signature, hardened runtime and embedded loader
        # before notarization. Private smoke state never contacts the user host.
        backend = mac / b['backend']
        # LaunchServices discovers the nested appex when the host is launched,
        # even though smoke disables File Provider work. Remove only this
        # disposable bundle's registrations before another installation can
        # choose it as its provider host; do this on smoke failures too.
        with temporary_registration(app, I['macos']['file_provider_bundle'], I['macos']['file_provider_id']):
            launch_smoke(backend, seconds=10)
        record['launch_smoke'] = {'status': 'passed', 'seconds': 10, 'signed': True,
                                  'backend': backend.relative_to(app).as_posix(),
                                  'backend_sha256': digest(backend)}
        app_notary = {'status': 'not-submitted'}
        if a.notary_profile:
            submission = tmp / 'submission.zip'
            run('ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', app, submission)
            # A ZIP is submitted, but the returned ticket belongs to its app.
            app_notary = notarize(submission, a.notary_profile, a.notary_keychain, a.notary_timeout)
            if app_notary['status'] == 'Accepted':
                staple_and_validate(app)
                app_notary['stapled'] = True
        stem = f"{I['name']}-{a.version}-{a.build}-macos-{a.arch}"
        dmg = a.output / f'{stem}.dmg'
        from dmg import create
        create(app, dmg, a.sparkle_cache)
        private_run('codesign', '--force', '--timestamp', '--sign', a.identity, dmg)
        private_run('codesign', '--verify', '--strict', dmg)
        dmg_notary = notarize(dmg, a.notary_profile, a.notary_keychain, a.notary_timeout)
        archive = a.output / f'{stem}.zip'
        run('ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', app, archive)
        record['notarization'] = {'app': app_notary, 'dmg': dmg_notary}
        record['notarized'] = app_notary.get('stapled', False) and dmg_notary.get('stapled', False)
        record['team_id'] = a.team
        record['certificate_sha1'] = a.identity
        record['artifacts'] = [{'name': f.name, 'size': f.stat().st_size,
                                'sha256': hashlib.sha256(f.read_bytes()).hexdigest()} for f in [dmg, archive]]
        (a.output / 'release.json').write_text(json.dumps(record, indent=2) + '\n')
        shutil.rmtree(app)
        (a.output / 'SHA256SUMS').write_text(''.join(f"{f['sha256']}  {f['name']}\n" for f in record['artifacts']))


if __name__ == '__main__':
    main()
