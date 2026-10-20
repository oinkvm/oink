#!/usr/bin/env python3
"""One local command builds a signed Mac DMG/ZIP or a Windows x64 MSI.

No CI, installation, service registration or credential creation. Mac packaging
launches the backend with temporary private state and desktop I/O disabled.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
from provenance import ROOT, source_inventory, source_tree_sha256
from identity import IDENTITY
from versioning import local_source_commit, release_version


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--platform', choices=['macos', 'windows'], required=True)
    p.add_argument('--output-root', type=Path, required=True)
    p.add_argument('--build', type=int, default=1)
    p.add_argument('--windows-update-test-feed', help='TEST ONLY: compile a literal HTTP loopback feed and TEST public key')
    versions = p.add_mutually_exclusive_group()
    versions.add_argument('--source-commit', help='Full source commit; required for local source archives')
    versions.add_argument('--public-version', action='store_true', help='Use the canonical manifest version for public releases')
    p.add_argument('--source-inventory', type=Path,
                   help='Portable source modes for an archive without a Git index')
    p.add_argument('--tool-cache', type=Path, default=Path.home() / '.cache/oink-build')
    p.add_argument('--signing-fingerprint', default=os.environ.get('MACOS_SIGNING_FINGERPRINT'), help='Optional SHA1 of an existing Developer ID certificate')
    p.add_argument('--notary-profile', default=os.environ.get('NOTARY_PROFILE'))
    p.add_argument('--notary-keychain', type=Path)
    p.add_argument('--notary-timeout', type=int, default=300)
    p.add_argument('--windows-update-test-fail-health', action='store_true', help='TEST ONLY: withhold candidate health proof for rollback qualification')
    a = p.parse_args()
    if a.windows_update_test_fail_health and not a.windows_update_test_feed:
        p.error('test health failure requires a test feed')
    from update_qualification import origin as test_origin
    try:
        test_origin(a.windows_update_test_feed)
    except ValueError as error:
        p.error(str(error))
    if a.windows_update_test_feed and (a.platform != 'windows' or a.public_version):
        p.error('test update feeds require a local Windows package, never --public-version')
    expected = 'Darwin' if a.platform == 'macos' else 'Windows'
    if platform.system() != expected or not supported_host(a.platform):
        p.error('run on an arm64 Mac or x64 Windows build host for the selected platform')
    a.output_root = a.output_root.resolve()
    if a.output_root.is_relative_to(ROOT):
        p.error('package output must be outside the source tree')
    if not 1 <= a.build < 2**31:
        p.error('build must be in 1..2147483647')
    try:
        commit = None if a.public_version else local_source_commit(a.source_commit)
        version = release_version(IDENTITY['version'], commit)
    except ValueError as error:
        p.error(str(error))
    tools = {'rust': subprocess.check_output(['rustc', '--version'], text=True).strip(),
             'node': subprocess.check_output(['node', '--version'], text=True).strip(),
             'npm': subprocess.check_output(['npm.cmd' if expected == 'Windows' else 'npm', '--version'], text=True).strip()}
    if not tools['rust'].startswith('rustc 1.99.0 ') or tools['node'] != 'v24.14.1' or tools['npm'] != '11.11.0':
        p.error('build requires Rust 1.99.0, Node 24.14.1 and npm 11.11.0')
    inventory = json.loads(a.source_inventory.read_text())['files'] if a.source_inventory else source_inventory()
    tree = source_tree_sha256(ROOT, inventory)
    output = a.output_root / tree / ('macos-arm64' if a.platform == 'macos' else 'windows-x64')
    output.mkdir(parents=True, exist_ok=True)
    if (output / 'release.json').exists():
        p.error('this source tree already has a package; choose a fresh output root for a rebuild')
    snapshot = output / 'source-inventory.json'
    (output / 'build-environment.json').write_text(json.dumps({**tools, 'version': version, 'source_commit': commit}, indent=2) + '\n')
    snapshot.write_text(json.dumps({'schema': 1, 'source_tree_sha256': tree, 'files': inventory}, indent=2) + '\n')
    macos = a.platform == 'macos'
    if macos:
        sys.path.insert(0, str(ROOT / 'packaging/macos'))
        from signing import select_identity
        a.signing_fingerprint = select_identity('893V37X2CW', a.signing_fingerprint)
    target = 'aarch64-apple-darwin' if macos else 'x86_64-pc-windows-msvc'
    common = ['--build', str(a.build), '--source-tree-sha256', tree]
    if commit:
        common += ['--source-commit', commit]
    if a.windows_update_test_feed:
        common += ['--windows-update-test-feed', a.windows_update_test_feed]
    command = [sys.executable, str(ROOT / 'build-support/build.py'), '--target', target,
               '--source-inventory', str(snapshot), '--symbols-output', str(output / 'symbols.noindex'), *common]
    if macos:
        command += ['--sparkle-cache', str(a.tool_cache)]
    if a.windows_update_test_fail_health:
        command += ['--windows-update-test-fail-health']
    subprocess.run(command, cwd=ROOT, check=True)
    binaries = Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target')) / target / 'release'
    command = [sys.executable, str(ROOT / 'packaging' / a.platform / 'build.py'),
               '--binaries', str(binaries), '--output', str(output), *common]
    if macos:
        command += ['--sparkle-cache', str(a.tool_cache), '--notary-timeout', str(a.notary_timeout)]
        for flag, value in [('--signing-fingerprint', a.signing_fingerprint),
                            ('--notary-profile', a.notary_profile), ('--notary-keychain', a.notary_keychain)]:
            if value:
                command += [flag, str(value)]
    else:
        command += ['--tool-cache', str(a.tool_cache)]
    subprocess.run(command, cwd=ROOT, check=True)
    print(f'Package complete: {output}')


def supported_host(selected):
    return platform.machine().lower() in (('arm64',) if selected == 'macos' else ('amd64', 'x86_64'))


if __name__ == '__main__':
    main()
