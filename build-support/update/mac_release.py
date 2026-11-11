#!/usr/bin/env python3
"""Sign a published, notarized release with Sparkle; never publish a draft.

Run trusted default-branch code only. The release tag is read as data, never
checked out or executed. The private seed reaches the pinned signer via stdin.
"""
import argparse
import importlib.util
import io
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess
import sys
import tarfile
import tempfile
import xml.etree.ElementTree as ET

import feed
import feed_site
import feed_v2
import published_release
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from provenance import source_tree_sha256
from sparkle import prepare
spec = importlib.util.spec_from_file_location('mac_appcast', Path(__file__).with_name('mac-appcast.py'))
appcast = importlib.util.module_from_spec(spec)
spec.loader.exec_module(appcast)

SECRET = 'SPARKLE_ED25519_PRIVATE_KEY'
# 0.3.6 (build encoding §1) and later Mac apps contain no Sparkle; older
# Sparkle clients still install them from the v1 appcast's signed ZIP.
SPARKLE_FREE_BUILD = 306000


def command(argv, **kwargs):
    result = subprocess.run(list(map(str, argv)), capture_output=True, **kwargs)
    feed.require(result.returncode == 0, Path(argv[0]).name.replace('-', '_') + '_failed')
    return result.stdout


def tag_source(tag):
    feed.require(re.fullmatch(published_release.TAG, tag), 'release_tag')
    command(['git', 'fetch', '--no-tags', 'origin', 'refs/tags/' + tag])
    commit = command(['git', 'rev-parse', 'FETCH_HEAD^{commit}']).decode().strip()
    # Public release tags must come from reviewed default-branch history.
    branch = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_bytes())['repository']['default_branch']
    command(['git', 'merge-base', '--is-ancestor', commit, 'origin/' + branch])
    with tempfile.TemporaryDirectory(prefix='oink-release-source-') as tmp:
        with tarfile.open(fileobj=io.BytesIO(command(['git', 'archive', commit]))) as archive:
            archive.extractall(tmp, filter='data')
        tree = source_tree_sha256(Path(tmp))
        built_trust = feed.read(Path(tmp) / 'packaging/update-trust.json')['mac']
    return commit, tree, built_trust


def published(tag):
    return published_release.published(tag, request=lambda path: feed.loads(command(['gh', 'api', path])))


def validate_release(record, annotations, tag, commit, tree, platform="macos"):
    feed.require(record['schema'] == 1 and record['product'] == 'Oink' and
                 record['version'] == tag.removeprefix('v') and record['built_commit'] == commit and
                 record['source_tree_sha256'] == tree and type(record['build']) is int and
                 0 < record['build'] < 2**31, 'release_provenance')
    mac = record['platforms'][platform]
    feed.require(all(mac.get(k) == record[k] for k in ('version', 'build', 'built_commit', 'source_tree_sha256')) and
                 mac.get('source_commit') is None and mac['notarized'] is True and
                 mac['team_id'] == '893V37X2CW' and
                 all(mac['notarization'][k]['status'] == 'Accepted' and mac['notarization'][k]['stapled'] is True
                     for k in ('app', 'dmg')) and
                 mac['launch_smoke']['status'] == 'passed' and mac['launch_smoke']['signed'] is True and
                 mac['launch_smoke']['seconds'] == 10 and
                 (record['build'] >= SPARKLE_FREE_BUILD or mac['sparkle']['public_key_configured'] is True),
                 'notarized_signed_package_required')
    published_release.validate_annotations(annotations)
    # The ZIP serves only v1 Sparkle appcasts, which carry stable releases only.
    return artifact(record, platform, 'zip') if annotations['channel'] == 'stable' else None


def artifact(record, platform, suffix):
    matches = [a for a in record['platforms'][platform]['artifacts'] if a['name'].endswith('.' + suffix)]
    feed.require(len(matches) == 1, 'sparkle_archive_required' if suffix == 'zip' else 'dmg_required')
    asset = matches[0]
    feed.require(asset in record['artifacts'] and re.fullmatch(r'[A-Za-z0-9_.-]+\.' + suffix, asset['name']) and
                 type(asset['size']) is int and 0 < asset['size'] <= feed.MAX_PACKAGE and
                 re.fullmatch('[0-9a-f]{64}', asset['sha256']), 'archive_metadata')
    return asset


def verify_dmg(dmg, directory, record, native_arch):
    """The DMG v2 clients install: Developer ID + stapled ticket, and the same app identity."""
    from signing import verify_published_app, verify_published_dmg
    verify_published_dmg(dmg, '893V37X2CW')
    mount = directory / 'dmg.noindex'
    mounted = plistlib.loads(command(['hdiutil', 'attach', dmg, '-readonly', '-nobrowse', '-noautoopen',
                                      '-mountpoint', mount, '-plist']))
    try:
        app = mount / 'Oink.app'
        info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
        feed.require(info['CFBundleIdentifier'] == 'com.hachiwii.oink' and
                     info['CFBundleVersion'] == str(record['build']) and
                     info['CFBundleShortVersionString'] == record['version'], 'dmg_bundle_identity')
        arches = command(['lipo', '-archs', app / 'Contents/MacOS/oink-backend']).decode().split()
        feed.require(arches == [native_arch], 'dmg_architecture')
        verify_published_app(app, '893V37X2CW')
    finally:
        device = next(row['dev-entry'] for row in mounted['system-entities'] if row.get('mount-point'))
        command(['hdiutil', 'detach', device])


def v2_record(record, platform, architecture, tag, notes, channel, dmg, url, signature, epoch):
    mac = record['platforms'][platform]
    feed.require(mac['protocol_baseline'] == 1, 'release_provenance')
    release = {'build': record['build'], 'version': record['version'], 'tag': tag, 'channel': channel,
               'sourceTreeSha256': record['source_tree_sha256'], 'minimumOs': mac['minimum_os'],
               'protocolBaseline': 1, 'minimumUpdaterSchema': feed_v2.UPDATER_CAPABILITY, 'notes': notes,
               'package': {'url': url, 'length': dmg['size'], 'sha256': dmg['sha256'], 'keyId': epoch,
                           'ed25519': signature}}
    # Mac DMG installs exist only in v2 clients (updater capability 3).
    return {'schema': 1, 'architecture': architecture, 'developer_id_verified': True, 'notarized': True,
            'qualified_build': record['build'], 'release': release}


def sign(tool, path, public, *, xml=False):
    secret = os.environ.get(SECRET)
    feed.require(bool(secret), 'sparkle_secret_unavailable')
    # The official tool accepts '-' for the key, avoiding argv, Keychain setup
    # and private files. Capture ALL output, including errors, before checking.
    raw = command([tool, '--ed-key-file', '-', '-p', path], input=secret.encode())
    if xml:
        # The final key-less merge verifies the appcast signature once.
        return None
    signature = raw.decode().strip()
    feed.verify(path.read_bytes(), signature, public)
    return signature


def write_appcast(path, previous, metadata, signature, notes):
    if previous is None:
        root = ET.Element('rss', {'version': '2.0'})
        channel = ET.SubElement(root, 'channel')
        ET.SubElement(channel, 'title').text = 'Oink updates'
    else:
        root = ET.fromstring(previous)
        channel = root.find('channel')
        feed.require(channel is not None, 'appcast_channel')
    ET.register_namespace('sparkle', appcast.SPARKLE)
    ET.register_namespace('oink', appcast.OINK)
    ET.register_namespace('oink', appcast.OINK)
    item = ET.Element('item')
    ET.SubElement(item, 'title').text = 'Oink ' + metadata['version']
    ET.SubElement(item, f'{{{appcast.SPARKLE}}}version').text = str(metadata['build'])
    ET.SubElement(item, f'{{{appcast.SPARKLE}}}shortVersionString').text = metadata['version']
    ET.SubElement(item, f'{{{appcast.OINK}}}sha256').text = metadata['sha256']
    ET.SubElement(item, f'{{{appcast.SPARKLE}}}minimumSystemVersion').text = '14.2'
    ET.SubElement(item, 'enclosure', {'url': metadata['url'], 'length': str(metadata['length']),
                  'type': 'application/octet-stream', f'{{{appcast.SPARKLE}}}edSignature': signature})
    channel.insert(0, item)
    feed.write(path, ET.tostring(root, encoding='utf-8', xml_declaration=True))
    appcast.embed_notes(path, metadata['build'], notes)


def sign_v1(record, asset, base, config, catalog, output, directory, signer, notes, architecture, native_arch, arch_path):
    relative = f'mac/v1/{config["mac"]["epoch"]}/{arch_path}stable/appcast.xml'
    if relative in catalog:
        feed.require(record['build'] > catalog[relative]['build'], 'release_not_monotonic')
    archive = directory / asset['name']
    raw = feed.download(base + asset['name'], feed.MAX_PACKAGE, redirects=True)
    feed.require(len(raw) == asset['size'] and feed.digest(raw) == asset['sha256'], 'archive_digest')
    archive.write_bytes(raw)
    del raw
    # These exact stapled bytes came from the gated package run. Recheck
    # their native signature/ticket before granting a new update signature.
    staging = directory / 'staging.noindex'
    command(['ditto', '-x', '-k', archive, staging])
    app = staging / 'Oink.app'
    from signing import verify_published_app
    from content import verify_update_transport
    from macos_imports import verify_imports
    info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
    feed.require(info['CFBundleIdentifier'] == 'com.hachiwii.oink' and
                 info['CFBundleVersion'] == str(record['build']) and
                 info['CFBundleShortVersionString'] == record['version'] and
                 (record['build'] >= SPARKLE_FREE_BUILD or info['SUPublicEDKey'] == config['mac']['public_key']),
                 'archive_bundle_identity')
    arches = command(['lipo', '-archs', app / 'Contents/MacOS/oink-backend']).decode().split()
    feed.require(arches == [native_arch], 'archive_architecture')
    # ATS stays forbidden for every build; the SUFeedURL check applies only
    # when the key exists (Sparkle-free apps from 0.3.6 have none).
    verify_update_transport(info)
    verify_imports([app], '14.2')
    verify_published_app(app, '893V37X2CW')
    signature = sign(signer, archive, config['mac']['public_key'])
    metadata = dict(architecture=architecture, build=record['build'], version=record['version'],
                    url=base + asset['name'], length=asset['size'], sha256=asset['sha256'],
                    developer_id_verified=True, notarized=True, qualified_build=record['build'])
    # Stable updates are eligible for beta subscribers too. Keep a newer
    # beta feed byte-for-byte if a stable promotion is older/equal; never
    # strand a persisted beta preference on a missing/obsolete feed.
    for channel in ('stable', 'beta'):
        relative = f'mac/v1/{config["mac"]["epoch"]}/{arch_path}{channel}/appcast.xml'
        if channel == 'beta' and catalog.get(relative, {}).get('build', 0) >= record['build']:
            continue
        path = output / 'site' / relative
        previous = path.read_bytes() if path.exists() else None
        write_appcast(path, previous, metadata, signature, notes)
        sign(signer, path, config['mac']['public_key'], xml=True)
        catalog[relative] = metadata


def candidate(state, output, cache, tag):
    live = published(tag)
    config = feed.trust('mac')
    feed.require(config['mac']['enabled'], 'mac_updates_disabled')
    commit, tree, built_trust = tag_source(tag)
    feed.require(built_trust['enabled'] and built_trust['public_key'] == config['mac']['public_key'] and
                 built_trust['epoch'] == config['mac']['epoch'], 'release_trust_changed')
    base = f'https://github.com/{config["repository"]}/releases/download/{tag}/'
    record = feed.loads(feed.download(base + 'release.json', feed.MAX_FEED, redirects=True))
    # The channel is the live prerelease flag; notes come from the trusted checkout.
    annotations = published_release.annotations(live)
    assets = {platform: validate_release(record, annotations, tag, commit, tree, platform)
              for platform in ('macos', 'macos-x64') if platform in record['platforms']}
    feed.require('macos' in assets, 'sparkle_archive_required')
    reg = feed_site.state_info(state)
    feed_site.verify_platform(state, 'mac', config, signatures=False)
    catalog = reg['platforms']['mac'].get('catalog', {}).copy()
    output.mkdir(parents=True, exist_ok=False)
    for name, raw in feed_site.files(state / 'site/mac').items():
        feed.write(output / 'site/mac' / name, raw)
    stable = annotations['channel'] == 'stable'
    sys.path.insert(0, str(feed.ROOT / 'packaging/macos'))
    signer = prepare(cache) / 'bin/sign_update'
    for platform in ('macos', 'macos-x64'):
        # Historical arm-only releases remain publishable; new Intel records
        # receive an independent catalog and never enter the existing ARM feed.
        if platform not in record['platforms']:
            continue
        architecture = 'x64' if platform == 'macos-x64' else 'arm64'
        native_arch = 'x86_64' if architecture == 'x64' else 'arm64'
        arch_path = 'x64/' if architecture == 'x64' else ''
        feed.require(record['platforms'][platform]['architecture'] == architecture, 'archive_architecture')
        with tempfile.TemporaryDirectory(prefix='oink-mac-release-') as tmp:
            directory = Path(tmp)
            # v1 (Sparkle appcasts for <= 0.3.5 clients) carries stable releases only.
            if stable:
                sign_v1(record, assets[platform], base, config, catalog, output, directory, signer,
                        annotations['notes'], architecture, native_arch, arch_path)
            # v2 installs the notarized DMG. Its package signature is the same
            # Sparkle Ed25519 signature over the exact final DMG bytes.
            dmg = artifact(record, platform, 'dmg')
            image = directory / dmg['name']
            raw = feed.download(base + dmg['name'], feed.MAX_PACKAGE, redirects=True)
            feed.require(len(raw) == dmg['size'] and feed.digest(raw) == dmg['sha256'], 'dmg_digest')
            image.write_bytes(raw)
            del raw
            verify_dmg(image, directory, record, native_arch)
            path = output / 'site/mac/releases' / str(record['build']) / f'{architecture}.json'
            existing = {int(p.parent.name): feed.read(p)
                        for p in (output / 'site/mac/releases').glob(f'*/{architecture}.json')}
            previous = existing.get(record['build'])
            new = v2_record(record, platform, architecture, tag, annotations['notes'], annotations['channel'],
                            dmg, base + dmg['name'], sign(signer, image, config['mac']['public_key']),
                            config['mac']['epoch'])
            if previous is not None and previous['release']['channel'] != annotations['channel']:
                # Promotion: the build published from the prerelease moves to
                # stable unchanged (one record per build; never stable -> beta).
                feed.require(previous['release']['channel'] == 'beta' and stable and
                             previous == {**new, 'release': {**new['release'], 'channel': 'beta'}},
                             'release_build_changed')
            else:
                feed.require(previous is None, 'release_not_monotonic')
            # A stable hotfix may follow a newer beta; each channel only moves forward.
            feed.require(all(build < record['build'] for build, other in existing.items()
                             if build != record['build'] and other['release']['channel'] == annotations['channel']),
                         'release_not_monotonic')
            feed.write(path, new)
    feed.write(output / 'catalog.json', catalog)
    return record['build']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--release-tag', required=True)
    parser.add_argument('--state', type=Path, default=Path('feed-state'))
    parser.add_argument('--output', type=Path, default=Path('mac-input'))
    parser.add_argument('--tool-cache', type=Path, required=True)
    a = parser.parse_args()
    try:
        build = candidate(a.state, a.output, a.tool_cache, a.release_tag)
        with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
            stream.write(f'build={build}\n')
    except Exception as error:
        message = str(error) if type(error) is ValueError and re.fullmatch('[a-z_0-9]+', str(error)) else 'mac_release_failed'
        raise SystemExit(message) from None


if __name__ == '__main__': main()
