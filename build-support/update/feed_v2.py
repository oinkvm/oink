#!/usr/bin/env python3
"""Feed v2 writer: one signed manifest per (platform, epoch, architecture, channel).

The envelope is v1's: pure Ed25519 over the exact payload bytes, 30-day expiry,
sequence reserved durably in registry['v2'] (reserve.py) before signing, never
reused. Windows signs with its epoch key through OpenSSL. Mac signs with the
existing Sparkle key through the pinned official sign_update: the secret may be
Sparkle's 32-byte seed or its legacy 96-byte expanded key, which OpenSSL cannot
load. Every signature is verified with OpenSSL before it is written.

Releases come from the public per-build records in the site, which only a
merged candidate changes: windows/releases/ (the registered v1 records, the same
signed MSI package objects) and mac/releases/. Rules come from
packaging/update-rules.json (rules.py).
No command accepts a private key, URL, epoch override or release catalog on argv.
"""
import argparse
import base64
import os
from pathlib import Path
import re
import tempfile
import feed
import rules as rule_compiler

PRODUCT = 'com.hachiwii.oink'
UPDATER_CAPABILITY = 3
MAC_EPOCH = 'mac-2026-01'
MAC_ARCHES = ('arm64', 'x64')
MAC_SECRET = 'SPARKLE_ED25519_PRIVATE_KEY'
CHANNELS = ('stable', 'beta')
TTL = 30 * 86400
RENEW_BEFORE = 21 * 86400
MAX_RELEASES = 32
PATH = re.compile(r'(windows|mac)/v2/([a-z0-9-]+)/(x64|arm64)/(stable|beta)/manifest[.]json')
MAC_RECORD = re.compile(r'mac/releases/([1-9][0-9]*)/(arm64|x64)[.]json')
PAYLOAD_KEYS = ['schema', 'product', 'platform', 'keyEpoch', 'architecture', 'channel', 'sequence',
                'generatedAt', 'expiresAt', 'releases', 'rules']
RELEASE_KEYS = ['build', 'version', 'tag', 'channel', 'sourceTreeSha256', 'minimumOs', 'protocolBaseline',
                'minimumUpdaterSchema', 'notes', 'package']
PACKAGE_KEYS = ['url', 'length', 'sha256', 'keyId', 'ed25519']


def now():
    return int(feed.dt.datetime.now(feed.dt.timezone.utc).timestamp())


def relative(platform, epoch, arch, channel):
    feed.require(((platform == 'windows' and re.fullmatch(r'win-[0-9]{4}-[0-9]{2}', epoch) and arch == 'x64') or
                  (platform == 'mac' and epoch == MAC_EPOCH and arch in MAC_ARCHES)) and channel in CHANNELS, 'feed_path')
    return f'{platform}/v2/{epoch}/{arch}/{channel}/manifest.json'


def slots(platform, config):
    """(epoch, public key, architecture, channel) of every v2 manifest an enabled platform publishes."""
    if platform == 'windows':
        feed.require(config['windows'], 'platform_disabled')
        return [(e['epoch'], e['public_key'], 'x64', c) for e in config['windows'] for c in e['channels']]
    feed.require(platform == 'mac' and config['mac']['enabled'], 'platform_disabled')
    return [(config['mac']['epoch'], config['mac']['public_key'], a, c) for a in MAC_ARCHES for c in CHANNELS]


def validate_release(release, platform, epoch, repository):
    feed.require(type(release) is dict and set(release) == set(RELEASE_KEYS), 'release_shape')
    package = release['package']
    feed.require(type(release['build']) is int and 0 < release['build'] < 2**31 and
                 type(release['version']) is str and
                 re.fullmatch(r'(0|[1-9][0-9]*)[.](0|[1-9][0-9]*)[.](0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?', release['version']) and
                 type(release['tag']) is str and re.fullmatch(r'v[A-Za-z0-9_.-]+', release['tag']) and
                 release['channel'] in CHANNELS and
                 type(release['sourceTreeSha256']) is str and re.fullmatch('[0-9a-f]{64}', release['sourceTreeSha256']) and
                 type(release['minimumOs']) is str and
                 re.fullmatch(r'10[.]0[.][1-9][0-9]*' if platform == 'windows' else r'[1-9][0-9]*[.][0-9]+([.][0-9]+)?',
                              release['minimumOs']) and
                 release['protocolBaseline'] == 1 and type(release['protocolBaseline']) is int and
                 type(release['minimumUpdaterSchema']) is int and release['minimumUpdaterSchema'] >= 1, 'release_metadata')
    feed.validate_notes(release['notes'])
    feed.require(type(package) is dict and set(package) == set(PACKAGE_KEYS), 'release_shape')
    prefix = f'https://github.com/{repository}/releases/download/{release["tag"]}/'
    suffix = 'msi' if platform == 'windows' else 'dmg'
    feed.require(type(package['url']) is str and package['url'].startswith(prefix) and
                 re.fullmatch(r'[A-Za-z0-9_.-]+[.]' + suffix, package['url'][len(prefix):]) and
                 type(package['length']) is int and 0 < package['length'] <= feed.MAX_PACKAGE and
                 type(package['sha256']) is str and re.fullmatch('[0-9a-f]{64}', package['sha256']) and
                 package['keyId'] == epoch and type(package['ed25519']) is str, 'asset')
    feed.require(len(base64.b64decode(package['ed25519'], validate=True)) == 64, 'signature_length')
    return release


def payload(platform, epoch, arch, channel, sequence, generated, releases, rules):
    return {'schema': 2, 'product': PRODUCT, 'platform': platform, 'keyEpoch': epoch, 'architecture': arch,
            'channel': channel, 'sequence': sequence, 'generatedAt': feed.iso(generated),
            'expiresAt': feed.iso(generated + TTL), 'releases': releases, 'rules': rules}


def verify_manifest(raw, platform, epoch, public_key, arch, channel, repository, allow_expired=False, at=None):
    at = now() if at is None else at
    envelope = feed.loads(raw)
    feed.require(type(envelope) is dict and set(envelope) == {'envelopeSchema', 'payload', 'signatures'} and
                 envelope['envelopeSchema'] == 1, 'envelope')
    signatures = envelope['signatures']
    feed.require(type(signatures) is list and 1 <= len(signatures) <= 4 and
                 all(type(s) is dict and set(s) == {'keyId', 'ed25519'} for s in signatures) and
                 len({s['keyId'] for s in signatures}) == len(signatures), 'signatures')
    matching = [s for s in signatures if s['keyId'] == epoch]
    feed.require(len(matching) == 1, 'signatures')
    body = base64.b64decode(envelope['payload'], validate=True)
    feed.verify(body, matching[0]['ed25519'], public_key)
    obj = feed.loads(body)
    feed.require(type(obj) is dict and set(obj) == set(PAYLOAD_KEYS) and obj['schema'] == 2 and
                 obj['product'] == PRODUCT and obj['platform'] == platform and obj['keyEpoch'] == epoch and
                 obj['architecture'] == arch and obj['channel'] == channel and
                 type(obj['sequence']) is int and obj['sequence'] > 0, 'manifest_identity')
    start, end = feed.timestamp(obj['generatedAt']), feed.timestamp(obj['expiresAt'])
    feed.require(0 < end - start <= TTL and start <= at + 300, 'manifest_clock')
    feed.require(allow_expired or end > at, 'manifest_expired')
    releases = obj['releases']
    feed.require(type(releases) is list and len(releases) <= MAX_RELEASES, 'release_count')
    for release in releases:
        validate_release(release, platform, epoch, repository)
        feed.require(channel == 'beta' or release['channel'] == 'stable', 'release_channel')
    builds = [r['build'] for r in releases]
    feed.require(len(set(builds)) == len(builds), 'catalog')
    rule_compiler.validate(obj['rules'], set(builds))
    return obj


def windows_releases(state, epoch):
    """v2 views of the per-build Windows records in the site: identical signed MSI package objects.

    Like Mac, the catalog comes from site/windows/releases/<build>/release.json,
    which only a merged candidate changes (its digests are in the platform file
    list). Never from registry['feeds']: reserve.py commits a publication's
    record lists (v1 catalog and merge fence) before the deployment, so those
    lists describe the pending reservation, not the deployed snapshot that
    verify_snapshot checks. The publisher's register writes the new record into
    its own site checkout before allocating, so allocation sees it there.
    """
    result = []
    folder = state / 'site/windows/releases'
    for name, raw in sorted(_files(folder).items()) if folder.exists() else ():
        match = re.fullmatch(r'([1-9][0-9]*)/release[.]json', name)
        feed.require(match is not None, 'windows_feed_path')
        record_hash = feed.digest(raw)
        feed.require((state / 'records' / f'{record_hash}.json').read_bytes() == raw, 'record_digest')
        record = feed.loads(raw)
        feed.require(record['schema'] == 1 and record['approved'] is True and record['published'] is True, 'unapproved_record')
        r = record['release']
        feed.require(r['build'] == int(match[1]), 'record_digest')
        feed.require(r['architecture'] == 'x64', 'release_architecture')
        if r['package']['keyId'] != epoch:
            continue  # signed for another key epoch's feeds (register picks the epoch by keyId)
        result.append({'build': r['build'], 'version': r['version'], 'tag': r['tag'],
                       'channel': r.get('channel', 'stable'), 'sourceTreeSha256': r['sourceTreeSha256'],
                       'minimumOs': f'10.0.{r["minimumWindowsBuild"]}', 'protocolBaseline': r['protocolBaseline'],
                       'minimumUpdaterSchema': r['minimumUpdaterSchema'], 'notes': r['notes'],
                       'package': {k: r['package'][k] for k in PACKAGE_KEYS}})
    return result


def mac_record(raw, path, repository):
    match = MAC_RECORD.fullmatch(path)
    feed.require(match is not None, 'mac_feed_path')
    record = feed.loads(raw)
    feed.require(type(record) is dict and set(record) == {'schema', 'architecture', 'developer_id_verified',
                 'notarized', 'qualified_build', 'release'} and record['schema'] == 1 and
                 record['architecture'] == match[2] and record['developer_id_verified'] is True and
                 record['notarized'] is True, 'mac_qualification')
    release = validate_release(record['release'], 'mac', MAC_EPOCH, repository)
    feed.require(release['build'] == int(match[1]) == record['qualified_build'], 'mac_qualification')
    return record


def mac_records(state, repository):
    folder = state / 'site/mac/releases'
    records = {}
    if folder.exists():
        for name, raw in sorted(_files(folder).items()):
            path = 'mac/releases/' + name
            records[path] = mac_record(raw, path, repository)
    return records


def _files(folder):
    import feed_site
    return feed_site.files(folder)


def catalog(state, platform, config, epoch, arch, channel):
    if platform == 'windows':
        releases = windows_releases(state, epoch)
    else:
        releases = [r['release'] for r in mac_records(state, config['repository']).values() if r['architecture'] == arch]
    releases = sorted((r for r in releases if channel == 'beta' or r['channel'] == 'stable'), key=lambda r: r['build'])
    feed.require(len({r['build'] for r in releases}) == len(releases), 'catalog')
    return releases[-MAX_RELEASES:]


def promotion(old, new, build):
    """new is old with only `build` moved from beta to stable (published_release)."""
    before = [r for r in old if r['build'] == build]
    return (len(before) == 1 and before[0]['channel'] == 'beta' and
            [{**r, 'channel': 'stable'} if r['build'] == build else r for r in old] == new)


def allocate(state, platform, config, rules, at, release_build=None):
    """Reserve a new sequence for every v2 manifest whose payload must change.

    Renewal (release_build None) may change rules and expiry, never releases.
    A publication may only add release_build. Write pending plans; sign later.
    """
    reg = feed.registry(state)
    files = reg['platforms'][platform]['files']
    if platform == 'mac':
        # Records reach the snapshot only through a deployed candidate; only the
        # build being published may be new, or change by promotion (checked
        # against every signed manifest below).
        for path in mac_records(state, config['repository']):
            raw = (state / 'site' / path).read_bytes()
            feed.require(files.get(path) == feed.digest(raw) or
                         int(MAC_RECORD.fullmatch(path)[1]) == release_build, 'record_digest')
    v2 = reg.setdefault('v2', {})
    plans = []
    for epoch, key, arch, channel in slots(platform, config):
        path = relative(platform, epoch, arch, channel)
        releases = catalog(state, platform, config, epoch, arch, channel)
        builds = {r['build'] for r in releases}
        rule_compiler.validate(rules, builds)
        slot = v2.setdefault(path, {'last_allocated': 0})
        current = state / 'site' / path
        if current.exists():
            raw = current.read_bytes()
            feed.require(files.get(path) == feed.digest(raw), 'snapshot_digest')
            old = verify_manifest(raw, platform, epoch, key, arch, channel, config['repository'], True, at)
            feed.require(old['sequence'] <= slot['last_allocated'], 'allocation_watermark')
            changed = old['releases'] != releases
            feed.require(not changed or (release_build in builds and release_build not in
                                         {r['build'] for r in old['releases']}) or
                         promotion(old['releases'], releases, release_build), 'renewal_catalog_changed')
            if not changed and old['rules'] == rules and feed.timestamp(old['expiresAt']) - at > RENEW_BEFORE:
                continue
        else:
            # A failed first publication can consume reservations before a
            # signed snapshot exists. Never reuse those sequence numbers.
            feed.require(path not in files, 'snapshot_missing')
        slot['last_allocated'] += 1
        raw = feed.encode(payload(platform, epoch, arch, channel, slot['last_allocated'], at, releases, rules))
        plans.append({'path': path, 'payload': base64.b64encode(raw).decode(), 'payload_sha256': feed.digest(raw)})
    feed.write(state / 'registry.json', reg)
    feed.write(state / f'pending-v2-{platform}.json', {'schema': 1, 'platform': platform, 'plans': plans})
    return plans


def sign_mac(raw, public_key, tool_cache):
    import mac_release
    tool = mac_release.prepare(tool_cache) / 'bin/sign_update'
    with tempfile.TemporaryDirectory(prefix='oink-v2-manifest-') as directory:
        path = Path(directory) / 'payload'
        path.write_bytes(raw)
        # Same pinned signer and stdin key path as the Sparkle archives; the
        # signature is pure Ed25519 over these bytes and verified by OpenSSL.
        return mac_release.sign(tool, path, public_key)


def sign_plans(state, platform, config, tool_cache=None):
    pending = feed.read(state / f'pending-v2-{platform}.json')
    feed.require(pending['schema'] == 1 and pending['platform'] == platform, 'pending_platform')
    keys = {relative(platform, e, a, c): (e, k, a, c) for e, k, a, c in slots(platform, config)}
    if platform == 'windows':
        feed.require(all(os.environ.get(feed.KEY_ENV[e['epoch']]) for e in config['windows']), 'secret_unavailable')
        signer = lambda raw, epoch, key: feed.sign(raw, epoch, key)
    else:
        feed.require(bool(os.environ.get(MAC_SECRET)) and tool_cache is not None, 'sparkle_secret_unavailable')
        signer = lambda raw, epoch, key: sign_mac(raw, key, tool_cache)
    reg = feed.registry(state)
    for plan in pending['plans']:
        feed.require(plan['path'] in keys, 'feed_path')
        epoch, key, arch, channel = keys[plan['path']]
        raw = base64.b64decode(plan['payload'], validate=True)
        obj = feed.loads(raw)
        feed.require(feed.digest(raw) == plan['payload_sha256'] and
                     obj['sequence'] == reg['v2'][plan['path']]['last_allocated'], 'allocation_changed')
        envelope = feed.encode({'envelopeSchema': 1, 'payload': plan['payload'],
                                'signatures': [{'keyId': epoch, 'ed25519': signer(raw, epoch, key)}]})
        verify_manifest(envelope, platform, epoch, key, arch, channel, config['repository'])
        feed.write(state / 'site' / plan['path'], envelope)


def verify_snapshot(state, platform, config, at=None):
    """Signed v2 manifests of one platform: identity, expiry, reserved sequence and exact catalog."""
    reg = feed.registry(state)
    files = reg['platforms'][platform]['files']
    expected = {relative(platform, e, a, c): (e, k, a, c) for e, k, a, c in slots(platform, config)}
    present = [path for path in files if PATH.fullmatch(path)]
    feed.require(set(present) <= set(expected), platform + '_feed_path')
    v2 = reg.get('v2', {})
    for path in present:
        epoch, key, arch, channel = expected[path]
        obj = verify_manifest((state / 'site' / path).read_bytes(), platform, epoch, key, arch, channel,
                              config['repository'], at=at)
        feed.require(path in v2 and obj['sequence'] <= v2[path]['last_allocated'], 'deployment_sequence')
        feed.require(obj['releases'] == catalog(state, platform, config, epoch, arch, channel), 'signed_catalog_changed')


def sequence(raw):
    return feed.loads(base64.b64decode(feed.loads(raw)['payload'], validate=True))['sequence']


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('command', choices=['allocate', 'sign'])
    parser.add_argument('--platform', choices=['windows', 'mac'], required=True)
    parser.add_argument('--state', type=Path, default=Path('feed-state'))
    parser.add_argument('--release-build', type=int, help='publication only: the one build allowed to enter catalogs')
    parser.add_argument('--tool-cache', type=Path, help='Mac signing: pinned Sparkle tool cache')
    args = parser.parse_args()
    config = feed.trust(args.platform)
    if args.command == 'allocate':
        allocate(args.state, args.platform, config, rule_compiler.load(), now(), args.release_build)
    else:
        sign_plans(args.state, args.platform, config, args.tool_cache)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        code = str(error) if type(error) is ValueError and re.fullmatch('[a-z_0-9]+', str(error)) else 'feed_v2_operation_failed'
        raise SystemExit(code)
