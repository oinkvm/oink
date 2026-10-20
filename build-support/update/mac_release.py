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
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from provenance import source_tree_sha256
from sparkle import prepare
spec = importlib.util.spec_from_file_location('mac_appcast', Path(__file__).with_name('mac-appcast.py'))
appcast = importlib.util.module_from_spec(spec)
spec.loader.exec_module(appcast)

SECRET = 'SPARKLE_ED25519_PRIVATE_KEY'


def command(argv, **kwargs):
    result = subprocess.run(list(map(str, argv)), capture_output=True, **kwargs)
    feed.require(result.returncode == 0, Path(argv[0]).name.replace('-', '_') + '_failed')
    return result.stdout


def tag_source(tag):
    feed.require(re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+', tag), 'release_tag')
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
    event = feed.read(Path(os.environ['GITHUB_EVENT_PATH']))
    release = event.get('release', {})
    feed.require(os.environ.get('GITHUB_EVENT_NAME') == 'release' and
                 os.environ.get('GITHUB_REPOSITORY') == 'oinkvm/oink' and
                 event.get('action') == 'published' and release.get('draft') is False and
                 release.get('prerelease') is False and release.get('tag_name') == tag,
                 'published_release_required')
    current = feed.loads(command(['gh', 'api', 'repos/oinkvm/oink/releases/tags/' + tag]))
    feed.require(current['id'] == release['id'] and current['tag_name'] == tag and
                 current['draft'] is False and current['prerelease'] is False, 'release_changed')


def validate_release(record, annotations, tag, commit, tree):
    feed.require(record['schema'] == 1 and record['product'] == 'Oink' and
                 record['version'] == tag.removeprefix('v') and record['built_commit'] == commit and
                 record['source_tree_sha256'] == tree and type(record['build']) is int and
                 0 < record['build'] < 2**31, 'release_provenance')
    mac = record['platforms']['macos']
    feed.require(all(mac.get(k) == record[k] for k in ('version', 'build', 'built_commit', 'source_tree_sha256')) and
                 mac.get('source_commit') is None and mac['notarized'] is True and
                 mac['team_id'] == '893V37X2CW' and
                 all(mac['notarization'][k]['status'] == 'Accepted' and mac['notarization'][k]['stapled'] is True
                     for k in ('app', 'dmg')) and
                 mac['launch_smoke']['status'] == 'passed' and mac['launch_smoke']['signed'] is True and
                 mac['launch_smoke']['seconds'] == 10 and mac['sparkle']['public_key_configured'] is True,
                 'notarized_signed_package_required')
    feed.require(set(annotations) == {'channel', 'severity', 'notes'} and
                 annotations['channel'] == 'stable' and annotations['severity'] in ('normal', 'critical'),
                 'stable_annotations_required')
    feed.validate_notes(annotations['notes'])
    archives = [a for a in mac['artifacts'] if a['name'].endswith('.zip')]
    feed.require(len(archives) == 1, 'sparkle_archive_required')
    asset = archives[0]
    feed.require(asset in record['artifacts'] and re.fullmatch(r'[A-Za-z0-9_.-]+\.zip', asset['name']) and
                 type(asset['size']) is int and 0 < asset['size'] <= feed.MAX_PACKAGE and
                 re.fullmatch('[0-9a-f]{64}', asset['sha256']), 'archive_metadata')
    return asset


def sign(tool, path, public, *, xml=False):
    secret = os.environ.get(SECRET)
    feed.require(secret is not None, 'sparkle_secret_unavailable')
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


def candidate(state, output, cache, tag):
    published(tag)
    config = feed.trust('mac')
    feed.require(config['mac']['enabled'], 'mac_updates_disabled')
    commit, tree, built_trust = tag_source(tag)
    feed.require(built_trust['enabled'] and built_trust['public_key'] == config['mac']['public_key'] and
                 built_trust['epoch'] == config['mac']['epoch'], 'release_trust_changed')
    base = f'https://github.com/{config["repository"]}/releases/download/{tag}/'
    record = feed.loads(feed.download(base + 'release.json', feed.MAX_FEED, redirects=True))
    annotation_bytes = feed.download(base + 'update.json', feed.MAX_FEED, redirects=True)
    annotations = feed.loads(annotation_bytes)
    annotation_asset = {'name': 'update.json', 'size': len(annotation_bytes), 'sha256': feed.digest(annotation_bytes)}
    feed.require(annotation_asset in record['artifacts'], 'annotations_digest')
    asset = validate_release(record, annotations, tag, commit, tree)
    reg = feed_site.state_info(state)
    feed_site.verify_platform(state, 'mac', config, signatures=False)
    relative = f'mac/v1/{config["mac"]["epoch"]}/stable/appcast.xml'
    catalog = reg['platforms']['mac'].get('catalog', {}).copy()
    if relative in catalog:
        feed.require(record['build'] > catalog[relative]['build'], 'release_not_monotonic')
    output.mkdir(parents=True, exist_ok=False)
    for name, raw in feed_site.files(state / 'site/mac').items():
        feed.write(output / 'site/mac' / name, raw)
    with tempfile.TemporaryDirectory(prefix='oink-mac-release-') as tmp:
        directory = Path(tmp)
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
        sys.path.insert(0, str(feed.ROOT / 'packaging/macos'))
        from signing import verify_published_app
        from content import verify_update_transport
        from macos_imports import verify_imports
        info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
        feed.require(info['CFBundleIdentifier'] == 'com.hachiwii.oink' and
                     info['CFBundleVersion'] == str(record['build']) and
                     info['CFBundleShortVersionString'] == record['version'] and
                     info['SUPublicEDKey'] == config['mac']['public_key'], 'archive_bundle_identity')
        verify_update_transport(info)
        verify_imports([app], '14.2')
        verify_published_app(app, '893V37X2CW')
        signer = prepare(cache) / 'bin/sign_update'
        signature = sign(signer, archive, config['mac']['public_key'])
        metadata = dict(architecture='arm64', build=record['build'], version=record['version'],
                        url=base + asset['name'], length=asset['size'], sha256=asset['sha256'],
                        developer_id_verified=True, notarized=True, qualified_build=record['build'])
        # Stable updates are eligible for beta subscribers too. Keep a newer
        # beta feed byte-for-byte if a stable promotion is older/equal; never
        # strand a persisted beta preference on a missing/obsolete feed.
        for channel in ('stable', 'beta'):
            relative = f'mac/v1/{config["mac"]["epoch"]}/{channel}/appcast.xml'
            if channel == 'beta' and catalog.get(relative, {}).get('build', 0) >= record['build']:
                continue
            path = output / 'site' / relative
            previous = path.read_bytes() if path.exists() else None
            write_appcast(path, previous, metadata, signature, annotations['notes'])
            sign(signer, path, config['mac']['public_key'], xml=True)
            catalog[relative] = metadata
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
