#!/usr/bin/env python3
"""Build the two product targets without installing or launching them."""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import shlex
import subprocess
from provenance import digest, source_tree_sha256
from sparkle import prepare as prepare_sparkle
from versioning import release_version
from package_cache import configure_cache
from update_qualification import origin as test_origin, verify_binaries

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = json.loads((ROOT / 'packaging/identity.json').read_text())
TARGETS = ('aarch64-apple-darwin', 'x86_64-pc-windows-msvc')


def run(*args, **kwargs):
    subprocess.run([str(a) for a in args], check=True, **kwargs)


def release_build(explicit, environment):
    supplied = [(name, value) for name, value in [('--build', explicit),
                ('OINK_RELEASE_BUILD', environment.get('OINK_RELEASE_BUILD')),
                ('RELEASE_BUILD', environment.get('RELEASE_BUILD'))] if value is not None]
    if not supplied:
        raise ValueError('release build required: use --build or OINK_RELEASE_BUILD')
    numbers = []
    for name, value in supplied:
        if not re.fullmatch(r'[0-9]{1,10}', value) or not 1 <= int(value) < 2**31:
            raise ValueError(f'{name} must be an integer in 1..2147483647')
        numbers.append(int(value))
    if len(set(numbers)) != 1:
        raise ValueError('release build arguments and environment must agree')
    return numbers[0]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--target', required=True, choices=TARGETS)
    p.add_argument('--source-tree-sha256', required=True)
    p.add_argument('--source-inventory', type=Path)
    p.add_argument('--version', default=IDENTITY['version'])
    p.add_argument('--source-commit', help='Full source commit for a local commit-labelled version')
    p.add_argument('--build', required=True)
    p.add_argument('--sparkle-cache', type=Path)
    p.add_argument('--windows-update-test-feed', help='TEST ONLY: compile a literal HTTP loopback feed and public TEST key')
    p.add_argument('--symbols-output', type=Path, help='Private symbols outside the source and package payload')
    p.add_argument('--windows-update-test-fail-health', action='store_true', help='TEST ONLY: withhold candidate health proof for rollback qualification')
    a = p.parse_args()
    environment = os.environ.copy()
    try:
        test_origin(a.windows_update_test_feed)
    except ValueError as error:
        p.error(str(error))
    if a.windows_update_test_feed and a.target != TARGETS[1]:
        p.error('test update feeds are Windows-only')
    if environment.get('OINK_WINDOWS_UPDATE_TEST_FEED') and environment['OINK_WINDOWS_UPDATE_TEST_FEED'] != a.windows_update_test_feed:
        p.error('test feed environment requires the matching explicit build flag')
    if a.windows_update_test_fail_health and not a.windows_update_test_feed:
        p.error('test health failure requires a test feed')
    if environment.get('OINK_WINDOWS_UPDATE_TEST_FAIL_HEALTH') and not a.windows_update_test_fail_health:
        p.error('test health environment requires the matching explicit flag')
    if a.windows_update_test_feed:
        environment['OINK_WINDOWS_UPDATE_TEST_FEED'] = a.windows_update_test_feed
    if a.windows_update_test_fail_health:
        environment['OINK_WINDOWS_UPDATE_TEST_FAIL_HEALTH'] = '1'
    try:
        a.build = release_build(a.build, environment)
    except ValueError as error:
        p.error(str(error))
    if not re.fullmatch(r'[0-9a-f]{64}', a.source_tree_sha256):
        p.error('source-tree-sha256 must be 64 lowercase hexadecimal characters')
    if a.version != IDENTITY['version']:
        p.error('version must match packaging/identity.json and the Cargo/npm manifests')
    try:
        display_version = release_version(a.version, a.source_commit)
    except ValueError as error:
        p.error(str(error))
    inventory = json.loads(a.source_inventory.read_text())['files'] if a.source_inventory else None
    if source_tree_sha256(ROOT, inventory) != a.source_tree_sha256:
        p.error('source tree changed before build')
    environment['OINK_RELEASE_BUILD'] = environment['RELEASE_BUILD'] = str(a.build)
    environment['OINK_RELEASE_VERSION'] = display_version
    environment['OINK_SOURCE_COMMIT'] = a.source_commit or ''
    environment['OINK_SOURCE_TREE_SHA256'] = a.source_tree_sha256
    environment['MACOSX_DEPLOYMENT_TARGET'] = IDENTITY['macos_minimum']
    if a.symbols_output:
        if a.symbols_output.resolve().is_relative_to(ROOT):
            p.error('private symbols must be outside the source tree')
        environment['OINK_SYMBOLS_DIR'] = str(a.symbols_output.resolve() / 'javascript')
    environment.update(CARGO_INCREMENTAL='0', CARGO_PROFILE_DEV_DEBUG='0', CARGO_PROFILE_TEST_DEBUG='0')
    environment['CARGO_PROFILE_RELEASE_DEBUG'] = '1'
    environment['CARGO_PROFILE_RELEASE_STRIP'] = 'none'
    # Remap compiler paths in panic locations and debug metadata; source provenance
    # is the explicit tree hash, never the builder's home/checkout directory.
    flags = environment.get('CARGO_ENCODED_RUSTFLAGS', '').split('\x1f') if environment.get('CARGO_ENCODED_RUSTFLAGS') else shlex.split(environment.get('RUSTFLAGS', ''))
    cargo_home = environment.get('CARGO_HOME') or str(Path.home() / '.cargo')
    flags += [f'--remap-path-prefix={ROOT}=oink', f'--remap-path-prefix={cargo_home}=cargo']
    # Opus C assertions retain __FILE__ at runtime. Rust's path remapping does
    # not affect native C/C++; MSVC pathmap requires deterministic compilation.
    native = [] if a.target == TARGETS[0] else ['/experimental:deterministic']
    for directory, logical in ((ROOT, 'oink'), (cargo_home, 'cargo')):
        native.append(f'-ffile-prefix-map={directory}={logical}' if a.target == TARGETS[0]
                      else f'/pathmap:{directory}={logical}')
    environment['CC_SHELL_ESCAPED_FLAGS'] = '1'
    for name in ('CFLAGS', 'CXXFLAGS'):
        environment[name] = (environment.get(name, '') + ' ' + shlex.join(native)).strip()
    if a.target == TARGETS[1]:
        # Keep the private PDB filename, never the builder's absolute directory,
        # in the installed executable's CodeView record.
        flags += ['-C', 'link-arg=/PDBALTPATH:%_PDB%', '-C', 'target-feature=+crt-static']
    environment['CARGO_ENCODED_RUSTFLAGS'] = '\x1f'.join(flags)
    environment.pop('RUSTFLAGS', None)
    features = 'custom-protocol'
    if a.windows_update_test_feed:
        features += ',oink-winsvc/test-feed'
    macos = a.target == TARGETS[0]
    if macos:
        # rustc must gather native objects while its LTO temporary directories
        # still exist; running dsymutil later loses their debug information.
        environment['CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO'] = 'packed'
        if not a.sparkle_cache:
            p.error('Mac product builds require the pinned Sparkle framework')
        environment['SPARKLE_FRAMEWORK_DIR'] = str(prepare_sparkle(a.sparkle_cache))
        features += ',oink-agent/sparkle'
    elif a.sparkle_cache:
        p.error('Sparkle is macOS-only')
    environment = configure_cache(environment)
    npm = 'npm.cmd' if platform.system() == 'Windows' else 'npm'
    # A hash can be an all-numeric identifier with a leading zero, which SemVer
    # rejects. Keep Tauri's internal version canonical; runtime labels use env.
    config = {'productName': IDENTITY['name'], 'identifier': IDENTITY['bundle_id'],
              'version': a.version, 'bundle': {'macOS': {'minimumSystemVersion': IDENTITY['macos_minimum']}}}
    run(npm, 'ci', cwd=ROOT, env=environment)
    run(npm, 'run', 'licenses:check', cwd=ROOT, env=environment)
    # Source archives preserve old mtimes. Cargo may otherwise reuse an older
    # workspace body even when the recorded source bytes changed. Preserve the
    # registry/sccache caches, but compile every product workspace package fresh.
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'], cwd=ROOT, env=environment))
    members = set(metadata['workspace_members'])
    workspace = sorted(package['name'] for package in metadata['packages'] if package['id'] in members)
    # opusic-sys's CMake build does not emit rerun-if-env-changed for CFLAGS.
    # Rebuild it so cached native file macros cannot bypass the current maps.
    clean = [argument for name in sorted({*workspace, 'opusic-sys'}) for argument in ('-p', name)]
    run('cargo', 'clean', '--release', '--target', a.target, *clean, cwd=ROOT, env=environment)
    # Tauri forwards these Cargo arguments and adds --bins. Select the UI and
    # every product package together so dependencies have one feature resolution.
    # A second cargo build without the UI used a smaller graph and recompiled
    # the backend and its dependencies. Keep Tauri's config/frontend preparation.
    packages = ['-p', 'oink-ui', '-p', 'oink-agent', '-p', 'oink-crash']
    if not macos:
        packages += ['-p', 'oink-winsvc']
    run(npm, 'run', '-w', 'apps/oink', 'tauri', '--', 'build', '--ci', '--no-bundle', '--no-sign',
        '--target', a.target, '--features', features, '--config', json.dumps(config), '--', '--locked', *packages,
        cwd=ROOT, env=environment)
    roles = ['ui', 'agent', 'crash_collector', 'backend'] if macos else ['ui', 'agent', 'service', 'update_worker', 'crash_collector']
    output = Path(environment.get('CARGO_TARGET_DIR', ROOT / 'target')) / a.target / 'release'
    names = [IDENTITY['binaries'][role] + ('' if macos else '.exe') for role in roles]
    if not macos:
        from windows_version import set_display_version
        for name in names:
            changed = set_display_version(output / name, display_version)
            if name == IDENTITY['binaries']['ui'] + '.exe' and not changed:
                raise RuntimeError('Windows UI version resource is missing')
    if macos:
        from macos_imports import verify_imports
        verify_imports([output / name for name in names], IDENTITY['macos_minimum'])
    if source_tree_sha256(ROOT, inventory) != a.source_tree_sha256:
        raise RuntimeError('Source tree changed during build; rebuild the recorded tree')
    verify_binaries([output / name for name in names], a.windows_update_test_feed)
    verify_binaries([path for path in (ROOT / 'apps/oink/dist').rglob('*') if path.is_file()], a.windows_update_test_feed)
    record = dict(schema=1, provenance='source-tree', source_tree_sha256=a.source_tree_sha256,
                  version=display_version, source_commit=a.source_commit, build=a.build, target=a.target, sparkle=macos,
                  updater_included=not macos, collector_included=True, worker_static_crt=not macos,
                  update_test_feed=a.windows_update_test_feed, update_test_fail_health=a.windows_update_test_fail_health,
                  identity_sha256=digest(ROOT / 'packaging/identity.json'),
                  files={name: digest(output / name) for name in names})
    (output / 'release-build.json').write_text(json.dumps(record, indent=2) + '\n')


if __name__ == '__main__':
    main()
