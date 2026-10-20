#!/usr/bin/env python3
"""Key-less platform snapshot merge. The deployment job holds the shared writer lock.

A candidate fences its platform revision; the final Git push fences the complete
state revision. Signing can run concurrently. No code is loaded from feed-state.
"""
import argparse
import base64
import copy
import os
from pathlib import Path
import re
import shutil
import tempfile
import xml.etree.ElementTree as ET
import feed

PLATFORMS = ('mac', 'windows')
SPARKLE = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
OINK = 'https://updates.oink.hachiwii.com/xml-namespaces/update'


def files(folder):
    result = {}
    for path in folder.rglob('*'):
        feed.require(not path.is_symlink(), 'site_symlink')
        if path.is_file():
            name = path.relative_to(folder).as_posix()
            feed.require(all(re.fullmatch('[A-Za-z0-9_.-]+', part) for part in path.relative_to(folder).parts), 'site_path')
            result[name] = path.read_bytes()
    return result


def state_info(state):
    reg = feed.registry(state)
    feed.require(type(reg['revision']) is int and reg['revision'] >= 0, 'state_revision')
    feed.require(set(reg['platforms']) == set(PLATFORMS), 'platform_state')
    for platform in PLATFORMS:
        slot = reg['platforms'][platform]
        feed.require(type(slot['enabled']) is bool and type(slot['revision']) is int and slot['revision'] >= 0, 'platform_state')
        if not slot['enabled']:
            feed.require(not slot['files'] and not files(state / 'site' / platform), 'disabled_feed_present')
    return reg


def appcast(raw, config, channel, verify_signature=True):
    # Sparkle 2.9.6 signs the exact XML bytes before this terminal comment.
    match = re.search(rb'<!-- sparkle-signatures:\nedSignature: ([A-Za-z0-9+/=]+)\nlength: ([0-9]+)\n-->\n\Z', raw)
    feed.require(match is not None and len(raw) <= feed.MAX_FEED, 'appcast_signature')
    content = raw[:match.start()]
    feed.require(int(match[2]) == len(content), 'appcast_length')
    if verify_signature:
        feed.verify(content, match[1].decode(), config['mac']['public_key'])
    feed.require(b'<!DOCTYPE' not in content and b'<!ENTITY' not in content, 'appcast_xml')
    root = ET.fromstring(content)
    feed.require(root.tag == 'rss', 'appcast_xml')
    entries = []
    for item in root.findall('./channel/item'):
        enclosure = item.find('enclosure')
        feed.require(enclosure is not None, 'appcast_package')
        build = item.findtext(f'{{{SPARKLE}}}version') or enclosure.get(f'{{{SPARKLE}}}version')
        version = item.findtext(f'{{{SPARKLE}}}shortVersionString') or enclosure.get(f'{{{SPARKLE}}}shortVersionString')
        feed.require(build and build.isdecimal() and 0 < int(build) < 2**31 and version, 'appcast_build')
        item_channel = item.findtext(f'{{{SPARKLE}}}channel')
        feed.require(item_channel in (None, 'beta') and (channel == 'beta' or item_channel is None), 'appcast_channel')
        feed.validate_notes(feed.loads(item.findtext(f'{{{OINK}}}releaseNotes', '').encode()))
        url, length = enclosure.get('url', ''), int(enclosure.get('length', '0'))
        prefix = f'https://github.com/{config["repository"]}/releases/download/'
        feed.require(url.startswith(prefix) and re.fullmatch(r'v[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\.(zip|dmg)', url[len(prefix):]) and
                     0 < length <= feed.MAX_PACKAGE, 'asset')
        signature = enclosure.get(f'{{{SPARKLE}}}edSignature', '')
        sha256 = item.findtext(f'{{{OINK}}}sha256', '')
        feed.require(re.fullmatch('[0-9a-f]{64}', sha256), 'asset_digest')
        entry = {'build': int(build), 'version': version, 'url': url, 'length': length, 'sha256': sha256}
        entries.append(entry)
    feed.require(entries and len({e['build'] for e in entries}) == len(entries), 'appcast_catalog')
    return entries


def verify_platform(state, platform, config, packages=False, signatures=True, verified_files=None):
    reg = state_info(state)
    slot = reg['platforms'][platform]
    if not slot['enabled']:
        return
    enabled = config['mac']['enabled'] if platform == 'mac' else bool(config['windows'])
    feed.require(enabled, 'enabled_platform_missing_trust')
    actual = {platform + '/' + name: feed.digest(raw) for name, raw in files(state / 'site' / platform).items()}
    feed.require(actual == slot['files'] and actual, 'platform_digest')
    if platform == 'windows':
        manifests = {feed.relative(e['epoch'], c) for e in config['windows'] for c in e['channels']}
        for path in set(actual) - manifests:
            match = re.fullmatch(r'windows/releases/([1-9][0-9]*)/release.json', path)
            feed.require(match is not None, 'windows_feed_path')
            raw = (state / 'site' / path).read_bytes()
            record = feed.loads(raw)
            feed.require(record['release']['build'] == int(match[1]) and
                         (state / 'records' / (feed.digest(raw) + '.json')).read_bytes() == raw, 'record_digest')
        feed.verify_site(state, config)
        if packages:
            feed.verify_packages(state, config)
    else:
        feed.require(set(slot['catalog']) == set(actual), 'mac_catalog')
        verified = set()
        for path, record in slot['catalog'].items():
            match = re.fullmatch(r'mac/v1/mac-2026-01/(stable|beta)/appcast.xml', path)
            feed.require(match and record['architecture'] == 'arm64' and
                         record['developer_id_verified'] is True and record['notarized'] is True and
                         record['qualified_build'] == record['build'], 'mac_qualification')
            raw = (state / 'site' / path).read_bytes()
            entries = appcast(raw, config, match[1], signatures and actual[path] != (verified_files or {}).get(path) and raw not in verified)
            verified.add(raw)
            latest = max(entries, key=lambda entry: entry['build'])
            feed.require(all(record[k] == v for k, v in latest.items()) and re.fullmatch('[0-9a-f]{64}', record['sha256']), 'mac_catalog')


def verify_all(state, packages=False, signatures=True, verified_mac_files=None):
    for platform in PLATFORMS:
        # An absent platform needs no keys and is never synthesized.
        if state_info(state)['platforms'][platform]['enabled']:
            verify_platform(state, platform, feed.trust(platform), packages, signatures, verified_mac_files if platform == 'mac' else None)


def candidate(state, platform, output, catalog=None):
    reg = feed.registry(state)
    slot = copy.deepcopy(reg['platforms'][platform])
    slot['enabled'] = True
    slot['files'] = {platform + '/' + name: feed.digest(raw) for name, raw in files(state / 'site' / platform).items()}
    if platform == 'mac':
        slot['catalog'] = feed.read(catalog)
        previous = reg['platforms']['mac']
        for path, record in slot['catalog'].items():
            unchanged = (slot['files'].get(path) == previous['files'].get(path) and
                         record == previous.get('catalog', {}).get(path))
            feed.require(unchanged or str(record['qualified_build']) == os.environ.get('OINK_UPDATE_QUALIFIED_BUILD'), 'update_not_qualified')
    proposal = {'schema': 1, 'platform': platform, 'base_revision': slot['revision'], 'slot': slot}
    output.mkdir(parents=True, exist_ok=False)
    shutil.copytree(state / 'site' / platform, output / 'site' / platform)
    if platform == 'windows':
        proposal['feeds'] = reg['feeds']
        shutil.copytree(state / 'records', output / 'records')
    # Verify in an isolated state whose only enabled platform is this candidate.
    isolated = copy.deepcopy(reg)
    isolated['platforms'][platform] = slot
    other = next(p for p in PLATFORMS if p != platform)
    isolated['platforms'][other] = {'enabled': False, 'revision': 0, 'files': {}}
    feed.write(output / 'registry.json', isolated)
    verify_platform(output, platform, feed.trust(platform), packages=True, signatures=platform != 'mac')
    (output / 'registry.json').unlink()
    feed.write(output / 'candidate.json', proposal)


def merge(state, proposal_dir, output, expected_revision):
    reg = state_info(state)
    feed.require(reg['revision'] == expected_revision, 'state_cas')
    proposal = feed.read(proposal_dir / 'candidate.json')
    platform = proposal['platform']
    feed.require(proposal['schema'] == 1 and platform in PLATFORMS, 'candidate_platform')
    old, slot = reg['platforms'][platform], proposal['slot']
    feed.require(proposal['base_revision'] == old['revision'] == slot['revision'], 'platform_cas')
    feed.require(slot['enabled'] is True and slot['files'], 'implicit_delete')
    feed.require(set(old['files']).issubset(slot['files']), 'implicit_delete')
    feed.require(set(files(proposal_dir / 'site')) == set(slot['files']) and
                 all(path.startswith(platform + '/') for path in slot['files']), 'candidate_prefix')
    # Windows allocations are durable before signing; reject a superseded reservation.
    if platform == 'windows':
        for epoch, channels in proposal['feeds'].items():
            for channel, new in channels.items():
                current = reg['feeds'][epoch][channel]
                feed.require(new['last_allocated'] == current['last_allocated'] and new['records'] == current['records'], 'reservation_changed')
    output.mkdir(parents=True, exist_ok=False)
    for directory in ('site', 'records'):
        # Git cannot retain an empty records directory. A Mac-only first
        # deployment has no Windows records yet, but deploy.commit copies the
        # complete snapshot including that directory.
        (output / directory).mkdir()
        for name, raw in files(state / directory).items():
            feed.write(output / directory / name, raw)
    for path, raw in files(proposal_dir / 'site').items():
        feed.write(output / 'site' / path, raw)
    if platform == 'windows':
        reg['feeds'] = proposal['feeds']
        for name, raw in files(proposal_dir / 'records').items():
            feed.require(re.fullmatch('[0-9a-f]{64}.json', name) and feed.digest(raw) == name[:-5], 'record_digest')
            feed.write(output / 'records' / name, raw)
        # Signed sequence must strictly increase whenever bytes change.
        for epoch, channels in reg['feeds'].items():
            for channel in channels:
                path = feed.relative(epoch, channel)
                if path in old['files'] and old['files'][path] != slot['files'][path]:
                    before = feed.loads(base64.b64decode(feed.read(state / 'site' / path)['payload']))
                    after = feed.loads(base64.b64decode(feed.read(output / 'site' / path)['payload']))
                    feed.require(after['sequence'] > before['sequence'], 'stale_candidate')
    else:
        for path, previous in old.get('catalog', {}).items():
            if old['files'][path] != slot['files'][path]:
                feed.require(slot['catalog'][path]['build'] > previous['build'], 'stale_candidate')
    slot['revision'] += 1
    reg['platforms'][platform] = slot
    reg['revision'] += 1
    feed.write(output / 'registry.json', reg)
    # The new Mac archive was verified by the signer. Its hash is bound into
    # the signed appcast. Verify that appcast once here, at the final write
    # boundary; untouched platforms and HTTPS readback need only exact digests.
    verify_all(output, packages=platform == 'windows', signatures=platform == 'mac',
               verified_mac_files=old['files'] if platform == 'mac' else None)
    feed.write(output / 'site/CNAME', b'updates.oink.hachiwii.com\n')
    feed.write(output / 'site/heartbeat.json', {'schema': 1, 'revision': reg['revision'],
               'checked_at': feed.iso(int(feed.dt.datetime.now(feed.dt.timezone.utc).timestamp()))})


def verify_public(state, platform=None):
    if platform is None:
        verify_all(state, signatures=False)
    else:
        verify_platform(state, platform, feed.trust(platform), signatures=False)
    for path, raw in files(state / 'site').items():
        if platform is not None and not path.startswith(platform + '/'):
            continue
        feed.require(feed.download(feed.BASE + path, max(feed.MAX_FEED, len(raw)), False, True) == raw, 'deployment_digest')
    verify_absent(state_info(state), PLATFORMS if platform is None else (platform,))


def verify_absent(reg, platforms=PLATFORMS):
    # A successful rollback must also remove a newly introduced platform prefix.
    import urllib.error
    for platform in platforms:
        if not reg['platforms'][platform]['enabled']:
            epoch = 'mac-2026-01' if platform == 'mac' else 'win-2026-01'
            filename = 'appcast.xml' if platform == 'mac' else 'manifest.json'
            for channel in ('stable', 'beta'):
                try:
                    feed.download(f'{feed.BASE}{platform}/v1/{epoch}/{channel}/{filename}', feed.MAX_FEED, False, True)
                except urllib.error.HTTPError as error:
                    try:
                        feed.require(error.code == 404, 'disabled_feed_status')
                    finally:
                        error.close()
                else:
                    raise ValueError('disabled_feed_present')


def initialize(output):
    """Prepare data locally; the user creates/pushes the data branch separately."""
    output.mkdir(parents=True, exist_ok=False)
    feed.write(output / 'registry.json', {
        'schema': 1, 'revision': 0,
        'platforms': {p: {'enabled': False, 'revision': 0, 'files': {}} for p in PLATFORMS},
        'feeds': {'win-2026-01': {c: {'records': [], 'last_allocated': 0, 'manifest_sha256': None}
                                 for c in ('stable', 'beta')}}})
    feed.write(output / 'site/CNAME', b'updates.oink.hachiwii.com\n')
    feed.write(output / 'site/index.html', b'<!doctype html><title>Oink updates</title><p>Oink update feeds.</p>\n')


def bootstrap(state, output):
    """Deploy only the original disabled landing site, before HTTPS readback works.

    Normal publication requires HTTPS readback before committing its state. This
    one-time landing deployment lets Pages provision the site without that
    dependency; it cannot replace a live feed or advance the data branch.
    """
    with tempfile.TemporaryDirectory() as temp:
        initial = Path(temp) / 'initial'
        initialize(initial)
        feed.require(feed.read(state / 'registry.json') == feed.read(initial / 'registry.json'),
                     'bootstrap_noninitial_state')
        feed.require(files(state / 'site') == files(initial / 'site') and not files(state / 'records'),
                     'bootstrap_noninitial_site')
        shutil.copytree(state / 'site', output)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('command', choices=['init', 'bootstrap', 'candidate', 'merge', 'verify', 'verify-public', 'enabled'])
    parser.add_argument('--platform', choices=PLATFORMS)
    parser.add_argument('--state', type=Path, default=Path('feed-state'))
    parser.add_argument('--candidate', type=Path, default=Path('candidate-state'))
    parser.add_argument('--output', type=Path, default=Path('next-state'))
    parser.add_argument('--catalog', type=Path)
    parser.add_argument('--revision', type=int)
    parser.add_argument('--require-catalog', action='store_true')
    parser.add_argument('--config-only', action='store_true')
    args = parser.parse_args()
    if args.command == 'init':
        initialize(args.output)
    elif args.command == 'bootstrap':
        bootstrap(args.state, args.output)
    elif args.command == 'enabled':
        config = feed.trust(args.platform)
        enabled = config['mac']['enabled'] if args.platform == 'mac' else bool(config['windows'])
        if not args.config_only:
            slot = state_info(args.state)['platforms'][args.platform]
            feed.require(enabled or not slot['enabled'], 'enabled_platform_missing_trust')
            if args.require_catalog:
                enabled = enabled and slot['enabled']
        print('true' if enabled else 'false')
    elif args.command == 'candidate': candidate(args.state, args.platform, args.output, args.catalog)
    elif args.command == 'merge': merge(args.state, args.candidate, args.output, args.revision)
    elif args.command == 'verify': verify_all(args.state)
    else: verify_public(args.state, args.platform)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        code = str(error) if type(error) is ValueError and re.fullmatch('[a-z_0-9]+', str(error)) else 'site_operation_failed'
        raise SystemExit(code)
