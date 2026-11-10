#!/usr/bin/env python3
"""Build/test the workspace or the changed packages and their reverse dependencies."""
import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
from sparkle import prepare
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def selected_packages(metadata, changed):
    packages = {p['name']: p for p in metadata['packages'] if p['id'] in metadata['workspace_members']}
    if changed is None:
        return sorted(packages)
    chosen = set()
    for name in changed:
        path = ROOT / name
        owners = [p['name'] for p in packages.values() if path.is_relative_to(Path(p['manifest_path']).parent)]
        if name.startswith('apps/oink/'):
            owners += [p['name'] for p in packages.values() if Path(p['manifest_path']).parent == ROOT / 'apps/oink/src-tauri']
        if owners:
            chosen.update(owners)
        elif not (name.startswith(('docs/', 'apps/file-provider/')) or name.endswith('.md')):
            return sorted(packages)
    while True:
        dependents = {p['name'] for p in packages.values() if any(d['name'] in chosen for d in p['dependencies'])}
        if dependents <= chosen:
            return sorted(chosen)
        chosen.update(dependents)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--base', help='PR merge base; omit for a full build/test')
    p.add_argument('--plan', action='store_true')
    p.add_argument('--slow', action='store_true', help='Run explicitly opted-in slow, device, and native tests')
    a = p.parse_args()
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--format-version', '1', '--locked'], cwd=ROOT))
    changed = subprocess.check_output(['git', 'diff', '--name-only', f'{a.base}...HEAD'], cwd=ROOT, text=True).splitlines() if a.base else None
    selected = selected_packages(metadata, changed)
    print(json.dumps({'mode': 'affected' if a.base else 'full', 'packages': selected}), flush=True)
    if a.plan or not selected:
        return
    packages = [arg for name in selected for arg in ['-p', name]] if a.base else ['--workspace']
    if a.slow:
        subprocess.run(['cargo', 'test', '--locked', *packages, '--', '--ignored', '--skip', 'held_process_fixture'],
                       cwd=ROOT, check=True)
        subprocess.run(['npm', 'run', '-w', 'apps/oink', 'test:slow'], cwd=ROOT, check=True)
        environment = os.environ.copy()
        environment['OINK_SLOW_TESTS'] = '1'
        subprocess.run([sys.executable, '-m', 'unittest', 'discover', '-s', 'packaging/tests', '-v'],
                       cwd=ROOT, env=environment, check=True)
        subprocess.run([sys.executable, '-m', 'unittest', 'discover', '-s', 'build-support', '-p', 'test_*.py', '-v'],
                       cwd=ROOT, env=environment, check=True)
        subprocess.run([sys.executable, '-m', 'unittest', 'discover', '-s', 'tools/session-recorder',
                        '-p', 'test_export_layout.py', '-v'], cwd=ROOT, env=environment, check=True)
        return
    for args in [['clippy', '--locked', '--all-targets', *packages, '--', '-D', 'warnings'],
                 ['test', '--locked', *packages], ['check', '--locked', '--release', *packages],
                 ['test', '--locked', '--release', '--lib', *packages]]:
        environment = os.environ.copy()
        if '--release' in args:
            # Release parser/cfg tests need optimized cfg, not expensive LTO.
            # Product packaging retains its normal thin-LTO profile.
            environment.update(CARGO_PROFILE_RELEASE_LTO='false', CARGO_PROFILE_RELEASE_CODEGEN_UNITS='16', CARGO_PROFILE_RELEASE_DEBUG='0')
        subprocess.run(['cargo', *args], cwd=ROOT, env=environment, check=True)
    if platform.system() == 'Darwin' and 'oink-agent' in selected:
        with tempfile.TemporaryDirectory(prefix='oink-ci-sparkle-') as cache:
            environment = os.environ.copy()
            environment['SPARKLE_FRAMEWORK_DIR'] = str(prepare(Path(cache)))
            environment.update(CARGO_PROFILE_RELEASE_LTO='false', CARGO_PROFILE_RELEASE_CODEGEN_UNITS='16', CARGO_PROFILE_RELEASE_DEBUG='0')
            # Compile the native bridge here. The release workflow exercises the
            # real embedded framework loader in its mandatory signed smoke.
            subprocess.run(['cargo', 'check', '--release', '--locked', '-p', 'oink-agent', '--features', 'sparkle'],
                           cwd=ROOT, env=environment, check=True)
            # Run the production bridge's isolated continuation/feed tests in
            # the selected architecture (Rosetta executes x86 tests on ARM).
            arch = {'x86_64-apple-darwin': 'x86_64', 'aarch64-apple-darwin': 'arm64'}.get(
                environment.get('CARGO_BUILD_TARGET'), platform.machine())
            native = Path(cache) / 'update-tests'
            subprocess.run(['xcrun', 'clang', '-target', f'{arch}-apple-macos14.2',
                            '-fobjc-arc', '-fblocks', '-Werror',
                            '-F' + environment['SPARKLE_FRAMEWORK_DIR'],
                            '-framework', 'AppKit', '-framework', 'Sparkle',
                            '-Wl,-rpath,' + environment['SPARKLE_FRAMEWORK_DIR'],
                            str(ROOT / 'crates/oink-agent/native/update-tests.m'), '-o', str(native)],
                           cwd=ROOT, env=environment, check=True)
            subprocess.run([str(native)], cwd=ROOT, env=environment, check=True)


if __name__ == '__main__':
    main()
